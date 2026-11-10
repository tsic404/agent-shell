//! TC-windows-01（AC-10，P1）：窗口列表的字段完整性与空列表边界。
//!
//! - 场景：有窗口时的 `windows.list` 全字段投影；零窗口桌面上的 `windows.list`
//! - 判据：条目字段完整且取值与合成器状态一致；空列表是 RPC 成功（退出码 0
//!   语义）并返回空数组，不是错误
//! - 执行：`cargo test -p agent-shell-acceptance --test TC-windows-01`
//!
//! 判定面是真实 `windows.list`：请求走 daemon 的真实 `dispatch`，窗口来源是
//! 注入的脚本化合成器（无图形 CI 下替代桌面协议通道），缓存与投影仍由 daemon
//! 执行。本用例全部可在无显示服务器的环境中执行，无环境绑定部分。

use acceptance::{window, FakeCompositor, Fixture, PrivateBus};
use agent_shell_compositor_mutter::{parse_shell_version, GnomeEvalBridge, MutterError};
use agent_shell_core::types::{
    DesktopEnvironment, MonitorId, Rect, WindowState, WindowType, WorkspaceId,
};
use agent_shell_rpc::method;
use serde_json::{json, Value};

/// `windows.list` 条目的完整字段集（§17.2 九字段 + 后增四字段）。
const ENTRY_KEYS: [&str; 13] = [
    "native_id",
    "title",
    "app_id",
    "pid",
    "x",
    "y",
    "width",
    "height",
    "workspace",
    "stacking_order",
    "states",
    "window_type",
    "keep_above",
];

fn entry_keys(entry: &Value) -> Vec<&str> {
    let object = entry.as_object().expect("window entry is a JSON object");
    let mut keys: Vec<&str> = object.keys().map(String::as_str).collect();
    keys.sort_unstable();
    keys
}

#[tokio::test]
async fn list_reports_complete_fields_and_an_empty_desktop_is_not_an_error() {
    let mut fixture = Fixture::start().await;

    let mut first = window("w-1", "Terminal — Konsole", "konsole", 10, 20, 800, 600);
    first.pid = 4242;
    first.workspace_id = Some(WorkspaceId {
        native_id: "ws-1".into(),
        de_type: DesktopEnvironment::KDE,
    });
    first.monitor_id = Some(MonitorId {
        native_id: "eDP-1".into(),
        de_type: DesktopEnvironment::KDE,
    });
    first.stacking_order = 3;
    first.states = vec![WindowState::Maximized];
    first.window_type = WindowType::Dialog;
    first.keep_above = true;
    let second = window("w-2", "Files", "org.gnome.Nautilus", 100, 200, 640, 480);

    let compositor = FakeCompositor::with_windows(vec![first.clone(), second.clone()]);
    fixture.install_compositor(&compositor, DesktopEnvironment::KDE);

    let result = fixture.call_ok(method::WINDOWS_LIST, None).await;
    assert_eq!(
        result.get("from_cache"),
        Some(&json!(false)),
        "a cold cache must report from_cache=false"
    );
    let windows = result
        .get("windows")
        .and_then(Value::as_array)
        .expect("windows.list carries a windows array");
    assert_eq!(windows.len(), 2, "both injected windows must be listed");

    let entry = &windows[0];
    let mut expected_keys = ENTRY_KEYS.to_vec();
    expected_keys.sort_unstable();
    assert_eq!(
        entry_keys(entry),
        expected_keys,
        "windows.list entries must carry exactly the documented fields: {entry}"
    );

    assert_eq!(entry["native_id"], json!("w-1"));
    assert_eq!(entry["title"], json!("Terminal — Konsole"));
    assert_eq!(entry["app_id"], json!("konsole"));
    assert_eq!(entry["pid"], json!(4242));
    assert_eq!(entry["x"], json!(10));
    assert_eq!(entry["y"], json!(20));
    assert_eq!(entry["width"], json!(800));
    assert_eq!(entry["height"], json!(600));
    assert_eq!(entry["workspace"], json!("ws-1"));
    assert_eq!(entry["stacking_order"], json!(3));
    assert_eq!(entry["states"], json!(["Maximized"]));
    assert_eq!(entry["window_type"], json!("Dialog"));
    assert_eq!(entry["keep_above"], json!(true));

    // 第二个窗口：无工作区（null）、空状态集合、默认窗口类型仍须原样投影。
    let entry = &windows[1];
    assert_eq!(entry["native_id"], json!("w-2"));
    assert_eq!(entry["workspace"], Value::Null);
    assert_eq!(entry["states"], json!([]));
    assert_eq!(entry["window_type"], json!("Normal"));
    assert_eq!(entry["keep_above"], json!(false));
    assert_eq!(entry["pid"], json!(second.pid));

    // 边界：零窗口桌面。窗口列表为空是正常查询结果（RPC 成功 + 空数组），
    // 不得被当作错误或缺失字段。
    let mut empty_fixture = Fixture::start().await;
    let empty = FakeCompositor::new();
    empty_fixture.install_compositor(&empty, DesktopEnvironment::KDE);
    let result = empty_fixture.call_ok(method::WINDOWS_LIST, None).await;
    assert_eq!(result["windows"], json!([]));
    assert_eq!(result["from_cache"], json!(false));
}

