//! 验收用例公共夹具（实现仓 `tests/acceptance/`）。
//!
//! 与设计仓 `03-acceptance.md` 的用例标识一一对应：一个 TC 一个测试目标
//! （`TC-<id>.rs` + `[[test]]`），本模块提供它们共用的夹具。
//!
//! 判定面是真实 daemon 的协议与事件面：装配走 `Daemon::connect`、请求走
//! `dispatch`、事件走 `EventHub` / `EventRing`——与合成器归一化管线同一枢纽、
//! 同一缓冲。CI 与验收机上没有图形会话，事件因此由夹具注入而不是由合成器产生；
//! 订阅侧的预过滤、回放侧的过滤与计数仍全部在生产路径上执行。
//!
//! 会话隔离与并发约束见 [`headless_session`]：一个测试目标内保持单个
//! `#[tokio::test]`。

pub mod a11y;
pub mod apparatus;
pub mod bus;
pub mod compositor;
pub mod events;
pub mod input;
pub mod journey;
pub mod portal;

pub use a11y::{element, FakeA11y};
pub use bus::PrivateBus;
pub use compositor::{window, workspace, FakeCompositor, FakeDesktop};
pub use events::RecordingRawSource;
pub use input::{RecordedOp, RecordingInput, SetupBehaviour};
pub use portal::{PortalBehaviour, ScreenshotMock};

use std::path::{Path, PathBuf};
use std::time::{Duration, Instant};

use agent_shell_core::security::{AgentShellConfig, SecurityManager};
use agent_shell_core::types::{
    DesktopEnvironment, Rect, WindowId, WindowInfo, WindowType, WorkspaceId, WorkspaceInfo,
};
use agent_shell_daemon::dispatch;
use agent_shell_daemon::state::Daemon;
use agent_shell_rpc::{method, Request, Response, RpcError};
use event::{DesktopEvent, EventSource, EventSubscription};
use serde_json::{json, Value};
use tempfile::TempDir;

/// 夹具 daemon 的空闲时限：夹具不跑 serve 循环，取值只落在 daemon 状态字段上。
const IDLE_TIMEOUT: Duration = Duration::from_secs(30);

/// 请求标识：单连接内唯一即可，夹具固定一个值。
const REQUEST_ID: u64 = 1;

/// 实现仓根目录（含 `[workspace]` 的根 `Cargo.toml` 所在目录）。
///
/// 构建期约束组（TC-tech-01/03/04/05）以 `cargo metadata` 与根清单的构建事实
/// 取证，需要从测试进程定位工作区根；`CARGO_MANIFEST_DIR` 在本包内是
/// `tests/acceptance/`（工作区根下两级），故向上逐级找含 `[workspace]` 的清单。
pub fn workspace_root() -> PathBuf {
    let mut dir = Path::new(env!("CARGO_MANIFEST_DIR")).to_path_buf();
    loop {
        let manifest = dir.join("Cargo.toml");
        if manifest.is_file()
            && std::fs::read_to_string(&manifest).is_ok_and(|text| text.contains("[workspace]"))
        {
            return dir;
        }
        assert!(
            dir.pop(),
            "未在 {} 向上找到含 [workspace] 的根清单",
            env!("CARGO_MANIFEST_DIR")
        );
    }
}

/// 验收夹具：装配完成的真实 daemon，事件面可注入。
pub struct Fixture {
    daemon: Daemon,
    /// 隔离根目录，随夹具存活（会话、状态与配置路径都指向它，见 [`headless_session`]）。
    _sandbox: TempDir,
}

impl Fixture {
    /// 装配夹具：隔离 ambient 会话与用户目录后走生产装配路径。
    ///
    /// 判定对象是事件面，与会话后端无关；`caller_id` / `security` 两个覆盖与
    /// daemon 单测基座同口径——环境身份不参与判定，审计路径仍按生产解析落到
    /// 夹具目录内。
    pub async fn start() -> Self {
        Self::start_with_env(&[]).await
    }

