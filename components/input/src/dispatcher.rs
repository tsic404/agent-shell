//! `InputService` trait 与 `InputDispatcher`（设计文档 §12.2）。
//!
//! 降级链构造顺序（§12.1/§12.2）：
//! libei（Wayland 首选，非 X11 会话压入）→ ydotool（跨 DE 保底，需
//! `ydotool` 可执行且 `/dev/uinput` 存在）→ XTest（仅原生 X11，连接成功
//! 才压入）→ xdotool（`DISPLAY` 存在即压入：原生 X11 或 XWayland 会话，
//! 且可执行存在）。随后取第一个
//! `is_available() == true` 的后端为 `active`；全部不可用则 `active = None`
//! （调用返回错误，不 panic）。
//!
//! **操作期降级**（§12.2）：libei 的 `is_available` 仅验证 portal 在场、
//! 不建立会话（授权弹窗延迟到首次注入），因此构造期选中 libei 不保证注入
//! 成功。dispatcher 先经 `ensure_ready` 建立通道并校验所需能力——libei 会话
//! 建立失败（`AccessDenied: Invalid session`）或能力缺失（门户仅授权 pointer
//! 而缺 keyboard/scroll 等）即摘除并回落下一候选（ydotool/xdotool），且不重复
//! 弹窗重试同一后端；通道就绪后注入一次，注入期错误直接返回调用方、不降级
//! 重放（避免已注入部分事件后回落造成的重复键击/点击/文本）。
//!
//! 超时/重试（§19）：input 注入超时 1s、重试 3 次——由各后端的命令执行层
//! 统一施加（[`super::ydotool`] / [`super::xdotool`]），dispatcher 不重复包装。

use std::path::{Path, PathBuf};
use std::time::Duration;

use agent_shell_core::component::ComponentHealth;
use agent_shell_core::error::{AgentShellError, Result};
use agent_shell_core::types::{DesktopEnvironment, KeyCombo, MouseButton};
use async_trait::async_trait;

/// input 注入超时（§19 超时与重试表）。
pub const INPUT_TIMEOUT: Duration = Duration::from_secs(1);
/// input 注入重试次数（§19；首次 + 重试共 3 次尝试由后端命令层实现）。
pub const INPUT_RETRIES: u32 = 3;

/// 单个输入注入后端的统一接口。
#[async_trait]
pub trait InputService: Send + Sync {
    /// 后端名（doctor 输出 / 日志）。
    fn name(&self) -> &'static str;

    /// 异步可用性探测（dispatcher 构造期逐个调用，选中第一个 true）。
    async fn is_available(&self) -> bool;

    /// 构造期之后的健康检查（doctor 命令调用）。
    ///
    /// 默认按 `is_available` 归一化为 Healthy/Degraded；后端可覆盖以给出
    /// 更细的降级原因。
    async fn health(&self) -> ComponentHealth {
        if self.is_available().await {
            ComponentHealth::Healthy
        } else {
            ComponentHealth::Degraded("probe failed at health check".into())
        }
    }

    /// 建立/验证注入通道，但不注入任何事件。
    ///
    /// `op` 用于校验该操作所需能力——libei 建会话后据此检查 keyboard/
    /// absolute-pointer/button/scroll 能力（门户可能只授权部分设备，如仅
    /// pointer 而缺 keyboard）。命令型后端（ydotool/xdotool/XTest）无持久
    /// 会话，默认 `Ok(())` 忽略 `op`。dispatcher 在执行注入操作前先调用本
    /// 方法：失败（含能力缺失）发生在任何注入之前，可安全降级重放；注入期
    /// 错误则直接返回调用方，不重放（避免重复键击/点击/文本）。
    async fn ensure_ready(&self, _op: Op<'_>) -> Result<()> {
        Ok(())
    }

    /// 注入按键组合。
    async fn send_key(&self, combo: &KeyCombo) -> Result<()>;

    /// 键入文本（`delay_ms` 为字符间隔）。
    async fn type_text(&self, text: &str, delay_ms: u32) -> Result<()>;

    /// 移动鼠标到绝对坐标 `(x, y)`。
    async fn mouse_move(&self, x: i32, y: i32) -> Result<()>;

    /// 点击鼠标按键。
    async fn mouse_click(&self, button: MouseButton) -> Result<()>;

    /// 滚动（正值 dy 向下、正值 dx 向右）。
    async fn mouse_scroll(&self, dx: i32, dy: i32) -> Result<()>;
}

