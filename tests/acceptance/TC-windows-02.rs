//! TC-windows-02（AC-11，P1）：四种匹配模式与 app_id 匹配规则。
//!
//! - 场景：`windows.list` 的 `substring` / `exact` / `regex` / `glob`；
//!   同一过滤器下 app_id 与标题的不同判定；未命中与非法正则
//! - 判据：四种模式各自命中预期窗口；`substring`/`exact` 下 app_id 恒精确比对，
//!   `glob`/`regex` 才作为模式作用于 app_id；未命中返回空数组而非错误；非法
//!   正则视为无匹配而非 RPC 错误
//! - 执行：`cargo test -p agent-shell-acceptance --test TC-windows-02`
//!
//! 判定面是真实 `windows.list` 的过滤语义（daemon 真实 `dispatch` + 真实
//! `TitleMatcher`）；窗口来源为脚本化合成器。无需图形会话，无环境绑定部分。

use acceptance::{window, FakeCompositor, Fixture};
use agent_shell_core::types::DesktopEnvironment;
use agent_shell_rpc::method;
use serde_json::{json, Value};

fn ids(result: &Value) -> Vec<String> {
    result["windows"]
        .as_array()
        .expect("windows.list carries a windows array")
        .iter()
        .map(|w| {
            w["native_id"]
                .as_str()
                .expect("entry carries native_id")
                .to_string()
        })
        .collect()
}

/// 发一条带过滤器的 `windows.list` 并返回命中窗口的 native_id。
async fn query(fixture: &mut Fixture, filter: &str, mode: &str) -> Vec<String> {
    let params = json!({"filter": filter, "match": mode});
    let result = fixture.call_ok(method::WINDOWS_LIST, Some(params)).await;
    ids(&result)
}

#[tokio::test]
async fn match_modes_select_the_documented_windows_and_never_error_on_no_match() {
    let mut fixture = Fixture::start().await;
    let compositor = FakeCompositor::with_windows(vec![
        window("w-1", "Terminal — Konsole", "konsole", 0, 0, 800, 600),
        window(
            "w-2",
            "GIMP Image Editor",
            "org.gimp.GIMP",
            10,
            10,
            900,
            700,
        ),
        window("w-3", "Files", "org.gnome.Nautilus", 20, 20, 640, 480),
    ]);
    fixture.install_compositor(&compositor, DesktopEnvironment::KDE);

    // substring：标题子串命中。
    assert_eq!(
        query(&mut fixture, "Konsole", "substring").await,
        vec!["w-1"]
    );
    // substring：app_id 不作子串匹配（"kon" 不是 app_id "konsole" 的精确值，
    // 标题也不含该子串）——空结果证明 app_id 走精确比对。
    assert!(query(&mut fixture, "kon", "substring").await.is_empty());
    // substring：app_id 精确值仍可命中。
    assert_eq!(
        query(&mut fixture, "konsole", "substring").await,
        vec!["w-1"]
    );

    // exact：标题整体相等才命中。
    assert_eq!(
        query(&mut fixture, "GIMP Image Editor", "exact").await,
        vec!["w-2"]
    );
    assert!(query(&mut fixture, "GIMP", "exact").await.is_empty());
    // exact：app_id 精确值命中。
    assert_eq!(
        query(&mut fixture, "org.gimp.GIMP", "exact").await,
        vec!["w-2"]
    );

    // regex：模式作用于标题，也作用于 app_id。
    assert_eq!(
        query(&mut fixture, "Terminal.*Konsole", "regex").await,
        vec!["w-1"]
    );
    assert_eq!(
        query(&mut fixture, r"^org\.gnome\.", "regex").await,
        vec!["w-3"]
    );

    // glob：模式作用于标题，也作用于 app_id。
    assert_eq!(query(&mut fixture, "*Konsole*", "glob").await, vec!["w-1"]);
    assert_eq!(query(&mut fixture, "org.gimp.*", "glob").await, vec!["w-2"]);

    // 未命中：空数组 + RPC 成功（不是 NotFound）。
    assert!(query(&mut fixture, "NoSuchWindow", "substring")
        .await
        .is_empty());

    // 非法正则：视为无匹配，而不是参数错误或后端错误。
    assert!(query(&mut fixture, "[", "regex").await.is_empty());
    // 非法的、但可编译的正则同样只是无匹配。
    assert!(query(&mut fixture, "(?P<bad", "regex").await.is_empty());
}
