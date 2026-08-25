//! 键码映射：core 类型 → evdev 键码（linux/input-event-codes.h）。
//!
//! ydotool 与 libei/EIS 共用 evdev 空间；XTest 后端使用 Xorg 默认 keymap
//! （evdev+8），自行维护映射。US QWERTY 近似——完整 keysym/keycode 解析
//! 需要 xkbcommon，后续任务接入。

use agent_shell_core::types::{Key, KeyCombo, KeyName};

/// 命名键 → evdev 键码。
pub fn named_to_evdev(name: KeyName) -> Option<u32> {
    use KeyName::*;
    Some(match name {
        Return => 28,
        Escape => 1,
        BackSpace => 14,
        Tab => 15,
        Space => 57,
        Left => 105,
        Right => 106,
        Up => 103,
        Down => 108,
        Home => 102,
        End => 107,
        PageUp => 104,
        PageDown => 109,
        Insert => 110,
        Delete => 111,
        Menu => 139,
        F1 => 59,
        F2 => 60,
        F3 => 61,
        F4 => 62,
        F5 => 63,
        F6 => 64,
        F7 => 65,
        F8 => 66,
        F9 => 67,
        F10 => 68,
        F11 => 87,
        F12 => 88,
    })
}

/// 单字符 → (evdev 键码, 是否需要 shift)，US QWERTY。
pub fn char_to_evdev(original: char) -> Option<(u32, bool)> {
    // Shift 符号先折回其底键（US QWERTY），再查键码。
    let c = match original {
        '!' => '1',
        '@' => '2',
        '#' => '3',
        '$' => '4',
        '%' => '5',
        '^' => '6',
        '&' => '7',
        '*' => '8',
        '(' => '9',
        ')' => '0',
        '_' => '-',
        '+' => '=',
        '{' => '[',
        '}' => ']',
        '|' => '\\',
        ':' => ';',
        '"' => '\'',
        '<' => ',',
        '>' => '.',
        '?' => '/',
        '~' => '`',
        other => other,
    };
    let lower = c.to_ascii_lowercase();
    let code: u32 = match lower {
        'a'..='z' => 30 + (lower as u32 - b'a' as u32), // a=30..z=55
        '1'..='9' => 2 + (lower as u32 - u32::from(b'1')), // 1=2..9=10
        '0' => 11,
        ' ' => 57,
        '\n' | '\r' => 28,
        '\t' => 15,
        '.' => 52,
        ',' => 51,
        '/' => 53,
        ';' => 39,
        '\'' => 40,
        '[' => 26,
        ']' => 27,
        '\\' => 43,
        '`' => 41,
        '-' => 12,
        '=' => 13,
        _ => return None,
    };
    // shift 判断基于折叠前的原始字符。
    let shifted = original.is_ascii_uppercase()
        || matches!(
            original,
            '!' | '@'
                | '#'
                | '$'
                | '%'
                | '^'
                | '&'
                | '*'
                | '('
                | ')'
                | '_'
                | '+'
                | '{'
                | '}'
                | '|'
                | ':'
                | '"'
                | '<'
                | '>'
                | '?'
                | '~'
        );
    Some((code, shifted))
}

/// 组合键 → evdev press 序列（修饰键 → 实体键；含临时 shift 标记）。
///
/// 返回 `(keycode, is_temp_shift)` 列表；调用方负责逆序释放
/// （临时 shift 在其实体键之后弹起）。
pub fn combo_to_press_sequence(combo: &KeyCombo) -> Result<Vec<(u32, bool)>, String> {
    let mut seq: Vec<(u32, bool)> = Vec::new();
    // 左系修饰键码：ctrl=29 alt=56 shift=42 meta=125。
    if combo.modifiers.ctrl {
        seq.push((29, false));
    }
    if combo.modifiers.alt {
        seq.push((56, false));
    }
    if combo.modifiers.shift {
        seq.push((42, false));
    }
    if combo.modifiers.meta {
        seq.push((125, false));
    }
    for key in &combo.keys {
        match key {
            Key::Named(n) => {
                let code =
                    named_to_evdev(*n).ok_or_else(|| format!("no evdev code mapped for {n:?}"))?;
                seq.push((code, false));
            }
            Key::Char(c) => {
                let (code, shift) =
                    char_to_evdev(*c).ok_or_else(|| format!("cannot inject character {c:?}"))?;
                if shift && !combo.modifiers.shift {
                    seq.push((42, true)); // 补充临时 shift
                }
                seq.push((code, false));
            }
        }
    }
    Ok(seq)
}

#[cfg(test)]
mod tests {
    use super::*;
    use agent_shell_core::types::ModifierMask;

    #[test]
    fn char_mapping_covers_common_ascii() {
        assert_eq!(char_to_evdev('a'), Some((30, false)));
        assert_eq!(char_to_evdev('A'), Some((30, true)));
        assert_eq!(char_to_evdev('1'), Some((2, false)));
        assert_eq!(char_to_evdev('!'), Some((2, true)));
        assert_eq!(char_to_evdev(' '), Some((57, false)));
        assert_eq!(char_to_evdev('\n'), Some((28, false)));
        assert_eq!(char_to_evdev('中'), None);
    }

    #[test]
    fn named_mapping_is_total_over_keyname() {
        let all = [
            KeyName::Return,
            KeyName::Escape,
            KeyName::BackSpace,
            KeyName::Tab,
            KeyName::Space,
            KeyName::Left,
            KeyName::Right,
            KeyName::Up,
            KeyName::Down,
            KeyName::Home,
            KeyName::End,
            KeyName::PageUp,
            KeyName::PageDown,
            KeyName::Insert,
            KeyName::Delete,
            KeyName::Menu,
            KeyName::F1,
            KeyName::F2,
            KeyName::F3,
            KeyName::F4,
            KeyName::F5,
            KeyName::F6,
            KeyName::F7,
            KeyName::F8,
            KeyName::F9,
            KeyName::F10,
            KeyName::F11,
            KeyName::F12,
        ];
        for n in all {
            assert!(named_to_evdev(n).is_some(), "{n:?} unmapped");
        }
    }

    #[test]
    fn press_sequence_orders_modifiers_before_keys() {
        let combo = KeyCombo {
            keys: vec![Key::Named(KeyName::Return)],
            modifiers: ModifierMask::CTRL,
        };
        let seq = combo_to_press_sequence(&combo).unwrap();
        assert_eq!(seq.first(), Some(&(29, false))); // ctrl 先按
        assert_eq!(*seq.last().unwrap(), (28, false)); // Return 后按
    }

    #[test]
    fn uppercase_char_gets_temp_shift() {
        let combo = KeyCombo {
            keys: vec![Key::Char('A')],
            modifiers: ModifierMask::NONE,
        };
        let seq = combo_to_press_sequence(&combo).unwrap();
        assert_eq!(seq, vec![(42, true), (30, false)]);
    }
}
