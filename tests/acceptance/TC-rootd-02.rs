//! TC-rootd-02（AC-81，P2）：系统日志过滤查询与无匹配。
//!
//! - 场景：按 unit/priority 过滤查询系统日志；查询一个无匹配的过滤条件
//! - 判据：结果符合过滤条件；无匹配时返回空结果并以零退出（RPC 成功）
//! - 执行：`cargo test -p agent-shell-acceptance --test TC-rootd-02`
//!
//! 判定面是真实 `dispatch` → `RootdConnector` → 私有 system bus 上的
//! `RootdInterface`；`journalctl` 由 PATH stub 承载（记录参数并返回固定 JSON
//! 流）。测试断言 rootd 把结构化过滤条件翻译成 journalctl 参数，且无匹配时
//! 空输出不升级为错误。不读取宿主 journal。
//!
//! 环境限制：真实 journal 内容与真实 `journalctl` 退出码属真机验收；本用例
//! 验证过滤参数的透传与空结果语义。

use std::collections::HashMap;
use std::os::unix::fs::PermissionsExt;
use std::path::Path;

use acceptance::bus::claim;
use acceptance::{Fixture, PrivateBus};
use agent_shell_rootd::dbus::RootdInterface;
use agent_shell_rpc::method;
use serde_json::{json, Value};

const ROOTD_SERVICE: &str = "org.agentshell.Rootd";
const ROOTD_PATH: &str = "/org/agentshell/Rootd";
const POLKIT_SERVICE: &str = "org.freedesktop.PolicyKit1";
const POLKIT_PATH: &str = "/org/freedesktop/PolicyKit1/Authority";

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

/// journalctl stub：记录参数；命中 nginx 时返回两行 JSON，否则空输出。
fn write_journal_stub(dir: &Path, log: &Path) -> std::path::PathBuf {
    let path = dir.join("journalctl");
    std::fs::write(
        &path,
        format!(
            r#"#!/bin/sh
printf '%s\n' "$*" >> '{log}'
case " $* " in
  *" --unit=nginx "*)
    echo '{{"__REALTIME_TIMESTAMP":"1","MESSAGE":"started"}}'
    echo '{{"__REALTIME_TIMESTAMP":"2","MESSAGE":"ready"}}'
    ;;
esac
exit 0
"#,
            log = log.display()
        ),
    )
    .expect("write journalctl stub");
    std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o755)).expect("chmod stub");
    path
}

/// daemon 的 `{"result": "<journalctl JSON 行流>"}`。
fn journal_output(result: &Value) -> String {
    result
        .get("result")
        .and_then(Value::as_str)
        .expect("result carries the journal JSON line stream")
        .to_string()
}

#[tokio::test]
async fn journal_filter_is_forwarded_and_no_match_is_an_empty_success() {
    let bus = PrivateBus::start();
    let polkit_server = bus.serve(POLKIT_PATH, AllowAllPolkit).await;
    claim(&polkit_server, POLKIT_SERVICE).await;
    let rootd_server = bus.serve(ROOTD_PATH, RootdInterface::new()).await;
    claim(&rootd_server, ROOTD_SERVICE).await;

    let stub_dir = tempfile::tempdir().expect("stub temp");
    let log = stub_dir.path().join("journalctl.log");
    write_journal_stub(stub_dir.path(), &log);
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

    // 命中过滤：结果行来自 journal 流，且 journalctl 参数逐项符合过滤条件。
    let result = fixture
        .call_ok(
            method::SYSTEM_LOG_VIEW,
            Some(json!({"filter": {"unit": "nginx", "priority": "err"}})),
        )
        .await;
    let output = journal_output(&result);
    assert!(
        output.contains("started") && output.contains("ready"),
        "matched journal lines must be returned: {output:?}"
    );
    let logged = std::fs::read_to_string(&log).expect("journalctl stub must be called");
    for expected in [
        "--output=json",
        "--unit=nginx",
        "--priority=err",
        "--lines=1000",
        "--since=-24h",
    ] {
        assert!(
            logged.contains(expected),
            "journalctl args must contain {expected:?}, got: {logged:?}"
        );
    }

    // 无匹配：空输出是成功结果（零退出），不是 NotFound。
    let empty = fixture
        .call_ok(
            method::SYSTEM_LOG_VIEW,
            Some(json!({"filter": {"unit": "missing.service"}})),
        )
        .await;
    assert_eq!(
        journal_output(&empty),
        "",
        "no-match filter must return an empty journal stream"
    );
}