    /// [`Self::start`] 的扩展：装配前先施加额外环境覆盖（最后一次出现者胜）。
    ///
    /// 只供确有需要改变生产环境解析的用例（如 TC-tech-06 把 sysfs 根指向空目录、
    /// PATH 置空隐藏 `brightnessctl`）使用；覆盖必须在 [`headless_session`] 之后
    /// 应用，避免被夹具的隔离值冲掉。
    pub async fn start_with_env(extra: &[(&str, String)]) -> Self {
        let sandbox = headless_session();
        for (key, value) in extra {
            std::env::set_var(key, value);
        }
        let mut daemon = Daemon::connect(IDLE_TIMEOUT).await;
        daemon.caller_id = "*".into();
        daemon.security = SecurityManager::with_config(AgentShellConfig::default());
        Self {
            daemon,
            _sandbox: sandbox,
        }
    }

    /// 隔离根目录：本轮旅程的状态、配置与会话路径都必须落在它之内。
    pub fn sandbox_root(&self) -> &Path {
        self._sandbox.path()
    }

    /// 设定调用方身份：审计按调用方留痕（C-107），验收据此把本轮与其它运行区分开。
    pub fn set_caller_id(&mut self, caller_id: &str) {
        self.daemon.caller_id = caller_id.into();
    }

    /// 写入 ScreenCast 恢复令牌：capture 的 `restore_token` 后端就是 daemon 持有的
    /// 会话管理器（装配期注入），故这是该通道在验收环境下的真实写入面。
    pub fn save_screencast_token(&self, token: &str) -> Result<(), String> {
        // kind 与 persist_mode 同 `TokenStore` 实现：screencast / persist_until_revoked。
        self.daemon
            .portal_sessions
            .set_token("screencast", token.to_string(), 2)
    }

    /// 注入输入候选链（夹具用）。
    ///
    /// 生产输入装配依赖本机环境探测（portal / ydotool / `/dev/uinput` / 原生
    /// 协议握手），在 CI 与验收机上不可复现；`input.click --at` 的判定面是
    /// **已装配好的链上的候选选择与降级语义**，故由夹具给出确定性候选链，
    /// 请求仍走真实 `dispatch` 与真实 dispatcher（TC-input-03）。
    pub async fn set_input_chain(
        &mut self,
        de_type: DesktopEnvironment,
        candidates: Vec<Box<dyn agent_shell_input::InputService>>,
    ) {
        let handle = agent_shell_input::InputComponentHandle::from_candidates(de_type, candidates)
            .await
            .expect("fixture chain must have at least one available candidate");
        self.daemon.input = Some(handle);
    }

