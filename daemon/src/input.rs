//! daemon 侧输入注入：经 components/input 降级链执行（§12）。
//!
//! daemon 持有 `InputComponentHandle`（libei → ydotool → uinput → XTest → xdotool），
//! CLI 的 `input.send` 在此解析参数后委托 active 后端。参数校验先于后端
//! 探测——坏载荷必须返回 InvalidParams（-32602），而非被后端不可用（1002）
//! 掩盖（CI 无显示服务器环境回归锚定）。

use agent_shell_core::error::AgentShellError;
use agent_shell_core::types::{Key, KeyCombo, KeyName, ModifierMask, MouseButton};
use agent_shell_input::InputDispatcher;
use agent_shell_rpc::{InputKind, RpcErrorCode};
use serde_json::Value;

/// 参数解析与校验（纯逻辑，不触后端）。
pub(crate) enum PreparedOp {
    Key(KeyCombo),
    TypeText(String),
    Click(MouseButton, Option<(i32, i32)>),
    Scroll(i32, i32),
}

/// 解析并校验输入操作载荷，返回待执行操作。坏载荷返回 InvalidParams。
pub(crate) fn prepare(
    kind: InputKind,
    payload: &Value,
) -> Result<PreparedOp, (RpcErrorCode, String)> {
    Ok(match kind {
        InputKind::Key => {
            let spec = str_field(payload, "combo")?;
            PreparedOp::Key(parse_combo(spec)?)
        }
        InputKind::TypeText => PreparedOp::TypeText(str_field(payload, "text")?.to_string()),
        InputKind::Click => PreparedOp::Click(
            mouse_button(
                payload
                    .get("button")
                    .and_then(Value::as_str)
                    .unwrap_or("left"),
            )?,
            parse_at(payload.get("at"))?,
        ),
        InputKind::Scroll => PreparedOp::Scroll(
            payload.get("dx").and_then(Value::as_i64).unwrap_or(0) as i32,
            payload.get("dy").and_then(Value::as_i64).unwrap_or(0) as i32,
        ),
    })
}

/// 经 active 后端执行一条已校验的输入操作。
pub(crate) async fn execute(
    dispatcher: &InputDispatcher,
    op: PreparedOp,
) -> Result<(), (RpcErrorCode, String)> {
    let r = match op {
        PreparedOp::Key(combo) => dispatcher.send_key(&combo).await,
        PreparedOp::TypeText(text) => dispatcher.type_text(&text, 0).await,
        PreparedOp::Click(button, at) => {
            if let Some((x, y)) = at {
                dispatcher.mouse_move(x, y).await.map_err(map_input_err)?;
            }
            dispatcher.mouse_click(button).await
        }
        PreparedOp::Scroll(dx, dy) => dispatcher.mouse_scroll(dx, dy).await,
    };
    r.map_err(map_input_err)
}

fn str_field<'a>(v: &'a Value, key: &str) -> Result<&'a str, (RpcErrorCode, String)> {
    v.get(key).and_then(Value::as_str).ok_or((
        RpcErrorCode::InvalidParams,
        format!("payload.{key} required"),
    ))
}

/// 可选点击坐标 `[x, y]`：`at` 缺键或为 `null` → `None`（未指定坐标）；其余
/// 非整数二元组、超出 32 位像素域的坐标 → `InvalidParams`。
///
/// 「参数语义不满足」与「未指定坐标」是两种不同事实：把坏载荷当作未指定会静默
/// 点击**当前位置**（rc=0、用户以为点到了目标坐标），而 CLI 层对同一载荷是硬
/// 拒绝——两侧口径必须一致，坏参数不得降级成另一种成功语义。CLI 的点击载荷恒带
/// `at` 键（无 `--at` 时序列化为 `null`），故 `null` 与缺键同义。
fn parse_at(at: Option<&Value>) -> Result<Option<(i32, i32)>, (RpcErrorCode, String)> {
    let Some(at) = at.filter(|v| !v.is_null()) else {
        return Ok(None);
    };
    let arr = at.as_array().ok_or_else(|| bad_at(at))?;
    if arr.len() != 2 {
        return Err(bad_at(at));
    }
    let (Some(x), Some(y)) = (arr[0].as_i64(), arr[1].as_i64()) else {
        return Err(bad_at(at));
    };
    // i64 → i32 必须受检：截断把超范围坐标回绕到用户未指定的位置（rc=0，本仓
    // 禁止的静默错误），与 i32 内越界的夹取口径也不一致。
    match (i32::try_from(x), i32::try_from(y)) {
        (Ok(x), Ok(y)) => Ok(Some((x, y))),
        _ => Err(at_out_of_range(x, y)),
    }
}

