//! TC-input-07（AC-122，P1）：常驻形态下授权待确认会话跨命令续用与自身预算。
//!
//! - 场景：常驻服务实例持有 portal 会话，用户在授权界面确认前命令先返回
//! - 判据：首条命令在自身预算内返回「授权待确认」且界面保留；确认后下一条
//!   命令接续同一会话完成建立与注入；首次建立后再次调用不再出现授权请求；
//!   预算内的后续调用不重置预算；无后续调用时会话预算耗尽撤下界面，下一次
//!   调用重新发起授权请求
//! - 执行：`cargo test -p agent-shell-acceptance --test TC-input-07`
//!
//! 判定面是真实 RemoteDesktop portal 协议（请求/Response/会话对象）与真实 EI
//! 握手。预算用缩短值验证语义：生产值见 `PORTAL_SESSION_BUDGET` 与
//! `LIBEI_SETUP_BUDGET`。

use std::time::{Duration, Instant};

use acceptance::{serve_remote_desktop, FakeEis, RemoteDesktopMock};
use agent_shell_core::types::{Key, KeyCombo, KeyName, ModifierMask};
use agent_shell_input::{InputService, LibeiInput, Op, SetupFailure};

/// 单条命令的建立预算（缩短；生产 3s）。
const COMMAND_BUDGET: Duration = Duration::from_millis(250);
/// 待确认会话自身预算（缩短；生产 60s）。
const SESSION_BUDGET: Duration = Duration::from_millis(2_500);

fn return_key() -> KeyCombo {
    KeyCombo {
        keys: vec![Key::Named(KeyName::Return)],
        modifiers: ModifierMask::NONE,
    }
}

async fn wait_until(deadline: Duration, mut predicate: impl FnMut() -> bool) -> bool {
    let started = Instant::now();
    while started.elapsed() < deadline {
        if predicate() {
            return true;
        }
        tokio::time::sleep(Duration::from_millis(20)).await;
    }
    predicate()
}

/// 常驻形态：授权待确认会话由服务实例保留，后续命令接续同一请求；
/// 会话预算自首次请求起算，不因后续调用后移。
#[tokio::test]
async fn pending_authorization_survives_commands_and_keeps_its_own_budget() {
    let bus = acceptance::PrivateBus::start();
    let client = bus.connect().await;
    let (portal_side, eis_side) = std::os::unix::net::UnixStream::pair().expect("socketpair");
    let std_fd: std::os::fd::OwnedFd = portal_side.into();
    // `Start` 挂起：复刻用户尚未在授权界面确认。
    let portal = RemoteDesktopMock::new(zbus::zvariant::OwnedFd::from(std_fd), true);
    let _server = serve_remote_desktop(&bus, portal.clone()).await;
    let eis = FakeEis::start(eis_side);
    let libei = LibeiInput::from_bus_with_budgets(client, SESSION_BUDGET, COMMAND_BUDGET)
        .await
        .expect("probe the fixture portal");
    let combo = return_key();

    // (1) 首条命令：在自身预算内返回「授权待确认」，授权请求仍挂着。
    let first_started = Instant::now();
    let pending = libei
        .ensure_ready(Op::Key(&combo))
        .await
        .expect_err("unconfirmed authorization must be pending");
    assert!(
        matches!(pending, SetupFailure::Pending(_)),
        "got: {pending}"
    );
    assert!(
        first_started.elapsed() < COMMAND_BUDGET + Duration::from_millis(700),
        "first command must return inside its own budget: {:?}",
        first_started.elapsed()
    );
    assert_eq!(portal.calls().creates, 1);
    assert_eq!(portal.calls().starts, 1);
    assert_eq!(
        portal.held_starts(),
        1,
        "the authorization request must outlive the first command"
    );
    assert_eq!(
        portal.calls().closes,
        0,
        "pending request must not be closed"
    );
    assert_eq!(
        portal.calls().request_closes,
        0,
        "pending request must not be withdrawn"
    );

    // (2) 预算内的第二条命令仍待确认，但既不重新请求授权，也不重置预算。
    tokio::time::sleep(Duration::from_millis(900)).await;
    let second = libei
        .ensure_ready(Op::Key(&combo))
        .await
        .expect_err("still waiting for the same authorization");
    assert!(matches!(second, SetupFailure::Pending(_)), "got: {second}");
    assert_eq!(portal.calls().creates, 1, "must resume the same session");
    assert_eq!(portal.calls().starts, 1, "must not raise a second dialog");
    assert_eq!(portal.held_starts(), 1);

    // (3) 无后续调用：会话预算耗尽后请求关闭、界面撤下。预算起点是首次
    //     请求，第二条命令（t≈1.2s）不得把它推到 t≈3.7s。
    assert!(
        wait_until(Duration::from_secs(5), || portal.calls().closes >= 1).await,
        "session budget exhaustion must close the pending request: {:?}",
        portal.calls()
    );
    assert_eq!(
        portal.calls().request_closes,
        1,
        "expiry must withdraw the request object that owns the authorization dialog"
    );
    assert_eq!(portal.held_starts(), 0, "dialog must be withdrawn");
    let budget_elapsed = first_started.elapsed();
    assert!(
        budget_elapsed < SESSION_BUDGET + Duration::from_millis(900),
        "later commands must not extend the session budget: closed after {budget_elapsed:?}"
    );

    // (4) 预算耗尽后的下一次调用重新发起授权请求。
    let reissued = libei
        .ensure_ready(Op::Key(&combo))
        .await
        .expect_err("reissued authorization is unconfirmed");
    assert!(
        matches!(reissued, SetupFailure::Pending(_)),
        "got: {reissued}"
    );
    assert_eq!(
        portal.calls().creates,
        2,
        "next call must create a new session"
    );
    assert_eq!(portal.calls().starts, 2, "next call must prompt again");
    assert_eq!(portal.held_starts(), 1);

    // (5) 用户确认后，下一条命令接续同一会话完成建立，不重新弹窗。
    assert_eq!(portal.release_start().await, 1, "one held Start response");
    libei
        .ensure_ready(Op::Key(&combo))
        .await
        .expect("confirmation must complete the pending session");
    assert_eq!(portal.calls().creates, 2);
    assert_eq!(portal.calls().starts, 2, "no new dialog after confirmation");
    assert_eq!(portal.calls().selects, 2);
    assert_eq!(portal.calls().connects, 1);
    assert!(eis.log().binds >= 1, "EI handshake must bind a device");

    // (6) 建立完成后再次调用：不出现新的授权请求，注入仍然完成。
    libei
        .ensure_ready(Op::Key(&combo))
        .await
        .expect("established session is reused");
    libei
        .send_key(&combo)
        .await
        .expect("inject over reused session");
    assert!(
        eis.wait_for_keys(2, Duration::from_secs(2)),
        "injection must reach the EIS peer: {:?}",
        eis.log()
    );
    assert_eq!(portal.calls().creates, 2);
    assert_eq!(portal.calls().starts, 2);
    assert_eq!(portal.calls().connects, 1);
    assert_eq!(portal.held_starts(), 0, "no authorization request remains");
}

