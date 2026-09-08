//! libei/EIS 后端（设计文档 §12.3）。
//!
//! 通过 `xdg-desktop-portal.RemoteDesktop` 获取 EIS 连接 fd，再用纯 Rust
//! EI 客户端实现 [`reis`] 建立 EI 协议连接并注入：
//!
//! ```text
//! 1. portal.CreateSession        → session handle
//! 2. portal.SelectDevices        → KEYBOARD | POINTER
//! 3. portal.Start                → 用户确认弹窗（常驻 daemon 持有，见 §21.22）
//! 4. portal.ConnectToEIS         → socket fd
//! 5. reis ei handshake           → EI 协议连接（Sender 角色）
//! 6. seat.bind + device 注入     → 键盘/指针事件
//! ```
//!
//! 会话授权弹窗意味着：本后端的 `new()` 只做「session bus + portal 服务
//! 存在」的轻探测；完整会话建立延迟到首次注入（`ensure_connected`），且
//! 连接由实例持有、跨调用复用——与 §21.22 PortalSessionManager「daemon
//! 持有会话、避免 CLI 瞬态进程反复弹窗」的原则一致。

use std::collections::HashMap;
use std::sync::Arc;

use agent_shell_core::error::{AgentShellError, Result};
use agent_shell_core::types::{KeyCombo, MouseButton};
use async_trait::async_trait;
use enumflags2::BitFlags;
use futures_util::StreamExt;
use reis::event::{DeviceCapability, EiEvent};
use tokio::sync::Mutex;

use super::dispatcher::{InputService, Op, INPUT_TIMEOUT};
use crate::keymap::{char_to_evdev, combo_to_press_sequence};

/// portal 服务 bus 名。
const PORTAL_DESTINATION: &str = "org.freedesktop.portal.Desktop";
/// portal RemoteDesktop 接口名。
const REMOTE_DESKTOP_IFACE: &str = "org.freedesktop.portal.RemoteDesktop";

/// libei/EIS 注入后端。
pub struct LibeiInput {
    /// session bus 连接（构造时建立）。
    bus: zbus::Connection,
    /// 已建立的 EI 会话（跨调用复用，避免重复弹窗）。
    session: Mutex<Option<Arc<EiSession>>>,
}

/// 一条已授权的 portal→EI 链路。
struct EiSession {
    inner: reis::event::Connection,
    keyboard: Option<reis::ei::keyboard::Keyboard>,
    pointer_absolute: Option<reis::ei::pointer_absolute::PointerAbsolute>,
    button: Option<reis::ei::button::Button>,
    scroll: Option<reis::ei::scroll::Scroll>,
    device: reis::event::Device,
}

impl LibeiInput {
    /// 构造候选实例。轻探测：只验证 session bus 可连、portal 服务在 bus 上；
    /// 不触发 portal 弹窗。失败返回 `Err` 由 dispatcher 跳过该候选。
    pub async fn new() -> Result<Self> {
        let bus = zbus::Connection::session()
            .await
            .map_err(|e| AgentShellError::DBus(format!("session bus: {e}")))?;
        let has_portal = zbus::fdo::DBusProxy::new(&bus)
            .await
            .map_err(|e| AgentShellError::DBus(format!("DBusProxy: {e}")))?
            .name_has_owner(
                PORTAL_DESTINATION
                    .try_into()
                    .map_err(|e| AgentShellError::DBus(format!("bus name: {e}")))?,
            )
            .await
            .map_err(|e| {
                tracing::debug!("libei probe: NameHasOwner failed: {e}");
                e
            })
            .unwrap_or(false);
        if !has_portal {
            return Err(AgentShellError::BackendUnavailable(
                "xdg-desktop-portal not running".into(),
            ));
        }
        Ok(Self {
            bus,
            session: Mutex::new(None),
        })
    }