    /// 当前输入链的 active 后端名（判定「能力缺口不得摘除候选」用）。
    pub fn input_backend_name(&self) -> Option<&'static str> {
        self.daemon.input.as_ref().and_then(|i| i.backend_name())
    }

    /// 当前输入链的候选后端名（链序即生产选择顺序）。
    pub fn input_chain_names(&self) -> Vec<&'static str> {
        self.daemon
            .input
            .as_ref()
            .map(|i| i.dispatcher().backend_names())
            .unwrap_or_default()
    }

    /// 让输入组件缺席并给出装配根因（doctor/input.send 的错误透出面）。
    pub fn set_input_unavailable(&mut self, reason: Option<&str>) {
        self.daemon.input = None;
        self.daemon.input_error = reason.map(str::to_string);
    }

    /// 注入验收夹具合成器；后续窗口、工作区与 doctor 语义走真实 dispatch。
    #[cfg(debug_assertions)]
    pub fn install_compositor(&mut self, compositor: &FakeCompositor, de_type: DesktopEnvironment) {
        self.daemon.install_acceptance_compositor(
            std::sync::Arc::new(compositor.clone()),
            de_type,
            event::EventSource::KWinWayland,
        );
    }

    /// 替换合成器 doctor 子行（懒启动前后装载态的观测点）。
    #[cfg(debug_assertions)]
    pub fn set_compositor_doctor_lines(&self, lines: Vec<String>) {
        self.daemon.set_acceptance_doctor_lines(lines);
    }

    /// 设定首次订阅交付的原始事件源（订阅装配的观测点）。
    #[cfg(debug_assertions)]
    pub fn set_compositor_raw_source(&self, source: Option<Box<dyn event::RawSource>>) {
        self.daemon.set_acceptance_raw_source(source);
    }

    /// 事件管线状态：`(是否已装配, 原始源被取走次数)`。
    #[cfg(debug_assertions)]
    pub fn pipeline_state(&self) -> (bool, usize) {
        (
            self.daemon.acceptance_event_pipeline_started(),
            self.daemon.acceptance_raw_source_taken(),
        )
    }

    /// 注入无障碍夹具。
    pub fn set_a11y(&mut self, a11y: FakeA11y) {
        self.daemon.a11y = Some(std::sync::Arc::new(a11y));
    }

    /// 移除无障碍组件（判定不可达分支）。
    pub fn clear_a11y(&mut self) {
        self.daemon.a11y = None;
    }

    /// 裸 daemon 的只读/可变借用：验收用例按需注入其余组件（亮度、MIME、
    /// 外设、快捷键、rootd 连接器）——这些字段都是公开装配面。
    pub fn daemon(&self) -> &Daemon {
        &self.daemon
    }

    /// 见 [`Self::daemon`]。
    pub fn daemon_mut(&mut self) -> &mut Daemon {
        &mut self.daemon
    }

    /// 发一条 JSON-RPC 请求（真实 `dispatch` 入口）。
    pub async fn call(&mut self, method_name: &str, params: Option<Value>) -> Response {
        let request = match params {
            Some(params) => Request::new(REQUEST_ID, method_name, params),
            None => Request::without_params(REQUEST_ID, method_name),
        };
        dispatch::dispatch(&mut self.daemon, &request).await
    }

    /// 发一条请求并要求成功，返回结果对象。
    pub async fn call_ok(&mut self, method_name: &str, params: Option<Value>) -> Value {
        let response = self.call(method_name, params).await;
        match (response.result, response.error) {
            (Some(result), None) => result,
            (_, Some(error)) => panic!("{method_name} failed: {} ({})", error.code, error.message),
            (None, None) => panic!("{method_name} returned neither result nor error"),
        }
    }

    /// 发一条请求并要求失败，返回 RPC 错误。
    pub async fn call_err(&mut self, method_name: &str, params: Option<Value>) -> RpcError {
        let response = self.call(method_name, params).await;
        match (response.result, response.error) {
            (None, Some(error)) => error,
            (Some(result), _) => panic!("{method_name} unexpectedly succeeded: {result}"),
            (None, None) => panic!("{method_name} returned neither result nor error"),
        }
    }

    /// `events.subscribe`：成功返回订阅句柄，失败返回 RPC 错误。
    ///
    /// 句柄从 `daemon.subscriptions` 取走（与 serve 循环取走它的位置一致），
    /// 后续交付因此与「客户端经 notification 收到的集合」同源。
    pub async fn subscribe(&mut self, filter: Option<&str>) -> Result<Subscription, RpcError> {
        let params = filter.map(|filter| json!({ "filter": filter }));
        let response = self.call(method::EVENTS_SUBSCRIBE, params).await;
        if let Some(error) = response.error {
            return Err(error);
        }
        let result = response.result.expect("subscribe result");
        let id = result
            .get("subscriber_id")
            .and_then(Value::as_str)
            .expect("subscribe result carries subscriber_id")
            .to_string();
        let mut handles = std::mem::take(&mut self.daemon.subscriptions);
        assert_eq!(handles.len(), 1, "one subscribe creates exactly one handle");
        Ok(Subscription {
            id,
            handle: handles.pop().expect("subscription handle"),
        })
    }

    /// 当前订阅者数（`daemon status` 报出的同一计数）。
    pub fn subscriber_count(&self) -> usize {
        self.daemon.subscriber_count()
    }

    /// 向事件枢纽发布事件（C-086 的订阅者预过滤在同一枢纽内生效；投递在返回前
    /// 完成，故随后 `drain` 无需等待）。
    pub async fn publish(&self, events: &[DesktopEvent]) {
        for event in events {
            self.daemon.hub.publish(event.clone()).await;
        }
    }

    /// 写入回放环形缓冲（C-087：与事件归一化管线写入同一缓冲）。
    pub fn push_history(&self, events: &[DesktopEvent]) {
        for event in events {
            self.daemon.ring.push(event.clone());
        }
    }
}

/// 已建立的订阅句柄。
pub struct Subscription {
    id: String,
    handle: EventSubscription,
}

