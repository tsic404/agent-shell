//! TC-rootd-04（AC-83，P2）：软件包安装（可回滚环境）与无授权拒绝。
//!
//! - 场景：L4 调用方经 rootd 安装/刷新软件包；安装任务的 job.status 查询；
//!   任务失败保留退出码；polkit 拒绝
//! - 判据：`package.install` 返回 `{job_id, pm}`；`job.status` 返回
//!   `{found, method, progress, done, success, exit_code, stderr}` 终态；非零
//!   退出的任务报 `success:false` 且保留退出码；授权拒绝不伪造任务
//! - 执行：`cargo test -p agent-shell-acceptance --test TC-rootd-04`
//!
//! 判定面是真实 `dispatch` → `RootdConnector` → 私有 system bus 上的 rootd
//! 代理对端。真实软件包安装是设计明确的环境受限项（「仅在可回滚环境执行」），
//! 本用例以 mock 对端承载 rootd 的 D-Bus 契约：daemon 侧的参数透传、任务载荷
//! 解析、状态查询与错误映射全部走生产路径；rootd 的包管理器执行、真实安装与
//! 收尾卸载由一次性可回滚环境验收，不在 CI 执行（无沙箱即记阻塞，本用例不
//! 触碰宿主包管理器）。

use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};

use acceptance::bus::claim;
use acceptance::{Fixture, PrivateBus};
use agent_shell_rpc::{method, RpcErrorCode};
use serde_json::{json, Value};

const ROOTD_SERVICE: &str = "org.agentshell.Rootd";
const ROOTD_PATH: &str = "/org/agentshell/Rootd";

/// 可切换授权/失败行为的 rootd 对端。
#[derive(Clone)]
struct MockRootd {
    allow: Arc<AtomicBool>,
    calls: Arc<Mutex<Vec<String>>>,
}

impl MockRootd {
    fn record(&self, call: String) {
        self.calls.lock().expect("rootd call log").push(call);
    }
}

#[zbus::interface(name = "org.agentshell.Rootd")]
impl MockRootd {
    async fn package_install(&self, packages: Vec<String>) -> zbus::fdo::Result<String> {
        self.record(format!("PackageInstall:{packages:?}"));
        if !self.allow.load(Ordering::SeqCst) {
            return Err(zbus::fdo::Error::AuthFailed(
                "polkit denied action: com.agentshell.pkexec.install-package".into(),
            ));
        }
        let job = if packages.iter().any(|package| package == "fail-me") {
            "job-fail"
        } else {
            "job-ok"
        };
        Ok(serde_json::to_string(&json!({"job_id": job, "pm": "Apt"})).expect("package payload"))
    }

    async fn package_refresh(&self) -> zbus::fdo::Result<String> {
        self.record("PackageRefresh".into());
        if !self.allow.load(Ordering::SeqCst) {
            return Err(zbus::fdo::Error::AuthFailed(
                "polkit denied action: com.agentshell.package.refresh".into(),
            ));
        }
        Ok(
            serde_json::to_string(&json!({"job_id": "job-refresh", "pm": "Apt"}))
                .expect("package payload"),
        )
    }

    async fn job_status(&self, job_id: String) -> zbus::fdo::Result<String> {
        self.record(format!("JobStatus:{job_id}"));
        let payload = match job_id.as_str() {
            "job-ok" => json!({
                "found": true,
                "method": "PackageInstall",
                "progress": 1.0,
                "done": true,
                "success": true,
                "exit_code": 0,
                "stderr": "",
            }),
            "job-fail" => json!({
                "found": true,
                "method": "PackageInstall",
                "progress": 1.0,
                "done": true,
                "success": false,
                "exit_code": 7,
                "stderr": "E: simulated failure",
            }),
            _ => json!({"found": false}),
        };
        Ok(serde_json::to_string(&payload).expect("job payload"))
    }
}

