//! Shell Extension 路径（设计文档 §8.1 路径 B / §8.4，GNOME 47+ 推荐）。
//!
//! Extension `agent-shell-bridge@tsic.top` 在 session bus 注册
//! `org.gnome.Shell.AgentShell` 接口，Rust 端只做客户端（调方法、订阅信号）；
//! extension.js 由本 crate 内嵌交付（单一事实来源）。信号语义见 §8.1：
//! WindowOpened / WindowClosed / ActiveWindowChanged，info 为单窗 JSON。

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
    use std::ffi::OsString;
    use std::path::Path;

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

    /// 选项清单是否声明 `--check-syntax`。
    ///
    /// 只认以该参数开头的清单行：gjs 对未知参数打印的
    /// `gjs: unrecognized option '--check-syntax'` 含同一字符串，不能当作支持证据。
    fn help_advertises_check_syntax(stdout: &[u8], stderr: &[u8]) -> bool {
        let lists_option = |raw: &[u8]| {
            String::from_utf8_lossy(raw)
                .lines()
                .any(|line| line.trim_start().starts_with("--check-syntax"))
        };
        lists_option(stdout) || lists_option(stderr)
    }

    /// 要求开关：置位后没有可用校验器即失败，避免 CI 静默失去校验。
    const JS_SYNTAX_REQUIRE_ENV: &str = "AGENT_SHELL_REQUIRE_JS_SYNTAX_CHECK";

    /// gjs 是否支持 `--check-syntax`：以 `--help` 的选项清单为准。
    fn gjs_supports_check_syntax(gjs_bin: &Path) -> bool {
        match std::process::Command::new(gjs_bin).arg("--help").output() {
            Ok(out) => help_advertises_check_syntax(&out.stdout, &out.stderr),
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => false,
            Err(e) => panic!("failed to run gjs --help: {e}"),
        }
    }

    /// `node --check --input-type=module` 纯解析：`None` 表示 node 不可用。
    ///
    /// 源码走 stdin + `--input-type=module`，不受扩展名与旧版本模块自动探测影响；
    /// `--check` 只解析不执行，extension.js 的 GNOME 导入无需可解析。
    fn node_check_module_source(node_bin: &Path, source: &str) -> Option<Result<(), String>> {
        use std::io::Write;
        use std::process::{Command, Stdio};

        let mut child = match Command::new(node_bin)
            .args(["--input-type=module", "--check"])
            .stdin(Stdio::piped())
            .stdout(Stdio::null())
            .stderr(Stdio::piped())
            .spawn()
        {
            Ok(child) => child,
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => return None,
            Err(e) => panic!("failed to run node --check: {e}"),
        };
        // 写入失败不单独报错：node 拒绝该命令行时由退出码与 stderr 说明原因。
        if let Some(mut stdin) = child.stdin.take() {
            let _ = stdin.write_all(source.as_bytes());
        }
        let out = child.wait_with_output().expect("wait for node --check");
        Some(if out.status.success() {
            Ok(())
        } else {
            Err(String::from_utf8_lossy(&out.stderr).into_owned())
        })
    }

    /// node 能否执行上面的调用：用最小合法 ESM 实测。
    ///
    /// 旧版 node 不认 `--input-type`、PATH 上的 `node` 是别的程序时命令行会被拒；
    /// 先探测才能把这类失败归为「工具不支持」，而不是脚本语法错误。
    fn node_can_check_module_syntax(node_bin: &Path) -> bool {
        matches!(
            node_check_module_source(node_bin, "export const syntaxCheckSupported = true;\n"),
            Some(Ok(()))
        )
    }

    /// JS 语法校验结论：通过 / 脚本真错 / 没有可用校验器。
    enum JsSyntaxVerdict {
        Passed,
        Rejected(String),
        Unavailable(String),
    }

    /// 按 gjs → node 的顺序取第一个可用校验器；两个都不可用时返回原因而非通过。
    fn extension_js_syntax_verdict(
        js_path: &Path,
        gjs_bin: &Path,
        node_bin: &Path,
    ) -> JsSyntaxVerdict {
        if gjs_supports_check_syntax(gjs_bin) {
            let out = std::process::Command::new(gjs_bin)
                .args(["-m", "--check-syntax"])
                .arg(js_path)
                .output()
                .expect("failed to run gjs --check-syntax");
            return if out.status.success() {
                JsSyntaxVerdict::Passed
            } else {
                JsSyntaxVerdict::Rejected(String::from_utf8_lossy(&out.stderr).into_owned())
            };
        }
        if !node_can_check_module_syntax(node_bin) {
            return JsSyntaxVerdict::Unavailable(
                "gjs lacks --check-syntax and node cannot run --check --input-type=module".into(),
            );
        }
        let source = std::fs::read_to_string(js_path).expect("read extension.js");
        match node_check_module_source(node_bin, &source) {
            Some(Ok(())) => JsSyntaxVerdict::Passed,
            Some(Err(detail)) if detail.contains("SyntaxError") => {
                JsSyntaxVerdict::Rejected(detail)
            }
            Some(Err(detail)) => JsSyntaxVerdict::Unavailable(format!(
                "node rejected the check invocation:\n{detail}"
            )),
            None => JsSyntaxVerdict::Unavailable("node vanished after its capability probe".into()),
        }
    }

    /// 裁决 + 要求开关 → 用例结果：通过 Ok，脚本错 Err，跳过由开关决定。
    fn run_js_syntax_check(
        js_path: &Path,
        gjs_bin: &Path,
        node_bin: &Path,
        require: Option<OsString>,
    ) -> Result<(), String> {
        match extension_js_syntax_verdict(js_path, gjs_bin, node_bin) {
            JsSyntaxVerdict::Passed => Ok(()),
            JsSyntaxVerdict::Rejected(detail) => {
                Err(format!("extension.js fails the JS syntax check:\n{detail}"))
            }
            JsSyntaxVerdict::Unavailable(reason) if require.is_some() => Err(format!(
                "{JS_SYNTAX_REQUIRE_ENV} is set but no JS syntax checker is usable: {reason}"
            )),
            JsSyntaxVerdict::Unavailable(reason) => {
                eprintln!("skipping JS syntax check: {reason}");
                Ok(())
            }
        }
    }

    /// 判别力守护：只有选项清单声明了 `--check-syntax` 才算支持；未知参数报错里的
    /// 同一字符串不构成支持证据。
    #[test]
    fn help_advertises_check_syntax_only_from_option_listing() {
        let listing = b"Application Options:\n  -m, --module   Load as a module\n  --check-syntax Parse without executing\n";
        let without = b"Application Options:\n  -m, --module   Load as a module\n  --version      Print version\n";
        assert!(help_advertises_check_syntax(listing, b""));
        assert!(help_advertises_check_syntax(b"", listing));
        assert!(!help_advertises_check_syntax(without, b""));
        assert!(!help_advertises_check_syntax(
            b"",
            b"gjs: unrecognized option '--check-syntax'\n"
        ));
    }

    /// 支持 `--check-syntax` 的 gjs 替身：按夹具内容给出通过/语法错误。
    const GJS_CHECKING_SH: &str = r#"#!/bin/sh