// ── GNOME 51 扩展载荷 → daemon 投影（真机线格式） ──────────────────────────

/// GNOME 51.1 Wayland 真机会话 `ListWindows` 的返回串（单窗，取值来自真窗口）。
///
/// 采集自 gnome-text-editor：扩展把 `Meta.Rectangle` 四边显式取出后序列化；
/// 工作区是 0 基序号，窗口类型是契约枚举名。本常量即真机线格式，
/// 解析与投影都不得退化。
const GNOME_LIST_WINDOWS_PAYLOAD: &str = r#"[{"id":52877633,"title":"New Document (Draft) - Text Editor","appId":"org.gnome.TextEditor","pid":619,"minimized":false,"maximized":false,"fullscreen":false,"geometry":{"x":290,"y":156,"width":700,"height":520},"workspace":0,"windowType":"Normal","keepAbove":false,"hasFocus":false}]"#;

/// mock `org.gnome.Shell.AgentShell`：只实现被测的 ListWindows。
struct GnomeExtensionShell {
    payload: String,
}

#[zbus::interface(name = "org.gnome.Shell.AgentShell")]
impl GnomeExtensionShell {
    fn list_windows(&self) -> String {
        self.payload.clone()
    }
    fn get_active_window(&self) -> String {
        String::new()
    }
    fn focus_window(&self, _window_id: u32) {}
    fn move_window(&self, _window_id: u32, _x: i32, _y: i32) {}
    fn close_window(&self, _window_id: u32) {}
    fn minimize_window(&self, _window_id: u32, _minimize: bool) {}
    fn maximize_window(&self, _window_id: u32, _maximize: bool) {}
}

/// GNOME 通道端到端：扩展载荷经 ExtensionRunner 解析后，`windows.list`
/// 条目携带真实几何/工作区/类型——0×0、Unknown、null 都是失败。
#[tokio::test]
async fn gnome_extension_payload_keeps_geometry_type_and_workspace_in_the_projection() {
    let bus = PrivateBus::start();
    let server = bus
        .serve(
            "/org/gnome/Shell/AgentShell",
            GnomeExtensionShell {
                payload: GNOME_LIST_WINDOWS_PAYLOAD.to_string(),
            },
        )
        .await;
    acceptance::bus::claim(&server, "org.gnome.Shell.AgentShell").await;

    let conn = bus.connect().await;
    let windows = agent_shell_compositor_mutter::ExtensionRunner::new(conn)
        .list_windows()
        .await
        .expect("GNOME extension payload parses over D-Bus");
    assert_eq!(windows.len(), 1, "captured payload carries one window");

    let mut fixture = Fixture::start().await;
    fixture.install_compositor(
        &FakeCompositor::with_windows(windows),
        DesktopEnvironment::GNOME,
    );
    let result = fixture.call_ok(method::WINDOWS_LIST, None).await;
    let entry = &result["windows"][0];

    // 逐字段等于真窗口取值：0×0 / Unknown / null 任何一个出现都判失败。
    assert_eq!(entry["native_id"], json!("52877633"));
    assert_eq!(entry["x"], json!(290));
    assert_eq!(entry["y"], json!(156));
    assert_eq!(entry["width"], json!(700));
    assert_eq!(entry["height"], json!(520));
    assert_eq!(entry["workspace"], json!("0"));
    assert_eq!(entry["window_type"], json!("Normal"));
    assert_eq!(entry["keep_above"], json!(false));
}

// ── GNOME <47：Eval 回退跳的壳编码层 → 解析 → daemon 投影 ────────────────────
//
// `org.gnome.Shell.Eval` 的实现是 `returnValue = JSON.stringify(eval(code))`：
// 脚本自身 `JSON.stringify` 后，线上 `result` 是带引号的 JSON 字符串。mock 必须
// 复刻这层编码，只回裸数组会让「生产只拿到 Value::String」的缺陷继续隐藏。

/// 复刻 gnome-shell `Eval` 的编码层：脚本返回值是 JSON 文本时，协议回包是这段
/// 文本的 JSON 字符串字面量（`JSON.stringify(eval(code))`）。
fn shell_eval_result(json_text: &str) -> String {
    serde_json::to_string(json_text).expect("encode shell Eval result")
}

