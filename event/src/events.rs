//! 事件流系统（协议无关）— 统一桌面事件、EventHub fan-out、归一化与环形缓冲。
//!
//! 对应设计文档 §18（事件流系统）与架构决策 D4（§22.5）：daemon 内 EventHub
//! 单一事件网关，各 DE 原始事件经 `EventNormalizer` 归一化为 [`DesktopEvent`]
//! 后 fan-out 给订阅者；环形缓冲保留最近事件供 `--replay` 与断线补发。

#![forbid(unsafe_code)]
#![allow(missing_docs)]

use std::time::Instant;

use agent_shell_core::services::{AccessibilityChange, PowerState};
use agent_shell_core::types::{
    DesktopEnvironment, KeyCombo, MonitorInfo, MouseButton, Rect, WindowId, WindowInfo,
    WindowState, WorkspaceId, WorkspaceInfo,
};

// ───────────────────────── 事件优先级 ─────────────────────────

/// 事件优先级（决定背压时的丢弃策略）。
///
/// 对应设计文档 §18.4：High（窗口焦点/开关——必须送达）>
/// Medium（工作区切换/监视器变化/输入）> Low（其余系统事件）。
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, serde::Serialize, serde::Deserialize)]
pub enum EventPriority {
    /// 高优先级（窗口开关/聚焦——必须送达）
    High,
    /// 中优先级（工作区切换/监视器变化）
    Medium,
    /// 低优先级（音量/DPMS/外观）
    Low,
}

/// `std::time::Instant` 的 serde 桥接：以「自进程基准时刻的毫秒偏移」编码。
///
/// `Instant` 本身不可跨进程序列化（monotonic 时钟值仅本进程有意义）；
/// daemon 推送事件到 CLI/MCP 时只需相对时间序，毫秒精度足够。
/// 基准取首次调用时的 `Instant::now()`，进程内单调一致。
mod serde_instant {
    use serde::Deserialize;
    use std::sync::LazyLock;
    use std::time::{Duration, Instant};

    static EPOCH: LazyLock<Instant> = LazyLock::new(Instant::now);

    fn epoch() -> Instant {
        *EPOCH
    }

    pub(crate) fn serialize<S: serde::Serializer>(v: &Instant, s: S) -> Result<S::Ok, S::Error> {
        s.serialize_u64(v.duration_since(epoch()).as_millis() as u64)
    }

    pub(crate) fn deserialize<'de, D: serde::Deserializer<'de>>(d: D) -> Result<Instant, D::Error> {
        let ms = u64::deserialize(d)?;
        Ok(epoch() + Duration::from_millis(ms))
    }
}

// ───────────────────────── 事件源 ─────────────────────────

/// 事件源标识（用于过滤和调试）。对应设计文档 §18.1 映射表。
#[derive(Clone, Debug, PartialEq, Eq, Hash, serde::Serialize, serde::Deserialize)]
pub enum EventSource {
    /// KWin Wayland (org_kde_* 协议)
    KWinWayland,
    /// KWin X11
    KWinX11,
    /// Mutter Shell Extension
    MutterShell,
    /// Mutter Shell.Eval
    MutterEval,
    /// Mutter Extension
    MutterExtension,
    /// Hyprland (socket2)
    Hyprland,
    /// Treeland (treeland_* 协议)
    Treeland,
    /// Sway (IPC)
    Sway,
    /// WLR Wayland
    WlrWayland,
    /// DDE X11 会话事件（复用 KWin EWMH 桥，窗口 ID 打 KDE 标签）。
    ///
    /// 生产代码中仅由 `daemon/src/state.rs` 的 `dde_event_source` 构造（测试
    /// 在 `event/tests/events.rs` 另有直接构造）；通用 X11 兜底 backend
    /// （`backends/generic` 的 `X11Backend`）无原生窗口事件流，若未来接入
    /// 事件流须新增独立事件源，不得复用本变体（`de_type()` 恒为 KDE）。
    X11Generic,
    /// AT-SPI 无障碍
    AtSpi,
    /// 输入（libei）
    Input,
    /// Portal
    Portal,
    /// 电源
    Power,
}

