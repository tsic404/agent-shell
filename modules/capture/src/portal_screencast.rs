//! portal ScreenCast → PipeWire 流式捕获（设计文档 §13.2）。
//!
//! 流程固定五步：CreateSession → SelectSources → Start（用户确认弹窗）→
//! OpenPipeWireRemote 拿 (node_id, fd) → PipeWireNode 订阅节点取帧。
//!
//! 会话由常驻 daemon 持有复用（§21.22/21.24）：CLI 瞬态建会话会反复弹窗。
//! ScreenCast 超时 10s / 重试 1 次（§19.3）。

use std::collections::HashMap;
use std::time::Duration;

use agent_shell_core::error::{AgentShellError, Result};
use zbus::zvariant::{self, ObjectPath};

use crate::portal_common::{portal_proxy, wait_for_response, PORTAL_SERVICE};

/// ScreenCast 通道默认超时（§19.3）。
pub const SCREENCAST_TIMEOUT: Duration = Duration::from_secs(10);

/// 捕获目标（portal SourceType 位掩码）。
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum CaptureTarget {
    /// 整个显示器。
    Monitor,
    /// 单个窗口。
    Window,
}

impl CaptureTarget {
    fn source_type_u32(self) -> u32 {
        match self {
            // portal SourceType: MONITOR=1, WINDOW=2；请求全部可用类型，
            // 由用户在 Start 弹窗里实际选择。
            CaptureTarget::Monitor => 1,
            CaptureTarget::Window => 2,
        }
    }
}

/// 帧像素布局（消费方据此做通道序与 bpp 解析）。
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum PixelFormat {
    /// 4 字节：B,G,R,x（X11 ZPixmap little-endian / PipeWire BGRA）。
    Bgra,
    /// 4 字节：B,G,R,x——x 未定义（PipeWire BGRx），与 Bgra 同序。
    Bgrx,
    /// 4 字节：R,G,B,A（PipeWire RGBA）。
    Rgba,
    /// 2 字节：RGB565 little-endian（X11 depth 16）。
    Rgb565,
    /// 1 字节调色板索引（X11 depth 8）。
    Clut8,
}

impl PixelFormat {
    /// 每像素字节数。
    pub fn bytes_per_pixel(self) -> usize {
        match self {
            PixelFormat::Bgra | PixelFormat::Bgrx | PixelFormat::Rgba => 4,
            PixelFormat::Rgb565 => 2,
            PixelFormat::Clut8 => 1,
        }
    }
}

/// 一帧捕获结果：原始像素 + 帧元数据。
#[derive(Clone, Debug)]
pub struct Frame {
    /// 原始像素，布局由 [`Frame::format`] 描述。
    pub data: Vec<u8>,
    pub width: u32,
    pub height: u32,
    pub stride: usize,
    /// 协商出的/源端像素格式。
    pub format: PixelFormat,
}

/// 通过 ScreenCast portal 建立 PipeWire 流并持续取帧。
///
/// 内部线程运行 PipeWire main loop；`capture_frame` 从共享槽位
/// 取最新帧。会话句柄由本对象持有，Drop 时关闭 portal session。
pub struct ScreenCastCapture {
    conn: zbus::Connection,
    session_path: ObjectPath<'static>,
    /// 关闭信号：Drop 时通知内部线程退出 main loop 并释放资源。
    shutdown: std::sync::mpsc::Sender<()>,
    /// 最新帧共享槽位（worker 写，capture_frame 读）。
    latest: std::sync::Arc<(std::sync::Mutex<Option<Frame>>, std::sync::Condvar)>,
    worker: Option<std::thread::JoinHandle<()>>,
}

impl ScreenCastCapture {
    /// 走完整 portal 五步流程建立流。
    ///
    /// 阻塞直至用户在 portal 弹窗确认（或超时 `SCREENCAST_TIMEOUT`）。
    pub async fn start(conn: zbus::Connection, target: CaptureTarget) -> Result<Self> {
        let proxy = portal_proxy(&conn, "org.freedesktop.portal.ScreenCast")
            .await
            .map_err(|e| AgentShellError::DBus(format!("ScreenCast proxy: {e}")))?;
        Self::start_via_proxy(&conn, &proxy, target).await
    }

