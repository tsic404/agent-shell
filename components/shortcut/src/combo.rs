//! 组合键 → 各 DE 快捷键语法转换（§21.33）。
//!
//! 全局快捷键不是「注入按键」：每个 DE 用各自的语法描述绑定，语法错误只在
//! 用户真正按键时才暴露，因此转换表单独成模块，逐一对齐上游常量——Qt keycode
//! 取自 Qt6 `qnamespace.h`（`Qt::Key_*` 与 `Qt::*Modifier`），GTK accelerator 与
//! Hyprland 用 X11 keysym 名（`Prior`/`Next` 是 PageUp/PageDown 的 keysym 名）。

use agent_shell_core::types::{Key, KeyCombo, KeyName};

/// Qt 修饰键位：ShiftModifier。
const QT_SHIFT: i32 = 0x0200_0000;
/// Qt 修饰键位：ControlModifier。
const QT_CTRL: i32 = 0x0400_0000;
/// Qt 修饰键位：AltModifier。
const QT_ALT: i32 = 0x0800_0000;
/// Qt 修饰键位：MetaModifier。
const QT_META: i32 = 0x1000_0000;

/// 取组合键中唯一的非修饰键。
///
/// 全局快捷键绑定的是单个按键 + 修饰键；多键序列（如 `ctrl+k ctrl+c`）在
/// KGlobalAccel / gsettings / Hyprland 三处都没有等价语法，显式拒绝而非截断。
pub(crate) fn single_key(combo: &KeyCombo) -> Result<&Key, String> {
    match combo.keys.as_slice() {
        [key] => Ok(key),
        other => Err(format!(
            "global shortcut requires exactly one key (got {})",
            other.len()
        )),
    }
}

/// 组合键规范形式（`meta+shift+t`），用于 RPC 回执。
pub(crate) fn canonical(combo: &KeyCombo) -> String {
    let mut parts: Vec<String> = Vec::new();
    if combo.modifiers.ctrl {
        parts.push("ctrl".into());
    }
    if combo.modifiers.alt {
        parts.push("alt".into());
    }
    if combo.modifiers.shift {
        parts.push("shift".into());
    }
    if combo.modifiers.meta {
        parts.push("meta".into());
    }
    if let Some(key) = combo.keys.first() {
        parts.push(key_label(key));
    }
    parts.join("+")
}

/// 键的规范书写：字符键原样（字母小写），命名键用 CLI 侧同一套名字。
fn key_label(key: &Key) -> String {
    match key {
        Key::Char(c) => c.to_lowercase().to_string(),
        Key::Named(named) => named_label(*named).to_string(),
    }
}

/// 命名键的 CLI 写法（与 `input send` 的解析表一致，§17.2）。
fn named_label(named: KeyName) -> &'static str {
    match named {
        KeyName::Return => "return",
        KeyName::Escape => "escape",
        KeyName::BackSpace => "backspace",
        KeyName::Tab => "tab",
        KeyName::Space => "space",
        KeyName::Left => "left",
        KeyName::Right => "right",
        KeyName::Up => "up",
        KeyName::Down => "down",
        KeyName::Home => "home",
        KeyName::End => "end",
        KeyName::PageUp => "pageup",
        KeyName::PageDown => "pagedown",
        KeyName::Insert => "insert",
        KeyName::Delete => "delete",
        KeyName::Menu => "menu",
        KeyName::F1 => "f1",
        KeyName::F2 => "f2",
        KeyName::F3 => "f3",
        KeyName::F4 => "f4",
        KeyName::F5 => "f5",
        KeyName::F6 => "f6",
        KeyName::F7 => "f7",
        KeyName::F8 => "f8",
        KeyName::F9 => "f9",
        KeyName::F10 => "f10",
        KeyName::F11 => "f11",
        KeyName::F12 => "f12",
    }
}

/// 命名键的 X11 keysym 名（GTK accelerator 与 Hyprland 共用同一张表）。
fn keysym_name(named: KeyName) -> &'static str {
    match named {
        KeyName::Return => "Return",
        KeyName::Escape => "Escape",
        KeyName::BackSpace => "BackSpace",
        KeyName::Tab => "Tab",
        // keysym 名小写 `space`；`Space` 不是合法 keysym 名。
        KeyName::Space => "space",
        KeyName::Left => "Left",
        KeyName::Right => "Right",
        KeyName::Up => "Up",
        KeyName::Down => "Down",
        KeyName::Home => "Home",
        KeyName::End => "End",
        // PageUp/PageDown 的 keysym 名是 Prior/Next。
        KeyName::PageUp => "Prior",
        KeyName::PageDown => "Next",
        KeyName::Insert => "Insert",
        KeyName::Delete => "Delete",
        KeyName::Menu => "Menu",
        KeyName::F1 => "F1",
        KeyName::F2 => "F2",
        KeyName::F3 => "F3",
        KeyName::F4 => "F4",
        KeyName::F5 => "F5",
        KeyName::F6 => "F6",
        KeyName::F7 => "F7",
        KeyName::F8 => "F8",
        KeyName::F9 => "F9",
        KeyName::F10 => "F10",
        KeyName::F11 => "F11",
        KeyName::F12 => "F12",
    }
}