/// mock `org.gnome.Shell` 根接口：原样返回 `(success, payload)`。
struct GnomeShellEval {
    success: bool,
    payload: String,
}

#[zbus::interface(name = "org.gnome.Shell")]
impl GnomeShellEval {
    fn eval(&self, _script: String) -> (bool, String) {
        (self.success, self.payload.clone())
    }
}

/// 在私有总线上起 Eval mock 并返回桥接；`_server` 必须与测试同寿命，
/// 否则 mock 对象随连接一起销毁。
async fn eval_bridge(
    bus: &PrivateBus,
    success: bool,
    payload: &str,
) -> (zbus::Connection, GnomeEvalBridge) {
    let server = bus
        .serve(
            "/org/gnome/Shell",
            GnomeShellEval {
                success,
                payload: payload.to_string(),
            },
        )
        .await;
    acceptance::bus::claim(&server, "org.gnome.Shell").await;
    let conn = bus.connect().await;
    (
        server,
        GnomeEvalBridge::new(conn, parse_shell_version("45.2").expect("GNOME 45")),
    )
}

/// 两窗 Eval 载荷（脚本 `LIST_WINDOWS_JS` 的线上形状：字段与真机一致，
/// 不含 frameGeometry）。
const GNOME_EVAL_LIST_WINDOWS_PAYLOAD: &str = r#"[{"id":52877633,"title":"New Document (Draft) - Text Editor","appId":"org.gnome.TextEditor","pid":619,"minimized":false,"maximized":false,"fullscreen":false,"geometry":{"x":290,"y":156,"width":700,"height":520},"workspace":0,"windowType":"Normal","keepAbove":false,"hasFocus":true},{"id":42,"title":"Terminal","appId":"org.gnome.Console","pid":1337,"minimized":true,"maximized":false,"fullscreen":false,"geometry":{"x":0,"y":0,"width":1024,"height":768},"workspace":1,"windowType":"Dialog","keepAbove":true,"hasFocus":false}]"#;

/// GNOME <47 Eval 跳：双层编码的真实回包经解析后字段齐全、序号按载荷顺序；
/// daemon `windows.list` 投影与扩展跳同形。
#[tokio::test]
async fn gnome_eval_window_payload_projects_fields_and_stacking_order() {
    let bus = PrivateBus::start();
    let (_server, bridge) = eval_bridge(
        &bus,
        true,
        &shell_eval_result(GNOME_EVAL_LIST_WINDOWS_PAYLOAD),
    )
    .await;

    let windows = bridge
        .list_windows()
        .await
        .expect("double-encoded Eval payload parses over D-Bus");
    assert_eq!(windows.len(), 2, "双层编码的载荷不得退化成空列表");

    let first = &windows[0];
    assert_eq!(first.id.native_id, "52877633");
    assert_eq!(first.id.de_type, DesktopEnvironment::GNOME);
    assert_eq!(first.title, "New Document (Draft) - Text Editor");
    assert_eq!(first.app_id, "org.gnome.TextEditor");
    assert_eq!(first.pid, 619);
    assert_eq!(
        first.geometry,
        Rect {
            x: 290,
            y: 156,
            width: 700,
            height: 520,
        }
    );
    // 载荷不带 frameGeometry：按既有规则回落内容几何（与扩展跳同口径）。
    assert_eq!(first.frame_geometry, first.geometry);
    assert_eq!(
        first.workspace_id.as_ref().map(|w| w.native_id.as_str()),
        Some("0")
    );
    assert_eq!(first.window_type, WindowType::Normal);
    assert_eq!(first.stacking_order, 0);
    assert!(!first.keep_above);

    let second = &windows[1];
    assert_eq!(second.id.native_id, "42");
    assert_eq!(
        second.workspace_id.as_ref().map(|w| w.native_id.as_str()),
        Some("1")
    );
    assert_eq!(second.window_type, WindowType::Dialog);
    assert_eq!(second.stacking_order, 1);
    assert!(second.keep_above);
    assert_eq!(second.states, vec![WindowState::Minimized]);

    // 活动窗口走同一载荷：hasFocus 位命中第一条。
    let active = bridge
        .get_active_window()
        .await
        .expect("active window decodes the same payload")
        .expect("the first window carries hasFocus");
    assert_eq!(active.id.native_id, "52877633");

    // daemon 方法投影：windows.list 条目字段与序号。
    let mut fixture = Fixture::start().await;
    fixture.install_compositor(
        &FakeCompositor::with_windows(windows),
        DesktopEnvironment::GNOME,
    );
    let result = fixture.call_ok(method::WINDOWS_LIST, None).await;
    let entries = result["windows"].as_array().expect("windows array");
    assert_eq!(entries.len(), 2);
    assert_eq!(entries[0]["native_id"], json!("52877633"));
    assert_eq!(
        entries[0]["title"],
        json!("New Document (Draft) - Text Editor")
    );
    assert_eq!(entries[0]["app_id"], json!("org.gnome.TextEditor"));
    assert_eq!(entries[0]["pid"], json!(619));
    assert_eq!(entries[0]["x"], json!(290));
    assert_eq!(entries[0]["y"], json!(156));
    assert_eq!(entries[0]["width"], json!(700));
    assert_eq!(entries[0]["height"], json!(520));
    assert_eq!(entries[0]["workspace"], json!("0"));
    assert_eq!(entries[0]["stacking_order"], json!(0));
    assert_eq!(entries[0]["states"], json!(["Normal"]));
    assert_eq!(entries[0]["window_type"], json!("Normal"));
    assert_eq!(entries[0]["keep_above"], json!(false));
    assert_eq!(entries[1]["native_id"], json!("42"));
    assert_eq!(entries[1]["workspace"], json!("1"));
    assert_eq!(entries[1]["stacking_order"], json!(1));
    assert_eq!(entries[1]["states"], json!(["Minimized"]));
    assert_eq!(entries[1]["window_type"], json!("Dialog"));
    assert_eq!(entries[1]["keep_above"], json!(true));
}

