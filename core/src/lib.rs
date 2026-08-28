//! agent-shell core crate.
//!
//! 提供所有模块共享的统一类型系统、组件 trait 契约、组件注册表与错误类型。
//! 本 crate 不包含任何具体 DE 后端实现，仅定义接口与数据结构。

#![forbid(unsafe_code)]
#![allow(missing_docs)]

pub mod audit;
pub mod component;
pub mod de_detection;
pub mod error;
pub mod event;
pub mod fallback;
pub mod registry;
pub mod security;
pub mod services;
pub mod types;

pub use component::{
    A11yComponent, AppearanceComponent, AudioServerComponent, BackendCapabilities,
    CaptureComponent, ClipboardComponent, ComponentHealth, ComponentType, CompositorComponent,
    DesktopComponent, DisplayLayoutComponent, InputComponent, LauncherComponent, NetworkComponent,
    NotificationComponent, PowerComponent, SessionManagerComponent, SystemComponent,
};
pub use error::AgentShellError;
pub use event::{
    DesktopEvent, EventFilter, EventHub, EventPriority, EventStream, EventSubscription,
    RawEventSource,
};
pub use registry::{BackendKind, ComponentRegistry};
pub use services::{
    AccessibilityChange, AppInfo, AppTarget, AudioDevice, AudioDeviceType, AudioState,
    BatteryState, BtDevice, ColorScheme, Connectivity, MonitorConfig, MonitorTransform,
    NetworkState, NotificationSpec, NotificationUrgency, PowerState, SystemdTimer, SystemdUnit,
    WifiNetwork,
};
pub use types::{
    CaptureTarget, DesktopEnvironment, Key, KeyCombo, KeyName, MonitorId, MonitorInfo, MouseButton,
    Rect, ScrollDelta, SemanticTarget, SessionInfo, TitleMatchMode, UnitStatus, WindowId,
    WindowInfo, WindowState, WindowType, WorkspaceId, WorkspaceInfo,
};
