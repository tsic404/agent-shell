//! Shell Extension 路径（设计文档 §8.1 路径 B / §8.4，GNOME 47+ 推荐）。
//!
//! GNOME Shell Extension `agent-shell-bridge@multica.dev` 在 session bus
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
/// 安装位置：`~/.local/share/gnome-shell/extensions/agent-shell-bridge@multica.dev/`。
/// GNOME 45+ 使用 ESM 导入；Wayland/X11 会话均可——接口经 Gio.DBus
/// 注册，会话类型无关（§8.4 共享能力表）。
pub const EXTENSION_JS: &str = r#"// agent-shell-bridge@multica.dev — org.gnome.Shell.AgentShell
// 由 agent-shell-compositor-mutter 内嵌交付；安装到 extensions 目录后启用。
import GLib from 'gi://GLib';
import Gio from 'gi://Gio';
import Meta from 'gi://Meta';

const DBusInterface = `
<node>
  <interface name="org.gnome.Shell.AgentShell">
    <method name="ListWindows"><arg type="s" direction="out"/></method>
    <method name="GetActiveWindow"><arg type="s" direction="out"/></method>
    <method name="FocusWindow"><arg type="u" name="window_id" direction="in"/></method>
    <method name="MoveWindow">
      <arg type="u" name="window_id" direction="in"/>
      <arg type="i" name="x" direction="in"/>
      <arg type="i" name="y" direction="in"/>
    </method>
    <method name="CloseWindow"><arg type="u" name="window_id" direction="in"/></method>
    <method name="MinimizeWindow">
      <arg type="u" name="window_id" direction="in"/>
      <arg type="b" name="minimize" direction="in"/>
    </method>
    <method name="MaximizeWindow">
      <arg type="u" name="window_id" direction="in"/>
      <arg type="b" name="maximize" direction="in"/>
    </method>
    <method name="GetMonitorLayout"><arg type="s" direction="out"/></method>
    <signal name="WindowOpened"><arg type="s" name="info"/></signal>
    <signal name="WindowClosed"><arg type="u" name="window_id"/></signal>
    <signal name="ActiveWindowChanged"><arg type="s" name="info"/></signal>
  </interface>
</node>`;

class AgentShellBridge {
    constructor() {
        this._impl = null;
        this._nodeInfo = null;
        this._registrationIds = [];
        this._signalIds = [];
        this._watchers = new Map();
    }

    enable() {
        this._nodeInfo = Gio.DBusNodeInfo.new_for_xml(DBusInterface);
        this._impl = {
            ListWindows: () => this._listWindows(),
            GetActiveWindow: () => this._getActiveWindow(),
            FocusWindow: id => this._withWindow(id, mw => mw.activate(global.get_current_time())),
            MoveWindow: (id, x, y) => this._withWindow(id, mw => mw.move_frame(true, x, y)),
            CloseWindow: id => this._withWindow(id, mw => mw.delete()),
            MinimizeWindow: (id, minimize) =>
                this._withWindow(id, mw => { mw.minimized = !!minimize; }),
            MaximizeWindow: (id, maximize) => this._withWindow(id, mw =>
                maximize ? mw.maximize(Meta.MaximizeFlags.BOTH)
                         : mw.unmaximize(Meta.MaximizeFlags.BOTH)),
            GetMonitorLayout: () => this._monitorLayout(),
        };
        const ownerId = Gio.bus_own_name(Gio.BusType.SESSION,
            'org.gnome.Shell.AgentShell',
            Gio.BusNameOwnerFlags.NONE, null, null, null);
        this._registrationIds.push(ownerId);
        const regId = Gio.DBus.session.register_object(
            '/org/gnome/Shell/AgentShell', this._nodeInfo.interfaces[
                'org.gnome.Shell.AgentShell'], this._impl, null, null);
        this._registrationIds.push(regId);

        // 窗口事件 → D-Bus 信号（§8.1 三信号）。
        const display = global.display ?? global.compositor;
        if (display) {
            this._signalIds.push(display.connect('window-created', (_d, mw) =>
                this._onWindowCreated(mw)));
            this._signalIds.push(display.connect('window-destroyed', (_d, mw) =>
                this._emit('WindowClosed', [mw.get_id() >>> 0])));
            this._signalIds.push(display.connect('notify-focus-window', () =>
                this._emit('ActiveWindowChanged', [this._getActiveWindow()])));
        }
    }

