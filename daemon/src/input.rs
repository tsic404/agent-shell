//! daemon 侧输入注入：经 components/input 降级链执行（§12）。
//!
//! daemon 持有 `InputComponentHandle`（libei → ydotool → XTest → xdotool），
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
            parse_at(payload.get("at")),
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

/// 可选点击坐标 `[x, y]`；非二元组视为未指定（与 CLI `--at` 契约一致）。
fn parse_at(at: Option<&Value>) -> Option<(i32, i32)> {
    let arr = at?.as_array()?;
    if arr.len() != 2 {
        return None;
    }
    Some((
        arr[0].as_i64().unwrap_or(0) as i32,
        arr[1].as_i64().unwrap_or(0) as i32,
    ))
}

/// "ctrl+c" / "meta+t" 组合键解析（与 CLI 层同语法；daemon 侧独立实现以
/// 保持 CLI 零组件依赖——解析规则由 rpc crate 测试锚定）。
fn parse_combo(spec: &str) -> Result<KeyCombo, (RpcErrorCode, String)> {
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
        assert_eq!(parse_at(Some(&at)), Some((10, 20)));
        // 非二元组 → 视为未指定，不移动。
        assert_eq!(parse_at(Some(&serde_json::json!([1]))), None);
        assert_eq!(parse_at(None), None);
    }

    #[test]
    fn click_unknown_button_is_invalid_params() {
        let err = mouse_button("middle-click").unwrap_err();
        assert_eq!(err.0, agent_shell_rpc::RpcErrorCode::InvalidParams);
    }
}