/// 命名键的 Qt 键值（`Qt::Key_*`）；命名键与 `Key::Char` 之外的按键无关。
fn qt_named(named: KeyName) -> i32 {
    match named {
        KeyName::Return => 0x0100_0004,
        KeyName::Escape => 0x0100_0000,
        KeyName::BackSpace => 0x0100_0003,
        KeyName::Tab => 0x0100_0001,
        KeyName::Space => 0x20,
        KeyName::Left => 0x0100_0012,
        KeyName::Right => 0x0100_0014,
        KeyName::Up => 0x0100_0013,
        KeyName::Down => 0x0100_0015,
        KeyName::Home => 0x0100_0010,
        KeyName::End => 0x0100_0011,
        KeyName::PageUp => 0x0100_0016,
        KeyName::PageDown => 0x0100_0017,
        KeyName::Insert => 0x0100_0006,
        KeyName::Delete => 0x0100_0007,
        KeyName::Menu => 0x0100_0055,
        KeyName::F1 => 0x0100_0030,
        KeyName::F2 => 0x0100_0031,
        KeyName::F3 => 0x0100_0032,
        KeyName::F4 => 0x0100_0033,
        KeyName::F5 => 0x0100_0034,
        KeyName::F6 => 0x0100_0035,
        KeyName::F7 => 0x0100_0036,
        KeyName::F8 => 0x0100_0037,
        KeyName::F9 => 0x0100_0038,
        KeyName::F10 => 0x0100_0039,
        KeyName::F11 => 0x0100_003a,
        KeyName::F12 => 0x0100_003b,
    }
}

/// Qt keycode（`setForeignShortcut` 的 `ai` 载荷）。
///
/// 可打印 ASCII 的 Qt 键值等于其 ASCII 码（字母用大写，`Qt::Key_A` = 0x41），
/// 其余字符无对应键值——非 ASCII 字符（如 `é`）在物理键盘布局里没有稳定键位，
/// 显式拒绝。
pub(crate) fn qt_keycode(combo: &KeyCombo) -> Result<i32, String> {
    let key = single_key(combo)?;
    let mut code = match key {
        Key::Char(c) => {
            let upper = c.to_ascii_uppercase();
            if !upper.is_ascii_graphic() {
                return Err(format!("key {c:?} has no Qt keycode"));
            }
            upper as i32
        }
        Key::Named(named) => qt_named(*named),
    };
    if combo.modifiers.shift {
        code |= QT_SHIFT;
    }
    if combo.modifiers.ctrl {
        code |= QT_CTRL;
    }
    if combo.modifiers.alt {
        code |= QT_ALT;
    }
    if combo.modifiers.meta {
        code |= QT_META;
    }
    Ok(code)
}

/// GTK accelerator（GNOME `custom-keybinding binding` 值）。
pub(crate) fn gtk_accelerator(combo: &KeyCombo) -> Result<String, String> {
    let key = single_key(combo)?;
    let mut accel = String::new();
    if combo.modifiers.ctrl {
        accel.push_str("<Control>");
    }
    if combo.modifiers.alt {
        accel.push_str("<Alt>");
    }
    if combo.modifiers.shift {
        accel.push_str("<Shift>");
    }
    if combo.modifiers.meta {
        accel.push_str("<Super>");
    }
    accel.push_str(&match key {
        Key::Char(c) => c.to_lowercase().to_string(),
        Key::Named(named) => keysym_name(*named).to_string(),
    });
    Ok(accel)
}

/// Hyprland 绑定的键位部分（`bind = <mods>, <key>, …` 的 `<mods>, <key>`）。
pub(crate) fn hyprland_bind(combo: &KeyCombo) -> Result<String, String> {
    let key = single_key(combo)?;
    let mut mods: Vec<&str> = Vec::new();
    if combo.modifiers.meta {
        mods.push("SUPER");
    }
    if combo.modifiers.shift {
        mods.push("SHIFT");
    }
    if combo.modifiers.ctrl {
        mods.push("CTRL");
    }
    if combo.modifiers.alt {
        mods.push("ALT");
    }
    let key_name = match key {
        // Hyprland 以 XKB_KEYSYM_CASE_INSENSITIVE 解析键名，字母统一大写更贴近
        // 上游文档示例（`bind = SUPER, Q, …`）。
        Key::Char(c) => c.to_uppercase().to_string(),
        Key::Named(named) => keysym_name(*named).to_string(),
    };
    Ok(format!("{}, {}", mods.join(" "), key_name))
}

