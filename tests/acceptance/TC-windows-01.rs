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
use agent_shell_core::types::{
    DesktopEnvironment, MonitorId, WindowState, WindowType, WorkspaceId,
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
