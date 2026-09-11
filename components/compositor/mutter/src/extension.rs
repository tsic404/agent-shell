//! Shell Extension 路径（设计文档 §8.1 路径 B / §8.4，GNOME 47+ 推荐）。
//!
//! GNOME Shell Extension `agent-shell-bridge@tsic.top` 在 session bus
//! 注册 `org.gnome.Shell.AgentShell` 接口（XML 定义见 §8.1）。Rust 端
//! 只做客户端：调用方法、订阅信号。extension.js 本体由本 crate 内嵌
//! 常量交付（单一事实来源），安装流程落盘后启用。
//!
//! 信号语义：`WindowOpened(s info)` / `WindowClosed(u window_id)` /
//! `ActiveWindowChanged(s info)`——info 为与 ListWindows 单窗相同的 JSON。

use serde_json::Value;
use zbus::{Connection, MessageStream};

use crate::error::{MutterError, Result, AGENTSHELL_IFACE, AGENTSHELL_PATH, EXTENSION_ID};
use crate::eval::parse;

/// org.gnome.Shell.AgentShell proxy（extension.js 注册的接口）。
#[zbus::proxy(
    default_service = "org.gnome.Shell.AgentShell",
    default_path = "/org/gnome/Shell/AgentShell",
    interface = "org.gnome.Shell.AgentShell"
)]
trait AgentShell {
    /// 全部窗口 JSON 数组字符串。
    fn list_windows(&self) -> zbus::Result<String>;

    /// 当前活动窗口 JSON（空串=无焦点窗口）。
    fn get_active_window(&self) -> zbus::Result<String>;

    fn focus_window(&self, window_id: u32) -> zbus::Result<()>;

    fn move_window(&self, window_id: u32, x: i32, y: i32) -> zbus::Result<()>;

    fn close_window(&self, window_id: u32) -> zbus::Result<()>;

    fn minimize_window(&self, window_id: u32, minimize: bool) -> zbus::Result<()>;

    fn maximize_window(&self, window_id: u32, maximize: bool) -> zbus::Result<()>;
}

/// extension.js 源码（§8.1 XML 定义的实现，单一事实来源）。
///
/// 内容与 `components/compositor/mutter/src/extension.js` 同源（`include_str!`
/// 内嵌），安装与打包均从该文件落盘——避免内容两处漂移。
/// 安装位置：`~/.local/share/gnome-shell/extensions/agent-shell-bridge@tsic.top/`。
/// GNOME 45+ 使用 ESM 导入；Wayland/X11 会话均可——接口经 Gio.DBus
/// 注册，会话类型无关（§8.4 共享能力表）。
pub const EXTENSION_JS: &str = include_str!("extension.js");

/// extension.js 配套的 metadata.json（uuid/shell-version/description）。
///
/// 与 [`EXTENSION_JS`] 同为单一事实来源
/// （`components/compositor/mutter/src/metadata.json`），安装与打包均从
/// 该文件落盘。GNOME Shell 从它读取扩展身份与版本约束。
pub const EXTENSION_METADATA: &str = include_str!("metadata.json");

/// Extension D-Bus 客户端（路径 B）。
///
/// 复用共享 session bus 连接；方法调用 + 信号订阅。Extension 未安装/
/// 未启用时所有调用报 [`MutterError::Extension`]——上层据此给出可操作
/// 的安装提示。
pub struct ExtensionRunner {
    conn: Connection,
}

impl ExtensionRunner {
    /// 建桥：复用既有 session bus 连接。
    pub fn new(conn: Connection) -> Self {
        Self { conn }
    }

    fn proxy_err(e: impl std::fmt::Display) -> MutterError {
        MutterError::Extension(format!(
            "{AGENTSHELL_IFACE} call failed: {e} \
             (install & enable extension `{EXTENSION_ID}`)"
        ))
    }

