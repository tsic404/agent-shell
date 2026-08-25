//! 无障碍树模型（设计文档 §14.1 `a11y::tree`）。
//!
//! 节点持有 AT-SPI D-Bus 引用 `(bus_name, object_path)`——AT-SPI 树的每个
//! 可访问对象都由「应用总线名 + 对象路径」唯一定位；属性按需经
//! [`crate::atspi_bridge::AtspiBridge`] 读取，不在本地缓存整棵树，
//! 避免 DEFUNCT（应用销毁后引用失效）问题。

use agent_shell_core::types::Rect;

/// 状态集：AT-SPI `GetState` 返回 `(u32, u32)` 位图，本模块解码为命名集合。
///
/// 位索引与 AT-SPI `AtspiStateType` 枚举一致
/// （at-spi-2.0 atspi-constants.h；ACTIVE=1、FOCUSED=12 等）。
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct AtspiState(pub u64);

impl AtspiState {
    /// 当前活动窗口。
    pub const ACTIVE: Self = Self(1 << 1);
    /// 对象可接受焦点。
    pub const FOCUSABLE: Self = Self(1 << 11);
    /// 对象当前持有键盘焦点。
    pub const FOCUSED: Self = Self(1 << 12);
    /// 对象可见（含被遮挡）。
    pub const VISIBLE: Self = Self(1 << 30);
    /// 对象在屏幕上实际显示。
    pub const SHOWING: Self = Self(1 << 25);
    /// 对象可编辑。
    pub const EDITABLE: Self = Self(1 << 7);
    /// 引用已失效（对象已销毁）。
    pub const DEFUNCT: Self = Self(1 << 6);

    /// 从 AT-SPI `(low, high)` 双字位图构造。
    pub fn from_pair(low: u32, high: u32) -> Self {
        Self((low as u64) | ((high as u64) << 32))
    }

    /// 是否包含指定状态。
    pub fn contains(&self, state: AtspiState) -> bool {
        self.0 & state.0 == state.0
    }
}

/// 元素角色（AT-SPI `AtspiRoleType` 数值 + 规范名）。
///
/// 匹配按角色名进行（`GetRoleName` 返回 snake_case 名，如 "push button"）；
/// 同时保留数值以便未知角色的稳定比较。
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct AtspiRole {
    /// AT-SPI 角色数值（`GetRole`）。
    pub code: u32,
    /// 规范角色名（`GetRoleName`），如 `"push button"`、`"text"`。
    pub name: String,
}

impl AtspiRole {
    /// 按名称匹配（大小写/分隔符不敏感；含跨实现的别名归一）。
    ///
    /// 实测差异：规范 `GetRoleName` 对 BUTTON 返回 "push button"，
    /// GTK atk-bridge 返回 "button"；TEXT 规范 "text"、Qt 亦 "text"。
    /// 用别名表吸收实现差异，避免调用方必须猜工具包方言。
    pub fn matches_name(&self, wanted: &str) -> bool {
        let norm = |s: &str| s.to_ascii_lowercase().replace([' ', '-', '_'], "");
        let a = norm(&self.name);
        let b = norm(wanted);
        if a == b {
            return true;
        }
        // 双向别名：button↔pushbutton、menuitem↔menu item 等已由
        // 分隔符归一覆盖；此处只留真正异名的对。
        const ALIASES: &[(&str, &str)] = &[
            ("button", "pushbutton"),
            ("checkbox", "checkbutton"),
            ("combobox", "dropdownlistbox"),
            ("entry", "textbox"),
            ("text", "label"),
        ];
        ALIASES
            .iter()
            .any(|&(x, y)| (a == x && b == y) || (a == y && b == x))
    }
}

/// 应用节点（桌面根的下层；对应 AT-SPI `APPLICATION` 角色）。
#[derive(Clone, Debug)]
pub struct ApplicationNode {
    /// D-Bus 引用：应用总线名。
    pub bus_name: String,
    /// D-Bus 引用：对象路径（通常为 root 路径）。
    pub path: String,
    /// 应用名（Accessible.Name 属性）。
    pub name: String,
    /// 进程 PID（经 DBus daemon GetConnectionUnixProcessID 解析）。
    pub pid: Option<u32>,
}

/// 窗口节点（应用的下层；对应 `FRAME` / `WINDOW` 角色）。
#[derive(Clone, Debug)]
pub struct WindowNode {
    /// 所属应用总线名。
    pub bus_name: String,
    /// 窗口对象路径。
    pub path: String,
    /// 窗口标题（Accessible.Name）。
    pub name: String,
    /// 状态集（活动窗口判定用 ACTIVE/FOCUSED）。
    pub states: AtspiState,
}

/// 通用元素节点（窗口内任意可访问对象）。
#[derive(Clone, Debug)]
pub struct ElementNode {
    /// 所属应用总线名。
    pub bus_name: String,
    /// 对象路径。
    pub path: String,
    /// 元素名称（Accessible.Name）。
    pub name: String,
    /// 角色信息。
    pub role: AtspiRole,
    /// 状态集。
    pub states: AtspiState,
}

impl ElementNode {
    /// 元素中心点坐标（屏幕坐标系；点击降级路径使用）。
    ///
    /// 坐标来自 Component.GetExtents(coord_type = SCREEN = 0)。
    pub fn center_of(extents: Rect) -> (i32, i32) {
        extents.center()
    }
}