impl EventSource {
    /// 事件源对应的桌面环境标签（`WindowId.de_type`）。
    ///
    /// 窗口事件源必然对应具体 DE：KWin 双通道标 KDE（deepin-kwin 复用
    /// org_kde 协议同口径）；X11Generic 亦标 KDE——它仅由 DDE X11 会话发出
    /// （生产代码中 `daemon/src/state.rs` 的 `dde_event_source` 为唯一构造点，
    /// 复用 KWin EWMH 桥、窗口 ID 打 KDE 标签；测试另有直接构造）。通用 X11
    /// 兜底 backend 无原生窗口事件流，若未来接入事件流须新增独立事件源，
    /// 勿复用本变体。非窗口源（AT-SPI/输入/Portal/电源）无窗口语义，
    /// 返回 `Unknown`。
    pub fn de_type(&self) -> DesktopEnvironment {
        match self {
            Self::KWinWayland | Self::KWinX11 | Self::X11Generic => DesktopEnvironment::KDE,
            Self::MutterShell | Self::MutterEval | Self::MutterExtension => {
                DesktopEnvironment::GNOME
            }
            Self::Hyprland => DesktopEnvironment::Hyprland,
            Self::Treeland => DesktopEnvironment::DDE,
            Self::Sway => DesktopEnvironment::Sway,
            Self::WlrWayland => DesktopEnvironment::WLRWayland,
            Self::AtSpi | Self::Input | Self::Portal | Self::Power => DesktopEnvironment::Unknown,
        }
    }
}

// ───────────────────────── 统一桌面事件 ─────────────────────────

/// 统一桌面事件（所有 backend 归一化后的输出）。
///
/// 对应设计文档 §18.1 全量枚举。窗口状态一律 `Vec<WindowState>`
/// （多状态可共存，非单值枚举）；每个事件携带 `source` + `occurred_at`。
///
/// serde：`occurred_at: Instant` 经 [`serde_instant`] 以
/// 「自进程基准的毫秒偏移」序列化——daemon → CLI/MCP 推送需要
/// Serialize/Deserialize（审查建议 8）；跨进程仅相对时间有意义。
#[derive(Clone, Debug, serde::Serialize, serde::Deserialize)]
#[serde(bound = "")]
pub enum DesktopEvent {
    // ========== 窗口事件（High） ==========
    /// 窗口打开
    WindowOpened {
        /// 完整窗口信息
        info: WindowInfo,
        source: EventSource,
        #[serde(with = "serde_instant")]
        occurred_at: Instant,
    },
    /// 窗口关闭
    WindowClosed {
        id: WindowId,
        source: EventSource,
        #[serde(with = "serde_instant")]
        occurred_at: Instant,
    },
    /// 窗口聚焦变化
    WindowFocused {
        info: WindowInfo,
        source: EventSource,
        #[serde(with = "serde_instant")]
        occurred_at: Instant,
    },
    /// 窗口移动/缩放（100ms 内合并，仅推送最终位置）
    WindowMoved {
        id: WindowId,
        geometry: Rect,
        source: EventSource,
        #[serde(with = "serde_instant")]
        occurred_at: Instant,
    },
    /// 窗口状态变化（最大化/最小化/全屏/置顶）
    WindowStateChanged {
        id: WindowId,
        /// 变化后的完整状态集合
        states: Vec<WindowState>,
        source: EventSource,
        #[serde(with = "serde_instant")]
        occurred_at: Instant,
    },
    /// 窗口标题/应用名变化（低频率，不合并）
    WindowMetadataChanged {
        id: WindowId,
        title: Option<String>,
        app_id: Option<String>,
        source: EventSource,
        #[serde(with = "serde_instant")]
        occurred_at: Instant,
    },
    /// 窗口层叠顺序变化（z-order，从顶到底）
    WindowStackingChanged {
        ids: Vec<WindowId>,
        source: EventSource,
        #[serde(with = "serde_instant")]
        occurred_at: Instant,
    },

    // ========== 工作区事件（Medium） ==========
    /// 工作区切换
    WorkspaceChanged {
        info: WorkspaceInfo,
        source: EventSource,
        #[serde(with = "serde_instant")]
        occurred_at: Instant,
    },
    /// 工作区增删
    WorkspaceListChanged {
        workspaces: Vec<WorkspaceInfo>,
        source: EventSource,
        #[serde(with = "serde_instant")]
        occurred_at: Instant,
    },
    /// 窗口移入/移出工作区
    WorkspaceWindowMoved {
        window_id: WindowId,
        from: Option<WorkspaceId>,
        to: Option<WorkspaceId>,
        source: EventSource,
        #[serde(with = "serde_instant")]
        occurred_at: Instant,
    },

