//! XTest 扩展输入注入的合成器层封装（设计文档 §11 模块结构表 `xtest.rs`）。
//!
//! [`X11DisplayServer`] 已在协议层（§6.3）实现 `fake_key_event` /
//! `fake_button_event` / `fake_motion_event`；本模块仅提供 thin wrapper：
//! 预检 XTest 可用性并统一错误归一化为 `AgentShellError::Input`，供
//! [`crate::X11Compositor`] 的能力报告与降级链使用。
//!
//! **注意**（§6.3）：XTest 仅在原生 X11 会话（`XDG_SESSION_TYPE=X11`）下
//! 可用；XWayland 下被禁用，输入注入应走 libei/EIS（T2a）。

use agent_shell_core::error::{AgentShellError, Result};
use agent_shell_core::types::{KeyCombo, KeyName, MouseButton};
use agent_shell_displayserver_x11::X11DisplayServer;

/// XTest 输入注入是否原生可用（构造时探测；false 时上层降级 libei/ydotool）。
pub fn is_available(ds: &X11DisplayServer) -> bool {
    ds.is_xtest_available()
}

/// 按下/释放按键（keycode 为硬件键码，非 keysym）。
pub fn fake_key_event(ds: &X11DisplayServer, keycode: u8, is_press: bool) -> Result<()> {
    ds.fake_key_event(keycode, is_press)
}

/// 按下/释放鼠标按键（button: 1=左 2=中 3=右 8=后退 9=前进）。
pub fn fake_button_event(ds: &X11DisplayServer, button: u8, is_press: bool) -> Result<()> {
    ds.fake_button_event(button, is_press)
}

/// 绝对指针移动（屏幕坐标）。
pub fn fake_motion_event(ds: &X11DisplayServer, x: i16, y: i16) -> Result<()> {
    ds.fake_motion_event(x, y)
}

/// 鼠标按键 → X 按钮编码（X11 鼠标按钮编号约定）。
pub fn button_code(b: MouseButton) -> u8 {
    match b {
        MouseButton::Left => 1,
        MouseButton::Middle => 2,
        MouseButton::Right => 3,
        MouseButton::Back => 8,
        MouseButton::Forward => 9,
    }
}

/// 按下并释放鼠标按键（click = press + release + flush）。
pub fn click(ds: &X11DisplayServer, button: MouseButton) -> Result<()> {
    let code = button_code(button);
    fake_button_event(ds, code, true)?;
    fake_button_event(ds, code, false)
}

/// XTest 注入入口：合成一次按键组合（modifier + key 序列）。
///
/// 当前只支持单键；组合键需要 keysym → keycode 映射（依赖 X server 键表），
/// 属于 T2a InputComponent 的 dispatcher 职责——本方法仅暴露底层 XTest
/// 通道，不做键码翻译。
pub fn send_keycombo(_ds: &X11DisplayServer, combo: &KeyCombo) -> Result<()> {
    // XTest 以 keycode 为参数；无键表映射时无法从 Key::Char/Named 推导。
    // 上层（InputComponent dispatcher）负责 keysym → keycode 转换并直接调用
    // fake_key_event；本方法保留接口占位以表明本 crate 的 XTest 通道位置。
    let _ = combo;
    Err(AgentShellError::Input(
        "xtest: KeyCombo keycode mapping is InputComponent dispatcher's job".into(),
    ))
}

/// 命名键 → 是否被本模块识别（占位：真实映射在 InputComponent）。
pub fn named_keysym(_n: KeyName) -> Option<u8> {
    None
}

/// 字符键 → 是否被本模块识别（占位：真实映射在 InputComponent）。
pub fn char_keycode(_c: char) -> Option<u8> {
    None
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn button_codes_match_x11_convention() {
        // X11 鼠标按钮编号：1=左 2=中 3=右 8=后退 9=前进（XInput2 约定）。
        assert_eq!(button_code(MouseButton::Left), 1);
        assert_eq!(button_code(MouseButton::Middle), 2);
        assert_eq!(button_code(MouseButton::Right), 3);
        assert_eq!(button_code(MouseButton::Back), 8);
        assert_eq!(button_code(MouseButton::Forward), 9);
    }
}
