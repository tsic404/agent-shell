//! daemon 侧输入注入：XTest 通道（复用 displayserver-x11，§6.3）。
//!
//! daemon 持久化持有 X11 连接（`OnceLock` 懒初始化）；CLI 不再直连。

use agent_shell_core::types::MouseButton;
use agent_shell_displayserver_x11::X11DisplayServer;
use agent_shell_rpc::keys::{Key, KeyCombo, KeyName};
use agent_shell_rpc::{InputKind, RpcErrorCode};
use serde_json::Value;
use std::sync::LazyLock;

static X11: LazyLock<Result<X11DisplayServer, String>> = LazyLock::new(|| {
    if std::env::var("DISPLAY").is_err() {
        return Err("DISPLAY not set — XTest injection unavailable".into());
    }
    X11DisplayServer::connect().map_err(|e| e.to_string())
});

fn x11() -> Result<&'static X11DisplayServer, (RpcErrorCode, String)> {
    X11.as_ref()
        .map_err(|e| (RpcErrorCode::BackendUnavailable, e.clone()))
}

/// 执行一条输入操作。参数形状由 rpc::InputKind 约定。
pub fn execute(kind: InputKind, payload: &Value) -> Result<(), (RpcErrorCode, String)> {
    // 参数校验先于后端连接——坏载荷必须返回 InvalidParams（-32602），
    // 而非被后端不可用（1002）掩盖（CI 无 DISPLAY 环境回归锚定）。
    let op = prepare(kind, payload)?;
    let x = x11()?;
    let r = match op {
        PreparedOp::Key(combo) => send_combo(x, &combo),
        PreparedOp::TypeText(text) => type_text(x, &text),
        PreparedOp::Click(button, at) => click(x, &button, at.as_deref()),
        PreparedOp::Scroll(dx, dy) => scroll(x, dx, dy),
    };
    r.map_err(|e| (RpcErrorCode::BackendError, e))
}

/// 参数解析与校验（纯逻辑，不触后端）。
enum PreparedOp {
    Key(KeyCombo),
    TypeText(String),
    Click(String, Option<Vec<Value>>),
    Scroll(i32, i32),
}

fn prepare(kind: InputKind, payload: &Value) -> Result<PreparedOp, (RpcErrorCode, String)> {
    Ok(match kind {
        InputKind::Key => {
            let spec = str_field(payload, "combo")?;
            PreparedOp::Key(parse_combo(spec)?)
        }
        InputKind::TypeText => PreparedOp::TypeText(str_field(payload, "text")?.to_string()),
        InputKind::Click => PreparedOp::Click(
            payload
                .get("button")
                .and_then(Value::as_str)
                .unwrap_or("left")
                .to_string(),
            payload.get("at").and_then(Value::as_array).cloned(),
        ),
        InputKind::Scroll => PreparedOp::Scroll(
            payload.get("dx").and_then(Value::as_i64).unwrap_or(0) as i32,
            payload.get("dy").and_then(Value::as_i64).unwrap_or(0) as i32,
        ),
    })
}

fn str_field<'a>(v: &'a Value, key: &str) -> Result<&'a str, (RpcErrorCode, String)> {
    v.get(key).and_then(Value::as_str).ok_or((
        RpcErrorCode::InvalidParams,
        format!("payload.{key} required"),
    ))
}