    /// 探测：introspect 总线名是否在位（只读、无副作用）。
    ///
    /// 用 introspect 而非真实方法调用——探测不应触发窗口枚举这类
    /// 有实际代价的操作。
    pub async fn probe(&self) -> Result<()> {
        let node = zbus::fdo::IntrospectableProxy::builder(&self.conn)
            .destination("org.gnome.Shell.AgentShell")
            .expect("static service name")
            .path(AGENTSHELL_PATH)
            .expect("static object path")
            .build()
            .await
            .map_err(Self::proxy_err)?;
        match node.introspect().await {
            Ok(xml) if xml.contains(AGENTSHELL_IFACE) => Ok(()),
            Ok(_) => Err(MutterError::Extension(format!(
                "{AGENTSHELL_PATH} present but {AGENTSHELL_IFACE} not advertised"
            ))),
            Err(e) => Err(Self::proxy_err(e)),
        }
    }

    async fn proxy(&self) -> Result<AgentShellProxy<'_>> {
        AgentShellProxy::builder(&self.conn)
            .cache_properties(zbus::proxy::CacheProperties::No)
            .build()
            .await
            .map_err(Self::proxy_err)
    }

    async fn list_json(&self, what: &'static str) -> Result<Value> {
        let raw = self
            .proxy()
            .await?
            .list_windows()
            .await
            .map_err(Self::proxy_err)?;
        serde_json::from_str(&raw)
            .map_err(|e| MutterError::Extension(format!("{what} returned invalid JSON: {e}")))
    }

    /// 列出全部窗口。
    pub async fn list_windows(&self) -> Result<Vec<agent_shell_core::types::WindowInfo>> {
        let v = self.list_json("ListWindows").await?;
        let arr = v.as_array().cloned().unwrap_or_default();
        Ok(arr
            .iter()
            .enumerate()
            .filter_map(|(i, w)| parse::window(w, i as u32))
            .collect())
    }

    /// 当前活动窗口（空串返回 None）。
    pub async fn get_active_window(&self) -> Result<Option<agent_shell_core::types::WindowInfo>> {
        let raw = self
            .proxy()
            .await?
            .get_active_window()
            .await
            .map_err(Self::proxy_err)?;
        if raw.is_empty() {
            return Ok(None);
        }
        let v: Value = serde_json::from_str(&raw)
            .map_err(|e| MutterError::Extension(format!("GetActiveWindow invalid JSON: {e}")))?;
        parse::window(&v, 0).map(Some).ok_or_else(|| {
            MutterError::Extension("GetActiveWindow payload missing window id".into())
        })
    }

    pub async fn focus_window(&self, native_id: &str) -> Result<()> {
        self.proxy()
            .await?
            .focus_window(parse_u32(native_id)?)
            .await
            .map_err(Self::proxy_err)
    }

    pub async fn close_window(&self, native_id: &str) -> Result<()> {
        self.proxy()
            .await?
            .close_window(parse_u32(native_id)?)
            .await
            .map_err(Self::proxy_err)
    }

    pub async fn minimize_window(&self, native_id: &str, minimize: bool) -> Result<()> {
        self.proxy()
            .await?
            .minimize_window(parse_u32(native_id)?, minimize)
            .await
            .map_err(Self::proxy_err)
    }

    pub async fn move_window(&self, native_id: &str, x: i32, y: i32) -> Result<()> {
        self.proxy()
            .await?
            .move_window(parse_u32(native_id)?, x, y)
            .await
            .map_err(Self::proxy_err)
    }

    pub async fn maximize_window(&self, native_id: &str, maximize: bool) -> Result<()> {
        self.proxy()
            .await?
            .maximize_window(parse_u32(native_id)?, maximize)
            .await
            .map_err(Self::proxy_err)
    }

    /// 订阅三个信号（`WindowOpened` / `WindowClosed` /
    /// `ActiveWindowChanged`），归一化为 core `DesktopEvent` 流。
    ///
    /// MessageStream + Header 过滤：三信号参数形状不同（s / u / s），
    /// 统一按 interface + member 分派；归一化失败的事件跳过（不中断流）。
    pub async fn subscribe(&self) -> Result<Box<dyn agent_shell_core::EventStream>> {
        let stream = MessageStream::from(&self.conn);
        Ok(Box::new(ExtensionEventStream {
            inner: tokio::sync::Mutex::new(stream),
        }))
    }
}