    // ========== 监视器事件（Medium） ==========
    /// 监视器热插拔
    MonitorHotplug {
        monitor: MonitorInfo,
        /// true=接入, false=移除
        added: bool,
        source: EventSource,
        #[serde(with = "serde_instant")]
        occurred_at: Instant,
    },
    /// 监视器分辨率/缩放变化
    MonitorChanged {
        info: MonitorInfo,
        source: EventSource,
        #[serde(with = "serde_instant")]
        occurred_at: Instant,
    },

    // ========== 输入事件（Medium） ==========
    /// 鼠标点击（可映射为 WindowFocused）
    PointerButton {
        /// 点击位置下的窗口
        window_id: Option<WindowId>,
        button: MouseButton,
        pressed: bool,
        position: (i32, i32),
        source: EventSource,
        #[serde(with = "serde_instant")]
        occurred_at: Instant,
    },
    /// 键盘快捷键触发
    KeyComboPressed {
        combo: KeyCombo,
        source: EventSource,
        #[serde(with = "serde_instant")]
        occurred_at: Instant,
    },

    // ========== 系统事件（Low） ==========
    /// 应用启动
    AppLaunched {
        app_id: String,
        pid: u32,
        desktop_file: Option<String>,
        source: EventSource,
        #[serde(with = "serde_instant")]
        occurred_at: Instant,
    },
    /// 应用退出
    AppExited {
        pid: u32,
        source: EventSource,
        #[serde(with = "serde_instant")]
        occurred_at: Instant,
    },
    /// 全屏状态变化
    FullscreenChanged {
        enabled: bool,
        window_id: Option<WindowId>,
        source: EventSource,
        #[serde(with = "serde_instant")]
        occurred_at: Instant,
    },
    /// 电源状态变化（DPMS/休眠）
    PowerStateChanged {
        state: PowerState,
        source: EventSource,
        #[serde(with = "serde_instant")]
        occurred_at: Instant,
    },
    /// 无障碍树变化（仅 AT-SPI 源）
    AccessibilityTreeChanged {
        app_pid: u32,
        change_type: AccessibilityChange,
        source: EventSource,
        #[serde(with = "serde_instant")]
        occurred_at: Instant,
    },
    /// 保底/无操作（归一化丢弃）
    Noop,
}

impl DesktopEvent {
    /// 事件优先级推导（对应设计文档 §18.1 优先级表）。
    ///
    /// High：WindowOpened / WindowClosed / WindowFocused；
    /// Medium：WindowMoved / WindowStateChanged / WorkspaceChanged / WorkspaceListChanged /
    /// WorkspaceWindowMoved / MonitorHotplug / MonitorChanged / PointerButton /
    /// KeyComboPressed；Low：其余。
    pub fn priority(&self) -> EventPriority {
        match self {
            Self::WindowOpened { .. } | Self::WindowClosed { .. } | Self::WindowFocused { .. } => {
                EventPriority::High
            }
            Self::WindowMoved { .. }
            | Self::WindowStateChanged { .. }
            | Self::WorkspaceChanged { .. }
            | Self::WorkspaceListChanged { .. }
            | Self::WorkspaceWindowMoved { .. }
            | Self::MonitorHotplug { .. }
            | Self::MonitorChanged { .. }
            | Self::PointerButton { .. }
            | Self::KeyComboPressed { .. } => EventPriority::Medium,
            _ => EventPriority::Low,
        }
    }

    /// 事件来源标识。`Noop` 无来源，返回 `None`。
    pub fn source(&self) -> Option<&EventSource> {
        match self {
            Self::Noop => None,
            Self::WindowOpened { source, .. }
            | Self::WindowClosed { source, .. }
            | Self::WindowFocused { source, .. }
            | Self::WindowMoved { source, .. }
            | Self::WindowStateChanged { source, .. }
            | Self::WindowMetadataChanged { source, .. }
            | Self::WindowStackingChanged { source, .. }
            | Self::WorkspaceChanged { source, .. }
            | Self::WorkspaceListChanged { source, .. }
            | Self::WorkspaceWindowMoved { source, .. }
            | Self::MonitorHotplug { source, .. }
            | Self::MonitorChanged { source, .. }
            | Self::PointerButton { source, .. }
            | Self::KeyComboPressed { source, .. }
            | Self::AppLaunched { source, .. }
            | Self::AppExited { source, .. }
            | Self::FullscreenChanged { source, .. }
            | Self::PowerStateChanged { source, .. }
            | Self::AccessibilityTreeChanged { source, .. } => Some(source),
        }
    }

