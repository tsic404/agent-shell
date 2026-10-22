//! TC-rootd-01（AC-80，P2）：系统服务状态查询与重启无害服务。
//!
//! - 场景：经特权链路重启一个无害的临时单元，并用系统侧通道读回其状态
//! - 判据：状态与系统工具一致；重启后读回为活动
//! - 执行：`cargo test -p agent-shell-acceptance --test TC-rootd-01`
//!
//! 判定面是真实 `dispatch` → `RootdConnector` → 私有 system bus 上的
//! `agent_shell_rootd::dbus::RootdInterface`；systemd Manager 由同 bus 上的
//! mock 承载（`RestartUnit` 记录参数并把单元状态置为 active），polkit 恒放行。
//! 测试用**同一个 systemd mock 的状态读回**充当「系统工具」查询，不触碰宿主
//! systemd、不重启任何真实服务。
//!
//! 环境限制：真实 systemd 单元重启与真实 `systemctl is-active` 属真机验收；
//! 本用例验证 rootd 的转发、权限与状态可读回语义。

use std::collections::HashMap;
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};

use acceptance::bus::claim;
use acceptance::{Fixture, PrivateBus};
use agent_shell_rootd::dbus::RootdInterface;
use agent_shell_rpc::{method, RpcErrorCode};
use serde_json::json;

const ROOTD_SERVICE: &str = "org.agentshell.Rootd";
const ROOTD_PATH: &str = "/org/agentshell/Rootd";
const POLKIT_SERVICE: &str = "org.freedesktop.PolicyKit1";
const POLKIT_PATH: &str = "/org/freedesktop/PolicyKit1/Authority";
const SYSTEMD_SERVICE: &str = "org.freedesktop.systemd1";
const SYSTEMD_PATH: &str = "/org/freedesktop/systemd1";

/// 恒放行的 polkit mock。
struct AllowAllPolkit;

#[zbus::interface(name = "org.freedesktop.PolicyKit1.Authority")]
impl AllowAllPolkit {
    async fn check_authorization(
        &self,
        _subject: (String, HashMap<String, zbus::zvariant::OwnedValue>),
        _action_id: String,
        _details: HashMap<String, String>,
        _flags: u32,
        _cancellation_id: String,
    ) -> (bool, bool, HashMap<String, String>) {
        (true, false, HashMap::new())
    }
}

/// systemd Manager mock：记录调用并维护单元活动态。
#[derive(Clone)]
struct MockSystemd {
    active: Arc<AtomicBool>,
    calls: Arc<Mutex<Vec<String>>>,
    invocations: Arc<AtomicUsize>,
}

#[zbus::interface(name = "org.freedesktop.systemd1.Manager")]
impl MockSystemd {
    fn restart_unit(
        &self,
        name: String,
        mode: String,
    ) -> zbus::fdo::Result<zbus::zvariant::OwnedObjectPath> {
        self.invocations.fetch_add(1, Ordering::SeqCst);
        self.calls
            .lock()
            .expect("systemd call log")
            .push(format!("RestartUnit:{name}:{mode}"));
        self.active.store(true, Ordering::SeqCst);
        zbus::zvariant::OwnedObjectPath::try_from("/org/freedesktop/systemd1/job/1")
            .map_err(|e| zbus::fdo::Error::Failed(format!("job path: {e}")))
    }

    /// 「系统工具」读回：返回单元活动态。
    fn acceptance_active_state(&self, _unit: String) -> zbus::fdo::Result<String> {
        Ok(if self.active.load(Ordering::SeqCst) {
            "active".into()
        } else {
            "inactive".into()
        })
    }
}

#[tokio::test]
async fn service_restart_is_forwarded_and_status_reads_back_active() {
    let bus = PrivateBus::start();
    let polkit_server = bus.serve(POLKIT_PATH, AllowAllPolkit).await;
    claim(&polkit_server, POLKIT_SERVICE).await;
    let systemd = MockSystemd {
        active: Arc::new(AtomicBool::new(false)),
        calls: Arc::new(Mutex::new(Vec::new())),
        invocations: Arc::new(AtomicUsize::new(0)),
    };
    let systemd_server = bus.serve(SYSTEMD_PATH, systemd.clone()).await;
    claim(&systemd_server, SYSTEMD_SERVICE).await;
    let rootd_server = bus.serve(ROOTD_PATH, RootdInterface::new()).await;
    claim(&rootd_server, ROOTD_SERVICE).await;

    let mut fixture =
        Fixture::start_with_env(&[("DBUS_SYSTEM_BUS_ADDRESS", bus.address().to_string())]).await;
    fixture
        .call_ok(
            method::SECURITY_GRANT,
            Some(json!({"agent_id": "trusted", "level": "L4"})),
        )
        .await;
    fixture.set_caller_id("trusted");

    let unit = "agent-shell-acceptance.service";
    let result = fixture
        .call_ok(
            method::SERVICE_CONTROL,
            Some(json!({"action": "restart", "unit": unit})),
        )
        .await;
    assert_eq!(result.get("accepted"), Some(&json!(true)), "{result}");
    assert_eq!(result.get("action"), Some(&json!("restart")), "{result}");
    assert_eq!(result.get("unit"), Some(&json!(unit)), "{result}");

    // rootd 必须把重启请求转发到 systemd Manager（单元与 mode 原样）。
    assert_eq!(
        systemd.calls.lock().expect("systemd call log").as_slice(),
        &[format!("RestartUnit:{unit}:replace")]
    );

    // 状态读回：与「系统工具」一致（mock 维护的活动态）。
    let conn = bus.connect().await;
    let proxy = zbus::proxy::Proxy::new(
        &conn,
        SYSTEMD_SERVICE,
        SYSTEMD_PATH,
        "org.freedesktop.systemd1.Manager",
    )
    .await
    .expect("systemd manager proxy");
    let state: String = proxy
        .call("AcceptanceActiveState", &(unit.to_string(),))
        .await
        .expect("status readback");
    assert_eq!(state, "active", "restarted unit must read back active");

    // 未知 action 在 daemon 层被拒绝，不触达 rootd/systemd。
    let before = systemd.invocations.load(Ordering::SeqCst);
    let error = fixture
        .call_err(
            method::SERVICE_CONTROL,
            Some(json!({"action": "status", "unit": unit})),
        )
        .await;
    assert_eq!(error.code, RpcErrorCode::InvalidParams as i32);
    assert_eq!(systemd.invocations.load(Ordering::SeqCst), before);
}