/// 校验并解析窗口 id 为 u32（D-Bus in 签名 `u`）。
fn parse_u32(native_id: &str) -> Result<u32> {
    native_id.parse::<u32>().map_err(|_| {
        MutterError::Extension(format!("non-numeric window id `{native_id}` rejected"))
    })
}

/// Extension 信号流 → core `DesktopEvent` 的最小映射。
///
/// `EventStream::next_event(&self)` 是共享引用——MessageStream 的
/// `try_next` 需要 `&mut`，以 tokio Mutex 串行化（单消费者语义不变）。
struct ExtensionEventStream {
    inner: tokio::sync::Mutex<MessageStream>,
}
#[async_trait::async_trait]
impl agent_shell_core::EventStream for ExtensionEventStream {
    async fn next_event(&self) -> Option<agent_shell_core::event::DesktopEvent> {
        use futures_lite::stream::StreamExt;
        loop {
            // try_next: Result<Option<Message>>——Err 或 None（流结束）
            // 都返回 None。
            let msg = self.inner.lock().await.try_next().await.ok()??;
            let header = msg.header();
            if header.message_type() != zbus::message::Type::Signal {
                continue;
            }
            if header.interface().map(|i| i.as_str()) != Some(AGENTSHELL_IFACE) {
                continue;
            }
            let member = header.member().map(|m| m.as_str()).unwrap_or_default();
            let source = agent_shell_core::event::EventSource::MutterExtension;
            let occurred_at = std::time::Instant::now();
            let body = msg.body();
            match member {
                "WindowOpened" | "ActiveWindowChanged" => {
                    let info: String = match body.deserialize::<String>() {
                        Ok(s) => s,
                        Err(_) => continue,
                    };
                    // ActiveWindowChanged 的空 info = 桌面无焦点窗口。
                    if member == "ActiveWindowChanged" && info.is_empty() {
                        return Some(agent_shell_core::event::DesktopEvent::WindowFocused {
                            info: empty_focus_placeholder(),
                            source,
                            occurred_at,
                        });
                    }
                    let v: Value = match serde_json::from_str(&info) {
                        Ok(v) => v,
                        Err(_) => continue,
                    };
                    match parse::window(&v, 0) {
                        Some(w) => {
                            let event = if member == "WindowOpened" {
                                agent_shell_core::event::DesktopEvent::WindowOpened {
                                    info: w,
                                    source,
                                    occurred_at,
                                }
                            } else {
                                agent_shell_core::event::DesktopEvent::WindowFocused {
                                    info: w,
                                    source,
                                    occurred_at,
                                }
                            };
                            return Some(event);
                        }
                        None => continue,
                    }
                }
                "WindowClosed" => {
                    let wid: u32 = match body.deserialize::<u32>() {
                        Ok(w) => w,
                        Err(_) => continue,
                    };
                    return Some(agent_shell_core::event::DesktopEvent::WindowClosed {
                        id: agent_shell_core::types::WindowId {
                            native_id: wid.to_string(),
                            de_type: agent_shell_core::types::DesktopEnvironment::GNOME,
                        },
                        source,
                        occurred_at,
                    });
                }
                _ => continue,
            }
        }
    }
}

