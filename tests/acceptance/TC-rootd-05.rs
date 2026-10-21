//! TC-rootd-05（AC-84，P2）：Mount/Unmount 参数校验与命令参数透传。
//!
//! - 场景：非法 fstype、相对/穿越 target，以及合法 mount/unmount 参数
//! - 判据：非法参数在 rootd 校验层被拒绝，D-Bus 错误保留原始取值，且不调用
//!   `mount`/`umount`；合法参数经 PATH stub 执行，并收到与 §23.4.3 一致的命令行
//!   参数（`-t <fstype> [-o <opts>] <device> <target>` / `<target>`）
//! - 执行：`cargo test -p agent-shell-acceptance --test TC-rootd-05`
//!
//! 判定面是真实 `dispatch` → `RootdConnector` → 私有 bus 上的
//! `agent_shell_rootd::dbus::RootdInterface`。rootd 的 `mount_bin` 经 `which` 按
//! PATH 解析可执行文件，故用临时目录中的 stub 承载命令并回读参数。
//!
//! 环境限制：绝不执行真实挂载——所有命令由 stub 接收后直接退出；非法参数分支
//! 在 spawn 之前完成校验，stub 日志因此必须不存在。不连接宿主 system bus。

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

/// 恒放行的 polkit mock（Mount/Unmount 受 `com.agentshell.mount` 授权约束）。
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
async fn mount_and_unmount_validate_arguments_before_invoking_commands() {
    let bus = PrivateBus::start();
    let polkit_server = bus.serve(POLKIT_PATH, AllowAllPolkit).await;
    claim(&polkit_server, POLKIT_SERVICE).await;
    let rootd_server = bus
        .serve("/org/agentshell/Rootd", RootdInterface::new())
        .await;
    claim(&rootd_server, "org.agentshell.Rootd").await;

    let stub_dir = tempfile::tempdir().expect("stub temp");
    let mount_log = stub_dir.path().join("mount.log");
    let umount_log = stub_dir.path().join("umount.log");
    write_stub(stub_dir.path(), "mount", &mount_log);
    write_stub(stub_dir.path(), "umount", &umount_log);
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

    // 非法 fstype：错误文案保留原始取值，且不得执行 mount。
    let illegal_fstype = "ext4;rm -rf /";
    let error = fixture
        .call_err(
            method::MOUNT,
            Some(json!({
                "device": "/dev/sda1",
                "target": "/mnt/data",
                "fstype": illegal_fstype
            })),
        )
        .await;
    assert_eq!(
        error.code,
        RpcErrorCode::BackendError as i32,
        "{}",
        error.message
    );
    assert!(
        error.message.contains("illegal character in fstype"),
        "必须给出 fstype 校验错误，实际：{}",
        error.message
    );
    assert!(
        error.message.contains(illegal_fstype),
        "错误文案必须保留原始 fstype 取值，实际：{}",
        error.message
    );
    assert!(!mount_log.exists(), "校验失败时不得调用 mount stub");

    // 相对 target：必须拒绝并保留原始取值。
    let error = fixture
        .call_err(
            method::MOUNT,
            Some(json!({
                "device": "/dev/sda1",
                "target": "mnt/data",
                "fstype": "ext4"
            })),
        )
        .await;
    assert_eq!(
        error.code,
        RpcErrorCode::BackendError as i32,
        "{}",
        error.message
    );
    assert!(
        error.message.contains("target path must be absolute")
            && error.message.contains("mnt/data"),
        "错误文案必须说明绝对路径要求并保留原值，实际：{}",
        error.message
    );
    assert!(!mount_log.exists(), "校验失败时不得调用 mount stub");

    // 穿越 target：拒绝且保留原值。
    let traversal = "/mnt/data/../etc";
    let error = fixture
        .call_err(
            method::MOUNT,
            Some(json!({
                "device": "/dev/sda1",
                "target": traversal,
                "fstype": "ext4"
            })),
        )
        .await;
    assert!(
        error.message.contains("'..' traversal") && error.message.contains(traversal),
        "错误文案必须指出穿越并保留原值，实际：{}",
        error.message
    );
    assert!(!mount_log.exists(), "校验失败时不得调用 mount stub");

    // 合法 mount：命令经 stub 执行并收到完整参数。
    let mounted = fixture
        .call_ok(
            method::MOUNT,
            Some(json!({
                "device": "/dev/sda1",
                "target": "/mnt/data",
                "fstype": "ext4",
                "options": ["rw", "noatime"]
            })),
        )
        .await;
    assert_eq!(mounted.get("accepted"), Some(&json!(true)), "{mounted}");
    assert_eq!(
        mounted.get("device"),
        Some(&json!("/dev/sda1")),
        "{mounted}"
    );
    assert_eq!(
        mounted.get("target"),
        Some(&json!("/mnt/data")),
        "{mounted}"
    );
    assert_eq!(mounted.get("fstype"), Some(&json!("ext4")), "{mounted}");
    assert_eq!(
        mounted.get("options"),
        Some(&json!(["rw", "noatime"])),
        "{mounted}"
    );
    let logged = std::fs::read_to_string(&mount_log).expect("mount stub 必须被调用");
    assert!(
        logged.contains("-t ext4 -o rw,noatime /dev/sda1 /mnt/data"),
        "mount stub 参数必须与契约一致，实际：{logged:?}"
    );

    // 非法 unmount target：拒绝且不执行 umount。
    let error = fixture
        .call_err(method::UNMOUNT, Some(json!({"target": "mnt/data"})))
        .await;
    assert!(
        error.message.contains("target path must be absolute")
            && error.message.contains("mnt/data"),
        "unmount 必须复用 target 校验，实际：{}",
        error.message
    );
    assert!(!umount_log.exists(), "校验失败时不得调用 umount stub");

    // 合法 unmount：stub 收到 target。
    let unmounted = fixture
        .call_ok(method::UNMOUNT, Some(json!({"target": "/mnt/data"})))
        .await;
    assert_eq!(unmounted.get("accepted"), Some(&json!(true)), "{unmounted}");
    assert_eq!(
        unmounted.get("target"),
        Some(&json!("/mnt/data")),
        "{unmounted}"
    );
    let logged = std::fs::read_to_string(&umount_log).expect("umount stub 必须被调用");
    assert!(
        logged.trim() == "/mnt/data",
        "umount stub 必须只收到 target，实际：{logged:?}"
    );
}
