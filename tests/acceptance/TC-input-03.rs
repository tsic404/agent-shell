//! TC-input-03（AC-32，P0）：坐标点击、指定按键与默认位置点击。
//!
//! - 场景：`input click --at X,Y` / `--button` / 不带 `--at`
//! - 判据：指针到位并触发点击；默认位置行为一致
//! - 执行：`cargo test -p agent-shell-acceptance --test TC-input-03`
//!
//! 判定面是真实 daemon 的 `input.send`（真实 dispatch → 真实 dispatcher → 候选链），
//! 候选链由夹具给出：链形复刻实测的 KDE Wayland 未绑定原生会话——无 ABS 轴的
//! ydotool（active）+ 有桌面几何的 uinput 直写。据此断言：`--at` 按绝对坐标能力位
//! 二次筛链（移动落到位、点击留给 active）、能力缺口不摘除候选、整链无该能力时
//! 错误点名能力并携带原因、坏 `--at` 载荷不被当作「未指定坐标」（CLI 的
//! `at: null` 形态与超出 32 位像素域的坐标各有用例锚定）。
//!
//! 环境变量是进程级状态（夹具的 `headless_session`），本目标保持单个 `#[tokio::test]`。

use std::sync::{Arc, Mutex};

use acceptance::Fixture;
use agent_shell_core::error::Result;
use agent_shell_core::types::{DesktopEnvironment, KeyCombo, MouseButton};
use agent_shell_input::{InputService, Op, SetupFailure};
use agent_shell_rpc::{method, RpcErrorCode};
use async_trait::async_trait;
use serde_json::{json, Value};

/// 记录调用的假后端。
///
/// `abs_capable == false` 复刻「ydotoold 虚拟设备未声明 ABS 轴」的 ydotool：
/// 绝对移动静默无效，故按能力缺口声明；`true` 复刻拿到桌面几何的 uinput 直写。
struct RecordingBackend {
    name: &'static str,
    abs_capable: bool,
    calls: Arc<Mutex<Vec<String>>>,
}

impl RecordingBackend {
    fn boxed(
        name: &'static str,
        abs_capable: bool,
        calls: &Arc<Mutex<Vec<String>>>,
    ) -> Box<dyn InputService> {
        Box::new(Self {
            name,
            abs_capable,
            calls: Arc::clone(calls),
        })
    }

    fn record(&self, call: impl std::fmt::Display) {
        self.calls
            .lock()
            .expect("call log lock")
            .push(format!("{}: {call}", self.name));
    }
}

#[async_trait]
impl InputService for RecordingBackend {
    fn name(&self) -> &'static str {
        self.name
    }

    async fn is_available(&self) -> bool {
        true
    }

    fn supports(&self, op: Op<'_>) -> bool {
        self.abs_capable || !matches!(op, Op::Move(..))
    }

    async fn ensure_ready(&self, _op: Op<'_>) -> Result<(), SetupFailure> {
        Ok(())
    }

    async fn send_key(&self, _combo: &KeyCombo) -> Result<()> {
        self.record("key");
        Ok(())
    }

    async fn type_text(&self, text: &str, _delay_ms: u32) -> Result<()> {
        self.record(format!("type {text}"));
        Ok(())
    }

    async fn mouse_move(&self, x: i32, y: i32) -> Result<()> {
        self.record(format!("move {x},{y}"));
        Ok(())
    }

    async fn mouse_click(&self, button: MouseButton) -> Result<()> {
        self.record(format!("click {button:?}"));
        Ok(())
    }

    async fn mouse_scroll(&self, dx: i32, dy: i32) -> Result<()> {
        self.record(format!("scroll {dx},{dy}"));
        Ok(())
    }
}

/// 点击载荷：与 CLI 实发形态同形——`--at` 缺省时 `at` 键仍在、值为 `null`
/// （`cli/src/main.rs` 的 `json!({"button": button, "at": at_v})`）。
fn click_payload(button: &str, at: Option<Value>) -> Value {
    json!({"kind": "click", "payload": {"button": button, "at": at}})
}

fn recorded(calls: &Arc<Mutex<Vec<String>>>) -> Vec<String> {
    calls.lock().expect("call log lock").clone()
}

fn reset(calls: &Arc<Mutex<Vec<String>>>) {
    calls.lock().expect("call log lock").clear();
}

