//! agent-shell CLI-daemon JSON-RPC 2.0 协议（设计文档 §22.2 D1）。
//!
//! CLI 是瞬态无状态客户端，daemon 持有全部系统连接。本 crate 定义双方
//! 共享的：请求/响应信封、方法名常量、参数与结果载荷（serde 序列化）。
//! 传输为 JSON-RPC 2.0 over stdio——每行一个消息（newline-delimited）。

use serde::{Deserialize, Serialize};
use serde_json::Value;

/// 输入按键模型（CLI 与 daemon 共用，§22.2 协议载荷）。
///
/// CLI 侧解析组合键语法（快速失败），daemon 侧执行注入；双方以本模块
/// 的类型为协议契约——避免两套定义漂移（审查项 #3）。
pub mod keys {
    use serde::{Deserialize, Serialize};

    /// 按键组合。
    #[derive(Clone, Debug, PartialEq, Eq, Hash, Serialize, Deserialize)]
    pub struct KeyCombo {
        pub keys: Vec<Key>,
        pub modifiers: ModifierMask,
    }

    /// 单个按键。
    #[derive(Clone, Debug, PartialEq, Eq, Hash, Serialize, Deserialize)]
    pub enum Key {
        Char(char),
        Named(KeyName),
    }

    /// 命名按键（F1..F12 全量；F1/F2 曾被遗漏，审查锚定）。
    #[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, Serialize, Deserialize)]
    pub enum KeyName {
        Return,
        Escape,
        BackSpace,
        Tab,
        Space,
        Left,
        Right,
        Up,
        Down,
        Home,
        End,
        PageUp,
        PageDown,
        Insert,
        Delete,
        Menu,
        F1,
        F2,
        F3,
        F4,
        F5,
        F6,
        F7,
        F8,
        F9,
        F10,
        F11,
        F12,
    }

    /// 修饰键掩码。
    #[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Hash, Serialize, Deserialize)]
    pub struct ModifierMask {
        pub ctrl: bool,
        pub alt: bool,
        pub shift: bool,
        pub meta: bool,
    }
}

