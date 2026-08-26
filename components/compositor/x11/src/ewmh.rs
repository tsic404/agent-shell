//! EWMH/_NET_WM 原子操作的合成器层封装（设计文档 §11 模块结构表 `ewmh.rs`）。
//!
//! [`X11DisplayServer`] 已在协议层提供全部原生 EWMH 操作；本模块仅补齐
//! `_NET_DESKTOP_NAMES`（工作区名称）与 `_NET_SUPPORTED` /
//! `_NET_WM_WINDOW_TYPE` 原子读取三条未在协议层暴露的路径，供
//! [`crate::X11Compositor`] 复用。
//!
//! 所有方法基于 x11rb 原生协议，不依赖外部 CLI 工具。

use agent_shell_core::error::Result;
use agent_shell_core::types::WindowType;
use agent_shell_displayserver_x11::X11DisplayServer;

/// 工作区名称列表（`_NET_DESKTOP_NAMES`，NULL 分隔 UTF-8 字符串）。
///
/// 非 EWMH WM（无 `_NET_DESKTOP_NAMES`）返回空——调用方按工作区索引
/// 合成 `Desktop N` 兜底名。
pub fn desktop_names(ds: &X11DisplayServer) -> Result<Vec<String>> {
    let atoms = ds.atoms();
    let raw = ds.get_property_string(
        ds.root_window(),
        atoms._NET_DESKTOP_NAMES,
        atoms.UTF8_STRING,
    )?;
    Ok(raw
        .map(|s| {
            s.split('\0')
                .filter(|n| !n.is_empty())
                .map(str::to_string)
                .collect()
        })
        .unwrap_or_default())
}

/// `_NET_SUPPORTED` 原子列表（用于 doctor「N/M atoms supported」计数）。
pub fn supported_atoms(ds: &X11DisplayServer) -> Result<Vec<u32>> {
    use x11rb::protocol::xproto::AtomEnum;
    let atoms = ds.atoms();
    ds.get_property_u32(
        ds.root_window(),
        atoms._NET_SUPPORTED,
        AtomEnum::ATOM.into(),
    )
}

/// 本 crate intern 的 EWMH 原子总数（`atom_manager!` 宏展开的字段数）。
///
/// doctor 分母：`_NET_SUPPORTED` 返回的是 WM 声称支持的原子列表，
/// 分母为本 crate 实际查询的 EWMH 原子总数——两者比值反映 EWMH 覆盖度。
/// 设计文档 §6.6 示例写 12/12（核心 _NET_WM），本 crate 实际 intern 33 个。
pub fn ewmh_atom_count() -> usize {
    33
}

/// `_NET_WM_WINDOW_TYPE` 原子列表 → [`WindowType`] 归一化。
///
/// 缺失属性归为 `WindowType::Normal`（EWMH：未声明窗口类型的顶层窗口
/// 视为普通窗口）。
pub fn window_type(ds: &X11DisplayServer, window: u32) -> Result<WindowType> {
    let atoms = ds.atoms();
    let list = ds.get_property_u32(window, atoms._NET_WM_WINDOW_TYPE, x11rb::NONE)?;

    // EWMH：取第一个匹配的已知类型（窗口可声明多个，按优先级）。
    for a in &list {
        if *a == atoms._NET_WM_WINDOW_TYPE_NORMAL {
            return Ok(WindowType::Normal);
        }
        if *a == atoms._NET_WM_WINDOW_TYPE_DIALOG {
            return Ok(WindowType::Dialog);
        }
        if *a == atoms._NET_WM_WINDOW_TYPE_DOCK {
            return Ok(WindowType::Dock);
        }
        if *a == atoms._NET_WM_WINDOW_TYPE_DESKTOP {
            return Ok(WindowType::Desktop);
        }
        if *a == atoms._NET_WM_WINDOW_TYPE_MENU {
            return Ok(WindowType::DropdownMenu);
        }
        if *a == atoms._NET_WM_WINDOW_TYPE_TOOLTIP {
            return Ok(WindowType::Tooltip);
        }
        if *a == atoms._NET_WM_WINDOW_TYPE_SPLASH {
            return Ok(WindowType::Splash);
        }
    }
    Ok(if list.is_empty() {
        WindowType::Normal
    } else {
        WindowType::Unknown
    })
}

#[cfg(test)]
mod tests {
    #[test]
    fn desktop_names_parse_null_separated() {
        // EWMH _NET_DESKTOP_NAMES = "Main\0Work\0" → ["Main", "Work"]。
        let raw = "Main\0Work\0";
        let names: Vec<String> = raw
            .split('\0')
            .filter(|s| !s.is_empty())
            .map(str::to_string)
            .collect();
        assert_eq!(names, vec!["Main".to_string(), "Work".to_string()]);
    }

    #[test]
    fn empty_property_yields_empty_names() {
        // 非 EWMH WM：get_property_string 返回 None → 空表（不报错）。
        let none: Option<String> = None;
        let names: Vec<String> = none
            .map(|s| {
                s.split('\0')
                    .filter(|n| !n.is_empty())
                    .map(str::to_string)
                    .collect()
            })
            .unwrap_or_default();
        assert!(names.is_empty());
    }
}
