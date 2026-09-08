//! 核心类型系统 — 所有模块共享的统一类型定义。
//!
//! 对应设计文档 `design/01-core-types/` §2：所有窗口/工作区/监视器/输入/定位/截图的
//! 共享数据结构集中在此，保证跨后端、跨组件的数据形状一致。

use std::collections::HashMap;

use serde::{Deserialize, Serialize};

// ───────────────────────── 标识类型 ─────────────────────────

/// 全局唯一窗口标识。
///
/// 由原生窗口 id（合成器侧标识）与桌面环境类型共同构成，确保跨 DE 不冲突。
#[derive(Clone, Debug, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub struct WindowId {
    /// 合成器/后端原生窗口标识（KWin internalId、X11 window xid、Hyprland address 等）
    pub native_id: String,
    /// 窗口所属桌面环境，用于跨 DE 去重
    pub de_type: DesktopEnvironment,
}

/// 工作区标识。
#[derive(Clone, Debug, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub struct WorkspaceId {
    /// 原生工作区标识
    pub native_id: String,
    pub de_type: DesktopEnvironment,
}

/// 监视器标识。
#[derive(Clone, Debug, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub struct MonitorId {
    /// 原生监视器标识（输出名，如 "eDP-1"、"HDMI-A-1"）
    pub native_id: String,
    pub de_type: DesktopEnvironment,
}

// ───────────────────────── 几何类型 ─────────────────────────

/// 矩形区域（逻辑像素坐标）。
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct Rect {
    /// 左上角 X 坐标
    pub x: i32,
    /// 左上角 Y 坐标
    pub y: i32,
    /// 宽度
    pub width: i32,
    /// 高度
    pub height: i32,
}

impl Rect {
    /// 矩形中心点坐标。
    pub fn center(&self) -> (i32, i32) {
        (self.x + self.width / 2, self.y + self.height / 2)
    }

    /// 判断点是否在矩形内。
    pub fn contains(&self, x: i32, y: i32) -> bool {
        x >= self.x && x < self.x + self.width && y >= self.y && y < self.y + self.height
    }
}

// ───────────────────────── 窗口类型 ─────────────────────────

/// 窗口信息（窗口列表/查询/事件的统一数据结构）。
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct WindowInfo {
    /// 窗口唯一标识
    pub id: WindowId,
    /// 窗口标题
    pub title: String,
    /// 应用标识（Wayland app_id / X11 WM_CLASS）
    pub app_id: String,
    /// 进程 PID
    pub pid: u32,
    /// 内容几何（窗口内部区域，不含装饰）
    pub geometry: Rect,
    /// 边框几何（含窗口装饰的外框）
    pub frame_geometry: Rect,
    /// 当前状态集合（多状态可共存，如 Maximized + FullScreen）
    pub states: Vec<WindowState>,
    /// 所属工作区（None = 全部工作区可见 / 未分组）
    pub workspace_id: Option<WorkspaceId>,
    /// 所在监视器（None = 未绑定具体显示器）
    pub monitor_id: Option<MonitorId>,
    /// 层叠顺序（z-order，越大越靠上）
    pub stacking_order: u32,
    /// 关联的 .desktop 文件（如能解析到）
    pub desktop_file: Option<String>,
    /// 窗口类型
    pub window_type: WindowType,
    /// 任务栏图标几何（最小化时的提示区域）
    pub icon_geometry: Option<Rect>,
    /// 是否置顶（keep-above）
    pub keep_above: bool,
}

/// 窗口状态（多状态可共存）。
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub enum WindowState {
    /// 正常
    Normal,
    /// 最小化
    Minimized,
    /// 最大化
    Maximized,
    /// 全屏
    FullScreen,
    /// 隐藏
    Hidden,
}

/// 窗口类型（EWMH `_NET_WM_WINDOW_TYPE` 归一化）。
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub enum WindowType {
    /// 普通窗口
    Normal,
    /// 对话框
    Dialog,
    /// 面板/任务栏/Dock
    Dock,
    /// 桌面背景窗口
    Desktop,
    /// 下拉菜单
    DropdownMenu,
    /// 工具提示
    Tooltip,
    /// 通知
    Notification,
    /// 启动闪屏
    Splash,
    /// 工具窗口
    Utility,
    /// 未知类型
    Unknown,
}

// ───────────────────────── 工作区/监视器 ─────────────────────────

