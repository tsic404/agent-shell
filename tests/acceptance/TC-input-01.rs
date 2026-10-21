//! TC-input-01（AC-30，P0）：修饰键组合与非法键名。
//!
//! - 场景：`input key <combo>` 的修饰键组合、别名等价与非法键名
//! - 判据：组合键触发对应动作；非法键名明确报错且非静默丢弃
//! - 执行：`cargo test -p agent-shell-acceptance --test TC-input-01`
//!
//! 判定面是真实 `input.send` → `input::prepare` → 真实 dispatcher → 候选链。
//! 候选链由记录型夹具给出；"目标应用收到动作"的桌面侧观察需要真实会话，
//! 本用例锚定注入前的组合解析、后端分派与错误语义（不静默丢弃）。

use acceptance::{Fixture, RecordedOp, RecordingInput, SetupBehaviour};
use agent_shell_core::types::{DesktopEnvironment, Key, KeyCombo, KeyName, ModifierMask};
use agent_shell_rpc::{method, RpcErrorCode};
use serde_json::json;

fn key_params(combo: &str) -> serde_json::Value {
    json!({"kind": "key", "payload": {"combo": combo}})
}

fn expected(modifiers: ModifierMask, keys: Vec<Key>) -> RecordedOp {
    RecordedOp::Key(KeyCombo { keys, modifiers })
}

#[tokio::test]
async fn modifier_combo_reaches_backend_and_illegal_tokens_fail_loudly() {
    let mut fixture = Fixture::start().await;
    let backend = RecordingInput::new("record");
    fixture
        .set_input_chain(DesktopEnvironment::KDE, vec![Box::new(backend.clone())])
        .await;

    // 修饰键组合：ctrl+shift+t 原样到达后端。
    let result = fixture
        .call_ok(method::INPUT_SEND, Some(key_params("ctrl+shift+t")))
        .await;
    assert_eq!(result.get("ok"), Some(&json!(true)));
    assert_eq!(result.get("backend"), Some(&json!("record")));
    assert_eq!(
        backend.calls(),
        vec![expected(
            ModifierMask {
                ctrl: true,
                alt: false,
                shift: true,
                meta: false,
            },
            vec![Key::Char('t')],
        )]
    );

    // 别名与大小写不敏感：三种写法解析为同一组合。
    backend.clear();
    for spec in ["CTRL+Return", "control+enter", "Ctrl+RETURN"] {
        fixture
            .call_ok(method::INPUT_SEND, Some(key_params(spec)))
            .await;
    }
    let calls = backend.calls();
    assert_eq!(calls.len(), 3);
    for call in &calls {
        assert_eq!(
            call,
            &expected(
                ModifierMask {
                    ctrl: true,
                    ..ModifierMask::NONE
                },
                vec![Key::Named(KeyName::Return)],
            ),
            "all spellings must parse to the same combo"
        );
    }

    // 非法键名：明确报错、错误文本点名 token、后端零调用（不静默丢弃）。
    backend.clear();
    let error = fixture
        .call_err(method::INPUT_SEND, Some(key_params("ctrl+NoSuch")))
        .await;
    assert_eq!(error.code, RpcErrorCode::InvalidParams as i32);
    assert!(
        error.message.contains("unknown key name: nosuch"),
        "error must name the offending token: {}",
        error.message
    );
    assert!(
        backend.calls().is_empty(),
        "invalid combo must not reach the backend"
    );

    // 只有修饰键：同样在解析层拒绝。
    let error = fixture
        .call_err(method::INPUT_SEND, Some(key_params("ctrl+alt")))
        .await;
    assert_eq!(error.code, RpcErrorCode::InvalidParams as i32);
    assert!(
        error
            .message
            .contains("no non-modifier key in combo: ctrl+alt"),
        "error must explain the missing non-modifier key: {}",
        error.message
    );
    assert!(backend.calls().is_empty());

    // 通道建立失败的安全降级：键击落到下一候选，回执报实际后端。
    let mut degraded = Fixture::start().await;
    let first = RecordingInput::new("native").with_setup(SetupBehaviour::BackendUnavailable);
    let second = RecordingInput::new("record-2");
    degraded
        .set_input_chain(
            DesktopEnvironment::KDE,
            vec![Box::new(first.clone()), Box::new(second.clone())],
        )
        .await;
    let result = degraded
        .call_ok(method::INPUT_SEND, Some(key_params("ctrl+c")))
        .await;
    assert_eq!(result.get("backend"), Some(&json!("record-2")));
    assert!(first.calls().is_empty());
    assert_eq!(
        second.calls(),
        vec![expected(
            ModifierMask {
                ctrl: true,
                ..ModifierMask::NONE
            },
            vec![Key::Char('c')],
        )]
    );
}
