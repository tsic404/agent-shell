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
use std::time::{Duration, Instant};

fn key_params(combo: &str) -> serde_json::Value {
    json!({"kind": "key", "payload": {"combo": combo}})
}

fn expected(modifiers: ModifierMask, keys: Vec<Key>) -> RecordedOp {
    RecordedOp::Key(KeyCombo { keys, modifiers })
}

/// 授权恢复的端到端证据：真实 RemoteDesktop portal 的 `Start` Response 晚于
/// 首次命令预算到达时，第二条命令必须接续同一会话（不重新弹窗）完成
/// ConnectToEIS 与 EI 握手，并真的把按键写进 EIS peer。
#[tokio::test]
async fn delayed_portal_authorization_resumes_the_same_session() {
    use acceptance::{serve_remote_desktop, FakeEis, RemoteDesktopMock};
    use agent_shell_input::{InputService, LibeiInput, Op, SetupFailure};

    let bus = acceptance::PrivateBus::start();
    let client = bus.connect().await;

    let (portal_side, eis_side) = std::os::unix::net::UnixStream::pair().expect("socketpair");
    let std_fd: std::os::fd::OwnedFd = portal_side.into();
    let eis_fd = zbus::zvariant::OwnedFd::from(std_fd);
    // `Start` 先挂起：复刻用户在命令预算耗尽后才点「允许」。
    let portal = RemoteDesktopMock::new(eis_fd, true);
    let _server = serve_remote_desktop(&bus, portal.clone()).await;
    let eis = FakeEis::start(eis_side);

    let libei = LibeiInput::from_bus(client)
        .await
        .expect("probe the fixture portal");
    let combo = KeyCombo {
        keys: vec![Key::Named(KeyName::Return)],
        modifiers: ModifierMask::NONE,
    };

    // 第一条命令：授权未确认，预算内返回 Pending 而不是挂到 CLI 超时。
    let started = Instant::now();
    let pending = libei
        .ensure_ready(Op::Key(&combo))
        .await
        .expect_err("unconfirmed portal authorization must be pending");
    assert!(
        matches!(pending, SetupFailure::Pending(_)),
        "got: {pending}"
    );
    assert!(
        started.elapsed() < Duration::from_secs(4),
        "first command must return within the setup budget: {:?}",
        started.elapsed()
    );
    assert_eq!(portal.calls().creates, 1);
    assert_eq!(portal.calls().starts, 1);

    // 用户晚一些允许了已经弹出的对话框。
    assert_eq!(portal.release_start().await, 1, "one held Start response");

    // 第二条命令：接续同一会话，不重新 CreateSession/Start。
    libei
        .ensure_ready(Op::Key(&combo))
        .await
        .expect("late authorization must complete the pending session");
    assert_eq!(
        portal.calls().creates,
        1,
        "resume must reuse the authorized portal session"
    );
    assert_eq!(
        portal.calls().starts,
        1,
        "resume must not raise a second authorization dialog"
    );
    assert_eq!(portal.calls().selects, 1);
    assert_eq!(portal.calls().connects, 1);
    assert!(eis.log().binds >= 1, "EI handshake must bind a device");

    // 注入真的到达 EIS peer（press + release 两条按键请求）。
    libei.send_key(&combo).await.expect("inject after resume");
    assert!(
        eis.wait_for_keys(2, Duration::from_secs(2)),
        "EIS peer must receive the injected key events: {:?}",
        eis.log()
    );
}