    async fn start_via_proxy(
        conn: &zbus::Connection,
        proxy: &zbus::Proxy<'_>,
        target: CaptureTarget,
    ) -> Result<Self> {
        let pid = std::process::id();
        let token = format!("agent_shell_screencast_{}", std::process::id());

        // 1. CreateSession：方法返回值是 **Request 对象路径**；
        //    真正的 session_handle 在其 Response 信号的
        //    results["session_handle"] 里（xdg-desktop-portal 规范）。
        let mut o = std::collections::HashMap::<&str, zvariant::Value>::new();
        o.insert("handle_token", zvariant::Value::from(token.as_str()));
        o.insert(
            "session_handle_token",
            zvariant::Value::from(format!("agent_shell_{pid}")),
        );
        let create_request: zvariant::OwnedObjectPath = proxy
            .call("CreateSession", &(o,))
            .await
            .map_err(|e| AgentShellError::DBus(format!("CreateSession: {e}")))?;
        let (_, create_results) =
            wait_for_response(conn, &create_request, SCREENCAST_TIMEOUT).await?;
        let session_handle = crate::portal_common::string_field(&create_results, "session_handle")
            .ok_or_else(|| {
                AgentShellError::Capture(
                    "ScreenCast: no session_handle in CreateSession response".into(),
                )
            })?;
        let session_path: ObjectPath<'static> =
            zvariant::ObjectPath::try_from(session_handle.to_owned())
                .map_err(|e| AgentShellError::Capture(format!("bad session_handle: {e}")))?
                .into_owned();

        // 2. SelectSources：返回其 Request handle，须等 Response 才算完成。
        let mut o = std::collections::HashMap::<&str, zvariant::Value>::new();
        o.insert("handle_token", zvariant::Value::from(token.as_str()));
        o.insert("types", zvariant::Value::from(target.source_type_u32()));
        o.insert("multiple", zvariant::Value::from(false));
        let select_request: zvariant::OwnedObjectPath = proxy
            .call::<_, (
                &ObjectPath<'_>,
                std::collections::HashMap<&str, zvariant::Value>,
            ), zvariant::OwnedObjectPath>("SelectSources", &(&session_path, o))
            .await
            .map_err(|e| AgentShellError::DBus(format!("SelectSources: {e}")))?;
        wait_for_response(conn, &select_request, SCREENCAST_TIMEOUT).await?;

        // 3. Start：触发用户确认弹窗；Response（在其 Request handle 上）
        //    携带 streams 数组。
        let mut o = std::collections::HashMap::<&str, zvariant::Value>::new();
        o.insert("handle_token", zvariant::Value::from(token.as_str()));
        let start_request: zvariant::OwnedObjectPath = proxy
            .call::<_, (
                &ObjectPath<'_>,
                &str,
                std::collections::HashMap<&str, zvariant::Value>,
            ), zvariant::OwnedObjectPath>("Start", &(&session_path, "", o))
            .await
            .map_err(|e| AgentShellError::DBus(format!("Start: {e}")))?;
        let (_, results) = wait_for_response(conn, &start_request, SCREENCAST_TIMEOUT).await?;
        let node_id = extract_node_id(&results).ok_or_else(|| {
            AgentShellError::Capture("ScreenCast: no stream node in response".into())
        })?;

        // 4. OpenPipeWireRemote → fd
        let fd: zbus::zvariant::OwnedFd = proxy
            .call(
                "OpenPipeWireRemote",
                &(&session_path, HashMap::<&str, zvariant::Value>::new()),
            )
            .await
            .map_err(|e| AgentShellError::DBus(format!("OpenPipeWireRemote: {e}")))?;

        // 5. 启动 PipeWire 订阅线程
        let (shutdown_tx, shutdown_rx) = std::sync::mpsc::channel::<()>();
        let latest = std::sync::Arc::new((
            std::sync::Mutex::<Option<Frame>>::new(None),
            std::sync::Condvar::new(),
        ));
        let latest_for_thread = std::sync::Arc::clone(&latest);
        // OwnedFd 是 RawFd 包装；跨线程交给 pw 前复制出 owned fd。
        use std::os::fd::AsFd as _;
        let raw_fd = fd
            .as_fd()
            .try_clone_to_owned()
            .map_err(|e| AgentShellError::Capture(format!("dup pipewire fd: {e}")))?;
        let worker = std::thread::Builder::new()
            .name("pw-screencast".into())
            .spawn(move || {
                run_pipewire_node(raw_fd, node_id, latest_for_thread, shutdown_rx);
            })
            .map_err(|e| AgentShellError::Other(Box::new(e)))?;

        Ok(Self {
            conn: conn.clone(),
            session_path,
            shutdown: shutdown_tx,
            latest,
            worker: Some(worker),
        })
    }