/// 方法名常量（`域.操作` 命名，§22.2 协议示例 `windows.list`）。
pub mod method {
    /// 完整健康诊断。
    pub const DOCTOR: &str = "doctor.run";
    /// DE/后端能力报告。
    pub const INFO: &str = "info.show";
    /// 窗口列表。
    pub const WINDOWS_LIST: &str = "windows.list";
    /// 单窗查询。
    pub const WINDOW_INFO: &str = "windows.info";
    /// 窗口写操作（focus/move/resize/minimize/close，按 params.op 分派）。
    pub const WINDOW_OP: &str = "windows.op";
    /// 工作区列表。
    pub const WORKSPACES_LIST: &str = "workspaces.list";
    /// 工作区切换。
    pub const WORKSPACE_SWITCH: &str = "workspaces.switch";
    /// 输入注入。
    pub const INPUT_SEND: &str = "input.send";
    /// 截图落盘（daemon 侧执行捕获与写文件）。
    pub const SCREENSHOT_CAPTURE: &str = "screenshot.capture";
    /// AT-SPI Registry 可达性探测。
    pub const A11Y_STATUS: &str = "a11y.status";
    /// 语义查询（role/name 过滤，§14.3）。
    pub const A11Y_QUERY: &str = "a11y.query";
    // ── 事件（§22.5 D4）──
    /// 订阅事件流（返回 subscriber_id；事件经 notification 推送）。
    pub const EVENTS_SUBSCRIBE: &str = "events.subscribe";
    /// 取消订阅。
    pub const EVENTS_UNSUBSCRIBE: &str = "events.unsubscribe";
    /// 回放环形缓冲历史事件。
    pub const EVENTS_REPLAY: &str = "events.replay";
    /// 事件推送通知（daemon → 订阅者，无 id）。
    pub const EVENTS_NOTIFY: &str = "events.notify";
    // ── daemon 管理（§22.2）──
    /// daemon 状态查询。
    pub const DAEMON_STATUS: &str = "daemon.status";
    /// 查看 portal 会话。
    pub const DAEMON_SESSIONS: &str = "daemon.sessions";
    // ── IME（§22.8 D7）──
    /// 列出可用输入法引擎。
    pub const IME_ENGINE_LIST: &str = "ime.engine.list";
    /// 设置当前引擎。
    pub const IME_ENGINE_SET: &str = "ime.engine.set";
    /// 查询当前引擎。
    pub const IME_ENGINE_CURRENT: &str = "ime.engine.current";
    /// 通过 IME 输入文本（非 ASCII）。
    pub const IME_TYPE: &str = "ime.type";
    // ── 扩展系统服务（§21.35）──
    /// 安全状态。
    pub const SECURITY_STATUS: &str = "security.status";
    /// 授权操作。
    pub const SECURITY_GRANT: &str = "security.grant";
    /// 撤销授权。
    pub const SECURITY_REVOKE: &str = "security.revoke";
    /// 审计日志。
    pub const SECURITY_AUDIT: &str = "security.audit";
    /// 获取亮度。
    pub const BRIGHTNESS_GET: &str = "brightness.get";
    /// 设置亮度。
    pub const BRIGHTNESS_SET: &str = "brightness.set";
    /// 文件选择器。
    pub const FILE_PICK: &str = "file.pick";
    /// 文件删除到回收站。
    pub const FILE_TRASH: &str = "file.trash";
    /// 打开目录。
    pub const FILE_OPEN_DIR: &str = "file.open_directory";
    /// 查询默认应用。
    pub const MIME_GET: &str = "mime.get";
    /// 设置默认应用。
    pub const MIME_SET: &str = "mime.set";
    /// 默认浏览器。
    pub const MIME_DEFAULT_BROWSER: &str = "mime.default_browser";
    /// 蓝牙扫描。
    pub const BLUETOOTH_SCAN: &str = "bluetooth.scan";
    /// 蓝牙连接。
    pub const BLUETOOTH_CONNECT: &str = "bluetooth.connect";
    /// 蓝牙断开。
    pub const BLUETOOTH_DISCONNECT: &str = "bluetooth.disconnect";
    /// 蓝牙列表。
    pub const BLUETOOTH_LIST: &str = "bluetooth.list";
    /// Flatpak 列表。
    pub const FLATPAK_LIST: &str = "flatpak.list";
    /// Flatpak 安装。
    pub const FLATPAK_INSTALL: &str = "flatpak.install";
    /// 软件更新检查。
    pub const SOFTWARE_UPDATES: &str = "software.updates";
    /// 触控板状态。
    pub const TOUCHPAD_STATUS: &str = "touchpad.status";
    /// 触控板设置。
    pub const TOUCHPAD_SET: &str = "touchpad.set";
    /// 键盘布局列表。
    pub const KBD_LAYOUT_LIST: &str = "kbd.layout.list";
    /// 键盘布局设置。
    pub const KBD_LAYOUT_SET: &str = "kbd.layout.set";
    /// 密钥存储。
    pub const SECRET_SET: &str = "secret.set";
    /// 密钥读取。
    pub const SECRET_GET: &str = "secret.get";
    /// 快捷键绑定。
    pub const SHORTCUT_BIND: &str = "shortcut.bind";
    /// 快捷键触发。
    pub const SHORTCUT_TRIGGER: &str = "shortcut.trigger";
    /// Timer 列表。
    pub const TIMER_LIST: &str = "timer.list";
    /// Timer 下次触发。
    pub const TIMER_NEXT: &str = "timer.next";
    // ── rootd 特权代理（§23.4）──
    /// 启停系统服务（rootd ServiceStart/Stop/Restart）。
    pub const SERVICE_CONTROL: &str = "service.control";
    /// 查看系统日志（rootd JournalQuery）。
    pub const SYSTEM_LOG_VIEW: &str = "system-log.view";
    /// rootd 版本对账（rootd Hello）。
    pub const ROOTD_HELLO: &str = "rootd.hello";
    /// 设置系统主机名（rootd HostnameSet）。
    pub const HOSTNAME_SET: &str = "hostname.set";
}

#[derive(Clone, Debug, Serialize, Deserialize, PartialEq)]
pub struct Request {
    pub jsonrpc: String,
    pub id: u64,
    pub method: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub params: Option<Value>,
}

impl Request {
    /// 构造请求；`jsonrpc` 字段固定 `"2.0"`。
    pub fn new(id: u64, method: &str, params: Value) -> Self {
        Self {
            jsonrpc: "2.0".into(),
            id,
            method: method.to_string(),
            params: Some(params),
        }
    }

    /// 无参请求。
    pub fn without_params(id: u64, method: &str) -> Self {
        Self {
            jsonrpc: "2.0".into(),
            id,
            method: method.to_string(),
            params: None,
        }
    }