/// EI 能力绑定与健康/执行同判：完整键鼠能力下按键、绝对移动、点击、滚动
/// 都经真实 `input.send` 到达 EIS peer；peer 只广告键盘时 doctor 不得假报
/// 就绪，点击以能力错误失败。
#[tokio::test]
async fn ei_capabilities_match_doctor_and_mouse_events_reach_peer() {
    use acceptance::{serve_remote_desktop, FakeEis, RemoteDesktopMock};
    use agent_shell_core::types::DesktopEnvironment;
    use agent_shell_input::LibeiInput;
    use agent_shell_rpc::DoctorResult;

    // 完整键鼠能力：只放 libei 一条链，鼠标接口必须由客户端 bind 掩码争取。
    {
        let bus = acceptance::PrivateBus::start();
        let client = bus.connect().await;
        let (portal_side, eis_side) = std::os::unix::net::UnixStream::pair().expect("socketpair");
        let std_fd: std::os::fd::OwnedFd = portal_side.into();
        let portal = RemoteDesktopMock::new(zbus::zvariant::OwnedFd::from(std_fd), false);
        let _server = serve_remote_desktop(&bus, portal).await;
        let eis = FakeEis::start(eis_side);
        let libei = LibeiInput::from_bus(client)
            .await
            .expect("probe the fixture portal");

        let mut fixture = Fixture::start().await;
        fixture
            .set_input_chain(DesktopEnvironment::GNOME, vec![Box::new(libei)])
            .await;

        let key = fixture
            .call_ok(
                method::INPUT_SEND,
                Some(json!({"kind": "key", "payload": {"combo": "Return"}})),
            )
            .await;
        assert_eq!(key.get("backend"), Some(&json!("libei")));

        let doctor: DoctorResult =
            serde_json::from_value(fixture.call_ok(method::DOCTOR, None).await)
                .expect("DoctorResult");
        let input_line = doctor
            .lines
            .iter()
            .find(|line| line.contains("输入后端"))
            .expect("doctor input row");
        assert!(
            input_line.starts_with('✓'),
            "full EI capabilities must report ready: {input_line}"
        );

        let click = fixture
            .call_ok(
                method::INPUT_SEND,
                Some(json!({"kind": "click", "payload": {"button": "left", "at": [640, 360]}})),
            )
            .await;
        assert_eq!(click.get("backend"), Some(&json!("libei")));

        let scroll = fixture
            .call_ok(
                method::INPUT_SEND,
                Some(json!({"kind": "scroll", "payload": {"dx": 0, "dy": 3}})),
            )
            .await;
        assert_eq!(scroll.get("backend"), Some(&json!("libei")));

        assert!(
            eis.wait_until(
                |l| l.keys >= 2
                    && l.motion_absolute >= 1
                    && l.button_presses >= 1
                    && l.button_releases >= 1
                    && l.scrolls >= 1,
                Duration::from_secs(2),
            ),
            "all injected events must reach the EIS peer: {:?}",
            eis.log()
        );
        assert_eq!(
            eis.log().last_absolute,
            Some((640.0, 360.0)),
            "absolute motion must preserve logical pixel coordinates"
        );
        assert_eq!(
            eis.log().last_scroll,
            Some((0.0, 3.0)),
            "scroll delta must reach the peer"
        );
    }

    // 只有键盘能力的 peer：按键可用，但 doctor 必须报缺项，点击不得假装成功。
    {
        let bus = acceptance::PrivateBus::start();
        let client = bus.connect().await;
        let (portal_side, eis_side) = std::os::unix::net::UnixStream::pair().expect("socketpair");
        let std_fd: std::os::fd::OwnedFd = portal_side.into();
        let portal = RemoteDesktopMock::new(zbus::zvariant::OwnedFd::from(std_fd), false);
        let _server = serve_remote_desktop(&bus, portal).await;
        let eis = FakeEis::start_with_capabilities(
            eis_side,
            reis::event::DeviceCapability::Keyboard.into(),
        );
        let libei = LibeiInput::from_bus(client)
            .await
            .expect("probe the fixture portal");
        let mut fixture = Fixture::start().await;
        fixture
            .set_input_chain(DesktopEnvironment::GNOME, vec![Box::new(libei)])
            .await;

        fixture
            .call_ok(
                method::INPUT_SEND,
                Some(json!({"kind": "key", "payload": {"combo": "Return"}})),
            )
            .await;
        assert!(eis.wait_for_keys(2, Duration::from_secs(2)));

        let doctor: DoctorResult =
            serde_json::from_value(fixture.call_ok(method::DOCTOR, None).await)
                .expect("DoctorResult");
        let input_line = doctor
            .lines
            .iter()
            .find(|line| line.contains("输入后端"))
            .expect("doctor input row");
        assert!(
            input_line.starts_with('⚠'),
            "missing mouse capabilities must degrade the health row: {input_line}"
        );
        assert!(
            input_line.contains("absolute-pointer") && input_line.contains("button"),
            "health row must name the missing capabilities: {input_line}"
        );

        let error = fixture
            .call_err(
                method::INPUT_SEND,
                Some(json!({"kind": "click", "payload": {"button": "left", "at": [10, 10]}})),
            )
            .await;
        assert_eq!(error.code, RpcErrorCode::BackendUnavailable as i32);
        assert!(
            error.message.contains("absolute-pointer") || error.message.contains("button"),
            "execution must report the missing capability: {}",
            error.message
        );
    }
}