    /// 惰性建立完整 portal→EI 会话（首次注入时调用）。
    async fn ensure_connected(&self) -> Result<Arc<EiSession>> {
        let mut guard = self.session.lock().await;
        if let Some(s) = guard.as_ref() {
            return Ok(Arc::clone(s));
        }

        // 1–4. portal RemoteDesktop 全流程（异步），拿到 EIS socket fd。
        let bus = self.bus.clone();
        let owned_fd = portal_connect_to_eis(&bus).await?;

        // 5–6. EI 握手与 seat/device 协商在独立线程完成：
        //      reis 的 EiConvertEventStream 持有非 Send 回调表，其 future
        //      不能跨 async_trait 的 Send 边界；协商是一次性流程，用临时
        //      current_thread runtime 隔离执行，仅回传可 Send 的连接与代理。
        let negotiated = tokio::task::spawn_blocking(move || -> Result<EiNegotiated> {
            tokio::runtime::Builder::new_current_thread()
                .enable_all()
                .build()
                .map_err(|e| AgentShellError::Input(format!("libei runtime: {e}")))?
                .block_on(async move { ei_handshake_and_negotiate(owned_fd).await })
        })
        .await
        .map_err(|e| AgentShellError::Input(format!("libei setup join: {e}")))??;

        let s = Arc::new(EiSession {
            inner: negotiated.inner,
            keyboard: negotiated.device.interface(),
            pointer_absolute: negotiated.device.interface(),
            button: negotiated.device.interface(),
            scroll: negotiated.device.interface(),
            device: negotiated.device,
        });
        *guard = Some(Arc::clone(&s));
        Ok(s)
    }

    fn now_us() -> u64 {
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|d| d.as_micros() as u64)
            .unwrap_or(0)
    }

    /// 注入一组 evdev 键事件并逐事件成帧。
    async fn key_sequence(&self, seq: &[(u32, bool)]) -> Result<()> {
        use reis::ei::keyboard::KeyState;
        let s = self.ensure_connected().await?;
        let kb = s.keyboard.as_ref().ok_or_else(|| {
            AgentShellError::BackendUnavailable("libei: no keyboard capability".into())
        })?;
        for (code, down) in seq {
            kb.key(
                *code,
                if *down {
                    KeyState::Press
                } else {
                    KeyState::Released
                },
            );
            s.device.device().frame(0, Self::now_us());
        }
        s.inner.flush().map_err(flush_err)
    }

    async fn send_key_combo(&self, combo: &KeyCombo) -> Result<()> {
        // combo_to_press_sequence 的 bool 是 is_temp_shift 标记，非 down 状态；
        // press 阶段统一置 true（按下），逆序释放阶段置 false（临时 shift
        // 在其实体键之后弹起）。
        let order = combo_to_press_sequence(combo).map_err(AgentShellError::Input)?;
        let presses: Vec<(u32, bool)> = order.iter().map(|(c, _)| (*c, true)).collect();
        let releases: Vec<(u32, bool)> = order.iter().rev().map(|(c, _)| (*c, false)).collect();
        let mut full = presses;
        full.extend(releases);
        self.key_sequence(&full).await
    }

    /// 鼠标按键注入（press+release 各自成帧）。
    async fn click_button(&self, button: u32) -> Result<()> {
        use reis::ei::button::ButtonState;
        let s = self.ensure_connected().await?;
        let btn = s.button.as_ref().ok_or_else(|| {
            AgentShellError::BackendUnavailable("libei: no pointer/button capability".into())
        })?;
        btn.button(button, ButtonState::Press);
        s.device.device().frame(0, Self::now_us());
        btn.button(button, ButtonState::Released);
        s.device.device().frame(0, Self::now_us());
        s.inner.flush().map_err(flush_err)
    }
}

/// 握手+协商结果（全部字段可跨线程传递）。
struct EiNegotiated {
    inner: reis::event::Connection,
    device: reis::event::Device,
}