/// 坏 `at` 载荷的统一拒绝（含 CLI 同款写法提示）。
fn bad_at(at: &Value) -> (RpcErrorCode, String) {
    (
        RpcErrorCode::InvalidParams,
        format!("payload.at must be [x, y] integers (e.g. [100, 200]); got {at}"),
    )
}

/// 坐标超出 32 位像素域的拒绝：CLI 按 i64 解析，这里是最后一道范围校验。
fn at_out_of_range(x: i64, y: i64) -> (RpcErrorCode, String) {
    (
        RpcErrorCode::InvalidParams,
        format!("payload.at out of range: [{x}, {y}] must fit in 32-bit pixels"),
    )
}

/// "ctrl+c" / "meta+t" 组合键解析（与 CLI 层同语法；daemon 侧独立实现以
/// 保持 CLI 零组件依赖——解析规则由 rpc crate 测试锚定）。
pub(crate) fn parse_combo(spec: &str) -> Result<KeyCombo, (RpcErrorCode, String)> {
    let mut modifiers = ModifierMask::default();
    let mut keys = Vec::new();
    for part in spec.split('+').filter(|p| !p.is_empty()) {
        match part.to_ascii_lowercase().as_str() {
            "ctrl" | "control" => modifiers.ctrl = true,
            "alt" => modifiers.alt = true,
            "shift" => modifiers.shift = true,
            "meta" | "super" | "win" => modifiers.meta = true,
            other => {
                let key = if other.chars().count() == 1 {
                    Key::Char(other.chars().next().expect("count == 1"))
                } else {
                    let named = match other {
                        "return" | "enter" => KeyName::Return,
                        "escape" | "esc" => KeyName::Escape,
                        "backspace" => KeyName::BackSpace,
                        "tab" => KeyName::Tab,
                        "space" => KeyName::Space,
                        "left" => KeyName::Left,
                        "right" => KeyName::Right,
                        "up" => KeyName::Up,
                        "down" => KeyName::Down,
                        "home" => KeyName::Home,
                        "end" => KeyName::End,
                        "pageup" => KeyName::PageUp,
                        "pagedown" => KeyName::PageDown,
                        "insert" => KeyName::Insert,
                        "delete" => KeyName::Delete,
                        "menu" => KeyName::Menu,
                        "f1" => KeyName::F1,
                        "f2" => KeyName::F2,
                        "f3" => KeyName::F3,
                        "f4" => KeyName::F4,
                        "f5" => KeyName::F5,
                        "f6" => KeyName::F6,
                        "f7" => KeyName::F7,
                        "f8" => KeyName::F8,
                        "f9" => KeyName::F9,
                        "f10" => KeyName::F10,
                        "f11" => KeyName::F11,
                        "f12" => KeyName::F12,
                        _ => {
                            return Err((
                                RpcErrorCode::InvalidParams,
                                format!("unknown key name: {other}"),
                            ))
                        }
                    };
                    Key::Named(named)
                };
                keys.push(key);
            }
        }
    }
    if keys.is_empty() {
        return Err((
            RpcErrorCode::InvalidParams,
            format!("no non-modifier key in combo: {spec}"),
        ));
    }
    Ok(KeyCombo { keys, modifiers })
}

fn mouse_button(b: &str) -> Result<MouseButton, (RpcErrorCode, String)> {
    match b {
        "left" => Ok(MouseButton::Left),
        "middle" => Ok(MouseButton::Middle),
        "right" => Ok(MouseButton::Right),
        "back" => Ok(MouseButton::Back),
        "forward" => Ok(MouseButton::Forward),
        _ => Err((RpcErrorCode::InvalidParams, format!("unknown button {b:?}"))),
    }
}

