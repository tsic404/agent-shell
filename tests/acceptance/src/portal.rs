//! portal Screenshot 夹具：私有 session bus 上模拟授权门户的成功/拒绝。
//!
//! 截图验收的判定面是「daemon 与 portal 的协议交互 + 落盘编码」，真实屏幕
//! 内容不可在 CI 复现；本夹具让 `CaptureDispatcher` 走真实 portal 客户端、
//! 真实 PNG 校验与真实落盘路径。

use std::collections::HashMap;
use std::path::PathBuf;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};

use zbus::zvariant::OwnedValue;

use crate::bus::{claim, PrivateBus};

/// portal service / path / interface 坐标（与 `modules/capture` 同源）。
pub const PORTAL_SERVICE: &str = "org.freedesktop.portal.Desktop";
pub const PORTAL_PATH: &str = "/org/freedesktop/portal/desktop";
pub const SCREENSHOT_IFACE: &str = "org.freedesktop.portal.Screenshot";

/// 一次 portal Screenshot 调用的行为。
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum PortalBehaviour {
    /// 返回 0 并给出 PNG 文件 URI。
    Success,
    /// 返回非零响应码（1 = 用户取消 / 2 = 其它），客户端归一到权限错误。
    Cancel,
}

/// portal Screenshot mock：返回指定 PNG 字节与响应码。
#[derive(Clone)]
pub struct ScreenshotMock {
    behaviour: Arc<Mutex<PortalBehaviour>>,
    interactive_behaviour: Arc<Mutex<Option<PortalBehaviour>>>,
    png: Arc<Mutex<Vec<u8>>>,
    calls: Arc<AtomicUsize>,
    /// PNG 落盘目录（每个请求生成独立文件）。
    root: PathBuf,
}

impl ScreenshotMock {
    /// 成功返回的 mock（PNG 字节由测试给定）。
    pub fn new(root: PathBuf, png: Vec<u8>) -> Self {
        Self {
            behaviour: Arc::new(Mutex::new(PortalBehaviour::Success)),
            interactive_behaviour: Arc::new(Mutex::new(None)),
            png: Arc::new(Mutex::new(png)),
            calls: Arc::new(AtomicUsize::new(0)),
            root,
        }
    }

    /// 切换后续调用的行为（测试分阶段驱动）。
    pub fn set_behaviour(&self, behaviour: PortalBehaviour) {
        *self.behaviour.lock().expect("portal behaviour") = behaviour;
    }

    /// 为 `interactive=true` 的调用单独设定行为（None = 与静默行为一致）。
    pub fn set_interactive_behaviour(&self, behaviour: PortalBehaviour) {
        *self.interactive_behaviour.lock().expect("portal behaviour") = Some(behaviour);
    }

    /// 替换后续成功响应落盘的 PNG 字节（分阶段驱动）。
    pub fn set_png(&self, png: Vec<u8>) {
        *self.png.lock().expect("portal png") = png;
    }

    /// 已收到的 Screenshot 方法调用数。
    pub fn calls(&self) -> usize {
        self.calls.load(Ordering::Relaxed)
    }

    fn next_png(&self) -> PathBuf {
        let path = self.root.join(format!(
            "portal-shot-{}.png",
            self.calls.load(Ordering::Relaxed)
        ));
        std::fs::write(&path, self.png.lock().expect("portal png").clone())
            .expect("write mock portal png");
        path
    }
}

#[zbus::interface(name = "org.freedesktop.portal.Screenshot")]
impl ScreenshotMock {
    /// `Screenshot(s parent_window, a{sv} options) -> o handle`。
    ///
    /// 返回按 handle_token 规则预算的 Request 路径，并在返回前发出 `Response`
    /// ——客户端在调用前已订阅该路径，信号不会丢失。
    async fn screenshot(
        &self,
        _parent_window: String,
        options: HashMap<String, OwnedValue>,
        #[zbus(connection)] conn: &zbus::Connection,
        #[zbus(header)] header: zbus::message::Header<'_>,
    ) -> zbus::fdo::Result<zbus::zvariant::OwnedObjectPath> {
        self.calls.fetch_add(1, Ordering::Relaxed);
        let token = options
            .get("handle_token")
            .and_then(|v| String::try_from(v.clone()).ok())
            .unwrap_or_else(|| "agent-shell-acceptance".into());
        let sender = header
            .sender()
            .map(|s| s.as_str().trim_start_matches(':').replace('.', "_"))
            .unwrap_or_else(|| "unknown".into());
        let request_path = format!("/org/freedesktop/portal/desktop/request/{sender}/{token}");

        let interactive = options
            .get("interactive")
            .and_then(|v| bool::try_from(v.clone()).ok())
            .unwrap_or(false);
        let behaviour = if interactive {
            self.interactive_behaviour
                .lock()
                .expect("portal behaviour")
                .unwrap_or_else(|| *self.behaviour.lock().expect("portal behaviour"))
        } else {
            *self.behaviour.lock().expect("portal behaviour")
        };
        let code: u32 = match behaviour {
            PortalBehaviour::Success => 0,
            PortalBehaviour::Cancel => 1,
        };
        let mut results: HashMap<String, OwnedValue> = HashMap::new();
        if behaviour == PortalBehaviour::Success {
            let path = self.next_png();
            let uri = format!("file://{}", path.display());
            results.insert(
                "uri".into(),
                OwnedValue::try_from(zvariant_string(&uri)).expect("uri value"),
            );
        }
        conn.emit_signal(
            None::<&str>,
            request_path.as_str(),
            "org.freedesktop.portal.Request",
            "Response",
            &(code, results),
        )
        .await
        .map_err(|e| zbus::fdo::Error::Failed(format!("emit Response: {e}")))?;

        zbus::zvariant::OwnedObjectPath::try_from(request_path)
            .map_err(|e| zbus::fdo::Error::Failed(format!("request path: {e}")))
    }
}

fn zvariant_string(value: &str) -> zbus::zvariant::Value<'static> {
    zbus::zvariant::Value::from(value.to_string())
}

/// 在私有 bus 上启动成功/拒绝可控的 Screenshot portal。
pub async fn serve_screenshot_portal(bus: &PrivateBus, mock: ScreenshotMock) -> zbus::Connection {
    let server = bus.serve(PORTAL_PATH, mock).await;
    claim(&server, PORTAL_SERVICE).await;
    server
}
