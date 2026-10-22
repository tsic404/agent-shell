//! TC-system-09（AC-78，P2）：daemon 状态与 portal 会话列表。
//!
//! - 场景：`daemon.status` 报告运行态、订阅者数与当前 IME 引擎；写入
//!   ScreenCast 恢复令牌后 `daemon.sessions` 列出该会话。
//! - 判据：status 必含 `running=true`/`subscribers`/`ime_engine`（`ime_engine`
//!   初始为 null，设置引擎后同源反映；冷缓存时省略 `windows_cached`）；
//!   sessions 是 `{"sessions":[...]}`，写令牌后出现
//!   `kind=screencast`、`restore_token` 精确回读、`persist_mode=2`、
//!   `created_at` 非空的条目。
//! - 执行：`cargo test -p agent-shell-acceptance --test TC-system-09`
//!
//! 环境限制：doctor 截图行与恢复令牌的一致性取决于真实 portal/ScreenCast
//! 会话，headless 验收机没有 portal；因此本用例只断言可观察的 daemon 状态与
//! 持久化会话条目（写令牌走 daemon 持有的 `PortalSessionManager`，即
//! capture 的 restore_token 后端本身），不对 doctor 行文本做环境绑定断言。

use acceptance::Fixture;
use agent_shell_rpc::method;
use serde_json::{json, Value};

fn keys(value: &Value) -> Vec<String> {
    let mut keys: Vec<String> = value
        .as_object()
        .unwrap_or_else(|| panic!("expected a JSON object, got: {value}"))
        .keys()
        .cloned()
        .collect();
    keys.sort();
    keys
}

#[tokio::test]
async fn daemon_status_and_portal_session_listing() {
    let mut fixture = Fixture::start().await;

    // 1. 初始状态：运行中、无订阅者、无 IME 引擎、冷窗口缓存省略字段。
    let status = fixture.call_ok(method::DAEMON_STATUS, None).await;
    for required in ["running", "subscribers", "ime_engine"] {
        assert!(
            status.get(required).is_some(),
            "daemon.status 必须携带 {required}，got: {status}"
        );
    }
    let mut allowed = keys(&status);
    allowed.retain(|key| key != "windows_cached");
    assert_eq!(
        allowed,
        vec![
            "ime_engine".to_string(),
            "running".to_string(),
            "subscribers".to_string()
        ],
        "daemon.status 不得出现协议外字段，got: {status}"
    );
    assert_eq!(status["running"], true);
    assert_eq!(status["subscribers"], 0);
    assert_eq!(status["ime_engine"], Value::Null);

    // 2. 初始会话列表：结构固定，空列表而非缺键。
    let sessions = fixture.call_ok(method::DAEMON_SESSIONS, None).await;
    assert_eq!(keys(&sessions), vec!["sessions".to_string()]);
    assert_eq!(
        sessions["sessions"],
        json!([]),
        "全新夹具不得报告任何 portal 会话"
    );

    // 3. 写入 ScreenCast 恢复令牌（capture 的 TokenStore 真实后端）后可见。
    const TOKEN: &str = "tc-system-09-restore-token";
    fixture
        .save_screencast_token(TOKEN)
        .expect("save screencast restore token");
    let sessions = fixture.call_ok(method::DAEMON_SESSIONS, None).await;
    let entries = sessions["sessions"]
        .as_array()
        .unwrap_or_else(|| panic!("sessions must be an array, got: {sessions}"));
    assert_eq!(entries.len(), 1, "写入后应恰有一个 screencast 会话");
    let entry = &entries[0];
    assert_eq!(
        keys(entry),
        vec![
            "created_at".to_string(),
            "kind".to_string(),
            "persist_mode".to_string(),
            "restore_token".to_string()
        ]
    );
    assert_eq!(entry["kind"], "screencast");
    assert_eq!(entry["restore_token"], TOKEN);
    assert_eq!(
        entry["persist_mode"], 2,
        "screencast 恢复令牌必须按 persist_until_revoked(2) 持久化"
    );
    assert!(
        entry["created_at"]
            .as_str()
            .is_some_and(|stamp| !stamp.is_empty()),
        "created_at 必须是非空时间戳，got: {}",
        entry["created_at"]
    );

    // 4. status 的 ime_engine 与 IME 会话同源（不是另存副本）。
    fixture
        .call_ok(method::IME_ENGINE_SET, Some(json!({ "engine": "wubi" })))
        .await;
    let status = fixture.call_ok(method::DAEMON_STATUS, None).await;
    assert_eq!(status["ime_engine"], "wubi");
    assert_eq!(status["running"], true);
    assert_eq!(status["subscribers"], 0);
}
