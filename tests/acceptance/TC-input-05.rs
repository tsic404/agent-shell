//! TC-input-05（AC-34，P1）：Wayland 降级链、X11 降级链与 XWayland 负向判定。
//!
//! - 场景：候选链门控、选中后端、XWayland 下不得把 XTest 当通道、全链不可用
//! - 判据：候选链符合门控；XWayland 会话不出现 XTest；全链不可用时报错
//! - 执行：`cargo test -p agent-shell-acceptance --test TC-input-05`
//!
//! 门控谓词（xdotool 的 `DISPLAY`+PATH 判定、XTest 仅 X11Generic 候选）用
//! `InputDispatcher::new_with_native` 的真实装配路径驱动：夹具环境可按门控
//! 需要构造（PATH 里的 xdotool stub、DISPLAY/WAYLAND_DISPLAY 组合），libei 与
//! uinput 的宿主探测不可离线复现，链形断言只取「候选名是否出现」这一层。
//! 降级与选中语义走 `Fixture::set_input_chain` 的记录型后端（同一生产 dispatcher）。

use acceptance::{Fixture, RecordingInput, SetupBehaviour};
use agent_shell_core::types::DesktopEnvironment;
use agent_shell_input::InputDispatcher;
use agent_shell_rpc::{method, RpcErrorCode};
use serde_json::json;

/// 在临时目录放一个可执行的 xdotool stub（PATH 门控的可用性来源）。
fn stub_dir() -> tempfile::TempDir {
    let dir = tempfile::tempdir().expect("stub dir");
    let stub = dir.path().join("xdotool");
    std::fs::write(&stub, "#!/bin/sh\nexit 0\n").expect("write xdotool stub");
    std::fs::set_permissions(&stub, std::os::unix::fs::PermissionsExt::from_mode(0o755))
        .expect("make stub executable");
    dir
}

#[tokio::test]
async fn candidate_gates_and_xwayland_x_test_negative_hold() {
    // KDE Wayland：xdotool 不得入链（XWayland 会话禁用 xdotool 通道）。
    let stub = stub_dir();
    let mut wayland_env = Fixture::start_with_env(&[
        ("DISPLAY", ":0".to_string()),
        ("WAYLAND_DISPLAY", "wayland-test".to_string()),
        (
            "PATH",
            format!(
                "{}:{}",
                stub.path().display(),
                std::env::var("PATH").unwrap_or_default()
            ),
        ),
    ])
    .await;
    wayland_env
        .set_input_chain(
            DesktopEnvironment::KDE,
            vec![Box::new(RecordingInput::new("noop"))],
        )
        .await;
    // 直接构造真实装配路径：门控在 `InputDispatcher::new_with_native` 内执行。
    let chain = InputDispatcher::new_with_native(DesktopEnvironment::KDE, None, None)
        .await
        .expect("dispatcher assembles even without candidates");
    assert!(
        !chain.backend_names().contains(&"xdotool"),
        "KDE Wayland session must not offer xdotool; got {:?}",
        chain.backend_names()
    );
    assert!(
        !chain.backend_names().contains(&"xtest"),
        "KDE Wayland session must never offer XTest; got {:?}",
        chain.backend_names()
    );

    // Hyprland（Wayland 系、无 KDE 专属禁用）：xdotool 按 DISPLAY+PATH 入链。
    let hypr = InputDispatcher::new_with_native(DesktopEnvironment::Hyprland, None, None)
        .await
        .expect("hyprland dispatcher");
    assert!(
        hypr.backend_names().contains(&"xdotool"),
        "Hyprland with DISPLAY must offer xdotool; got {:?}",
        hypr.backend_names()
    );

    // 夹具注入的降级链：首个可用候选为 active，先前候选被摘除。
    let mut fixture = Fixture::start().await;
    let native = RecordingInput::new("native").with_setup(SetupBehaviour::BackendUnavailable);
    let libei = RecordingInput::new("libei");
    fixture
        .set_input_chain(
            DesktopEnvironment::KDE,
            vec![Box::new(native.clone()), Box::new(libei.clone())],
        )
        .await;
    assert_eq!(fixture.input_backend_name(), Some("native"));
    // 通道建立失败在首次操作时摘除该候选并降级重放。
    let result = fixture
        .call_ok(
            method::INPUT_SEND,
            Some(json!({"kind": "key", "payload": {"combo": "ctrl+c"}})),
        )
        .await;
    assert_eq!(result.get("backend"), Some(&json!("libei")));
    assert!(native.calls().is_empty());

    // 全链不可用：`input.send` 明确报错而非静默成功。
    let mut empty = Fixture::start().await;
    empty.set_input_unavailable(Some("portal 缺注入接口"));
    let error = empty
        .call_err(
            method::INPUT_SEND,
            Some(json!({"kind": "key", "payload": {"combo": "a"}})),
        )
        .await;
    assert_eq!(error.code, RpcErrorCode::BackendUnavailable as i32);
    assert!(
        error.message.contains("portal 缺注入接口"),
        "empty-chain error must surface the probe reason: {}",
        error.message
    );
}