    pub fn to_line(&self) -> String {
        let mut s = serde_json::to_string(self).expect("Request is JSON-serializable");
        s.push('\n');
        s
    }

    /// 解析一行；校验 `jsonrpc: "2.0"` 标识。
    pub fn from_line(line: &str) -> Result<Self, String> {
        let req: Request =
            serde_json::from_str(line).map_err(|e| format!("bad request line: {e}"))?;
        if req.jsonrpc != "2.0" {
            return Err(format!("unsupported jsonrpc version: {}", req.jsonrpc));
        }
        Ok(req)
    }
}

/// JSON-RPC 2.0 响应（daemon → CLI）。成功携带 `result`，失败携带 `error`。
#[derive(Clone, Debug, Serialize, Deserialize, PartialEq)]
pub struct Response {
    pub jsonrpc: String,
    pub id: u64,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub result: Option<Value>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub error: Option<RpcError>,
}

impl Response {
    pub fn ok(id: u64, result: Value) -> Self {
        Self {
            jsonrpc: "2.0".into(),
            id,
            result: Some(result),
            error: None,
        }
    }

    pub fn err(id: u64, code: RpcErrorCode, message: impl Into<String>) -> Self {
        Self {
            jsonrpc: "2.0".into(),
            id,
            result: None,
            error: Some(RpcError {
                code: code as i32,
                message: message.into(),
                data: None,
            }),
        }
    }

    pub fn to_line(&self) -> String {
        let mut s = serde_json::to_string(self).expect("Response is JSON-serializable");
        s.push('\n');
        s
    }

    pub fn from_line(line: &str) -> Result<Self, String> {
        serde_json::from_str(line).map_err(|e| format!("bad response line: {e}"))
    }
}

/// JSON-RPC 2.0 通知（daemon → 订阅者，无 id 字段，§22.5 D4）。
///
/// 事件订阅（`events.subscribe`）生效后，daemon 在每一条匹配事件上以
/// `events.notify` 方法推送一行通知；同一 stdio 连接上的普通请求响应
/// 与通知共享行流，接收端按 `id` 有无区分。
#[derive(Clone, Debug, Serialize, Deserialize, PartialEq)]
pub struct Notification {
    pub jsonrpc: String,
    pub method: String,
    pub params: Value,
}

impl Notification {
    /// 构造事件推送通知。
    pub fn event(params: Value) -> Self {
        Self {
            jsonrpc: "2.0".into(),
            method: method::EVENTS_NOTIFY.into(),
            params,
        }
    }

    pub fn to_line(&self) -> String {
        let mut s = serde_json::to_string(self).expect("Notification is JSON-serializable");
        s.push('\n');
        s
    }
}

/// 错误对象。code 采用 JSON-RPC 2.0 规范保留区间 + 应用自定义区间。
#[derive(Clone, Debug, Serialize, Deserialize, PartialEq)]
pub struct RpcError {
    pub code: i32,
    pub message: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub data: Option<Value>,
}

/// 错误码（-32768..-32000 为规范保留；1000+ 为应用自定义）。
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
#[repr(i32)]
pub enum RpcErrorCode {
    ParseError = -32700,
    InvalidRequest = -32600,
    MethodNotFound = -32601,
    InvalidParams = -32602,
    InternalError = -32603,
    /// daemon 未运行且自动激活失败。
    DaemonUnavailable = 1001,
    /// 后端/组件不可用（会话类型不支持等）。
    BackendUnavailable = 1002,
    /// 目标不存在（窗口未找到等）。
    NotFound = 1003,
    /// 操作被拒绝（权限/策略）。
    Denied = 1004,
    /// 底层系统调用失败。
    BackendError = 1005,
    /// 需要用户确认（安全判定为 Confirm，纯后端无 UI 时短路返回）。
    ConfirmationRequired = 1006,
}

// ───────────────────────── 载荷定义 ─────────────────────────

/// windows.list 结果条目（§22.2 表格）。
///
/// `from_cache` 是查询结果级别的属性（整批命中缓存与否），不在逐项条目中
/// 重复——由响应外层的 `from_cache` 字段携带，避免逐项冗余与契约漂移。
#[derive(Clone, Debug, Serialize, Deserialize, PartialEq)]
pub struct WindowEntry {
    pub native_id: String,
    pub title: String,
    pub app_id: String,
    pub pid: u32,
    /// 窗口几何信息（§17.2 要求 windows.list 含 geometry）。
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub x: Option<i32>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub y: Option<i32>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub width: Option<i32>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub height: Option<i32>,
    /// 工作区标识（§17.2 要求 windows.list 含 workspace）。
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub workspace: Option<String>,
}