#[tokio::test]
async fn coordinate_click_routes_by_absolute_pointer_capability() {
    let mut fixture = Fixture::start().await;

    // ── 场景 1：坐标点击（KDE Wayland 无原生注入通道的候选链）──
    let calls = Arc::new(Mutex::new(Vec::new()));
    fixture
        .set_input_chain(
            DesktopEnvironment::KDE,
            vec![
                RecordingBackend::boxed("ydotool", false, &calls),
                RecordingBackend::boxed("uinput", true, &calls),
            ],
        )
        .await;
    assert_eq!(
        fixture.input_backend_name(),
        Some("ydotool"),
        "构造期 active = 链上首个可用候选（与实测会话「选中 ydotool」一致）"
    );

    let result = fixture
        .call_ok(
            method::INPUT_SEND,
            Some(click_payload("left", Some(json!([100, 100])))),
        )
        .await;
    assert_eq!(
        recorded(&calls),
        ["uinput: move 100,100", "ydotool: click Left"],
        "坐标点击：绝对移动必须落在有绝对坐标能力的候选上，点击由 active 候选完成"
    );
    assert_eq!(
        result["backend"], "ydotool",
        "回传的注入后端是 active 候选（本次操作未推进降级链）"
    );
    assert_eq!(
        fixture.input_backend_name(),
        Some("ydotool"),
        "能力缺口只跳过本次操作，不得摘除候选"
    );

    // ── 场景 2：指定按键 + 坐标（`--button right --at X,Y`）──
    reset(&calls);
    fixture
        .call_ok(
            method::INPUT_SEND,
            Some(click_payload("right", Some(json!([400, 300])))),
        )
        .await;
    assert_eq!(
        recorded(&calls),
        ["uinput: move 400,300", "ydotool: click Right"],
        "指定按键与坐标一并生效：先移动到位，再按指定键"
    );

    // ── 场景 3：不带 `--at` → 当前位置点击，不发生移动 ──
    reset(&calls);
    fixture
        .call_ok(method::INPUT_SEND, Some(click_payload("middle", None)))
        .await;
    assert_eq!(
        recorded(&calls),
        ["ydotool: click Middle"],
        "默认位置行为一致：CLI 实发 `at: null` 时不得移动指针（也不得报参数错误）"
    );

    // 缺 `at` 键的载荷同样等价（非 CLI 客户端形态），仍须是当前位置点击。
    reset(&calls);
    fixture
        .call_ok(
            method::INPUT_SEND,
            Some(json!({"kind": "click", "payload": {"button": "middle"}})),
        )
        .await;
    assert_eq!(
        recorded(&calls),
        ["ydotool: click Middle"],
        "缺键与 null 同义：两种「未指定坐标」形态都不得移动指针"
    );

    // ── 场景 4：整链无绝对坐标能力 → 可归因的能力错误，且不得静默注入 ──
    let no_abs = Arc::new(Mutex::new(Vec::new()));
    fixture
        .set_input_chain(
            DesktopEnvironment::KDE,
            vec![
                RecordingBackend::boxed("ydotool", false, &no_abs),
                RecordingBackend::boxed("uinput", false, &no_abs),
            ],
        )
        .await;
    let error = fixture
        .call_err(
            method::INPUT_SEND,
            Some(click_payload("left", Some(json!([100, 100])))),
        )
        .await;
    assert_eq!(
        error.code,
        RpcErrorCode::BackendUnavailable as i32,
        "能力缺失是后端不可用错误，不是参数错误（C-043）"
    );
    assert!(
        error.message.contains("absolute-pointer"),
        "错误必须点名缺的能力（绝对坐标），而非泛化的「不支持该操作」：{}",
        error.message
    );
    assert!(
        error.message.contains("2 candidate"),
        "错误必须说明缺口来源（几个候选为该操作声明能力缺口）：{}",
        error.message
    );
    assert!(
        recorded(&no_abs).is_empty(),
        "无绝对坐标能力时不得注入任何事件"
    );
    assert_eq!(
        fixture.input_backend_name(),
        Some("ydotool"),
        "能力缺口不得摘除候选（HID 之外的按键/无坐标点击仍须可用该后端）"
    );

    // 同一会话下不带 `--at` 的点击仍可用：两类错误不得混同（C-043）。
    fixture
        .call_ok(method::INPUT_SEND, Some(click_payload("left", None)))
        .await;
    assert_eq!(recorded(&no_abs), ["ydotool: click Left"]);

    // ── 场景 5：坏 `--at` 载荷 → 参数错误，且不得退化为「当前位置点击」──
    reset(&no_abs);
    for bad in [
        json!([100]),
        json!(["100", 100]),
        json!("100,100"),
        // i64 域内但超 i32 像素域：截断会把坐标回绕到用户未指定的位置（rc=0）。
        json!([4294967396_i64, 100]),
        json!([3000000000_i64, 0]),
        json!([0, -3000000000_i64]),
    ] {
        let error = fixture
            .call_err(
                method::INPUT_SEND,
                Some(click_payload("left", Some(bad.clone()))),
            )
            .await;
        assert_eq!(
            error.code,
            RpcErrorCode::InvalidParams as i32,
            "坏 --at 载荷必须以参数错误拒绝：{bad}"
        );
        assert!(
            error.message.contains("payload.at"),
            "错误必须指向载荷字段：{}",
            error.message
        );
    }
    assert!(
        recorded(&no_abs).is_empty(),
        "坏载荷不得触发任何注入（否则即为「看起来成功、实际点错位置」）"
    );
}