    /// portal 服务是否可达（装配探测用；不弹窗）。
    pub async fn available(conn: &zbus::Connection) -> bool {
        use zbus::fdo::DBusProxy;
        match DBusProxy::new(conn).await {
            Ok(dbus) => dbus
                .name_has_owner(PORTAL_SERVICE.try_into().unwrap())
                .await
                .unwrap_or(false),
            Err(_) => false,
        }
    }

    /// 取最新一帧（自上次调用以来的最新帧；尚无帧则阻塞等待至多一个超时周期）。
    pub async fn capture_frame(&self) -> Result<Frame> {
        let slot = std::sync::Arc::clone(&self.latest);
        tokio::task::spawn_blocking(move || {
            let (lock, cvar) = &*slot;
            let guard = lock.lock().unwrap_or_else(|p| p.into_inner());
            let (mut guard, _timeout) = cvar
                .wait_timeout_while(guard, SCREENCAST_TIMEOUT, |f| f.is_none())
                .unwrap_or_else(|p| p.into_inner());
            guard
                .take()
                .ok_or_else(|| AgentShellError::Timeout("no PipeWire frame arrived in time".into()))
        })
        .await
        .map_err(|e| AgentShellError::Other(Box::new(e)))?
    }

    /// portal session 对象路径（PortalSessionManager 登记用）。
    pub fn session_path(&self) -> &ObjectPath<'static> {
        &self.session_path
    }

    /// 关闭 portal session（Close 方法；幂等）。
    pub async fn close_session(&self) -> Result<()> {
        let proxy = portal_proxy(&self.conn, "org.freedesktop.portal.ScreenCast")
            .await
            .map_err(|e| AgentShellError::DBus(format!("ScreenCast proxy: {e}")))?;
        proxy
            .call::<_, (&str,), ()>("Close", &(self.session_path.as_str(),))
            .await
            .map_err(|e| AgentShellError::DBus(format!("Close session: {e}")))?;
        Ok(())
    }
}

impl Drop for ScreenCastCapture {
    fn drop(&mut self) {
        let _ = self.shutdown.send(());
        if let Some(worker) = self.worker.take() {
            let _ = worker.join();
        }
    }
}

/// 从 Start 的 Response 结果字典提取第一个流的 node id。
///
/// `streams` 类型为 `a(ua{sv})`。
fn extract_node_id(results: &HashMap<String, zvariant::OwnedValue>) -> Option<u32> {
    let streams = results.get("streams")?;
    let arr = streams.downcast_ref::<zvariant::Array>().ok()?;
    let first = arr.first()?;
    let structure = first.downcast_ref::<zvariant::Structure>().ok()?;
    let fields = structure.fields();
    let node = fields.first()?.downcast_ref::<u32>().ok()?;
    Some(node)
}