/// windows.op 参数。
#[derive(Clone, Debug, Serialize, Deserialize, PartialEq)]
pub struct WindowOpParams {
    pub op: WindowOpKind,
    pub target: String,
}

/// 窗口写操作种类。
#[derive(Clone, Copy, Debug, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum WindowOpKind {
    Focus,
    Move,
    Resize,
    Minimize,
    Close,
}

/// input.send 参数。
#[derive(Clone, Debug, Serialize, Deserialize, PartialEq)]
pub struct InputParams {
    pub kind: InputKind,
    /// key: 组合键串（"ctrl+c"）；type: 文本；click: 按钮 + 可选坐标；scroll: dx,dy。
    pub payload: Value,
}

/// 输入操作种类。
#[derive(Clone, Copy, Debug, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum InputKind {
    Key,
    TypeText,
    Click,
    Scroll,
}

/// screenshot.capture 参数（daemon 侧落盘后返回尺寸信息）。
#[derive(Clone, Debug, Serialize, Deserialize, PartialEq)]
pub struct CaptureParams {
    /// X11 window id 十进制串；None = root。
    pub window: Option<String>,
    /// 区域裁剪 X,Y,W,H。
    pub area: Option<[i32; 4]>,
    /// daemon 侧输出路径（CLI 与 daemon 同机同用户，共享文件系统）。
    pub output_path: String,
}

/// screenshot.capture 结果。
#[derive(Clone, Debug, Serialize, Deserialize, PartialEq)]
pub struct CaptureResult {
    pub width: u32,
    pub height: u32,
    pub path: String,
}

/// doctor.run 结果行集合（渲染由 CLI 完成）。
#[derive(Clone, Debug, Serialize, Deserialize, PartialEq)]
pub struct DoctorResult {
    pub lines: Vec<String>,
    pub healthy: bool,
}

/// info.show 结果。
#[derive(Clone, Debug, Serialize, Deserialize, PartialEq)]
pub struct InfoResult {
    /// DE 检测摘要（core detect_report 渲染）。
    pub detection: String,
    /// 能力位表：name → enabled。
    pub capabilities: Vec<(String, bool)>,
}

/// a11y.status 结果。
#[derive(Clone, Debug, Serialize, Deserialize, PartialEq)]
pub struct A11yStatusResult {
    pub available: bool,
    pub detail: String,
}

/// a11y.query 结果条目（daemon 侧 `ElementNode` 的协议投影，§14.3）。
///
/// rpc crate 保持与重依赖（zbus/a11y crate）解耦——daemon 把
/// `components/a11y` 的节点投影为纯数据字段，CLI/MCP 只消费本结构。
#[derive(Clone, Debug, Serialize, Deserialize, PartialEq)]
pub struct A11yElementResult {
    pub bus_name: String,
    pub path: String,
    pub name: String,
    pub role: String,
    pub role_code: u32,
    pub states: u64,
}

/// a11y.query 结果。
#[derive(Clone, Debug, Serialize, Deserialize, PartialEq)]
pub struct A11yQueryResult {
    pub count: usize,
    pub elements: Vec<A11yElementResult>,
}

// ───────────────────────── 事件 / daemon / IME 载荷 ─────────────────────────

/// IME type 结果（§22.8 D7）。
#[derive(Clone, Debug, Serialize, Deserialize, PartialEq)]
pub struct ImeTypeResult {
    pub committed: String,
    pub verified: bool,
}

/// daemon.status 结果（§22.2）。
#[derive(Clone, Debug, Serialize, Deserialize, PartialEq)]
pub struct DaemonStatusResult {
    pub running: bool,
    pub windows_cached: usize,
    pub subscribers: usize,
    pub ime_engine: Option<String>,
}

/// portal 会话条目（daemon.sessions 结果）。
#[derive(Clone, Debug, Serialize, Deserialize, PartialEq)]
pub struct SessionEntry {
    pub kind: String,
    pub restore_token: String,
    pub created_at: String,
    pub persist_mode: u32,
}

/// events.replay 结果。
#[derive(Clone, Debug, Serialize, Deserialize, PartialEq)]
pub struct EventsReplayResult {
    pub count: usize,
    pub events: Vec<Value>,
}

