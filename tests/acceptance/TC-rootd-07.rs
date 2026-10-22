//! TC-rootd-07（AC-86，P2）：无授权明确拒绝与授权后生效。
//!
//! - 场景：① polkit 拒绝 `com.agentshell.sysctl.get`；② 默认调用方 `"*"`（L2）
//!   调用 L3 方法；③ 经 `security.grant` 授权 `trusted` L4 后 polkit 放行
//! - 判据：① 拒绝为非零错误（1007 AuthenticationRequired），文案含
//!   "authentication required" 与 action id，面向用户可理解；② 未授权时在门禁
//!   短路（1006），不触达 rootd/polkit；③ 授权后行为与已授权路径一致
//! - 执行：`cargo test -p agent-shell-acceptance --test TC-rootd-07`
//!
//! 判定面是真实 `dispatch` → `RootdConnector` → 私有 bus 上的
//! `agent_shell_rootd::dbus::RootdInterface`，polkit 由同 bus 上的
//! `org.freedesktop.PolicyKit1.Authority` mock 提供（`CheckAuthorization` 返回
//! 可切换的授权位）。`sysctl.get` 的生产实现直接读 `/proc/sys/<key>`（不 spawn
//! 命令），故其成功判据取真实内核值；`sysctl.set` 走 `Command::new("sysctl")`，
//! 由 PATH 上的 stub 承载并回读参数。
//!
//! 环境限制：不触碰宿主 system bus 与真实 polkit；不写任何内核参数（set 只由
//! stub 接收）。`kernel.hostname` 为只读可读键，读取无副作用。

use acceptance::bus::claim;
use acceptance::{Fixture, PrivateBus};
use agent_shell_daemon::rootd_client::{connector, RootdConnector, RootdProxy};
use agent_shell_rootd::dbus::RootdInterface;
use agent_shell_rpc::{method, RpcErrorCode};
use serde_json::json;
use std::collections::HashMap;
use std::os::unix::fs::PermissionsExt;
use std::path::Path;
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use std::sync::Arc;

const POLKIT_SERVICE: &str = "org.freedesktop.PolicyKit1";
const POLKIT_PATH: &str = "/org/freedesktop/PolicyKit1/Authority";

/// 可切换授权结果的 polkit mock，并记录调用次数。
#[derive(Clone)]
struct PolkitAuthority {
    allow: Arc<AtomicBool>,
    calls: Arc<AtomicUsize>,
}

#[zbus::interface(name = "org.freedesktop.PolicyKit1.Authority")]
impl PolkitAuthority {
    /// `CheckAuthorization((sa{sv}) s a{ss} u s) -> (bba{ss})`。
    ///
    /// rootd 只在 `is_authorized=true` 时放行；拒绝以 D-Bus AuthFailed 返回。
    async fn check_authorization(
        &self,
        _subject: (String, HashMap<String, zbus::zvariant::OwnedValue>),
        action_id: String,
        _details: HashMap<String, String>,
        _flags: u32,
        _cancellation_id: String,
    ) -> (bool, bool, HashMap<String, String>) {
        self.calls.fetch_add(1, Ordering::SeqCst);
        let allow = self.allow.load(Ordering::SeqCst);
        eprintln!("TC-rootd-07 polkit: action={action_id} allow={allow}");
        (allow, false, HashMap::new())
    }
}

/// 权限门禁若在 rootd 连接之前短路，此计数必须保持为零。
static CONNECTOR_CALLS: AtomicUsize = AtomicUsize::new(0);

fn counting_offline_connector() -> RootdConnector {
    || {
        CONNECTOR_CALLS.fetch_add(1, Ordering::SeqCst);
        Box::pin(async { Option::<RootdProxy<'static>>::None })
    }
}

/// 在临时目录写入记录参数的 stub（busybox `/bin/sh`）。
fn write_stub(dir: &Path, name: &str, log: &Path) -> std::path::PathBuf {
    let path = dir.join(name);
    std::fs::write(
        &path,
        format!(
            "#!/bin/sh\nprintf '%s\\n' \"$*\" >> '{}'\nexit 0\n",
            log.display()
        ),
    )
    .expect("write stub");
    std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o755)).expect("chmod stub");
    path
}