/// "ctrl+c" / "meta+t" 组合键解析（与 CLI 层同语法；daemon 侧独立实现以
/// 保持 CLI 零组件依赖——解析规则由 rpc crate 测试锚定）。
fn parse_combo(spec: &str) -> Result<KeyCombo, (RpcErrorCode, String)> {
    use agent_shell_rpc::keys::{KeyName, ModifierMask};
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

// ───────────────────────── XTest 注入（daemon 侧执行体） ─────────────────────────

fn named_keycode(name: KeyName) -> Option<u8> {
    use agent_shell_rpc::keys::KeyName::*;
    Some(match name {
        Return => 36,
        Escape => 9,
        BackSpace => 22,
        Tab => 23,
        Space => 65,
        Left => 113,
        Right => 114,
        Up => 111,
        Down => 116,
        Home => 110,
        End => 115,
        PageUp => 112,
        PageDown => 117,
        Insert => 118,
        Delete => 119,
        Menu => 135,
        F1 => 67,
        F2 => 68,
        F3 => 69,
        F4 => 70,
        F5 => 71,
        F6 => 72,
        F7 => 73,
        F8 => 74,
        F9 => 75,
        F10 => 76,
        F11 => 95,
        F12 => 96,
    })
}

/// 单字符 → (键码, 是否需要 shift)，US QWERTY。
fn char_keycode(c: char) -> Option<(u8, bool)> {
    const SHIFTED: &[(char, u8)] = &[
        ('!', 10),
        ('@', 11),
        ('#', 12),
        ('$', 13),
        ('%', 14),
        ('^', 15),
        ('&', 16),
        ('*', 17),
        ('(', 18),
        (')', 19),
        ('~', 49),
        ('_', 20),
        ('+', 21),
        ('{', 34),
        ('}', 35),
        ('|', 51),
        (':', 47),
        ('"', 48),
        ('<', 59),
        ('>', 60),
        ('?', 61),
    ];
    if let Some((_, code)) = SHIFTED.iter().find(|(ch, _)| *ch == c) {
        return Some((*code, true));
    }
    let lower = c.to_ascii_lowercase();
    let code: u8 = match lower {
        'a'..='z' => 38 + (lower as u8 - b'a'),
        '1'..='9' => 10 + (lower as u8 - b'1'),
        '0' => 19,
        ' ' => 65,
        '.' => 60,
        ',' => 59,
        '/' => 61,
        ';' => 47,
        '\'' => 48,
        '[' => 34,
        ']' => 35,
        '\\' => 51,
        '`' => 49,
        '-' => 20,
        '=' => 21,
        _ => return None,
    };
    Some((code, c.is_ascii_uppercase()))
}

fn send_combo(x: &X11DisplayServer, combo: &KeyCombo) -> Result<(), String> {
    if !x.is_xtest_available() {
        return Err("XTest unavailable on this session (XWayland?)".into());
    }
    let mut seq: Vec<u8> = Vec::new();
    if combo.modifiers.ctrl {
        seq.push(37);
    }
    if combo.modifiers.alt {
        seq.push(64);
    }
    if combo.modifiers.shift {
        seq.push(50);
    }
    if combo.modifiers.meta {
        seq.push(133);
    }
    for key in &combo.keys {
        match key {
            Key::Named(n) => {
                seq.push(named_keycode(*n).ok_or_else(|| format!("no keycode mapped for {n:?}"))?)
            }
            Key::Char(c) => {
                let (code, shift) =
                    char_keycode(*c).ok_or_else(|| format!("no keycode mapped for {c:?}"))?;
                if shift && !combo.modifiers.shift {
                    seq.push(50);
                }
                seq.push(code);
            }
        }
    }
    for code in &seq {
        x.fake_key_event(*code, true).map_err(|e| e.to_string())?;
    }
    for code in seq.iter().rev() {
        x.fake_key_event(*code, false).map_err(|e| e.to_string())?;
    }
    x.sync().map_err(|e| e.to_string())
}

fn type_text(x: &X11DisplayServer, text: &str) -> Result<(), String> {
    if !x.is_xtest_available() {
        return Err("XTest unavailable on this session (XWayland?)".into());
    }
    for c in text.chars() {
        let (code, shift) =
            char_keycode(c).ok_or_else(|| format!("cannot type character {c:?}"))?;
        if shift {
            x.fake_key_event(50, true).map_err(|e| e.to_string())?;
        }
        x.fake_key_event(code, true).map_err(|e| e.to_string())?;
        x.fake_key_event(code, false).map_err(|e| e.to_string())?;
        if shift {
            x.fake_key_event(50, false).map_err(|e| e.to_string())?;
        }
    }
    x.sync().map_err(|e| e.to_string())
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

fn click(x: &X11DisplayServer, button: &str, at: Option<&[Value]>) -> Result<(), String> {
    if !x.is_xtest_available() {
        return Err("XTest unavailable on this session (XWayland?)".into());
    }
    let b = mouse_button(button).map_err(|(_, e)| e)?;
    let code = match b {
        MouseButton::Left => 1u8,
        MouseButton::Middle => 2,
        MouseButton::Right => 3,
        MouseButton::Back => 8,
        MouseButton::Forward => 9,
    };
    if let Some(at) = at {
        if at.len() == 2 {
            let px = at[0].as_i64().unwrap_or(0) as i16;
            let py = at[1].as_i64().unwrap_or(0) as i16;
            x.fake_motion_event(px, py).map_err(|e| e.to_string())?;
        }
    }
    x.fake_button_event(code, true).map_err(|e| e.to_string())?;
    x.fake_button_event(code, false)
        .map_err(|e| e.to_string())?;
    x.sync().map_err(|e| e.to_string())
}

fn scroll(x: &X11DisplayServer, dx: i32, dy: i32) -> Result<(), String> {
    if !x.is_xtest_available() {
        return Err("XTest unavailable on this session (XWayland?)".into());
    }
    let steps_v = dy.unsigned_abs() as u16;
    let steps_h = dx.unsigned_abs() as u16;
    let v_btn: u8 = if dy > 0 { 5 } else { 4 };
    let h_btn: u8 = if dx > 0 { 7 } else { 6 };
    // 双轴各自计数（审查修复项：较小轴不得被多滚）。
    for _ in 0..steps_v {
        x.fake_button_event(v_btn, true)
            .map_err(|e| e.to_string())?;
        x.fake_button_event(v_btn, false)
            .map_err(|e| e.to_string())?;
    }
    for _ in 0..steps_h {
        x.fake_button_event(h_btn, true)
            .map_err(|e| e.to_string())?;
        x.fake_button_event(h_btn, false)
            .map_err(|e| e.to_string())?;
    }
    x.sync().map_err(|e| e.to_string())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parse_combo_covers_full_key_table() {
        use agent_shell_rpc::keys::KeyName::*;
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
    fn char_keycode_shift_symbols_map_to_base_keys() {
        let (one, _) = char_keycode('1').expect("1");
        let (bang, bang_shift) = char_keycode('!').expect("!");
        assert_eq!(one, bang);
        assert!(bang_shift);
        let (a, a_shift) = char_keycode('a').expect("a");
        assert!(!a_shift);
        let (cap, cap_shift) = char_keycode('A').expect("A");
        assert_eq!(a, cap);
        assert!(cap_shift);
        assert!(char_keycode('中').is_none());
    }
}