/// `Start` 方法应答本身晚于命令预算：请求已发出、授权界面已弹出，但方法
/// 回复被后端拖住。此时会话、Response 流与固定截止时间必须已在持久状态里，
/// 下一条命令才能接续同一会话，而不是重新 CreateSession/Start。
#[tokio::test]
async fn start_method_reply_delayed_past_command_budget_keeps_session_resumable() {
    const LONG_COMMAND: Duration = Duration::from_millis(1_200);
    const LONG_SESSION: Duration = Duration::from_millis(4_000);

    let bus = acceptance::PrivateBus::start();
    let client = bus.connect().await;
    let (portal_side, eis_side) = std::os::unix::net::UnixStream::pair().expect("socketpair");
    let std_fd: std::os::fd::OwnedFd = portal_side.into();
    let portal = RemoteDesktopMock::new_with_delayed_start_reply(
        zbus::zvariant::OwnedFd::from(std_fd),
        true,
    );
    let _server = serve_remote_desktop(&bus, portal.clone()).await;
    let eis = FakeEis::start(eis_side);
    let libei = LibeiInput::from_bus_with_budgets(client, LONG_SESSION, LONG_COMMAND)
        .await
        .expect("probe the fixture portal");
    let combo = return_key();

    // (1) `Start` 方法应答被挂起：命令在自身预算内返回「待确认」，
    //     不是把会话连同局部变量一起丢掉。
    let started = Instant::now();
    let pending = libei
        .ensure_ready(Op::Key(&combo))
        .await
        .expect_err("delayed Start reply must leave authorization pending");
    assert!(
        matches!(pending, SetupFailure::Pending(_)),
        "got: {pending}"
    );
    assert!(
        started.elapsed() < LONG_COMMAND + Duration::from_millis(700),
        "first command must return inside its own budget: {:?}",
        started.elapsed()
    );
    assert_eq!(portal.calls().creates, 1);
    assert_eq!(portal.calls().starts, 1);
    assert_eq!(portal.held_starts(), 1, "authorization UI must stay up");

    // 后端终于回复了方法调用；第一条命令的等待早已结束，回复被丢弃。
    portal.release_start_reply();
    tokio::time::sleep(Duration::from_millis(100)).await;

    // (2) 下一条命令接续同一会话：不重新 CreateSession/Start。
    let resumed = libei
        .ensure_ready(Op::Key(&combo))
        .await
        .expect_err("still awaiting the user's confirmation");
    assert!(
        matches!(resumed, SetupFailure::Pending(_)),
        "got: {resumed}"
    );
    assert_eq!(
        portal.calls().creates,
        1,
        "delayed Start reply must not orphan the session"
    );
    assert_eq!(
        portal.calls().starts,
        1,
        "resume must not raise a second authorization dialog"
    );

    // (3) 用户确认后，同一条持久会话完成 EIS 建立与注入。
    assert_eq!(portal.release_start().await, 1, "one held Start response");
    libei
        .ensure_ready(Op::Key(&combo))
        .await
        .expect("late authorization must complete the resumed session");
    assert_eq!(portal.calls().creates, 1);
    assert_eq!(portal.calls().starts, 1);
    assert_eq!(portal.calls().connects, 1);
    assert!(eis.log().binds >= 1, "EI handshake must bind a device");
    libei.send_key(&combo).await.expect("inject after resume");
    assert!(
        eis.wait_for_keys(2, Duration::from_secs(2)),
        "EIS peer must receive the injected key events: {:?}",
        eis.log()
    );
}