impl Subscription {
    /// `events.subscribe` 返回的订阅标识。
    pub fn id(&self) -> &str {
        &self.id
    }

    /// 排空已交付事件：即 serve 循环会转发给客户端的那些（被过滤器丢弃的不在此）。
    pub fn drain(&mut self) -> Vec<DesktopEvent> {
        let mut received = Vec::new();
        while let Some(event) = self.handle.try_recv() {
            received.push(event);
        }
        received
    }
}

/// 「订阅 → 发布 → 取交付集合」一步完成，返回已交付事件的线上变体名。
pub async fn delivery(
    fixture: &mut Fixture,
    filter: Option<&str>,
    events: &[DesktopEvent],
) -> Vec<String> {
    let mut subscription = fixture.subscribe(filter).await.unwrap_or_else(|error| {
        panic!(
            "subscribe with filter {filter:?} failed: {} ({})",
            error.code, error.message
        )
    });
    fixture.publish(events).await;
    wire_names(&subscription.drain())
}

/// 事件序列化后的变体名（C-082 外部标签对象：变体名为键）。
///
/// 判定面取客户端实际收到的形状，而不是 crate 内部枚举。
pub fn json_wire_name(value: &Value) -> String {
    match value {
        Value::Object(map) => map
            .keys()
            .next()
            .cloned()
            .expect("externally tagged event object"),
        Value::String(name) => name.clone(),
        other => panic!("unexpected event encoding: {other}"),
    }
}

/// 事件的线上变体名，见 [`json_wire_name`]。
pub fn wire_name(event: &DesktopEvent) -> String {
    json_wire_name(&serde_json::to_value(event).expect("DesktopEvent is serializable"))
}

/// 一组事件的线上变体名。
pub fn wire_names(events: &[DesktopEvent]) -> Vec<String> {
    events.iter().map(wire_name).collect()
}

/// 窗口打开事件。
pub fn window_opened(native_id: &str) -> DesktopEvent {
    DesktopEvent::WindowOpened {
        info: window_info(native_id),
        source: event_source(),
        occurred_at: Instant::now(),
    }
}

/// 窗口聚焦事件。
pub fn window_focused(native_id: &str) -> DesktopEvent {
    DesktopEvent::WindowFocused {
        info: window_info(native_id),
        source: event_source(),
        occurred_at: Instant::now(),
    }
}

/// 窗口关闭事件。
pub fn window_closed(native_id: &str) -> DesktopEvent {
    DesktopEvent::WindowClosed {
        id: window_id(native_id),
        source: event_source(),
        occurred_at: Instant::now(),
    }
}

/// 工作区切换事件（窗口类别之外的负向样本）。
pub fn workspace_changed() -> DesktopEvent {
    DesktopEvent::WorkspaceChanged {
        info: WorkspaceInfo {
            id: WorkspaceId {
                native_id: "1".into(),
                de_type: DesktopEnvironment::KDE,
            },
            name: "1".into(),
            number: 1,
            is_active: true,
            monitor_ids: vec![],
            window_ids: vec![],
        },
        source: event_source(),
        occurred_at: Instant::now(),
    }
}

/// 注入事件的事件来源：本版事件产出以 KWin 系为准（C-085），与无合成器会话下
/// daemon 自报的来源一致。
fn event_source() -> EventSource {
    EventSource::KWinWayland
}

/// 窗口标识：桌面环境标签按 KWin 系会话（C-091：标识归属由会话检测决定）。
fn window_id(native_id: &str) -> WindowId {
    WindowId {
        native_id: native_id.into(),
        de_type: DesktopEnvironment::KDE,
    }
}

/// 窗口信息（字段形状见 C-081；用例只取事件种类）。
fn window_info(native_id: &str) -> WindowInfo {
    let geometry = Rect {
        x: 0,
        y: 0,
        width: 800,
        height: 600,
    };
    WindowInfo {
        id: window_id(native_id),
        title: format!("window-{native_id}"),
        app_id: "acceptance".into(),
        pid: 1000,
        geometry,
        frame_geometry: geometry,
        states: vec![],
        workspace_id: None,
        monitor_id: None,
        stacking_order: 0,
        desktop_file: None,
        window_type: WindowType::Normal,
        icon_geometry: None,
        keep_above: false,
    }
}