/// daemon 二进制定位（CLI 与 MCP 共享，TSI-2471）。
pub mod daemon_bin {
    /// daemon 二进制名。
    pub const NAME: &str = "agent-shell-daemon";

    /// 定位 daemon 二进制。
    ///
    /// 查找顺序（优先级递减）：
    /// 1. 同目录下——CLI/MCP 与 daemon 并列安装时命中；
    /// 2. workspace target 目录——开发构建产物，优先于 PATH，避免
    ///    `~/.local/bin` 等位置已安装的旧版遮蔽本次构建的新二进制
    ///    （TSI-2471）；
    /// 3. PATH——已安装（systemd user unit / 包管理器安装）兜底。
    ///
    /// `extra_target_dirs` 由调用方注入（cli 与 mcp 的 workspace 相对
    /// 层级不同），避免在本模块硬编码 crate 位置。
    pub fn find_daemon_binary(extra_target_dirs: &[&str]) -> Result<String, String> {
        let dirs = candidate_dirs(extra_target_dirs);
        for dir in &dirs {
            let p = std::path::Path::new(dir).join(NAME);
            if p.is_file() {
                return Ok(p.to_string_lossy().into_owned());
            }
        }
        Err(format!(
            "daemon binary `{NAME}` not found in candidate dirs: {}",
            dirs.join(", ")
        ))
    }

    /// 查找目录候选列表（顺序即优先级）。
    pub fn candidate_dirs(extra_target_dirs: &[&str]) -> Vec<String> {
        let mut dirs: Vec<String> = Vec::new();
        // 1. 同目录下（调用方自身所在目录）。
        if let Ok(exe) = std::env::current_exe() {
            if let Some(d) = exe.parent() {
                dirs.push(d.to_string_lossy().into_owned());
            }
        }
        // 2. workspace target 目录（开发构建产物，优先于 PATH）。
        //    extra_target_dirs 由调用方提供，指向 workspace 根下的 target。
        for candidate in extra_target_dirs {
            dirs.push(candidate.to_string());
        }
        // 3. PATH 兜底（已安装 daemon）。
        if let Ok(path) = std::env::var("PATH") {
            for dir in path.split(':') {
                if !dir.is_empty() {
                    dirs.push(dir.to_string());
                }
            }
        }
        dirs
    }

    #[cfg(test)]
    mod tests {
        use super::*;

        /// workspace target 目录必须排在 PATH 之前——TSI-2471 核心契约。
        /// 比较时跳过调用方自身所在目录（exe_dir，合法的绝对路径且
        /// 优先级更高），仅验证 PATH 兜底段在 target 之后。
        #[test]
        fn target_dirs_precede_path_entries() {
            let dirs = candidate_dirs(&["target/debug", "target/release"]);
            assert!(!dirs.is_empty(), "candidate_dirs must not be empty");
            let exe_dir = std::env::current_exe()
                .ok()
                .and_then(|e| e.parent().map(|p| p.to_string_lossy().into_owned()));
            let first_target_debug = dirs
                .iter()
                .position(|d| d == "target/debug")
                .expect("target/debug is a candidate");
            let first_path_entry = dirs
                .iter()
                .position(|d| {
                    std::path::Path::new(d).is_absolute() && Some(d.as_str()) != exe_dir.as_deref()
                })
                .unwrap_or(usize::MAX);
            assert!(
                first_target_debug < first_path_entry,
                "target/debug must precede any PATH-sourced absolute entry — \
                 got dirs={dirs:?}"
            );
        }

        /// target/debug 先于 target/release——debug 构建在开发周期中更新更频繁，
        /// 应优先命中以反映最新改动。
        #[test]
        fn debug_precedes_release() {
            let dirs = candidate_dirs(&["target/debug", "target/release"]);
            assert!(!dirs.is_empty());
            let dbg = dirs.iter().position(|d| d == "target/debug").unwrap();
            let rel = dirs.iter().position(|d| d == "target/release").unwrap();
            assert!(dbg < rel);
        }

        /// 同目录（调用方自身目录）优先级最高——adjacent binary 规则。
        #[test]
        fn exe_dir_is_first_if_available() {
            let dirs = candidate_dirs(&[]);
            assert!(!dirs.is_empty(), "candidate_dirs must not be empty");
            if let Ok(exe) = std::env::current_exe() {
                if let Some(parent) = exe.parent() {
                    assert_eq!(dirs[0], parent.to_string_lossy());
                }
            }
        }