for arg in "$@"; do
  if [ "$arg" = "--help" ]; then
    printf 'Application Options:\n  --check-syntax  Parse without executing\n'
    exit 0
  fi
done
for arg in "$@"; do last="$arg"; done
if grep -qF 'function {' "$last"; then
  echo "SyntaxError: Unexpected token '{'" >&2
  exit 1
fi
exit 0
"#;

    /// 不支持 `--check-syntax` 的 gjs 替身：清单不含该参数，未知参数零退出。
    const GJS_UNSUPPORTED_SH: &str = r#"#!/bin/sh
for arg in "$@"; do
  if [ "$arg" = "--help" ]; then
    printf 'Application Options:\n  -m, --module  Load as a module\n'
    exit 0
  fi
done
echo "gjs: unrecognized option '--check-syntax'" >&2
exit 1
"#;

    /// 写入可执行的替身程序。
    fn write_executable(path: &Path, body: &str) {
        use std::os::unix::fs::PermissionsExt;
        std::fs::write(path, body).expect("write stub program");
        std::fs::set_permissions(path, std::fs::Permissions::from_mode(0o755))
            .expect("chmod stub program");
    }

    /// 两路校验都必须真的解析脚本：注入 `function {` 的夹具裁决为 Rejected，
    /// 合法夹具通过；gjs 与 node 两条路径都覆盖，不依赖 PATH 上的真实工具。
    #[test]
    fn injected_syntax_error_is_rejected_on_both_checker_paths() {
        let dir = std::env::temp_dir().join(format!(
            "agent-shell-js-syntax-{}-{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .expect("clock after epoch")
                .as_nanos()
        ));
        std::fs::create_dir_all(&dir).expect("create fixture dir");
        let good = dir.join("good.js");
        let bad = dir.join("bad.js");
        std::fs::write(&good, "export const ok = true;\n").expect("write good fixture");
        std::fs::write(&bad, "export const ok = true;\nfunction {\n").expect("write bad fixture");
        let gjs_checking = dir.join("gjs-checking");
        let gjs_plain = dir.join("gjs-no-check-syntax");
        write_executable(&gjs_checking, GJS_CHECKING_SH);
        write_executable(&gjs_plain, GJS_UNSUPPORTED_SH);
        let no_node = dir.join("node-missing");

        // gjs 路径：它确证支持该参数时，由它的退出码裁决脚本。
        assert!(matches!(
            extension_js_syntax_verdict(&good, &gjs_checking, &no_node),
            JsSyntaxVerdict::Passed
        ));
        assert!(matches!(
            extension_js_syntax_verdict(&bad, &gjs_checking, &no_node),
            JsSyntaxVerdict::Rejected(_)
        ));
        // Rejected 优先于跳过语义：即使未置位要求开关也必须失败。
        assert!(run_js_syntax_check(&bad, &gjs_checking, &no_node, None).is_err());

        // node 路径：gjs 不支持该参数时，兜底必须真的解析而不是静默通过。
        let node = Path::new("node");
        if node_can_check_module_syntax(node) {
            assert!(matches!(
                extension_js_syntax_verdict(&good, &gjs_plain, node),
                JsSyntaxVerdict::Passed
            ));
            assert!(matches!(
                extension_js_syntax_verdict(&bad, &gjs_plain, node),
                JsSyntaxVerdict::Rejected(_)
            ));
            assert!(run_js_syntax_check(&bad, &gjs_plain, node, None).is_err());
        } else {
            eprintln!("node unavailable; skipping node-path assertions");
        }
        std::fs::remove_dir_all(&dir).ok();
    }

    /// 闸门守护：无可用校验器时，置位要求变量必须失败，未置位才按跳过。
    #[test]
    fn missing_checker_fails_only_when_required() {
        let js = Path::new(concat!(env!("CARGO_MANIFEST_DIR"), "/src/extension.js"));
        let missing = Path::new("/nonexistent/agent-shell-js-syntax-checker");
        let err = run_js_syntax_check(js, missing, missing, Some(OsString::from("1")))
            .expect_err("REQUIRE set must fail without a usable checker");
        assert!(err.contains(JS_SYNTAX_REQUIRE_ENV), "got: {err}");
        assert!(run_js_syntax_check(js, missing, missing, None).is_ok());
    }

    #[test]
    fn extension_js_passes_gjs_syntax_check() {
        // 核心 D-Bus 行为仅靠 contains() 字符串断言，抓不住 JS 语法/类型错误。
        // gjs 有 --check-syntax 时以它为准；发布版 gjs 都没有该选项，此时退到
        // node 的纯解析。跳过不等于通过：CI 置位要求变量后没有校验器即失败。
        let path = Path::new(concat!(env!("CARGO_MANIFEST_DIR"), "/src/extension.js"));
        let require = std::env::var_os(JS_SYNTAX_REQUIRE_ENV);
        if let Err(message) =
            run_js_syntax_check(path, Path::new("gjs"), Path::new("node"), require)
        {
            panic!("{message}");
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
