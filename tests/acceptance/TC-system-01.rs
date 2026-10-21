//! TC-system-01（AC-70，P2）：屏幕亮度查询 / 设置 / 回读与越界拒绝。
//!
//! - 场景：`brightness.get` 裸数组形状；`brightness.set` 对 0/50/100 的放行与
//!   回读；非法值（负数、浮点、>100、字符串、缺参）先于后端拒绝；后端缺席与
//!   策略拒绝的错误码。
//! - 判据：查询结果每项含 `monitor`/`brightness`/`max_brightness`/`adaptive`
//!   四键；`brightness.set` 成功回执恰为 `{"status":"ok","value":N}` 且后端收到
//!   同一值；非法参数恒 `-32602` 且后端零调用；`brightness: None` → `1002`；
//!   `permissions.deny` 命中 → `1004` 且后端不被调用。
//! - 执行：`cargo test -p agent-shell-acceptance --test TC-system-01`
//!
//! 环境限制：验收机没有真实背光设备，亮度后端由状态化 fake 注入 daemon 的
//! 公开装配字段（`Daemon::brightness`）；请求仍走真实 `dispatch` 与权限门禁。
//! 真实 powerdevil/brightnessctl/sysfs 分类链由 TC-tech-06 端到端覆盖，此处不
//! 重复。

use std::sync::{Arc, Mutex};

use acceptance::Fixture;
use agent_shell_core::error::Result;
use agent_shell_core::services::BrightnessState;
use agent_shell_power::BrightnessOps;
use agent_shell_rpc::{method, RpcErrorCode};
use async_trait::async_trait;
use serde_json::{json, Value};

/// 状态化亮度 fake：`set` 记录调用并成为下一次 `get` 的当前值。
struct StatefulBrightness {
    current: Mutex<u8>,
    sets: Mutex<Vec<u8>>,
    gets: Mutex<usize>,
}

impl StatefulBrightness {
    fn new(current: u8) -> Self {
        Self {
            current: Mutex::new(current),
            sets: Mutex::new(Vec::new()),
            gets: Mutex::new(0),
        }
    }

    fn sets(&self) -> Vec<u8> {
        self.sets.lock().unwrap().clone()
    }

    fn gets(&self) -> usize {
        *self.gets.lock().unwrap()
    }
}

#[async_trait]
impl BrightnessOps for StatefulBrightness {
    async fn get(&self) -> Result<Vec<BrightnessState>> {
        *self.gets.lock().unwrap() += 1;
        Ok(vec![BrightnessState {
            monitor: "fake-panel".into(),
            brightness: *self.current.lock().unwrap(),
            max_brightness: 100,
            adaptive: false,
        }])
    }

    async fn set(&self, value: u8) -> Result<()> {
        self.sets.lock().unwrap().push(value);
        *self.current.lock().unwrap() = value;
        Ok(())
    }
}

fn array(value: &Value) -> &Vec<Value> {
    value
        .as_array()
        .unwrap_or_else(|| panic!("expected a bare JSON array, got: {value}"))
}

/// 对象键集合（升序，便于与协议字段逐一比对）。
fn keys(value: &Value) -> Vec<String> {
    let mut keys: Vec<String> = value
        .as_object()
        .unwrap_or_else(|| panic!("expected a JSON object, got: {value}"))
        .keys()
        .cloned()
        .collect();
    keys.sort();
    keys
}

fn own_state(value: &Value) -> &Value {
    array(value)
        .first()
        .unwrap_or_else(|| panic!("brightness.get must report the injected panel"))
}