#[tokio::test]
async fn polkit_denial_surfaces_authentication_required_and_gate_precedes_rootd() {
    let bus = PrivateBus::start();
    let allow = Arc::new(AtomicBool::new(false));
    let calls = Arc::new(AtomicUsize::new(0));
    let polkit = PolkitAuthority {
        allow: allow.clone(),
        calls: calls.clone(),
    };
    let polkit_server = bus.serve(POLKIT_PATH, polkit).await;
    claim(&polkit_server, POLKIT_SERVICE).await;
    let rootd_server = bus
        .serve("/org/agentshell/Rootd", RootdInterface::new())
        .await;
    claim(&rootd_server, "org.agentshell.Rootd").await;

    let stub_dir = tempfile::tempdir().expect("stub temp");
    let sysctl_log = stub_dir.path().join("sysctl.log");
    write_stub(stub_dir.path(), "sysctl", &sysctl_log);
    let path = format!(
        "{}:{}",
        stub_dir.path().display(),
        std::env::var("PATH").unwrap_or_default()
    );

    let mut fixture = Fixture::start_with_env(&[
        ("DBUS_SYSTEM_BUS_ADDRESS", bus.address().to_string()),
        ("PATH", path),
    ])
    .await;

    // ① polkit 拒绝：rootd 以 AuthFailed 报错，daemon 映射为 1007。
    let denied = fixture
        .call_err(method::SYSCTL_GET, Some(json!({"key": "kernel.hostname"})))
        .await;
    assert_eq!(
        denied.code,
        RpcErrorCode::AuthenticationRequired as i32,
        "polkit 拒绝必须映射为 1007，实际 {}: {}",
        denied.code,
        denied.message
    );
    assert!(
        denied.message.contains("authentication required"),
        "授权失败文案必须可读，实际：{}",
        denied.message
    );
    assert!(
        denied.message.contains("com.agentshell.sysctl.get"),
        "授权失败必须带出 action id，实际：{}",
        denied.message
    );
    assert!(
        calls.load(Ordering::SeqCst) >= 1,
        "polkit mock 必须实际被调用"
    );

    // ② L3 方法在默认 "*"（L2）下必须先经权限门禁短路，不接触 rootd/polkit。
    fixture.daemon_mut().rootd_connect = counting_offline_connector();
    CONNECTOR_CALLS.store(0, Ordering::SeqCst);
    let polkit_before = calls.load(Ordering::SeqCst);
    let gated = fixture
        .call_err(
            method::SYSCTL_SET,
            Some(json!({"key": "kernel.sysrq", "value": "1"})),
        )
        .await;
    assert_eq!(
        gated.code,
        RpcErrorCode::ConfirmationRequired as i32,
        "L3 方法对 L2 调用方必须返回 1006，实际 {}: {}",
        gated.code,
        gated.message
    );
    assert_eq!(
        CONNECTOR_CALLS.load(Ordering::SeqCst),
        0,
        "权限门禁必须在 rootd 连接之前短路"
    );
    assert_eq!(
        calls.load(Ordering::SeqCst),
        polkit_before,
        "权限门禁短路时不得调用 polkit"
    );

    // ③ 授权 L4 并放行 polkit：sysctl.get / sysctl.set 走真实 rootd 路径。
    fixture
        .call_ok(
            method::SECURITY_GRANT,
            Some(json!({"agent_id": "trusted", "level": "L4"})),
        )
        .await;
    fixture.set_caller_id("trusted");
    fixture.daemon_mut().rootd_connect = connector();
    allow.store(true, Ordering::SeqCst);

    let value = fixture
        .call_ok(method::SYSCTL_GET, Some(json!({"key": "kernel.hostname"})))
        .await;
    let expected =
        std::fs::read_to_string("/proc/sys/kernel/hostname").expect("读 /proc/sys/kernel/hostname");
    assert_eq!(
        value,
        json!(expected.trim()),
        "sysctl.get 必须返回真实内核值"
    );

    let set = fixture
        .call_ok(
            method::SYSCTL_SET,
            Some(json!({"key": "kernel.sysrq", "value": "1"})),
        )
        .await;
    assert_eq!(set.get("accepted"), Some(&json!(true)), "{set}");
    assert_eq!(set.get("key"), Some(&json!("kernel.sysrq")), "{set}");
    let logged = std::fs::read_to_string(&sysctl_log).expect("sysctl stub 必须被调用");
    assert!(
        logged.contains("-w kernel.sysrq=1"),
        "sysctl stub 必须收到 `-w <key>=<value>`，实际：{logged:?}"
    );
    assert!(
        calls.load(Ordering::SeqCst) > polkit_before,
        "放行分支必须再次经 polkit 校验"
    );
}