        /// extra_target_dirs 注入的目录出现在 exe_dir 之后、PATH 之前。
        #[test]
        fn extra_dirs_between_exe_and_path() {
            let extra = "custom/target/path";
            let dirs = candidate_dirs(&[extra]);
            assert!(!dirs.is_empty());
            let extra_idx = dirs
                .iter()
                .position(|d| d == extra)
                .expect("extra dir must be present");
            let path_idx = dirs
                .iter()
                .position(|d| {
                    std::path::Path::new(d).is_absolute()
                        && d != extra
                        && std::env::var("PATH")
                            .map(|p| p.split(':').any(|entry| entry == d))
                            .unwrap_or(false)
                })
                .unwrap_or(usize::MAX);
            assert!(extra_idx < path_idx, "extra dir must precede PATH entries");
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn window_op_kind_snake_case() {
        assert_eq!(
            serde_json::to_value(WindowOpKind::Minimize).expect("ser"),
            json!("minimize")
        );
        assert_eq!(
            serde_json::from_value::<WindowOpKind>(json!("focus")).expect("de"),
            WindowOpKind::Focus
        );
    }

    /// 回归锚定（TSI-2439）：daemon windows_list 只发 native_id/title/app_id/pid
    /// 逐项，from_cache 仅在响应外层。WindowEntry 必须能从 daemon 实际发送的
    /// 逐项 JSON 反序列化——逐项 from_cache 字段会导致 missing field 反序列化失败。
    #[test]
    fn window_entry_deserializes_daemon_item_shape() {
        let item = json!({
            "native_id": "0x1200009",
            "title": "Editor — main.rs",
            "app_id": "org.kde.kate",
            "pid": 4242
        });
        let w: WindowEntry = serde_json::from_value(item).expect("de");
        assert_eq!(w.native_id, "0x1200009");
        assert_eq!(w.pid, 4242);
    }

    /// 响应外层 from_cache 仍可解析——它是查询结果级别字段，不属于逐项条目。
    #[test]
    fn windows_list_response_carries_top_level_from_cache() {
        let resp = json!({
            "windows": [
                {"native_id": "a", "title": "T", "app_id": "app", "pid": 1}
            ],
            "from_cache": true
        });
        #[derive(serde::Deserialize)]
        struct W {
            windows: Vec<WindowEntry>,
            from_cache: bool,
        }
        let w: W = serde_json::from_value(resp).expect("de");
        assert!(w.from_cache);
        assert_eq!(w.windows.len(), 1);
    }

    #[test]
    fn request_roundtrip_preserves_fields() {
        let r = Request::new(
            7,
            method::WINDOWS_LIST,
            serde_json::json!({"filter": "kate"}),
        );
        assert_eq!(Request::from_line(&r.to_line()).expect("parse"), r);
    }

    #[test]
    fn request_without_params_omits_field() {
        let r = Request::without_params(1, method::DOCTOR);
        assert!(!r.to_line().contains("params"));
        assert_eq!(Request::from_line(&r.to_line()).expect("parse"), r);
    }

    #[test]
    fn request_rejects_wrong_version() {
        assert!(Request::from_line(r#"{"jsonrpc":"1.0","id":1,"method":"x"}"#).is_err());
    }

    #[test]
    fn response_ok_and_err_roundtrip() {
        let ok = Response::ok(3, json!({"windows": []}));
        assert!(ok.error.is_none());
        assert_eq!(Response::from_line(&ok.to_line()).expect("parse"), ok);

        let err = Response::err(3, RpcErrorCode::NotFound, "window gone");
        assert!(err.result.is_none());
        assert_eq!(
            err.error.as_ref().expect("err").code,
            RpcErrorCode::NotFound as i32
        );
        assert_eq!(Response::from_line(&err.to_line()).expect("parse"), err);
    }

    #[test]
    fn input_params_deserializes_kinds() {
        let v = json!({"kind": "type_text", "payload": {"text": "hi"}});
        let p: InputParams = serde_json::from_value(v).expect("de");
        assert_eq!(p.kind, InputKind::TypeText);
        assert_eq!(p.payload["text"], "hi");
    }

    #[test]
    fn capture_params_carries_area() {
        let v = json!({"area": [0, 0, 800, 600], "output_path": "/tmp/s.ppm"});
        let p: CaptureParams = serde_json::from_value(v).expect("de");
        assert_eq!(p.area, Some([0, 0, 800, 600]));
        assert_eq!(p.window, None);
    }
}