#[cfg(test)]
mod tests {
    use super::*;
    use agent_shell_core::types::ModifierMask;

    fn combo(modifiers: ModifierMask, key: Key) -> KeyCombo {
        KeyCombo {
            keys: vec![key],
            modifiers,
        }
    }

    fn ctrl_alt() -> ModifierMask {
        ModifierMask {
            ctrl: true,
            alt: true,
            ..ModifierMask::NONE
        }
    }

    #[test]
    fn qt_keycode_matches_qt6_constants() {
        // Meta+T = MetaModifier | Qt::Key_T(0x54)，与 kglobalaccel 回读值一致。
        let meta_t = combo(
            ModifierMask {
                meta: true,
                ..ModifierMask::NONE
            },
            Key::Char('t'),
        );
        assert_eq!(qt_keycode(&meta_t).unwrap(), 268_435_540);
        assert_eq!(qt_keycode(&meta_t).unwrap(), 0x1000_0000 | 0x54);

        let ctrl_alt_1 = combo(ctrl_alt(), Key::Char('1'));
        assert_eq!(qt_keycode(&ctrl_alt_1).unwrap(), 0x0c00_0000 | 0x31);

        let f12 = combo(ModifierMask::NONE, Key::Named(KeyName::F12));
        assert_eq!(qt_keycode(&f12).unwrap(), 0x0100_003b);

        let shift_space = combo(
            ModifierMask {
                shift: true,
                ..ModifierMask::NONE
            },
            Key::Named(KeyName::Space),
        );
        assert_eq!(qt_keycode(&shift_space).unwrap(), 0x0200_0000 | 0x20);
    }

    #[test]
    fn qt_keycode_rejects_unmappable_and_multi_key_combos() {
        let non_ascii = combo(ModifierMask::NONE, Key::Char('é'));
        assert!(qt_keycode(&non_ascii).is_err());
        let multi = KeyCombo {
            keys: vec![Key::Char('k'), Key::Char('c')],
            modifiers: ModifierMask::NONE,
        };
        assert!(single_key(&multi).is_err());
        assert!(gtk_accelerator(&multi).is_err());
        assert!(hyprland_bind(&multi).is_err());
    }

    #[test]
    fn gtk_accelerator_uses_gtk_modifier_and_keysym_names() {
        let meta_t = combo(
            ModifierMask {
                meta: true,
                ..ModifierMask::NONE
            },
            Key::Char('t'),
        );
        assert_eq!(gtk_accelerator(&meta_t).unwrap(), "<Super>t");

        let ctrl_alt_pageup = combo(ctrl_alt(), Key::Named(KeyName::PageUp));
        assert_eq!(
            gtk_accelerator(&ctrl_alt_pageup).unwrap(),
            "<Control><Alt>Prior"
        );

        let space = combo(ModifierMask::NONE, Key::Named(KeyName::Space));
        assert_eq!(gtk_accelerator(&space).unwrap(), "space");

        let ctrl_shift_f5 = combo(
            ModifierMask {
                ctrl: true,
                shift: true,
                ..ModifierMask::NONE
            },
            Key::Named(KeyName::F5),
        );
        assert_eq!(
            gtk_accelerator(&ctrl_shift_f5).unwrap(),
            "<Control><Shift>F5"
        );
    }

    #[test]
    fn hyprland_bind_orders_modifiers_and_uppercases_chars() {
        let meta_shift_t = combo(
            ModifierMask {
                meta: true,
                shift: true,
                ..ModifierMask::NONE
            },
            Key::Char('t'),
        );
        assert_eq!(hyprland_bind(&meta_shift_t).unwrap(), "SUPER SHIFT, T");

        let ctrl_alt_delete = combo(ctrl_alt(), Key::Named(KeyName::Delete));
        assert_eq!(hyprland_bind(&ctrl_alt_delete).unwrap(), "CTRL ALT, Delete");

        let no_mods_f1 = combo(ModifierMask::NONE, Key::Named(KeyName::F1));
        assert_eq!(hyprland_bind(&no_mods_f1).unwrap(), ", F1");
    }

    #[test]
    fn canonical_form_lists_modifiers_then_key() {
        let meta_shift_t = combo(
            ModifierMask {
                meta: true,
                shift: true,
                ..ModifierMask::NONE
            },
            Key::Char('t'),
        );
        assert_eq!(canonical(&meta_shift_t), "shift+meta+t");
        let ctrl_alt_pageup = combo(ctrl_alt(), Key::Named(KeyName::PageUp));
        assert_eq!(canonical(&ctrl_alt_pageup), "ctrl+alt+pageup");
    }
}
