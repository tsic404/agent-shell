//! TC-input-06（AC-109，P1）：按键词表与组合键语法。
//!
//! - 场景：封闭词表、别名、大小写、空白、空段、多键序列、非法 token、规范串
//! - 判据：各语法分支行为与退出码符合；规范串写入与回读一致
//! - 执行：`cargo test -p agent-shell-acceptance --test TC-input-06`
//!
//! 词表与语法由 daemon 的 `input::parse_combo` 在 `input.send` 前执行；规范串
//! 由 `shortcut.bind` 的 Hyprland 后端（`ShortcutBinder::for_test` + 可执行
//! stub）出具，写入片段文件后读回同值。命名键的长名/短名与 CLI 表一致
//! （return/escape 等），字符键小写，修饰键按 ctrl、alt、shift、meta 排序。

use std::path::PathBuf;

use acceptance::{Fixture, RecordedOp, RecordingInput};
use agent_shell_core::types::{DesktopEnvironment, Key, KeyCombo, KeyName, ModifierMask};
use agent_shell_rpc::{method, RpcErrorCode};
use agent_shell_shortcut::{ShortcutBinder, ShortcutOps};
use serde_json::json;

fn key_params(combo: &str) -> serde_json::Value {
    json!({"kind": "key", "payload": {"combo": combo}})
}

#[tokio::test]
async fn vocabulary_grammar_and_canonical_string_hold() {
    let mut fixture = Fixture::start().await;
    let backend = RecordingInput::new("record");
    fixture
        .set_input_chain(DesktopEnvironment::KDE, vec![Box::new(backend.clone())])
        .await;

    // 词表封闭：命名键覆盖回车/转义/退格/制表/空格/方向/翻页/功能键。
    for spec in [
        "F5", "HOME", "PageUp", "Insert", "Delete", "Menu", "Left", "space", "1", "A",
    ] {
        fixture
            .call_ok(method::INPUT_SEND, Some(key_params(spec)))
            .await;
    }
    let calls = backend.calls();
    assert_eq!(calls.len(), 10);
    for (call, key) in calls.iter().zip([
        Key::Named(KeyName::F5),
        Key::Named(KeyName::Home),
        Key::Named(KeyName::PageUp),
        Key::Named(KeyName::Insert),
        Key::Named(KeyName::Delete),
        Key::Named(KeyName::Menu),
        Key::Named(KeyName::Left),
        Key::Named(KeyName::Space),
        Key::Char('1'),
        Key::Char('a'),
    ]) {
        assert_eq!(
            call,
            &RecordedOp::Key(KeyCombo {
                keys: vec![key.clone()],
                modifiers: ModifierMask::NONE,
            })
        );
    }

    // 大小写不敏感与别名：仅两个别名（enter/esc），其余简称非法。
    backend.clear();
    for spec in ["RETURN", "enter", "ctrl+Enter", "ESCAPE", "esc"] {
        fixture
            .call_ok(method::INPUT_SEND, Some(key_params(spec)))
            .await;
    }
    assert_eq!(backend.calls().len(), 5);
    for spec in ["crtl", "cmnd", "mod4", "option"] {
        let error = fixture
            .call_err(method::INPUT_SEND, Some(key_params(spec)))
            .await;
        assert_eq!(error.code, RpcErrorCode::InvalidParams as i32);
        assert!(
            error.message.contains("unknown key name"),
            "illegal alias must be rejected: {spec} -> {}",
            error.message
        );
    }

    // 空白不裁剪、空段静默丢弃、多键序列合法、仅修饰键报错。
    backend.clear();
    let error = fixture
        .call_err(method::INPUT_SEND, Some(key_params("ctrl + c")))
        .await;
    assert_eq!(error.code, RpcErrorCode::InvalidParams as i32);
    assert!(
        error.message.contains("unknown key name: ctrl "),
        "whitespace must not be trimmed: {}",
        error.message
    );
    fixture
        .call_ok(method::INPUT_SEND, Some(key_params("ctrl++c")))
        .await;
    fixture
        .call_ok(method::INPUT_SEND, Some(key_params("a+b")))
        .await;
    assert_eq!(backend.calls().len(), 2);
    let error = fixture
        .call_err(method::INPUT_SEND, Some(key_params("ctrl+alt")))
        .await;
    assert_eq!(error.code, RpcErrorCode::InvalidParams as i32);
    assert!(
        error.message.contains("no non-modifier key in combo"),
        "modifiers-only must be rejected: {}",
        error.message
    );

    // 规范串：Hyprland 后端出具 canonical 并写回片段，读回一致。
    let config = tempfile::tempdir().expect("config dir");
    let hyprctl = config.path().join("hyprctl");
    std::fs::write(&hyprctl, "#!/bin/sh\necho ok\nexit 0\n").expect("write hyprctl stub");
    std::fs::set_permissions(
        &hyprctl,
        std::os::unix::fs::PermissionsExt::from_mode(0o755),
    )
    .expect("make hyprctl stub executable");
    let binder = ShortcutBinder::for_test(
        DesktopEnvironment::Hyprland,
        None,
        config.path().join("data"),
        config.path().join("config"),
        PathBuf::from("gsettings"),
        hyprctl,
    );
    let combo = KeyCombo {
        keys: vec![Key::Named(KeyName::Return)],
        modifiers: ModifierMask {
            meta: true,
            shift: true,
            ctrl: true,
            alt: true,
        },
    };
    let binding = binder
        .bind(&combo, "notify-send hi")
        .await
        .expect("hyprland bind with stub");
    assert_eq!(binding.backend, "hyprland");
    assert_eq!(
        binding.combo, "ctrl+alt+shift+meta+return",
        "canonical form"
    );
    let fragment = config
        .path()
        .join("config")
        .join("hypr")
        .join("agent-shell.conf");
    let fragment_text = std::fs::read_to_string(&fragment).expect("fragment written");
    let lowered = fragment_text.to_lowercase();
    assert!(
        lowered.contains("ctrl")
            && lowered.contains("return")
            && lowered.contains("exec, notify-send hi"),
        "fragment must carry the bound combo, key and action: {fragment_text}"
    );
}