/// `AgentShellError` → RPC 错误码：后端不可用/权限不足独立成码，其余归 BackendError。
fn map_input_err(e: AgentShellError) -> (RpcErrorCode, String) {
    match e {
        AgentShellError::BackendUnavailable(msg) => (RpcErrorCode::BackendUnavailable, msg),
        AgentShellError::Permission(msg) => (RpcErrorCode::Denied, msg),
        other => (RpcErrorCode::BackendError, other.to_string()),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use agent_shell_core::types::KeyName::*;

    #[test]
    fn parse_combo_covers_full_key_table() {
        // F1..F12 + 全命名键（daemon 侧解析与 CLI 侧同语法）。
        let cases = [
            ("f1", F1),
            ("f2", F2),
            ("f3", F3),
            ("f12", F12),
            ("return", Return),
            ("enter", Return),
            ("esc", Escape),
            ("delete", Delete),
            ("menu", Menu),
            ("space", Space),
        ];
        for (name, expected) in cases {
            let c = parse_combo(name).unwrap_or_else(|e| panic!("{name}: {e:?}"));
            assert_eq!(c.keys.len(), 1);
            assert!(
                matches!(&c.keys[0], Key::Named(k) if k == &expected),
                "{name}"
            );
        }
    }

    #[test]
    fn parse_combo_modifiers_and_rejects() {
        let c = parse_combo("ctrl+alt+t").expect("parse");
        assert!(c.modifiers.ctrl && c.modifiers.alt && !c.modifiers.meta);
        // 仅修饰键 → 报错。
        assert!(parse_combo("ctrl").is_err());
        // 未知键名 → 报错（InvalidParams）。
        let err = parse_combo("nosuchkey").unwrap_err();
        assert_eq!(err.0, agent_shell_rpc::RpcErrorCode::InvalidParams);
    }

    #[test]
    fn click_at_parses_two_element_array_only() {
        let at = serde_json::json!([10, 20]);
        assert_eq!(parse_at(Some(&at)), Ok(Some((10, 20))));
        assert_eq!(parse_at(None), Ok(None), "无 at 即「在当前位置点击」");
        // CLI 无 `--at` 时实发 `"at": null`（`cli/src/main.rs` 的载荷构造）：
        // 只认缺键会让默认位置点击在真实入口上变成参数错误。
        assert_eq!(
            parse_at(Some(&serde_json::Value::Null)),
            Ok(None),
            "at: null 与缺键同义（CLI 实发形态）"
        );
        // 坏载荷是参数错误，不得退化为「未指定坐标」（那会静默点当前位置）。
        for bad in [
            serde_json::json!([1]),
            serde_json::json!([1, 2, 3]),
            serde_json::json!("100,200"),
            serde_json::json!(["100", 200]),
            serde_json::json!([100, null]),
            // i64 域内但超 i32 像素域：截断会回绕到用户未指定的坐标。
            serde_json::json!([4294967396_i64, 100]),
            serde_json::json!([3000000000_i64, 0]),
            serde_json::json!([0, -3000000000_i64]),
            serde_json::json!([i64::from(i32::MAX) + 1, 0]),
        ] {
            let err = parse_at(Some(&bad)).expect_err(&format!("{bad} must be rejected"));
            assert_eq!(err.0, agent_shell_rpc::RpcErrorCode::InvalidParams);
            assert!(err.1.contains("payload.at"), "{err:?}");
        }
        // 可表示的边界值仍须接受（越出桌面的夹取由后端负责，不在参数层拒绝）。
        assert_eq!(
            parse_at(Some(&serde_json::json!([i32::MIN, i32::MAX]))),
            Ok(Some((i32::MIN, i32::MAX)))
        );
        assert_eq!(
            parse_at(Some(&serde_json::json!([-1, 0]))),
            Ok(Some((-1, 0)))
        );
    }

    #[test]
    fn click_unknown_button_is_invalid_params() {
        let err = mouse_button("middle-click").unwrap_err();
        assert_eq!(err.0, agent_shell_rpc::RpcErrorCode::InvalidParams);
    }
}