#[tokio::test]
async fn package_jobs_and_authorization_follow_rootd_contract() {
    let bus = PrivateBus::start();
    let allow = Arc::new(AtomicBool::new(true));
    let mock = MockRootd {
        allow: allow.clone(),
        calls: Arc::new(Mutex::new(Vec::new())),
    };
    let server = bus.serve(ROOTD_PATH, mock.clone()).await;
    claim(&server, ROOTD_SERVICE).await;

    // 默认调用方 L2：L3 的 package.install 在门禁处要求确认，不触达 rootd。
    let mut unprivileged =
        Fixture::start_with_env(&[("DBUS_SYSTEM_BUS_ADDRESS", bus.address().to_string())]).await;
    let error = unprivileged
        .call_err(
            method::PACKAGE_INSTALL,
            Some(json!({"packages": ["nginx"]})),
        )
        .await;
    assert_eq!(error.code, RpcErrorCode::ConfirmationRequired as i32);
    assert!(
        mock.calls.lock().expect("rootd call log").is_empty(),
        "gate must stop the call before rootd"
    );

    // L4 授权：安装任务载荷与终态查询。
    let mut fixture =
        Fixture::start_with_env(&[("DBUS_SYSTEM_BUS_ADDRESS", bus.address().to_string())]).await;
    fixture
        .call_ok(
            method::SECURITY_GRANT,
            Some(json!({"agent_id": "trusted", "level": "L4"})),
        )
        .await;
    fixture.set_caller_id("trusted");

    let installed: Value = fixture
        .call_ok(
            method::PACKAGE_INSTALL,
            Some(json!({"packages": ["nginx", "curl"]})),
        )
        .await;
    assert_eq!(installed.get("job_id"), Some(&json!("job-ok")));
    assert_eq!(installed.get("pm"), Some(&json!("Apt")));
    assert!(
        mock.calls
            .lock()
            .expect("rootd call log")
            .iter()
            .any(|call| call == "PackageInstall:[\"nginx\", \"curl\"]"),
        "daemon must forward the package list verbatim"
    );

    let status: Value = fixture
        .call_ok(method::JOB_STATUS, Some(json!({"job_id": "job-ok"})))
        .await;
    assert_eq!(status.get("found"), Some(&json!(true)));
    assert_eq!(status.get("method"), Some(&json!("PackageInstall")));
    assert_eq!(status.get("progress"), Some(&json!(1.0)));
    assert_eq!(status.get("done"), Some(&json!(true)));
    assert_eq!(status.get("success"), Some(&json!(true)));
    assert_eq!(status.get("exit_code"), Some(&json!(0)));
    assert!(status.get("stderr").is_some());

    // 失败任务：success=false 且保留退出码。
    let failing: Value = fixture
        .call_ok(
            method::PACKAGE_INSTALL,
            Some(json!({"packages": ["fail-me"]})),
        )
        .await;
    let failing_id = failing
        .get("job_id")
        .and_then(Value::as_str)
        .expect("job id");
    let failing_status: Value = fixture
        .call_ok(method::JOB_STATUS, Some(json!({"job_id": failing_id})))
        .await;
    assert_eq!(failing_status.get("success"), Some(&json!(false)));
    assert_eq!(failing_status.get("exit_code"), Some(&json!(7)));
    assert_eq!(
        failing_status.get("stderr"),
        Some(&json!("E: simulated failure"))
    );

    // 刷新任务载荷。
    let refreshed: Value = fixture.call_ok(method::PACKAGE_REFRESH, None).await;
    assert_eq!(refreshed.get("job_id"), Some(&json!("job-refresh")));
    assert_eq!(refreshed.get("pm"), Some(&json!("Apt")));

    // 未知任务：合法结果（未找到），不是错误。
    let unknown: Value = fixture
        .call_ok(method::JOB_STATUS, Some(json!({"job_id": "no-such"})))
        .await;
    assert_eq!(unknown, json!({"found": false}));

    // 授权拒绝：错误透传；不伪造任务。
    allow.store(false, Ordering::SeqCst);
    let denied = fixture
        .call_err(
            method::PACKAGE_INSTALL,
            Some(json!({"packages": ["nginx"]})),
        )
        .await;
    assert_eq!(denied.code, RpcErrorCode::BackendError as i32);
    assert!(
        denied.message.contains("AuthFailed")
            && denied
                .message
                .contains("com.agentshell.pkexec.install-package"),
        "authorization failure must be preserved: {}",
        denied.message
    );
    assert!(
        denied.message.contains("rootd:"),
        "daemon must preserve the rootd error context: {}",
        denied.message
    );
}