    disable() {
        const session = Gio.DBus.session;
        for (const [, toId] of this._watchers) GLib.source_remove(toId);
        this._watchers.clear();
        for (const id of this._signalIds)
            (global.display ?? global.compositor)?.disconnect(id);
        this._signalIds = [];
        for (const id of this._registrationIds.splice(0).reverse()) {
            try { typeof id === 'number' && id > 0
                ? Gio.bus_unown_name(id) : session.unregister_object(id); } catch {}
        }
        this._impl = null;
        this._nodeInfo = null;
    }

    _withWindow(id, fn) {
        const actor = global.get_window_actors()
            .find(a => a.meta_window.get_id() === id);
        if (!actor) throw new GLib.Error(GLib.quark_from_string('agent-shell'),
            1, `window ${id} not found`);
        fn(actor.meta_window);
    }

    _onWindowCreated(mw) {
        // window-created 时 actor 属性未就绪，延迟一拍再取详情并广播。
        const toId = GLib.idle_add(GLib.PRIORITY_DEFAULT_IDLE, () => {
            this._watchers.delete(mw);
            try { this._emit('WindowOpened', [this._windowJson(mw)]); } catch {}
            return GLib.SOURCE_REMOVE;
        });
        this._watchers.set(mw, toId);
    }

    // 按 XML 声明类型构造 Variant：WindowClosed 是 u（uint32），
    // WindowOpened / ActiveWindowChanged 是 s。类型不匹配时 GDBus
    // 拒绝发送——Rust 端按同型反序列化，两端以本表为单一事实来源。
    static SIGNAL_TYPES = {
        'WindowOpened': ['s'],
        'ActiveWindowChanged': ['s'],
        'WindowClosed': ['u'],
    };

    _emit(name, params) {
        const types = AgentShellBridge.SIGNAL_TYPES[name];
        if (!types) throw new Error(`unknown signal ${name}`);
        Gio.DBus.session.emit_signal(null, '/org/gnome/Shell/AgentShell',
            'org.gnome.Shell.AgentShell', name,
            new GLib.Variant(types, params));
    }

    _windowJson(a_or_mw) {
        let mw = a_or_mw.meta_window ?? a_or_mw;
        return JSON.stringify({
            id: mw.get_id(), title: mw.get_title() || '',
            appId: mw.get_wm_class() || '', pid: mw.get_pid(),
            minimized: mw.minimized, maximized: mw.maximized,
            fullscreen: mw.fullscreen,
            geometry: mw.get_frame_rect(),
            hasFocus: mw.has_focus()
        });
    }

    _listWindows() {
        return JSON.stringify(global.get_window_actors().map(a =>
            JSON.parse(this._windowJson(a))));
    }

    _getActiveWindow() {
        const a = global.get_window_actors().find(a => a.meta_window.has_focus());
        return a === undefined ? '' : this._windowJson(a);
    }

    _monitorLayout() {
        const mm = global.backend.get_monitor_manager();
        return JSON.stringify(mm.get_monitors().map(m => m.get_properties?.() ?? {}));
    }
};

export function init() { return new AgentShellBridge(); }
"#;

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
    fn signal_variant_types_match_xml_declarations() {
        // 🔴1 回归守卫：SIGNAL_TYPES 表必须与 XML 声明的 arg 类型一致——
        // WindowClosed 是 u，两个 info 信号是 s。不一致时 GDBus 拒发，
        // Rust 端 deserialize 静默丢事件。
        assert!(EXTENSION_JS.contains("'WindowOpened': ['s'],"));
        assert!(EXTENSION_JS.contains("'ActiveWindowChanged': ['s'],"));
        assert!(EXTENSION_JS.contains("'WindowClosed': ['u'],"));
        // emit 调用点不得再按统一 's' 构造（旧实现回归检测）。
        assert!(!EXTENSION_JS.contains("params.map(() => 's')"));
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