/// GNOME 把指针、绝对指针与键盘拆成三个 `ei_device`。客户端必须聚合整批
/// 设备而不是只留首个：四条 `input.send` 与 doctor 行都要按同一能力集合判定，
/// 按键、绝对移动、点击、滚动都要真的到达 EIS peer。
#[tokio::test]
async fn split_eis_devices_are_aggregated_for_all_input_paths() {
    use acceptance::{serve_remote_desktop, FakeEis, RemoteDesktopMock};
    use agent_shell_core::types::DesktopEnvironment;
    use agent_shell_input::LibeiInput;
    use agent_shell_rpc::DoctorResult;

    let bus = acceptance::PrivateBus::start();
    let client = bus.connect().await;
    let (portal_side, eis_side) = std::os::unix::net::UnixStream::pair().expect("socketpair");
    let std_fd: std::os::fd::OwnedFd = portal_side.into();
    let portal = RemoteDesktopMock::new(zbus::zvariant::OwnedFd::from(std_fd), false);
    let _server = serve_remote_desktop(&bus, portal).await;
    let eis = FakeEis::start_split_devices(eis_side);
    let libei = LibeiInput::from_bus(client)
        .await
        .expect("probe the fixture portal");

    let mut fixture = Fixture::start().await;
    fixture
        .set_input_chain(DesktopEnvironment::GNOME, vec![Box::new(libei)])
        .await;

    for params in [
        json!({"kind": "key", "payload": {"combo": "Return"}}),
        json!({"kind": "type_text", "payload": {"text": "hi"}}),
        json!({"kind": "click", "payload": {"button": "left", "at": [640, 360]}}),
        json!({"kind": "scroll", "payload": {"dx": 0, "dy": 3}}),
    ] {
        let result = fixture.call_ok(method::INPUT_SEND, Some(params)).await;
        assert_eq!(result.get("backend"), Some(&json!("libei")));
    }

    let doctor: DoctorResult =
        serde_json::from_value(fixture.call_ok(method::DOCTOR, None).await).expect("DoctorResult");
    let input_line = doctor
        .lines
        .iter()
        .find(|line| line.contains("输入后端"))
        .expect("doctor input row");
    assert!(
        input_line.starts_with('✓'),
        "capabilities split across devices must still report ready: {input_line}"
    );

    assert!(
        eis.wait_until(
            |l| l.devices >= 3
                && l.keys >= 6
                && l.motion_absolute >= 1
                && l.button_presses >= 1
                && l.button_releases >= 1
                && l.scrolls >= 1,
            Duration::from_secs(2),
        ),
        "all split-device capabilities must reach the EIS peer: {:?}",
        eis.log()
    );
    assert_eq!(eis.log().last_absolute, Some((640.0, 360.0)));
    assert_eq!(eis.log().last_scroll, Some((0.0, 3.0)));
}

/// 整条命令预算取消复合命令时，pending portal 会话必须留在持久状态：
/// `click --at` 首段降级成功后，第二段恢复 libei 被外层 4s 取消；用户随后
/// 确认原弹窗，下一条命令接续同一会话完成建立，不重新授权。
#[tokio::test]
async fn cancelled_compound_command_keeps_pending_session_for_resume() {
    use acceptance::{serve_remote_desktop, FakeEis, RecordingInput, RemoteDesktopMock};
    use agent_shell_core::types::DesktopEnvironment;
    use agent_shell_input::LibeiInput;

    let bus = acceptance::PrivateBus::start();
    let client = bus.connect().await;
    let (portal_side, eis_side) = std::os::unix::net::UnixStream::pair().expect("socketpair");
    let std_fd: std::os::fd::OwnedFd = portal_side.into();
    let portal = RemoteDesktopMock::new(zbus::zvariant::OwnedFd::from(std_fd), true);
    let _server = serve_remote_desktop(&bus, portal.clone()).await;
    let eis = FakeEis::start(eis_side);
    let libei = LibeiInput::from_bus(client)
        .await
        .expect("probe the fixture portal");
    let fallback = RecordingInput::new("uinput");

    let mut fixture = Fixture::start().await;
    fixture
        .set_input_chain(
            DesktopEnvironment::GNOME,
            vec![Box::new(libei), Box::new(fallback.clone())],
        )
        .await;

    let params = json!({"kind": "click", "payload": {"button": "left", "at": [640, 360]}});
    let started = Instant::now();
    let error = fixture
        .call_err(method::INPUT_SEND, Some(params.clone()))
        .await;
    assert_eq!(error.code, RpcErrorCode::BackendError as i32);
    assert!(
        error.message.contains("did not complete within"),
        "the whole command must hit the daemon budget: {}",
        error.message
    );
    assert!(
        started.elapsed() >= Duration::from_millis(3_500),
        "first command must have waited for authorization: {:?}",
        started.elapsed()
    );
    assert_eq!(portal.calls().creates, 1);
    assert_eq!(portal.calls().starts, 1);
    assert_eq!(portal.calls().connects, 0);
    assert_eq!(
        fallback.calls(),
        vec![RecordedOp::Move(640, 360)],
        "the move segment falls back while libei waits for authorization"
    );

    // 用户确认原弹窗；下一条命令必须接续同一会话。
    assert_eq!(portal.release_start().await, 1, "one held Start response");
    let resumed = fixture.call_ok(method::INPUT_SEND, Some(params)).await;
    assert_eq!(resumed.get("backend"), Some(&json!("libei")));
    assert_eq!(
        portal.calls().creates,
        1,
        "resume must not create a new session"
    );
    assert_eq!(portal.calls().starts, 1, "resume must not prompt again");
    assert_eq!(portal.calls().selects, 1);
    assert_eq!(portal.calls().connects, 1);
    assert_eq!(
        fallback.calls(),
        vec![RecordedOp::Move(640, 360)],
        "resumed libei must handle both segments"
    );
    assert!(
        eis.wait_until(
            |l| l.motion_absolute >= 1 && l.button_presses >= 1 && l.button_releases >= 1,
            Duration::from_secs(2),
        ),
        "resumed session must inject move and click to the EIS peer: {:?}",
        eis.log()
    );
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