/// C-042：零窗口桌面返回空列表（零退出语义），不是解析失败——与「条目全被
/// 丢弃」区分，后者必须报错。
#[tokio::test]
async fn gnome_eval_empty_window_list_is_an_empty_list() {
    let bus = PrivateBus::start();
    let (_server, bridge) = eval_bridge(&bus, true, &shell_eval_result("[]")).await;

    let windows = bridge
        .list_windows()
        .await
        .expect("empty desktop is a valid empty list");
    assert!(windows.is_empty());
    let active = bridge
        .get_active_window()
        .await
        .expect("empty desktop has no active window");
    assert!(active.is_none());
}

/// 非数组 / 全部条目被丢弃 / 内层非 JSON / 壳侧失败都必须明确报错，
/// 不得静默成空集（C-151）。
#[tokio::test]
async fn gnome_eval_window_payload_failures_are_explicit_errors() {
    // 非数组：协议层是合法 JSON 字符串，内层不是数组。
    let not_array = PrivateBus::start();
    let (_server, bridge) = eval_bridge(&not_array, true, &shell_eval_result("42")).await;
    let err = bridge
        .list_windows()
        .await
        .expect_err("non-array payload must be an error");
    assert!(matches!(err, MutterError::Eval(_)), "got {err:?}");

    // 字段缺失：数组里每条都缺原生标识 → 全部丢弃 → 明确失败。
    let all_dropped = PrivateBus::start();
    let (_server, bridge) = eval_bridge(
        &all_dropped,
        true,
        &shell_eval_result(r#"[{"title":"no id"}]"#),
    )
    .await;
    let err = bridge
        .list_windows()
        .await
        .expect_err("all-incomplete payload must be an error");
    assert!(matches!(err, MutterError::Eval(_)), "got {err:?}");

    // 焦点条目缺原生标识：hasFocus 已命中却无法投影，必须报错——不得静默
    // 退化成「没有活动窗口」（`list_windows` 丢弃该条目的语义不适用于这里）。
    let focused_without_id = PrivateBus::start();
    let (_server, bridge) = eval_bridge(
        &focused_without_id,
        true,
        &shell_eval_result(r#"[{"title":"no id","hasFocus":true}]"#),
    )
    .await;
    let err = bridge
        .get_active_window()
        .await
        .expect_err("focused entry without a native id must be an error");
    assert!(matches!(err, MutterError::Eval(_)), "got {err:?}");

    // 内层不是 JSON（脚本返回非 JSON 文本）。
    let mangled = PrivateBus::start();
    let (_server, bridge) = eval_bridge(&mangled, true, &shell_eval_result("not-json")).await;
    let err = bridge
        .list_windows()
        .await
        .expect_err("inner non-JSON must be an error");
    assert!(matches!(err, MutterError::Eval(_)), "got {err:?}");

    // 壳侧失败（Eval 被禁用 / 脚本抛错）：success=false 必须报错。
    let shell_failed = PrivateBus::start();
    let (_server, bridge) = eval_bridge(&shell_failed, false, "").await;
    let err = bridge
        .list_windows()
        .await
        .expect_err("shell-side failure must be an error");
    assert!(matches!(err, MutterError::Eval(_)), "got {err:?}");
}
