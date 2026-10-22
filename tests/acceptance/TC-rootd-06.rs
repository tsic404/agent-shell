//! TC-rootd-06（AC-85，P2）：HostnameSet 校验与 hostnamectl 参数透传。
//!
//! - 场景：非法主机名标签（下划线、空 label）与合法主机名写回
//! - 判据：非法主机名在 rootd `validate_hostname` 处被拒绝并保留原始取值，且不
//!   调用 `hostnamectl`；合法主机名经 PATH stub 执行并收到
//!   `set-hostname <hostname>`，daemon 回执 `accepted: true`
//! - 执行：`cargo test -p agent-shell-acceptance --test TC-rootd-06`
//!
//! 判定面是真实 `dispatch` → `RootdConnector` → 私有 bus 上的
//! `agent_shell_rootd::dbus::RootdInterface`。`HostnameSet` 经 rootd 的
//! hostname 写回执行器 spawn `hostnamectl`，PATH 上的 stub 承载该命令并回读参数。
//!
//! 环境限制（按设计豁免）：真实主机名写入是一次性、不可逆的宿主系统变更，验收
//! 环境不得执行；本用例只验证 rootd 的校验与命令参数契约，写回命令完全由 stub
//! 接收。不连接宿主 system bus。

use acceptance::bus::claim;
use acceptance::{Fixture, PrivateBus};
use agent_shell_rootd::dbus::RootdInterface;
use agent_shell_rpc::{method, RpcErrorCode};
use serde_json::json;
use std::collections::HashMap;
use std::os::unix::fs::PermissionsExt;
use std::path::Path;

const POLKIT_SERVICE: &str = "org.freedesktop.PolicyKit1";
const POLKIT_PATH: &str = "/org/freedesktop/PolicyKit1/Authority";

/// 恒放行的 polkit mock（HostnameSet 受 `com.agentshell.hostname.set` 授权约束）。
#[derive(Clone)]
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
async fn hostname_set_validates_labels_before_invoking_hostnamectl() {
    let bus = PrivateBus::start();
    let polkit_server = bus.serve(POLKIT_PATH, AllowAllPolkit).await;
    claim(&polkit_server, POLKIT_SERVICE).await;
    let rootd_server = bus
        .serve("/org/agentshell/Rootd", RootdInterface::new())
        .await;
    claim(&rootd_server, "org.agentshell.Rootd").await;

    let stub_dir = tempfile::tempdir().expect("stub temp");
    let hostnamectl_log = stub_dir.path().join("hostnamectl.log");
    write_stub(stub_dir.path(), "hostnamectl", &hostnamectl_log);
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
    fixture
        .call_ok(
            method::SECURITY_GRANT,
            Some(json!({"agent_id": "trusted", "level": "L4"})),
        )
        .await;
    fixture.set_caller_id("trusted");

    // 非法字符（下划线）：拒绝并保留原值，不执行 hostnamectl。
    let illegal = "bad_host";
    let error = fixture
        .call_err(method::HOSTNAME_SET, Some(json!({"hostname": illegal})))
        .await;
    assert_eq!(
        error.code,
        RpcErrorCode::BackendError as i32,
        "{}",
        error.message
    );
    assert!(
        error.message.contains("illegal character in hostname") && error.message.contains(illegal),
        "错误文案必须指出非法字符并保留原值，实际：{}",
        error.message
    );
    assert!(
        !hostnamectl_log.exists(),
        "校验失败时不得调用 hostnamectl stub"
    );

    // 空 label（连续点）：拒绝并保留原值。
    let empty_label = "a..b";
    let error = fixture
        .call_err(method::HOSTNAME_SET, Some(json!({"hostname": empty_label})))
        .await;
    assert!(
        error.message.contains("empty labels"),
        "错误文案必须指出空 label，实际：{}",
        error.message
    );
    assert!(
        !hostnamectl_log.exists(),
        "校验失败时不得调用 hostnamectl stub"
    );

    // 合法主机名：stub 收到 set-hostname 参数，daemon 回执 accepted。
    let hostname = "workstation-01";
    let result = fixture
        .call_ok(method::HOSTNAME_SET, Some(json!({"hostname": hostname})))
        .await;
    assert_eq!(result.get("accepted"), Some(&json!(true)), "{result}");
    assert_eq!(result.get("hostname"), Some(&json!(hostname)), "{result}");
    let logged = std::fs::read_to_string(&hostnamectl_log).expect("hostnamectl stub 必须被调用");
    assert!(
        logged.contains(&format!("set-hostname {hostname}")),
        "hostnamectl stub 必须收到 `set-hostname <hostname>`，实际：{logged:?}"
    );
}
