//! TC-rootd-08（AC-108，P2）：rootd 接口白名单、字段形状、错误语义与授权。
//!
//! - 场景：服务标识与版本对账、逐方法入参与返回形状、任务信号与状态一致、
//!   错误语义与授权
//! - 判据：接口只导出白名单方法；只读与可逆方法字段一致；未授权返回需授权；
//!   未知方法返回方法不存在；超时不留孤儿进程
//! - 执行：`cargo test -p agent-shell-acceptance --test TC-rootd-08`
//!
//! 判定面是真实 `agent_shell_rootd::dbus::RootdInterface` 在私有 bus 上生成的
//! 接口与真实 `dispatch` 调用：内省 XML 即生产方法面；JobStatus 未知 id 的
//! 成功载荷、sysctl 的授权失败映射、未知成员的 UnknownMethod 均取实际返回值。
//!
//! 环境限制：30s/60s 方法超时与孤儿进程回收需要真实等待并观测子进程，由
//! rootd 的 `dispatch_with_timeout_tracked` 单测与真机验收覆盖；本用例不制造
//! 真实超时，也不触碰宿主 system bus、polkit 或系统命令。

use std::collections::HashMap;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;

use acceptance::bus::claim;
use acceptance::{Fixture, PrivateBus};
use agent_shell_rootd::dbus::RootdInterface;
use agent_shell_rootd::whitelist_methods;
use agent_shell_rpc::{method, RpcErrorCode};
use serde_json::{json, Value};

const ROOTD_SERVICE: &str = "org.agentshell.Rootd";
const ROOTD_PATH: &str = "/org/agentshell/Rootd";
const ROOTD_INTERFACE: &str = "org.agentshell.Rootd";
const POLKIT_SERVICE: &str = "org.freedesktop.PolicyKit1";
const POLKIT_PATH: &str = "/org/freedesktop/PolicyKit1/Authority";

/// 可切换授权结果的 polkit mock。
#[derive(Clone)]
struct PolkitAuthority {
    allow: Arc<AtomicBool>,
}

#[zbus::interface(name = "org.freedesktop.PolicyKit1.Authority")]
impl PolkitAuthority {
    async fn check_authorization(
        &self,
        _subject: (String, HashMap<String, zbus::zvariant::OwnedValue>),
        _action_id: String,
        _details: HashMap<String, String>,
        _flags: u32,
        _cancellation_id: String,
    ) -> (bool, bool, HashMap<String, String>) {
        (self.allow.load(Ordering::SeqCst), false, HashMap::new())
    }
}

/// 抽取 `<tag name="...">` 形式的成员名（method/signal）。
fn member_names(xml: &str, tag: &str) -> Vec<String> {
    let needle = format!("<{tag} name=\"");
    let mut rest = xml;
    let mut names = Vec::new();
    while let Some(index) = rest.find(&needle) {
        rest = &rest[index + needle.len()..];
        let Some(end) = rest.find('"') else { break };
        names.push(rest[..end].to_string());
    }
    names
}

#[tokio::test]
async fn interface_whitelist_payloads_errors_and_authorization_match_the_contract() {
    let bus = PrivateBus::start();
    let allow = Arc::new(AtomicBool::new(true));
    let polkit_server = bus
        .serve(
            POLKIT_PATH,
            PolkitAuthority {
                allow: allow.clone(),
            },
        )
        .await;
    claim(&polkit_server, POLKIT_SERVICE).await;
    let rootd_server = bus.serve(ROOTD_PATH, RootdInterface::new()).await;
    claim(&rootd_server, ROOTD_SERVICE).await;

    let conn = bus.connect().await;
    // 1) 方法面：接口只导出白名单方法 + 两个任务信号。
    let introspectable = zbus::proxy::Proxy::new(
        &conn,
        ROOTD_SERVICE,
        ROOTD_PATH,
        "org.freedesktop.DBus.Introspectable",
    )
    .await
    .expect("introspectable proxy");
    let xml: String = introspectable
        .call("Introspect", &())
        .await
        .expect("Introspect must succeed");
    let open = format!("<interface name=\"{ROOTD_INTERFACE}\">");
    let block_start = xml
        .find(&open)
        .unwrap_or_else(|| panic!("introspection lacks {ROOTD_INTERFACE}: {xml}"))
        + open.len();
    let block_end = block_start
        + xml[block_start..]
            .find("</interface>")
            .expect("interface block end");
    let block = &xml[block_start..block_end];
    let mut methods = member_names(block, "method");
    methods.sort();
    let mut expected: Vec<String> = whitelist_methods().iter().map(|m| m.to_string()).collect();
    expected.sort();
    assert_eq!(
        methods, expected,
        "interface must export exactly the whitelist"
    );
    let mut signals = member_names(block, "signal");
    signals.sort();
    assert_eq!(
        signals,
        ["JobDone", "JobProgress"],
        "progress/done signals live on the same interface"
    );
    for test_only in ["TestSlowMethod", "TestSpawnSleep"] {
        assert!(
            !methods.iter().any(|m| m == test_only),
            "test-only method {test_only} must not be exported"
        );
    }

    // 2) 未知方法：方法不存在（不是参数错误或静默成功）。
    let rootd = zbus::proxy::Proxy::new(&conn, ROOTD_SERVICE, ROOTD_PATH, ROOTD_INTERFACE)
        .await
        .expect("rootd proxy");
    let error = rootd
        .call::<_, _, ()>("TestSlowMethod", &())
        .await
        .expect_err("unknown member must fail");
    assert!(
        matches!(&error, zbus::Error::MethodError(name, _, _) if name.as_str().ends_with("UnknownMethod")),
        "unknown member must map to UnknownMethod, got: {error:?}"
    );

    let mut fixture =
        Fixture::start_with_env(&[("DBUS_SYSTEM_BUS_ADDRESS", bus.address().to_string())]).await;

    // 3) 未知 job：合法结果 `{"found": false}`，字段形状完整。
    let status: Value = fixture
        .call_ok(
            method::JOB_STATUS,
            Some(json!({"job_id": "job-does-not-exist"})),
        )
        .await;
    assert_eq!(status, json!({"found": false}));

    // 4) 未授权：sysctl.get 映射为 1007 并带 action id；拒绝是明确错误。
    allow.store(false, Ordering::SeqCst);
    let denied = fixture
        .call_err(method::SYSCTL_GET, Some(json!({"key": "kernel.hostname"})))
        .await;
    assert_eq!(denied.code, RpcErrorCode::AuthenticationRequired as i32);
    assert!(
        denied.message.contains("authentication required")
            && denied.message.contains("com.agentshell.sysctl.get"),
        "authorization failure must be user-readable and name the action: {}",
        denied.message
    );

    // 5) 校验失败：项目错误码与 rootd 原文保留，不伪造成功。
    allow.store(true, Ordering::SeqCst);
    let invalid = fixture
        .call_err(method::SYSCTL_GET, Some(json!({"key": "KERNEL"})))
        .await;
    assert_eq!(invalid.code, RpcErrorCode::BackendError as i32);
    assert!(
        invalid.message.contains("rootd:") && invalid.message.contains("KERNEL"),
        "rootd validation text and offending value must be preserved: {}",
        invalid.message
    );
}