/// 工作区信息。
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct WorkspaceInfo {
    /// 工作区标识
    pub id: WorkspaceId,
    /// 工作区名称
    pub name: String,
    /// 工作区编号（从 1 开始）
    pub number: u32,
    /// 是否为当前活动工作区
    pub is_active: bool,
    /// 该工作区显示的监视器列表
    pub monitor_ids: Vec<MonitorId>,
    /// 该工作区上的窗口列表
    pub window_ids: Vec<WindowId>,
}

/// 监视器信息。
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct MonitorInfo {
    /// 监视器标识
    pub id: MonitorId,
    /// 输出名称（如 "eDP-1"）
    pub name: String,
    /// 逻辑几何（Wayland: 缩放后坐标; X11: 物理坐标）
    pub geometry: Rect,
    /// 物理像素尺寸（原始分辨率）
    pub physical_geometry: Rect,
    /// 缩放因子（Wayland: 1.0-2.0; X11: 1.0）
    pub scale: f64,
    /// 是否为主显示器
    pub is_primary: bool,
    /// 当前显示的工作区（None = 全部工作区 / 未分组）
    pub workspace_id: Option<WorkspaceId>,
}

// ───────────────────────── 桌面环境 ─────────────────────────

/// 桌面环境枚举（DE 检测结果）。
///
/// 对应设计文档 §16.1 检测策略。TTY 为纯终端环境（无合成器/窗口管理）。
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub enum DesktopEnvironment {
    /// KDE Plasma
    KDE,
    /// GNOME
    GNOME,
    /// Hyprland
    Hyprland,
    /// Deepin DDE
    DDE,
    /// Sway
    Sway,
    /// Budgie
    Budgie,
    /// XFCE
    XFCE,
    /// Cinnamon
    Cinnamon,
    /// COSMIC
    Cosmic,
    /// LXQt
    LXQt,
    /// MATE
    MATE,
    /// 未知 Wayland compositor（wlroots 系兜底）
    WLRWayland,
    /// 未知 X11 桌面（兜底）
    X11Generic,
    /// 纯终端（无显示服务器）
    Tty,
    /// 未知环境
    Unknown,
}

impl DesktopEnvironment {
    /// 该 DE 是否为 Wayland 合成器（仅 Wayland 会话）。
    pub const fn is_wayland_only(self) -> bool {
        matches!(self, Self::Hyprland | Self::Sway | Self::WLRWayland)
    }

    /// 该 DE 是否支持 X11 会话路由。
    pub const fn supports_x11(self) -> bool {
        matches!(
            self,
            Self::KDE
                | Self::GNOME
                | Self::DDE
                | Self::Budgie
                | Self::XFCE
                | Self::Cinnamon
                | Self::Cosmic
                | Self::LXQt
                | Self::MATE
                | Self::X11Generic
        )
    }

    /// 该 DE 是否为纯终端环境（无窗口/输入/截图等能力）。
    pub const fn is_tty(self) -> bool {
        matches!(self, Self::Tty)
    }
}

impl std::fmt::Display for DesktopEnvironment {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "{self:?}")
    }
}

// ───────────────────────── 输入类型 ─────────────────────────

/// 按键组合。
///
/// 派生 `Eq`/`Hash`：快捷键匹配与去重场景需要（如注册表去重、热键表查找）。
#[derive(Clone, Debug, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub struct KeyCombo {
    /// 按键序列（可多键，如 [Char('a')] 或 [Named(Tab)]）
    pub keys: Vec<Key>,
    /// 修饰键掩码
    pub modifiers: ModifierMask,
}

/// 单个按键。
#[derive(Clone, Debug, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub enum Key {
    /// 字符键（如 'a', '1'）
    Char(char),
    /// 命名键（功能键、方向键等）
    Named(KeyName),
}

/// 命名按键。
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub enum KeyName {
    /// 回车
    Return,
    /// Esc
    Escape,
    /// 退格
    BackSpace,
    /// Tab
    Tab,
    /// 空格
    Space,
    /// 左/右/上/下
    Left,
    /// 右
    Right,
    /// 上
    Up,
    /// 下
    Down,
    /// Home
    Home,
    /// End
    End,
    /// PageUp
    PageUp,
    /// PageDown
    PageDown,
    /// Insert
    Insert,
    /// Delete
    Delete,
    /// 菜单键
    Menu,
    /// F1-F12
    F1,
    /// F2
    F2,
    /// F3
    F3,
    /// F4
    F4,
    /// F5
    F5,
    /// F6
    F6,
    /// F7
    F7,
    /// F8
    F8,
    /// F9
    F9,
    /// F10
    F10,
    /// F11
    F11,
    /// F12
    F12,
}