#[tokio::test]
async fn brightness_query_set_readback_and_policy_deny() {
    let mut fixture = Fixture::start().await;
    let backend = Arc::new(StatefulBrightness::new(42));
    fixture.daemon_mut().brightness = Some(Arc::clone(&backend) as Arc<dyn BrightnessOps>);

    // 1. 查询面：裸数组 + 四字段，值来自后端。
    let states = fixture.call_ok(method::BRIGHTNESS_GET, None).await;
    assert_eq!(
        keys(own_state(&states)),
        vec![
            "adaptive".to_string(),
            "brightness".to_string(),
            "max_brightness".to_string(),
            "monitor".to_string()
        ],
        "brightness.get 每项必须恰好携带协议字段，got: {states}"
    );
    assert_eq!(own_state(&states)["monitor"], "fake-panel");
    assert_eq!(own_state(&states)["brightness"], 42);
    assert_eq!(own_state(&states)["max_brightness"], 100);
    assert_eq!(own_state(&states)["adaptive"], false);
    assert_eq!(backend.gets(), 1, "查询必须直达注入的后端");

    // 2. 设置面：0/50/100 放行，回执精确，后端收到同一值，回读生效。
    for value in [0u64, 50, 100] {
        let response = fixture
            .call_ok(method::BRIGHTNESS_SET, Some(json!({ "value": value })))
            .await;
        assert_eq!(
            response,
            json!({ "status": "ok", "value": value }),
            "brightness.set 回执必须恰为 status+value"
        );
        assert_eq!(
            keys(&response),
            vec!["status".to_string(), "value".to_string()]
        );

        let state = fixture.call_ok(method::BRIGHTNESS_GET, None).await;
        assert_eq!(
            own_state(&state)["brightness"],
            value,
            "set({value}) 后回读必须反映同一值"
        );
    }
    assert_eq!(backend.sets(), vec![0, 50, 100]);

    // 3. 参数校验先于后端：任何非法值都是 -32602，后端零新增调用。
    let calls_before = backend.sets().len();
    let gets_before = backend.gets();
    for params in [
        Some(json!({ "value": -1 })),
        Some(json!({ "value": 101 })),
        Some(json!({ "value": 50.5 })),
        Some(json!({ "value": "50" })),
        Some(json!({})),
        None,
    ] {
        let error = fixture
            .call_err(method::BRIGHTNESS_SET, params.clone())
            .await;
        assert_eq!(
            error.code,
            RpcErrorCode::InvalidParams as i32,
            "非法参数 {params:?} 必须返回 -32602，got: {error:?}"
        );
    }
    assert_eq!(
        backend.sets().len(),
        calls_before,
        "非法参数不得触达亮度后端"
    );
    assert_eq!(backend.gets(), gets_before, "非法参数不得触发后端查询");

    // 4. 无后端：查询与设置都以 BackendUnavailable 显式失败。
    fixture.daemon_mut().brightness = None;
    for (method_name, params) in [
        (method::BRIGHTNESS_GET, None),
        (method::BRIGHTNESS_SET, Some(json!({ "value": 50 }))),
    ] {
        let error = fixture.call_err(method_name, params.clone()).await;
        assert_eq!(
            error.code,
            RpcErrorCode::BackendUnavailable as i32,
            "无后端时 {method_name} 必须返回 1002，got: {error:?}"
        );
        assert!(
            !error.message.is_empty(),
            "{method_name} 的 BackendUnavailable 必须给出原因"
        );
    }

    // 5. 策略拒绝：deny 命中 brightness.set → 1004，且后端不被调用。
    fixture.daemon_mut().brightness = Some(Arc::clone(&backend) as Arc<dyn BrightnessOps>);
    fixture
        .daemon_mut()
        .security
        .config
        .permissions
        .deny
        .insert("brightness.set".into(), true);
    let calls_before = backend.sets().len();
    let error = fixture
        .call_err(method::BRIGHTNESS_SET, Some(json!({ "value": 50 })))
        .await;
    assert_eq!(
        error.code,
        RpcErrorCode::Denied as i32,
        "deny 命中的写操作必须返回 1004，got: {error:?}"
    );
    assert!(
        error.message.contains("denied"),
        "拒绝理由必须可归因，got: {:?}",
        error.message
    );
    assert_eq!(
        backend.sets().len(),
        calls_before,
        "策略拒绝不得触达亮度后端"
    );

    // 查询是 L0，同一条 deny 不应波及只读面。
    let state = fixture.call_ok(method::BRIGHTNESS_GET, None).await;
    assert_eq!(own_state(&state)["brightness"], 100);
}