    /// 事件发生时间。`Noop` 返回 `None`。
    pub fn occurred_at(&self) -> Option<Instant> {
        match self {
            Self::Noop => None,
            Self::WindowOpened { occurred_at, .. }
            | Self::WindowClosed { occurred_at, .. }
            | Self::WindowFocused { occurred_at, .. }
            | Self::WindowMoved { occurred_at, .. }
            | Self::WindowStateChanged { occurred_at, .. }
            | Self::WindowMetadataChanged { occurred_at, .. }
            | Self::WindowStackingChanged { occurred_at, .. }
            | Self::WorkspaceChanged { occurred_at, .. }
            | Self::WorkspaceListChanged { occurred_at, .. }
            | Self::WorkspaceWindowMoved { occurred_at, .. }
            | Self::MonitorHotplug { occurred_at, .. }
            | Self::MonitorChanged { occurred_at, .. }
            | Self::PointerButton { occurred_at, .. }
            | Self::KeyComboPressed { occurred_at, .. }
            | Self::AppLaunched { occurred_at, .. }
            | Self::AppExited { occurred_at, .. }
            | Self::FullscreenChanged { occurred_at, .. }
            | Self::PowerStateChanged { occurred_at, .. }
            | Self::AccessibilityTreeChanged { occurred_at, .. } => Some(*occurred_at),
        }
    }

    /// 事件种类；`Noop` 无种类，返回 `None`。
    pub fn kind(&self) -> Option<EventKind> {
        match self {
            Self::WindowOpened { .. } => Some(EventKind::WindowOpened),
            Self::WindowClosed { .. } => Some(EventKind::WindowClosed),
            Self::WindowFocused { .. } => Some(EventKind::WindowFocused),
            Self::WindowMoved { .. } => Some(EventKind::WindowMoved),
            Self::WindowStateChanged { .. } => Some(EventKind::WindowStateChanged),
            Self::WindowMetadataChanged { .. } => Some(EventKind::WindowMetadataChanged),
            Self::WindowStackingChanged { .. } => Some(EventKind::WindowStackingChanged),
            Self::WorkspaceChanged { .. } => Some(EventKind::WorkspaceChanged),
            Self::WorkspaceListChanged { .. } => Some(EventKind::WorkspaceListChanged),
            Self::WorkspaceWindowMoved { .. } => Some(EventKind::WorkspaceWindowMoved),
            Self::MonitorHotplug { .. } => Some(EventKind::MonitorHotplug),
            Self::MonitorChanged { .. } => Some(EventKind::MonitorChanged),
            Self::PointerButton { .. } => Some(EventKind::PointerButton),
            Self::KeyComboPressed { .. } => Some(EventKind::KeyComboPressed),
            Self::AppLaunched { .. } => Some(EventKind::AppLaunched),
            Self::AppExited { .. } => Some(EventKind::AppExited),
            Self::FullscreenChanged { .. } => Some(EventKind::FullscreenChanged),
            Self::PowerStateChanged { .. } => Some(EventKind::PowerStateChanged),
            Self::AccessibilityTreeChanged { .. } => Some(EventKind::AccessibilityTreeChanged),
            Self::Noop => None,
        }
    }
}

// ───────────────────────── 事件过滤 ─────────────────────────

