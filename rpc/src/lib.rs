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
}
/// JSON-RPC 2.0 请求（CLI → daemon）。
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
}

// ───────────────────────── 载荷定义 ─────────────────────────

/// windows.list 结果条目（daemon 缓存返回，含来源标注 §22.2 表格）。
#[derive(Clone, Debug, Serialize, Deserialize, PartialEq)]
pub struct WindowEntry {
    pub native_id: String,
    pub title: String,
    pub app_id: String,
    pub pid: u32,
    /// 查询是否命中 WindowStateCache（事件驱动缓存，§22.2）。
    pub from_cache: bool,
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
