//! TC-system-08（AC-77，P2）：IME 引擎列表与文本输入验证语义。
//!
//! - 场景：`ime.engine.list` / `ime.engine.current` / `ime.type` 走 daemon
//!   持有的 `ImeSession`。
//! - 判据：引擎列表为固定的四种引擎；ASCII 文本（含空串）回执
//!   `verified=true` 且 committed 原样保留；非 ASCII 文本 committed 原样保留
//!   但 `verified=false`（v1 stub 不虚报已提交）；缺 text/非字符串 → `-32602`。
//! - 执行：`cargo test -p agent-shell-acceptance --test TC-system-08`
//!
//! 环境限制：IBus 连接属于 Phase 3 接线，daemon 没有「禁用 IME」的装配字段
//! ——`ImeSession` 恒存在且引擎表是进程内固定值，因此不存在可在 headless
//! 环境注入的 no-IME 分支；本用例据实断言该表面（列表恒成功、无非 ASCII
//! 谎报）。真实 IBus `ProcessKeyEvent`/`CommitText` 待接线后另立端到端用例。

use acceptance::Fixture;
use agent_shell_rpc::{method, RpcErrorCode};
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
async fn ime_engines_and_text_verification_flags() {
    let mut fixture = Fixture::start().await;

    // 1. 引擎列表：固定四引擎，顺序稳定。
    let listed = fixture.call_ok(method::IME_ENGINE_LIST, None).await;
    assert_eq!(keys(&listed), vec!["engines".to_string()]);
    assert_eq!(
        listed["engines"],
        json!(["libpinyin", "pinyin", "wubi", "sunpinyin"]),
        "IME 引擎表是进程内固定值，got: {listed}"
    );

    // 2. 初始无当前引擎（明示 null，而非缺键）。
    let current = fixture.call_ok(method::IME_ENGINE_CURRENT, None).await;
    assert_eq!(current, json!({ "engine": Value::Null }));

    // 3. ASCII（含空串）是性能路径：直接提交且 verified=true。
    for text in ["hello world", "abc123!@#", ""] {
        let typed = fixture
            .call_ok(method::IME_TYPE, Some(json!({ "text": text })))
            .await;
        assert_eq!(
            keys(&typed),
            vec!["committed".to_string(), "verified".to_string()]
        );
        assert_eq!(typed["committed"], text, "committed 必须原样保留输入");
        assert_eq!(
            typed["verified"], true,
            "ASCII 文本 {text:?} 必须标记 verified=true"
        );
    }

    // 4. 非 ASCII：committed 保留，但 v1 stub 不得谎报已验证。
    for text in ["你好，世界", "naïve café", "🙂"] {
        let typed = fixture
            .call_ok(method::IME_TYPE, Some(json!({ "text": text })))
            .await;
        assert_eq!(
            typed["committed"], text,
            "非 ASCII committed 必须原样保留输入"
        );
        assert_eq!(
            typed["verified"], false,
            "非 ASCII 文本 {text:?} 在 v1 stub 下必须 verified=false"
        );
    }

    // 5. 参数校验：缺 text / 非字符串 → -32602（先于任何输入动作）。
    for params in [
        None,
        Some(json!({})),
        Some(json!({ "text": Value::Null })),
        Some(json!({ "text": 42 })),
    ] {
        let error = fixture.call_err(method::IME_TYPE, params.clone()).await;
        assert_eq!(
            error.code,
            RpcErrorCode::InvalidParams as i32,
            "非法 text 参数 {params:?} 必须返回 -32602，got: {error:?}"
        );
    }

    // 6. 设置引擎后 current 可回读（daemon.status 的 ime_engine 同源）。
    let set = fixture
        .call_ok(
            method::IME_ENGINE_SET,
            Some(json!({ "engine": "libpinyin" })),
        )
        .await;
    assert_eq!(set, json!({ "set": "libpinyin" }));
    let current = fixture.call_ok(method::IME_ENGINE_CURRENT, None).await;
    assert_eq!(current, json!({ "engine": "libpinyin" }));
}