/// 修饰键掩码。
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub struct ModifierMask {
    /// Ctrl
    pub ctrl: bool,
    /// Alt
    pub alt: bool,
    /// Shift
    pub shift: bool,
    /// Meta/Super/Win
    pub meta: bool,
}

impl ModifierMask {
    /// 无修饰键。
    pub const NONE: Self = Self {
        ctrl: false,
        alt: false,
        shift: false,
        meta: false,
    };

    /// 仅 Ctrl
    pub const CTRL: Self = Self {
        ctrl: true,
        alt: false,
        shift: false,
        meta: false,
    };

    /// 仅 Alt
    pub const ALT: Self = Self {
        ctrl: false,
        alt: true,
        shift: false,
        meta: false,
    };

    /// 仅 Shift
    pub const SHIFT: Self = Self {
        ctrl: false,
        alt: false,
        shift: true,
        meta: false,
    };

    /// 仅 Meta
    pub const META: Self = Self {
        ctrl: false,
        alt: false,
        shift: false,
        meta: true,
    };

    /// 是否有任意修饰键按下。
    pub const fn is_empty(self) -> bool {
        !self.ctrl && !self.alt && !self.shift && !self.meta
    }
}

/// 鼠标按键。
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub enum MouseButton {
    /// 左键
    Left,
    /// 中键
    Middle,
    /// 右键
    Right,
    /// 侧后键
    Back,
    /// 侧前键
    Forward,
}

/// 鼠标滚动增量。
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct ScrollDelta {
    /// 水平滚动量（正值向右）
    pub dx: i32,
    /// 垂直滚动量（正值向下）
    pub dy: i32,
}

// ───────────────────────── 语义定位 ─────────────────────────

/// 语义定位目标（跨后端统一的窗口/元素定位描述）。
///
/// 对应设计文档 §15.3 语义定位优先级链。
#[derive(Clone, Debug, Serialize, Deserialize)]
pub enum SemanticTarget {
    /// 按 WindowId 精确匹配（最可靠）
    ById(WindowId),
    /// 按 app_id 匹配
    ByAppId(String),
    /// 按窗口标题匹配（指定匹配模式）
    ByTitle(String, TitleMatchMode),
    /// 按 PID 匹配
    ByPid(u32),
    /// 按 .desktop 文件匹配
    ByDesktopFile(String),
    /// 按 AT-SPI 无障碍语义匹配
    ByAccessibility {
        /// 元素角色（如 "button"、"menu_item"）
        role: Option<String>,
        /// 元素名称
        name: Option<String>,
        /// 父元素角色
        parent_role: Option<String>,
        /// 父元素名称
        parent_name: Option<String>,
    },
    /// 当前活动窗口
    Active,
    /// 按坐标定位
    ByCoordinate {
        /// X 坐标
        x: i32,
        /// Y 坐标
        y: i32,
    },
    /// 按区域定位
    ByRegion(Rect),
}

/// 窗口标题匹配模式。
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum TitleMatchMode {
    /// 子串包含
    Substring,
    /// 精确匹配
    Exact,
    /// 正则匹配
    Regex,
    /// Glob 通配符
    Glob,
}

/// 预编译标题匹配器（§15.3 第 3 级：子串 → 精确 → 正则 → glob）。
///
/// `Regex` 模式在构造时编译一次，供批量过滤（`windows list` 逐窗口）与
/// 轮询（`windows wait` 每 200ms 一轮）复用，避免每次匹配重复编译同一
/// pattern。非 `Regex` 模式不额外分配。
pub struct TitleMatcher<'a> {
    mode: TitleMatchMode,
    pattern: &'a str,
    regex: Option<regex::Regex>,
}

impl<'a> TitleMatcher<'a> {
    /// 构造匹配器。`Regex` 在此预编译一次；编译失败按「不匹配」降级
    /// （不向上抛错），与非法表达式应视为零命中的过滤语义一致。
    pub fn new(mode: TitleMatchMode, pattern: &'a str) -> Self {
        let regex = match mode {
            TitleMatchMode::Regex => regex::Regex::new(pattern).ok(),
            _ => None,
        };
        TitleMatcher {
            mode,
            pattern,
            regex,
        }
    }

    /// 原始 pattern（供 `app_id` 精确比对等旁路判断）。
    pub fn pattern(&self) -> &str {
        self.pattern
    }