/// 事件种类：每个实体 [`DesktopEvent`] 变体占位图一位。
///
/// 变体声明顺序即位序，仅 `Noop` 无位；新增事件变体时同步本枚举、
/// [`DesktopEvent::kind`] 与 [`EventKind::ALL`]。
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum EventKind {
    /// 窗口打开
    WindowOpened,
    /// 窗口关闭
    WindowClosed,
    /// 窗口聚焦变化
    WindowFocused,
    /// 窗口移动/缩放
    WindowMoved,
    /// 窗口状态变化
    WindowStateChanged,
    /// 窗口标题/应用名变化
    WindowMetadataChanged,
    /// 窗口层叠顺序变化
    WindowStackingChanged,
    /// 全屏状态变化
    FullscreenChanged,
    /// 工作区切换
    WorkspaceChanged,
    /// 工作区增删
    WorkspaceListChanged,
    /// 窗口跨工作区移动
    WorkspaceWindowMoved,
    /// 监视器热插拔
    MonitorHotplug,
    /// 监视器变化
    MonitorChanged,
    /// 指针按键
    PointerButton,
    /// 组合键按下
    KeyComboPressed,
    /// 应用启动
    AppLaunched,
    /// 应用退出
    AppExited,
    /// 电源状态变化
    PowerStateChanged,
    /// 无障碍树变化
    AccessibilityTreeChanged,
}

impl EventKind {
    /// 全部实体事件种类，顺序即 `--help` 与错误文案中的枚举顺序。
    pub const ALL: [Self; 19] = [
        Self::WindowOpened,
        Self::WindowClosed,
        Self::WindowFocused,
        Self::WindowMoved,
        Self::WindowStateChanged,
        Self::WindowMetadataChanged,
        Self::WindowStackingChanged,
        Self::FullscreenChanged,
        Self::WorkspaceChanged,
        Self::WorkspaceListChanged,
        Self::WorkspaceWindowMoved,
        Self::MonitorHotplug,
        Self::MonitorChanged,
        Self::PointerButton,
        Self::KeyComboPressed,
        Self::AppLaunched,
        Self::AppExited,
        Self::PowerStateChanged,
        Self::AccessibilityTreeChanged,
    ];

    /// 全部实体事件位的并集。
    pub const ALL_MASK: u32 = {
        let mut mask = 0;
        let mut i = 0;
        while i < Self::ALL.len() {
            mask |= Self::ALL[i].bit();
            i += 1;
        }
        mask
    };

    /// 单点位。
    pub const fn bit(self) -> u32 {
        1 << self as u32
    }

    /// 与 `DesktopEvent` 变体名一致的枚举名。
    pub const fn name(self) -> &'static str {
        match self {
            Self::WindowOpened => "WindowOpened",
            Self::WindowClosed => "WindowClosed",
            Self::WindowFocused => "WindowFocused",
            Self::WindowMoved => "WindowMoved",
            Self::WindowStateChanged => "WindowStateChanged",
            Self::WindowMetadataChanged => "WindowMetadataChanged",
            Self::WindowStackingChanged => "WindowStackingChanged",
            Self::FullscreenChanged => "FullscreenChanged",
            Self::WorkspaceChanged => "WorkspaceChanged",
            Self::WorkspaceListChanged => "WorkspaceListChanged",
            Self::WorkspaceWindowMoved => "WorkspaceWindowMoved",
            Self::MonitorHotplug => "MonitorHotplug",
            Self::MonitorChanged => "MonitorChanged",
            Self::PointerButton => "PointerButton",
            Self::KeyComboPressed => "KeyComboPressed",
            Self::AppLaunched => "AppLaunched",
            Self::AppExited => "AppExited",
            Self::PowerStateChanged => "PowerStateChanged",
            Self::AccessibilityTreeChanged => "AccessibilityTreeChanged",
        }
    }

    /// 按枚举名查找；大小写敏感。
    pub fn from_name(name: &str) -> Option<Self> {
        Self::ALL.into_iter().find(|kind| kind.name() == name)
    }

    /// 所属类别（`FullscreenChanged` 归 `window`）。
    pub const fn category(self) -> EventCategory {
        match self {
            Self::WindowOpened
            | Self::WindowClosed
            | Self::WindowFocused
            | Self::WindowMoved
            | Self::WindowStateChanged
            | Self::WindowMetadataChanged
            | Self::WindowStackingChanged
            | Self::FullscreenChanged => EventCategory::Window,
            Self::WorkspaceChanged | Self::WorkspaceListChanged | Self::WorkspaceWindowMoved => {
                EventCategory::Workspace
            }
            Self::MonitorHotplug | Self::MonitorChanged => EventCategory::Monitor,
            Self::PointerButton | Self::KeyComboPressed => EventCategory::Input,
            Self::AppLaunched | Self::AppExited => EventCategory::App,
            Self::PowerStateChanged => EventCategory::Power,
            Self::AccessibilityTreeChanged => EventCategory::A11y,
        }
    }
}

