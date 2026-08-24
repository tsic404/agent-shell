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
    KeyCombo, MonitorInfo, MouseButton, Rect, WindowId, WindowInfo, WindowState, WorkspaceId,
    WorkspaceInfo,
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
    /// X11 通用
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
}

// ───────────────────────── 事件过滤 ─────────────────────────

/// 事件过滤器（订阅者按类别筛选）。对应设计文档 §18.1。
#[derive(Clone, Debug, Default)]
pub struct EventFilter {
    /// 窗口事件
    pub window_events: bool,
    /// 工作区事件
    pub workspace_events: bool,
    /// 监视器事件
    pub monitor_events: bool,
    /// 输入事件
    pub input_events: bool,
    /// 应用生命周期事件
    pub app_events: bool,
    /// 无障碍事件
    pub a11y_events: bool,
    /// 电源事件
    pub power_events: bool,
    /// 优先级过滤（None = 全部）
    pub priority: Option<EventPriority>,
}

impl EventFilter {
    /// 匹配所有事件。
    pub const fn all() -> Self {
        Self {
            window_events: true,
            workspace_events: true,
            monitor_events: true,
            input_events: true,
            app_events: true,
            a11y_events: true,
            power_events: true,
            priority: None,
        }
    }

    /// 仅窗口事件。
    pub const fn windows_only() -> Self {
        Self {
            window_events: true,
            workspace_events: false,
            monitor_events: false,
            input_events: false,
            app_events: false,
            a11y_events: false,
            power_events: false,
            priority: None,
        }
    }

    /// 判断事件是否匹配过滤器。`Noop` 永不匹配（归一化已丢弃）。
    pub fn matches(&self, event: &DesktopEvent) -> bool {
        if let Some(p) = self.priority {
            if event.priority() != p {
                return false;
            }
        }
        match event {
            DesktopEvent::WindowOpened { .. }
            | DesktopEvent::WindowClosed { .. }
            | DesktopEvent::WindowFocused { .. }
            | DesktopEvent::WindowMoved { .. }
            | DesktopEvent::WindowStateChanged { .. }
            | DesktopEvent::WindowMetadataChanged { .. }
            | DesktopEvent::WindowStackingChanged { .. } => self.window_events,
            DesktopEvent::WorkspaceChanged { .. }
            | DesktopEvent::WorkspaceListChanged { .. }
            | DesktopEvent::WorkspaceWindowMoved { .. } => self.workspace_events,
            DesktopEvent::MonitorHotplug { .. } | DesktopEvent::MonitorChanged { .. } => {
                self.monitor_events
            }
            DesktopEvent::PointerButton { .. } | DesktopEvent::KeyComboPressed { .. } => {
                self.input_events
            }
            DesktopEvent::AppLaunched { .. } | DesktopEvent::AppExited { .. } => self.app_events,
            DesktopEvent::FullscreenChanged { .. } => self.window_events,
            DesktopEvent::PowerStateChanged { .. } => self.power_events,
            DesktopEvent::AccessibilityTreeChanged { .. } => self.a11y_events,
            DesktopEvent::Noop => false,
        }
    }
}