    /// 标题是否命中。
    pub fn matches(&self, title: &str) -> bool {
        match self.mode {
            TitleMatchMode::Substring => title.contains(self.pattern),
            TitleMatchMode::Exact => title == self.pattern,
            TitleMatchMode::Regex => self
                .regex
                .as_ref()
                .map(|r| r.is_match(title))
                .unwrap_or(false),
            TitleMatchMode::Glob => glob_match::glob_match(self.pattern, title),
        }
    }
}

// ───────────────────────── 截图类型 ─────────────────────────

/// 截图目标。
#[derive(Clone, Debug, Serialize, Deserialize)]
pub enum CaptureTarget {
    /// 全屏
    Screen,
    /// 指定监视器
    Monitor(MonitorId),
    /// 指定窗口
    Window(WindowId),
    /// 指定区域
    Area(Rect),
}

// ───────────────────────── 系统服务类型 ─────────────────────────

/// 系统服务单元状态（systemd）。
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub enum UnitStatus {
    /// 活动中
    Active,
    /// 重新加载中
    Reloading,
    /// 未活动
    Inactive,
    /// 失败
    Failed,
    /// 激活中
    Activating,
    /// 去激活中
    Deactivating,
    /// 未知
    Unknown,
}

/// 登录会话信息（logind）。
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct SessionInfo {
    /// 会话 ID
    pub id: String,
    /// 用户 UID
    pub uid: u32,
    /// 用户名
    pub user_name: String,
    /// seat（如 "seat0"）
    pub seat: String,
    /// 显示（如 "tty2" / ":0" / "wayland-0"）
    pub display: String,
    /// 是否远程会话
    pub remote: bool,
    /// 远程主机（非远程为 None）
    pub remote_host: Option<String>,
    /// 会话状态（"active" / "online" / "closing"）
    pub state: String,
    /// TTY（纯终端会话）
    pub tty: Option<String>,
}

// ───────────────────────── 附加类型（系统服务 §21） ─────────────────────────

/// 窗口过滤器（用于 list_windows 命令）。
#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct WindowFilter {
    /// 按 app_id 过滤
    pub app_id: Option<String>,
    /// 按工作区编号过滤
    pub workspace: Option<u32>,
}

/// 元数据映射（通用 KV 袋，用于 D-Bus hints 等透传场景）。
pub type Metadata = HashMap<String, String>;

#[cfg(test)]
mod tests {
    use super::{TitleMatchMode, TitleMatcher};

    #[test]
    fn match_modes_positive_and_negative() {
        let cases = [
            ("Konsole", TitleMatchMode::Substring, true),
            ("GIMP", TitleMatchMode::Substring, false),
            ("终端 — Konsole", TitleMatchMode::Exact, true),
            ("终端", TitleMatchMode::Exact, false),
            ("^Unt.*Kate$", TitleMatchMode::Regex, true),
            ("^\\d+$", TitleMatchMode::Regex, false),
            ("*Document*", TitleMatchMode::Glob, true),
            ("*.pdf", TitleMatchMode::Glob, false),
        ];
        let titles = ["Untitled Document — Kate", "终端 — Konsole"];
        for (pattern, mode, want_hit) in cases {
            let matcher = TitleMatcher::new(mode, pattern);
            let hit = titles.iter().any(|t| matcher.matches(t));
            assert_eq!(
                hit, want_hit,
                "pattern {pattern:?} mode {mode:?} should be {want_hit}"
            );
        }
    }

    #[test]
    fn invalid_regex_is_no_match_not_panic() {
        let matcher = TitleMatcher::new(TitleMatchMode::Regex, "([ bad");
        assert!(!matcher.matches("终端 — Konsole"));
    }

    #[test]
    fn match_mode_serde_is_snake_case() {
        // 线格式契约：CLI 发送 lowercase，daemon 反序列化为同一枚举。
        for (mode, wire) in [
            (TitleMatchMode::Substring, "\"substring\""),
            (TitleMatchMode::Exact, "\"exact\""),
            (TitleMatchMode::Regex, "\"regex\""),
            (TitleMatchMode::Glob, "\"glob\""),
        ] {
            let encoded = serde_json::to_string(&mode).expect("serialize");
            assert_eq!(encoded, wire, "{mode:?}");
            let decoded: TitleMatchMode = serde_json::from_str(wire).expect("deserialize");
            assert_eq!(decoded, mode);
        }
    }
}