/// EI 握手（Sender）→ bind 能力 → 等待带键盘/指针能力的设备就绪。
async fn ei_handshake_and_negotiate(owned_fd: zbus::zvariant::OwnedFd) -> Result<EiNegotiated> {
    let std_fd: std::os::fd::OwnedFd = owned_fd.into();
    let ctx = reis::ei::Context::new(std::os::unix::net::UnixStream::from(std_fd))
        .map_err(|e| AgentShellError::Input(format!("libei: context: {e}")))?;
    let (conn, mut events) = ctx
        .handshake_tokio("agent-shell", reis::ei::handshake::ContextType::Sender)
        .await
        .map_err(|e| AgentShellError::Input(format!("libei: handshake: {e}")))?;

    #[allow(unused_assignments)] // loop 控制流下编译器无法证明赋值被读取
    let mut device = None;
    let deadline = tokio::time::Instant::now() + INPUT_TIMEOUT * 5;
    loop {
        let ev = tokio::time::timeout_at(deadline, events.next())
            .await
            .map_err(|_| {
                AgentShellError::Timeout("libei: seat/device negotiation timed out".into())
            })?
            .ok_or_else(|| AgentShellError::Input("libei: EIS connection closed".into()))?
            .map_err(|e| AgentShellError::Input(format!("libei event: {e}")))?;
        match ev {
            EiEvent::SeatAdded(added) => {
                added.seat.bind_capabilities(
                    BitFlags::from(DeviceCapability::Keyboard)
                        | BitFlags::from(DeviceCapability::Pointer),
                );
            }
            EiEvent::DeviceAdded(added) => {
                let d = added.device;
                if d.has_capability(DeviceCapability::Keyboard)
                    || d.has_capability(DeviceCapability::Pointer)
                {
                    d.device().start_emulating(0, 1);
                    device = Some(d);
                    break;
                }
            }
            EiEvent::Disconnected(dc) => {
                return Err(AgentShellError::BackendUnavailable(format!(
                    "libei: disconnected during setup ({})",
                    dc.explanation.unwrap_or_else(|| "no reason".into())
                )));
            }
            _ => {}
        }
    }
    conn.flush().map_err(flush_err)?;
    let device =
        device.ok_or_else(|| AgentShellError::BackendUnavailable("libei: no device".into()))?;
    Ok(EiNegotiated {
        inner: conn,
        device,
    })
}

fn flush_err(e: rustix::io::Errno) -> AgentShellError {
    AgentShellError::Input(format!("libei flush: {e}"))
}

/// portal CreateSession → SelectDevices → Start → ConnectToEIS 全流程，
/// 返回 EIS socket fd。
///
/// 响应式 portal 调用按 handle_token 规范监听
/// `/org/freedesktop/portal/desktop/request/<sender>/<token>` 对象路径上的
/// `Response` 信号；Start 步骤触发用户确认弹窗——调用方必须是持有本实例的
/// 常驻 daemon（§21.22），CLI 瞬态进程不应走到这里。
async fn portal_connect_to_eis(bus: &zbus::Connection) -> Result<zbus::zvariant::OwnedFd> {
    use zbus::zvariant::{ObjectPath, Value};

    let unique = bus
        .unique_name()
        .expect("session bus connection always has a unique name");
    // sender 部分：":1.42" → "1_42"（handle_token 路径编码规则）。
    let sender_part = unique.trim_start_matches(':').replace('.', "_");
    let token = format!("agent_shell_input_{}", std::process::id());
    let request_path = ObjectPath::try_from(format!(
        "/org/freedesktop/portal/desktop/request/{sender_part}/{token}"
    ))
    .map_err(|e| AgentShellError::DBus(format!("request path: {e}")))?;
    let session_path = ObjectPath::try_from(format!(
        "/org/freedesktop/portal/desktop/session/{sender_part}/{token}"
    ))
    .map_err(|e| AgentShellError::DBus(format!("session path: {e}")))?;

    let rd = zbus::Proxy::new(
        bus,
        PORTAL_DESTINATION,
        "/org/freedesktop/portal/desktop",
        REMOTE_DESKTOP_IFACE,
    )
    .await
    .map_err(|e| AgentShellError::DBus(format!("RemoteDesktop proxy: {e}")))?;

    let options: HashMap<&str, Value> = [
        ("handle_token", Value::from(token.as_str())),
        ("session_handle_token", Value::from(token.as_str())),
    ]
    .into_iter()
    .collect();

    // 订阅 Start 的 Response 信号后再发请求（避免竞态）。
    let rule = zbus::MatchRule::builder()
        .msg_type(zbus::message::Type::Signal)
        .interface("org.freedesktop.portal.Request")
        .map_err(|e| AgentShellError::DBus(format!("match rule interface: {e}")))?
        .member("Response")
        .map_err(|e| AgentShellError::DBus(format!("match rule member: {e}")))?
        .path(request_path.clone())
        .map_err(|e| AgentShellError::DBus(format!("match rule path: {e}")))?
        .build();
    let mut signal_stream = zbus::MessageStream::for_match_rule(rule, bus, Some(4))
        .await
        .map_err(|e| AgentShellError::DBus(format!("signal stream: {e}")))?;

    // 1–2. CreateSession / SelectDevices(KEYBOARD|POINTER)。
    rd.call_method("CreateSession", &(&options,))
        .await
        .map_err(portal_err)?;
    rd.call_method("SelectDevices", &(&session_path, &options))
        .await
        .map_err(portal_err)?;

    // 3. Start（触发用户确认弹窗）并等待 Response(u==0)。
    rd.call_method("Start", &(&session_path, "", &options))
        .await
        .map_err(portal_err)?;
    let response_code = wait_response(&mut signal_stream).await?;
    drop(signal_stream);
    if response_code != 0 {
        return Err(AgentShellError::Permission(format!(
            "portal RemoteDesktop authorization refused (code {response_code})"
        )));
    }

    // 4. ConnectToEIS → fd（响应体 a{sa{sv}}(h)：fd 数组取第一个）。
    let msg = rd
        .call_method("ConnectToEIS", &(&session_path, &options))
        .await
        .map_err(portal_err)?;
    let mut fields: Vec<zbus::zvariant::Value<'static>> = msg
        .body()
        .deserialize::<zbus::zvariant::OwnedStructure>()
        .map_err(|e| AgentShellError::DBus(format!("ConnectToEIS body: {e}")))?
        .0
        .into_fields();
    for field in fields.drain(..).rev() {
        // 签名 a{sa{sv}}(h)：结构体最后一个字段是 fd 数组。
        if let Ok(arr) = field.downcast::<zbus::zvariant::Array<'_>>() {
            for item in arr.iter() {
                if let Ok(fd) = item.downcast_ref::<zbus::zvariant::Fd<'_>>() {
                    // Fd<'_> 借用消息体；dup 成自有 fd。
                    if let Ok(owned) = fd.try_to_owned() {
                        return Ok(zbus::zvariant::OwnedFd::from(owned));
                    }
                }
            }
        }
    }
    Err(AgentShellError::DBus(
        "ConnectToEIS: no fd in response body".into(),
    ))
}