/// 事件类别：过滤器的类别名 token。
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum EventCategory {
    /// 窗口事件
    Window,
    /// 工作区事件
    Workspace,
    /// 监视器事件
    Monitor,
    /// 输入事件
    Input,
    /// 应用生命周期事件
    App,
    /// 无障碍事件
    A11y,
    /// 电源事件
    Power,
}

impl EventCategory {
    /// 全部类别，顺序即 `--help` 中的类别顺序。
    pub const ALL: [Self; 7] = [
        Self::Window,
        Self::Workspace,
        Self::Monitor,
        Self::Input,
        Self::App,
        Self::A11y,
        Self::Power,
    ];

    /// 类别名 token。
    pub const fn name(self) -> &'static str {
        match self {
            Self::Window => "window",
            Self::Workspace => "workspace",
            Self::Monitor => "monitor",
            Self::Input => "input",
            Self::App => "app",
            Self::A11y => "a11y",
            Self::Power => "power",
        }
    }

    /// 按类别名查找。
    pub fn from_name(name: &str) -> Option<Self> {
        Self::ALL
            .into_iter()
            .find(|category| category.name() == name)
    }

    /// 整类掩码：该类全部事件位的并集。
    pub fn mask(self) -> u32 {
        EventKind::ALL
            .into_iter()
            .filter(|kind| kind.category() == self)
            .fold(0, |mask, kind| mask | kind.bit())
    }
}

/// 事件过滤器（订阅者按事件种类位图筛选）。对应设计文档 §18.1。
///
/// 类别 token 置整类掩码；事件枚举名 token 只置单点位——`--filter WindowOpened`
/// 只匹配该事件，不退化为整个 `window` 类别。
#[derive(Clone, Debug, Default)]
pub struct EventFilter {
    /// 选中的事件种类位图
    mask: u32,
    /// 优先级过滤（None = 全部）
    pub priority: Option<EventPriority>,
}

impl EventFilter {
    /// 匹配所有事件。
    pub fn all() -> Self {
        Self {
            mask: EventKind::ALL_MASK,
            priority: None,
        }
    }

    /// 仅窗口类别。
    pub fn windows_only() -> Self {
        Self {
            mask: EventCategory::Window.mask(),
            priority: None,
        }
    }

    /// 将单个过滤器 token 应用到自身，返回是否识别。
    ///
    /// `events.subscribe` / `events.replay` 的 `--filter` 解析复用本方法，
    /// 使 token → 掩码与 [`EventFilter::matches`] 保持单一事实来源，避免两处
    /// 枚举漂移。`all` 覆盖式重置为全量，其余 token 追加；类别名置整类，
    /// 事件枚举名只置单点。未识别返回 `false`（调用方据此上抛 InvalidParams）。
    pub fn apply_token(&mut self, token: &str) -> bool {
        if token == "all" {
            self.mask = EventKind::ALL_MASK;
            return true;
        }
        if let Some(category) = EventCategory::from_name(token) {
            self.mask |= category.mask();
            return true;
        }
        match EventKind::from_name(token) {
            Some(kind) => {
                self.mask |= kind.bit();
                true
            }
            None => false,
        }
    }

    /// 全部可用过滤器值（`--help` / 错误提示列出）。
    ///
    /// 由类别表与事件种类表派生，顺序固定：`all`、类别名、事件枚举名
    /// （按类别分组）。新增变体只改两张表，帮助文案与报错同步更新。
    pub fn valid_values() -> &'static [&'static str] {
        static VALUES: std::sync::LazyLock<Vec<&'static str>> = std::sync::LazyLock::new(|| {
            let mut values =
                Vec::with_capacity(1 + EventCategory::ALL.len() + EventKind::ALL.len());
            values.push("all");
            values.extend(EventCategory::ALL.iter().map(|category| category.name()));
            values.extend(EventKind::ALL.iter().map(|kind| kind.name()));
            values
        });
        VALUES.as_slice()
    }

    /// 判断事件是否匹配过滤器。`Noop` 永不匹配（归一化已丢弃）。
    pub fn matches(&self, event: &DesktopEvent) -> bool {
        let Some(kind) = event.kind() else {
            return false;
        };
        if let Some(p) = self.priority {
            if event.priority() != p {
                return false;
            }
        }
        self.mask & kind.bit() != 0
    }
}
