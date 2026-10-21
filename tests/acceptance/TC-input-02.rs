//! TC-input-02（AC-31，P0）：文本键入、特殊字符与空串。
//!
//! - 场景：`input type <text>` 的逐字键入、特殊字符与空串
//! - 判据：文本逐字一致；特殊输入不崩溃
//! - 执行：`cargo test -p agent-shell-acceptance --test TC-input-02`
//!
//! 判定面是真实 `input.send` → `input::prepare` → dispatcher：后端必须收到
//! 与请求逐字相同的字符串（含换行、制表、非 ASCII），空串与坏载荷各有确定
//! 语义。真实编辑器里的字符显现需要图形会话，由真机验收覆盖。

use acceptance::{Fixture, RecordedOp, RecordingInput};
use agent_shell_core::types::{DesktopEnvironment, Key, KeyCombo, KeyName, ModifierMask};
use agent_shell_rpc::{method, RpcErrorCode};
use serde_json::json;

fn type_params(text: &str) -> serde_json::Value {
    json!({"kind": "type_text", "payload": {"text": text}})
}

#[tokio::test]
async fn text_is_forwarded_verbatim_and_empty_input_is_defined() {
    let mut fixture = Fixture::start().await;
    let backend = RecordingInput::new("record");
    fixture
        .set_input_chain(DesktopEnvironment::KDE, vec![Box::new(backend.clone())])
        .await;

    // 逐字一致：空白、换行、制表、非 ASCII 不做任何转义或裁剪。
    let text = "hello 世界\tline2\nline3  ";
    let result = fixture
        .call_ok(method::INPUT_SEND, Some(type_params(text)))
        .await;
    assert_eq!(result.get("ok"), Some(&json!(true)));
    assert_eq!(
        backend.calls(),
        vec![RecordedOp::Text(text.to_string(), 0)],
        "backend must receive the exact text (delay 0)"
    );

    // 空串：可解释且不崩溃——到达后端、不报 RPC 错误。
    backend.clear();
    let result = fixture
        .call_ok(method::INPUT_SEND, Some(type_params("")))
        .await;
    assert_eq!(result.get("ok"), Some(&json!(true)));
    assert_eq!(backend.calls(), vec![RecordedOp::Text(String::new(), 0)]);

    // 特殊字符：命名键经 key 通道逐键抵达（不经过文本转义）。
    backend.clear();
    fixture
        .call_ok(
            method::INPUT_SEND,
            Some(json!({"kind": "key", "payload": {"combo": "Return"}})),
        )
        .await;
    fixture
        .call_ok(
            method::INPUT_SEND,
            Some(json!({"kind": "key", "payload": {"combo": "Tab"}})),
        )
        .await;
    fixture
        .call_ok(
            method::INPUT_SEND,
            Some(json!({"kind": "key", "payload": {"combo": "Esc"}})),
        )
        .await;
    assert_eq!(
        backend.calls(),
        vec![
            RecordedOp::Key(KeyCombo {
                keys: vec![Key::Named(KeyName::Return)],
                modifiers: ModifierMask::NONE,
            }),
            RecordedOp::Key(KeyCombo {
                keys: vec![Key::Named(KeyName::Tab)],
                modifiers: ModifierMask::NONE,
            }),
            RecordedOp::Key(KeyCombo {
                keys: vec![Key::Named(KeyName::Escape)],
                modifiers: ModifierMask::NONE,
            }),
        ]
    );

    // 坏载荷：类型错误在解析层拒绝，后端零调用。
    backend.clear();
    for payload in [json!({}), json!({"text": 42}), json!({"text": null})] {
        let error = fixture
            .call_err(
                method::INPUT_SEND,
                Some(json!({"kind": "type_text", "payload": payload})),
            )
            .await;
        assert_eq!(error.code, RpcErrorCode::InvalidParams as i32);
        assert!(
            error.message.contains("payload.text required"),
            "bad text payload must be InvalidParams: {}",
            error.message
        );
    }
    assert!(backend.calls().is_empty());
}