/// 从订阅流中读出 Request.Response 的第一个参数（响应码）。
async fn wait_response(stream: &mut zbus::MessageStream) -> Result<u32> {
    // 弹窗等待放宽到 30s：用户确认不可预期。
    let deadline = tokio::time::Instant::now() + INPUT_TIMEOUT * 30;
    // Response 信号首条即为 Start 请求的应答（订阅在请求前建立）。
    {
        let msg = tokio::time::timeout_at(deadline, stream.next())
            .await
            .map_err(|_| {
                AgentShellError::Timeout("portal RemoteDesktop response timed out".into())
            })?
            .ok_or_else(|| AgentShellError::DBus("portal signal stream ended".to_string()))?
            .map_err(|e| AgentShellError::DBus(format!("signal read: {e}")))?;
        let body = msg.body();
        let (code, _): (u32, zbus::zvariant::OwnedValue) = body
            .deserialize()
            .map_err(|e| AgentShellError::DBus(format!("Response body: {e}")))?;
        Ok(code)
    }
}

fn portal_err(e: zbus::Error) -> AgentShellError {
    AgentShellError::DBus(format!("RemoteDesktop call: {e}"))
}

#[async_trait]
impl InputService for LibeiInput {
    fn name(&self) -> &'static str {
        "libei"
    }

    async fn is_available(&self) -> bool {
        // 构造已验证 portal 在场；可用性即「会话能建立或已建立」。
        // 未连接 → 保持 true 惰性重试，真实失败在首次注入时以错误暴露。
        true
    }

    async fn health(&self) -> agent_shell_core::component::ComponentHealth {
        use agent_shell_core::component::ComponentHealth;
        match self.session.lock().await.as_ref() {
            Some(_) => ComponentHealth::Healthy,
            None => ComponentHealth::Degraded(
                "portal present but EIS session not yet established (lazy)".into(),
            ),
        }
    }

    async fn ensure_ready(&self, op: Op<'_>) -> Result<()> {
        // 建立 portal→EI 会话（可能触发授权弹窗）但不注入。会话建立失败
        // （如 `AccessDenied: Invalid session`）或所需能力缺失（门户可能只
        // 授权部分设备，如仅 pointer 而缺 keyboard）都发生在任何注入之前，
        // dispatcher 据此安全降级重放，而不会在注入期重试同一后端。
        let s = self.ensure_connected().await?;
        let missing = match op {
            Op::Key(_) | Op::Text(..) => s.keyboard.is_none().then_some("keyboard"),
            Op::Move(..) => s.pointer_absolute.is_none().then_some("absolute-pointer"),
            Op::Click(_) => s.button.is_none().then_some("button"),
            Op::Scroll(..) => s.scroll.is_none().then_some("scroll"),
        };
        match missing {
            Some(cap) => Err(AgentShellError::BackendUnavailable(format!(
                "libei: no {cap} capability"
            ))),
            None => Ok(()),
        }
    }

    async fn send_key(&self, combo: &KeyCombo) -> Result<()> {
        self.send_key_combo(combo).await
    }

    async fn type_text(&self, text: &str, delay_ms: u32) -> Result<()> {
        for c in text.chars() {
            let (code, shift) = char_to_evdev(c)
                .ok_or_else(|| AgentShellError::Input(format!("cannot type character {c:?}")))?;
            let mut seq: Vec<(u32, bool)> = Vec::new();
            if shift {
                seq.push((42, true));
            }
            seq.push((code, true));
            seq.push((code, false));
            if shift {
                seq.push((42, false));
            }
            self.key_sequence(&seq).await?;
            if delay_ms > 0 {
                tokio::time::sleep(std::time::Duration::from_millis(u64::from(delay_ms))).await;
            }
        }
        Ok(())
    }

    async fn mouse_move(&self, x: i32, y: i32) -> Result<()> {
        let s = self.ensure_connected().await?;
        if let Some(pa) = s.pointer_absolute.as_ref() {
            // 绝对移动：region 尺寸归一化；设备未公布尺寸时按常见
            // 1920×1080 逻辑分辨率近似。
            let (w, h) = match s.device.dimensions() {
                Some((w, h)) => (w as f32, h as f32),
                None => {
                    tracing::warn!(
                        "libei mouse_move: device dimensions unknown, assuming 1920x1080"
                    );
                    (1920.0_f32, 1080.0_f32)
                }
            };
            pa.motion_absolute(x as f32 / w, y as f32 / h);
            s.device.device().frame(0, Self::now_us());
            s.inner.flush().map_err(flush_err)
        } else {
            Err(AgentShellError::NotImplemented(
                "libei backend: absolute motion unsupported by this device".into(),
            ))
        }
    }

    async fn mouse_click(&self, button: MouseButton) -> Result<()> {
        self.click_button(evdev_mouse_button(button)).await
    }

    async fn mouse_scroll(&self, dx: i32, dy: i32) -> Result<()> {
        let s = self.ensure_connected().await?;
        let sc = s.scroll.as_ref().ok_or_else(|| {
            AgentShellError::BackendUnavailable("libei: no scroll capability".into())
        })?;
        if dy != 0 {
            sc.scroll(0.0, dy as f32);
            s.device.device().frame(0, Self::now_us());
        }
        if dx != 0 {
            sc.scroll(dx as f32, 0.0);
            s.device.device().frame(0, Self::now_us());
        }
        s.inner.flush().map_err(flush_err)
    }
}