/// 无焦点窗口时的占位 WindowInfo（native_id 为空串，调用方以 id 判空）。
fn empty_focus_placeholder() -> agent_shell_core::types::WindowInfo {
    use agent_shell_core::types::{
        DesktopEnvironment, Rect, WindowId, WindowInfo, WindowState, WindowType,
    };
    WindowInfo {
        id: WindowId {
            native_id: String::new(),
            de_type: DesktopEnvironment::GNOME,
        },
        title: String::new(),
        app_id: String::new(),
        pid: 0,
        geometry: Rect::default(),
        frame_geometry: Rect::default(),
        states: vec![WindowState::Normal],
        workspace_id: None,
        monitor_id: None,
        stacking_order: 0,
        desktop_file: None,
        window_type: WindowType::Unknown,
        icon_geometry: None,
        keep_above: false,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn extension_js_registers_declared_interface() {
        // §8.4 验收：extension.js 按 XML 定义注册 org.gnome.Shell.AgentShell。
        assert!(EXTENSION_JS.contains("<interface name=\"org.gnome.Shell.AgentShell\">"));
        assert!(EXTENSION_JS.contains("/org/gnome/Shell/AgentShell"));
    }

    #[test]
    fn extension_js_exposes_all_methods_and_signals() {
        // issue 验收 XML 的全部 method + signal 必须齐备——缺一个即验收失败。
        for needle in [
            "ListWindows",
            "GetActiveWindow",
            "FocusWindow",
            "MoveWindow",
            "GetMonitorLayout",
            "CloseWindow",
            "MinimizeWindow",
            "MaximizeWindow",
            "<signal name=\"WindowOpened\"",
            "<signal name=\"WindowClosed\"",
            "<signal name=\"ActiveWindowChanged\"",
        ] {
            assert!(
                EXTENSION_JS.contains(needle),
                "EXTENSION_JS missing `{needle}`"
            );
        }
    }

    #[test]
    fn signal_emits_use_tuple_variant_signatures() {
        // 回归守卫：信号 Variant 须用元组签名 '(s)'/'(u)'，数组签名 ['s'] 会抛错
        // 致三信号全部失效（审查🔴3）。
        assert!(EXTENSION_JS.contains("new GLib.Variant('(u)', [mw.get_id() >>> 0])"));
        assert!(EXTENSION_JS.contains("new GLib.Variant('(s)', [this._getActiveWindow()])"));
        assert!(EXTENSION_JS.contains("new GLib.Variant('(s)', [this._windowJson(mw)])"));
        // 不得再按数组形式构造 Variant。
        assert!(!EXTENSION_JS.contains("new GLib.Variant(['"));
        assert!(!EXTENSION_JS.contains("SIGNAL_TYPES"));
    }

    #[test]
    fn extension_js_finds_interface_by_name_not_array_index() {
        // 审查🔴1：interfaces 是数组，按名字符串索引得 undefined——须 find(name)。
        assert!(EXTENSION_JS.contains("interfaces.find(i => i.name === IFACE_NAME)"));
        assert!(!EXTENSION_JS.contains("interfaces['org.gnome.Shell.AgentShell']"));
    }

    #[test]
    fn extension_js_registers_with_method_call_vtable() {
        // 审查🔴2：方法分派须经含 method_call 的 vtable（普通对象不是合法 vtable）。
        assert!(EXTENSION_JS.contains("new Gio.DBusInterfaceVTable()"));
        assert!(EXTENSION_JS.contains(".method_call = "));
        assert!(EXTENSION_JS.contains("register_object("));
    }

    #[test]
    fn extension_js_validates_sender() {
        // 审查🔴6：well-known 名暴露窗口控制，method_call 须先校验 sender
        // 确为 daemon（持有 org.agentshell.Daemon 名）。
        assert!(EXTENSION_JS.contains("_isAuthorized(sender)"));
        assert!(EXTENSION_JS.contains("get_name_owner_sync(DAEMON_BUS_NAME)"));
        assert!(EXTENSION_JS.contains("owner !== null && owner === sender"));
        assert!(EXTENSION_JS.contains(".Error.Unauthorized"));
    }

    #[test]
    fn daemon_bus_name_rust_js_cross_assertion() {
        // 🟡 审查建议：DAEMON_BUS_NAME 在 Rust(error.rs) 与 JS(extension.js) 双源
        // 定义，任一单侧改动会静默杀死 sender 校验（GNOME 47+ 无回退）。用 Rust
        // 常量交叉断言 JS 内嵌源码，保证两侧同值。
        assert!(EXTENSION_JS.contains(&format!(
            "DAEMON_BUS_NAME = '{}'",
            crate::error::DAEMON_BUS_NAME
        )));
    }

    #[test]
    fn extension_js_passes_gjs_syntax_check() {
        // 🟡 审查建议：核心 D-Bus 行为仅靠 contains() 字符串断言，抓不住 GJS
        // 语法/运行期类型错误。gjs 可用时跑 --check-syntax（-m ESM 模式）真校验；
        // gjs 不可用（本地/无 GNOME CI）时跳过——Nix doCheck 注入 gjs 后生效。
        let path = concat!(env!("CARGO_MANIFEST_DIR"), "/src/extension.js");
        let output = std::process::Command::new("gjs")
            .args(["-m", "--check-syntax", path])
            .output();
        match output {
            Ok(out) => assert!(
                out.status.success(),
                "gjs --check-syntax failed:\n{}",
                String::from_utf8_lossy(&out.stderr)
            ),
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => {
                eprintln!("gjs not found; skipping GJS syntax check");
            }
            Err(e) => panic!("failed to run gjs --check-syntax: {e}"),
        }
    }

    #[test]
    fn extension_js_uses_notify_focus_window_signal() {
        // 审查🔴5：焦点信号是 notify::focus-window（属性通知），notify-focus-window 不存在。
        assert!(EXTENSION_JS.contains("'notify::focus-window'"));
        assert!(!EXTENSION_JS.contains("'notify-focus-window'"));
    }

    #[test]
    fn extension_js_exports_default_extension_class() {
        // GNOME 45+ ESM 入口：默认导出 Extension 子类（init() 返回对象不是 45+ 生命周期）。
        assert!(EXTENSION_JS.contains("export default class"));
        assert!(EXTENSION_JS.contains("extends Extension"));
        assert!(EXTENSION_JS.contains(
            "import {Extension} from 'resource:///org/gnome/shell/extensions/extension.js'"
        ));
    }

    #[test]
    fn extension_js_separates_owner_and_registration_cleanup() {
        // 审查🔴4：owner id 与 registration id 分离清理（前者 bus_unown_name，
        // 后者 unregister_object），不得混用或空 catch 吞错。
        assert!(EXTENSION_JS.contains("Gio.bus_unown_name(this._nameOwnerId)"));
        assert!(EXTENSION_JS.contains("unregister_object(this._regId)"));
        assert!(!EXTENSION_JS.contains("} catch {}"));
    }

    #[test]
    fn extension_js_imports_meta_and_glib_esm() {
        // 🔴2/🟡10：GNOME 45+ ESM 显式导入；GLib 用 gi:// 而非 resource://。
        assert!(EXTENSION_JS.contains("import Meta from 'gi://Meta';"));
        assert!(EXTENSION_JS.contains("import GLib from 'gi://GLib';"));
    }

    #[test]
    fn xml_matches_issue_contract_methods() {
        // issue 验收 XML 的核心方法签名逐字出现（FocusWindow u / MoveWindow ui）。
        assert!(EXTENSION_JS.contains(
            "<method name=\"FocusWindow\"><arg type=\"u\" name=\"window_id\" direction=\"in\"/></method>"
        ));
        assert!(EXTENSION_JS.contains(
            "<method name=\"MoveWindow\">\n      <arg type=\"u\" name=\"window_id\" direction=\"in\"/>\n      <arg type=\"i\" name=\"x\" direction=\"in\"/>\n      <arg type=\"i\" name=\"y\" direction=\"in\"/>\n    </method>"
        ));
    }

    #[test]
    fn parses_numeric_ids_only() {
        assert_eq!(parse_u32("42").unwrap(), 42);
        assert!(parse_u32("-1").is_err());
        assert!(parse_u32("0x10").is_err());
        assert!(parse_u32("").is_err());
    }

    #[test]
    fn proxy_targets_extension_bus_name() {
        // 编译期断言 proxy 默认值指向 extension 注册的总线名/路径/接口
        // （zbus 5 的 Defaults 常量是 Option 包裹的静态名）。
        use zbus::proxy::Defaults;
        let dest = <AgentShellProxy as Defaults>::DESTINATION;
        let path = <AgentShellProxy as Defaults>::PATH;
        let iface = <AgentShellProxy as Defaults>::INTERFACE;
        assert_eq!(
            dest.as_ref().expect("static destination").as_str(),
            "org.gnome.Shell.AgentShell"
        );
        assert_eq!(
            path.as_ref().expect("static path").as_str(),
            AGENTSHELL_PATH
        );
        assert_eq!(
            iface.as_ref().expect("static interface").as_str(),
            AGENTSHELL_IFACE
        );
    }
}