/// 注入操作描述——把各后端统一接口归并为一个分派点，供 [`InputDispatcher::dispatch`]
/// 沿降级链逐个尝试（不借走参数，`Copy` 便于循环内复用）。
///
/// `pub`：作为 [`InputService::ensure_ready`] 的参数，libei 据此校验所需能力
/// （keyboard/absolute-pointer/button/scroll）。
#[derive(Clone, Copy)]
pub enum Op<'a> {
    Key(&'a KeyCombo),
    Text(&'a str, u32),
    Move(i32, i32),
    Click(MouseButton),
    Scroll(i32, i32),
}

/// 带降级链的输入分发器（§12.2）。
///
/// `active` 用 `Mutex` 包裹：操作期降级会在注入失败时推进 active，doctor
/// 与后续调用观察到的是降级后的真实选中后端。
pub struct InputDispatcher {
    backends: Vec<Box<dyn InputService>>,
    active: std::sync::Mutex<Option<usize>>,
    /// active 状态持久化文件；`None` = 不持久化（测试装配）。
    state_path: Option<PathBuf>,
}

/// 状态文件名（D9 状态持久化：`$XDG_RUNTIME_DIR/agent-shell/input-active.json`）。
const ACTIVE_STATE_FILE: &str = "input-active.json";

/// 持久化的 active 后端状态（§12.2 操作期降级跨进程可见）。
#[derive(serde::Serialize, serde::Deserialize)]
struct PersistedActive {
    active: String,
}

/// 会话级状态目录 `$XDG_RUNTIME_DIR/agent-shell`（随登出清理）。
///
/// 操作期降级是登录会话内的事实（portal 授权每次登录重新判定），不应跨重启
/// 粘连：用 `$XDG_RUNTIME_DIR` 而非 `~/.local/state`，既覆盖「瞬态 daemon
/// 重启」这一缺口，又在下次登录时重新探测。未设置 `XDG_RUNTIME_DIR`
/// （SSH/TTY）返回 `None`——该场景本就没有可持久化的降级链。
fn runtime_state_dir() -> Option<PathBuf> {
    std::env::var_os("XDG_RUNTIME_DIR").map(|d| PathBuf::from(d).join("agent-shell"))
}

/// active 状态文件路径；无会话运行目录时为 `None`（不持久化）。
fn active_state_path() -> Option<PathBuf> {
    runtime_state_dir().map(|d| d.join(ACTIVE_STATE_FILE))
}

/// 读取上次持久化的 active 后端名；文件缺失/损坏返回 `None`。
fn load_persisted_active(path: &Path) -> Option<String> {
    let content = std::fs::read_to_string(path).ok()?;
    serde_json::from_str::<PersistedActive>(&content)
        .ok()
        .map(|p| p.active)
}

/// 把持久化的 active 后端名解析为当前链中的下标（None = 未持久化/不在链中）。
///
/// 名字解析而非下标持久化：链顺序/成员随会话变化时（如切换 DE 导致某后端
/// 不再压入）自动失效。可用性复验由 [`restore_persisted_active`] 完成。
fn resolve_persisted_active(path: &Path, backends: &[Box<dyn InputService>]) -> Option<usize> {
    let name = load_persisted_active(path)?;
    backends.iter().position(|b| b.name() == name.as_str())
}

/// 恢复持久化的 active 后端：名字仍在链中且复验 `is_available()` 通过才覆盖
/// 构造期探测结果；否则返回 `None`（回退首个可用候选，不复活损坏后端）。
async fn restore_persisted_active(
    path: &Path,
    backends: &[Box<dyn InputService>],
) -> Option<usize> {
    let i = resolve_persisted_active(path, backends)?;
    backends[i].is_available().await.then_some(i)
}

/// 原子写入 active 后端名（先写 `.tmp` 再 rename，避免崩溃产生半截文件）。
///
/// 失败不静默：持久化失败意味着下次重启回退到构造期快照，必须留下
/// `tracing::warn!` 诊断痕迹供排查。
fn persist_active(path: &Path, name: &str) {
    let Some(dir) = path.parent() else {
        tracing::warn!(path = %path.display(), "input: persist active state: no parent dir");
        return;
    };
    if let Err(e) = std::fs::create_dir_all(dir) {
        tracing::warn!(dir = %dir.display(), error = %e, "input: persist active state: create dir failed");
        return;
    }
    let payload = PersistedActive {
        active: name.to_string(),
    };
    let json = match serde_json::to_string(&payload) {
        Ok(j) => j,
        Err(e) => {
            tracing::warn!(error = %e, "input: persist active state: serialize failed");
            return;
        }
    };
    let tmp = path.with_extension("json.tmp");
    if let Err(e) = std::fs::write(&tmp, &json) {
        tracing::warn!(path = %tmp.display(), error = %e, "input: persist active state: write tmp failed");
        return;
    }
    if let Err(e) = std::fs::rename(&tmp, path) {
        tracing::warn!(path = %path.display(), error = %e, "input: persist active state: rename failed");
    }
}

/// 清除持久化的 active 后端状态（demote 到链尾无候选时，避免重启复活已摘除后端）。
fn clear_active_state(path: &Path) {
    match std::fs::remove_file(path) {
        Ok(()) => {}
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => {}
        Err(e) => tracing::warn!(
            path = %path.display(),
            error = %e,
            "input: clear persisted active state failed"
        ),
    }
}

/// xdotool 是否应入链（§12.2 最后兜底）：`DISPLAY` 存在且 `xdotool` 可执行。
///
/// 刻意不依赖 `de_type`：xdotool 直连 `DISPLAY` 指向的 X server，原生 X11
/// 与 XWayland 会话均可注入。Wayland-only DE（Hyprland/Sway/WLRWayland）在
/// XWayland 会话下（`DISPLAY` 存在）此前因 `supports_x11()` 为 false 而缺
/// 最后一级兜底，ydotool 缺失时 input 完全不可用。
fn xdotool_candidate(display: Option<&std::ffi::OsStr>, has_xdotool: bool) -> bool {
    display.is_some() && has_xdotool
}

impl InputDispatcher {
    /// 按 DE 探测候选集合并选出第一个可用的 active 后端。
    pub async fn new(de_type: DesktopEnvironment) -> Result<Self> {
        let mut backends: Vec<Box<dyn InputService>> = Vec::new();

        // 1. libei/EIS（Wayland 首选）：portal RemoteDesktop → EI 协议。
        //    探测失败（无 portal / 用户未授权 / 非 Wayland）不阻塞后续候选。
        if de_type != DesktopEnvironment::X11Generic && !de_type.is_tty() {
            match super::libei::LibeiInput::new().await {
                Ok(libei) => backends.push(Box::new(libei)),
                Err(e) => tracing::debug!("input: libei candidate unavailable: {e}"),
            }
        }

        // 2. ydotool（跨 DE 保底）：需要 ydotool 可执行 + /dev/uinput。
        if which::which("ydotool").is_ok() && std::path::Path::new("/dev/uinput").exists() {
            backends.push(Box::new(super::ydotool::YdotoolInput::new()));
        }

        // 3. XTest 扩展（X11 原生）：连接失败不压入。
        if de_type == DesktopEnvironment::X11Generic {
            if let Ok(xtest) = super::xtest::XTestInput::new() {
                backends.push(Box::new(xtest));
            }
        }
        // 4. xdotool（X11 / XWayland 兜底）：`DISPLAY` 存在即入链——xdotool
        //    直连 `DISPLAY` 指向的 X server（原生 X11 或 XWayland 均可），
        //    不依赖 `de_type.supports_x11()`：Wayland-only DE（Hyprland/Sway/
        //    WLRWayland）在 XWayland 会话下同样可经 xdotool 注入，作为
        //    ydotool 之下的最后兜底（此前 Wayland 会话无此级时 input 全不可用）。
        if xdotool_candidate(
            std::env::var_os("DISPLAY").as_deref(),
            which::which("xdotool").is_ok(),
        ) {
            backends.push(Box::new(super::xdotool::XdotoolInput::new()));
        }

        // 选择第一个可用后端。
        let mut active = None;
        for (i, b) in backends.iter().enumerate() {
            if b.is_available().await {
                active = Some(i);
                break;
            }
        }
        // 恢复操作期降级：上次 demote 持久化的后端名若仍在本链中，以其为
        // 初始 active——瞬态 daemon 重启后 doctor 显示降级后的真实后端，
        // 而非重新探测 libei 得到的构造期快照（也避免重复 portal 弹窗）。
        let state_path = active_state_path();
        if let Some(path) = &state_path {
            if let Some(i) = restore_persisted_active(path, &backends).await {
                active = Some(i);
            }
        }
        tracing::info!(
            candidates = backends.len(),
            active = active.map(|i| backends[i].name()).unwrap_or("none"),
            "input dispatcher assembled"
        );
        Ok(Self {
            backends,
            active: std::sync::Mutex::new(active),
            state_path,
        })
    }

    /// 当前激活的后端名（doctor：Healthy/Degraded 标明实际选中后端）。
    pub fn active_backend_name(&self) -> Option<&'static str> {
        self.current_active().map(|i| self.backends[i].name())
    }

    /// 候选后端名列表（诊断/测试用，按降级链顺序）。
    pub fn backend_names(&self) -> Vec<&'static str> {
        self.backends.iter().map(|b| b.name()).collect()
    }

    /// active 后端健康检查。
    pub async fn active_health(&self) -> ComponentHealth {
        match self.current_active() {
            Some(i) => self.backends[i].health().await,
            None => ComponentHealth::Unavailable,
        }
    }

    /// 读当前 active 下标（无锁竞争时直接取）。
    fn current_active(&self) -> Option<usize> {
        *self.active.lock().unwrap_or_else(|p| p.into_inner())
    }

    /// 把失败后端从 active 摘除，active 前进到下一候选（末尾则 None）。
    ///
    /// 仅当 `failed` 仍是当前 active 时才推进——并发下其它调用可能已摘除
    /// 更靠前的后端，此时保持现状。持久化在锁内完成：并发 demote 串行化，
    /// 避免旧写入的 rename 后完成覆盖新状态（lost update）。
    fn demote(&self, failed: usize) {
        let mut g = self.active.lock().unwrap_or_else(|p| p.into_inner());
        if *g != Some(failed) {
            return;
        }
        let next = (failed + 1 < self.backends.len()).then_some(failed + 1);
        *g = next;
        // 锁内持久化新的 active 后端；demote 到链尾（None）时清除状态文件，
        // 避免瞬态 daemon 重启后复活已摘除后端。
        if let Some(path) = self.state_path.as_deref() {
            match next {
                Some(i) => persist_active(path, self.backends[i].name()),
                None => clear_active_state(path),
            }
        }
    }

    /// 在降级链上执行一次注入操作：先逐个后端建立通道（`ensure_ready`），
    /// 失败即摘除并试下一候选；通道就绪后注入一次，注入期错误直接返回，
    /// 不降级重放（避免已注入部分事件后回落造成的重复键击/点击/文本）。
    async fn dispatch(&self, op: Op<'_>) -> Result<()> {
        let mut idx = self.current_active();
        let mut last_err = None;
        while let Some(i) = idx {
            let backend = self.backends[i].as_ref();
            // 通道建立/能力校验失败发生在任何注入之前——可安全降级重放。
            if let Err(e) = backend.ensure_ready(op).await {
                tracing::warn!(
                    backend = self.backends[i].name(),
                    error = %e,
                    "input: backend setup failed; demoting to next candidate"
                );
                last_err = Some(e);
                self.demote(i);
                idx = self.current_active();
                continue;
            }
            // 通道就绪：注入一次。注入期错误不再降级重放。
            return match op {
                Op::Key(c) => backend.send_key(c).await,
                Op::Text(t, d) => backend.type_text(t, d).await,
                Op::Move(x, y) => backend.mouse_move(x, y).await,
                Op::Click(b) => backend.mouse_click(b).await,
                Op::Scroll(dx, dy) => backend.mouse_scroll(dx, dy).await,
            };
        }
        Err(last_err.unwrap_or_else(|| {
            AgentShellError::BackendUnavailable(
                "input: no available backend in this session".into(),
            )
        }))
    }

    /// 注入按键组合（经降级链，失败自动回落）。
    pub async fn send_key(&self, combo: &KeyCombo) -> Result<()> {
        self.dispatch(Op::Key(combo)).await
    }

    /// 键入文本（经降级链，失败自动回落）。
    pub async fn type_text(&self, text: &str, delay_ms: u32) -> Result<()> {
        self.dispatch(Op::Text(text, delay_ms)).await
    }

    /// 移动鼠标（经降级链，失败自动回落）。
    pub async fn mouse_move(&self, x: i32, y: i32) -> Result<()> {
        self.dispatch(Op::Move(x, y)).await
    }

    /// 点击鼠标（经降级链，失败自动回落）。
    pub async fn mouse_click(&self, button: MouseButton) -> Result<()> {
        self.dispatch(Op::Click(button)).await
    }

    /// 滚动（经降级链，失败自动回落）。
    pub async fn mouse_scroll(&self, dx: i32, dy: i32) -> Result<()> {
        self.dispatch(Op::Scroll(dx, dy)).await
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use agent_shell_core::types::{KeyName, ModifierMask};
    use std::sync::atomic::{AtomicUsize, Ordering};
    use std::sync::Arc;

    /// 记录探测顺序的假后端：第 N 个实例前 N-1 次探测返回 false。
    #[derive(Clone)]
    struct FakeBackend {
        name: &'static str,
        fail_first: Arc<AtomicUsize>,
        probed: Arc<AtomicUsize>,
        inject_ok: bool,
    }

    impl FakeBackend {
        fn new(name: &'static str, fail_count: usize) -> Self {
            Self {
                name,
                fail_first: Arc::new(AtomicUsize::new(fail_count)),
                probed: Arc::new(AtomicUsize::new(0)),
                inject_ok: true,
            }
        }
    }

    /// 注入即失败的假后端（验证错误透传）。
    struct FailingBackend;

    #[async_trait]
    impl InputService for FailingBackend {
        fn name(&self) -> &'static str {
            "failing"
        }
        async fn is_available(&self) -> bool {
            true
        }
        async fn send_key(&self, _combo: &KeyCombo) -> Result<()> {
            Err(AgentShellError::Input("injection refused".into()))
        }
        async fn type_text(&self, _text: &str, _delay_ms: u32) -> Result<()> {
            unimplemented!()
        }
        async fn mouse_move(&self, _x: i32, _y: i32) -> Result<()> {
            unimplemented!()
        }
        async fn mouse_click(&self, _button: MouseButton) -> Result<()> {
            unimplemented!()
        }
        async fn mouse_scroll(&self, _dx: i32, _dy: i32) -> Result<()> {
            unimplemented!()
        }
    }

    /// 通道建立即失败的假后端（模拟 libei 会话建立失败 `AccessDenied`）。
    struct SetupFailingBackend;

    #[async_trait]
    impl InputService for SetupFailingBackend {
        fn name(&self) -> &'static str {
            "setup-failing"
        }
        async fn is_available(&self) -> bool {
            true
        }
        async fn ensure_ready(&self, _op: Op<'_>) -> Result<()> {
            Err(AgentShellError::BackendUnavailable("setup refused".into()))
        }
        async fn send_key(&self, _combo: &KeyCombo) -> Result<()> {
            unimplemented!("never reached: ensure_ready fails first")
        }
        async fn type_text(&self, _text: &str, _delay_ms: u32) -> Result<()> {
            unimplemented!()
        }
        async fn mouse_move(&self, _x: i32, _y: i32) -> Result<()> {
            unimplemented!()
        }
        async fn mouse_click(&self, _button: MouseButton) -> Result<()> {
            unimplemented!()
        }
        async fn mouse_scroll(&self, _dx: i32, _dy: i32) -> Result<()> {
            unimplemented!()
        }
    }

    #[async_trait]
    impl InputService for FakeBackend {
        fn name(&self) -> &'static str {
            self.name
        }
        async fn is_available(&self) -> bool {
            let n = self.probed.fetch_add(1, Ordering::SeqCst);
            n >= self.fail_first.load(Ordering::SeqCst)
        }
        async fn send_key(&self, _combo: &KeyCombo) -> Result<()> {
            if self.inject_ok {
                Ok(())
            } else {
                Err(AgentShellError::Input("no".into()))
            }
        }
        async fn type_text(&self, _text: &str, _delay_ms: u32) -> Result<()> {
            Ok(())
        }
        async fn mouse_move(&self, _x: i32, _y: i32) -> Result<()> {
            Ok(())
        }
        async fn mouse_click(&self, _button: MouseButton) -> Result<()> {
            Ok(())
        }
        async fn mouse_scroll(&self, _dx: i32, _dy: i32) -> Result<()> {
            Ok(())
        }
    }

    fn combo(ctrl: bool) -> KeyCombo {
        KeyCombo {
            keys: vec![agent_shell_core::types::Key::Named(KeyName::Return)],
            modifiers: if ctrl {
                ModifierMask::CTRL
            } else {
                ModifierMask::NONE
            },
        }
    }

    /// 用假后端组装 dispatcher 的测试通道（绕过真机探测）。
    fn dispatch_with(
        backends: Vec<Box<dyn InputService>>,
        active: Option<usize>,
    ) -> InputDispatcher {
        // 通过模块内私有字段构造——同 crate 测试可见。
        InputDispatcher {
            backends,
            active: std::sync::Mutex::new(active),
            state_path: None,
        }
    }

    #[tokio::test]
    async fn picks_first_available_backend_in_chain_order() {
        let a = FakeBackend::new("a", usize::MAX); // 永不可用
        let b = FakeBackend::new("b", 0); // 首次即可用
        let c = FakeBackend::new("c", 0);
        let d = dispatch_with(vec![Box::new(a), Box::new(b), Box::new(c)], None);
        let d = reselect(d).await;
        assert_eq!(d.active_backend_name(), Some("b"));
    }

    #[tokio::test]
    async fn no_available_backend_yields_none_not_panic() {
        let a = FakeBackend::new("a", usize::MAX);
        let d = dispatch_with(vec![Box::new(a)], None);
        let d = reselect(d).await;
        assert_eq!(d.active_backend_name(), None);
        let err = d.send_key(&combo(true)).await.unwrap_err();
        assert!(matches!(err, AgentShellError::BackendUnavailable(_)));
    }

    #[tokio::test]
    async fn delegation_reaches_active_backend() {
        let dead = FakeBackend::new("dead", usize::MAX);
        let live = FakeBackend::new("live", 0);
        let d = dispatch_with(vec![Box::new(dead), Box::new(live)], None);
        let d = reselect(d).await;
        assert_eq!(d.active_backend_name(), Some("live"));
        d.send_key(&combo(false)).await.expect("send_key delegates");
        d.type_text("hi", 10).await.expect("type_text delegates");
        d.mouse_move(5, 6).await.expect("mouse_move delegates");
        d.mouse_click(MouseButton::Left)
            .await
            .expect("mouse_click delegates");
        d.mouse_scroll(0, -3).await.expect("mouse_scroll delegates");
    }

    #[tokio::test]
    async fn backend_error_surfaces_as_input_error() {
        let d = dispatch_with(vec![Box::new(FailingBackend)], Some(0));
        let err = d.send_key(&combo(true)).await.unwrap_err();
        assert!(matches!(err, AgentShellError::Input(msg) if msg.contains("refused")));
    }

    #[tokio::test]
    async fn operation_falls_back_when_active_backend_setup_fails() {
        let failing = SetupFailingBackend; // ensure_ready 恒失败（注入前）
        let ok = FakeBackend::new("ok", 0); // inject_ok = true
        let d = dispatch_with(vec![Box::new(failing), Box::new(ok)], Some(0));
        // active = "setup-failing"；通道建立失败后应摘除并降级到 "ok"。
        d.send_key(&combo(true))
            .await
            .expect("falls back to next backend");
        assert_eq!(d.active_backend_name(), Some("ok"));
    }

    #[tokio::test]
    async fn injection_failure_is_not_replayed_on_next_backend() {
        // 通道就绪后的注入期错误（send_key 已可能产生副作用）不应降级重放。
        let failing = FailingBackend; // ensure_ready 默认 Ok，send_key 失败
        let ok = FakeBackend::new("ok", 0);
        let d = dispatch_with(vec![Box::new(failing), Box::new(ok)], Some(0));
        let err = d.send_key(&combo(true)).await.unwrap_err();
        assert!(matches!(err, AgentShellError::Input(msg) if msg.contains("refused")));
        // 未降级：active 仍停留在注入期失败的后端。
        assert_eq!(d.active_backend_name(), Some("failing"));
    }

    #[test]
    fn xdotool_candidate_requires_display_and_binary() {
        // §12.2 兜底：`DISPLAY` 存在（原生 X11 或 XWayland）且 `xdotool` 可
        // 执行即入链；不依赖 de_type（Wayland-only DE 的 XWayland 会话同样
        // 可经 xdotool 注入）。
        assert!(!xdotool_candidate(None, true), "无 DISPLAY 不压入");
        assert!(
            !xdotool_candidate(Some(std::ffi::OsStr::new(":0")), false),
            "无 xdotool 不压入"
        );
        assert!(xdotool_candidate(Some(std::ffi::OsStr::new(":0")), true));
    }

    #[test]
    fn timeout_constants_match_design() {
        // §19：input 注入超时 1s、重试 3 次。
        assert_eq!(INPUT_TIMEOUT, Duration::from_secs(1));
        assert_eq!(INPUT_RETRIES, 3);
    }

    #[tokio::test]
    async fn demote_persists_active_backend_name() {
        let dir = tempfile::tempdir().expect("tempdir");
        let state_path = dir.path().join("input-active.json");
        let failing = SetupFailingBackend;
        let ok = FakeBackend::new("ok", 0);
        let d = InputDispatcher {
            backends: vec![Box::new(failing), Box::new(ok)],
            active: std::sync::Mutex::new(Some(0)),
            state_path: Some(state_path.clone()),
        };
        d.send_key(&combo(true)).await.expect("falls back");
        assert_eq!(d.active_backend_name(), Some("ok"));
        // 降级后 active 名持久化到状态文件，供瞬态 daemon 重启后恢复。
        let content = std::fs::read_to_string(&state_path).expect("state file written");
        let parsed: PersistedActive = serde_json::from_str(&content).expect("valid json");
        assert_eq!(parsed.active, "ok");
    }

    #[test]
    fn resolve_persisted_active_restores_and_tolerates_stale_state() {
        let dir = tempfile::tempdir().expect("tempdir");
        let state_path = dir.path().join("input-active.json");
        let a = FakeBackend::new("libei", 0); // 构造期首个可用
        let b = FakeBackend::new("ydotool", 0);
        let backends: Vec<Box<dyn InputService>> = vec![Box::new(a), Box::new(b)];

        // 未持久化 → None（回退首个可用）。
        assert_eq!(resolve_persisted_active(&state_path, &backends), None);

        // 持久化 "ydotool" → 恢复到其下标（跳过首个可用的 libei）。
        persist_active(&state_path, "ydotool");
        assert_eq!(
            resolve_persisted_active(&state_path, &backends),
            Some(1),
            "restored backend must override construction-time first-available"
        );

        // 持久化名不在当前链中（如切换 DE）→ None。
        persist_active(&state_path, "xdotool");
        assert_eq!(resolve_persisted_active(&state_path, &backends), None);

        // 损坏/缺失文件 → None，不 panic。
        std::fs::write(&state_path, "not-json").unwrap();
        assert_eq!(resolve_persisted_active(&state_path, &backends), None);
        assert_eq!(
            resolve_persisted_active(&dir.path().join("absent.json"), &backends),
            None
        );
    }

    #[tokio::test]
    async fn demote_to_none_clears_persisted_state() {
        let dir = tempfile::tempdir().expect("tempdir");
        let state_path = dir.path().join("input-active.json");
        persist_active(&state_path, "setup-failing");
        let failing = SetupFailingBackend; // 唯一后端：demote 后链尾 → None
        let d = InputDispatcher {
            backends: vec![Box::new(failing)],
            active: std::sync::Mutex::new(Some(0)),
            state_path: Some(state_path.clone()),
        };
        let err = d.send_key(&combo(true)).await.unwrap_err();
        assert!(matches!(err, AgentShellError::BackendUnavailable(_)));
        assert_eq!(d.active_backend_name(), None);
        // demote 到 None 清除状态文件，避免重启复活已摘除后端。
        assert!(
            !state_path.exists(),
            "state file should be cleared on demote-to-none"
        );
    }

    #[tokio::test]
    async fn restore_persisted_active_requires_availability() {
        let dir = tempfile::tempdir().expect("tempdir");
        let state_path = dir.path().join("input-active.json");
        persist_active(&state_path, "ydotool");
        // ydotool 名字在链中但 is_available() == false → 不得复活。
        let libei = FakeBackend::new("libei", 0);
        let ydotool = FakeBackend::new("ydotool", usize::MAX); // 永不可用
        let backends: Vec<Box<dyn InputService>> = vec![Box::new(libei), Box::new(ydotool)];
        assert_eq!(restore_persisted_active(&state_path, &backends).await, None);
    }

    /// 测试辅助：对已构造的 dispatcher 重新执行「选第一个可用」逻辑，
    /// 使 FakeBackend 的探测计数语义生效。
    async fn reselect(mut d: InputDispatcher) -> InputDispatcher {
        *d.active.get_mut().expect("owned dispatcher uncontended") = None;
        for (i, b) in d.backends.iter().enumerate() {
            if b.is_available().await {
                *d.active.get_mut().expect("owned dispatcher uncontended") = Some(i);
                break;
            }
        }
        d
    }
}