/// MouseButton → evdev 按钮码（BTN_LEFT=0x110 起；侧键 BTN_BACK/FORWARD）。
fn evdev_mouse_button(b: MouseButton) -> u32 {
    match b {
        MouseButton::Left => 0x110,
        MouseButton::Right => 0x111,
        MouseButton::Middle => 0x112,
        MouseButton::Back => 0x116,
        MouseButton::Forward => 0x115,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn mouse_button_codes_match_evdev() {
        assert_eq!(evdev_mouse_button(MouseButton::Left), 0x110);
        assert_eq!(evdev_mouse_button(MouseButton::Right), 0x111);
        assert_eq!(evdev_mouse_button(MouseButton::Middle), 0x112);
        assert_eq!(evdev_mouse_button(MouseButton::Back), 0x116);
        assert_eq!(evdev_mouse_button(MouseButton::Forward), 0x115);
    }

    #[test]
    fn capability_bitflags_cover_keyboard_and_pointer() {
        let flags =
            BitFlags::from(DeviceCapability::Keyboard) | BitFlags::from(DeviceCapability::Pointer);
        assert!(flags.contains(DeviceCapability::Keyboard));
        assert!(flags.contains(DeviceCapability::Pointer));
        assert!(!flags.contains(DeviceCapability::Touch));
    }
}