/// PipeWire 端：连 fd、订阅 node、把最新帧写进共享槽位。
///
/// 在专用线程上运行独立 main loop（pipewire crate 的回调是同步的，
/// 不与 tokio 运行时耦合）；`shutdown_rx` 收到消息即 quit。
fn run_pipewire_node(
    fd: std::os::fd::OwnedFd,
    node_id: u32,
    latest: std::sync::Arc<(std::sync::Mutex<Option<Frame>>, std::sync::Condvar)>,
    shutdown_rx: std::sync::mpsc::Receiver<()>,
) {
    use pipewire as pw;
    use pipewire::spa;
    use pipewire::spa::param::format_utils;
    use pipewire::spa::pod::Pod;
    pw::init();
    let Ok(mainloop) = pw::main_loop::MainLoopRc::new(None) else {
        tracing::error!("pipewire: MainLoop creation failed");
        return;
    };
    let Ok(context) = pw::context::ContextRc::new(&mainloop, None) else {
        tracing::error!("pipewire: Context creation failed");
        return;
    };
    // connect_fd：通过 portal 下发的 fd 连接远端 graph（非本地默认实例）。
    let core = match context.connect_fd(fd, None) {
        Ok(core) => core,
        Err(e) => {
            tracing::error!("pipewire: connect_fd failed: {e}");
            return;
        }
    };

    struct UserData {
        format: spa::param::video::VideoInfoRaw,
        latest: std::sync::Arc<(std::sync::Mutex<Option<Frame>>, std::sync::Condvar)>,
        negotiated: bool,
        /// 协商出的像素格式；None = 未协商或协商出不受支持格式。
        pixel_format: Option<PixelFormat>,
    }

    let props = pw::properties::properties! {
        *pw::keys::MEDIA_TYPE => "Video",
        *pw::keys::MEDIA_CATEGORY => "Capture",
        *pw::keys::MEDIA_ROLE => "Screen",
    };
    let Ok(stream) = pw::stream::StreamBox::new(&core, "agent-shell-screencast", props) else {
        tracing::error!("pipewire: Stream creation failed");
        return;
    };

    let data = UserData {
        format: spa::param::video::VideoInfoRaw::new(),
        latest,
        negotiated: false,
        pixel_format: None,
    };

    let listener = stream
        .add_local_listener_with_user_data(data)
        .param_changed(|_stream, user_data, id, param| {
            if id != pw::spa::param::ParamType::Format.as_raw() {
                return;
            }
            let Some(param) = param else { return };
            if format_utils::parse_format(param).is_err() {
                return;
            }
            if user_data.format.parse(param).is_ok() {
                user_data.pixel_format = match user_data.format.format() {
                    pw::spa::param::video::VideoFormat::BGRA => Some(PixelFormat::Bgra),
                    pw::spa::param::video::VideoFormat::BGRx => Some(PixelFormat::Bgrx),
                    pw::spa::param::video::VideoFormat::RGBA => Some(PixelFormat::Rgba),
                    other => {
                        tracing::warn!("pipewire: unexpected negotiated format {other:?}");
                        None
                    }
                };
                user_data.negotiated = true;
            }
        })
        .process(|stream, user_data| {
            // 未协商出受支持的格式时不产帧（消费方按 Frame.format 解析）。
            let Some(pixel_format) = user_data.pixel_format else {
                return;
            };
            let Some(mut buffer) = stream.dequeue_buffer() else {
                return;
            };
            let datas = buffer.datas_mut();
            if datas.is_empty() {
                return;
            }
            let data0 = &mut datas[0];
            let size = data0.chunk().size() as usize;
            let Some(bytes) = data0.data() else { return };
            if size == 0 || size > bytes.len() {
                return;
            }
            let rect = user_data.format.size();
            let frame = Frame {
                data: bytes[..size].to_vec(),
                width: rect.width.max(1),
                height: rect.height.max(1),
                stride: (size / rect.height.max(1) as usize),
                format: pixel_format,
            };
            let (lock, cvar) = &*user_data.latest;
            if let Ok(mut guard) = lock.lock() {
                *guard = Some(frame);
                cvar.notify_all();
            }
        })
        .register();
    let _listener = match listener {
        Ok(l) => l,
        Err(e) => {
            tracing::error!("pipewire: listener register failed: {e}");
            return;
        }
    };

    // 枚举常见 BGRA/BGRx/RGBA 布局；实际格式由 portal 端协商，
    // 协商结果在 param_changed 回调里解析。
    use pw::spa::param::format::FormatProperties;
    use pw::spa::param::video::VideoFormat;
    let obj = pw::spa::pod::object!(
        pw::spa::utils::SpaTypes::ObjectParamFormat,
        pw::spa::param::ParamType::EnumFormat,
        pw::spa::pod::property!(
            FormatProperties::MediaType,
            Id,
            spa::param::format::MediaType::Video
        ),
        pw::spa::pod::property!(
            FormatProperties::MediaSubtype,
            Id,
            spa::param::format::MediaSubtype::Raw
        ),
        pw::spa::pod::property!(
            FormatProperties::VideoFormat,
            Choice,
            Enum,
            Id,
            VideoFormat::BGRA,
            VideoFormat::BGRA,
            VideoFormat::BGRx,
            VideoFormat::RGBA
        ),
    );
    let values: Vec<u8> = pw::spa::pod::serialize::PodSerializer::serialize(
        std::io::Cursor::new(Vec::new()),
        &pw::spa::pod::Value::Object(obj),
    )
    .map(|v| v.0.into_inner())
    .unwrap_or_default();
    let Some(pod) = Pod::from_bytes(&values) else {
        tracing::error!("pipewire: pod serialization failed");
        return;
    };

    if let Err(e) = stream.connect(
        spa::utils::Direction::Input,
        Some(node_id),
        pw::stream::StreamFlags::AUTOCONNECT
            | pw::stream::StreamFlags::MAP_BUFFERS
            | pw::stream::StreamFlags::RT_PROCESS,
        &mut [pod],
    ) {
        tracing::error!("pipewire: stream connect failed: {e}");
        return;
    }

    // mainloop.run() 永久阻塞无法外部退出；改为 iterate 轮询驱动，
    // 每 50ms 处理一轮事件并检查 shutdown channel。
    loop {
        if shutdown_rx.recv_timeout(Duration::from_millis(50)).is_ok() {
            break;
        }
        mainloop
            .loop_()
            .iterate(pw::loop_::Timeout::Finite(Duration::from_millis(50)));
    }
}