/// 抹掉 ambient 图形会话与用户目录，返回夹具的临时根目录。
///
/// 不隔离时，有桌面会话的机器上 `Daemon::connect` 会装配真实合成器，而
/// `events.subscribe` 会把事件脚本装进用户正在使用的合成器并常驻（C-092）；
/// 每次 `dispatch` 还会在门禁处按 C-107 追加审计记录，写进真实 `$HOME` 的状态
/// 目录。验收用例不得对被测主机产生这两种副作用。
///
/// 隔离的是目录位置，不是行为：审计、配置与会话状态仍走各自的生产解析路径
/// （`XDG_STATE_HOME` → `~/.local/state/agent-shell`、`XDG_CONFIG_HOME` →
/// `~/.config/agent-shell`），只是落在夹具的临时目录内。
///
/// 环境变量是进程级状态：一个测试目标内保持单个 `#[tokio::test]`，装配前调用
/// 一次，进程内不存在并发读写。
fn headless_session() -> TempDir {
    for key in [
        "AGENT_SHELL_AGENT_ID",
        "DBUS_SESSION_BUS_ADDRESS",
        "DESKTOP_SESSION",
        "DISPLAY",
        "HYPRLAND_INSTANCE_SIGNATURE",
        "WAYLAND_DISPLAY",
        "WAYLAND_SOCKET",
        "XDG_CURRENT_DESKTOP",
        "XDG_SESSION_DESKTOP",
    ] {
        std::env::remove_var(key);
    }
    std::env::set_var("XDG_SESSION_TYPE", "tty");
    let root = TempDir::new().expect("temporary fixture root");
    // 图形面：临时 XDG_RUNTIME_DIR 摘掉 `$XDG_RUNTIME_DIR/bus` 这条 session bus
    // 入口，DE 检测的 D-Bus 兜底与各组件的 `Connection::session()` 因此确定性
    // 不可达。
    std::env::set_var("XDG_RUNTIME_DIR", root.path());
    // 状态/配置面：审计留痕、配置读写与会话状态都落在夹具目录内。
    std::env::set_var("HOME", root.path().join("home"));
    std::env::set_var("XDG_CONFIG_HOME", root.path().join("config"));
    std::env::set_var("XDG_STATE_HOME", root.path().join("state"));
    root
}

/// 4×3 RGB 样例 PNG（非全黑）：供 portal 夹具产出可被捕获链校验的截图。
///
/// 字节内置而非运行时编码：验收夹具不引入图像编码依赖，产物确定。
pub fn sample_png() -> Vec<u8> {
    vec![
        137, 80, 78, 71, 13, 10, 26, 10, 0, 0, 0, 13, 73, 72, 68, 82, 0, 0, 0, 4, 0, 0, 0, 3, 8, 2,
        0, 0, 0, 59, 150, 57, 145, 0, 0, 0, 48, 73, 68, 65, 84, 120, 218, 99, 224, 18, 145, 115,
        19, 113, 107, 18, 201, 219, 39, 50, 141, 129, 43, 197, 205, 45, 37, 175, 41, 101, 218, 190,
        148, 125, 12, 92, 91, 242, 220, 182, 76, 107, 218, 178, 111, 223, 150, 103, 0, 234, 15, 15,
        121, 53, 212, 26, 182, 0, 0, 0, 0, 73, 69, 78, 68, 174, 66, 96, 130,
    ]
}

/// 全黑 PNG（1×1）：验证捕获链拒绝全黑产物。
pub fn black_png() -> Vec<u8> {
    // 1×1 RGB(0,0,0)，PNG 编码固定。
    vec![
        137, 80, 78, 71, 13, 10, 26, 10, 0, 0, 0, 13, 73, 72, 68, 82, 0, 0, 0, 1, 0, 0, 0, 1, 8, 2,
        0, 0, 0, 144, 119, 83, 222, 0, 0, 0, 12, 73, 68, 65, 84, 120, 218, 99, 104, 96, 96, 0, 0,
        0, 4, 0, 1, 155, 200, 195, 167, 0, 0, 0, 0, 73, 69, 78, 68, 174, 66, 96, 130,
    ]
}
