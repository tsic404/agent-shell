//! TC-rootd-03（AC-82，P2）：内核参数查询、临时设置与还原。
//!
//! - 场景：读取内核参数、临时设置新值、读回确认后还原原值
//! - 判据：读回一致；测试结束必须还原原值（未还原即判流程违规）
//! - 执行：`cargo test -p agent-shell-acceptance --test TC-rootd-03`
//!
//! 判定面是真实 `dispatch` → `RootdConnector` → 私有 system bus 上的 rootd
//! 契约对端（mock 维护参数值）。测试记录 get/set 序列，断言设置后读回一致、
//! 最后一步把原值写回。真实 `/proc/sys` 写入是可逆但不该在 CI 触碰的主机变更，
//! 由一次性可回滚环境验收；本用例不写宿主内核参数。
//!
//! 环境限制：mock 对端承载 rootd 的 D-Bus 入出参；rootd 的真实 sysctl 读写由
//! 组件单测与真机验收覆盖。

use std::sync::{Arc, Mutex};

use acceptance::bus::claim;
use acceptance::{Fixture, PrivateBus};
use agent_shell_rpc::{method, RpcErrorCode};
use serde_json::json;
use zbus::zvariant::Value;

const ROOTD_SERVICE: &str = "org.agentshell.Rootd";
const ROOTD_PATH: &str = "/org/agentshell/Rootd";

/// 维护单个内核参数值的 rootd 对端。
#[derive(Clone)]
struct MockRootd {
    value: Arc<Mutex<String>>,
    calls: Arc<Mutex<Vec<String>>>,
}

#[zbus::interface(name = "org.agentshell.Rootd")]
impl MockRootd {
    fn sysctl_get(&self, key: String) -> zbus::fdo::Result<String> {
        self.calls
            .lock()
            .expect("rootd call log")
            .push(format!("SysctlGet:{key}"));
        Ok(self.value.lock().expect("sysctl value").clone())
    }

    fn sysctl_set(&self, key: String, value: Value<'_>) -> zbus::fdo::Result<()> {
        let text = match &value {
            Value::Str(s) => s.as_str().to_string(),
            Value::U8(v) => v.to_string(),
            Value::U16(v) => v.to_string(),
            Value::U32(v) => v.to_string(),
            Value::U64(v) => v.to_string(),
            Value::I32(v) => v.to_string(),
            Value::I64(v) => v.to_string(),
            Value::Bool(v) => v.to_string(),
            other => other.to_string(),
        };
        self.calls
            .lock()
            .expect("rootd call log")
            .push(format!("SysctlSet:{key}={text}"));
        *self.value.lock().expect("sysctl value") = text;
        Ok(())
    }
}

#[tokio::test]
async fn sysctl_set_is_read_back_and_original_value_is_restored() {
    let bus = PrivateBus::start();
    let mock = MockRootd {
        value: Arc::new(Mutex::new("1".into())),
        calls: Arc::new(Mutex::new(Vec::new())),
    };
    let server = bus.serve(ROOTD_PATH, mock.clone()).await;
    claim(&server, ROOTD_SERVICE).await;

    let mut fixture =
        Fixture::start_with_env(&[("DBUS_SYSTEM_BUS_ADDRESS", bus.address().to_string())]).await;
    fixture
        .call_ok(
            method::SECURITY_GRANT,
            Some(json!({"agent_id": "trusted", "level": "L4"})),
        )
        .await;
    fixture.set_caller_id("trusted");

    let key = "kernel.sysrq";
    // 1) 原值。
    let original: String = fixture
        .call_ok(method::SYSCTL_GET, Some(json!({"key": key})))
        .await
        .as_str()
        .expect("sysctl value string")
        .to_string();
    assert_eq!(original, "1");

    // 2) 临时设置并读回一致。
    fixture
        .call_ok(method::SYSCTL_SET, Some(json!({"key": key, "value": "0"})))
        .await;
    let changed: String = fixture
        .call_ok(method::SYSCTL_GET, Some(json!({"key": key})))
        .await
        .as_str()
        .expect("sysctl value string")
        .to_string();
    assert_eq!(changed, "0", "set value must read back consistently");

    // 3) 还原原值并确认。
    fixture
        .call_ok(
            method::SYSCTL_SET,
            Some(json!({"key": key, "value": original})),
        )
        .await;
    let restored: String = fixture
        .call_ok(method::SYSCTL_GET, Some(json!({"key": key})))
        .await
        .as_str()
        .expect("sysctl value string")
        .to_string();
    assert_eq!(restored, original, "the original value must be restored");

    // 参数顺序与还原动作可核对。
    assert_eq!(
        mock.calls.lock().expect("rootd call log").as_slice(),
        &[
            format!("SysctlGet:{key}"),
            format!("SysctlSet:{key}=0"),
            format!("SysctlGet:{key}"),
            format!("SysctlSet:{key}=1"),
            format!("SysctlGet:{key}"),
        ]
    );

    // 缺失 value 在 daemon 层拒绝，不触达 rootd。
    let before = mock.calls.lock().expect("rootd call log").len();
    let error = fixture
        .call_err(method::SYSCTL_SET, Some(json!({"key": key})))
        .await;
    assert_eq!(error.code, RpcErrorCode::InvalidParams as i32);
    assert_eq!(mock.calls.lock().expect("rootd call log").len(), before);
}
