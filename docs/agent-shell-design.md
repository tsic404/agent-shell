# Agent Shell — Linux Desktop Agent Shell 设计文档

> 版本：v1.0 · 2026-08-20
> 作者：tsip404
> 状态：Draft

---

## 目录
1. [项目结构](#1-项目结构)
2. [核心类型系统](#2-核心类型系统)
3. [组件体系与装配契约](#3-组件体系与装配契约)
4. [AgentShell 核心：装配器与组件注册](#4-agentshell-核心装配器与组件注册)
5. [WaylandDisplayServer 组件](#5-waylanddisplayserver-组件协议层)
6. [X11DisplayServer 组件](#6-x11displayserver-组件协议层)
7. [KWinCompositor（KDE）](#7-kwinwindowmanagerkde)
8. [MutterCompositor（GNOME）](#8-gnomewindowmanagergnome)
9. [HyprlandCompositor](#9-hyprlandwindowmanager)
10. [DdeCompositor（Deepin）](#10-ddewindowmanagerdeepin)
11. [合成器：兜底（X11 / WLRWayland）](#11-合成器兜底x11--wlrwaylande)
12. [输入子系统](#12-输入子系统)
13. [截图与捕获子系统](#13-截图与捕获子系统)
14. [AT-SPI 无障碍模块](#14-at-spi-无障碍模块)
15. [语义路由层](#15-语义路由层)
16. [DE 检测与自动配置](#16-de-检测与自动配置)
17. [Agent 接口层](#17-agent-接口层)
18. [事件流系统](#18-事件流系统)
19. [错误处理与降级链](#19-错误处理与降级链)
20. [构建与部署](#20-构建与部署)
21. [系统服务能力层](#21-系统服务能力层)
22. [实施架构（架构决策记录）](#22-实施架构架构决策记录)
23. [进程架构与提权模型](#23-进程架构与提权模型)

---

## 1. 项目结构

```
agent-shell/
├── Cargo.toml                    # Rust workspace（members + workspace.dependencies）
├── Cargo.lock
│
├── core/                         # 核心抽象层（协议无关）
│   ├── Cargo.toml
│   └── src/
│       ├── lib.rs
│       ├── types.rs              # 统一类型定义（SemanticTarget/WindowInfo 等）
│       ├── error.rs              # 统一错误类型
│       ├── de_detection.rs       # 桌面环境检测（→ BackendKind）
│       ├── component.rs          # DesktopComponent / CompositorComponent trait
│       ├── event.rs              # EventSource 枚举（事件源标识）
│       ├── fallback.rs           # 降级链编排（FallbackChain）
│       ├── registry.rs           # ComponentRegistry 装配槽
│       ├── security.rs           # SecurityManager
│       ├── audit.rs              # 审计
│       └── services.rs           # 统一服务类型（BatteryState 等）
│
├── components/
│   ├── displayserver/            # 显示服务器协议层（协议通道基础，不实现 CompositorComponent）
│   │   ├── wayland/
│   │   │   ├── Cargo.toml
│   │   │   └── src/
│   │   │       ├── lib.rs        # WaylandDisplayServer
│   │   │       ├── registry.rs   # wl_display 连接 + registry 遍历
│   │   │       └── core_protocols.rs  # core 协议绑定接口
│   │   └── x11/
│   │       ├── Cargo.toml
│   │       └── src/
│   │           ├── lib.rs        # X11DisplayServer
│   │           ├── ewmh/mod.rs   # EWMH 原子缓存
│   │           ├── ewmh/server.rs
│   │           └── icccm/mod.rs  # ICCCM 客户端消息
│   │
│   ├── compositor/               # 合成器组件
│   │   ├── wayland-core/         # WaylandCompositor 抽象基类 trait（组合 WaylandDisplayServer）
│   │   │   ├── Cargo.toml
│   │   │   └── src/
│   │   │       ├── lib.rs
│   │   │       └── compositor.rs # pub trait WaylandCompositor: CompositorComponent
│   │   ├── wlr-wayland/          # WlrWaylandCompositor（wlr 标准协议绑定）
│   │   │   ├── Cargo.toml
│   │   │   └── src/
│   │   │       ├── lib.rs
│   │   │       ├── compositor.rs
│   │   │       └── wlr_protocols.rs  # WlrBindings 7 项 + 版本协商
│   │   ├── kwin/                 # KWinCompositor（KDE/DDE 共用，Wayland + X11 双模式）
│   │   │   ├── Cargo.toml
│   │   │   └── src/
│   │   │       ├── lib.rs
│   │   │       ├── kwin_compositor.rs  # KWinCompositor（单 struct，内部 session 路由）
│   │   │       ├── wayland.rs    # org_kde_* 私有协议
│   │   │       ├── dbus_bridge.rs    # D-Bus ↔ KWin Scripting 桥接（KWinBridge）
│   │   │       ├── scripts.rs    # 预置 KWin JS 脚本
│   │   │       ├── event_script.rs   # 长驻事件脚本管理
│   │   │       ├── version.rs    # KWin 版本探测
│   │   │       └── error.rs      # KWin 特有错误类型
│   │   ├── mutter/               # MutterCompositor（GNOME）
│   │   │   ├── Cargo.toml
│   │   │   └── src/
│   │   │       ├── lib.rs
│   │   │       ├── mutter_compositor.rs  # MutterCompositor
│   │   │       ├── eval.rs       # org.gnome.Shell.Eval 路径
│   │   │       ├── extension.rs  # Shell Extension 路径
│   │   │       ├── display_config.rs    # Mutter.DisplayConfig
│   │   │       ├── version.rs    # GNOME 版本探测
│   │   │       └── error.rs      # Mutter 特有错误类型
│   │   ├── hyprland/             # HyprlandCompositor（纯 Wayland）
│   │   │   ├── Cargo.toml
│   │   │   └── src/
│   │   │       ├── lib.rs
│   │   │       ├── compositor.rs # HyprlandCompositor
│   │   │       ├── hyprctl.rs    # hyprctl socket IPC
│   │   │       ├── event_socket.rs  # .socket2.sock 事件流
│   │   │       ├── wayland.rs    # wlr + hyprland_* 私有协议
│   │   │       ├── cache.rs      # 窗口缓存（事件流持续更新）
│   │   │       └── protocol_gen.rs   # hyprland 协议生成绑定
│   │   ├── sway/                 # SwayCompositor（纯 Wayland）
│   │   │   ├── Cargo.toml
│   │   │   └── src/
│   │   │       ├── lib.rs        # SwayCompositor
│   │   │       ├── ipc.rs        # Sway IPC（i3 兼容）
│   │   │       └── event.rs      # Sway 事件流（SwayEventStream）
│   │   └── x11/                  # X11Compositor（通用 X11 兜底）
│   │       ├── Cargo.toml
│   │       └── src/
│   │           ├── lib.rs
│   │           ├── compositor.rs # X11Compositor
│   │           ├── ewmh.rs       # EWMH 原子操作
│   │           ├── xtest.rs      # XTest 输入注入
│   │           ├── capture.rs    # MIT-SHM / XGetImage 截图
│   │           └── commands.rs   # xdotool/wmctrl 命令封装（保底）
│   │
│   ├── audio/                    # 音频：PipeWire / PulseAudio / DePriorityRouter
│   │   ├── Cargo.toml
│   │   └── src/
│   │       ├── lib.rs
│   │       ├── pipewire.rs
│   │       ├── pulseaudio.rs
│   │       └── router.rs         # DePriorityRouter（DE 封装优先路由）
│   ├── network/                  # 网络：NetworkManager / systemd-networkd
│   │   ├── Cargo.toml
│   │   └── src/
│   │       ├── lib.rs
│   │       ├── networkmanager.rs
│   │       └── networkd.rs
│   ├── input/                    # 输入：Libei / Ydotool / XTest
│   │   ├── Cargo.toml
│   │   └── src/
│   │       ├── lib.rs
│   │       ├── libei.rs
│   │       ├── ydotool.rs
│   │       ├── xdotool.rs
│   │       ├── xtest.rs
│   │       ├── keymap.rs
│   │       └── dispatcher.rs
│   ├── a11y/                     # 无障碍：AT-SPI
│   │   ├── Cargo.toml
│   │   └── src/
│   │       ├── lib.rs
│   │       ├── component.rs
│   │       ├── atspi_bridge.rs
│   │       ├── tree.rs
│   │       ├── semantic_locator.rs
│   │       └── action.rs
│   ├── clipboard/                # 剪贴板
│   │   ├── Cargo.toml
│   │   └── src/
│   │       └── lib.rs
│   ├── power/                    # 电源：UPower + login1（DE 专有实现见 backends/）
│   │   ├── Cargo.toml
│   │   └── src/
│   │       ├── lib.rs
│   │       └── router.rs
│   ├── notification/             # 通知（DE 专有实现见 backends/）
│   │   ├── Cargo.toml
│   │   └── src/
│   │       └── lib.rs
│   ├── appearance/               # 外观（DE 专有实现见 backends/）
│   │   ├── Cargo.toml
│   │   └── src/
│   │       └── lib.rs
│   ├── launcher/                 # 启动器（DE 专有实现见 backends/）
│   │   ├── Cargo.toml
│   │   └── src/
│   │       └── lib.rs
│   ├── systemd/                  # systemd D-Bus 封装
│   │   ├── Cargo.toml
│   │   └── src/
│   │       └── lib.rs
│   └── logind/                   # logind D-Bus 封装
│       ├── Cargo.toml
│       └── src/
│           └── lib.rs
│
├── backends/                     # 桌面环境（backend = 装配器）
│   ├── kde/                      # KDE backend
│   │   ├── Cargo.toml
│   │   └── src/
│   │       ├── lib.rs
│   │       ├── assemble.rs       # KdeBackend 装配清单
│   │       └── services.rs       # org.kde.* 服务路由（DE 封装优先 → 公共降级）
│   ├── dde/                      # DDE backend
│   │   ├── Cargo.toml
│   │   └── src/
│   │       ├── lib.rs
│   │       ├── assemble.rs       # DdeBackend 装配清单
│   │       ├── compositor.rs     # 合成器形态检测（Treeland vs deepin-kwin vs X11）
│   │       ├── dde_compositor.rs # DdeCompositor（复合装配）
│   │       ├── dde_api.rs        # org.deepin.dde.* 服务封装
│   │       ├── dde_audio.rs      # org.deepin.dde.Audio1 / com.deepin.daemon.Audio
│   │       ├── treeland.rs       # treeland_* 私有协议客户端
│   │       ├── protocol_gen.rs   # treeland 协议生成绑定
│   │       └── version.rs        # DDE 20 vs 25 版本兼容
│   ├── gnome/                    # GNOME backend
│   │   ├── Cargo.toml
│   │   └── src/
│   │       ├── lib.rs
│   │       ├── assemble.rs       # GnomeBackend 装配清单
│   │       └── services.rs       # org.gnome.* 服务路由
│   ├── hyprland/                 # Hyprland backend
│   │   ├── Cargo.toml
│   │   └── src/
│   │       └── lib.rs
│   ├── sway/                     # Sway backend
│   │   ├── Cargo.toml
│   │   └── src/
│   │       └── lib.rs
│   ├── generic/                  # 通用兜底 backend
│   │   ├── Cargo.toml
│   │   └── src/
│   │       └── lib.rs
│   └── tty/                      # TTY 后端
│       ├── Cargo.toml
│       └── src/
│           └── lib.rs
│
├── router/                       # 语义路由（协议无关）
│   ├── Cargo.toml
│   └── src/
│       ├── lib.rs
│       ├── command.rs
│       ├── dispatcher.rs
│       └── executor/
│           ├── mod.rs
│           ├── executor.rs
│           └── tests.rs
│
├── event/                        # 事件流系统（归一化 + 环形缓冲）
│   ├── Cargo.toml
│   ├── src/
│   │   ├── lib.rs
│   │   ├── events.rs             # DesktopEvent 统一枚举
│   │   ├── hub.rs                # EventHub
│   │   ├── adapter.rs            # RawSource trait + EventNormalizer
│   │   ├── normalize.rs          # 归一化映射
│   │   └── ring.rs               # 事件环形缓冲
│   └── tests/
│       └── events.rs
│
├── modules/capture/              # 截图：portal ScreenCast / X11
│   ├── Cargo.toml
│   └── src/
│       ├── lib.rs
│       ├── cache.rs
│       ├── portal_common.rs
│       ├── portal_screencast.rs
│       ├── portal_screenshot.rs
│       └── x11.rs
│
├── shell/                        # shell 能力（公共探测链组装）
│   ├── Cargo.toml
│   └── src/
│       └── lib.rs
│
├── rpc/                          # RPC 协议（方法常量）
│   ├── Cargo.toml
│   └── src/
│       └── lib.rs
│
├── cli/                          # CLI 入口
│   ├── Cargo.toml
│   └── src/
│       ├── main.rs
│       ├── cli.rs
│       ├── client.rs
│       ├── format.rs
│       └── repl.rs
│
├── mcp/                          # MCP 服务器
│   ├── Cargo.toml
│   └── src/
│       ├── main.rs
│       ├── lib.rs
│       └── server.rs
│
├── daemon/                       # user daemon（systemd --user 常驻）
│   ├── Cargo.toml
│   └── src/
│       ├── main.rs
│       ├── state.rs
│       ├── dispatch.rs
│       ├── a11y.rs
│       ├── capture.rs
│       ├── input.rs
│       ├── ime_session.rs
│       ├── portal_sessions.rs
│       ├── rootd_client.rs
│       └── single_instance.rs
│
├── rootd/                        # root 守护（polkit 提权通道）
│   ├── Cargo.toml
│   └── src/
│       ├── main.rs
│       ├── lib.rs                # polkit action 映射（polkit_action_for）
│       └── dbus.rs               # D-Bus 服务层
│
├── packaging/                    # 打包产物
│   └── com.agentshell.policy     # polkit policy（12+1 action）
│
└── docs/                         # 设计文档
    └── agent-shell-design.md
```

---

## 2. 核心类型系统

`core/src/types.rs` — 所有模块共享的统一类型定义：

```rust
use serde::{Deserialize, Serialize};
use std::collections::HashMap;

/// 全局唯一窗口标识
#[derive(Clone, Debug, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub struct WindowId {
    pub native_id: String,
    pub de_type: DesktopEnvironment,
}

/// 工作区标识
#[derive(Clone, Debug, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub struct WorkspaceId {
    pub native_id: String,
    pub de_type: DesktopEnvironment,
}

/// 监视器标识
#[derive(Clone, Debug, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub struct MonitorId {
    pub native_id: String,
    pub de_type: DesktopEnvironment,
}

/// 矩形区域
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct Rect {
    pub x: i32,
    pub y: i32,
    pub width: i32,
    pub height: i32,
}

/// 窗口信息
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct WindowInfo {
    pub id: WindowId,
    pub title: String,
    pub app_id: String,
    pub pid: u32,
    pub geometry: Rect,
    pub frame_geometry: Rect,
    pub states: Vec<WindowState>,  // 多状态可共存（如：Maximized + FullScreen）
    pub workspace_id: Option<WorkspaceId>,
    pub monitor_id: Option<MonitorId>,
    pub stacking_order: u32,
    pub desktop_file: Option<String>,
    pub window_type: WindowType,
    pub icon_geometry: Option<Rect>,
    pub keep_above: bool,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub enum WindowState { Normal, Minimized, Maximized, FullScreen, Hidden }

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub enum WindowType { Normal, Dialog, Dock, Desktop, DropdownMenu, Tooltip,
    Notification, Splash, Utility, Unknown }

/// 工作区信息
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct WorkspaceInfo {
    pub id: WorkspaceId,
    pub name: String,
    pub number: u32,
    pub is_active: bool,
    pub monitor_ids: Vec<MonitorId>,
    pub window_ids: Vec<WindowId>,
}

/// 监视器信息
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct MonitorInfo {
    pub id: MonitorId,
    pub name: String,
    pub geometry: Rect,             // 逻辑几何（Wayland: 缩放后坐标; X11: 物理坐标）
    pub physical_geometry: Rect,    // 物理像素尺寸（原始分辨率）
    pub scale: f64,                 // 缩放因子（Wayland: 1-2; X11: 1）
    pub is_primary: bool,
    pub workspace_id: Option<WorkspaceId>,
}

/// 桌面环境枚举
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub enum DesktopEnvironment {
    KDE, GNOME, Hyprland, DDE, Sway, Budgie, XFCE, Cinnamon, Cosmic, LXQt, MATE,
    WLRWayland, X11Generic, Tty, Unknown,
}

/// 按键组合
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct KeyCombo {
    pub keys: Vec<Key>,
    pub modifiers: ModifierMask,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub enum Key { Char(char), Named(KeyName) }

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub enum KeyName {
    Return, Escape, BackSpace, Tab, Space,
    Left, Right, Up, Down, Home, End, PageUp, PageDown,
    Insert, Delete, Menu,
    F1, F2, F3, F4, F5, F6, F7, F8, F9, F10, F11, F12,
    // ... 完整键名
}

#[derive(Clone, Copy, Debug, Default, Serialize, Deserialize)]
pub struct ModifierMask { pub ctrl: bool, pub alt: bool, pub shift: bool, pub meta: bool }

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub enum MouseButton { Left, Middle, Right, Back, Forward }

/// 鼠标滚动
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct ScrollDelta { pub dx: i32, pub dy: i32 }

/// 语义定位目标
#[derive(Clone, Debug, Serialize, Deserialize)]
pub enum SemanticTarget {
    ById(WindowId),
    ByAppId(String),
    ByTitle(String, TitleMatchMode),
    ByPid(u32),
    ByDesktopFile(String),
    ByAccessibility { role: Option<String>, name: Option<String>,
        parent_role: Option<String>, parent_name: Option<String> },
    Active,
    ByCoordinate { x: i32, y: i32 },
    ByRegion(Rect),
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub enum TitleMatchMode { Substring, Exact, Regex, Glob }

/// 截图目标
#[derive(Clone, Debug, Serialize, Deserialize)]
pub enum CaptureTarget {
    Screen,
    Monitor(MonitorId),
    Window(WindowId),
    Area(Rect),
}

/// 统一错误
#[derive(Debug, thiserror::Error)]
pub enum AgentShellError {
    #[error("DE not supported: {0}")] UnsupportedDE(String),
    #[error("Backend not available: {0}")] BackendUnavailable(String),
    #[error("Window not found: {0}")] WindowNotFound(String),
    #[error("D-Bus error: {0}")] DBus(#[from] zbus::Error),
    #[error("Input backend error: {0}")] Input(String),
    #[error("Capture error: {0}")] Capture(String),
    #[error("Permission denied: {0}")] Permission(String),
    #[error("Timeout: {0}")] Timeout(String),
    #[error("Not implemented: {0}")] NotImplemented(String),
    #[error(transparent)] Other(#[from] Box<dyn std::error::Error + Send + Sync>),
}

/// 系统服务单元状态（systemd）
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub enum UnitStatus {
    Active, Reloading, Inactive, Failed, Activating, Deactivating, Unknown,
}

/// 登录会话信息（logind）
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct SessionInfo {
    pub id: String,
    pub uid: u32,
    pub user_name: String,
    pub seat: String,
    pub display: String,
    pub remote: bool,
    pub remote_host: Option<String>,
    pub state: String,  // "active" / "online" / "closing"
    pub tty: Option<String>,
}
```


---

## 3. 公共组件体系与 Backend 装配契约

agent-shell 的项目结构分**两个抽象层次**：

```
┌─────────────────────────────────────────────────────────────┐
│  AgentShell（顶层装配器）                                      │
│  ├── core/          核心：类型系统、D-Bus、EventHub、Router      │
│  │                                                            │
│  ├── components/    公共组件层（抽象接口 + 具体实例）             │
│  │   ├── compositor/     合成器：KWin / Mutter / Hyprland /     │
│  │   │                      Treeland / Sway / X11 / WLRWayland│
│  │   ├── audio/          音频：PipeWire / PulseAudio            │
│  │   ├── network/        网络：NetworkManager / systemd-networkd │
│  │   ├── input/          输入：Libei / Ydotool / XTest          │
│  │   ├── capture/        截图：portal ScreenCast / X11          │
│  │   ├── a11y/           无障碍：AT-SPI                         │
│  │   ├── clipboard/      剪贴板：wl-clipboard / xclip           │
│  │   ├── power/          电源：UPower / DE 专有                  │
│  │   ├── notification/   通知：DE 专有 / portal                  │
│  │   ├── appearance/     外观：DE 专有 / portal                  │
│  │   └── launcher/       启动器：DE 专有 / portal                │
│  │                                                            │
│  └── backends/       桌面环境（backend = 装配器）                │
│      ├── kde/            KDE：kwin + networkmanager + pipewire │
│      ├── dde/            DDE：kwin(deepin-kwin) + dde-api D-Bus│
│      ├── gnome/          GNOME：mutter + ... (需实际调研)       │
│      ├── hyprland/       Hyprland：hyprland + ...              │
│      ├── sway/           Sway：sway + ...                      │
│      ├── generic/        通用兜底                               │
│      └── tty/            TTY（纯终端后端，仅系统服务）             │
└─────────────────────────────────────────────────────────────┘
```

**核心原则**：
1. **网络/音频/合成器/输入/截图等是公共组件**（`components/`），各自抽象接口 + 多个具体实例
2. **backend 是桌面环境**，不是与组件平级的另一套实现——它是**装配器**，按对应桌面环境**初始化公共组件中的具体实例**
3. 同一公共组件可为多个 DE 复用：KWin 合成器由 KDE 和 DDE 共用，PipeWire 音频所有 DE 共用
4. **DE 封装优先**：对某个具体能力的调用，**一律优先走 DE 封装的服务接口**（如 KDE 的 `org.kde.*`、DDE 的 `org.deepin.dde.*`/dde-api），**DE 没有对应接口时才回退到公共服务接口**（portal / freedesktop / 公共组件通用实例）。backend 的 services.rs 封装就是这条路由的实现

### 3.1 公共组件类型总表

| 组件 | 抽象接口 | 具体实例 | 运行时探测 |
|------|---------|---------|-----------|
| 合成器 | `CompositorComponent` | KWinCompositor / MutterCompositor / HyprlandCompositor / TreelandCompositor / SwayCompositor / X11Compositor / WlrWaylandCompositor | DE 检测 + 会话类型 |
| 音频服务器 | `AudioServerComponent` | PipeWireAudioServer / PulseAudioAudioServer | 进程存在 |
| 网络服务 | `NetworkComponent` | NetworkManagerComponent / SystemdNetworkdComponent | `org.freedesktop.NetworkManager` |
| 输入服务 | `InputComponent` | LibeiInput / YdotoolInput / XTestInput | portal / /dev/uinput / XTest |
| 截图服务 | `CaptureComponent` | PipeWireScreenCast / ScreenshotPortal / X11Capture | portal / PipeWire / X11 |
| 无障碍 | `A11yComponent` | AtSpiComponent | `org.a11y.atspi.Registry` |
| 剪贴板 | `ClipboardComponent` | WlClipboard / XClipboard | 会话类型 |
| 电源管理 | `PowerComponent` | 公共：UPowerComponent（`components/power/`）<br>DE 专有：KdePowerDevil（`org.kde.Solid.PowerManagement`）<br>DdePower（`org.deepin.dde.Power1`，system bus）<br>GnomePower（`org.gnome.SettingsDaemon.Power`） | 对应 D-Bus 服务 |
| 通知服务 | `NotificationComponent` | 公共：PortalNotification（`components/notification/`）<br>DE 专有：KdeNotification / DdeNotification / GnomeNotification（`backends/{kde,dde,gnome}/`） | 对应 D-Bus 服务 |
| 外观设置 | `AppearanceComponent` | 公共：PortalAppearance（`components/appearance/`）<br>DE 专有：KdeAppearance / DdeAppearance / GnomeAppearance（`backends/{kde,dde,gnome}/`） | 对应 D-Bus 服务 |
| 应用启动器 | `LauncherComponent` | 公共：PortalLauncher（`components/launcher/`）<br>DE 专有：KdeLauncher / DdeLauncher / GnomeLauncher（`backends/{kde,dde,gnome}/`） | 对应 D-Bus 服务 |
| 系统服务 | `SystemComponent` | SystemdComponent（`components/systemd/`） | `org.freedesktop.systemd1` |
| 会话管理 | `SessionManagerComponent` | LogindComponent（`components/logind/`） | `org.freedesktop.login1` |

### 3.2 合成器选择矩阵（按 DE × 会话）

| DE | Wayland 合成器 | X11 合成器 | 说明 |
|----|---------------|-----------|------|
| KDE | `KWinCompositor`（org_kde_* 私有协议） | `KWinCompositor`（EWMH + org.kde.KWin D-Bus） | 合成器 = 合成器一体 |
| DDE（当前） | `KWinCompositor`（deepin-kwin，复用 KWin 路径） | `KWinCompositor` + dde-api D-Bus | 界面差异由 dde-api 封装吸收 |
| DDE（未来） | `TreelandCompositor`（treeland_* 私有协议） | — | Treeland 迁移完成后弃 org.deepin.dde.* |
| GNOME | `MutterCompositor`（D-Bus Eval/Extension） | `MutterCompositor`（EWMH + GNOME D-Bus） | Mutter 无 Wayland 私有协议 |
| Hyprland | `HyprlandCompositor`（hyprland_* + wlr） | — | 仅 Wayland |
| Sway | `SwayCompositor`（wlr + Sway IPC） | — | 仅 Wayland |
| X11 通用 | — | `X11Compositor`（EWMH/XTest 原生） | 无法识别 DE |
| 未知 compositor | `WlrWaylandCompositor`（wlr + portal 降级） | — | 保底 |
| TTY（无 DE） | — | — | 纯终端环境，无合成器/窗口管理 |

**合成器内部协议通道**：

| 合成器 | 内部组合 | 基础通道 | 补充通道 |
|:---|:----------|:---------|:---------|
| KWinCompositor | WaylandDisplayServer + X11DisplayServer | org_kde_* 私有协议 | D-Bus / Scripting |
| MutterCompositor | WaylandDisplayServer + X11DisplayServer | D-Bus Eval/Extension | AT-SPI |
| HyprlandCompositor | WaylandDisplayServer | wlr + hyprland_* 私有协议 | hyprctl socket IPC |
| Treeland（backends/dde） | WaylandDisplayServer | treeland_* 私有协议（future D9） | dde-api D-Bus |
| SwayCompositor | WaylandDisplayServer | wlr 标准协议 | Sway IPC |
| X11Compositor | X11DisplayServer | EWMH/XTest 原生 | xdotool 保底 |
| WlrWaylandCompositor | WaylandDisplayServer | wlr 标准协议 | portal/AT-SPI 降级 |

### 3.3 Backend 装配矩阵（桌面环境 → 公共组件实例）

每个 backend 定义「装配清单」——按 DE 初始化公共组件中哪些具体实例。下表为**设计意图**，具体实例需实际环境调研确认：

**合成器继承层次（均实现 `CompositorComponent`）**：

```
CompositorComponent（trait——所有合成器统一接口）
├── WaylandCompositor（抽象类，组合 WaylandDisplayServer）
│   ├── WlrWaylandCompositor（wlroots 系基类，自身完整实现；子类叠加私有协议）
│   │   ├── Treeland 协议客户端（backends/dde/src/treeland.rs，future D9，非独立 compositor crate）
│   │   ├── HyprlandCompositor（+ hyprland_* 私有协议）
│   │   └── SwayCompositor（+ Sway IPC）
│   ├── KWinCompositor（直接继承，org_kde_* 私有协议）
│   └── MutterCompositor（直接继承，D-Bus Eval/Extension）
└── X11Compositor（直接实现 CompositorComponent，组合 X11DisplayServer）
```

`WaylandCompositor` 是 Wayland 系合成器共用的中间抽象层（组合 `WaylandDisplayServer` 提供通用协议绑定），
`X11Compositor` 组合 `X11DisplayServer` 提供 EWMH/XTest 协议绑定，无需中间层。

| 能力 | KDE | DDE | GNOME | Hyprland | Sway | X11Generic | WLRWayland | TTY |
|------|:---:|:---:|:-----:|:--------:|:----:|:----------:|:----------:|:---:|
| 合成器 | KWinCompositor | KWinCompositor(deepin-kwin) / Treeland(未来) | MutterCompositor | HyprlandCompositor | SwayCompositor | X11Compositor | WlrWaylandCompositor | None |
| 音频 | PipeWireAudioServer | PipeWireAudioServer | PipeWireAudioServer | PipeWireAudioServer | PipeWireAudioServer | PulseAudio / PipeWire | PipeWireAudioServer | None |
| 网络 | NetworkManager | org.deepin.dde.Network / NetworkManager | NetworkManager | NetworkManager | NetworkManager | NetworkManager | NetworkManager | systemd-networkd |
| 输入 | Input 探测链 | Input 探测链 | Input 探测链 | Input 探测链 | Input 探测链 | XTest 原生 | libei / ydotool | None |
| 截图 | Capture 探测链 | Capture 探测链 | Capture 探测链 | Capture 探测链 | Capture 探测链 | MIT-SHM 原生 | portal ScreenCast | None |
| 电源 | KdePowerDevil | DdePower | GnomePower | UPower | UPower | UPower | UPower | UPower |
| 通知 | KdeNotification | DdeNotification | GnomeNotification | PortalNotification | PortalNotification | DE 通知 | PortalNotification | None |
| 外观 | KdeAppearance | DdeAppearance | GnomeAppearance | PortalAppearance | PortalAppearance | None | PortalAppearance | None |
| 启动器 | KdeLauncher | DdeLauncher | GnomeLauncher | PortalLauncher | PortalLauncher | None | PortalLauncher | None |
| 无障碍 | AtSpiComponent | AtSpiComponent | AtSpiComponent | AtSpiComponent | AtSpiComponent | AtSpiComponent | AtSpiComponent | None |
| 剪贴板 | wl-clipboard / xclip | wl-clipboard / xclip | wl-clipboard / xclip | wl-clipboard | wl-clipboard | xclip | wl-clipboard | None |
| 系统服务 | SystemdComponent | SystemdComponent | SystemdComponent | SystemdComponent | SystemdComponent | SystemdComponent | SystemdComponent | SystemdComponent |
| 会话管理 | LogindComponent | LogindComponent | LogindComponent | LogindComponent | LogindComponent | LogindComponent | LogindComponent | LogindComponent |

**调用优先级（DE 封装优先）**：

```
能力：查询音量
  KDE backend    → org.kde.* / KMix D-Bus（存在？）──→ 直接使用
                   └─ 不存在 → PulseAudio/PipeWire 公共组件（PulseAudioAudioServer）
  DDE backend    → org.deepin.dde.Audio1（存在？）──→ 直接使用
                   └─ 不存在 → PulseAudio/PipeWire 公共组件
  GNOME backend  → org.gnome.SettingsDaemon.Audio（存在？）──→ 直接使用
                   └─ 不存在 → PulseAudio/PipeWire 公共组件

能力：窗口列表
  KDE backend    → KWinCompositor 的 org_kde_* 协议 ──→ 直接使用
                   └─ 协议不可用 → KWin Scripting D-Bus → AT-SPI 降级
  DDE backend    → DdeApi 封装的合成器接口 / treeland_* → KWin 复用
                   └─ 不存在 → WlrWaylandCompositor 兜底实例(wlr) → portal
  Hyprland       → HyprlandCompositor 的 wlr + hyprland_* 协议 ──→ 直接使用
                   └─ 协议不可用 → hyprctl socket IPC
  Sway           → SwayCompositor 的 wlr 协议 + Sway IPC ──→ 直接使用
  X11Generic     → X11Compositor 的 EWMH/XTest 原生 ──→ 直接使用
                   └─ 原生失败 → xdotool/wmctrl 保底
  WLRWayland     → WlrWaylandCompositor 的 wlr 协议 ──→ 直接使用
                   └─ wlr 协议不可用 → portal/AT-SPI 降级
  TTY            → 无合成器，窗口操作不可用
```

每条 backend 的 `services.rs` / `dde_api.rs` 就是这个优先级路由的实现：先探测 DE 封装服务，有则用，无则回退公共组件。

### 3.4 Component trait

```rust
use async_trait::async_trait;

/// 组件类型（公共组件抽象）
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ComponentType {
    Compositor, AudioServer, Network, Input, Capture,
    A11y, Clipboard, Power, Notification, Appearance, Launcher,
    InitSystem, SessionManager, SystemdService,
}

/// 组件健康状态
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum ComponentHealth {
    Healthy,           // 正常运行
    Degraded(String),  // 降级运行（如：协议部分绑定失败，回退 Scripting）
    Unavailable,       // 组件不可用（如：Wayland 下无 X11）
}

/// 公共组件统一接口
#[async_trait]
pub trait DesktopComponent: Send + Sync {
    fn name(&self) -> &'static str;
    fn component_type(&self) -> ComponentType;
    fn is_available(&self) -> bool;
    async fn health(&self) -> ComponentHealth;
}

/// 系统服务组件接口（init 系统、session 管理等）
/// 所有 Linux 环境（含 TTY）均有，是唯一"必选"组件族
#[async_trait]
pub trait SystemComponent: DesktopComponent {
    async fn daemon_reload(&self) -> Result<(), AgentShellError>;
    async fn list_units(&self) -> Result<Vec<SystemdUnit>, AgentShellError>;
    async fn start_unit(&self, name: &str) -> Result<(), AgentShellError>;
    async fn stop_unit(&self, name: &str) -> Result<(), AgentShellError>;
    async fn enable_unit(&self, name: &str) -> Result<(), AgentShellError>;
    async fn disable_unit(&self, name: &str) -> Result<(), AgentShellError>;
    async fn unit_status(&self, name: &str) -> Result<UnitStatus, AgentShellError>;
}

/// 会话管理器组件接口（logind/elogind）
#[async_trait]
pub trait SessionManagerComponent: DesktopComponent {
    async fn list_sessions(&self) -> Result<Vec<SessionInfo>, AgentShellError>;
    async fn get_session(&self, id: &str) -> Result<SessionInfo, AgentShellError>;
    async fn lock_session(&self, id: &str) -> Result<(), AgentShellError>;
    async fn unlock_session(&self, id: &str) -> Result<(), AgentShellError>;
    async fn can_reboot(&self) -> Result<bool, AgentShellError>;
    async fn reboot(&self) -> Result<(), AgentShellError>;
    async fn can_poweroff(&self) -> Result<bool, AgentShellError>;
    async fn poweroff(&self) -> Result<(), AgentShellError>;
}

/// 合成器组件接口
#[async_trait]
pub trait CompositorComponent: DesktopComponent {
    fn capabilities(&self) -> BackendCapabilities;
    async fn list_windows(&self) -> Result<Vec<WindowInfo>, AgentShellError>;
    async fn get_active_window(&self) -> Result<Option<WindowInfo>, AgentShellError>;
    async fn focus_window(&self, id: &WindowId) -> Result<(), AgentShellError>;
    async fn move_window(&self, id: &WindowId, x: i32, y: i32) -> Result<(), AgentShellError>;
    async fn resize_window(&self, id: &WindowId, w: i32, h: i32) -> Result<(), AgentShellError>;
    async fn minimize_window(&self, id: &WindowId) -> Result<(), AgentShellError>;
    async fn unminimize_window(&self, id: &WindowId) -> Result<(), AgentShellError>;
    async fn maximize_window(&self, id: &WindowId) -> Result<(), AgentShellError>;
    async fn close_window(&self, id: &WindowId) -> Result<(), AgentShellError>;
    async fn set_window_geometry(&self, id: &WindowId, geo: Rect) -> Result<(), AgentShellError>;
    async fn get_window_info(&self, id: &WindowId) -> Result<WindowInfo, AgentShellError>;
    async fn list_workspaces(&self) -> Result<Vec<WorkspaceInfo>, AgentShellError>;
    async fn activate_workspace(&self, id: &WorkspaceId) -> Result<(), AgentShellError>;
    async fn move_window_to_workspace(&self, wid: &WindowId, ws: &WorkspaceId) -> Result<(), AgentShellError>;
    async fn list_monitors(&self) -> Result<Vec<MonitorInfo>, AgentShellError>;
    async fn subscribe(&self) -> Result<Box<dyn EventStream>, AgentShellError>;
}

/// 能力掩码
#[derive(Clone, Copy, Debug)]
pub struct BackendCapabilities {
    pub window_management: bool, pub workspace_management: bool,
    pub monitor_layout: bool, pub window_events: bool, pub workspace_events: bool,
    pub native_input: bool, pub native_capture: bool, pub virtual_desktops: bool,
    pub effects_control: bool,
}
```

### 3.5 公共组件注册表（装配结果）

```rust
/// AgentShell 持有：公共组件装配结果 + 当前 backend 类型
/// 所有组件均为可选——TTY 模式无合成器/音频/输入/截图等 DE 组件，
/// 仅 init_system 和 session_manager 等系统服务可用。
pub struct ComponentRegistry {
    // 合成器（有 DE 时才存在，TTY 模式为 None）
    pub compositor: Option<Box<dyn CompositorComponent>>,
    // 音频（有音频服务器时才存在）
    pub audio: Option<Box<dyn AudioServerComponent>>,
    // 网络
    pub network: Option<Box<dyn NetworkComponent>>,
    // 输入
    pub input: Option<Box<dyn InputComponent>>,
    // 截图
    pub capture: Option<Box<dyn CaptureComponent>>,
    // 无障碍
    pub a11y: Option<Box<dyn A11yComponent>>,
    // 剪贴板
    pub clipboard: Option<Box<dyn ClipboardComponent>>,
    // 电源管理
    pub power: Option<Box<dyn PowerComponent>>,
    // 通知
    pub notification: Option<Box<dyn NotificationComponent>>,
    // 外观
    pub appearance: Option<Box<dyn AppearanceComponent>>,
    // 启动器
    pub launcher: Option<Box<dyn LauncherComponent>>,
    // 初始化系统（systemd，任何 Linux 环境都有）
    pub init_system: Option<Box<dyn SystemComponent>>,
    // 会话管理器（logind/elogind）
    pub session_manager: Option<Box<dyn SessionManagerComponent>>,
}

/// Backend 类型（当前处于哪个桌面环境）
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum BackendKind {
    Kde, Dde, Gnome, Hyprland, Sway, X11Generic, WLRWayland, Tty,
}
```

### 3.5 接口调研记录（2026-08 联网核对）

> 以下接口名与归属均基于上游源码/D-Bus 服务定义核对，作为装配矩阵的依据。**带「需真机确认」的项**为归属尚待核实（对应 DE 实测时确认）。

#### DDE（deepin-daemon 6.x，DDE 23/25）

- **命名迁移**：DDE 20 时代大量接口为 `com.deepin.daemon.*` 旧名；DDE 25（deepin-daemon 6.x，2026-07 版本 6.1.104）**已统一为 `org.deepin.dde.*`**。
- **会话总线（session bus）提供**：`org.deepin.dde.Audio1`（Audio1 服务）、`org.deepin.dde.InputDevices1`、`org.deepin.dde.Bluetooth1`、`org.deepin.dde.XEventMonitor1`、`org.deepin.dde.SystemInfo1`、`org.deepin.dde.SoundEffect1`、`org.deepin.dde.Search1`、`org.deepin.dde.Timedate1`、`org.deepin.dde.Zone1`、`org.deepin.dde.LangSelector1`、`org.deepin.dde.SessionWatcher1`。
- **系统总线（system bus）提供**：`org.deepin.dde.Display1`、`org.deepin.dde.Power1`、`org.deepin.dde.LockService1`、`org.deepin.dde.Accounts1`、`org.deepin.dde.Greeter1`、`org.deepin.dde.Gesture1`、`org.deepin.dde.Grub2`、`org.deepin.dde.BacklightHelper1`、`org.deepin.dde.AirplaneMode1`、`org.deepin.dde.Timedate1`、`org.deepin.dde.Daemon1`。
- **注意**：`dde-daemon` 包内**没有** `Appearance1`（外观由 `org.deepin.dde.Appearance1` 提供，但服务文件在 dde-session-shell 等其它包）❓需真机确认；**没有** `Network1`（网络使用 `dde-network-core` + NetworkManager）❓需真机确认；**没有** `Notification1` / `Application1` ❓需真机确认。
- polkit 策略存在 `org.deepin.dde.display.policy` / `power.policy` / `lockservice.policy`，确认了 display/power/lock 属于需要系统级授权的操作。

#### KDE / Plasma 6（KWin 6）

- **电源**：`org.kde.Solid.PowerManagement`（KDE/powerdevil）——含 `refreshStatus` / `backendCapabilities` / `loadProfile` / `currentProfile` / `batteryRemainingTime`；子接口 `Actions.PowerProfile`（`currentProfile` / `setProfile`）、`Actions.SuspendSession`（`suspendToRam` / `suspendToDisk` / `suspendHybrid`）、`Actions.BrightnessControl`（`setBrightness` / `brightness` / `brightnessMax`）、`Actions.HandleButtonEvents`、`Actions.KeyboardBrightnessControl`。
- **锁屏**：bus `org.kde.screensaver`（KDE/kscreenlocker），接口 `org.kde.screensaver`（`configure` + 信号 `AboutToLock`）。
- **窗口/合成器脚本**：`org.kde.KWin` → `/Scripting` → `org.kde.kwin.Scripting`（`loadScript` / `start` / `loadedScripts`）。KWin 6 全局对象：`workspace` / `windowList()` / `activeWindow` / `windowAdded` 信号 / `registerShortcut()` / `callDBus()`；窗口属性 `resourceClass` / `resourceName` / `internalId`（UUID）/ `geometry` / `minimized` / `desktops`（VirtualDesktop 数组，KWin 6 不再用 int）。
- **通知**：KDE 后端是 `org.freedesktop.Notifications` 的 KDE 实现（`xdg-desktop-portal-kde` 也转发同一总线）。
- **Portal 支持**：`xdg-desktop-portal-kde` 支持全部接口（除 Secret），即 Notification/Screenshot/ScreenCast/Clipboard/Settings/Wallpaper/DynamicLauncher/GlobalShortcuts/InputCapture 等全部可用。

#### GNOME / Mutter（GNOME 45+）

- **显示配置**：`org.gnome.Mutter.DisplayConfig`（Mutter D-Bus 接口）——`GetCurrentState`（返回 serial/monitors/logical_monitors/properties）与 `ApplyMonitorsConfig`（serial/method=0 verify|1 temporary|2 persistent/logical_monitors/properties）。方法 `ApplyMonitorsConfig` 真实签名：`a(iiduba(ssa{sv}))` + `a{sv}`。
- **电源**：`org.gnome.SettingsDaemon.Power`（gnome-settings-daemon，D-Bus name = `org.gnome.SettingsDaemon.Power`，path `/org/gnome/SettingsDaemon`）。
- **锁屏**：`org.gnome.ScreenSaver`。
- **音频/音量**：**GNOME 没有 DE 层音量 D-Bus 接口**（gnome-settings-daemon 只有 media-keys 处理快捷键、sound 插件走 PulseAudio/WirePlumber）——音量控制一律直接走公共组件（PipeWire/PulseAudio）。
- **Shell Eval**：`org.gnome.Shell` → `/org/gnome/Shell` → `org.gnome.Shell.Eval`（需开启允许 eval）。
- **Portal 支持**：`xdg-desktop-portal-gnome` 覆盖全部常用接口（含 InputCapture/RemoteDesktop/Screenshot/ScreenCast/Clipboard/Notification/Wallpaper，无 Secret）。

#### Hyprland / wlroots

- **协议**：wlr 标准协议（foreign-toplevel/wlr-virtual-pointer/wlr-screencopy）+ hyprland 私有协议；hyprctl socket IPC（unix socket `$XDG_RUNTIME_DIR/hypr/{HYPRLAND_INSTANCE_SIGNATURE}/.socket.sock`）+ 事件 socket `.socket2.sock`。
- **Portal**：`xdg-desktop-portal-hyprland` 只支持 **GlobalShortcuts + ScreenCast + Screenshot** 三个接口；其余能力（通知/剪贴板/外观等）需公共组件或其它 portal（GTK）兜底。

#### freedesktop portal（跨 DE 公共降级）

| portal 接口 | 能力 | KDE | DDE | GNOME | Hyprland |
|---|---|---|---|---|---|
| `org.freedesktop.portal.Notification` | 通知 | ✓ | ✓ | ✓ | ✗ |
| `org.freedesktop.portal.Screenshot` | 截图 | ✓ | ✓ | ✓ | ✓ |
| `org.freedesktop.portal.ScreenCast` | 录屏流 | ✓ | ✓ | ✓ | ✓ |
| `org.freedesktop.portal.Clipboard` | 剪贴板 | ✓ | ✗ | ✓ | ✗ |
| `org.freedesktop.portal.Settings` | 外观/设置 | ✓ | ✓ | ✓ | ✗ |
| `org.freedesktop.portal.Wallpaper` | 壁纸 | ✓ | ✓ | ✓ | ✗ |
| `org.freedesktop.portal.DynamicLauncher` | 应用启动器 | ✓ | ✗ | ✓ | ✗ |
| `org.freedesktop.portal.GlobalShortcuts` | 全局快捷键 | ✓ | ✓ | ✓ | ✓ |
| `org.freedesktop.portal.InputCapture` | 输入捕获 | ✓ | ✓ | ✓ | ✗ |

> 依据：ArchWiki「XDG Desktop Portal」后端支持表（2026-08）；上游源码 `KDE/powerdevil`、`KDE/kscreenlocker`、`GNOME/gnome-settings-daemon`、`linuxdeepin/dde-daemon` 的 D-Bus XML 定义。

---

## 4. AgentShell 核心：公共组件 + Backend 装配

`AgentShell` 是顶层 struct，启动时**检测桌面环境 → 选定 backend → 按 backend 装配清单初始化公共组件实例**。

### 4.1 AgentShell 结构

```rust
pub struct AgentShell {
    // 核心基础设施
    dbus: DBusManager,               // D-Bus 连接管理（session + system）
    event_hub: EventHub,             // 事件枢纽（所有组件事件归一化）

    // 当前 backend（桌面环境）
    backend: BackendKind,            // Kde / Dde / Gnome / Hyprland / Sway / X11Generic / WLRWayland / Tty

    // 公共组件装配结果
    components: ComponentRegistry,
}
```

### 4.2 装配流程

```
AgentShell::detect_and_assemble()
  │
  ├─ 1. 检测环境：XDG_SESSION_TYPE × XDG_CURRENT_DESKTOP → BackendKind
  │
  ├─ 2. 选定 backend（桌面环境）——决定公共组件实例的组合
  │
  ├─ 3. 按 backend 装配清单初始化公共组件：
  │
  │    合成器（按 BackendKind × 会话类型）：
  │      Kde         → KWinCompositor
  │      Dde         → KWinCompositor(deepin-kwin) / TreelandCompositor(未来)
  │      Gnome       → MutterCompositor
  │      Hyprland    → HyprlandCompositor
  │      Sway        → SwayCompositor
  │      X11Generic  → X11Compositor
  │      WLRWayland → WlrWaylandCompositor
  │
  │    音频服务器（公共组件，按探测）：
  │      pipewire 运行？  → PipeWireAudioServer
  │      pulseaudio 运行？→ PulseAudioAudioServer
  │
  │    网络（公共组件，按探测）：
  │      NetworkManager D-Bus？→ NetworkManagerComponent
  │      （DDE backend 可改用 dde 网络服务——需调研）
  │
  │    输入 / 截图 / 无障碍 / 剪贴板（公共组件，探测链）：
  │      input    libei portal → ydotool → XTest
  │      capture  portal ScreenCast → Screenshot → X11
  │      a11y     org.a11y.atspi.Registry 存在？
  │      clipboard 会话类型 → wl-clipboard / xclip
  │
  │    电源 / 通知 / 外观 / 启动器（公共组件，DE 专有实例）：
  │      按 BackendKind 选对应实例（Kde/Dde/Gnome），探测失败回退 portal
  │
  └─ 4. 返回 AgentShell
```

### 4.3 装配代码

```rust
impl AgentShell {
    pub async fn detect_and_assemble() -> Result<Self> {
        let de = detect_backend().await?;   // DesktopEnvironment (Kde/Dde/Gnome/...)
        let session_type = detect_session_type();

        // 公共基础设施（跨 DE 共享）
        let dbus = DBusManager::new().await?;
        let event_hub = EventHub::new();

        // 按 DE 调用对应 backend 的 assemble()，由 backend 决定：
        //   1. 选哪个合成器（KWinCompositor / TreelandCompositor / MutterCompositor…）
        //   2. 选哪些 DE 专有组件（KdePowerDevil / DdePower / GnomePower…）
        //   3. 未覆盖的公共组件（audio / input / capture / a11y）由公共组件层探测
        let components = match de {
            DesktopEnvironment::Kde => {
                let backend = KdeBackend::new(dbus.clone(), session_type);
                backend.assemble(event_hub.clone()).await?
            }
            DesktopEnvironment::Dde => {
                let backend = DdeBackend::new(dbus.clone(), session_type);
                backend.assemble(event_hub.clone()).await?
            }
            DesktopEnvironment::Gnome => {
                let backend = GnomeBackend::new(dbus.clone(), session_type);
                backend.assemble(event_hub.clone()).await?
            }
            DesktopEnvironment::Hyprland => {
                let backend = HyprlandBackend::new(dbus.clone());
                backend.assemble(event_hub.clone()).await?
            }
            DesktopEnvironment::Sway => {
                let backend = SwayBackend::new(dbus.clone());
                backend.assemble(event_hub.clone()).await?
            }
            DesktopEnvironment::X11Generic => {
                let backend = X11Backend::new();
                backend.assemble(event_hub.clone()).await?
            }
            DesktopEnvironment::WLRWayland => {
                let backend = WlrWaylandBackend::new(dbus.clone());
                backend.assemble(event_hub.clone()).await?
            }
            DesktopEnvironment::Tty => {
                let backend = TtyBackend::new();
                backend.assemble(event_hub.clone()).await?
            }
        };

        Ok(Self { dbus, event_hub, backend: de, components })
    }
}

// 每个 DE 的 assemble() 内部实现示例（KDE）：
impl KdeBackend {
    pub async fn assemble(&self, event_hub: EventHub) -> Result<ComponentRegistry> {
        // 1. 合成器：KWin（Wayland 优先，回退 X11）
        let compositor: Option<Box<dyn CompositorComponent>> = Some(match self.session_type {
            SessionType::Wayland => Box::new(KWinCompositor::new_wayland(self.dbus.clone()).await?),
            SessionType::X11 => Box::new(KWinCompositor::new_x11(self.dbus.clone()).await?),
        });  // TTY backend 用 None

        // 2. DE 专有组件优先（org.kde.* 接口）
        let power: Box<dyn PowerComponent> = KdePowerDevil::try_new(self.dbus.clone()).await?
            .unwrap_or_else(|| Box::new(UPowerComponent::new().await?));  // 回退公共组件
        let notification = KdeNotification::try_new(self.dbus.clone()).await?
            .unwrap_or_else(|| Box::new(PortalNotification::new().await?));
        let appearance = KdeAppearance::try_new(self.dbus.clone()).await?
            .unwrap_or_else(|| Box::new(PortalAppearance::new().await?));
        let launcher = KdeLauncher::try_new(self.dbus.clone()).await?
            .unwrap_or_else(|| Box::new(PortalAppLauncher::new().await?));

        // 3. 公共组件探测（跨 DE 一致）
        let audio: Option<Box<dyn AudioServerComponent>> = if pipewire_available() {
            Box::new(PipeWireAudioServer::new()?)
        } else if pulseaudio_available() {
            Some(Box::new(PulseAudioAudioServer::new()?))
        } else {
            None  // 无音频服务器
        };
        let network = NetworkManagerComponent::new(self.dbus.clone()).await?;
        let input = InputComponent::detect(self.session_type).await?;
        let capture = CaptureComponent::detect(self.session_type).await?;
        let a11y = AccessibilityComponent::detect().await?;
        let clipboard = ClipboardComponent::detect(self.session_type).await?;
        let init_system = SystemdComponent::new().await?;
        let session_manager = LogindComponent::new().await?;

        Ok(ComponentRegistry {
            compositor, audio, network, power, notification,
            appearance, launcher, input, capture, a11y, clipboard,
            init_system, session_manager,
        })
    }
}
```

### 4.4 组件查询

```rust
impl AgentShell {
    /// 合成器（窗口操作入口）
    pub fn compositor(&self) -> &dyn CompositorComponent {
        &*self.components.compositor
    }

    /// 音频
    pub fn audio(&self) -> &dyn AudioServerComponent {
        &*self.components.audio
    }

    /// 按类型查询组件
    pub fn component(&self, ct: ComponentType) -> Option<&dyn DesktopComponent> {
        match ct {
            ComponentType::Compositor => Some(&*self.components.compositor),
            ComponentType::AudioServer => Some(&*self.components.audio),
            ComponentType::Network => self.components.network.as_deref(),
            ComponentType::Input => self.components.input.as_deref(),
            ComponentType::Capture => self.components.capture.as_deref(),
            ComponentType::A11y => self.components.a11y.as_deref(),
            ComponentType::Clipboard => self.components.clipboard.as_deref(),
            ComponentType::Power => self.components.power.as_deref(),
            ComponentType::Notification => self.components.notification.as_deref(),
            ComponentType::Appearance => self.components.appearance.as_deref(),
            ComponentType::Launcher => self.components.launcher.as_deref(),
            ComponentType::InitSystem => self.components.init_system.as_deref(),
            ComponentType::SessionManager => self.components.session_manager.as_deref(),
            _ => None,
        }
    }

    /// 所有组件健康检查（doctor 命令调用）
    pub async fn doctor(&self) -> Vec<(&'static str, ComponentHealth)> {
        let mut results = Vec::new();
        results.push(("backend", ComponentHealth::Healthy));  // backend 本身
        // 遍历 ComponentRegistry 所有组件 ...
        results
    }
}
```

---

## 5. WaylandDisplayServer 组件（协议层）

WaylandDisplayServer 是**显示服务器组件**（合成器组件的协议通道基础），提供 `wl_display` 连接管理、registry 遍历、global 接口探测与 core 协议绑定。wlr 标准协议绑定位于 `WlrWaylandCompositor`（§8 前身见 §5.2 真值），各合成器（KWinCompositor/DdeCompositor 等）通过 `WaylandCompositor` trait 组合本层并追加私有协议。

- 连接 wayland display（`$WAYLAND_DISPLAY`）
- 遍历 registry，建立 globals 快照（`WaylandDisplayServer`，纯 Wayland core）
- wlr 标准协议绑定在 `components/compositor/wlr-wayland`（`WlrBindings`），不在此层
- 各合成器通过 `WaylandCompositor`（`components/compositor/wayland-core`）组合本层并叠加私有协议

### 5.2 协议绑定探测

```rust
// components/compositor/wlr-wayland/src/wlr_protocols.rs

/// wlr 标准协议绑定集合（7 项，每字段独立可选）
#[derive(Debug, Default)]
pub struct WlrBindings {
    pub foreign_toplevel: Option<ZwlrForeignToplevelManagerV1>,   // foreign-toplevel v3
    pub output_management: Option<ZwlrOutputManagerV1>,           // output-management v4
    pub screencopy: Option<ZwlrScreencopyManagerV1>,              // screencopy v3
    pub virtual_pointer: Option<ZwlrVirtualPointerManagerV1>,     // virtual-pointer v2
    pub ext_workspace: Option<ExtWorkspaceManagerV1>,             // ext-workspace v1
    pub virtual_keyboard: Option<ZwpVirtualKeyboardManagerV1>,    // virtual-keyboard v1
    pub data_control: Option<ExtDataControlManagerV1>,            // ext-data-control v1
    pub bind_failures: Vec<(&'static str, String)>,               // 绑定失败明细（doctor）
}
```

实现说明（现状）：
- `WlrBindings` 位于 `wlr-wayland` crate（非 §1 旧树的 `wayland/`），7 个字段 + `bind_failures`；
  `WaylandDisplayServer` 只提供 `connection` / `globals` 快照与 core 协议绑定接口，
  不持有 wlr 协议字段。
- foreign-toplevel 绑定的是 `wayland-protocols-wlr` 的
  `foreign_toplevel::v1::client::zwlr_foreign_toplevel_manager_v1`（接口最高 v3）；
  output-management 支持到 v4；virtual-pointer 接口规范最高 v2。
- 绑定区间常量为 `protocol_versions::*`，上界不得越出接口版本，否则
  `GlobalList::bind` 在取 min 前 panic（`wlr_protocols.rs` 测试回归约束）。

- 请求版本 = min(接口支持上限, global 公布版本)，绑定区间以 `protocol_versions` 常量表达。
  例如 virtual-pointer 区间固定 2..=2（接口最高 v2），output-management 区间 1..=4。
- 绑定失败不抛出异常，仅标记对应字段为 `None` 并记入 `bind_failures`。
  后续操作由调用方检查 availability 并触发降级。

**协议依赖链**（假阳性控制）：
```
foreign-toplevel 可用 → 窗口管理全功能（list/focus/move/close）
foreign-toplevel 不可用 → 降级 D-Bus / AT-SPI
  └─ screencopy + virtual-pointer 仍可用 → 截图 + 输入保持原生
       └─ 全部 wlr 协议不可用 → 全链路 portal 降级
```

### 5.3 子类扩展协议

| 后端 | 私有协议 |
|------|---------|
| KWinCompositor | org_kde_plasma_window_management, org_kde_kwin_fake_input, org_kde_plasma_virtual_desktop_management |
| HyprlandCompositor | hyprland_toplevel_export, hyprland_focus_grab, hyprland_global_shortcuts |
| Treeland（backends/dde/src/treeland.rs） | treeland_foreign_toplevel_manager, treeland_window_management（future D9 通道） |
| MutterCompositor | 无（Mutter 不实现私有协议） |
| SwayCompositor | 无（纯 wlr） |
| WlrWaylandCompositor | 无（纯 wlr 标准协议，子类叠加私有协议） |

### 5.4 降级策略

| 缺失协议 | 降级路径 |
|---------|---------|
| foreign-toplevel | D-Bus / KWin Scripting / hyprctl / AT-SPI |
| virtual-pointer | libei/EIS portal → ydotool → xdotool |
| screencopy | portal ScreenCast (PipeWire) |
| ext-workspace | D-Bus / hyprctl workspaces / EWMH _NET_CURRENT_DESKTOP |

### 5.5 验证输出

```
✓ Wayland 显示  : wayland-0 (wl_display connected, N globals)
✓ wlr 协议      : N/7 bound
  ├─ foreign-toplevel : v3 ✓
  ├─ output-mgmt      : v4 ✓
  ├─ screencopy       : v3 ✓
  ├─ virtual-pointer  : v2 ✓
  ├─ ext-workspace    : v1 ✓
  ├─ virtual-keyboard : v1 ✓
  ├─ data-control     : v1 ✓
  ⚠ not bound : <bind_failures 逐项>
```

实际 doctor 行（`wlr-wayland/src/compositor.rs`）：`✓ wlr 协议 : N/7 bound`，
分母 7 = `WlrBindings` 字段数；逐协议明细顺序为 foreign-toplevel / output-mgmt /
screencopy / virtual-pointer / ext-workspace / virtual-keyboard / data-control。

---

## 6. X11DisplayServer 组件（协议层）

X11DisplayServer 是**显示服务器组件**（不实现 CompositorComponent），提供 x11rb 连接管理、EWMH 原子缓存、_NET_WM 协议操作。各合成器（KWinCompositor/MutterCompositor 等）在其上追加 D-Bus 接口。

### 6.1 职责

- 连接 X11 display（`$DISPLAY`）
- 初始化 EWMH 原子（15+ 个 _NET_WM 原子）
- 提供通用窗口操作（list/focus/move/resize/close 等）
- 子类通过 D-Bus 接口追加各 DE 专有服务

### 6.2 核心操作

```rust
impl X11DisplayServer {
    pub fn connect() -> Result<Self> {
        // 检查 DISPLAY → x11rb::connect(None)
        // root window → intern_atom 批量获取原子
    }
    pub fn get_client_list(&self) -> Result<Vec<u32>> { /* _NET_CLIENT_LIST */ }
    pub fn activate_window(&self, window: u32) -> Result<()> { /* _NET_ACTIVE_WINDOW */ }
    pub fn close_window(&self, window: u32) -> Result<()> { /* _NET_CLOSE_WINDOW */ }
    pub fn move_resize_window(&self, w: u32, x: i32, y: i32, w2: i32, h: i32) -> Result<()> {
        // _NET_MOVERESIZE_WINDOW ClientMessage
    }
    pub fn get_current_desktop(&self) -> Result<u32> { /* _NET_CURRENT_DESKTOP */ }
    pub fn set_current_desktop(&self, desktop: u32) -> Result<()> { /* ClientMessage */ }
}
```

### 6.3 XTest 输入注入

X11 会话下输入注入通过 XTest 扩展实现，无需 xdotool。

```rust
// 来自 x11rb::protocol::xtest
use x11rb::protocol::xtest;

impl X11DisplayServer {
    pub fn fake_key_event(&self, keycode: u8, is_press: bool) -> Result<()> {
        self.conn.xtest_fake_input(
            if is_press { xtest::FakeInput::KEY_PRESS } else { xtest::FakeInput::KEY_RELEASE },
            keycode as u32,
            x11rb::CURRENT_TIME,
            self.root_window,
            0, 0, 0,
        )?;
        self.conn.flush()?;
        Ok(())
    }

    pub fn fake_button_event(&self, button: u8, is_press: bool) -> Result<()> {
        self.conn.xtest_fake_input(
            if is_press { xtest::FakeInput::BUTTON_PRESS } else { xtest::FakeInput::BUTTON_RELEASE },
            button as u32,
            x11rb::CURRENT_TIME,
            self.root_window,
            0, 0, 0,
        )?;
        self.conn.flush()?;
        Ok(())
    }

    pub fn fake_motion_event(&self, x: i32, y: i32) -> Result<()> {
        self.conn.xtest_fake_input(
            xtest::FakeInput::MOTION_NOTIFY,
            0,
            x11rb::CURRENT_TIME,
            self.root_window,
            x as i16, y as i16, 0,
        )?;
        self.conn.flush()?;
        Ok(())
    }
}
```

**注意事项**：XTest 仅在**原生 X11 会话**（XDG_SESSION_TYPE=X11）下可用。XWayland 下 XTest 被 blocking，输入注入应走 libei/EIS（见第 12 章）。

### 6.4 截图：XGetImage / MIT-SHM

X11 截图通过 `XGetImage` 或 MIT-SHM 扩展实现，不依赖 ImageMagick `import`。

```rust
impl X11DisplayServer {
    pub fn capture_window(&self, window: u32) -> Result<Vec<u8>> {
        // 首选 MIT-SHM（共享内存，零拷贝）
        if let Ok(img) = self.conn.xshm_get_image(...) {
            return Ok(img.data);
        }
        // 降级 XGetImage
        let img = self.conn.get_image(
            x11rb::NONE as u8, window, 0, 0, width, height, !0,
        )?;
        Ok(img.data)
    }
}
```

**注意事项**：X11 根窗口直捕仅在**原生 X11 会话**（XDG_SESSION_TYPE=X11）下可用。XWayland 下禁用——纯 Wayland 会话的 XWayland root 无合成器内容，直捕只会得到全黑帧；此时截图应走 portal ScreenCast/Screenshot。

### 6.5 子类额外接口

| 后端 | 额外 D-Bus 接口 |
|------|----------------|
| KWinCompositor（X11 会话） | org.kde.KWin (KWin Scripting via X11) |
| MutterCompositor（X11 会话） | org.gnome.Shell, org.gnome.Mutter.DisplayConfig |
| DdeCompositor（X11 会话） | org.deepin.dde.* / com.deepin.daemon.* |
| X11Compositor（通用 X11 兜底） | 无（纯 EWMH + CLI） |

### 6.6 验证输出

```
✓ X11 连接     : :0 (screen 0, 1920x1080)
✓ EWMH 协议   : 15/15 atoms bound
✓ XTest 扩展   : v2.2 ✓ (输入注入原生)
```

---

## 7. 合成器：KWin（KDE）

`KWinCompositor` 实现 CompositorComponent，**KDE 合成器组件**。内部根据 `XDG_SESSION_TYPE` 选择组合方式。

**核心结论（先读这段）**：KWin 有两条通道，按**优先级**排列：

1. **Wayland 私有协议通道（基础，首选）**—— `org_kde_plasma_window_management`（v20+）
   负责窗口管理：窗口列表、聚焦、最小化、关闭、状态推送。`org_kde_kwin_fake_input`（v6+）
   负责输入注入。协议路径是零开销的原生 compositor 接口。
2. **D-Bus / KWin Scripting 通道（补充，回退）**—— 协议搞不定的才走这里：
   - 窗口移动/缩放（`set_geometry` 协议不支持）
   - `org_kde_plasma_window_management` 单客户端限制（任务栏已占用时协议不可用，回退 Scripting）
   - 事件订阅（`callDBus` 信号比协议信号更全）
   - X11 会话（无 Wayland 协议可用）

**一句话**：能力够的走协议，协议不够的走 Scripting。

| 会话 | 组合 | 基础通道 | 补充通道 |
|------|------|---------|---------|
| **Wayland** | `WaylandDisplayServer` + org_kde_* 协议 | org_kde_* 私有协议 | D-Bus / Scripting |
| **X11** | `X11DisplayServer` + D-Bus | EWMH/XTest（原生 X11） | org.kde.KWin D-Bus |

模块路径：`components/compositor/kwin/src/`

### 7.1 双通道架构

```
┌──────────────┐
│  agent-shell │
│  kwin adapter│
│  (Rust/zbus) │
└──┬────────┬──┘
   │        │
   │Wayland  │D-Bus（补充）
   │协议(基础)│
   ▼        ▼
┌─────────────────────┐   ┌──────────────────────┐
│ KWin compositor     │   │ KWin (kwin_wayland/  │
│ (Wayland globals)   │   │ kwin_x11)            │
│  org_kde_plasma_    │   │  ┌────────────────┐  │
│    window_management│   │  │ JS Scripting   │  │
│    (v20+, 短绑)     │   │  │ (QJSEngine)    │  │
│  org_kde_kwin_fake_ │   │  │  workspace.*   │  │
│    input (v6+)      │   │  │  client.*      │  │
│  org_kde_plasma_    │   │  └────────────────┘  │
│    virtual_desktop  │   │  org.kde.KWin D-Bus   │
│    _management (v3) │   └──────────────────────┘
└─────────────────────┘
※ X11 会话无 Wayland 协议通道，基础通道替换为 EWMH/XTest（第 6 章）
```

### 7.2 通道选择矩阵（按优先级排列）

| 能力 | 首选（协议） | 回退（Scripting） | 原因 |
|------|------------|-------------------|------|
| 窗口列表 | `window_management.get_stacking_order` | `list_windows.js` | 协议零开销，单客户端约束时回退 |
| 窗口聚焦 | `window_management.activate(uuid)` | `activeWindow = w` | 同上 |
| 最小化/恢复 | `window_management.set_minimized` | `w.minimized = true/false` | 同上 |
| 关闭窗口 | `window_management.close(uuid)` | `w.close()` | 同上 |
| 状态推送 | `window_management` 事件（若绑定） | `event_monitor.js` 信号 | 协议事件更轻量 |
| 窗口移动/缩放 | **不支持** | `w.geometry = Qt.rect(...)` | 协议没有 set_geometry |
| 最大化 | **不支持** | `w.maximizeMode = 3` | 协议没有 set_maximized |
| 工作区管理 | `virtual_desktop_management` 协议 | Scripting desktops 数组 | |
| 输入注入 | `fake_input` 协议（authenticate 后） | ydotool / XTest（第 12 章） | 协议最直接 |
| 桌面显示 | `window_management.show_desktop` | -- | 协议原生支持 |
| 事件订阅 | D-Bus（`event_monitor.js` 长驻） | -- | 协议事件不如 Scripting 信号全 |
| 窗口信息 | `window_management.get_window_by_uuid` | `get_active_window.js` | 协议零开销 |

**结论**：协议覆盖 ~60% 能力（窗口列表/聚焦/最小化/关闭/输入注入/桌面显示/虚拟桌面）。
Scripting 补足 ~40%（移动/缩放/最大化/事件订阅/单客户端回退）。

### 7.3 核心机制：D-Bus ↔ KWin Scripting 桥接（补充通道）

KWin Scripting 的 `loadScript` 返回 object path，但 `run` 不直接返回结果。采用三种桥接策略：

**策略 A：print → journalctl 解析（简单查询）**

```
loadScript("list_windows.js") → 返回 /Scripting/Script<N>
Script.run() → JS 执行 print(JSON.stringify(result))
→ journalctl _COMM=kwin_wayland 读取输出
Script.stop()
```

**策略 B：callDBus → 自定义 D-Bus 服务（结构化数据，推荐）**

```
Agent Shell 注册临时 D-Bus 服务：
  com.agent_shell.Response /com/agent_shell/response

JS 脚本调用：
  callDBus("com.agent_shell.Response", "/com/agent_shell/response",
           "com.agent_shell.Response", "sendResult", JSON.stringify(result))

Agent Shell 监听信号，收到结果后 stop 脚本。
```

**策略 C：内联脚本（简单查询，避免文件 IO）**

```rust
async fn eval_script(&self, js: &str) -> Result<String> {
    // 1. 注册临时响应服务
    // 2. 构造内联脚本：
    //    var result = <JS>;
    //    callDBus("com.agent_shell.Response", ..., JSON.stringify(result));
    // 3. loadScript → run → 等待响应（5s 超时）→ stop
    // 4. 返回 JSON 字符串
}
```

### 7.4 预置 JS 脚本清单（补充通道）

**list_windows.js**

```javascript
var windows = workspace.windowList();
var result = [];
for (var i = 0; i < windows.length; i++) {
    var w = windows[i];
    result.push({
        id: w.internalId.toString(),
        title: w.caption,
        appId: w.resourceName || w.windowClass,
        pid: w.pid,
        geometry: { x: w.geometry.x, y: w.geometry.y, width: w.geometry.width, height: w.geometry.height },
        frameGeometry: { x: w.frameGeometry.x, y: w.frameGeometry.y,
                         width: w.frameGeometry.width, height: w.frameGeometry.height },
        minimized: w.minimized,
        maximized: (w.maximizeMode === 3),
        fullscreen: w.fullScreen,
        keepAbove: w.keepAbove,
        desktop: w.desktops[0] ? w.desktops[0].id : -1,
        stackingOrder: i,
        windowType: w.windowType,
        desktopFile: w.desktopFileName
    });
}
callDBus("com.agent_shell.Response", "/com/agent_shell/response",
         "com.agent_shell.Response", "sendResult", JSON.stringify(result));
```

**get_active_window.js**

```javascript
var w = workspace.activeWindow;
callDBus(..., JSON.stringify(w ? {
    id: w.internalId.toString(), title: w.caption, appId: w.resourceName, pid: w.pid
} : null));
```

**move_window.js**（`%s` / `%d` 占位符由 Rust 端注入）

```javascript
var w = workspace.windowList().find(function(w) { return w.internalId.toString() === "%s"; });
if (w) {
    w.geometry = Qt.rect(%d, %d, w.geometry.width, w.geometry.height);
    callDBus(..., JSON.stringify({ success: true }));
} else {
    callDBus(..., JSON.stringify({ success: false, error: "Window not found" }));
}
```

**monitor_layout.js**

```javascript
var screens = workspace.screens;
var result = [];
for (var i = 0; i < screens.length; i++) {
    var s = screens[i];
    result.push({ id: s.name, name: s.name,
        geometry: { x: s.geometry.x, y: s.geometry.y, width: s.geometry.width, height: s.geometry.height },
        scale: s.scale, isPrimary: (i === 0) });
}
callDBus(..., JSON.stringify(result));
```

**event_monitor.js**（长期运行脚本，不 stop）

```javascript
workspace.windowAdded.connect(function(w) {
    callDBus(..., JSON.stringify({ event: "windowOpened", id: w.internalId.toString() }));
});
workspace.windowRemoved.connect(function(w) {
    callDBus(..., JSON.stringify({ event: "windowClosed", id: w.internalId.toString() }));
});
workspace.activeWindowChanged.connect(function() {
    var w = workspace.activeWindow;
    if (w) callDBus(..., JSON.stringify({ event: "windowFocused", id: w.internalId.toString() }));
});
```

### 7.5 Rust 桥接实现（补充通道）

```rust
// components/compositor/kwin/src/dbus_bridge.rs

pub struct KWinBridge {
    connection: zbus::Connection,
    response_tx: tokio::sync::oneshot::Sender<String>,
    script_counter: Arc<AtomicU64>,
}

impl KWinBridge {
    async fn run_script(&self, js: &str) -> Result<String> {
        let (tx, rx) = tokio::sync::oneshot::channel();
        self.response_tx = tx;

        // 注册临时 D-Bus 响应服务（com.agent_shell.Response）
        // 构造内联脚本：执行 js → callDBus sendResult
        // loadScript → run
        let script_path = scripting.load_script(&inline).await?;
        let script_obj = ScriptProxy::new(&self.connection, &script_path).await?;
        script_obj.run().await?;

        // 等待响应（5s 超时）
        let result = tokio::time::timeout(Duration::from_secs(5), rx).await??;

        script_obj.stop().await.ok();
        Ok(result)
    }
}
```

**关键点**：
- KWin 脚本在 compositor 进程内执行，`callDBus` 是向 session bus 发起调用，agent-shell 作为接收方
- 脚本执行无返回值机制，必须靠 callDBus 回传；journalctl 解析仅作调试兜底
- 长期运行脚本（事件监听）必须与一次性脚本（查询）分开管理

### 7.6 Wayland 私有协议通道（基础）

KWin 提供完整的私有 Wayland 协议栈，**仅 Wayland 会话可用**，是所有能力的首选基础路径。

| 协议 | 版本 | 能力 | 备注 |
|------|:----:|------|------|
| `org_kde_plasma_window_management` | 20 | 窗口管理：show_desktop、get_window_by_uuid、get_stacking_order、activate、set_minimized、close、activation 反馈 | ⚠️ 仅一个客户端可绑定 |
| `org_kde_kwin_fake_input` | 6 | 假输入：pointer/keyboard/touch（XTest 等价物） | 需 authenticate |
| `org_kde_plasma_virtual_desktop_management` | 3 | 虚拟桌面：创建/删除/激活 | |
| `kde-output-management-v2` | - | 输出配置 | |
| `kde-dpms` | - | 显示器电源 | |
| `kde-idle` | - | 空闲检测 | |
| `kde-screencast` | - | 私有录屏 | |
| `kde-external-brightness` | - | 外部亮度 | |

**协议能做的**：窗口列表、聚焦、最小化、关闭、桌面显示、虚拟桌面管理、输入注入、输出配置。
**协议做不了的**：窗口移动/缩放（`set_geometry` 协议不支持）、最大化状态切换。

**设计决策（D8）**：`org_kde_plasma_window_management` 仅一个客户端可绑定。agent-shell 采用
**「需要时短绑」策略**——绑定成功则用协议做窗口管理（首选），绑定失败（任务栏已占用）回退
Scripting（补充通道）。实现时确认 KWin 6.7 上任务栏与第三方可共存。

### 7.7 实现设计

**模块结构**（`components/compositor/kwin/src/`）：

| 文件 | 职责 | 通道 |
|------|------|------|
| `lib.rs` | 模块根 + re-export | -- |
| `kwin_compositor.rs` | KWinCompositor + CompositorComponent impl | -- |
| `wayland.rs` | Wayland 私有协议客户端（org_kde_* 协议族） | 基础 |
| `dbus_bridge.rs` | D-Bus ↔ KWin Scripting 桥接（KWinBridge + ResponseService） | 补充 |
| `scripts.rs` | 预置 JS 脚本模板 + 版本兼容层 | 补充 |
| `event_script.rs` | 长期运行事件脚本管理 | 补充 |
| `version.rs` | KWin 版本探测（supportInformation 解析） | 共享 |
| `error.rs` | KWin 特有错误类型 | -- |

**核心接口**：

```rust
pub struct KWinCompositor {
    wayland: Option<KWinProtocols>,        // Wayland 协议通道（基础，首选）
    bridge: KWinBridge,                    // D-Bus / Scripting 通道（补充）
    version: KWinVersion,
    event_handle: Option<EventScriptHandle>,
}

pub struct KWinProtocols {
    wl: Arc<WaylandDisplayServer>,                  // 共享 wl_display
    window_mgmt: Option<WindowManagement>,    // 短绑，仅一个客户端
    fake_input: Option<FakeInput>,            // 输入注入
    vd_mgmt: Option<VirtualDesktopManagement>,// 虚拟桌面
}
```

**选择逻辑**：

```
list_windows():
  1. wayland.window_mgmt 绑定成功？
     → 是：get_stacking_order() 走协议
     → 否：bridge.list_windows.js 走 Scripting

focus_window(id):
  1. wayland.window_mgmt 绑定成功？
     → 是：activate(uuid) 走协议
     → 否：bridge.focus_window.js 走 Scripting

move_window(id, x, y):
  → 协议不支持 set_geometry，直接走 bridge.move_window.js

input.send_key(combo):
  → wayland.fake_input 绑定成功？authenticate() → fake_keyboard_key()
  → 绑定失败 → 降级 ydotool/XTest（第 12 章）
```

**预置脚本**：14 个（list_windows, get_active_window, focus_window, move_window, resize_window, close_window, set_window_geometry, minimize_window, maximize_window, list_workspaces, switch_workspace, move_window_to_workspace, list_monitors, event_monitor）

**Wayland 协议绑定**：遍历 registry globals 匹配 `org_kde_plasma_window_management` (v20+) / `org_kde_kwin_fake_input` (v6+) / `org_kde_plasma_virtual_desktop_management` (v3+)。fake_input 需 `authenticate`。window_management 仅一个客户端可绑定，失败回退 Scripting。

**KWin 版本兼容**：

| API | KWin 5 | KWin 6 |
|-----|--------|--------|
| 窗口几何 (Scripting) | w.geometry | w.rect / w.frameGeometry |
| 虚拟桌面 (Scripting) | w.desktop | w.desktops (数组) |
| 窗口 ID | w.internalId / w.id | w.internalId |
| 协议接口 | 同 (KWin 5.27+ 支持 org_kde_*) | 同 |

**会话无关的共享能力**：

| 能力 | Wayland 会话 | X11 会话 | 共享？ |
|------|-------------|---------|:------:|
| KWinBridge（loadScript/callDBus） | 同 | 同 | ✅ 共享 |
| 预置 JS 脚本 | 同 | 同 | ✅ 共享 |
| KWin 版本探测 | 同 | 同 | ✅ 共享 |
| 事件订阅（event_monitor.js） | 同 | 同 | ✅ 共享 |
| org.kde.KWin D-Bus 服务 | 同 | 同 | ✅ 共享 |
| 窗口管理 | org_kde_* 协议（基础） | EWMH（基础） | 各走各路 |
| 输入注入 | fake_input 协议（基础） | XTest（基础） | 各走各路 |
| 几何操作 | Scripting 补充 | Scripting + EWMH 补充 | 各走各路 |

**验证输出**：
```
✓ Wayland 协议  : 3/5 globals bound (window_mgmt v20, fake_input v6, vd_mgmt v3)
✓ KWin 服务    : org.kde.KWin reachable, 5s roundtrip
✓ D-Bus 桥接  : callDBus roundtrip 2.3ms (13/13 scripts)
✓ 输入注入    : fake_input authenticated ✓
⚠ 事件脚本    : loaded (workspace.windowAdded OK)
```

---

## 8. 合成器：Mutter（GNOME）

`MutterCompositor` 实现 CompositorComponent，**GNOME 合成器组件**。

**核心通道**：GNOME 的 Wayland 协议是有意留空的——Mutter 明确不实现
`wlr-foreign-toplevel-management` / `ext-foreign-toplevel-list`（隐私设计决策），也没有可用的窗口管理私有协议。
因此 **D-Bus 通道（`org.gnome.Shell`）就是事实上的基础通道**，所有窗口操作走 Eval/Extension；
输入和截图必须走 portal 路径（见第 12、13 章）。这是「协议缺失 → D-Bus 顶替」的唯一情况。

内部根据 session 类型组合：
- **Wayland 会话**：组合 `WaylandDisplayServer`（仅 wlr 通用协议，无窗口管理能力）+ `org.gnome.Shell` D-Bus（Eval/Extension 双路径）
- **X11 会话**：组合 `X11DisplayServer` + `org.gnome.Shell` + `Mutter.DisplayConfig`

### 8.1 双路径策略（D-Bus 通道）

**路径 A：`org.gnome.Shell.Eval`（直接，简单查询）**

```javascript
// gdbus call -e -d org.gnome.Shell -o /org/gnome/Shell \
//   -m org.gnome.Shell.Eval '<js>'
JSON.stringify(global.get_window_actors().map(a => {
    let mw = a.meta_window;
    return {
        id: mw.get_id(), title: mw.get_title() || '',
        appId: mw.get_wm_class() || '', pid: mw.get_pid(),
        geometry: mw.get_frame_rect(), hasFocus: mw.has_focus()
    };
}))
```

返回格式：`(true, '"JSON字符串"')`。注意新版 GNOME 可能默认禁用 Eval，需检测回退。

**路径 B：GNOME Shell Extension（推荐生产）**

```javascript
// gnome-shell-extension/extension.js（节选）
const DBusInterface = `
<node>
  <interface name="org.gnome.Shell.AgentShell">
    <method name="ListWindows"><arg type="s" direction="out"/></method>
    <method name="GetActiveWindow"><arg type="s" direction="out"/></method>
    <method name="FocusWindow"><arg type="u" name="window_id" direction="in"/></method>
    <method name="MoveWindow">
      <arg type="u" name="window_id" direction="in"/>
      <arg type="i" name="x" direction="in"/>
      <arg type="i" name="y" direction="in"/>
    </method>
    <method name="GetMonitorLayout"><arg type="s" direction="out"/></method>
    <signal name="WindowOpened"><arg type="s" name="info"/></signal>
    <signal name="WindowClosed"><arg type="u" name="window_id"/></signal>
    <signal name="ActiveWindowChanged"><arg type="s" name="info"/></signal>
  </interface>
</node>`;
```

### 8.2 Rust 实现

```rust
// components/compositor/mutter/src/eval_bridge.rs

pub struct GnomeEvalBridge { shell_proxy: ShellProxy }

impl GnomeEvalBridge {
    /// 执行 JS 表达式并解析返回值
    async fn eval_js(&self, js: &str) -> Result<serde_json::Value> {
        let (success, result_json) = self.shell_proxy.eval(js).await?;
        if !success { return Err(AgentShellError::Other(...)); }
        serde_json::from_str(&result_json).map_err(Into::into)
    }

    async fn list_windows(&self) -> Result<Vec<WindowInfo>> {
        let js = r#"JSON.stringify(global.get_window_actors().map(a => {
            let mw = a.meta_window;
            return { id: mw.get_id(), title: mw.get_title() || '',
                     appId: mw.get_wm_class() || '', pid: mw.get_pid(),
                     geometry: mw.get_frame_rect() };
        }))"#;
        let result = self.eval_js(js).await?;
        serde_json::from_value(result).map_err(Into::into)
    }
}
```

### 8.3 Wayland 协议状况

GNOME/Mutter 明确不实现 `wlr-foreign-toplevel-management` / `ext-foreign-toplevel-list`（隐私原则：不让第三方枚举窗口），也无公开的窗口管理私有协议。窗口语义路径只能是：

- `org.gnome.Shell` D-Bus（Eval 受限版本边界内）
- Shell Extension（注册 D-Bus 接口）
- AT-SPI（无障碍树兜底）

截图/输入走 portal（ScreenCast/RemoteDesktop），不依赖 Mutter 私有协议。

### 8.4 实现设计

**模块结构**（`components/compositor/mutter/src/`）：

| 文件 | 职责 |
|------|------|
| `lib.rs` | 模块根 + re-export |
| `mutter_compositor.rs` | MutterCompositor + CompositorComponent impl |
| `eval.rs` | org.gnome.Shell.Eval 路径 (GNOME <47) |
| `extension.rs` | Shell Extension 路径 (GNOME 47+) |
| `display_config.rs` | Mutter.DisplayConfig 显示器配置 |
| `version.rs` | GNOME 版本探测 |
| `error.rs` | Mutter 特有错误类型 |

**核心接口**：

```rust
pub enum GnomePath { Eval(EvalRunner), Extension(ExtensionRunner) }

pub struct MutterCompositor {
    path: GnomePath,
    version: GnomeVersion,
    display_config: DisplayConfig,
}
```

**能力现状**（`capabilities()` 真值）：

| 能力 | 值 | 说明 |
|------|-----|------|
| window_management | true | focus/close/minimize/maximize 经 Eval/Extension 可用 |
| workspace_management | false | GNOME 工作区语义不在 Eval/Extension 方法集内 |
| monitor_layout | true | 仅可读布局（GetCurrentState） |
| window_events / workspace_events | false | Extension 三信号存在但归一化依赖 extension.js 部署，语义未定前不声明 |
| native_input / native_capture | false | portal RemoteDesktop / ScreenCast 走输入与 capture 组件 |
| virtual_desktops / effects_control | false | -- |

`move_window` 已实现（Extension 路径 `ext.move_window(&id.native_id, x, y)`；
Eval 路径返回 `NotImplemented("mutter: window move unavailable on Eval path")`），
但能力矩阵不声明 move，仅供显式调用方使用。`resize_window` 恒 `NotImplemented`
（GNOME 无协议，portal-only）。resize / workspace / 原生事件流仍无——GNOME 是**最受限的后端**，
事件流通过 AT-SPI 补充。

**Eval 路径**（GNOME <47）：
```rust
// global.get_window_actors().map(mw => ({ id, title, appId, pid, geometry, state }))
// meta_window.activate() / delete() / minimized=max / maximize(val)
```

**Extension 路径**（GNOME 47+）：通过 Shell Extension `agent-shell-bridge@multica.dev` 注册 D-Bus 接口：
- `GetWindows()` → JSON
- `ActivateWindow(uuid)` / `CloseWindow(uuid)` / `MinimizeWindow(uuid)` / `MaximizeWindow(uuid)`
- 信号：`WindowOpened` / `WindowClosed` / `ActiveWindowChanged`

**会话无关的共享能力**：

| 能力 | Wayland 会话 | X11 会话 | 共享？ |
|------|-------------|---------|:------:|
| org.gnome.Shell D-Bus 接口 | 同 | 同 | ✅ 共享 |
| Eval 路径（GNOME <47） | 同 | 同 | ✅ 共享 |
| Extension 路径（GNOME 47+） | 同 | 同 | ✅ 共享 |
| Mutter.DisplayConfig | 同 | 同 | ✅ 共享 |
| 窗口操作 | Wayland 无协议 | X11 EWMH | 各走各路 |

**验证输出**：

✓ GNOME 版本   : GNOME 47.0 (Eval 受限)
✓ 后端路径     : Extension (agent-shell-bridge@multica.dev)
✓ D-Bus 接口   : org.gnome.Shell ✓, DisplayConfig ✓, ScreenSaver ✓
⚠ 窗口操作     : 受限（move 仅 Extension 路径；无 resize/workspace/事件流）
```

---

## 9. 合成器：Hyprland

`HyprlandCompositor` 实现 CompositorComponent，**纯 Wayland 合成器**（无 X11 变体）。

**核心通道**：Hyprland 有两条通道。
1. **Wayland 私有协议通道（基础）**—— `hyprland_toplevel_export`（窗口级截图）、
   `hyprland_focus_grab`（聚焦抓取）、`hyprland_global_shortcuts`（全局快捷键注册）在协议层完成。
   同时 wlr 标准协议（foreign-toplevel / ext-workspace / virtual-pointer / screencopy）承担
   窗口管理、工作区、输入、截图的通用部分。
2. **hyprctl socket IPC 通道（补充）**—— 窗口管理、工作区、监视器的剩余能力走 `hyprctl` 命令
   （`$XDG_RUNTIME_DIR/hypr/<HIS>/.socket.sock` 同步请求 + `.socket2.sock` 事件推送）。
   hyprctl 是 Hyprland 的实现细节，作为协议的补充；协议覆盖不了的操作（如精确几何设置）走它。

组合：`WaylandDisplayServer`（wlr 通用协议）+ `hyprland_*` 私有协议 + `hyprctl` socket IPC。

### 9.1 双 Socket：请求 + 事件

```
$XDG_RUNTIME_DIR/hypr/<HIS>/.socket.sock   → hyprctl 同步请求（JSON 响应）
$XDG_RUNTIME_DIR/hypr/<HIS>/.socket2.sock  → 事件推送（EVENT>>DATA\n）
```

### 9.2 hyprctl 封装

```rust
// components/compositor/hyprland/src/hyprctl.rs

pub struct Hyprctl { socket_path: PathBuf }

impl Hyprctl {
    pub fn new() -> Result<Self> {
        let his = env::var("HYPRLAND_INSTANCE_SIGNATURE")
            .map_err(|_| AgentShellError::BackendUnavailable("NOT in Hyprland session".into()))?;
        let runtime = env::var("XDG_RUNTIME_DIR")?;
        Ok(Self { socket_path: Path::new(&runtime).join("hypr").join(his).join(".socket.sock") })
    }

    /// 发送请求。注意：Hyprland 同步求值，连接必须即开即关，否则会阻塞 compositor。
    async fn request(&self, cmd: &str) -> Result<serde_json::Value> {
        let mut stream = UnixStream::connect(&self.socket_path).await?;
        stream.write_all(cmd.as_bytes()).await?;
        stream.shutdown(Write).unwrap();
        let mut buf = String::new();
        stream.read_to_string(&mut buf).await?;
        serde_json::from_str(&buf).map_err(Into::into)
    }

    async fn dispatch(&self, cmd: &str) -> Result<()> {
        self.request(&format!("dispatch {}", cmd)).await?;
        Ok(())
    }

    async fn list_windows(&self) -> Result<Vec<WindowInfo>> {
        // clients -j 返回完整 JSON
        Self::parse_clients(self.request("clients -j").await?)
    }

    async fn focus_window(&self, address: &str) -> Result<()> {
        self.dispatch(&format!("focuswindow address:0x{}", address)).await
    }

    async fn move_window(&self, address: &str, x: i32, y: i32) -> Result<()> {
        self.dispatch(&format!("setfloating address:0x{}", address)).await?;
        self.dispatch(&format!("movewindowpixel exact {} {},address:0x{}", x, y, address)).await
    }

    async fn close_window(&self, address: &str) -> Result<()> {
        self.dispatch(&format!("closewindow address:0x{}", address)).await
    }

    async fn activate_workspace(&self, id: i32) -> Result<()> {
        self.dispatch(&format!("workspace {}", id)).await
    }
}
```

### 9.3 事件流（socket2）

```rust
// components/compositor/hyprland/src/event_socket.rs

pub fn spawn_event_task() -> mpsc::Receiver<DesktopEvent> {
    let (tx, rx) = tokio::sync::mpsc::channel(256);
    tokio::spawn(async move {
        // 连接 .socket2.sock，逐行读取 "EVENT>>DATA"
        // openwindow   → WindowOpened(parts)
        // closewindow  → WindowClosed(address)
        // activewindowv2 → WindowFocused(address)
        // workspacev2  → WorkspaceChanged(id, name)
        // focusedmon   → MonitorFocused
        // fullscreen   → FullscreenChanged
    });
    rx
}
```

**事件行格式**：`openwindow>>ADDR,WS,CLASS,TITLE` / `workspacev2>>ID,NAME` 等（见 Hyprland wiki IPC 页）。

### 9.4 Wayland 协议路径（首选）

Hyprland 是 wlroots 系 compositor，支持全部 wlr 标准扩展 + Hyprland 私有协议：

**wlroots 标准扩展（wlroots 系通用）**：

| 协议 | 能力 |
|------|------|
| `wlr-foreign-toplevel-management` (v3) | 窗口列表+激活/关闭/状态（maximize/minimize/fullscreen） |
| `ext-foreign-toplevel-list` (staging) | **未绑定**——仅被 hyprland toplevel-mapping 协议 XML 作为外部接口引用，非独立绑定字段 |
| `ext-workspace` (staging) | 工作区列举与切换 |
| `wlr-virtual-pointer` | 鼠标注入 |
| `zwp-virtual-keyboard` | 键盘注入 |
| `wlr-screencopy` | 屏幕截图 |
| `wlr-output-management` | 输出配置 |
| `ext-data-control` (staging) | 剪贴板数据控制 |

**Hyprland 私有协议**（`hyprland/src/wayland.rs` 绑定 4 项）：

| 协议 | 版本 | 能力 |
|------|:----:|------|
| `hyprland_toplevel_export_manager_v1` | 2 | 窗口级内容捕获（WindowId 截图首选） |
| `hyprland_focus_grab_manager_v1` | 1 | 输入焦点白名单限制 |
| `hyprland_global_shortcuts_manager_v1` | 1 | 全局快捷键注册 |
| `hyprland_toplevel_mapping_manager_v1` | 1 | toplevel → 窗口地址映射 |

**实现思路**：窗口管理走 wlr-foreign-toplevel-management（纯协议，无 socket 命令解析），窗口截图走 hyprland_toplevel_export_manager_v1，输入注入走 wlr-virtual-pointer/virtual-keyboard；hyprctl socket 保留为扩展能力（dispatch exec 等）与版本探测后备。

### 9.5 实现设计

**模块结构**（`components/compositor/hyprland/src/`）：

| 文件 | 职责 |
|------|------|
| `lib.rs` | 模块根 + re-export（HyprlandCompositor） |
| `compositor.rs` | HyprlandCompositor + CompositorComponent impl |
| `hyprctl.rs` | UNIX Socket IPC — hyprctl 同步请求 |
| `event_socket.rs` | 事件流 — .socket2.sock 长期连接 |
| `wayland.rs` | wlr 标准扩展 + Hyprland 私有协议客户端 |
| `cache.rs` | 窗口缓存（事件流持续更新） |
| `protocol_gen.rs` | hyprland 协议生成绑定 |

**核心接口**：

```rust
pub struct HyprlandCompositor {
    wayland: Option<HyprlandWayland>,  // 首选
    hyprctl: Hyprctl,                   // 降级 + 扩展
    window_cache: Arc<RwLock<Vec<WindowInfo>>>,
    event_handle: Option<JoinHandle<()>>,
}
```

**Hyprctl 封装**：
```rust
pub async fn clients(&self) -> Result<Vec<Value>> { /* clients -j */ }
pub async fn monitors(&self) -> Result<Vec<Value>> { /* monitors -j */ }
pub async fn workspaces(&self) -> Result<Vec<Value>> { /* workspaces -j */ }
pub async fn dispatch(&self, action: &str) -> Result<()> { /* dispatch ... */ }
```

**操作选择矩阵**：

| 操作 | Wayland 协议 | hyprctl 替代 | 推荐 |
|------|-------------|-------------|:----:|
| 窗口列表 | foreign-toplevel（无 geometry） | clients -j（有 geometry） | 合并 |
| 聚焦 | foreign-toplevel.activate | dispatch focuswindow | Wayland |
| 移动 | 不支持 | dispatch movewindowpixel | hyprctl |
| 缩放 | 不支持 | dispatch resizewindowpixel | hyprctl |
| 最小化/最大化 | foreign-toplevel.set_minimized/maximized | 无 | Wayland |
| 关闭 | foreign-toplevel.close | dispatch closewindow | Wayland |
| 工作区 | ext-workspace | workspaces -j | Wayland |
| 截图 | hyprland_toplevel_export | 无 | 私有协议 |
| 输入 | wlr-virtual-pointer | 无 | Wayland |

**事件流**：`.socket2.sock` 行协议 `EVENT>>DATA`。
- `openwindow>>ADDR,WS,CLASS,TITLE` → WindowAdded
- `closewindow>>ADDR` → WindowRemoved
- `activewindowv2>>ADDR` → WindowFocusChanged
- `workspacev2>>ID` → WorkspaceActivated

事件流 task 持续更新 `window_cache`，后端查询优先从缓存读取。

**会话限制**：Hyprland 纯 Wayland，无 X11 变体。所有能力走 Wayland 协议 + hyprctl。

**验证输出**：

✓ Hyprland 会话  : HYPRLAND_INSTANCE_SIGNATURE=abc123
✓ Wayland 协议   : 8/11 globals bound (foreign-toplevel v3, virtual-pointer v2, hyprland-export v2)
✓ hyprctl socket : OK (<1ms, 12 windows, 2 monitors, 4 workspaces)
✓ 事件流        : connected (openwindow, closewindow, activewindow, workspacev2)
```

---

## 10. 合成器：DDE（deepin-kwin / Treeland）

`DdeCompositor` 实现 CompositorComponent，**Deepin 合成器组件**。

**核心通道**：DDE 在 Wayland 下使用 `deepin-kwin`（KWin fork），因此 D合成器组件 = **复用 KWinCompositor
双通道架构**（org_kde_* 协议基础 + Scripting 补充）+ **dde-api D-Bus 补充**（DDE 专有服务）。
当前 DDE 25 的 deepin-kwin 未启用 Treeland，走 KWin 通道；未来 Treeland 迁移后切换 treeland_* 私有协议
（treeland_foreign_toplevel_manager / treeland_capture / treeland_output_manager）为新的基础通道。

内部根据 session 类型组合：
- **Wayland 会话**：组合 `WaylandDisplayServer` + 复用 KWinCompositor（deepin-kwin）+ 未来 Treeland 私有协议
- **X11 会话**：组合 `X11DisplayServer` + `org.deepin.dde.*` / `com.deepin.daemon.*`

### 10.1 架构：组合复用

DDE 在 Wayland 下使用 `deepin-kwin`（KWin fork），因此 DDE backend = 复用 KWin 合成器组件 + `dde-api` D-Bus（DE 封装补充）。

```
┌──────────────┐     D-Bus      ┌──────────────────────┐
│  dde adapter │◄──────────────►│  deepin-kwin         │
│              │                │  (KWin Scripting API)│
│              │                ├──────────────────────┤
│              │                │  dde-api D-Bus 服务    │
│              │                │  org.deepin.dde.*     │
│              │  WM            │  ├─ WM (窗口管理)      │
│              │  Dock1         │  ├─ Dock (任务栏)      │
│              │  Launcher1     │  ├─ Launcher (启动器)  │
│              │  Appearance1   │  └─ Appearance (外观)  │
└──────────────┘                └──────────────────────┘
```

### 10.2 实现

```rust
// backends/dde/src/dde_api.rs

pub struct DdeCompositor {
    kwin: KWinCompositor,   // 复用 KWin 合成器组件
    dde: DdeApi,         // 补充 DDE 专有接口
}

#[async_trait]
impl CompositorComponent for DdeCompositor {
    fn name(&self) -> &'static str { "DDE (Wayland, deepin-kwin)" }
    fn de_type(&self) -> DesktopEnvironment { DesktopEnvironment::DDE }

    // 窗口管理：委托 KWin
    async fn list_windows(&self) -> Result<Vec<WindowInfo>> { self.kwin.list_windows().await }

    // 补充 DDE 专有能力（如窗口缩略图 / 拆分区域）
    pub async fn split_window(&self, id: &WindowId, region: SplitRegion) -> Result<()> {
        self.kwin.set_window_geometry(id, region_to_rect(region)).await
    }
}
```

**DDE 检测注意**：DDE 下 `XDG_CURRENT_DESKTOP=Deepin`，但 KWin D-Bus 服务名仍为 `org.kde.KWin`。需先判 DDE 再判 KDE（deepin-kwin 与 KWin 同接口但行为有差异，如 dde-shell 替换了部分 Plasma 组件）。

### 10.3 Wayland 协议路径（未来 Treeland，首选）

deepin 新一代 compositor **Treeland**（基于 wlroots，deepin 25+ 过渡）提供完整 Wayland 协议路径：

| 协议 | 版本 | 能力 |
|------|:----:|------|
| `treeland_foreign_toplevel_manager` | 1 | 完整窗口管理：set/unset_maximized/minimized、activate(seat)、close()、set_fullscreen、set_rectangle |
| `treeland_window_management` | 1 | 桌面状态：normal/show/preview_show |
| `treeland-capture` | - | 输出/窗口内容捕获 |
| `treeland-output-manager` + `virtual-output` | - | 输出配置 + 虚拟显示器 |
| `treeland-shortcut-manager` | - | 快捷键管理 |
| `treeland-dde-shell` | - | DDE shell 集成 |
| `treeland-wallpaper-manager/shell/color` | - | 壁纸管理 |

**设计决策（D9）**：DDE backend 实现为复合装配，优先 Treeland Wayland 协议，其次 deepin-kwin（复用 KWin 路径），最后 dde-api D-Bus。Treeland 迁移完成后可丢弃 `org.deepin.dde.*` 双版本兼容层。

### 10.4 实现设计

**模块结构**（`backends/dde/src/`）：

| 文件 | 职责 |
|------|------|
| `lib.rs` | 模块根 + re-export（DdeBackend / DdeCompositor） |
| `assemble.rs` | DdeBackend 装配清单（合成器 + 系统服务装配） |
| `compositor.rs` | Compositor 检测（Treeland vs deepin-kwin vs X11） |
| `dde_compositor.rs` | DdeCompositor + CompositorComponent impl |
| `dde_api.rs` | DDE 系统服务 D-Bus 封装（7+ 服务） |
| `dde_audio.rs` | org.deepin.dde.Audio1 / com.deepin.daemon.Audio 音频封装 |
| `treeland.rs` | Treeland Wayland 协议客户端 |
| `protocol_gen.rs` | treeland 协议生成绑定 |
| `version.rs` | DDE 版本探测（20 vs 25） |

**核心接口**：

```rust
pub enum Compositor { DeepinKwin(KWinCompositor), Treeland(Treeland), X11(X11DisplayServer) }

pub struct DdeCompositor {
    pub compositor: Compositor,
    pub dde: DdeApi,
    pub version: DdeVersion,
}
```

窗口管理全部委托给 compositor 后端。系统服务（音量/显示/电源/通知/壁纸）走 `DdeApi`。

**Compositor 检测**：
```rust
pub async fn detect_compositor() -> CompositorType {
    // 1. XDG_SESSION_TYPE == "wayland"
    // 2. 检查 registry globals:
    //    a. org_kde_plasma_window_management → deepin-kwin
    //    b. wlr-foreign-toplevel-management → Treeland
    // 3. X11: 检查 DISPLAY
}
```

**DdeApi**（backend dde 的 DE 封装服务封装，实现公共组件接口，DE 优先）：
- `get_monitor_layout()`：Display1.GetAll() → monitors 数组 → MonitorLayout
- `set_volume(volume)`：获取 DefaultSink 路径 → SetVolume(volume)
- `set_mute(muted)`：获取 DefaultSink 路径 → SetMute(muted)
- `lock_screen()`：LockService1.LockNow() 或 DDE20 ScreenSaver.Lock()
- `set_wallpaper(path)`：Appearance1.SetMonitorBackground(monitor_id, path)

**DDE 版本兼容**：

| 能力 | DDE 25 服务名 | DDE 20 服务名 |
|------|--------------|--------------|
| 显示 | org.deepin.dde.Display1 | com.deepin.daemon.Display |
| 音频 | org.deepin.dde.Audio1 | com.deepin.daemon.Audio |
| 电源 | org.deepin.dde.Power1 | com.deepin.daemon.Power |
| 通知 | org.deepin.dde.Notification1 | com.deepin.daemon.Notification |

**Treeland 后端**：`treeland_foreign_toplevel_manager` 提供 activate/close/maximize/minimize/fullscreen；`treeland-window-management` 提供桌面状态控制。

**会话无关的共享能力**：

| 能力 | Wayland 会话 | X11 会话 | 共享？ |
|------|-------------|---------|:------:|
| DdeApi 系统服务（Display1/Audio1/Power1/...） | 同 | 同 | ✅ 共享 |
| DDE 版本检测（20 vs 25 服务名路由） | 同 | 同 | ✅ 共享 |
| Compositor 探测 | registry globals | DISPLAY env | 各走各路 |
| 后端组合 | Treeland/org_kde_* 协议 | org.deepin.dde.* D-Bus | 各走各路 |

**验证输出**：

✓ DDE 版本     : DDE 25 (dde-daemon 6.1.84)
✓ Compositor  : deepin-kwin 6.1.17 (Wayland)
✓ DDE 服务     : 8/10 reachable (Display1, Audio1, Power1, Notify1, Lock1, Network1, Clipboard1)
```

---

## 11. 合成器：兜底（X11 / WLRWayland / TTY）

`X11Compositor` 实现 CompositorComponent，**通用 X11 兜底合成器**。组合 `X11DisplayServer`，通过 x11rb 原生协议实现全部窗口操作，CLI 工具仅作最后保底。

```rust
// components/displayserver/x11/src/lib.rs（协议层，非合成器）

pub struct X11DisplayServer { conn: x11rb::Connection }

impl X11DisplayServer {
    // 窗口列表：EWMH _NET_CLIENT_LIST（原生）
    // 聚焦：_NET_ACTIVE_WINDOW ClientMessage（原生）
    // 移动/缩放：x11rb::configure_window（原生）
    // 状态：_NET_WM_STATE ClientMessage（原生）
    // 截图：MIT-SHM / XGetImage（原生）
    // 输入：XTest 扩展（原生）
}
```

### 11.1 实现设计

**WlrWaylandCompositor**：wlroots 系合成器的基类，**自身就是完整实现**，可直接装配使用；
TreelandCompositor、HyprlandCompositor、SwayCompositor **组合**它（`self.base`）并叠加各自私有协议扩展。
组合 `WaylandDisplayServer` 并绑定 `wlr-*` 协议族（foreign-toplevel-management、virtual-pointer、screencopy 等），
实现全部 CompositorComponent 能力（窗口列表/聚焦/移动/关闭/工作区/截图），无需外部工具。

继承层次（Rust 无实现继承，`WlrWaylandCompositor` 是 struct，子类组合非继承）：

```text
CompositorComponent（trait）
└── WaylandCompositor（components/compositor/wayland-core——抽象基类，组合 WaylandDisplayServer）
    ├── WlrWaylandCompositor（components/compositor/wlr-wayland，叠加 wlr 标准协议）
    │   └── HyprlandCompositor / SwayCompositor（组合 self.base；Treeland 协议客户端在 backends/dde/src/treeland.rs）
    ├── KWinCompositor（org_kde_* 私有协议）
    └── MutterCompositor（D-Bus Eval/Extension）
```

```rust
// components/compositor/wayland-core/src/compositor.rs
pub trait WaylandCompositor: CompositorComponent {
    fn display_server(&self) -> &WaylandDisplayServer;
}

// components/compositor/wlr-wayland/src/compositor.rs
pub struct WlrWaylandCompositor {
    display: WaylandDisplayServer,   // wl_display 连接
    bindings: WlrBindings,           // wlr-* 协议绑定（7 项）
    windows: WindowCache,            // foreign-toplevel 事件驱动缓存
}

#[async_trait]
impl CompositorComponent for WlrWaylandCompositor {
    // 完整实现：list_windows / focus_window / move_window / close_window
    //   / workspace_list / switch_workspace / screenshot
    // 全部基于 wlr-* 协议，portal/AT-SPI 仅作补充降级
}

// 子类组合基类，追加私有协议；不复制 wlr 实现：
pub struct HyprlandCompositor {
    base: WlrWaylandCompositor,      // 组合 wlr 完整实现
    bindings: HyprlandBindings,      // hyprland_* 私有协议
    hyprctl: Hyprctl,                // + hyprland 专有通道
}
```

因此可实例化场景：
1. **无法识别具体 DE**（无 KDE/DDE/GNOME/Hyprland/Sway 特征）的 Wayland 会话 → 直接装配本类
2. **wlroots 系 DE 的基类**（Treeland/Hyprland/Sway 组合，叠加私有协议覆盖/增强）
所有窗口操作走 wlr 协议原生路径，portal/AT-SPI 仅当 wlr 协议缺失时降级。

**X11Compositor**：当检测到 X11 会话但无法识别具体 WM，装配 `X11Compositor`。
组合 `X11DisplayServer`（协议层在 `components/displayserver/x11/`，非合成器），通过 x11rb 原生协议实现全部操作。

**模块结构**（`components/compositor/x11/src/`）：

| 文件 | 职责 | 层级 |
|------|------|------|
| `lib.rs` | 模块根 + re-export（X11Compositor） | 主入口 |
| `compositor.rs` | X11Compositor + CompositorComponent impl | 主入口 |
| `ewmh.rs` | EWMH/_NET_WM 原子操作（原生 x11rb） | 原生 |
| `xtest.rs` | XTest 扩展输入注入（原生 x11rb） | 原生 |
| `capture.rs` | MIT-SHM / XGetImage 截图（原生 x11rb） | 原生 |
| `commands.rs` | xdotool/wmctrl 命令封装（仅保底） | 降级 |

> 协议层 `X11DisplayServer` 在 `components/displayserver/x11/src/`（lib.rs / ewmh/{mod,server}.rs / icccm/mod.rs），
> 不在此合成器 crate 内；本 crate 无 `event.rs`（X11 无合成器级原生事件流，XDamage/XRecord 可选）。

**核心接口**：

```rust
pub struct X11DisplayServer {
    conn: x11rb::Connection,
    ewmh: Ewmh,
    xtest: Option<XTestState>,  // XTest 扩展（原生）
    cmd: Option<X11Commands>,   // CLI 工具（仅保底）
    root_window: Window,
    screen_num: usize,
}
```

**原生协议实现路径**（无需外部工具）：

| 操作 | 实现方式 | 协议 |
|------|---------|------|
| 窗口列表 | `_NET_CLIENT_LIST` get_property | EWMH |
| 聚焦 | `_NET_ACTIVE_WINDOW` ClientMessage | EWMH |
| 移动/缩放 | `x11rb::configure_window` + `_NET_MOVERESIZE_WINDOW` | X11 core + EWMH |
| 最小化 | `_NET_WM_STATE` ClientMessage (_NET_WM_STATE_HIDDEN) | EWMH |
| 最大化 | `_NET_WM_STATE` ClientMessage (_NET_WM_STATE_MAXIMIZED_VERT/HORZ) | EWMH |
| 关闭 | `_NET_CLOSE_WINDOW` ClientMessage | EWMH |
| 获取窗口信息 | `GetGeometry` + `_NET_WM_NAME` + `WM_CLASS` + `_NET_WM_PID` | X11 core + EWMH |
| 工作区列表 | `_NET_NUMBER_OF_DESKTOPS` + `_NET_DESKTOP_NAMES` + `_NET_CURRENT_DESKTOP` | EWMH |
| 切换工作区 | `_NET_CURRENT_DESKTOP` ClientMessage | EWMH |
| 移窗口到工作区 | `_NET_WM_DESKTOP` ClientMessage | EWMH |
| 键鼠输入 | `xtest_fake_input` (XTest) | XTest 扩展 |
| 截图 | `xshm_get_image` (MIT-SHM) / `get_image` (XGetImage) | MIT-SHM / X11 core |

**CLI 工具保底**（仅当原生路径失败时）：xdotool/wmctrl 作为最后手段，非必需依赖。

**验证输出**：
```
✓ X11 连接     : :0 (screen 0, 1920x1080)
✓ 合成器   : Xfwm4 (Xfce 4.18)
✓ EWMH 协议   : 12/12 atoms supported
✓ XTest 扩展  : v2.2 ✓ (原生输入注入)
✓ MIT-SHM     : enabled ✓ (零拷贝截图)
⚠ 事件流       : 无原生支持（XDamage 可选）
```

### 11.2 TTY（纯终端）模式

**TtyBackend**：当检测到纯终端环境（无 Wayland/X11、无 DE），装配 `TtyBackend`。
它不是合成器，而是**无 DE 的操作系统 Agent**——仅提供系统服务（init、session、进程、文件、日志、网络）。

```rust
// backends/tty/src/mod.rs
pub struct TtyBackend;

impl TtyBackend {
    pub async fn assemble(&self, event_hub: EventHub) -> Result<ComponentRegistry> {
        Ok(ComponentRegistry {
            compositor: None,              // 无合成器
            audio: None,                   // 无音频
            network: detect_network().await?,  // systemd-networkd / NetworkManager（可选）
            input: None,                   // 无输入注入
            capture: None,                 // 无截图
            a11y: None,                    // 无无障碍
            clipboard: None,               // 无剪贴板
            power: None,                   // 无电源管理（或 UPower 可选）
            notification: None,            // 无通知
            appearance: None,              // 无外观
            launcher: None,                // 无启动器
            init_system: Some(Box::new(SystemdComponent::new().await?)),
            session_manager: Some(Box::new(LogindComponent::new().await?)),
        })
    }
}
```

**可用能力**：
- 系统服务管理（systemd units）
- 会话管理（logind lock/reboot/poweroff）
- 系统日志（journalctl）
- 进程管理（ps/kill）
- 系统信息（uname/uptime/memory/disk）
- 网络状态（NetworkManager D-Bus / systemd-networkd）
- 文件系统操作

**不可用能力**：
- 窗口/工作区/监视器操作（无合成器）
- 输入注入/截图/剪贴板（无显示服务器）
- 无障碍（无 AT-SPI 总线）
- 通知/外观/启动器（无 DE 服务）

---

## 12. 输入子系统

### 12.1 架构

```
┌──────────────┐
│  Input       │
│  Dispatcher  │
│              │
│  首选 libei  │──► libei (EIS)  ──► compositor (Wayland 原生)
│  降级 ydotool│──► ydotool       ──► /dev/uinput ──► 内核
│  X11 原生    │──► XTest 扩展     ──► x11rb::xtest_fake_input (仅 X11 会话)
│  再降级      │──► xdotool       ──► XTest (仅保底)
└──────────────┘
```

### 12.2 InputService trait

```rust
// input/src/dispatcher.rs

#[async_trait]
pub trait InputService: Send + Sync {
    fn name(&self) -> &'static str;
    async fn is_available(&self) -> bool;
    async fn send_key(&self, combo: &KeyCombo) -> Result<()>;
    async fn type_text(&self, text: &str, delay_ms: u32) -> Result<()>;
    async fn mouse_move(&self, x: i32, y: i32) -> Result<()>;
    async fn mouse_click(&self, button: MouseButton) -> Result<()>;
    async fn mouse_scroll(&self, dx: i32, dy: i32) -> Result<()>;
}

pub struct InputDispatcher {
    backends: Vec<Box<dyn InputService>>,
    active: Option<usize>,
}

impl InputDispatcher {
    pub async fn new(de_type: DesktopEnvironment) -> Result<Self> {
        let mut backends: Vec<Box<dyn InputService>> = Vec::new();
        // 1. libei/EIS (Wayland 首选)
        if de_type != DesktopEnvironment::X11Generic {
            backends.push(Box::new(LibeiInput::new().await?));
        }
        // 2. ydotool (跨 DE 保底)
        if which("ydotool").is_ok() && Path::new("/dev/uinput").exists() {
            backends.push(Box::new(YdotoolInput::new()));
        }
        // 3. XTest 扩展 (X11 原生)
        if de_type == DesktopEnvironment::X11Generic {
            if let Ok(xtest) = XTestInput::new().await {
                backends.push(Box::new(xtest));
            }
        }
        // 4. xdotool (X11 保底)
        if de_type == DesktopEnvironment::X11Generic && which("xdotool").is_ok() {
            backends.push(Box::new(XdotoolInput::new()));
        }
        // 选择第一个可用后端
        let active = backends.iter().enumerate()
            .find(|(_, b)| b.is_available().await).map(|(i, _)| i);
        Ok(Self { backends, active })
    }

    pub async fn send_key(&self, combo: &KeyCombo) -> Result<()> {
        self.active_backend()?.send_key(combo).await
    }
    pub async fn type_text(&self, text: &str, delay_ms: u32) -> Result<()> {
        self.active_backend()?.type_text(text, delay_ms).await
    }
    // ... mouse_move, click, scroll
}
```

### 12.3 libei/EIS 后端

通过 `xdg-desktop-portal.RemoteDesktop` 获取 EIS 连接：

```
1. portal.CreateSession → session handle
2. portal.SelectDevices(session, KEYBOARD | POINTER)
3. portal.Start(session) → 用户确认弹窗
4. portal.ConnectToEIS(session) → fd
5. libei ei_setup_backend_fd(fd) → 建立 EI 协议连接
6. 通过 EI 协议发送键盘/指针事件
```

Rust 侧需 libei-sys FFI 或直接实现 EI 协议（二进制长度前缀消息）。`libei` 1.0 已发布，含 `eis`（服务端）和 `ei`（客户端）两个库。

### 12.4 ydotool 后端

通过 `ydotool` 命令调用，依赖 `ydotoold` 守护进程：

```rust
impl InputService for YdotoolInput {
    async fn send_key(&self, combo: &KeyCombo) -> Result<()> {
        run_cmd(&format!("ydotool key {}", combo_to_ydotool(&combo)))
    }
    async fn mouse_move(&self, x: i32, y: i32) -> Result<()> {
        run_cmd(&format!("ydotool mousemove {} {}", x, y))
    }
    async fn mouse_click(&self, button: MouseButton) -> Result<()> {
        let btn = match button { Left => "1", Middle => "2", Right => "3", Back => "8", Forward => "9" };
        run_cmd(&format!("ydotool click {}", btn))
    }
}
```

---

## 13. 截图与捕获子系统

### 13.1 架构

```
┌──────────────┐
│  Capture     │
│  Dispatcher  │
│              │
│  首选 Portal │──► xdg-desktop-portal.ScreenCast → PipeWire → 帧
│  降级 Screenshot│──► xdg-desktop-portal.Screenshot → PNG
│  再降级 Native│──► KWin 截图 / DE 专有截图
└──────────────┘
```

### 13.2 ScreenCast (PipeWire 流式)

```rust
// capture/src/portal_screencast.rs

pub struct ScreenCastCapture {
    session: PortalSession,
    pw_node: PipeWireNode,
}

impl ScreenCastCapture {
    /// 通过 ScreenCast portal 获取 PipeWire 流
    pub async fn start(target: CaptureTarget) -> Result<Self> {
        let portal = ScreenCastPortalProxy::new(&session_bus).await?;
        let session = portal.create_session(options).await?;
        portal.select_sources(&session, source_type_from(target)).await?;
        portal.start(&session, "").await?;
        let (node_id, fd) = portal.open_pipewire_remote(&session).await?;
        let pw = PipeWireNode::new(fd, node_id)?;
        Ok(Self { session, pw_node: pw })
    }

    /// 捕获单帧
    pub async fn capture_frame(&self) -> Result<Vec<u8>> {
        self.pw_node.capture_frame().await
    }
}
```

### 13.3 Screenshot Portal

```rust
// capture/src/portal_screenshot.rs

pub struct ScreenshotPortal { portal: ScreenshotProxy }

impl ScreenshotPortal {
    pub async fn capture(target: ScreenshotTarget) -> Result<PathBuf> {
        let portal = ScreenshotProxy::new(&session_bus).await?;
        let request = portal.screenshot("", dict! { "target" => target as u32 }).await?;
        // 等待 Request::Response 信号获取 uri → 转为本地路径
        let (result, _) = wait_for_response(&request).await?;
        let uri = result["uri"].as_str().ok_or(...)?;
        Ok(parse_uri_to_path(uri))
    }
}
```

### 13.4 变化检测缓存

```rust
// capture/src/cache.rs

pub struct CaptureCache {
    last_frame: Option<Vec<u8>>,
    threshold: f64,  // 默认 0.01 (1% 像素变化)
}

impl CaptureCache {
    /// 快速像素级差分检测
    pub fn has_changed(&self, new_frame: &[u8]) -> bool {
        let Some(last) = &self.last_frame else { return true };
        if last.len() != new_frame.len() { return true; }
        // 逐像素 RGBA 比较，忽略 alpha
        let changed = last.chunks_exact(4).zip(new_frame.chunks_exact(4))
            .filter(|(a, b)| a[0].abs_diff(b[0]) > 10 ||
                              a[1].abs_diff(b[1]) > 10 ||
                              a[2].abs_diff(b[2]) > 10)
            .count();
        (changed as f64 / (last.len() / 4) as f64) > self.threshold
    }
}
```

---

## 14. AT-SPI 无障碍模块

### 14.1 架构

```
Agent Shell
  ├── a11y::atspi_bridge      → AT-SPI D-Bus (a11y bus, org.a11y.atspi.*)
  │     ├── 获取应用无障碍树
  │     ├── 查询元素属性
  │     └── 触发元素操作
  ├── a11y::tree              → 无障碍树模型 (ApplicationNode/WindowNode/ElementNode)
  ├── a11y::semantic_locator  → 语义定位引擎 (按角色/名称/属性/相对定位)
  └── a11y::action            → 元素操作封装 (click/focus/get_text/set_text)
```

### 14.2 核心桥接

```rust
// a11y/src/atspi_bridge.rs

pub struct AtspiBridge { connection: zbus::Connection }

impl AtspiBridge {
    /// 获取桌面根节点（所有应用）
    pub async fn get_desktop(&self) -> Result<ApplicationNode> {
        let registry = RegistryProxy::new(&self.connection).await?;
        registry.get_desktop(0).await.map(Into::into)
    }

    /// 获取指定 PID 的无障碍树
    pub async fn get_app_tree(&self, pid: u32) -> Result<ApplicationNode> {
        let desktop = self.get_desktop().await?;
        desktop.children().into_iter()
            .find(|c| c.pid() == pid)
            .ok_or(AgentShellError::WindowNotFound(format!("PID {}", pid)))
    }

    /// 获取当前活动窗口
    pub async fn get_active_window(&self) -> Result<WindowNode> {
        let desktop = self.get_desktop().await?;
        for app in desktop.children() {
            for window in app.children() {
                let states = window.get_state().await?;
                if states.contains(AtspiState::ACTIVE) || states.contains(AtspiState::FOCUSED) {
                    return Ok(window);
                }
            }
        }
        Err(AgentShellError::WindowNotFound("no active window".into()))
    }
}
```

### 14.3 语义定位引擎

```rust
// a11y/src/semantic_locator.rs

pub struct SemanticLocator { bridge: AtspiBridge }

impl SemanticLocator {
    /// 按语义描述定位元素
    pub async fn locate(&self, target: &SemanticTarget) -> Result<Vec<ElementNode>> {
        match target {
            SemanticTarget::ByAccessibility { role, name, parent_role, parent_name } => {
                let desktop = self.bridge.get_desktop().await?;
                let mut results = Vec::new();
                for window in self.find_all_windows(&desktop).await {
                    if let Some(parent) = parent_role.zip(parent_name) {
                        if let Ok(p) = self.find_element(&window, &parent.0, &parent.1).await {
                            results.extend(self.find_elements_in_parent(&p, role, name).await);
                        }
                    } else {
                        results.extend(self.find_elements_in_parent(&window, role, name).await);
                    }
                }
                Ok(results)
            }
            _ => Err(AgentShellError::NotImplemented("locator strategy".into()))
        }
    }
}
```

### 14.4 元素操作

```rust
// a11y/src/action.rs

pub struct ElementActions { bridge: AtspiBridge, input: InputDispatcher }

impl ElementActions {
    /// 点击元素：优先 AT-SPI Action 接口，降级坐标点击
    pub async fn click(&self, element: &ElementNode) -> Result<()> {
        let action = element.get_action_interface().await?;
        if action.get_n_actions() > 0 {
            action.do_action(0).await?;  // 0 = "click"
            return Ok(());
        }
        // 降级：获取元素中心坐标 → 鼠标移动 + 点击
        let rect = element.get_rect().await?;
        let cx = rect.x + rect.width / 2;
        let cy = rect.y + rect.height / 2;
        self.input.mouse_move(cx, cy).await?;
        self.input.mouse_click(MouseButton::Left).await
    }

    /// 获取元素文本
    pub async fn get_text(&self, element: &ElementNode) -> Result<String> {
        let text = element.get_text_interface().await?;
        text.get_text(0, -1).await.map_err(Into::into)
    }

    /// 输入文本（AT-SPI EditableText 接口）
    pub async fn set_text(&self, element: &ElementNode, text: &str) -> Result<()> {
        let editable = element.get_editable_text_interface().await?;
        editable.set_text_contents(text).await?;
        Ok(())
    }
}
```

---

## 15. 语义路由层

### 15.1 命令模型

```rust
// router/src/command.rs

pub enum Command {
    // 窗口
    ListWindows { filter: Option<WindowFilter> },
    GetActiveWindow,
    FocusWindow { target: SemanticTarget },
    MoveWindow { target: SemanticTarget, x: i32, y: i32 },
    ResizeWindow { target: SemanticTarget, w: i32, h: i32 },
    MinimizeWindow { target: SemanticTarget },
    CloseWindow { target: SemanticTarget },
    SetWindowGeometry { target: SemanticTarget, rect: Rect },

    // 工作区
    ListWorkspaces,
    SwitchWorkspace { id: WorkspaceId },
    MoveWindowToWorkspace { window: SemanticTarget, workspace: WorkspaceId },

    // 输入
    SendKey { combo: KeyCombo },
    TypeText { text: String, target: Option<SemanticTarget> },
    MouseClick { button: MouseButton, target: Option<SemanticTarget> },
    MouseMove { x: i32, y: i32 },
    Scroll { dx: i32, dy: i32 },

    // 截图
    Screenshot { target: CaptureTarget, output: Option<String> },

    // 无障碍
    GetElementText { target: SemanticTarget },
    GetA11yTree { target: Option<SemanticTarget> },

    // 等待
    WaitForWindow { app_id: String, timeout: Duration },
    WaitForText { text: String, timeout: Duration },

    // 系统
    GetDesktopInfo, GetBackendStatus,
}
```

### 15.2 执行引擎

```rust
// router/src/executor.rs

pub struct Executor {
    backend: Box<dyn CompositorComponent>,
    input: InputDispatcher,
    capture: CaptureDispatcher,
    a11y: ElementActions,
    locator: SemanticLocator,
}

impl Executor {
    pub async fn execute(&self, cmd: Command) -> Result<CommandResult> {
        match cmd {
            Command::FocusWindow { target } => {
                let win = self.resolve_target(&target).await?;
                self.backend.focus_window(&win.id).await?;
                Ok(CommandResult::Window(win))
            }
            Command::TypeText { text, target } => {
                if let Some(target) = &target {
                    // 优先 AT-SPI 语义输入
                    if let Ok(elements) = self.locator.locate(target).await {
                        if let Some(el) = elements.first() {
                            if self.a11y.set_text(el, &text).await.is_ok() {
                                return Ok(CommandResult::Success);
                            }
                        }
                    }
                }
                // 降级：键盘模拟
                self.input.type_text(&text, 0).await?;
                Ok(CommandResult::Success)
            }
            Command::Screenshot { target, output } => {
                let image = self.capture.capture(target).await?;
                let path = output.unwrap_or(format!("screenshot_{}.png", timestamp()));
                tokio::fs::write(&path, &image).await?;
                Ok(CommandResult::File(path))
            }
            Command::WaitForWindow { app_id, timeout } => {
                let start = Instant::now();
                loop {
                    let windows = self.backend.list_windows().await?;
                    if windows.iter().any(|w| w.app_id == app_id) {
                        return Ok(CommandResult::Success);
                    }
                    if start.elapsed() > timeout {
                        return Err(AgentShellError::Timeout(format!("'{}' not appeared", app_id)));
                    }
                    tokio::time::sleep(Duration::from_millis(100)).await;
                }
            }
            // ... 其他命令
        }
    }

    /// 解析语义目标为具体的 WindowInfo
    async fn resolve_target(&self, target: &SemanticTarget) -> Result<WindowInfo> {
        match target {
            SemanticTarget::Active =>
                self.backend.get_active_window().await?
                    .ok_or_else(|| AgentShellError::WindowNotFound("no active".into())),
            SemanticTarget::ById(id) => self.backend.get_window_info(id).await,
            SemanticTarget::ByAppId(id) => {
                let windows = self.backend.list_windows().await?;
                windows.into_iter().find(|w| w.app_id == *id)
                    .ok_or_else(|| AgentShellError::WindowNotFound(format!("app '{id}'")))
            }
            SemanticTarget::ByTitle(title, mode) => {
                let windows = self.backend.list_windows().await?;
                windows.into_iter().find(|w| title_matches(mode, title, &w.title))
                    .ok_or_else(|| AgentShellError::WindowNotFound(format!("title '{title}'")))
            }
            SemanticTarget::ByAccessibility { .. } => {
                // AT-SPI 定位 → 找到所属窗口 → 匹配 backend 窗口列表。
                // 双向子串匹配（有意为之）：Wayland 下 AT-SPI window name 常
                // 等于 app_id，X11 下常为标题子串——两者无规范映射。
                let elements = self.a11y.locate(target).await?;
                let el = elements.first()
                    .ok_or_else(|| AgentShellError::WindowNotFound("AT-SPI element".into()))?;
                let title = el.find_parent_window_name().await?;
                let windows = self.backend.list_windows().await?;
                windows.into_iter()
                    .find(|w| title.contains(&w.app_id) || w.title.contains(&title))
                    .ok_or_else(|| AgentShellError::WindowNotFound("AT-SPI window".into()))
            }
            // ByPid / ByDesktopFile / ByCoordinate / ByRegion 未实现——
            // SemanticTarget 枚举保留这些 variant，resolve 落 NotImplemented。
            other => Err(AgentShellError::NotImplemented(format!(
                "router: target resolution for {other:?}"
            ))),
        }
    }
}

/// 标题匹配（§15.3：子串 → 精确 → 正则 → glob）。
/// 正则编译失败按「不匹配」处理，不向上抛错。
fn title_matches(mode: &TitleMatchMode, pattern: &str, title: &str) -> bool {
    match mode {
        TitleMatchMode::Substring => title.contains(pattern),
        TitleMatchMode::Exact => title == pattern,
        TitleMatchMode::Regex => regex::Regex::new(pattern)
            .map(|r| r.is_match(title)).unwrap_or(false),
        TitleMatchMode::Glob => glob_match::glob_match(pattern, title),
    }
}
```

### 15.3 语义定位优先级

`resolve_target` 实际分支（实现现状）：

```
1. Active —— get_active_window()（直查）
2. ById —— get_window_info(id)（直查，WindowId 已可直查）
3. ByAppId —— list_windows().find(app_id 相等)
4. ByTitle —— list_windows().find(title_matches：子串 → 精确 → 正则 → glob)
5. ByAccessibility —— AT-SPI locate → 窗口名/标题与 backend 列表双向子串匹配
other —— NotImplemented（target resolution）
```

`SemanticTarget`（`core/src/types.rs`）保留 `ByPid` / `ByDesktopFile` /
`ByCoordinate` / `ByRegion` variant，但 `resolve_target` 不处理——
当前**未实现**，落 `NotImplemented`。窗口 ID（`ById`）不再需要
`list_windows` 扫描，直接 `get_window_info` 查询。

---

## 16. DE 检测与自动配置

### 16.1 检测策略

```rust
// core/src/de_detection.rs

pub fn detect_desktop_environment() -> DesktopEnvironment {
    // 1. XDG_CURRENT_DESKTOP 环境变量（首要信号）
    if let Ok(current) = env::var("XDG_CURRENT_DESKTOP") {
        let lower = current.to_lowercase();
        if lower.contains("deepin") || lower.contains("dde") { return DDE; }        // DDE 优先！
        if lower.contains("kde") || lower.contains("plasma") { return KDE; }
        if lower.contains("gnome") { return GNOME; }
        if lower.contains("hyprland") { return Hyprland; }
        if lower.contains("sway") { return Sway; }
        if lower.contains("cosmic") { return Cosmic; }
        if lower.contains("xfce") { return XFCE; }
        if lower.contains("cinnamon") { return Cinnamon; }
        if lower.contains("budgie") { return Budgie; }
        if lower.contains("lxqt") { return LXQt; }
        if lower.contains("mate") { return MATE; }
    }

    // 2. Wayland 会话：检查 D-Bus 服务名（可靠）
    if env::var("WAYLAND_DISPLAY").is_ok() {
        if env::var("HYPRLAND_INSTANCE_SIGNATURE").is_ok() { return Hyprland; }
        if dbus_service_exists("org.kde.KWin") { return KDE; }      // deepin-kwin 同名
        if dbus_service_exists("org.gnome.Shell") { return GNOME; }
    }

    // 3. X11 会话：检查 DISPLAY + WM
    if env::var("DISPLAY").is_ok() {
        return detect_x11_wm();   // _NET_SUPPORTING_WM_CHECK
    }

    // 4. 纯终端（TTY）：无 Wayland/X11，无 DE
    //    判据：stdin/stdout 是 tty 且无 WAYLAND_DISPLAY/DISPLAY
    if is_tty_session() && env::var("WAYLAND_DISPLAY").is_err() && env::var("DISPLAY").is_err() {
        return DesktopEnvironment::Tty;
    }

    DesktopEnvironment::Unknown
}
```

**要点**：
- DDE 判断必须在 KDE 之前（deepin-kwin 注册 org.kde.KWin 但 XDG_CURRENT_DESKTOP=Deepin）
- 最终以 D-Bus 服务探测为准（环境变量可能缺失或被覆盖）

### 16.2 后端装配

```rust
pub fn assemble_wm(de: DesktopEnvironment) -> Result<Box<dyn CompositorComponent>> {
    match de {
        DesktopEnvironment::DDE => Ok(Box::new(DdeCompositor::new()?)),
        DesktopEnvironment::KDE => Ok(Box::new(KWinCompositor::new()?)),
        DesktopEnvironment::GNOME => Ok(Box::new(MutterCompositor::new()?)),
        DesktopEnvironment::Hyprland => Ok(Box::new(HyprlandCompositor::new()?)),
        DesktopEnvironment::Sway => Ok(Box::new(SwayCompositor::new()?)),
        DesktopEnvironment::WLRWayland => Ok(Box::new(WlrWaylandCompositor::new()?)),
        DesktopEnvironment::X11Generic => Ok(Box::new(X11Compositor::new()?)),
        _ => Err(AgentShellError::UnsupportedDE(format!("{:?}", de))),
    }
}
```

### 16.3 doctor 诊断

```
$ agent-shell doctor
✓ DE 检测        : DDE (Wayland, deepin-kwin)
✓ 主后端        : dde (kwin scripting bridge OK, 12 windows discoverable)
✓ D-Bus 桥接    : org.kde.KWin reachable, callDBus roundtrip 2.3ms
✓ 输入后端      : libei/EIS via portal (keyboard + pointer)
  ├─ 降级1      : ydotool (daemon running)
  └─ 降级2      : n/a on Wayland
✓ 截图后端      : portal ScreenCast v6 (PipeWire)
  └─ 降级1      : portal Screenshot v3
✓ AT-SPI        : enabled (a11y bus up, 9 apps)
⚠ 事件流        : KWin event script loaded (workspace.windowAdded OK)
```

---

## 17. Agent 接口层

### 17.1 形态矩阵

| 形态 | 延迟 | 适用场景 | 传输 |
|------|------|---------|------|
| CLI | ~ms | 脚本化、调试、REPL | stdout |
| MCP | ~ms | Claude/Cline/Codex agent | stdio/SSE |
| JSON-RPC over stdio | ~ms | 自定义 agent 框架 | stdio |
| D-Bus 服务 | ~ms | 本地原生应用集成 | D-Bus |
| Unix Socket | ~ms | 事件流 + 长期会话 | AF_UNIX |
| HTTP/gRPC | ~10ms | 远程/集群 | TCP |

### 17.2 CLI 设计

```
agent-shell <command> [args...]

# 窗口
agent-shell windows [--filter APP_ID]
agent-shell window info <target>
agent-shell window focus <target>
agent-shell window move <target> X Y
agent-shell window resize <target> W H
agent-shell window minimize <target>
agent-shell window close <target>
agent-shell window geometry <target> X Y W H

# 工作区
agent-shell workspaces
agent-shell workspace switch <id>
agent-shell workspace move-window <window> <ws>

# 输入
agent-shell input key <key-combo>            # "ctrl+c", "meta+t"
agent-shell input type <text>
agent-shell input click [--button left] [--at X,Y]
agent-shell input scroll <dx> <dy>

# 截图
agent-shell screenshot [--window <target>] [--area X,Y,W,H] [-o out.png]

# 无障碍
agent-shell a11y tree [--app PID]
agent-shell a11y query --role button --name "确定"

# 事件
agent-shell events [--filter window,workspace]

# 系统
agent-shell doctor       # 后端健康诊断
agent-shell info         # DE/backend/能力报告

# REPL
agent-shell              # 交互模式（tab 补全）
```

### 17.3 MCP 服务器（工具定义）

```rust
// mcp/src/server.rs

router.register(Tool::new("list_windows")
    .description("列出所有窗口，含 app_id/title/pid/geometry/workspace")
    .input_schema(json!({
        "type": "object",
        "properties": {
            "app_id": {"type": "string"},
            "workspace": {"type": "integer"}
        }
    }))
    .handler(|params| async move {
        let filter = params.app_id.as_deref();
        let windows = backend.list_windows().await?;
        Ok(json!(windows.into_iter()
            .filter(|w| filter.map_or(true, |f| w.app_id == f))
            .collect::<Vec<_>>()))
    }));

router.register(Tool::new("focus_window")
    .description("聚焦窗口：支持 app_id/title/pid 定位")
    .input_schema(json!({
        "type": "object",
        "properties": {
            "app_id": {"type": "string"}, "title": {"type": "string"},
            "pid": {"type": "integer"}, "window_id": {"type": "string"}
        }
    })));

router.register(Tool::new("click_element")
    .description("语义化点击元素：按无障碍角色+名称，优先 AT-SPI，降级坐标")
    .input_schema(json!({
        "type": "object",
        "properties": {
            "role": {"type": "string", "enum": ["button","menu_item","checkbox"]},
            "name": {"type": "string"},
            "window": {"type": "string"},
            "fallback_coordinate": {"type": "array", "items": {"type": "number"}}
        }
    })));

router.register(Tool::new("screenshot")
    .description("截图：全屏/窗口/区域，返回文件路径")
    .input_schema(json!({
        "type": "object",
        "properties": {
            "target": {"type": "string", "enum": ["screen","window","area"]},
            "window": {"type": "string"},
            "area": {"type": "array", "items": {"type": "integer"}, "minItems": 4, "maxItems": 4}
        }
    })));

router.register(Tool::new("wait_for_window")
    .description("等待窗口出现，带超时")
    .input_schema(json!({
        "type": "object",
        "properties": {
            "app_id": {"type": "string"},
            "timeout_ms": {"type": "integer", "default": 15000}
        },
        "required": ["app_id"]
    })));

router.register(Tool::new("get_a11y_tree")
    .description("获取指定窗口的无障碍树（JSON）")
    .input_schema(json!({
        "type": "object",
        "properties": {
            "window": {"type": "string"},
            "depth": {"type": "integer", "default": 5}
        }
    })));

router.register(Tool::new("subscribe_events")
    .description("订阅桌面事件：window_opened/focused/closed, workspace_changed")
    .input_schema(json!({
        "type": "object",
        "properties": {
            "events": {"type": "array", "items": {"type": "string"}}
        }
    })));
```

### 17.4 SDK 编程接口（已并入 shell + rpc）

> 早期草案规划的独立 `sdk/` crate 未建立。顶层入口为 `shell` crate 的
> `AgentShell`，线协议类型在 `rpc` crate。不存在 `AgentShellClient`——
> 下文是设计的调用意图，以 shell/rpc 实际 API 形态呈现。

```rust
// shell/src/lib.rs

/// 顶层 AgentShell：事件枢纽 + 当前 backend + 公共组件装配结果。
pub struct AgentShell {
    pub event_hub: EventHub,
    pub backend: BackendKind,
    pub components: ComponentRegistry,
}

impl AgentShell {
    /// 检测环境 → 选定 backend → 装配组件（§4.2 装配流程）。
    pub async fn detect_and_assemble() -> Result<Self> { ... }
}

// rpc/src/lib.rs —— JSON-RPC 2.0 线协议类型（非 typed client）：
//   Request::new(id, method, params) / Response::ok / Notification::event(params)
//   及 WindowEntry / InputParams / CaptureParams / DoctorResult 等参数与结果 DTO。
```

调用方通过 `AgentShell::detect_and_assemble()` 得到 `ComponentRegistry`，直接
调用组件方法；跨进程/网络走 `rpc` 的 Request/Response/Notification 与 `cli`/
`mcp`/`daemon` 网关。

**验证输出**：
```
✓ Agent 形态    : CLI ✓, MCP ✓, JSON-RPC ✓, D-Bus ✓, shell ✓
✓ CLI 命令集     : 21 commands (window/workspace/monitor/system/agent)
✓ MCP 工具集     : 18 tools registered (list_windows, focus, move, ...)
✓ shell 绑定     : rust ✓（无独立 SDK crate，已并入 shell + rpc）
```

---

## 18. 事件流系统

### 18.1 事件模型

```rust
// core/src/event.rs


/// 事件优先级
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum EventPriority { High, Medium, Low }

/// 统一桌面事件（所有 Backend 归一化后的输出）
#[derive(Clone, Debug)]
pub enum DesktopEvent {
    // ========== 窗口事件（High） ==========
    /// 窗口打开
    WindowOpened {
        info: WindowInfo,              // 完整窗口信息
        source: EventSource,           // 事件源（kwin_wayland / hyprland / gnome / atspi）
        occurred_at: Instant,          // 事件发生时间
    },
    /// 窗口关闭
    WindowClosed {
        id: WindowId,
        source: EventSource,
        occurred_at: Instant,
    },
    /// 窗口聚焦变化
    WindowFocused {
        info: WindowInfo,
        source: EventSource,
        occurred_at: Instant,
    },
    /// 窗口移动/缩放（100ms 内合并，仅推送最终位置）
    WindowMoved {
        id: WindowId,
        geometry: Rect,
        source: EventSource,
        occurred_at: Instant,
    },
    /// 窗口状态变化（最大化/最小化/全屏/置顶）
    WindowStateChanged {
        id: WindowId,
        states: Vec<WindowState>,      // 变化后的完整状态集合
        source: EventSource,
        occurred_at: Instant,
    },
    /// 窗口标题/应用名变化（低频率，不合并）
    WindowMetadataChanged {
        id: WindowId,
        title: Option<String>,
        app_id: Option<String>,
        source: EventSource,
        occurred_at: Instant,
    },
    /// 窗口层叠顺序变化（z-order）
    WindowStackingChanged {
        ids: Vec<WindowId>,            // 从顶到底
        source: EventSource,
        occurred_at: Instant,
    },

    // ========== 工作区事件（Medium） ==========
    /// 工作区切换
    WorkspaceChanged {
        info: WorkspaceInfo,
        source: EventSource,
        occurred_at: Instant,
    },
    /// 工作区增删
    WorkspaceListChanged {
        workspaces: Vec<WorkspaceInfo>,
        source: EventSource,
        occurred_at: Instant,
    },
    /// 窗口移入/移出工作区
    WorkspaceWindowMoved {
        window_id: WindowId,
        from: Option<WorkspaceId>,
        to: Option<WorkspaceId>,
        source: EventSource,
        occurred_at: Instant,
    },

    // ========== 监视器事件（Medium） ==========
    /// 监视器热插拔
    MonitorHotplug {
        monitor: MonitorInfo,
        added: bool,                   // true=接入, false=移除
        source: EventSource,
        occurred_at: Instant,
    },
    /// 监视器分辨率/缩放变化
    MonitorChanged {
        info: MonitorInfo,
        source: EventSource,
        occurred_at: Instant,
    },

    // ========== 输入事件（Medium） ==========
    /// 鼠标点击 → 可映射为 WindowFocused
    PointerButton {
        window_id: Option<WindowId>,   // 点击位置下的窗口
        button: MouseButton,
        pressed: bool,
        position: (i32, i32),
        source: EventSource,
        occurred_at: Instant,
    },
    /// 键盘快捷键触发
    KeyComboPressed {
        combo: KeyCombo,
        source: EventSource,
        occurred_at: Instant,
    },

    // ========== 系统事件（Low） ==========
    /// 应用启动
    AppLaunched {
        app_id: String,
        pid: u32,
        desktop_file: Option<String>,
        source: EventSource,
        occurred_at: Instant,
    },
    /// 应用退出
    AppExited {
        pid: u32,
        source: EventSource,
        occurred_at: Instant,
    },
    /// 全屏状态变化
    FullscreenChanged {
        enabled: bool,
        window_id: Option<WindowId>,
        source: EventSource,
        occurred_at: Instant,
    },
    /// 电源状态变化（DPMS/休眠）
    PowerStateChanged {
        state: PowerState,             // On / Suspend / Hibernate / Off
        source: EventSource,
        occurred_at: Instant,
    },
    /// 无障碍树变化（仅 AT-SPI 源）
    AccessibilityTreeChanged {
        app_pid: u32,
        change_type: AccessibilityChange,  // NodeAdded / NodeRemoved / PropertyChanged
        source: EventSource,
        occurred_at: Instant,
    },
    /// 保底/无操作（归一化丢弃）
    Noop,
}

/// 事件源标识（用于过滤和调试）
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum EventSource {
    KWinWayland, KWinX11, MutterShell, MutterEval, MutterExtension,
    Hyprland, Treeland, Sway, WlrWayland,
    X11Generic, AtSpi, Input, Portal, Power,
}

/// 事件源 → 事件类型映射表
///
/// | 事件源 | 提供的事件 |
/// |--------|-----------|
/// | KWin Wayland (org_kde_* 协议) | WindowOpened/Closed/Focused, WorkspaceChanged, MonitorHotplug |
/// | KWin Scripting (D-Bus 信号) | WindowOpened/Closed, ActiveWindowChanged, WorkspaceChanged |
/// | Hyprland (.socket2.sock) | openwindow, closewindow, activewindow, movewindow, workspace, monitor |
/// | Mutter Shell Extension | WindowOpened/Closed, ActiveWindowChanged （自定义信号） |
/// | Mutter Shell.Eval | 主动轮询（无事件推送，走 polling 桥接） |
/// | Treeland (treeland_* 协议) | treeland_events 事件通道 |
/// | Sway IPC (事件订阅) | window::, workspace::, mode:: |
/// | AT-SPI (a11y 总线) | AccessibilityTreeChanged, 应用启停 |
/// | Input (libei 事件) | PointerButton, KeyComboPressed |
/// | Power (UPower / logind) | PowerStateChanged |

#[derive(Clone, Debug, Default)]
pub struct EventFilter {
    pub window_events: bool,
    pub workspace_events: bool,
    pub monitor_events: bool,
    pub input_events: bool,
    pub app_events: bool,
    pub a11y_events: bool,
    pub power_events: bool,
    pub priority: Option<EventPriority>,  // None = 全部
}

pub struct EventHub {
    subscribers: HashMap<Uuid, mpsc::Sender<DesktopEvent>>,  // 有界通道
    capacity: usize,  // 默认 1024
}

impl EventHub {
    pub fn publish(&self, event: DesktopEvent) {
        for tx in self.subscribers.values() {
            // 有界通道：高优先级阻塞，低优先级 try_send
            match event.priority() {
                EventPriority::High => { let _ = tx.try_send(event.clone()); },
                EventPriority::Medium | EventPriority::Low => { let _ = tx.try_send(event.clone()); },
            }
        }
    }
    pub fn subscribe(&mut self, filter: EventFilter) -> EventSubscription { ... }
}

/// 事件优先级推导
impl DesktopEvent {
    pub fn priority(&self) -> EventPriority {
        match self {
            DesktopEvent::WindowOpened { .. } | DesktopEvent::WindowClosed { .. }
            | DesktopEvent::WindowFocused { .. } => EventPriority::High,
            DesktopEvent::WindowMoved { .. } | DesktopEvent::WorkspaceChanged { .. }
            | DesktopEvent::MonitorHotplug { .. } | DesktopEvent::MonitorChanged { .. }
            | DesktopEvent::PointerButton { .. } => EventPriority::Medium,
            _ => EventPriority::Low,
        }
    }
}
```

### 18.2 事件归一化
合成器侧 `CompositorComponent::subscribe()` 返回 `core::event::EventStream`，由具体
组件包装为 `RawSource`（`event/src/adapter.rs`）再交 `EventNormalizer` 归一化推入 EventHub。
`EventSource` 保留为事件源标识枚举（core/src/event.rs / event/src/events.rs 两处同形，
见 §18.1）——与 `RawSource` trait 不同名、不同物。
```rust
// event/src/adapter.rs
pub trait RawSource: Send + Sync {
    fn source_name(&self) -> &'static str;
    fn source_kind(&self) -> crate::EventSource;
    fn events(&self) -> BoxStream<'static, RawEvent>;
}

/// 设计文档 §18.2 中 `EventSource` trait 的别名——crate 内 `EventSource`
/// 名称已被 core 的事件源标识枚举占用（§18.1）。
pub type SourceAdapter = dyn RawSource;

pub struct EventNormalizer {
    hub: EventHub,
    sources: Vec<Box<dyn RawSource>>,
    resolve: Option<crate::normalize::WindowResolver>,
    ring: crate::ring::EventRing,
}

impl EventNormalizer {
    pub fn run(self) -> Vec<&'static str> {
        // 为每个源 spawn 一个 tokio task，归一化后 publish + 写入环形缓冲。
        // 返回启动的源名列表（供 doctor 输出）。
    }
}
```

**事件保真策略**：
- 冲突时以「窗口 ID + 时间戳」关联
- 合并 100ms 内的连续 WindowMoved，避免事件风暴
- 所有事件携带 `backend` 标记，便于按 DE 过滤

### 18.3 事件归一化伪代码

```rust
// event/src/normalize.rs

pub async fn normalize(raw: RawEvent) -> DesktopEvent {
    match raw {
        // KWin/Hyprland/DDE 的 windowOpened 语义不同，统一为 WindowOpened
        RawEvent::KWinWindowAdded { id } |
        RawEvent::HyprlandOpenWindow { address } |
        RawEvent::DdeWindowOpened { id } => {
            DesktopEvent::WindowOpened {
                window: resolve_window_info_by_id(id).await?,
                occurred_at: now(),
            }
        }
        // 输入事件 → 窗口事件（点击某窗口 = 聚焦该窗口）
        RawEvent::PointerButtonPressed { x, y, button } => {
            if let Some(w) = window_at(x, y).await? {
                DesktopEvent::WindowFocused { window: w, occurred_at: now() }
            } else { DesktopEvent::Noop }
        }
        _ => DesktopEvent::Noop,
    }
}
```

### 18.4 事件风暴与背压

- EventHub 使用 `tokio::sync::mpsc` 有界通道（capacity=1024），超限丢弃 `Low` 优先级事件
- 组件事件源统一实现 `RawSource` trait（`SourceAdapter = dyn RawSource`），上游背压通过 `poll_ready()` 显式反馈

### 18.5 验证输出

```
✓ 事件源       : 5 registered (kwin, hyprland, gnome, atspi, input)
✓ 归一化       : 12 类原始事件 → 18 类 DesktopEvent（见 18.1 枚举）
✓ 背压         : capacity 1024, 0 overflow (load test 1000 events/s)
```

---

## 19. 错误处理与降级链

### 19.1 FallbackChain

```rust
// core/src/fallback.rs

pub trait FallbackStep<T>: Send + Sync {
    fn name(&self) -> &'static str;
    fn run(&self) -> Pin<Box<dyn Future<Output = Result<T>> + Send + '_>>;
}

pub struct FallbackChain<T> {
    steps: Vec<Box<dyn FallbackStep<T>>>,
}

impl<T> FallbackChain<T> {
    pub fn new() -> Self { ... }

    /// `AsyncFn` 在当前稳定版尚不能作为 trait object 存储，
    /// 等价的 boxed future trait 表达如下——对外签名语义不变（设计 §19.1 允许）。
    pub fn step<F, Fut>(mut self, name: &'static str, f: F) -> Self
    where
        F: Fn() -> Fut + Send + Sync + 'static,
        Fut: Future<Output = Result<T>> + Send + 'static,
    { ... }

    /// 按顺序执行步骤，全部失败返回最后错误
    pub async fn execute(&self) -> Result<T> {
        let mut last_err = None;
        for step in &self.steps {
            match step.run().await {
                Ok(r) => { tracing::debug!("step '{}' ok", step.name()); return Ok(r); }
                Err(e) => { tracing::warn!("step '{}' failed: {}", step.name(), e); last_err = Some(e); }
            }
        }
        Err(last_err.unwrap_or_else(|| AgentShellError::BackendUnavailable("empty chain".into())))
    }
}
```

### 19.2 典型降级链

```
聚焦窗口:
  1. backend.focus_window (DE 原生)       → 语义级，最可靠
  2. a11y 窗口节点 activate()              → AT-SPI 语义激活
  3. input.click(标题栏中心坐标)            → 坐标降级（需截图确认）

输入文本:
  1. a11y EditableText.setText             → 语义级，应用感知
  2. input.type_text (libei)               → 键盘事件
  3. input.type_text (ydotool)             → 内核注入

截图:
  1. portal ScreenCast (PipeWire 流)       → 跨 DE 标准
  2. DE 原生截图 (KWin D-Bus)              → 无权限弹窗
  3. portal Screenshot                     → 有权限弹窗
  4. xwd/import (X11)                      → 仅 X11
```

### 19.3 超时与重试

```
- 单命令默认超时：5s
- 等待类命令默认超时：15s（可配置）
- D-Bus 调用超时：3s
- 重试：幂等操作（focus/move）最多 2 次指数退避；非幂等（click/type）不重试
- 事件驱动等待优先于轮询等待
```

### 19.4 超时与重试默认值

| 操作 | 默认超时 | 重试次数 | 重试间隔(退避) |
|------|---------|---------|--------------|
| D-Bus 调用（窗口查询） | 5s | 2 | 500ms ×2 |
| KWin Scripting run_script | 5s | 1 | 1000ms |
| hyprctl socket 请求 | 2s | 2 | 200ms ×2 |
| portal ScreenCast 会话 | 10s | 1 | 2000ms |
| 输入注入 (fake_input/XTest) | 1s | 3 | 100ms ×2 |
| 截图 (portal→X11) | 8s | 2 | 1500ms |

### 19.5 验证输出

```
✓ 降级链       : list_windows: 协议 → Scripting → AT-SPI (>0 可用)
✓ 超时         : 0/100 ops timed out (p95=42ms)
✓ 重试         : 2/100 ops recovered after retry
```

---

## 20. 构建与部署

### 20.1 Cargo workspace

```toml
[workspace]
members = [
    "components/input",
    "components/displayserver/wayland",
    "components/displayserver/x11",
    "components/compositor/wayland-core",
    "components/compositor/wlr-wayland",
    "components/compositor/kwin",
    "components/compositor/sway",
    "components/compositor/hyprland",
    "components/compositor/mutter",
    "components/compositor/x11",
    "components/systemd",
    "components/logind",
    "components/notification",
    "components/appearance",
    "components/launcher",
    "components/clipboard",
    "components/network",
    "components/a11y",
    "router",
    "backends/kde",
    "backends/dde",
    "backends/gnome",
    "backends/hyprland",
    "backends/sway",
    "backends/generic",
    "backends/tty",
    "shell",
    "cli",
    "rpc",
    "daemon",
    "rootd",
    "modules/capture",
    "mcp",
    "event",
]

> `core` / `components/audio` / `components/power` 仅在 `[workspace.dependencies]`
> 以 path 出现，**不是** workspace members。

[workspace.dependencies]
zbus = "5"
tokio = { version = "1", features = ["full"] }
serde = { version = "1", features = ["derive"] }
serde_json = "1"
rmcp = "0.1"              # MCP runtime
tracing = "0.1"
pipewire = "0.8"
atspi = "0.27"

[profile.release]
lto = true
strip = true
```

### 20.2 打包

```
- Debian: dpkg-deb（二进制 + systemd user service + udev rules for ydotool）
- Arch:   PKGBUILD
- Nix:    flake.nix
- Flatpak: 沙箱内运行，依赖 portal 接口
```

### 20.3 权限模型

| 资源 | 授权方式 |
|------|---------|
| D-Bus (org.kde.KWin / org.gnome.Shell) | session bus 默认可调用 |
| portal ScreenCast / RemoteDesktop / Screenshot | portal 弹窗用户确认（可持久化 token） |
| /dev/uinput (ydotool) | udev 规则 `uinput` 组 |
| AT-SPI | 需 a11y-bus 可用（KDE 默认开启；GNOME 需手动开启） |

### 20.4 测试策略

```
单元测试:     core types 序列化回环 / router matcher / event 过滤归一化
集成测试:     MockCompositor 实现 CompositorComponent，验证 router 逻辑
真实环境:     DE matrix CI（GNOME/KDE/Hyprland headless）冒烟 list/focus/move/close
端到端:       zenity --info → wait_for_window → focus → screenshot → 校验图片非空
```

---

### 20.5 Cargo workspace 完整成员

```
members = [
    # 显示服务器协议层（不实现 CompositorComponent）
    "components/displayserver/wayland", "components/displayserver/x11",
    # 合成器组件
    "components/compositor/wayland-core", "components/compositor/wlr-wayland",
    "components/compositor/kwin", "components/compositor/sway",
    "components/compositor/hyprland", "components/compositor/mutter",
    "components/compositor/x11",
    # 系统与公共组件
    "components/systemd", "components/logind", "components/network",
    "components/a11y", "components/input", "components/notification",
    "components/appearance", "components/launcher", "components/clipboard",
    # 语义路由 + 事件流 + 截图
    "router", "event", "modules/capture",
    # 后端装配器
    "backends/kde", "backends/dde", "backends/gnome", "backends/hyprland",
    "backends/sway", "backends/generic", "backends/tty",
    # Agent 接口层 + 守护
    "shell", "cli", "rpc", "mcp", "daemon", "rootd",
]
```

`core` / `components/audio` / `components/power` 不在 members——仅
`[workspace.dependencies]` path 依赖，由引用方 crate 消费。

每个组件 crate 可独立编译测试：`cargo test -p agent-shell-compositor-kwin`（组件级）→ `cargo test --workspace`（集成）。

### 20.6 打包依赖清单

| 运行时依赖 | 用途 | 必需？ |
|-----------|------|:------:|
| zbus | D-Bus 通信 | ✅ |
| x11rb | X11 协议 | Wayland-only 可省 |
| wayland-client | Wayland 协议 | X11-only 可省 |
| pipewire (lib) | ScreenCast | ✅ |
| libei (lib) | 输入注入 | 降级可用 ydotoool 替代 |
| atspi (lib) | 无障碍 | 降级可用 AT-SPI D-Bus 直连 |

系统依赖（非 Rust crate）：`xdg-desktop-portal`（必）、`pipewire`（必）、`systemd`（启用 systemd 组件时）。

## 附录 A：模块依赖图

```
cli ──┐
mcp ──┤
rpc ──┼──► router ──► core (types/backend/fallback)
shell─┘         │
                ├──► components/compositor/{kwin,mutter,hyprland,sway,x11,
                │     wayland-core,wlr-wayland} + backends/*
                ├──► components/displayserver/{wayland,x11}
                ├──► components/input/
                ├──► modules/capture/
                └──► components/a11y/
```

> DDE 的 Treeland 私有协议客户端在 `backends/dde/src/treeland.rs`（非
> `components/compositor/treeland/`）；截图在 `modules/capture/`（非
> `components/capture/`）。

## 附录 B：核心设计决策

1. **Rust + tokio + zbus**：内存安全、异步、D-Bus 生态成熟（atspi/pipewire 均有 crate）
2. **组件体系是唯一契约**：DesktopComponent trait 定义公共接口，每 DE 一个 backend 装配清单，可独立编译测试；dde 复用 KWin 合成器组件
3. **语义优先于坐标**：AT-SPI + DE 原生接口构成语义层，模拟输入/截图仅作降级
4. **事件驱动优先于轮询**：KWin 信号 / Hyprland socket2 / AT-SPI 事件统一进 EventHub
5. **降级链显式化**：每操作声明可回退路径，doctor 可诊断每级可用性
6. **KWin Scripting 桥接是核心创新点**：callDBus 回传解决 KWin 脚本无返回值问题

## 附录 C：开放问题

1. GNOME 新版禁用 `Shell.Eval` 的版本边界（需实测各 major 版本行为）
2. DDE 与 upstream KWin 的 scripting API 差异清单（deepin-kwin 是否有自定义扩展）
3. libei Rust 绑定：手动 FFI vs 官方 libei-sys（当前生态成熟度）
4. 多显示器 + 分数缩放下逻辑/物理坐标换算的验证矩阵
5. Flatpak 沙箱下的 session bus 访问限制实测
6. `org.freedesktop.systemd1.Manager` 的 StartUnit mode 语义（replace/fail/isolate）需区分使用场景——agent 通常用 "replace"
7. journalctl JSON 输出解析需处理多行消息和二进制字段（_MESSAGE 可能跨多行）
8. D-Bus Introspect 返回的 XML 描述解析——需考虑接口继承、注释、空节点等边界
9. `org.freedesktop.UDisks2` 接口复杂（对象树深），仅基础磁盘信息可走 `df` CLI 简化
10. 进程管理涉及权限（`ps` 需要能读取 /proc），非 root 可能受限；`kill` 仅能操作自己/同用户进程
11. IBus `ProcessKeyEvent` 的 state 参数（修饰键位掩码）映射——xkbcommon 键码与 IBus 的 keyval 转换
12. Fcitx5 与 IBus 检测优先级，及两种方案并存时的冲突处理
13. XDG Activation Token 在 `gio launch` 中的正确传参方式（GLib 版本差异）
14. Daemon 模式下 portal 会话与 CLI 客户端之间的对象生命周期——CLI 结束后 session 是否保留
15. Secret Service 的默认集合锁定交互流程（GNOME Keyring/KDE Wallet 差异）
16. Brightness 的 Wayland `wlr-gamma-control`（writable）协议支持度——许多 compositor 未实现
17. FileChooser portal 在非交互式场景（无人值守）的行为——需要 `--non-interactive` 模式或直接文件系统访问

---

## 21. 系统服务组件实现

### 21.1 概述

前文聚焦于**合成器组件**（窗口管理、输入、截图、无障碍）。但一个完整的桌面 Agent Shell 还需操作**系统服务组件**：显示布局、音频、网络、应用启动、通知、电源、剪贴板、外观。

按照第 3-4 章的组件架构，每个系统服务是一个独立 `DesktopComponent` 实现，在 `AgentShell::detect_and_assemble()` 中按运行时探测装配：

| 组件 | 章节 | 实现 |
|------|------|------|
| AppLauncher | §21.6 | KdeAppLauncher / GnomeAppLauncher / DdeAppLauncher / PortalAppLauncher |
| AudioService | §21.7 | PipeWireComponent / PulseAudioComponent |
| NotificationService | §21.9 | KdeNotification / GnomeNotification / DdeNotification / PortalNotification |
| PowerManager | §21.11 | UPowerComponent / KdePowerDevil / GnomePower |
| AppearanceService | §21.8 | KdeAppearance / GnomeAppearance / DdeAppearance |
| ClipboardManager | §21.12 | WlClipboardComponent / XClipboardComponent |
| SessionManager | §21.11 | LogindComponent |
| InitSystem | §21.13 | SystemdComponent / OpenRcComponent |
| NetworkManager | §21.17 | NetworkManagerComponent / SystemdNetworkdComponent |

这些能力在每 DE 上通过不同的 D-Bus 接口暴露，但有跨 DE 的 portal 或 freedesktop 标准接口可做保底。所有组件实现**不定义第二套 trait**——统一使用第 3 章的 `DesktopComponent`，共用 `core::types` 和 `core::error`。

### 21.2 架构图

```
┌──────────────────────────────────────────────────────────────┐
│                    Agent Shell Core                          │
│  ┌──────────────────────────────────────────────────────┐   │
│  │                DesktopComponent                 │   │
│  │  trait 统一接口                                          │   │
│  └──┬───┬───┬───┬───┬───┬───┬───┬───┬───┬───┬───┬───┘   │
│     │   │   │   │   │   │   │   │   │   │   │   │       │
│   disp audio net apps notify power clipboard appear settings│
└─────┼───┼───┼───┼───┼───┼───┼───┼───┼───┼───┼───┼───────┘
      │   │   │   │   │   │   │   │   │   │   │   │
  ┌───┴───┴───┴───┴───┴───┴───┴───┴───┴───┴───┴───┴───┐
  │              DE 专有实现 + Portal 实现                 │
  │  ┌────────┐ ┌──────────┐ ┌──────────────────────┐  │
  │  │ KDE    │ │ GNOME    │ │ DDE (deepin-kwin)    │  │
  │  │ 适配器  │ │ 适配器   │ │ 适配器               │  │
  │  └────────┘ └──────────┘ └──────────────────────┘  │
  │  ┌────────┐ ┌──────────┐ ┌──────────────────────┐  │
  │  │Hyprland│ │ X11      │ │ Portal 保底          │  │
  │  │ 适配器  │ │ 适配器   │ │(跨 DE 标准化)        │  │
  │  └────────┘ └──────────┘ └──────────────────────┘  │
  └─────────────────────────────────────────────────────┘
```

### 21.3 跨 DE 接口映射总表

#### 屏幕布局 (Display)

| DE | 接口 | 路径 | 说明 |
|----|------|------|------|
| **KDE** | `org.kde.KScreen` | `/` | `GetConfig()` → KScreenConfig; `SetConfig(config)` → 应用 |
| **GNOME** | `org.gnome.Mutter.DisplayConfig` | `/org/gnome/Mutter/DisplayConfig` | `GetCurrentState()` → (serial, monitors, logicals, props); `ApplyMonitorsConfig(serial, method, logicals, props)` |
| **Hyprland** | `hyprctl output` + `hyprctl keyword monitor` | N/A (socket) | `hyprctl output <name> dpms on/off`; `hyprctl keyword monitor <desc>,preferred,auto,1` |
| **DDE** | `org.deepin.dde.Display1` | `/org/deepin/dde/Display1` | `GetConfig()` → JSON; `ApplyChanges(config_changes)` |
| **X11** | xrandr CLI | N/A | `xrandr --output <name> --mode <res> --right-of <other>` |
| **Portal** | -- | -- | 无 Display 标准 portal（需 DE 专有） |

#### 音频 (Audio)

| DE | 接口 | 说明 |
|----|------|------|
| **跨 DE** | `org.pulseaudio.Server` (session bus) | `GetSinkInfoByName`, `SetSinkVolumeIndex(sink_idx, volume_cvolume)`, `SetSinkMuteIndex` |
| **跨 DE** | `pactl` / `wpctl` CLI | `pactl set-sink-volume @DEFAULT_SINK@ 50%`; `wpctl set-volume @DEFAULT_AUDIO_SINK@ 0.5` |
| **GNOME** | `org.gnome.SettingsDaemon.Audio` | `SetVolume(uint mode, double volume)`; 属性 `ActiveOutput` |
| **DDE** | `org.deepin.dde.Audio1` | `SetVolume(double v, bool isPlay)`; `SetMute(bool mute)`; 属性 `SinkInputs`, `DefaultSink`, `Volume` |
| **Portal** | 尚在讨论 (xdg-desktop-portal #1142)，未稳定 | 音频 portal 未合入，暂不依赖 |

#### 网络 (Network)

| DE | 接口 | 说明 |
|------|------|------|
| **跨 DE** | `org.freedesktop.NetworkManager` (system bus) | 标准网络管理: `GetDeviceByIpIface`, `Devices`, `ActiveConnections`; 设备 `Disconnect()`, `State`, `IpInterface` |
| **跨 DE** | `org.freedesktop.portal.NetworkMonitor` | 仅读取网络状态: 属性 `Available`, `Metered`, `Connectivity` |
| **DDE** | `org.deepin.dde.Network1` | `EnableDevice`, `DisconnectDevice`, `GetConfig`, `SetWirelessEnabled`, `RequestWirelessScan`; 属性 `Devices`, `ActiveConnections`, `WiredConnections`, `WirelessConnections` |

#### 应用启动 (Apps)

| DE | 接口 | 说明 |
|------|------|------|
| **跨 DE** | `gio launch` / `gapplication` | `gio launch <desktop-file>`; `gapplication action <app-id> <action>` |
| **跨 DE** | 解析 `.desktop` 文件 | 扫描 `/usr/share/applications/` + `~/.local/share/applications/` → 获取应用列表 |
| **跨 DE** | `org.freedesktop.portal.OpenURI` | `OpenURI(parent_window, uri, options)` → 打开 URI/URL |
| **跨 DE** | `org.freedesktop.portal.DynamicLauncher` | 动态桌面入口; `PrepareInstall(uri, options)` → 安装应用入口 |
| **KDE** | `org.kde.klauncher` | `exec_blind(desktop, args, ...)` → 启动应用 |
| **GNOME** | `org.gnome.Shell.Eval` | 通过 Shell 内建 `AppSystem` 启动（但推荐 gio 路径） |
| **Hyprland** | `hyprctl dispatch exec` | `hyprctl dispatch exec <command>` — compositor 原生启动 |
| **DDE** | `org.deepin.dde.Application1` | `LaunchApp(app_id, timestamp, scaling)` — 启动桌面应用 |

#### 通知 (Notification)

| DE | 接口 | 说明 |
|------|------|------|
| **跨 DE** | `org.freedesktop.Notifications` (session bus) | 标准通知规范: `Notify(app_name, replaces_id, app_icon, summary, body, actions, hints, expire_timeout)` → `uint id` |
| **跨 DE** | `org.freedesktop.portal.Notification` | portal 通知: `AddNotification(id, notification)` / `RemoveNotification(id)` |
| **DDE** | `org.deepin.dde.Notification1` | `Notify(app_name, replaces_id, app_icon, summary, body, actions, hints, expire_timeout)` |
| **GNOME** | `org.gnome.Shell.Notifications` | 额外通知管理能力（已弃用，走 freedesktop） |

#### 电源与会话 (Power / Lock)

| DE | 接口 | 说明 |
|------|------|------|
| **跨 DE** | `org.freedesktop.login1` (system bus) | 会话管理: `Suspend(bool interactive)`, `Reboot(bool)`, `PowerOff(bool)`, `CanSuspend()`, `CanReboot()`; `Inhibit(what, who, why, mode)` → fd |
| **跨 DE** | `org.freedesktop.UPower` (system bus) | 电池信息: 属性 `Percentage`, `State` (charging/discharging/full), `TimeToEmpty`, `IsPresent` |
| **跨 DE** | `org.freedesktop.portal.PowerProfileMonitor` | 属性 `PowerProfile` ("performance"/"balanced"/"power-saver") |
| **KDE** | `org.kde.Solid.PowerManagement` | 属性 `BatteryChargePercent`, `AcAdapter`; 方法 `Suspend`, `Hibernate`, `ScreenLock` |
| **KDE** | `org.kde.kscreenlocker` | `lock()` → 锁屏; `isLocked` 属性 |
| **GNOME** | `org.gnome.ScreenSaver` | `Lock()` → 锁屏; `GetActive()` → 是否已锁 |
| **GNOME** | `org.gnome.SettingsDaemon.Power` | `SetPowerSaveMode(bool)` |
| **DDE** | `org.deepin.dde.Power1` | 属性 `BatteryPercentage`, `BatteryIsPresent`, `OnBattery`; 方法 `SetScreenBlackLock`, `SetSuspend` |
| **DDE** | `org.deepin.dde.LockService1` | `LockNow()` → 立即锁屏 |
| **Hyprland** | 无专用 D-Bus，依赖 hyprlock/swaylock | `hyprlock` (systemd user service) 或 `swaylock` |

#### 剪贴板 (Clipboard)

| DE | 接口 | 说明 |
|------|------|------|
| **跨 DE** | `org.freedesktop.portal.Clipboard` | `RequestClipboard(session_handle)` → 获取 clipboard fd; 会话绑定 |
| **Wayland** | `wl-copy` / `wl-paste` | `wl-copy <text>`; `wl-paste` |
| **X11** | `xclip` / `xsel` | `xclip -i -selection clipboard`; `xsel -b` |
| **DDE** | `org.deepin.dde.Clipboard1` | 剪贴板管理器 D-Bus（非标准剪切板内容，历史记录） |

#### 外观与壁纸 (Appearance)

| DE | 接口 | 说明 |
|------|------|------|
| **跨 DE** | `org.freedesktop.portal.Wallpaper` | `SetWallpaperURI(parent_window, uri, options)` — 跨 DE 设壁纸（GNOME 实现） |
| **跨 DE** | `org.freedesktop.portal.Settings` | `ReadAll(namespaces)` → `{namespace: {key: value}}`; 标准化 keys: `org.freedesktop.appearance` → `color-scheme`, `accent-color` |
| **KDE** | `plasma-apply-wallpaperimage` | CLI: `plasma-apply-wallpaperimage <path>` |
| **KDE** | `plasma-apply-colorscheme` | CLI: `plasma-apply-colorscheme <name>` |
| **GNOME** | `gsettings` | `gsettings set org.gnome.desktop.background picture-uri <uri>`; `gsettings set org.gnome.desktop.interface color-scheme prefer-dark` |
| **DDE** | `org.deepin.dde.Appearance1` | `SetMonitorBackground(monitor, image_path)`; `SetWallpaper(image_path)`; `SetTheme(type, name)`; 属性 `Themes`, `FontSize` |

### 21.4 组件接口（所有系统服务组件实现）

```rust
// components/各模块（见 §21.1 表）

/// 公共组件接口：屏幕布局（components/compositor/x11 + DE 封装优先）
#[async_trait]
pub trait DisplayLayoutComponent: DesktopComponent {
    async fn get_monitor_layout(&self) -> Result<MonitorLayout, AgentShellError>;
    async fn apply_monitor_layout(&self, layout: &MonitorLayout) -> Result<(), AgentShellError>;
    async fn set_dpms(&self, monitor_id: &MonitorId, on: bool) -> Result<(), AgentShellError>;
}

/// 公共组件接口：音频（components/audio）
#[async_trait]
pub trait AudioServerComponent: DesktopComponent {
    async fn get_volume(&self) -> Result<AudioState, AgentShellError>;
    async fn set_volume(&self, volume: f64) -> Result<(), AgentShellError>;
    async fn set_mute(&self, muted: bool) -> Result<(), AgentShellError>;
    async fn list_audio_devices(&self) -> Result<Vec<AudioDevice>, AgentShellError>;
    async fn set_default_sink(&self, sink_name: &str) -> Result<(), AgentShellError>;
}

/// 公共组件接口：网络（components/network）
#[async_trait]
pub trait NetworkComponent: DesktopComponent {
    async fn get_network_state(&self) -> Result<NetworkState, AgentShellError>;
    async fn list_wifi_networks(&self) -> Result<Vec<WifiNetwork>, AgentShellError>;
    async fn connect_wifi(&self, ssid: &str, password: Option<&str>) -> Result<(), AgentShellError>;
    async fn disconnect_wifi(&self) -> Result<(), AgentShellError>;
}

/// 公共组件接口：应用启动（components/launcher）
#[async_trait]
pub trait LauncherComponent: DesktopComponent {
    async fn list_installed_apps(&self) -> Result<Vec<AppInfo>, AgentShellError>;
    async fn launch_app(&self, app: &AppTarget) -> Result<(), AgentShellError>;
    async fn launch_uri(&self, uri: &str) -> Result<(), AgentShellError>;
}

/// 公共组件接口：通知（components/notification）
#[async_trait]
pub trait NotificationComponent: DesktopComponent {
    async fn send_notification(&self, notif: &NotificationSpec) -> Result<u32, AgentShellError>;
    async fn close_notification(&self, id: u32) -> Result<(), AgentShellError>;
}

/// 公共组件接口：电源与会话（components/power）
#[async_trait]
pub trait PowerComponent: DesktopComponent {
    async fn lock_screen(&self) -> Result<(), AgentShellError>;
    async fn logout(&self) -> Result<(), AgentShellError>;
    async fn suspend(&self) -> Result<(), AgentShellError>;
    async fn hibernate(&self) -> Result<(), AgentShellError>;
    async fn power_off(&self) -> Result<(), AgentShellError>;
    async fn get_battery_status(&self) -> Result<BatteryState, AgentShellError>;
}

/// 公共组件接口：剪贴板（components/clipboard）
#[async_trait]
pub trait ClipboardComponent: DesktopComponent {
    async fn clipboard_read(&self) -> Result<String, AgentShellError>;
    async fn clipboard_write(&self, text: &str) -> Result<(), AgentShellError>;
}

/// 公共组件接口：外观（components/appearance）
#[async_trait]
pub trait AppearanceComponent: DesktopComponent {
    async fn set_wallpaper(&self, path: &str) -> Result<(), AgentShellError>;
    async fn get_color_scheme(&self) -> Result<ColorScheme, AgentShellError>;
    async fn set_color_scheme(&self, scheme: ColorScheme) -> Result<(), AgentShellError>;
}

// ────────────────────────────────────────────────────────────
// DE 封装优先路由（backend 统一规则）
// 每个组件实例 = DE 封装服务优先，无则回退公共/标准接口
// ────────────────────────────────────────────────────────────

/// backend 提供 DE 封装服务路由：
/// 1. 探测 DE 专有 D-Bus 服务（org.kde.* / org.deepin.dde.* / org.gnome.*）
/// 2. 命中 → 用 DE 封装实现
/// 3. 未命中 → 回退公共组件实例（PulseAudio/NetworkManager/portal 等）
pub struct DePriorityRouter {
    de: BackendKind,
    dbus: DBusManager,
}

impl DePriorityRouter {
    /// 音量查询示例：DDE 优先 org.deepin.dde.Audio1 → 回退 PulseAudio/PipeWire
    pub async fn volume(&self) -> Result<AudioState, AgentShellError> {
        match self.de {
            BackendKind::Dde => {
                if self.dbus.service_exists("org.deepin.dde.Audio1").await? {
                    // DE 封装优先
                    DdeAudio::query_volume(&self.dbus).await
                } else {
                    // 回退公共组件
                    PulseAudioAudioServer::new()?.get_volume().await
                }
            }
            BackendKind::Kde => {
                if self.dbus.service_exists("org.kde.kmix").await? {
                    KmixAudio::query_volume(&self.dbus).await
                } else {
                    PulseAudioAudioServer::new()?.get_volume().await
                }
            }
            _ => PulseAudioAudioServer::new()?.get_volume().await,
        }
    }
}

#[derive(Clone, Debug)]
pub struct ComponentCapabilities {
    pub display_layout: bool,
    pub audio_control: bool,
    pub network_management: bool,
    pub network_monitor: bool,
    pub app_launch: bool,
    pub app_list: bool,
    pub notification: bool,
    pub power_and_battery: bool,
    pub lock_screen: bool,
    pub clipboard: bool,
    pub wallpaper: bool,
    pub color_scheme: bool,
}
rust
// components/各模块（见 §21.1 表） (新增类型)

/// 监视器配置
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct MonitorConfig {
    pub id: MonitorId,
    pub enabled: bool,
    pub resolution: (u32, u32),
    pub position: (i32, i32),
    pub scale: f64,
    pub refresh_rate: f64,
    pub primary: bool,
    pub transform: MonitorTransform,
    pub dpi: u32,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub enum MonitorTransform { Normal, Rot90, Rot180, Rot270, FlipX, FlipY }

/// 音频状态
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct AudioState {
    pub volume: f64,          // 0.0 - 1.0
    pub muted: bool,
    pub default_sink: String,
}

/// 音频设备
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct AudioDevice {
    pub name: String,
    pub description: String,
    pub volume: f64,
    pub muted: bool,
    pub is_default: bool,
    pub device_type: AudioDeviceType,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub enum AudioDeviceType { Sink, Source }

/// 网络状态
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct NetworkState {
    pub connectivity: Connectivity,
    pub wifi_enabled: bool,
    pub active_ssid: Option<String>,
    pub ip_address: Option<String>,
    pub metered: bool,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub enum Connectivity { Full, Limited, Local, None }

/// WiFi 网络
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct WifiNetwork {
    pub ssid: String,
    pub strength: u8,          // 0-100
    pub secured: bool,
    pub frequency: u32,        // MHz
    pub known: bool,
}

/// 应用信息
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct AppInfo {
    pub app_id: String,
    pub name: String,
    pub icon: Option<String>,
    pub categories: Vec<String>,
    pub desktop_file: String,
    pub exec: String,
    pub is_gui: bool,
}

/// 应用启动目标
#[derive(Clone, Debug, Serialize, Deserialize)]
pub enum AppTarget {
    ByDesktopFile(String),     // 路径或 ID
    ByAppId(String),           // 通过 app_id 查找
    ByCommand(String),         // 直接执行命令
    OpenUri(String),           // 用默认应用打开 URI
}

/// 通知规格
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct NotificationSpec {
    pub summary: String,
    pub body: Option<String>,
    pub urgency: NotificationUrgency,
    pub icon: Option<String>,
    pub timeout_ms: Option<i32>,
    pub actions: Vec<(String, String)>,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub enum NotificationUrgency { Low, Normal, Critical }

/// 电池状态
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct BatteryState {
    pub percentage: f64,
    pub charging: bool,
    pub time_to_empty: Option<u32>,
    pub time_to_full: Option<u32>,
    pub is_present: bool,
}

/// 配色方案
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub enum ColorScheme { NoPreference, Dark, Light }
```

### 21.6 每 DE 装配策略（DE 封装优先 → 公共组件降级）

> 以下接口名及 D-Bus 归属基于 2026-08 联网调研（上游源码 + D-Bus 服务定义），详见 §3.5。标识「❓需真机确认」的项为待核实项。

```
DE 检测 → 选定 backend → 按装配清单初始化公共组件实例
                          DE 专有实例（backends/）→ 公共降级（components/）

├── KDE backend（backends/kde/）
│   ├── 合成器:     KWinCompositor（components/compositor/kwin/）
│   ├── 音频:       PipeWireAudioServer（components/audio/）
│   ├── 网络:       NetworkManagerComponent（components/network/）
│   ├── 电源:       KdePowerDevil（org.kde.Solid.PowerManagement，含 PowerProfile + SuspendSession + BrightnessControl）→ UPower
│   ├── 锁屏:       org.kde.screensaver（kscreenlocker，接口 org.kde.screensaver，信号 AboutToLock）
│   ├── 通知:       KdeNotification（org.freedesktop.Notifications KDE 后端）→ PortalNotification
│   ├── 外观:       KdeAppearance（plasma-apply-wallpaperimage CLI + portal）→ PortalAppearance
│   ├── 启动器:     KdeLauncher（org.kde.klauncher）→ PortalLauncher
│   ├── 输入/截图/无障碍/剪贴板: 公共探测链（components/{input,a11y,clipboard}/ + modules/capture/）
│   └── 内部路由:   services.rs 探测 org.kde.Solid.PowerManagement → 命中用 KDE 封装，否则回退 UPower/portal

├── DDE backend（backends/dde/）
│   ├── 合成器:     KWinCompositor(deepin-kwin)（components/compositor/kwin/）/ Treeland（backends/dde/src/treeland.rs）
│   ├── 音频:       PipeWireAudioServer（components/audio/）；DDE 封装：org.deepin.dde.Audio1（session bus）
│   ├── 网络:       DDE 25: dde-network-core + NetworkManager ❓需真机确认；DDE 20: com.deepin.daemon.Network
│   ├── 电源:       DdePower（org.deepin.dde.Power1，system bus）→ UPower
│   ├── 锁屏:       org.deepin.dde.LockService1（system bus）
│   ├── 通知:       DdeNotification ❓需真机确认（dde-daemon 无 org.deepin.dde.Notification1，可能在 dde-shell 中）
│   ├── 外观:       DdeAppearance ❓需真机确认（org.deepin.dde.Appearance1，服务文件不在 dde-daemon）
│   ├── 启动器:     DdeLauncher ❓需真机确认（org.deepin.dde.Application1 或 dde-am，服务文件不在 dde-daemon）
│   ├── 输入/截图/无障碍/剪贴板: 公共探测链（components/{input,a11y,clipboard}/ + modules/capture/）
│   └── 内部路由:   dde_api.rs 探测 org.deepin.dde.Audio1（session bus）/ Power1（system bus）→ 命中用 DE 封装，否则回退公共组件

├── GNOME backend（backends/gnome/）
│   ├── 合成器:     MutterCompositor（components/compositor/mutter/）
│   ├── 音频:       PipeWireAudioServer（components/audio/）——GNOME 无 DE 层音量 D-Bus 接口，直接公共组件
│   ├── 网络:       NetworkManagerComponent（components/network/）
│   ├── 电源:       GnomePower（org.gnome.SettingsDaemon.Power）→ UPower
│   ├── 锁屏:       org.gnome.ScreenSaver
│   ├── 通知:       GnomeNotification（org.freedesktop.Notifications GNOME 后端）→ PortalNotification
│   ├── 外观:       GnomeAppearance（gsettings org.gnome.desktop.background + portal）→ PortalAppearance
│   ├── 启动器:     GnomeLauncher（gio launch + portal）→ PortalLauncher
│   ├── 输入/截图/无障碍/剪贴板: 公共探测链（components/{input,a11y,clipboard}/ + modules/capture/）
│   └── 内部路由:   services.rs 探测 org.gnome.SettingsDaemon.Power → 命中用 GNOME 封装，否则回退 UPower/portal

├── Hyprland backend（backends/hyprland/）
│   ├── 合成器:     HyprlandCompositor（components/compositor/hyprland/）
│   ├── Portal:     xdg-desktop-portal-hyprland 仅支持 GlobalShortcuts + ScreenCast + Screenshot
│   └── 其余:       公共组件探测链（通知/剪贴板/外观等需 portal-gtk 或公共组件兜底）

├── Sway backend（backends/sway/）
│   ├── 合成器:     SwayCompositor（components/compositor/sway/）
│   └── 其余:       公共组件探测链 + xdg-desktop-portal-wlr（ScreenCast/Screenshot 仅）+ portal-gtk 兜底

└── Generic backend（backends/generic/）
    ├── 合成器:     X11Compositor / WlrWaylandCompositor（components/compositor/x11/ + components/compositor/wlr-wayland/）
    └── 其余:       公共组件探测链 + portal-gtk 兜底
```
### 21.7 实现示例：DDE 音频 + DePriorityRouter

#### DDE 音频两级调用（`backends/dde/src/dde_audio.rs`）

```rust
// 服务名按版本探测：DDE25 主名 org.deepin.dde.Audio1，
// 失败回退 DDE20 com.deepin.daemon.Audio（DDE25 上为别名）。
pub const DDE25_AUDIO: &str = "org.deepin.dde.Audio1";
pub const DDE20_AUDIO: &str = "com.deepin.daemon.Audio";

// 两版根对象路径约定：/{base}/Audio1 或 /{base}/Audio。
const ROOT_PATH_CANDIDATES: [&str; 3] = [
    "/org/deepin/dde/Audio1",
    "/com/deepin/daemon/Audio",
    "/org/deepin/daemon/Audio",
];

pub struct DdeAudio {
    conn: zbus::Connection,
    variant: AudioServiceVariant,
    root_path: OwnedObjectPath,
}

impl DdeAudio {
    /// 探测：服务名在位 + DefaultSink 属性可读才视为命中。
    pub async fn with_connection(conn: &zbus::Connection) -> Result<Self> { ... }

    /// 根对象只暴露属性与 Sink 子对象枚举，不直接暴露音量方法——
    /// 控制必须落到 DefaultSink 指向的 Sink 子对象。
    async fn default_sink_path(&self) -> Result<OwnedObjectPath> {
        let p = self.root_proxy().await?;
        let v: OwnedValue = p.get_property("DefaultSink").await?;
        OwnedObjectPath::try_from(v).map_err(|e| dbus_err("sink path decode", e))
    }
}

impl AudioServerComponent for DdeAudio {
    async fn get_volume(&self) -> Result<AudioState> {
        let path = self.default_sink_path().await?;
        let p = self.sink_proxy(&path).await?;
        let volume: f64 = p.get_property("Volume").await?;   // Sink 子对象
        let muted: bool = p.get_property("Mute").await?;      // Sink 子对象
        Ok(AudioState { volume: volume.clamp(0.0, 1.0), muted,
                        default_sink: Self::sink_name(&path) })
    }

    async fn set_volume(&self, volume: f64) -> Result<()> {
        let path = self.default_sink_path().await?;
        let p = self.sink_proxy(&path).await?;
        // SetVolume(double)——Sink 子对象方法，非根对象；值 clamp 到 0.0-1.0。
        let _: () = p.call("SetVolume", &(volume.clamp(0.0, 1.0),)).await?;
        Ok(())
    }

    async fn set_mute(&self, muted: bool) -> Result<()> {
        let path = self.default_sink_path().await?;
        let p = self.sink_proxy(&path).await?;
        let _: () = p.call("SetMute", &(muted,)).await?;      // Sink 子对象
        Ok(())
    }

    async fn set_default_sink(&self, sink_name: &str) -> Result<()> {
        // Sinks 列表按子对象短名匹配后，根对象 SetDefaultSink(o)；未匹配报结构化错误。
        let target = self.sink_paths().await?
            .iter().find(|p| Self::sink_name(p) == sink_name)
            .ok_or_else(|| AgentShellError::DBus(format!("unknown sink {sink_name:?}")))?;
        let p = self.root_proxy().await?;
        let _: () = p.call("SetDefaultSink", &(target,)).await?;
        Ok(())
    }
}
```

#### DePriorityRouter：trait 注入解耦（`components/audio/src/router.rs`）

```rust
pub enum AudioChannel {
    DeWrapper(&'static str), PipeWireCli, PulseAudioCli,
}

/// DE 专有音频封装回调接口——由各 backend 的 DdeAudio 实现；
/// router 只依赖此 trait，不反向依赖具体 backend crate。
pub trait DeAudioWrapper: Send + Sync {
    async fn service_exists(&self) -> bool;          // DefaultSink 可读为判据
    fn service_name(&self) -> &'static str;          // 实际命中服务名
    fn inner(&self) -> &dyn AudioServerComponent;    // 委托底层实现
}

pub struct DePriorityRouter {
    de: BackendKind,
    fallback: Box<dyn AudioServerComponent>,
    de_wrapper: Option<Box<dyn DeAudioWrapper>>,
    last_channel: std::sync::atomic::AtomicU8,
}

impl DePriorityRouter {
    pub fn new(de: BackendKind,
               fallback: Box<dyn AudioServerComponent>,
               fallback_channel: AudioChannel,
               de_wrapper: Option<Box<dyn DeAudioWrapper>>) -> Self { ... }

    /// 路由决策核心：DE 封装服务在位 → 委托 DE 实现；
    /// 否则 → 公共降级实例（wpctl/pactl）。
    async fn resolve(&self) -> (&dyn AudioServerComponent, bool) {
        if let Some(w) = &self.de_wrapper {
            if w.service_exists().await { return (w.inner(), true); }
        }
        (self.fallback.as_ref(), false)
    }
}

// DePriorityRouter 自身实现 AudioServerComponent，
// 每个方法 resolve().await 后委托给返回的实现。
```

DDE 装配（`backends/dde/src/assemble.rs`）：先试 `DdeAudio`（服务在位 + 属性可读），
失败回退 `agent_shell_audio::router::assemble_audio_router(BackendKind::Dde)`。

### 21.8 实现示例：GNOME 屏幕布局

```rust
// backends/gnome/src/services.rs（屏幕布局）

pub struct GnomeDisplay {
    config: MutterDisplayConfigProxy,  // org.gnome.Mutter.DisplayConfig
}

impl GnomeDisplay {
    /// 获取当前显示状态
    pub async fn get_layout(&self) -> Result<MonitorLayout> {
        // GetCurrentState() 返回 (serial, monitors, logical_monitors, properties)
        let (serial, monitors, logicals, _props) = self.config.get_current_state().await?;
        // monitors: [(connector, vendor, product, serial_str, geometry, modes, ...)]
        // logicals: [(x, y, scale, transform, primary, monitors_refs, ...)]
        Self::parse_state(serial, monitors, logicals)
    }

    /// 应用显示布局
    pub async fn apply_layout(&self, layout: &MonitorLayout) -> Result<()> {
        // 先获取当前 serial
        let (serial, _monitors, _logicals, _props) = self.config.get_current_state().await?;
        // 构造 logical_monitors: [(x, y, scale, transform, primary, [monitor_spec])]
        let logicals = self.build_logicals(layout);
        // ApplyMonitorsConfig(serial, method, logicals, properties)
        self.config.apply_monitors_config(serial, 0u32, &logicals, &[]).await?;
        Ok(())
    }
}
```

### 21.9 实现示例：KDE 屏幕布局

```rust
// backends/kde/src/services.rs

pub struct KdeDisplay { kscreen: KScreenProxy }  // org.kde.KScreen

impl KdeDisplay {
    pub async fn get_layout(&self) -> Result<MonitorLayout> {
        let config = self.kscreen.get_config().await?;
        Self::parse_kscreen_config(config)
    }
    pub async fn apply_layout(&self, layout: &MonitorLayout) -> Result<()> {
        let kscreen_config = self.build_kscreen_config(layout);
        self.kscreen.set_config(kscreen_config).await?;
        Ok(())
    }
}
```

### 21.10 CLI 扩展

```bash
# 屏幕布局
agent-shell display list
agent-shell display apply --monitor HDMI-1 --mode 1920x1080 --right-of eDP-1
agent-shell display dpms HDMI-1 on

# 音频
agent-shell audio volume                # 当前音量
agent-shell audio volume 50             # 设 50%
agent-shell audio mute                  # 切换静音
agent-shell audio devices               # 列出设备
agent-shell audio default-sink "alsa_output.pci-0000_00_1f.3.analog-stereo"

# 网络
agent-shell network status
agent-shell network wifi list
agent-shell network wifi connect "MyWiFi" [--password "xxx"]
agent-shell network wifi disconnect

# 应用
agent-shell apps list [--filter terminal]
agent-shell apps launch firefox
agent-shell apps launch-uri https://example.com

# 通知
agent-shell notify send "备份完成" "文件已同步到 NAS" --urgency low
agent-shell notify close 42

# 电源
agent-shell power battery
agent-shell power lock
agent-shell power suspend

# 剪贴板
agent-shell clipboard get
agent-shell clipboard set "hello world"

# 外观
agent-shell wallpaper set ~/Pictures/desktop.png
agent-shell wallpaper get
agent-shell theme list
agent-shell theme set dark
```

### 21.11 MCP 工具扩展

```rust
// 新增工具
Tool::new("apply_display_layout").description("设置屏幕布局: 分辨率、位置、缩放、主屏").input_schema(...);
Tool::new("set_volume").description("设置系统音量 0-100").input_schema(...);
Tool::new("wifi_scan_connect").description("扫描并连接 WiFi").input_schema(...);
Tool::new("launch_app").description("启动应用 (桌面文件/命令/URI)").input_schema(...);
Tool::new("send_notification").description("发送桌面通知").input_schema(...);
Tool::new("get_battery").description("获取电池状态").input_schema(...);
Tool::new("lock_screen").description("锁屏").input_schema(...);
Tool::new("clipboard_read").description("读取剪贴板").input_schema(...);
Tool::new("clipboard_write").description("写入剪贴板").input_schema(...);
Tool::new("set_wallpaper").description("设置壁纸").input_schema(...);
Tool::new("set_color_scheme").description("切换深色/浅色模式").input_schema(...);
```

### 21.12 项目结构扩展

> 本设计早期草案曾规划 `services/`（系统服务能力层）与 `components/portal/`
> （跨 DE portal 公共降级）两个 crate。实现落地后两者均未建立——系统服务能力
> 由 `components/{audio,network,power,clipboard,…}` 与 `core/src/services.rs`
> 承担，portal 降级落在各 component 内部或 backend 装配层。下为实际结构：

```
agent-shell/
├── core/                         # 类型系统 + 组件 trait + 错误（非 member，path 依赖）
│   └── src/ { component.rs, registry.rs, services.rs, error.rs, … }
├── components/
│   ├── audio/                    # { lib.rs, router.rs, pipewire.rs, pulseaudio.rs }
│   ├── network/                  # { lib.rs, networkmanager.rs, networkd.rs }
│   ├── power/                    # { lib.rs, router.rs }
│   ├── input/ a11y/ clipboard/ notification/ appearance/ launcher/
│   ├── systemd/ logind/          # systemd / logind 能力
│   ├── displayserver/ {wayland, x11}/
│   └── compositor/ {wayland-core, wlr-wayland, kwin, sway, hyprland, mutter, x11}/
├── backends/                     # DE 专有装配（每个 DE 一个 crate）
│   ├── dde/                      # { lib.rs, assemble.rs, dde_api.rs, dde_audio.rs,
│   │                             #   dde_compositor.rs, compositor.rs, treeland.rs,
│   │                             #   protocol_gen.rs, version.rs }
│   ├── kde/                      # { lib.rs, assemble.rs, services.rs }
│   ├── gnome/                    # { lib.rs, assemble.rs, services.rs }
│   ├── hyprland/ sway/ generic/ tty/
├── router/                       # 语义路由
├── event/                        # 事件流 + 归一化
├── modules/capture/              # 截图
├── shell/                        # 聚合 shell（原设计 SDK 并入）
├── rpc/ cli/ mcp/ daemon/ rootd/ # Agent 接口层 + 守护
└── packaging/                    # polkit policy + 打包
```

portal 公共降级（Clipboard/Notification/OpenURI/…）没有独立 crate：公共组件内部
优先 portal 标准接口、失败回退 CLI/协议，具体见 §21.6 装配矩阵与各组件章节。

### 21.13 systemd 服务管理

这些能力通过 `org.freedesktop.systemd1.Manager` (system bus) 提供，**不依赖 DE**，是 systemd Linux 标配。

| 接口 | 路径 | 说明 |
|------|------|------|
| `org.freedesktop.systemd1.Manager` | `/org/freedesktop/systemd1` | 全面管理 systemd 单元 |

**关键方法：**

| 方法 | 签名 | 说明 |
|------|------|------|
| `ListUnits` | `→ a(ssssssouso)` | 列出所有已加载的 unit（名称、描述、加载状态、激活状态、子状态、对象路径等） |
| `ListUnitFiles` | `→ a(ss)` | 列出所有 unit 文件及 enablement 状态 |
| `GetUnit` | `(s) → o` | 根据 unit 名称获取对象路径 |
| `GetUnitByPID` | `(u) → o` | 根据 PID 获取所属 unit |
| `StartUnit` | `(ss) → o` | 启动 unit，mode: replace/fail/isolate/ignore-dependencies |
| `StopUnit` | `(ss) → o` | 停止 unit |
| `RestartUnit` | `(ss) → o` | 重启 unit |
| `ReloadUnit` | `(ss) → o` | 重新加载 unit |
| `KillUnit` | `(ssi) → ()` | 向 unit 进程发信号 (who: main/control/all, signal: 信号编号) |
| `EnableUnitFiles` | `(asbb) → (ba(sss))` | 启用 unit 文件（开机自启） |
| `DisableUnitFiles` | `(asb) → a(sss)` | 禁用 unit 文件 |
| `MaskUnitFiles` | `(asbb) → a(sss)` | 屏蔽 unit（禁止启动） |
| `UnmaskUnitFiles` | `(asb) → a(sss)` | 取消屏蔽 |
| `GetUnitFileState` | `(s) → s` | 查询 unit 文件状态 (enabled/disabled/static/masked/...) |
| `ResetFailedUnit` | `(s) → ()` | 重置 unit 的 failed 状态 |
| `Reload` | `() → ()` | 重新加载所有 unit 文件 |
| `Subscribe` / `Unsubscribe` | `() → ()` | 订阅 signal（UnitNew/UnitRemoved/JobNew/JobRemoved/UnitFilesChanged） |

**实现要点：**

```rust
// components/systemd/src/lib.rs (SystemdComponent)

pub struct SystemdManager {
    proxy: Systemd1ManagerProxy,  // org.freedesktop.systemd1.Manager
}

impl SystemdManager {
    /// 列出所有加载的 unit
    pub async fn list_units(&self) -> Result<Vec<SystemdUnit>> {
        let units = self.proxy.list_units().await?;
        // units: [(name, desc, load_state, active_state, sub_state, ...
        Ok(units.into_iter().map(Self::parse_unit).collect())
    }

    /// 启动服务 (返回 job 对象路径)
    pub async fn start_unit(&self, name: &str, mode: &str) -> Result<String> {
        let job = self.proxy.start_unit(name, mode).await?;
        Ok(job.as_str().to_string())
    }

    /// 停止服务
    pub async fn stop_unit(&self, name: &str) -> Result<()> {
        self.proxy.stop_unit(name, "replace").await?;
        Ok(())
    }

    /// 启用服务（开机自启）
    pub async fn enable_unit(&self, name: &str) -> Result<()> {
        self.proxy.enable_unit_files(&[name], false, false).await?;
        Ok(())
    }

    /// 查询 unit 状态
    pub async fn get_unit_state(&self, name: &str) -> Result<String> {
        self.proxy.get_unit_file_state(name).await.map_err(Into::into)
    }
}
```

**新增类型：**

```rust
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct SystemdUnit {
    pub name: String,
    pub description: String,
    pub load_state: String,    // "loaded" / "not-found" / "error"
    pub active_state: String,  // "active" / "inactive" / "activating" / "deactivating" / "failed"
    pub sub_state: String,     // "running" / "exited" / "dead" / "waiting"
    pub object_path: String,
    pub pid: Option<u32>,
    pub enabled_state: String, // "enabled" / "disabled" / "static" / "masked" / "indirect"
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub enum UnitAction { Start, Stop, Restart, Reload, Enable, Disable, Mask, Unmask }
```

---

### 21.14 日志管理 (Journal)

日志通过 `journalctl` 命令行实现，配合 `org.freedesktop.LogControl1` 控制日志级别。

| 能力 | 实现方式 | 说明 |
|------|---------|------|
| 查询日志 | `journalctl` CLI | 按 unit/时间/优先级/关键词过滤 |
| 实时跟踪 | `journalctl -f` 或 `--follow` | 流式日志输出 |
| 日志级别 | `org.freedesktop.LogControl1` | 查询/设置日志级别 |
| 日志大小 | `journalctl --disk-usage` | 磁盘占用 |

**实现要点：**

```rust
// 未实现（部分）：查询落地 rootd/src/lib.rs 的 JournalQuery（daemon 经 system.log.view 调用）；
// --follow / --disk-usage / LogControl1 级别控制未实现

pub struct JournalReader {
    log_control: LogControl1Proxy,  // org.freedesktop.LogControl1 (可选)
}

impl JournalReader {
    /// 查询日志 (按 unit 和时间范围)
    pub async fn query(&self, filter: &JournalFilter) -> Result<Vec<JournalEntry>> {
        let mut cmd = vec!["journalctl", "--no-pager", "-o", "json"];
        if let Some(unit) = &filter.unit {
            cmd.extend(["-u", unit]);
        }
        if let Some(prio) = filter.priority {
            cmd.extend(["-p", &prio.to_string()]);
        }
        if let Some(since) = &filter.since {
            cmd.extend(["--since", since]);
        }
        if let Some(until) = &filter.until {
            cmd.extend(["--until", until]);
        }
        if let Some(n) = filter.lines {
            cmd.extend(["-n", &n.to_string()]);
        }
        // 执行 journalctl 命令，解析 JSON 行
        let output = run_cmd(&cmd).await?;
        Self::parse_journal_json(&output)
    }

    /// 获取日志磁盘用量
    pub async fn disk_usage(&self) -> Result<u64> {
        let output = run_cmd(&["journalctl", "--disk-usage"]).await?;
        // 解析 "Archived and active journals take up 128.0M in the filesystem."
        Self::parse_usage(&output)
    }

    /// 设置日志级别（systemd journald 级别）
    pub async fn set_log_level(&self, level: &str) -> Result<()> {
        // 通过 LogControl1 设置
        self.log_control.set_log_level(level).await?;
        Ok(())
    }
}
```

**新增类型：**

```rust
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct JournalEntry {
    pub timestamp: String,
    pub message: String,
    pub priority: u8,         // 0=emerg, 1=alert, 2=crit, 3=err, 4=warning, 5=notice, 6=info, 7=debug
    pub unit: Option<String>,
    pub pid: Option<u32>,
    pub comm: Option<String>,  // 进程名
    pub boot_id: Option<String>,
    pub cursor: Option<String>,
}

#[derive(Clone, Debug, Default)]
pub struct JournalFilter {
    pub unit: Option<String>,
    pub priority: Option<u8>,
    pub since: Option<String>,  // "2024-01-01" 或 "-1h" 等
    pub until: Option<String>,
    pub lines: Option<u32>,
    pub follow: bool,
}
```

---

### 21.15 D-Bus 服务管理

通过 `org.freedesktop.DBus` 标准接口（session bus + system bus）进行 D-Bus 服务本身的查询和调用。

| 接口 | 总线 | 说明 |
|------|------|------|
| `org.freedesktop.DBus` | session + system | 核心 D-Bus 守护进程接口 |
| `org.freedesktop.DBus.Introspectable` | session + system | 对象内省 |

**关键方法：**

| 方法 | 签名 | 说明 |
|------|------|------|
| `ListNames` | `→ as` | 列出所有已注册总线名称 |
| `ListActivatableNames` | `→ as` | 列出所有可激活的服务名称 |
| `NameHasOwner` | `(s) → b` | 检查名称是否已被占用 |
| `GetNameOwner` | `(s) → s` | 获取名称持有者的唯一名称 |
| `GetConnectionUnixProcessID` | `(s) → u` | 获取连接进程的 PID |
| `GetConnectionUnixUser` | `(s) → u` | 获取连接进程的 UID |
| `Introspect` | `→ s` | 内省对象，返回 XML 描述 |
| `StartServiceByName` | `(su) → u` | 启动服务（激活） |

**实现要点：**

```rust
// 未实现：core 无 dbus.rs（D-Bus 服务管理能力未落地）

pub struct DBusInspector {
    session_dbus: DBusProxy,
    system_dbus: DBusProxy,
}

impl DBusInspector {
    /// 列出所有总线上注册的服务名称
    pub async fn list_services(&self, bus: BusType) -> Result<Vec<DBusService>> {
        let proxy = match bus {
            BusType::Session => &self.session_dbus,
            BusType::System => &self.system_dbus,
        };
        let names = proxy.list_names().await?;
        let mut services = Vec::new();
        for name in names {
            let pid = proxy.get_connection_unix_process_id(&name).await.ok();
            let uid = proxy.get_connection_unix_user(&name).await.ok();
            services.push(DBusService { name, pid, uid, bus });
        }
        services.sort_by(|a, b| a.name.cmp(&b.name));
        Ok(services)
    }

    /// 内省对象路径，返回 XML 描述
    pub async fn introspect(&self, bus: BusType, service: &str, path: &str) -> Result<String> {
        let conn = match bus {
            BusType::Session => zbus::Connection::session().await?,
            BusType::System => zbus::Connection::system().await?,
        };
        let proxy = IntrospectableProxy::builder(&conn)
            .destination(service)?.path(path)?.build().await?;
        proxy.introspect().await.map_err(Into::into)
    }
}
```

**新增类型：**

```rust
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct DBusService {
    pub name: String,        // 如 "org.kde.KWin", "org.freedesktop.systemd1"
    pub pid: Option<u32>,
    pub uid: Option<u32>,
    pub bus: BusType,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub enum BusType { Session, System }

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct DBusIntrospectNode {
    pub name: String,
    pub interfaces: Vec<String>,  // 接口名列表
    pub children: Vec<DBusIntrospectNode>,
}
```

---

### 21.16 系统信息 (Hostname / Time / Disk)

| 能力 | D-Bus 接口 | 方法/属性 |
|------|-----------|----------|
| **主机名** | `org.freedesktop.hostname1` | 属性 `Hostname`, `StaticHostname`, `PrettyHostname`, `KernelName`, `KernelRelease`, `OperatingSystemPrettyName` |
| **时间日期** | `org.freedesktop.timedate1` | 属性 `TimeUSec`, `RTCTimeUSec`, `Timezone`, `LocalRTC`, `NTP`, `NTPSynchronized`; 方法 `SetTimezone`, `SetNTP`, `SetTime` |
| **区域设置** | `org.freedesktop.locale1` | 属性 `Locale`, `X11Layout`, `X11Model`; 方法 `SetLocale` |
| **磁盘信息** | `org.freedesktop.UDisks2` | 对象 `/org/freedesktop/UDisks2/Manager` 驱动管理; `org.freedesktop.UDisks2.Block` 块设备; `org.freedesktop.UDisks2.Filesystem` 文件系统 |
| **磁盘用量** | `df` CLI | `df -h` |

**实现要点：**

```rust
// 未实现：无 components/misc（系统信息读取未落地；仅 rootd/src/lib.rs 承载 HostnameSet 设置特权路径）

pub struct SystemInfo {
    hostnamed: Hostname1Proxy,    // org.freedesktop.hostname1
    timedated: Timedate1Proxy,    // org.freedesktop.timedate1
    localed: Locale1Proxy,        // org.freedesktop.locale1
}

impl SystemInfo {
    pub async fn get_machine_info(&self) -> Result<MachineInfo> {
        Ok(MachineInfo {
            hostname: self.hostnamed.hostname().await?,
            static_hostname: self.hostnamed.static_hostname().await?,
            pretty_hostname: self.hostnamed.pretty_hostname().await?,
            kernel_name: self.hostnamed.kernel_name().await?,
            kernel_release: self.hostnamed.kernel_release().await?,
            os_pretty_name: self.hostnamed.operating_system_pretty_name().await?,
            os_cpe_name: self.hostnamed.operating_system_cpe_name().await.ok(),
        })
    }

    pub async fn get_datetime_info(&self) -> Result<DateTimeInfo> {
        Ok(DateTimeInfo {
            time_usec: self.timedated.time_usec().await?,
            timezone: self.timedated.timezone().await?,
            ntp_enabled: self.timedated.ntp().await?,
            ntp_synchronized: self.timedated.ntp_synchronized().await?,
            local_rtc: self.timedated.local_rtc().await?,
        })
    }
}
```

**新增类型：**

```rust
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct MachineInfo {
    pub hostname: String,
    pub static_hostname: String,
    pub pretty_hostname: String,
    pub kernel_name: String,        // "Linux"
    pub kernel_release: String,     // "6.7.1-zen1"
    pub os_pretty_name: String,     // "Arch Linux"
    pub os_cpe_name: Option<String>,
    pub architecture: Option<String>,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct DateTimeInfo {
    pub time_usec: u64,
    pub timezone: String,
    pub ntp_enabled: bool,
    pub ntp_synchronized: bool,
    pub local_rtc: bool,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct DiskInfo {
    pub filesystem: String,
    pub mount_point: String,
    pub total_bytes: u64,
    pub used_bytes: u64,
    pub available_bytes: u64,
    pub usage_percent: f64,
}
```

---

### 21.17 进程管理

基础进程管理，通过 CLI 实现，不依赖 DE。

| 能力 | 实现 | 说明 |
|------|------|------|
| 列出进程 | `ps aux --sort=-%mem` | 按条件过滤 |
| 查找进程 | `pgrep` / `pidof` | 按名称查找 |
| 发送信号 | `kill` | kill / kill -9 / kill -15 |
| 进程树 | `pstree` | 层级树 |
| 资源占用 | `top` / `htop` 单次快照 | CPU/内存 |

**实现要点：**

```rust
// 未实现：无 components/misc；kill 特权路径为 rootd/src/lib.rs 的 ProcessKill（仅发送信号）

pub struct ProcessManager;

impl ProcessManager {
    /// 列出进程（按条件过滤）
    pub async fn list(&self, filter: &ProcessFilter) -> Result<Vec<ProcessInfo>> {
        let mut cmd = vec!["ps", "--no-headers", "-o", "pid,comm,args,%cpu,%mem,rss,user,stat"];
        if let Some(name) = &filter.name {
            // 可以用 pgrep 找 PID 后过滤
            cmd.extend(["-C", name]);
        }
        let output = run_cmd(&cmd).await?;
        Self::parse_ps_output(&output)
    }

    /// 发送信号
    pub async fn kill(&self, pid: u32, signal: i32) -> Result<()> {
        run_cmd(&["kill", &format!("-{}", signal), &pid.to_string()]).await?;
        Ok(())
    }
}
```

**新增类型：**

```rust
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct ProcessInfo {
    pub pid: u32,
    pub name: String,
    pub command: String,
    pub cpu_percent: f64,
    pub mem_percent: f64,
    pub rss_kb: u64,
    pub user: String,
    pub state: String,
}

#[derive(Clone, Debug, Default)]
pub struct ProcessFilter {
    pub name: Option<String>,
    pub pid: Option<u32>,
    pub user: Option<String>,
    pub sort_by: Option<String>,  // "cpu", "mem", "pid"
    pub limit: Option<u32>,
}
```

---

### 21.18 CLI 扩展（基础设施）

```bash
# systemd 服务管理
agent-shell service list [--type service] [--state running]
agent-shell service status nginx.service
agent-shell service start nginx.service
agent-shell service stop nginx.service
agent-shell service restart nginx.service
agent-shell service enable nginx.service
agent-shell service disable nginx.service
agent-shell service mask nginx.service
agent-shell service unmask nginx.service

# 日志
agent-shell journal query -u nginx.service --since "-1h" --priority err
agent-shell journal follow -u nginx.service
agent-shell journal disk-usage

# D-Bus 服务
agent-shell dbus services [--bus system]
agent-shell dbus introspect org.freedesktop.systemd1 /org/freedesktop/systemd1
agent-shell dbus call org.freedesktop.systemd1 /org/freedesktop/systemd1 \
  org.freedesktop.systemd1.Manager ListUnits

# 系统信息
agent-shell sysinfo hostname
agent-shell sysinfo datetime
agent-shell sysinfo disk
agent-shell sysinfo all

# 进程管理
agent-shell ps [--name firefox] [--sort mem]
agent-shell kill 1234 [--signal 9]
```

### 21.19 MCP 工具扩展

```rust
// 新增工具
Tool::new("systemd_list_units").description("列出 systemd 单元").input_schema(...);
Tool::new("systemd_start_unit").description("启动 systemd 服务").input_schema(...);
Tool::new("systemd_stop_unit").description("停止 systemd 服务").input_schema(...);
Tool::new("systemd_enable_unit").description("启用 systemd 服务（开机自启）").input_schema(...);
Tool::new("journal_query").description("查询系统日志").input_schema(...);
Tool::new("journal_follow").description("实时跟踪日志").input_schema(/* streaming SSE */);
Tool::new("dbus_list_services").description("列出 D-Bus 服务").input_schema(...);
Tool::new("dbus_introspect").description("内省 D-Bus 对象").input_schema(...);
Tool::new("system_info").description("获取系统信息（主机名/内核/OS）").input_schema(...);
Tool::new("disk_usage").description("磁盘用量").input_schema(...);
Tool::new("list_processes").description("列出进程").input_schema(...);
Tool::new("kill_process").description("发送信号给进程").input_schema(...);
```

### 21.20 项目结构扩展

> 本节规划的基础设施模块未按此树落地：无 `services/` crate、无
> `components/init`、无 `components/misc`。systemd 能力在
> `components/systemd/src/lib.rs`，进程/日志/挂载等特权操作在
> `rootd/src/lib.rs`，systemd 相关数据类型在 `core/src/services.rs`。
> 实际目录树见 §21.12。

### 21.21 安全与授权模型

Agent 拥有桌面完全控制权是高危配置，必须定义权限边界。

> **威胁模型（审查项 F2）**：daemon 的 stdin/控制 socket 归属用户会话
> （同 UID）——任何能写它的调用方本就是用户自身可信进程；`caller_id`
> 由同 UID 编排进程经 `AGENT_SHELL_AGENT_ID` env 注入、可被同 UID 进程
> 伪造。因此 `security.grant` / `security.revoke` 按 `caller_id` 门禁
> 不构成额外权限边界。实现上以 `"*"` 为管理面边界：仅本地调用方
> （未注入 agent id 的 CLI/MCP 子进程）可授权/撤销；具名 agent 不可
> 自行授予白名单，必须经 `"*"`（本地用户/编排层）完成。

#### 21.21.1 权限分级

| 级别 | 定义 | 示例 | 操作方式 |
|------|------|------|---------|
| **L0 只读** | 不修改系统状态 | 窗口列表、系统信息、日志查询 | 自动执行 |
| **L1 低风险** | 可逆、影响小 | 聚焦窗口、打字、切换工作区、卷音量 | 自动执行 |
| **L2 中风险** | 可逆但影响面大 | 关闭窗口、安装应用、连接 WiFi、改壁纸、截图（内容敏感，非只读） | 自动执行 + 审计日志 |
| **L3 高风险** | 不可逆或影响系统 | `systemctl stop/disable`、`kill`、删文件、RCU 操作 | 默认确认，可配置白名单 |

#### 21.21.2 配置模型

```toml
# ~/.config/agent-shell/config.toml

[security]
# 默认确认级别：L3/L4 操作需确认
default_confirm_level = "L3"
# 审计日志
audit_log = true
audit_log_path = "~/.local/state/agent-shell/audit.log"

[permissions.allow]
# 白名单 agent（按 app_id 或 executable）
"*" = ["L0", "L1", "L2"]
"my-trusted-agent" = ["L0", "L1", "L2", "L3"]

[permissions.deny]
# 黑名单操作
"systemctl*" = false
"rm *" = false

[operations.confirm]
# 覆盖 default_confirm_level，按操作名
"poweroff" = "always"
"reboot" = "always"
"systemctl.stop.*" = "always"
"systemd.disable.*" = "always"
```

#### 21.21.3 实现

```rust
// core/src/security.rs

pub struct SecurityManager {
    config: AgentShellConfig,
    audit: AuditLogger,
}

impl SecurityManager {
    /// 检查操作是否被允许
    pub fn check_permission(&self, agent_id: &str, op: &Operation) -> Result<PermissionDecision> {
        // 1. 查黑名单 → Deny
        if self.is_denied(agent_id, op) {
            return Ok(PermissionDecision::Deny(op.to_string()));
        }
        // 2. 查操作确认覆盖 → Confirm
        if let Some(mode) = self.confirm_mode(op) {
            return Ok(PermissionDecision::Confirm(mode));
        }
        // 3. 查 agent 白名单级别
        let agent_level = self.agent_level(agent_id);
        if op.level() > agent_level {
            return Ok(PermissionDecision::Confirm(op.to_string()));
        }
        // 4. 默认允许
        Ok(PermissionDecision::Allow)
    }

    /// 记录审计日志
    pub fn audit(&self, agent_id: &str, op: &Operation, result: bool) {
        self.audit.log(AgentAction {
            timestamp: now(),
            agent_id, op: op.name().to_string(),
            args: op.args().clone(),
            result,
        });
    }
}

#[derive(Debug)]
pub enum PermissionDecision {
    Allow,
    Confirm(ConfirmMode),
    Deny(String),
}

#[derive(PartialEq)]
enum ConfirmMode { Always, Once, Timeout(Duration) }
```

#### 21.21.4 Portal 权限持久化

```
portal 弹窗（ScreenCast/RemoteDesktop/Screenshot）：
  L1: 每次操作都询问（不持久化）—— agent 高频截图会烦
  L2: 持久化 token（用户首次同意后记住）—— 需要 portal 的 persist_mode + restore_token
  L3: 弹窗 + 限时（token 24h 过期）

xdg-desktop-portal 支持：
  persist_mode: 1 = dont_persist, 2 = persist_only_while_system_run, 3 = persist_until_revoked
  restore_token: 保存/恢复会话 token
```

---

### 21.22 Portal SessionManager

ScreenCast / RemoteDesktop / Clipboard / InputCapture 等 portal 需要长会话，必须统一管理。

```rust
// daemon/src/portal_sessions.rs (PortalSessionManager；token 持久化按 kind 存取)

pub struct PortalSessionManager {
    sessions: HashMap<PortalSessionId, PortalSession>,
    registry: zbus::Connection,   // session bus
}

#[derive(Clone, Debug, PartialEq, Eq, Hash)]
pub struct PortalSessionId(pub String);

#[derive(Clone, Debug)]
pub struct PortalSession {
    pub id: PortalSessionId,
    pub kind: PortalKind,             // ScreenCast / RemoteDesktop / Clipboard / ...
    pub handle: zbus::ObjectPath,
    pub created_at: Instant,
    pub persist_token: Option<String>, // 持久化 token
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum PortalKind {
    ScreenCast,
    RemoteDesktop,
    Clipboard,
    InputCapture,
    GlobalShortcuts,
    Inhibit,
}

impl PortalSessionManager {
    pub async fn create_session(&mut self, kind: PortalKind, options: SessionOptions) -> Result<PortalSession> {
        // 1. 检查是否已有相同 kind 的会话（复用优先）
        if let Some(existing) = self.sessions.values().find(|s| s.kind == kind && s.is_valid()) {
            return Ok(existing.clone());
        }
        // 2. 通过对应 portal 创建
        let portal = match kind {
            PortalKind::ScreenCast => ScreenCastPortalProxy::new(&self.registry).await?,
            PortalKind::RemoteDesktop => RemoteDesktopPortalProxy::new(&self.registry).await?,
            PortalKind::Clipboard => ClipboardPortalProxy::new(&self.registry).await?,
            _ => return Err(AgentShellError::NotImplemented("portal kind")),
        };
        let session = portal.create_session(options).await?;
        // 3. 注册到 map
        let id = PortalSessionId(format!("{:?}-{}", kind, session.as_str()));
        self.sessions.insert(id.clone(), PortalSession { ... });
        Ok(self.sessions[&id].clone())
    }

    pub async fn restore_session(&mut self, kind: PortalKind, token: &str) -> Result<PortalSession> {
        // 用 restore_token 恢复持久化会话
        let options = SessionOptions { persist_mode: Some(PersistMode::PersistUntilRevoked), restore_token: Some(token.to_string()), ..Default::default() };
        self.create_session(kind, options).await
    }

    pub async fn close_session(&mut self, id: &PortalSessionId) -> Result<()> {
        if let Some(session) = self.sessions.remove(id) {
            match session.kind {
                PortalKind::ScreenCast => ScreenCastPortalProxy::new(&self.registry).await?.close(&session.handle).await?,
                PortalKind::RemoteDesktop => RemoteDesktopPortalProxy::new(&self.registry).await?.close(&session.handle).await?,
                _ => {}
            }
        }
        Ok(())
    }

    /// 保持会话存活的守护任务（定期检查超时/断开）
    pub async fn keep_alive_loop(&mut self) {
        loop {
            tokio::time::sleep(Duration::from_secs(30)).await;
            // 检查会话过期，断开则自动重连（如有 restore_token）
            self.sessions.retain(|_, s| !s.is_expired());
        }
    }
}
```

**关键点**：
- **会话复用**：同一 kind 的 session 可被多次操作复用，避免重复弹窗
- **Daemon 持有**：session 必须由长驻 daemon 持有，CLI 每次调用都重新建会话会反复弹窗
- **断线重连**：compositor 重启 / portal 崩溃后，用 restore_token 自动恢复

---

### 21.23 输入法引擎 (IBus / Fcitx)

非 ASCII 文本输入必须走输入法引擎。中文用户的**关键路径**。

#### 21.23.1 IBus 接口

| 对象 | 接口 | 关键方法/信号 |
|------|------|--------------|
| `org.freedesktop.IBus` | `InputBus` | `CreateInputContext(client, path, locale)` → 创建输入上下文 |
| | | `SetGlobalEngine(engine_name)` → 切换全局引擎（如 "libpinyin"） |
| | | `GetCurrentEngine()` → 当前引擎 |
| `/org/freedesktop/IBus/InputContext_N` | `InputContext` | `ProcessKeyEvent(keyval, keycode, state)` → 按键交给输入法处理 |
| | | `SetFocusLocation(x, y, w, h)` → 通知输入法焦点位置（候选窗口） |
| | | `SetContentType(purpose, hints)` → 密码框等 |
| | | 信号 `CommitText(text)` → 输入法输出的上屏文本 |
| | | 信号 `UpdatePreeditText(text, cursor, visible)` → 预编辑（下划线候选） |

#### 21.23.2 设计：完整输入流程

```
agent 想输入中文 "你好世界" 到文本输入框：

1. Focus: 定位到输入框（AT-SPI 或窗口聚焦）
2. 创建/复用 IBus InputContext
3. 设置焦点位置 (SetFocusLocation — 用窗口+元素几何)
4. 设置内容类型 (SetContentType — 普通文本)
5. 通过输入法处理按键序列（拼音 → 候选字）：
   a. 按键 "ni" → ProcessKeyEvent('n') → ProcessKeyEvent('i')
      → 收到 UpdatePreeditText("ni")
      → 读屏幕/AT-SPI 查看候选词
   b. 选择候选：
      - 用 ProcessKeyEvent(数字键) 或
      - 直接 CommitText("你") 强制上屏（如果候选窗口复杂）
6. 循环直到全部输入完成
```

#### 21.23.3 Fcitx5 fallback

| 接口 | 说明 |
|------|------|
| `org.fcitx.Fcitx.Controller1` | `setCurrentUI`, `toggle`, `activate` |
| `org.fcitx.Fcitx.InputMethod` | `CurrentIM`, `IMList` |
| `org.fcitx.Fcitx.KeyboardLayout` | 键盘布局管理 |

检测：`env IBUS_ADDRESS` / `env XMODIFIERS=@im=fcitx` / `ps aux | grep ibus-daemon`

#### 21.23.4 实现

```rust
// daemon/src/ime_session.rs (ImeSession)

pub struct ImeManager {
    ibus: IBusProxy,          // org.freedesktop.IBus
    context_path: Option<String>,
    commit_tx: mpsc::UnboundedSender<String>,  // CommitText 信号
}

impl ImeManager {
    pub async fn new() -> Result<Self> {
        // 通过 IBUS_ADDRESS 连接
        let address = std::env::var("IBUS_ADDRESS")?;
        let conn = zbus::ConnectionBuilder::address(&address)?.build().await?;
        let ibus = IBusProxy::builder(&conn).destination("org.freedesktop.IBus")?.build().await?;
        Ok(Self { ibus, context_path: None, commit_tx: _ })
    }

    pub async fn set_engine(&self, engine: &str) -> Result<()> {
        self.ibus.set_global_engine(engine).await?;   // engine: "libpinyin" / "xkb:us::eng"
        Ok(())
    }

    pub async fn ensure_context(&mut self) -> Result<&str> {
        if self.context_path.is_none() {
            let path = self.ibus.create_input_context(
                "agent-shell", "/com/agentshell/ime", "zh_CN"
            ).await?;
            self.context_path = Some(path);
        }
        Ok(self.context_path.as_deref().unwrap())
    }

    /// 通过输入法处理单个按键，返回输入法输出的事件
    pub async fn process_key(&mut self, keyval: u32) -> Result<ImeKeyEvent> {
        let path = self.ensure_context().await?.to_string();
        let ctx = InputContextProxy::new(&self.conn, &path).await?;
        let handled = ctx.process_key_event(keyval, 0, 0).await?;
        // handled=true → 输入法消费了此键，等待 CommitText/UpdatePreedit
        // handled=false → 按键未消费，转发到系统
        Ok(ImeKeyEvent { handled, committed: self.consume_commit().await })
    }
}
```

**降级链**：
```
1. IBus D-Bus 语义输入（推荐，精确）
2. Fcitx5 D-Bus
3. 键盘模拟 + 截图像素读候选窗口（复杂，最后手段）
4. 纯 ASCII 绕过（只处理 ascii，非 ascii 返回错误）

注意：很多文本输入其实不需要输入法——纯英文/数字直接 type_text 即可。
仅在检测到 target 需要非 ASCII 或当前引擎非英文时走 IME 路径。
```

---

### 21.24 Daemon 模式架构

Portal 会话、事件流、IME 上下文都需要**长驻进程**。CLI 是瞬态进程，无法持有这些状态。

```
┌─────────────────────────────────────────────────────┐
│                   Agent 客户端                        │
│  (CLI / MCP / SDK)  每次调用 = 一次命令               │
└──────────────┬──────────────────────────────┐       │
               │ JSON-RPC (stdio)              │       │
               │ 或 Unix Socket (AF_UNIX)      │       │
┌──────────────▼──────────────────────────────▼─────┐ │
│              agent-shell daemon (常驻)              │ │
│                                                   │ │
│  ┌─────────────────────────────────────────────┐  │ │
│  │ PortalSessionManager (ScreenCast/Remote...)  │  │ │
│  ├─────────────────────────────────────────────┤  │ │
│  │ EventHub (窗口/工作区/输入事件)                │  │ │
│  ├─────────────────────────────────────────────┤  │ │
│  │ ImeSession (IBus InputContext + 候选跟踪)     │  │ │
│  ├─────────────────────────────────────────────┤  │ │
│  │ KWin Script Bridge (长驻 event_monitor.js)   │  │ │
│  ├─────────────────────────────────────────────┤  │ │
│  │ SecurityManager + AuditLogger                │  │ │
│  └─────────────────────────────────────────────┘  │ │
│                                                   │ │
│  systemd user service:                            │ │
│  agent-shell.service (Restart=on-failure)         │ │
└─────────────────────────────────────────────────────┘
```

**接口协议**：JSON-RPC 2.0 over stdio（与 CLI 相同，只是进程常驻）

```bash
# 客户端调用 daemon
agent-shell window focus --app-id firefox
    → 通过 stdio/JSON-RPC 发给 daemon
    → daemon 执行并返回结果

# daemon 自身
systemctl --user start agent-shell
systemctl --user status agent-shell
```

**状态保持**：
- Portal sessions（避免每次弹窗）
- IME context（避免重新建立会话）
- 事件订阅（daemon 持续推送到订阅者）
- 日志缓冲（内存环形队列，供查询）

**生命周期**：
- D-Bus session activation（首次调用自动启动）
- 空闲超时退出（可配置）
- `agent-shell daemon` 手动前台模式（调试）

---

### 21.25 屏幕亮度

| DE | 接口 | 说明 |
|----|------|------|
| **跨 DE** | `brightnessctl` CLI | 通用: `brightnessctl set 50%`, `brightnessctl info` |
| **GNOME** | `org.gnome.SettingsDaemon.Power` | 属性 `ScreenBrightness`(0-100), `BrightnessSteps`; 方法 `StepUp`, `StepDown` |
| **KDE** | `org.kde.Solid.PowerManagement` | 属性 `Brightness`(0-100), `BrightnessStep` |
| **DDE** | `org.deepin.dde.Display1` | 属性 `Brightness`(map 显示器名→0-100), `MonitorBrightness`; 方法 `SetBrightness(monitor, brightness)` |
| **Wayland** | `wlr-gamma-control` / `wlr-output-management` | 经 wlroots 协议（需 compositor 支持） |

```rust
// 未实现（pending）：daemon 仅 stub_ok 占位（brightness.get/set）

pub struct BrightnessController {
    // 实现根据 DE 选择后端
}

impl BrightnessController {
    pub async fn get(&self) -> Result<Vec<BrightnessState>>;
    pub async fn set(&self, monitor: &str, value: u8) -> Result<()>;   // 0-100
    pub async fn step(&self, delta: i8) -> Result<()>;
}
```

**新增类型：**

```rust
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct BrightnessState {
    pub monitor: String,
    pub brightness: u8,       // 0-100
    pub max_brightness: u8,
    pub adaptive: bool,
}
```

---

### 21.26 文件系统浏览与操作

Agent 常需要浏览文件系统（"打开这个文件"、"找到 Downloads 目录"）。

| 能力 | 接口 | 说明 |
|------|------|------|
| **文件选择** | `org.freedesktop.portal.FileChooser` | `OpenFile(parent, title, options)` → uri; 选项可指定 filters |
| **保存文件** | `org.freedesktop.portal.FileChooser` | `SaveFile(parent, title, options)` → uri |
| **移动到回收站** | `org.freedesktop.portal.Trash` | `Trash(uris)` → 移到回收站（非永久删除） |
| **挂载管理** | `org.freedesktop.UDisks2` | `org.freedesktop.UDisks2.Manager`; `org.freedesktop.UDisks2.Filesystem.Mount/Unmount` |
| **目录浏览** | `org.freedesktop.portal.OpenURI.OpenDirectory` | 在文件管理器中打开目录 |
| **基础文件操作** | 直接文件系统访问 | agent 本身有文件工具，这里是"桌面角度"的封装 |

```rust
// 未实现（pending）：daemon 仅 stub_ok 占位（file.pick/trash/open_directory）

pub struct FilesystemService { /* ... */ }

impl FilesystemService {
    /// 打开文件选择器（portal 弹窗）
    pub async fn pick_file(&self, filters: &[FileFilter]) -> Result<PathBuf> {
        let portal = FileChooserProxy::new(&session_bus).await?;
        let request = portal.open_file("", dict! {
            "filters" => filters_to_variant(filters)
        }).await?;
        let (result, _) = wait_for_response(&request).await?;
        let uris = result["uris"].as_str_vec().unwrap_or_default();
        uris.first().map(parse_uri_to_path)
            .ok_or(AgentShellError::Capture("no file chosen".into()))
    }

    /// 移动多个文件到回收站
    pub async fn trash(&self, paths: &[PathBuf]) -> Result<()> {
        let portal = TrashProxy::new(&session_bus).await?;
        let uris: Vec<String> = paths.iter().map(|p| path_to_file_uri(p)).collect();
        let request = portal.trash(&uris).await?;
        wait_for_response(&request).await?;
        Ok(())
    }
}
```

---

### 21.27 默认应用与 MIME 类型

| 功能 | 接口 | 说明 |
|------|------|------|
| 查询默认应用 | `xdg-mime query default <mime>` | 如 `xdg-mime query default text/html` → "firefox.desktop" |
| 设置默认应用 | `xdg-mime default <app.desktop> <mime>` | 设置默认 |
| 列出 MIME handlers | `org.freedesktop.portal.AppChooser` | `ChooseApplication(uri, options)` → 应用选择器 |
| 文件管理器 | `xdg-settings get default-folder-viewer` | 查询默认文件管理器 |
| 浏览器 | `xdg-settings get default-web-browser` | 查询默认浏览器 |

```rust
// 未实现（pending）：daemon 仅 stub_ok 占位（mime.get/set/default_browser）

pub struct MimeService;

impl MimeService {
    pub async fn get_default_app(&self, mime: &str) -> Result<String> {
        let output = run_cmd(&["xdg-mime", "query", "default", mime]).await?;
        Ok(output.trim().to_string())
    }

    pub async fn set_default_app(&self, mime: &str, desktop_id: &str) -> Result<()> {
        run_cmd(&["xdg-mime", "default", desktop_id, mime]).await?;
        Ok(())
    }

    pub async fn get_default_browser(&self) -> Result<String> {
        let output = run_cmd(&["xdg-settings", "get", "default-web-browser"]).await?;
        Ok(output.trim().to_string())
    }
}
```

---

### 21.28 XDG Activation Token

Wayland 下启动应用后聚焦的**核心机制**。没有 token，启动的应用可能聚焦失败（focus stealing prevention）。

**v1 范围**：只透传进程环境已有的 `XDG_ACTIVATION_TOKEN`（`components/launcher/src/lib.rs` 经 `gio launch --activation-token` 透传，GLib ≥ 2.76）；DE 专有路径（KWin `activate` / GNOME Eval）直接激活窗口，绕过 token。**v1 不主动获取 token**。

**v2 候选**：主动获取 `xdg_activation_v1.get_activation_token`（见 §22.9 D8），属直连协议绑定，待 portal 路径无法满足聚焦需求时引入。

目标形态（v2）：

```
┌──────────┐                     ┌────────────────┐
│  agent    │  xdg_activation.v1 │  Wayland        │
│  (发起方)  │──────────────────►│  compositor     │
│          │  get_activation_token│                │
│          │◄────────────────── │  done(token)    │
│          │                     │                │
│  gio launch --activation-token │                │
│  ────────────────────────────►│  目标应用带着    │
│  (带 token 启动应用)           │  token activate │
└──────────┘                     └────────────────┘
```

**v2 实现草图**（`components/compositor/activation.rs`，非 v1 交付物）：

```rust
// 未实现（pending）：无 xdg_activation_v1 客户端
pub struct ActivationTokenManager {
    wl: WaylandConnection,   // agent-shell 自身的 wayland 客户端连接
}

impl ActivationTokenManager {
    /// 获取激活 token（v2 候选：xdg_activation_v1.get_activation_token）
    pub async fn request_token(&mut self) -> Result<String> {
        let token = self.wl.get_activation_token().await?;
        Ok(token)
    }

    /// 带 token 启动应用
    pub async fn launch_with_activation(&mut self, desktop_file: &str) -> Result<()> {
        let token = self.request_token().await?;
        run_cmd(&format!("XDG_ACTIVATION_TOKEN={} gio launch {}", token, desktop_file)).await?;
        Ok(())
    }
}
```

**v1 已落地**（`components/launcher/src/lib.rs`）：

```rust
// Wayland 下若设置了 XDG_ACTIVATION_TOKEN 则透传给 gio launch（GLib ≥2.76）
let activation_token = std::env::var("XDG_ACTIVATION_TOKEN").ok();
if let Some(token) = &activation_token {
    cmd.arg("launch").arg("--activation-token").arg(token);
} else {
    cmd.arg("launch");
}
```

**推荐实践**：
1. 首选 `XDG_ACTIVATION_TOKEN` 环境变量传给启动命令
2. `gio launch` 原生支持 `--activation-token` 参数（GLib ≥ 2.76）
3. DDE/KDE 通过 KWin scripting 直接 activate 窗口（绕过 token，DE 专有路径）

---

### 21.29 蓝牙管理 (BlueZ)

| 接口 | 路径 | 说明 |
|------|------|------|
| `org.bluez.Adapter1` | `/org/bluez/hci0` | 适配器: `StartDiscovery`, `StopDiscovery`, `RemoveDevice`, `SetDiscoveryFilter`; 属性 `Powered`, `Discovering` |
| `org.bluez.Device1` | `/org/bluez/hci0/dev_XX_XX_XX_XX_XX_XX` | 设备: `Connect`, `Disconnect`, `Pair`, `CancelPairing`; 属性 `Name`, `Address`, `Connected`, `Paired`, `Trusted`, `UUIDs`, `RSSI` |
| `org.bluez.Agent1` | 自定义 | 配对代理: `RequestPinCode`, `RequestConfirmation`, `AuthorizeService` |

```rust
// 未实现（pending）：daemon 仅 stub_ok 占位（bluetooth.*）

pub struct BluetoothManager {
    adapter: BluezAdapter1Proxy,
    agent_registered: bool,
}

impl BluetoothManager {
    pub async fn new() -> Result<Self> {
        let conn = zbus::Connection::system().await?;
        let adapter = BluezAdapter1Proxy::builder(&conn)
            .destination("org.bluez")?.path("/org/bluez/hci0")?.build().await?;
        Ok(Self { adapter, agent_registered: false })
    }

    pub async fn scan(&self, timeout: Duration) -> Result<Vec<BtDevice>> {
        // 注册 Agent1 (NoInputNoOutput 模式) → StartDiscovery → 等待 → StopDiscovery
        self.adapter.start_discovery().await?;
        tokio::time::sleep(timeout).await;
        self.adapter.stop_discovery().await?;
        // 枚举 /org/bluez/hci0/dev_* 设备
        Ok(self.list_devices().await?)
    }

    pub async fn connect(&self, address: &str) -> Result<()> {
        let path = format!("/org/bluez/hci0/dev_{}", address.replace(':', "_"));
        let dev = BluezDevice1Proxy::builder(&self.conn)
            .destination("org.bluez")?.path(&path)?.build().await?;
        dev.connect().await?;
        Ok(())
    }
}
```

**新增类型：**

```rust
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct BtDevice {
    pub address: String,
    pub name: String,
    pub paired: bool,
    pub connected: bool,
    pub trusted: bool,
    pub rssi: Option<i16>,
    pub uuids: Vec<String>,
}
```

---

### 21.30 Flatpak / PackageKit

| 能力 | 接口 | 说明 |
|------|------|------|
| **Flatpak** | `flatpak` CLI (或 `org.freedesktop.Flatpak` D-Bus) | `flatpak list`, `flatpak install`, `flatpak update`, `flatpak uninstall` |
| **PackageKit** | `org.freedesktop.PackageKit` (system bus) | 发行版软件包管理: `GetUpdates`, `GetPackages`, `InstallPackages`, `UpdatePackages`, `RefreshCache`（需 polkit） |
| **发行版 CLI** | `apt` / `dnf` / `pacman` | 直接调用，注意权限 |

```rust
// 未实现（pending）：daemon 仅 stub_ok 占位（flatpak.list/install、software.updates）

pub struct SoftwareManager {
    pkgkit: PackageKitProxy,     // org.freedesktop.PackageKit
}

impl SoftwareManager {
    /// 列出已安装 Flatpak
    pub async fn list_flatpaks(&self) -> Result<Vec<FlatpakApp>> {
        let output = run_cmd(&["flatpak", "list", "--app", "--columns=application,name,origin"]).await?;
        Self::parse_flatpak_list(&output)
    }

    /// 检查系统更新（PackageKit）
    pub async fn check_updates(&self) -> Result<Vec<PackageUpdate>> {
        let request = self.pkgkit.get_updates(0x25, &[]).await?;  // filter: 群组过滤
        // 等待 PackageKit.Transaction Done 信号
        let updates = wait_for_transaction(&request).await?;
        Ok(updates)
    }
}
```

---

### 21.31 触控板 / 键盘布局

| 功能 | DDE | GNOME | KDE |
|------|-----|-------|-----|
| **触控板开关** | `org.deepin.dde.InputDevices1` 属性 `TouchpadEnable` | gsettings `org.gnome.desktop.peripherals.touchpad send-events` | `org.kde.kcm_touchpad` 或直接写 kwin config |
| **自然滚动** | 同上 `TouchpadNaturalScroll` | gsettings `natural-scroll` | `org.kde.kcm_touchpad` property |
| **点击手势** | `TouchpadTapClick` | gsettings `tap-to-click` | 同上 |
| **键盘布局** | `org.deepin.dde.InputDevices1` 方法 `SetLayoutList` | gsettings `org.gnome.desktop.input-sources sources` | `org.freedesktop.locale1` 属性 `X11Layout` |

---

### 21.32 Secret Service 密钥环

| 接口 | 说明 |
|------|------|
| `org.freedesktop.secrets` | Secret Service API（GNOME Keyring / KDE Wallet / KeePassXC 统一实现） |
| `org.freedesktop.Secret.Collection` | 集合: `CreateItem`, `Delete`, 解锁 |
| `org.freedesktop.Secret.Item` | 条目: `GetSecret`, `SetSecret` |
| `org.freedesktop.Secret.Service` | `OpenSession`, `GetDefaultCollection`, `Lock`, `Unlock` |

用途：agent 安全存储 API 密钥、WiFi 密码、数据库凭证。

```rust
// 未实现（pending）：daemon 仅 stub_ok 占位（secret.set/get）

pub struct SecretService {
    service: SecretServiceProxy,   // org.freedesktop.Secret.Service
    session_handle: String,
}

impl SecretService {
    /// 开启会话并获取默认集合
    pub async fn open_session(&mut self) -> Result<String> {
        let (output, _) = self.service.open_session("plain", &Vec::new()).await?;
        self.session_handle = output;
        // 返回默认 collection path
        self.service.get_default_collection().await.map_err(Into::into)
    }

    /// 存储密钥
    pub async fn store(&self, collection: &str, key: &str, secret: &[u8]) -> Result<()> {
        let properties = dict! {
            "org.freedesktop.Secret.Item.Label" => key,
            "org.freedesktop.Secret.Item.Attributes" => dict!{"key" => key},
        };
        self.service.create_item(collection, properties, secret, true).await?;
        Ok(())
    }

    /// 读取密钥
    pub async fn retrieve(&self, collection: &str, key: &str) -> Result<Vec<u8>> {
        // 通过 SearchItems 查找，然后 GetSecret
        unimplemented!("按 attributes 搜索")
    }
}
```

**注意**：GNOME Keyring 默认锁定，需要 `Unlock`（可能弹窗/polkit）。KDE Wallet 同理。此模块应标记为"需要用户交互"的能力。

---

### 21.33 全局快捷键

| 接口 | 说明 |
|------|------|
| `org.freedesktop.portal.GlobalShortcuts` | 标准 portal: `CreateSession` → `BindShortcuts` → 发 `ShortcutsChanged` 信号 |
| `org.kde.kglobalaccel` | KDE 专用: `GetComponent`, `InvokeShortcut` |
| GNOME | 需 Shell Extension 暴露（或 gsettings 自定义快捷键） |

**用途**：agent 注册/触发全局热键（如截图快捷键、通知中心开关）。

---

### 21.34 systemd timer 管理

设计 18.13 只覆盖了 service，补 timer：

| 能力 | 接口 | 说明 |
|------|------|------|
| 列出 timer | `org.freedesktop.systemd1.Manager.ListTimers` | `→ a(ssssssussss)`（id, next_elapse_real, ...） |
| 列出 timer 文件 | `ListTimerFiles` | `→ a(ss)` |
| 创建 timer | 写 unit 文件 + `daemon-reload` + `StartUnit` | 间接 |
| 启用 timer | `EnableUnitFiles` + `StartUnit` | 标准流程 |

```rust
// 未实现（pending）：SystemdComponent 未实现 list_timers；SystemdTimer 类型在 core/src/services.rs
impl SystemdManager {
    /// 列出所有 timer
    pub async fn list_timers(&self) -> Result<Vec<SystemdTimer>> {
        let timers = self.proxy.list_timers().await?;
        Ok(timers.into_iter().map(Self::parse_timer).collect())
    }

    /// 下一个触发时间
    pub async fn next_elapse(&self, timer_name: &str) -> Result<SystemdTimer> {
        let timers = self.list_timers().await?;
        timers.into_iter().find(|t| t.name == timer_name)
            .ok_or_else(|| AgentShellError::WindowNotFound(timer_name))
    }
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct SystemdTimer {
    pub name: String,
    pub next_elapse_real: String,   // ISO 时间
    pub last_trigger_real: String,
    pub unit_path: String,
    pub next_elapse_monotonic: u64,
    pub last_trigger_monotonic: u64,
    pub running: bool,
}
```

---

### 21.35 CLI / MCP 扩展汇总（安全、IME、系统补充）

```bash
# 安全与授权
agent-shell security status                 # 当前权限配置
agent-shell security grant "my-agent" L3    # 授权
agent-shell security revoke "my-agent"      # 撤销
agent-shell security audit                  # 审计日志

# Daemon
agent-shell daemon start                    # 启动守护进程
agent-shell daemon stop
agent-shell daemon status
agent-shell daemon sessions                 # 查看 portal 会话

# 输入法
agent-shell ime engine list
agent-shell ime engine set libpinyin
agent-shell ime engine current
agent-shell ime type "你好世界"              # 通过 IME 输入中文

# 亮度
agent-shell brightness
agent-shell brightness 50

# 文件
agent-shell file pick                        # portal 文件选择器
agent-shell file trash ./tmp.log
agent-shell file open-directory ~/Downloads

# 默认应用
agent-shell mime get text/html
agent-shell mime set text/html firefox.desktop
agent-shell mime default-browser

# 蓝牙
agent-shell bluetooth scan
agent-shell bluetooth connect XX:XX:XX:XX:XX:XX
agent-shell bluetooth disconnect XX:XX:XX:XX:XX:XX
agent-shell bluetooth list

# 软件
agent-shell software flatpak list
agent-shell software flatpak install org.videolan.VLC
agent-shell software updates

# 触控板
agent-shell touchpad status
agent-shell touchpad on/off
agent-shell touchpad natural-scroll on

# 键盘布局
agent-shell kbd layout list
agent-shell kbd layout set us,cn   # 双布局

# 密钥环
agent-shell secret set db-credentials "user:pass"
agent-shell secret get db-credentials

# 全局快捷键
agent-shell shortcut bind "meta+shift+s" screenshot
agent-shell shortcut trigger "meta+shift+s"

# systemd timer
agent-shell timer list
agent-shell timer next backup.timer
```

**MCP 新增工具**：
```rust
Tool::new("security_check").description("检查某操作是否被授权").input_schema(...);
Tool::new("daemon_status").description("daemon 状态和 portal session").input_schema(...);
Tool::new("ime_set_engine").description("切换输入法引擎").input_schema(...);
Tool::new("ime_type").description("通过输入法输入文本（支持非 ASCII）").input_schema(...);
Tool::new("set_brightness").description("设置屏幕亮度 0-100").input_schema(...);
Tool::new("file_picker").description("打开文件选择器并返回路径").input_schema(...);
Tool::new("file_trash").description("移动文件到回收站").input_schema(...);
Tool::new("default_app_get").description("查询默认应用").input_schema(...);
Tool::new("default_app_set").description("设置默认应用").input_schema(...);
Tool::new("bluetooth_scan").description("扫描蓝牙设备").input_schema(...);
Tool::new("bluetooth_connect").description("连接蓝牙设备").input_schema(...);
Tool::new("flatpak_list").description("列出 Flatpak 应用").input_schema(...);
Tool::new("touchpad_set").description("开关触控板/设置自然滚动").input_schema(...);
Tool::new("keyboard_layout_set").description("设置键盘布局").input_schema(...);
Tool::new("secret_set").description("存储密钥").input_schema(...);
Tool::new("secret_get").description("读取密钥").input_schema(...);
Tool::new("list_timers").description("列出 systemd timer").input_schema(...);
```

### 21.36 版本兼容矩阵（2026-08-21 实测）

> **重要**：桌面 DE 的 D-Bus 接口**不跨版本稳定**。下表区分 DDE 20 / DDE 25 / KDE6 / GNOME（默认最新），标注实测结论。

#### 21.36.1 DDE 接口版本差异（实测 DDE 25 vs DDE 20/UOS20）

| 能力 | DDE 25 (Deepin 25, dde-daemon 6.1.84) | DDE 20 (UOS 20 Pro, dde-daemon 5.19.16) | 备注 |
|------|-----------------------------------------|------------------------------------------|------|
| **服务命名** | `org.deepin.dde.*`（主）+ `com.deepin.daemon.*`（别名） | 仅 `com.deepin.daemon.*` | DDE25 双名并存、方法集完全一致；DDE20 无 `org.deepin.dde.Audio1` 等 |
| **音频控制** | 根对象**无** `SetVolume/SetMute`；控制移到 **Sink 子对象** `SetVolume(d)`, `SetMute(b)`, `SetBalance(d)`, `SetFade(d)` | **同左**（Sink 子对象 `SetVolume(d)`） | 两版一致！旧文档假设「Audio1 根对象 SetVolume(v, isPlay)」**已过时** |
| **音频属性** | `Sinks`/`DefaultSink`(object path)/`SinkInputs`/`Cards`(JSON)/`CurrentAudioServer=pipewire` | `Sinks`/`DefaultSink`/`Cards`(JSON)/Sink 属性 `Volume`/`Mute` | 结构一致 |
| **显示** | Display1: `ApplyChanges()`(无参), `GetBrightness(a{sd})`, `SetBrightness(s,d)`, `CanSetBrightness`, `Save`, `GetAll` | **同左**（com.deepin.daemon.Display 方法集与 DDE25 完全一致） | 两版一致，无 `GetConfig()` 方法 |
| **电源** | Power1: `SetPrepareSuspend(i)`, 属性 `BatteryPercentage(a{sd})`, `OnBattery`, `BatteryState(a{su})`, `BatteryIsPresent(a{sb})` | 同左 + `ScreenSaver` 独立服务 `com.deepin.daemon.ScreenSaver` | DDE20 屏保独立服务 |
| **通知** | `org.deepin.dde.Notification1.Notify(appName, replacesId, appIcon, summary, body, actions, hints, timeout)` | `com.deepin.daemon.Notification` 存在 | 方法签名同 freedesktop |
| **输入设备** | `org.deepin.dde.InputDevices1` | `com.deepin.daemon.InputDevices` + `com.deepin.daemon.KWayland`(仅20) | DDE20 独有 KWayland 服务 |
| **键盘** | `org.deepin.dde.Keyboard1` (由 trayplugin 提供) | `com.deepin.daemon.Keybinding` | 服务名不同 |
| **剪贴板** | `org.deepin.dde.Clipboard1` (dde-clipboard) + ClipboardLoader1 + ClipboardManager1 | `com.deepin.daemon.Clipboard` + `com.deepin.daemon.ClipboardManager` | 服务名不同 |
| **区域监控** | `com.deepin.api.XEventMonitor`: `RegisterArea`, `RegisterAreas(a(iiii))`, `RegisterFullScreen`, `CancelAllArea` | **有**（同接口） | 两版一致，X11 下可用 |
| **应用启动** | `dde-am` (dde-application-manager) | `org.deepin.dde.Application1`? — 未在 UOS20 发现 | DDE25 使用 dde-am CLI |
| **KWin 版本** | **kwin 6.1.17** (wayland + x11) | **kwin 6.1.29** (x11 为主) | 两版都是 KWin 6！脚本属性需用 6.x 规范 |

#### 21.36.2 KWin 版本差异（KWin 5 vs KWin 6 脚本 API）

| 脚本 API | KWin 5.x | KWin 6.x (6.1/6.7 实测) | 影响 |
|----------|----------|------------------------|------|
| 窗口几何 | `w.geometry` | `w.rect` / `w.frameGeometry` | **6.x 无 `geometry` 属性** |
| 虚拟桌面 | `w.desktop` | `w.desktops` (数组) | **6.x 无单数 `desktop`** |
| 窗口 ID | `w.internalId` / `w.id` | `w.internalId` (5.x 的 `id` 已移除) | 用 internalId |
| 应用标识 | `w.resourceClass` | `w.resourceName` / `w.resourceClass` | 推荐 resourceClass |
| callDBus | 签名 `callDBus(service, path, iface, method, ...args)` | 同左 | 两版一致，实测可用 |
| org.kde.KWin 方法 | `activeWindow`, `queryWindowInfo` | `queryWindowInfo`, `getWindowInfo(s)` | KWin6 有 `getWindowInfo(string)` |
| D-Bus Scripting | `/Scripting` `loadScript` 可用 | `/Scripting` `loadScript` 可用（实测返回脚本序号，`isScriptLoaded` 确认） | 6.7.4 Wayland 实测成功 |
| Print 输出 | journald `js:` 输出 | journald `js:`（`_COMM=kwin_wayland`） | 实测可读 |

#### 21.36.3 GNOME 版本差异（需在目标环境实测确认）

| 接口 | GNOME 45-46 | GNOME 47+（主流） | 备注 |
|------|-----------|------------------|------|
| `org.gnome.Shell.Eval` | 可用 | **受限/禁用**（GNOME 移除了模块 Eval 权限） | 需确认具体版本边界 |
| Mutter DisplayConfig | `GetCurrentState` → `ApplyMonitorsConfig` 始终存在 | 同左 | 版本稳定 |
| 截图 portal | `org.freedesktop.portal.Screenshot` | 同左 | 稳定 |
| Screensaver | `org.gnome.ScreenSaver.Lock` | 同左 | 稳定 |

#### 21.36.4 适配器内的版本处理策略

**DDE 服务组件实现**：
1. 启动时探测：尝试 `org.deepin.dde.Audio1`（DDE25 名）→ 失败则 `com.deepin.daemon.Audio`（DDE20 名）
2. 音频控制统一走 **Sink 子对象**（两版一致）：`/{base}/Audio1/Sink{N}` 或 `/{base}/Audio/Sink{N}`，读取 `DefaultSink` 属性拿到路径
3. `dde-am` 与 D-Bus 应用启动双通道（v25 优先 dde-am，v20 回退）

**KWin 合成器组件实现**：
1. `KWinMajorVersion`：从 `supportInformation` 解析（"KWin version: 6.7.4" → major=6）
2. 脚本模板按版本选择属性名（geometry vs rect、desktop vs desktops）
3. helpers.js 内置兼容层：

```javascript
// helpers.js —— KWin 5/6 兼容
function winGeometry(w) {
    if (w.frameGeometry) return w.frameGeometry;
    if (w.geometry) return w.geometry;
    return w.rect;
}
function winDesktops(w) {
    // 6.x: w.desktops 数组; 5.x: w.desktop 单值
    if (w.desktops !== undefined) return w.desktops;
    if (w.desktop !== undefined) return [w.desktop];
    return [];
}
```

**HIERARCHY**：所有组件实现按「探测实际接口 → 选择对应实现 → 缓存能力快照」的流程装配，避免把版本假设写死。

**组件内实现要求**：接口调用不能假定单一名字/签名。每个能力方法内先做小步探测（服务是否在 → 对象是否存在 → 方法是否存在），构建 capability 时记录实际使用的接口版本，CLI `agent-shell doctor` 输出版本报告。

---

## 22. 实施架构（架构决策记录）

本节将前 18 章的设计固化为可实施的具体架构，每个决策给出理由和取舍。

### 22.1 决策总表

| # | 决策点 | 结论 | 一句话理由 |
|---|--------|------|-----------|
| D1 | 进程模型 | **三进程：CLI（纯 RPC 客户端）+ user daemon（状态持有）+ rootd（提权）** | daemon 持全部持久化连接，CLI 退化为纯 JSON-RPC 客户端不直连系统服务；权限划分简化为 2 层（非特权/特权）；见第 23 章 |
| D2 | 后端加载 | **编译期 features + 运行时检测** | Rust 生态成熟路径，避免动态加载复杂度 |
| D3 | KWin 桥接 | **InjectScript + callDBus 回传 + 常驻响应服务** | 解决脚本无返回值的核心方案 |
| D4 | 事件流 | **daemon 内 EventHub 归一化 + JSON-RPC 推送** | 单一事件网关，多客户端复用 |
| D5 | Portal 会话 | **daemon 持有 + restore_token 持久化** | 避免每次弹窗，崩溃可恢复 |
| D6 | 安全边界 | **router 层统一检查 + daemon 执行** | 全部命令必经安全层，审计集中 |
| D7 | IME 集成 | **daemon 内 ImeSession + 独立 IBus 连接** | IBus 连接与输入上下文长驻，CLI 无状态 |
| D8 | Wayland 协议 | **优先 portal，直连协议仅做可选能力** | portal 已是标准路径，直连协议工作量大 |
| D9 | 状态持久化 | **XDG 规范路径 + JSON 文件** | `~/.config` 配置、`~/.local/state` 运行时状态 |
| D10 | 跨 DE 路由 | **AgentShell 检测 + trait 动态分发** | 运行时选择 backend，与编译期 features 结合 |

---

### 22.2 D1 进程模型：CLI → daemon 单形态

**核心问题**：portal 会话（ScreenCast/RemoteDesktop）、IME context、事件订阅都必须长驻；daemon 持所有持久化连接，通过事件驱动维护窗口状态缓存，CLI 所有操作经 daemon 路由。

**架构**：

```
┌──────────────┐    ┌──────────────────────────────────────────────────┐
│ CLI 瞬态进程   │    │ daemon 常驻进程                                   │
│ - 所有操作     │───►│ - WindowStateCache（事件驱动更新，类似任务栏）      │
│ - 路由到 daemon│    │ - PortalSessionManager (ScreenCast/RemoteDesktop)│
│ - 无状态      │    │ - ImeSession (IBus 连接)                         │
│ - 不直连 D-Bus│    │ - EventHub + 订阅者管理（事件→状态缓存）            │
│ - 不连显示服务 │    │ - KwinBridge (常驻响应服务+事件脚本)               │
│ - 不连 portal │    │ - D-Bus session bus 连接（KWin/GNOME/Portal）     │
│              │    │ - Wayland 显示服务器连接（wl_display 常驻）          │
│              │    │ - SecurityManager + AuditLogger                   │
│              │    │ - 状态持久化 (sessions.json + state cache)         │
└──────────────┘    └───────────────┬──────────────────────────────────┘
                                    │
                                    ├──► D-Bus (session: KWin/GNOME/Portal)
                                    ├──► Wayland 显示服务器
                                    ├──► portal (ScreenCast/Screenshot/Clipboard)
                                    └──► polkit (system bus 提权 → rootd)
```

**CLI 行为分派**：所有操作经 daemon 统一路由，CLI 不直接与任何系统服务交互。

| 操作类别 | 执行路径 | 说明 |
|---------|---------|------|
| 只读查询（windows/list/status/ps） | CLI → daemon → 状态缓存返回 | 缓存由 EventHub 事件驱动更新，零延迟 |
| 写操作（focus/move/close） | CLI → daemon → DE 原生接口 | 经 daemon 持久化连接执行 |
| 需会话操作（截图流式/远程输入/IME 打字） | CLI → daemon → portal/IBus 会话 | 会话由 daemon 持有 |
| 特权操作 | CLI → daemon → rootd (polkit) | 经 daemon 路由到系统总线 |
| 事件订阅 | CLI → daemon (JSON-RPC 推送) | daemon 的 EventHub 统一分发 |

**daemon 激活策略**：
- D-Bus session activation：首个需要 daemon 的操作自动拉起（`agent-shell.service` systemd user unit, `Restart=on-failure`）
- 空闲超时退出（默认 30min，可配置）：避免常驻浪费
- `agent-shell daemon` 手动前台模式用于调试

**CLI-Daemon 协议**：JSON-RPC 2.0 over stdio（daemon 与 CLI 进程互连），与 MCP 传输同构，复用序列化代码：

```json
{"jsonrpc": "2.0", "id": 1, "method": "windows.list"}
{"jsonrpc": "2.0", "id": 1, "result": {"windows": [...], "from_cache": true}}
```

**取舍**：
- ✅ **CLI 极大简化**：CLI 退化为纯 JSON-RPC 客户端，无需实现 D-Bus/Wayland/X11/portal/AT-SPI 等任何系统服务协议，只维护一个 daemon 连接
- ✅ **权限划分 2 层**：非特权（daemon 域）vs 特权（rootd 域），无需区分「CLI 直连 / 经 daemon / 经 rootd」三路径
- ✅ 事件驱动缓存：CLI 查询走 daemon 缓存，无需主动 D-Bus 调用，比直连更快
- ✅ 统一入口：所有操作走同一条路径，安全/审计/日志集中
- ❌ 简单查询需要 daemon 运行（daemon 自动激活，首次查询有 ~100ms 启动延迟）

---

### 22.3 D2 后端加载：编译期 features + 运行时检测

**架构**：

```
Cargo features:               运行时检测:
[features]                    detect_desktop_environment()
default = ["kwin", "dde",     → DesktopEnvironment::KDE
           "gnome", "hyprland"]  → match DE → 对应 Backend 实现
kwin     = []                 
dde      = ["kwin"]
gnome    = []
hyprland = []
portal   = []

Cargo.toml workspace members:
core, components/*, backends/*, router, event, cli, mcp, daemon, rootd, sdk,
cli, mcp, event, sdk, daemon
```

**feature 设计**：

| feature | 含义 | 依赖 |
|---------|------|------|
| `kwin` | KWin scripting 桥接 | zbus, 内嵌 JS 脚本 |
| `dde` | DDE 服务 | `kwin` + dde-api 代理 |
| `gnome` | GNOME Eval/Extension | zbus + gsettings 可选 |
| `hyprland` | hyprctl socket | tokio Unix socket |
| `x11` | xdotool/wmctrl fallback | 命令调用 |
| `portal-services` | 跨 DE portal 服务 | zbus |
| `ime` | IBus/Fcitx 支持 | zbus |

**编译策略**：
- `cargo build`：全 feature（一个二进制服务所有 DE）
- 发行版打包：按目标 DE 裁剪（`--no-default-features --features kwin,dde`）

**运行时路由**（D10 详述）：

```rust
pub struct AgentShell {
    de_type: DesktopEnvironment,
    backend: Box<dyn CompositorComponent>,
    // 原 SystemServices 大 trait 已废弃，改为公共组件层（components/*）
    input: InputDispatcher,
    capture: CaptureDispatcher,
}

impl AgentShell {
    pub async fn initialize() -> Result<Self> {
        let de_type = detect_desktop_environment();
        let backend = assemble_wm(de_type)?;
        let services = assemble_services(de_type)?;
        // 探测能力，构建 capability 报告
        Ok(Self { ... })
    }
}
```

---

### 22.4 D3 KWin 桥接：完整方案

**问题**：KWin JS Scripting 的 `Script.run()` 不返回结果。历史方案靠 `print()` + journalctl 解析——**慢且脆**。

**方案**：agent-shell 内嵌一个**常驻 D-Bus 响应服务**，KWin 脚本通过 `callDBus` 回传结构化 JSON。

```
启动时:
  agent-shell daemon 注册 D-Bus 服务 com.agent_shell.Response
  路径 /com/agent_shell/response
  接口 com.agent_shell.Response
  方法 sendResult(s: String)   ← KWin 脚本调用这里

每次执行:
  cli.eval(js):
    1. 设置 oneshot channel 等待
    2. loadScript("data:text/plain,<js>")   ← 内联或临时文件
    3. Script.run()
    4. KWin 脚本执行 js，结果 JSON.stringify → callDBus sendResult
    5. daemon 收到 → 通过 channel 返回
    6. Script.stop()
```

**响应服务注册**（Rust 侧）：

```rust
// components/compositor/kwin/src/dbus_bridge.rs

/// 按请求 id 分发回传的共享表（ResponseService 写、查询协程读删）。
#[derive(Default)]
struct ResponseRouter {
    waiters: HashMap<String, oneshot::Sender<String>>,
}
type SharedRouter = Arc<Mutex<ResponseRouter>>;

/// D-Bus ↔ KWin Scripting 桥接（补充通道入口）。
pub struct KWinBridge {
    conn: Connection,                          // session bus 保活句柄
    router: SharedRouter,                      // 按请求 id 的回传路由表
    event_rx: Mutex<Option<mpsc::UnboundedReceiver<Value>>>,  // subscribe 取走
}

/// `com.agent_shell.Response` 响应服务。
struct ResponseService {
    router: SharedRouter,
    events: mpsc::UnboundedSender<Value>,
}

#[zbus::interface(name = "com.agent_shell.Response")]
impl ResponseService {
    /// JS 侧 `callDBus(..., "sendResult", json)` 的接收端。
    /// 必须 camelCase 注解：zbus v5 默认导出 PascalCase `SendResult`，
    /// 与脚本常量 RESPONSE_METHOD 不一致会致 UnknownMethod 5s timeout。
    #[zbus(name = "sendResult")]
    async fn send_result(&self, payload: String) {
        // {"req": "<uuid>", "result": <json>} → 按 req 路由到等待者；
        // 无 req 字段 → 整条 JSON 进事件队列（event_monitor.js）。
        match serde_json::from_str::<Value>(&payload) {
            Ok(v) => match v.get("req").and_then(Value::as_str) {
                Some(req_id) => {
                    let matched = self.router.lock().await.dispatch(req_id, payload);
                    if !matched { tracing::debug!(req = req_id, "no waiter (late/dup)"); }
                }
                None => { let _ = self.events.send(v); }
            },
            Err(e) => tracing::warn!("sendResult non-JSON payload dropped: {e}"),
        }
    }
}
```

方法名常量在 `components/compositor/kwin/src/scripts.rs` 与回传模板共享：
`RESPONSE_METHOD = "sendResult"`，响应服务 `#[zbus(name = "sendResult")]` 与之
对齐（§22.4 D3 方案 + TSI-2436 实测）。

**预置脚本清单**（`scripts.rs` 的 `ScriptTemplate` 枚举，模板 inline 渲染而非文件目录）：

```
components/compositor/kwin/src/scripts.rs  →  ScriptTemplate 变体：
├── list_windows.js
├── get_active_window.js
├── focus_window.js
├── move_window.js
├── resize_window.js
├── close_window.js
├── set_window_geometry.js
├── minimize_window.js
├── maximize_window.js
├── list_workspaces.js
├── switch_workspace.js
├── move_window_to_workspace.js
├── list_monitors.js
└── event_monitor.js                        ← 长驻事件脚本
```

**事件订阅**：`event_monitor.js` 常驻运行（KWin 5/6 兼容由 `scripts.rs` 的 `Compat`
内联生成），把 KWin signals 转成 callDBus 事件；响应服务无 `req` 字段的 payload
整条进事件队列（§22.4 响应服务分支），daemon 里 EventHub 统一接收。

**性能指标目标**：
- 单次 eval 往返：≤ 50ms
- 并发 eval：10 req/s 无阻塞（KWin scripts 本身串行，避免重入）

**风险与对策**：
- `callDBus` 在 KWin 6 有签名差异 → 封装统一 `sendResult`，脚本内兼容版本检测
- Script.stop 有竞态 → 响应到达后 stop，超时后 force stop

---

### 22.5 D4 事件流：daemon 内 EventHub

```
DE 源                     daemon                    订阅者
┌────────────┐    ┌────────────────────────┐   ┌──────────┐
│ KWin events │───►│ EventNormalizer        │──►│ MCP 客户端│
│ Hypr socket2│───►│  (统一 DesktopEvent)    │   ├──────────┤
│ AT-SPI      │───►│ EventHub (fan-out)     │──►│ CLI -f   │
└────────────┘    │  + 事件缓冲 (ring)      │   ├──────────┤
                  │  + 去重/节流/限流        │   │ SDK 订阅  │
                  └────────────────────────┘   └──────────┘
```

**关键设计**：
- **EventHub 单例**：daemon 内只有一个 EventHub，所有源注册进来
- **归一化**：各 DE 原始事件 → 统一 `DesktopEvent` 枚举
- **过滤**：订阅者带 filter，EventHub 只推匹配事件
- **背压**：慢订阅者用 bounded channel，溢出时丢弃低优先级事件（monitor 事件 > 输入事件）
- **缓冲**：环形队列保留最近 N=1000 条，CLI `agent-shell events --replay` 可查历史
- **断线**：订阅者断开自动移除，重连恢复时补发最近事件

**当前实现偏差（Phase 2/TSI-2317，PR #46）**：
- `EventNormalizer` 尚未装配：`daemon/` 无 `RawSource` 注册，`KWinCompositor::
  subscribe()` 未被 daemon 消费，KWin 原始事件流未持续推送。
- 事件仅在 `windows_list` 触发窗口缓存刷新时经「窗口差分」产生（`WindowOpened`
  / `WindowClosed`），`hub.publish` 挂在该轮询差分路径上。
- `events subscribe` 因此只交付 `subscriber_id` 协议与 replay 数据源，不含
  持续的真实事件源推送；`events --replay` 数据同样只反映查询触发的差分历史。
- 后续接线：把 `KWinCompositor::subscribe()` 的原始流包装为 `RawSource`，
  `add_source` 进 `EventNormalizer` 并 `run()`，以恢复本节描述的持续推送语义。

**事件去重/节流**：
- 100ms 窗口内的连续 WindowMoved 合并
- 同一 window state 变化 500ms 内只发一次（防抖）

---

### 22.6 D5 Portal 会话：daemon 持有 + 持久化

**生命周期**：

```
首次创建:
  cli/daemon → PortalSessionManager.create(ScreenCast)
            → portal.CreateSession() → SelectSources → Start()
            → persist_mode=3 (persist_until_revoked) 返回 restore_token
            → 保存 token 到 ~/.local/state/agent-shell/portal_tokens.json

后续复用:
  cli → daemon.session(ScreenCast)
      → 已有活跃会话 → 直接返回
      → 无活跃但 token 存在 → portal.RestoreSession(token) 恢复
      → 都失败 → 新建（触发弹窗）

daemon 重启:
  启动时加载 tokens → 尝试恢复（后台静默）→ 失败则标记 invalid

compositor 重启:
  portal 会话断连 → 监听 org.freedesktop.portal.Desktop 退出
  → 标记会话失效 → 下次调用自动重建
```

**会话清单**（daemon 内）：

| kind | 令牌文件字段 | 恢复方式 |
|------|------------|---------|
| ScreenCast | screencast_token | portal.RestoreSession |
| RemoteDesktop | remotedesktop_token | portal.RestoreSession |
| Clipboard | clipboard_token | portal.RestoreSession |
| Inhibit | — (fd 持有) | daemon 重启时重新 acquire |
| InputCapture | inputcapture_token | portal.RestoreSession |

**持久化文件**：

```json
// ~/.local/state/agent-shell/sessions.json
{
  "version": 1,
  "sessions": {
    "screencast": {
      "kind": "screencast",
      "restore_token": "92f8a1c2-...",
      "created_at": "2026-08-20T10:00:00Z",
      "persist_mode": 3
    }
  }
}
```

---

### 22.7 D6 安全边界：router 层统一检查

**执行链**：

```
CLI/MCP/daemon 命令
      │
      ▼
┌─────────────────────┐
│ SecurityManager     │  ← 唯一入口，所有命令必经
│ check_permission()  │
│  → config 白/黑名单  │
│  → 操作确认覆盖      │
│  → audit 记录        │
└─────────┬───────────┘
          │ Allow
          ▼
      Executor 执行      ← 实际调用 backend/services/input/IME
          
          │ 高风险 (L3/L4)
          ▼
┌─────────────────────┐
│ ConfirmationUI      │
│ daemon 发通知        │  "agent X 请求：关闭服务 nginx"
│ 用户确认/拒绝         │  "同意 / 拒绝"
└─────────────────────┘
```

**确认交互**（无头场景）：
- 通过 `org.freedesktop.Notifications` 发确认请求通知 + 按钮（"允许一次"/"总是允许"/"拒绝"）
- 用户点击→ action 信号回调→ daemon 继续
- 超时（默认 60s）→ 拒绝 + audit

**实现层分离**：
- `core/security.rs`：纯逻辑（配置解析、级别判定、audit 写入），无 IO 依赖可测试
- `daemon/src/dispatch.rs`：确认判定（返回 `ConfirmationRequired`）；交互确认 UI / 通知 / 超时未实现

---

### 22.8 D7 IME 集成：daemon 内 ImeSession

```
CLI 输入 "你好世界"
      │
      ▼
daemon: ImeSession (唯一 IBus 连接 + InputContext)
  1. 确保 context 存在（daemon 启动时惰性创建）
  2. SetGlobalEngine("libpinyin")（如需要）
  3. SetFocusLocation（目标窗口坐标）
  4. 拆分成按键序列：解析输入文本 → 拼音 → 逐键 ProcessKeyEvent
  5. 监听 CommitText / UpdatePreedit 信号
  6. 返回最终上屏文本；验证（AT-SPI 读输入框内容）
      │
      ▼
CLI 结果 {"committed": "你好世界", "verified": true}
```

**为什么 daemon 持有**：
- IBus 连接建立成本高（认证+CreateInputContext）
- InputContext 有状态（预编辑、候选）—— CLI 瞬态进程无法保持
- daemon 重启时 context 重建，但用户已授权过一次

**非 ASCII 判定**：检测输入文本是否含非 ASCII，若无则跳过 IME 直接 type_text（性能路径）。

---

### 22.9 D8 Wayland 协议：portal 优先，直连按需

**决策**：agent-shell v1 **不新增通用直连 Wayland 协议客户端**；具备 portal 覆盖的能力（ScreenCast/RemoteDesktop/Screenshot/Clipboard、OpenURI）一律走 portal，portal 未覆盖的能力（窗口列表/管理、输入注入等）走 DE 专有接口（KWin Scripting、GNOME Eval、hyprctl）或已落地的合成器私有协议绑定。

**代码库现状**（2026-08）：`wayland-client` 已非「可选 feature」，而是合成器 crates 的实际运行时依赖——`components/displayserver/wayland` 提供 `wl_display` 连接与 registry 探测，`components/compositor/wlr-wayland`、`kwin`、`hyprland` 各自叠加 wlr-* / org_kde_* / hyprland_* 私有协议绑定（`mutter` 的 `wayland-client` 仅 dev-dependencies，用于测试）。「直连」的真实语义是**按需的合成器私有协议绑定**，而非零 Wayland 客户端。

理由：
- portal 已是所有现代 DE 的标准路径，API 稳定
- 动态协议 XML 需逐协议维护（wlr-* 不稳定、ext-* 在 staging），只为 portal 未覆盖且 DE 专有接口缺失的能力引入
- 窗口列表/管理等 portal 没有的能力，优先 DE 专有接口；仅当 DE 专有接口不可用时，才以对应合成器私有协议绑定保底

**已落地/待落地的直连绑定**：
- v1 已落地（代表性列举，完整清单见 `components/compositor/wlr-wayland/src/wlr_protocols.rs`、`components/compositor/hyprland/src/wayland.rs`、`components/compositor/kwin/src/wayland.rs`）：wlr-*（`zwlr_foreign_toplevel_manager_v1`、`zwlr_output_manager_v1`、`zwlr_screencopy_manager_v1`、`zwlr_virtual_pointer_manager_v1`、`ext_workspace_manager_v1`、`zwp_virtual_keyboard_manager_v1`、`ext_data_control_manager_v1`）、org_kde_*（window_management/fake_input/virtual_desktop）、hyprland_*（toplevel_export/toplevel_mapping/focus_grab/global_shortcuts）
- v2 候选：`ext_foreign_toplevel_list_v1`（跨 DE 窗口列表保底）、`xdg_activation_v1`（activation token 主动获取）

**绑定 ≠ 消费**：`zwlr_screencopy_manager_v1` 等绑定当前仅能力/兜底登记（`inert_dispatch!`，无原生捕获消费方）；实际捕获走 portal 链（`modules/capture/src/lib.rs`：ScreenCast → Screenshot → X11），与「一律走 portal」不矛盾。

**XDG activation token 不在 v1**：launcher 目前仅透传进程环境已有的 `XDG_ACTIVATION_TOKEN` 给 `gio launch --activation-token`（GLib ≥ 2.76），**不实现** token 的主动获取（`xdg_activation_v1.get_activation_token`）。主动获取放入 v2 候选；DE 聚焦需求在 v1 由 DE 专有路径（KWin activate / GNOME Eval）覆盖（§21.28）。

---

### 22.10 D9 状态持久化：XDG 规范路径

| 内容 | 路径 |
|------|------|
| 配置 | `~/.config/agent-shell/config.toml` |
| Portal tokens | `~/.local/state/agent-shell/sessions.json` |
| 审计日志 | `~/.local/state/agent-shell/audit.log` |
| 事件缓冲 | `~/.local/state/agent-shell/events.jsonl`（可选） |
| 缓存（截图等） | `~/.cache/agent-shell/` |
| 临时 | `$XDG_RUNTIME_DIR/agent-shell/` |
| daemon PID/socket | `$XDG_RUNTIME_DIR/agent-shell/daemon.sock` |

**配置分层**：
```
/etc/agent-shell/config.toml        (系统默认，只读)
~/.config/agent-shell/config.toml   (用户覆盖)
命令行参数 (运行时覆盖)
```
后层覆盖前层，逐项合并。

---

### 22.10 D10 跨 DE 路由：组件体系 + 运行时装配

**核心问题**：agent-shell 如何在不修改代码的情况下适配不同桌面环境，并为每个 DE 选择正确的组件实例？

**架构**：

```
┌──────────────────────────────────────────────────────────────┐
│  AgentShell（顶层装配器）                                       │
│                                                                │
│  1. detect_backend() → BackendKind（Kde/Dde/Gnome/...）        │
│  2. 按 backend 装配清单初始化公共组件实例：                       │
│     Kde     → KWinCompositor + PipeWire + NetworkManager + ... │
│     Dde     → KWinCompositor(deepin-kwin) + PipeWire + ...    │
│     Gnome   → MutterCompositor + PipeWire + NetworkManager + ...│
│  3. DE 封装优先：backend 先探测 DE 专有 D-Bus 服务              │
│     (org.kde.* / org.deepin.dde.* / org.gnome.*)              │
│     命中 → 使用 DE 封装实现；未命中 → 回退公共组件               │
│  4. 所有组件统一实现 DesktopComponent trait                     │
└──────────────────────────────────────────────────────────────┘
```

**选择理由**：

| 方案 | 优点 | 缺点 |
|------|------|------|
| **编译期 features（选用）** | 零运行时开销，可独立编译测试 | 需预知 DE，切换需要重新编译 |
| 运行时动态加载 | 灵活，同一二进制适配所有 DE | 动态加载复杂度高，Rust 生态不成熟 |
| 脚本插件 | 热更新 | 类型安全差，性能差 |

**结论**：编译期 features 控制哪些 DE 被编译进去，运行时 `detect_backend()` 检测当前 DE 并装配对应组件。编译期决定「能跑哪些 DE」，运行时决定「当前是哪个 DE」。两者结合，无需动态加载。

**实现**：

```rust
// backends/{kde,dde,gnome}/src/assemble.rs（装配逻辑分散在各 backend crate）
#[cfg(feature = "kde")]
pub mod kde { pub fn assemble() -> Box<dyn AgentShell> { ... } }
#[cfg(feature = "dde")]
pub mod dde { pub fn assemble() -> Box<dyn AgentShell> { ... } }

pub fn assemble() -> Box<dyn AgentShell> {
    match detect_backend() {
        BackendKind::Kde => {
            #[cfg(feature = "kde")] { return kde::assemble(); }
            #[cfg(not(feature = "kde"))] { compile_error!("KDE backend not compiled"); }
        }
        BackendKind::Dde => { ... }
        _ => { generic::assemble() }
    }
}
```

**验证输出**：
```
agent-shell doctor
  Backend: KDE (Plasma 6.3)
  Compositor: KWinCompositor (Wayland, org_kde_* protocol)
  Audio:     PipeWire (公共组件)
  Power:     KdePowerDevil (org.kde.Solid.PowerManagement ✓)
  Network:   NetworkManagerComponent (公共组件)
  Input:     LibeiInput (portal 通道)
```

### 22.11 实施里程碑与当前状态

> 本节是历史里程碑记录，不是待办路线图。Phase 0–2 的交付物已实现并合入主分支，
> Phase 3–4 为「部分实现/部分接线」状态：组件 crate 与后端装配器已合入，但 daemon
> 侧接线与端到端验收尚未完成。读者不应把未标注完成状态的条目误读为「工作尚未开始」。

**Phase 0：骨架 + KWin/DDE 桥接（2-3 周）— ✅ 已完成**
- core 类型 + trait + error + config
- KwinBridge (callDBus 回传) + 预置脚本
- DE 检测 + AgentShell
- CLI: windows / window focus|move|close / info
- 验收：DDE 环境 `agent-shell windows` 输出正确 JSON

**Phase 1：输入 + 截图 + 无障碍（2-3 周）— ✅ 已完成**
- InputDispatcher：libei/EIS via portal（含会话管理基础）
- CaptureDispatcher：ScreenCast → PipeWire 单帧
- AT-SPI 桥接 + SemanticLocator
- CLI: input / screenshot / a11y
- 验收：能语义化点击按钮（zenity 测试窗口）

**Phase 2：Daemon + 安全 + 事件（2-3 周）— ✅ 已完成**
- daemon 主循环 + JSON-RPC
- PortalSessionManager + 持久化
- SecurityManager + AuditLogger
- EventHub + 归一化 + CLI `-f`
- 验收：`agent-shell daemon` 起后，事件流可订阅

**Phase 3：系统服务（2-3 周）— 🟡 部分实现/部分接线**
- services 组件 crate 已合入：`components/audio|network|power|notification|appearance|clipboard|launcher`（PR #6/#8/#9）
- 基础设施 crate 已合入：`components/systemd|logind`（PR #6）
- daemon 侧大量方法仍为 `stub_ok`（`daemon/src/dispatch.rs:78-101`：brightness/file/mime/bluetooth/flatpak/software/touchpad/kbd/secret/shortcut/timer）
- 验收「全部 CLI 命令在 DDE 实测通过」未达成

**Phase 4：跨 DE + MCP 打磨（2 周）— 🟡 部分实现/部分接线**
- GNOME/Hyprland/Sway backends + 8 后端装配器已合入（PR #37）；X11 兜底 backend 位于 `backends/generic`，尚未验证
- MCP server 18 tools 已实现（PR #39，`mcp/src/server.rs`）
- 「`agent-shell doctor` 全绿」未验证
- 验收：GNOME 容器 + Hyprland 容器冒烟测试 未完成

---

### 22.12 依赖选型

```toml
[workspace.dependencies]
# 核心
tokio = { version = "1", features = ["full"] }
serde = { version = "1", features = ["derive"] }
serde_json = "1"
tracing = "0.1"
tracing-subscriber = "0.3"
clap = { version = "4", features = ["derive"] }     # CLI
anyhow = "1"

# D-Bus
zbus = "5"                    # 异步 D-Bus（核心，几乎所有模块用）
zbus_macros = "5"

# Wayland (运行时依赖：合成器 crates 直连协议绑定)
wayland-client = "0.31"
wayland-protocols = "0.32"

# 截图
pipewire = "0.8"              # ScreenCast 流解析

# 无障碍
atspi = "0.27"

# 测试
criterion = "0.5"             # 性能基准
mockall = "0.13"              # backend mock

# JSON-RPC (CLI↔daemon)
jsonrpsee = "0.24"            # 或手动实现（协议简单）
```

---

## 23. 进程架构与提权模型

### 23.1 问题定义

桌面 Agent 操作天然分成两种权限域，单进程无法同时满足：

| 权限域 | 范围 | 示例 |
|--------|------|------|
| **非特权域（daemon）** | 用户会话内全部操作 | 窗口管理、打字、截图、音量、WiFi、Portal/IME 会话 |
| **特权域（rootd）** | 需要 root / 系统域 | 安装软件、启停系统服务、改系统配置、系统日志 |

若把所有能力塞进一个进程：
- daemon 需持 Portal/IME/Wayland 等会话态连接（天生常驻）
- daemon 若持 root 则爆炸半径过大（被攻破=整机沦陷）
- 全部提权 → polkit 弹窗泛滥，无头场景不可用

### 23.2 三进程架构

```
┌────────────────────────────────────────────────────────────────────┐
│                     用户会话 (user session)                          │
│                                                                    │
│  ┌──────────────┐    ┌──────────────────────────────────────────────────┐     │
│  │   CLI        │    │   agent-shell-daemon (systemd --user)             │     │
│  │  瞬态/无状态   │    │  有状态长驻                                       │     │
│  │              │    │                                                    │     │
│  │  所有操作     │───►│  WindowStateCache (事件驱动更新，类似任务栏)         │     │
│  │  → daemon    │    │  PortalSessionManager   (ScreenCast/RemoteDesktop) │     │
│  │              │    │  ImeSession            (IBus 连接)                 │     │
│  │  不直连 D-Bus│    │  EventHub              (事件网关→更新状态缓存)      │     │
│  │  不连显示服务  │    │  KwinBridge (常驻响应服务+事件脚本)                │     │
│  │  不连 portal  │    │  D-Bus session 连接 (KWin/GNOME/Portal)           │     │
│  │              │    │  Wayland 显示服务器连接 (wl_display 常驻)            │     │
│  │              │    │  SecurityManager       (权限判断)                   │     │
│  │              │    │  AuditLogger           (审计)                      │     │
│  │              │    │  state/ (sessions.json + state cache)              │     │
│  │              │    └──────────┬───────────────────────────────────────┘     │
│  │              │               │ polkit 授权 + D-Bus system bus            │
│  └──────────────┘               ▼                                           │
│                    org.freedesktop.PolicyKit1 (弹窗/规则) → rootd            │
└─────────────────────────────────────────────────────────────────────────────┘
                               │ D-Bus (system bus)
                               │ 仅白名单方法，每个方法 a polkit action
┌──────────────────────────────▼─────────────────────────────────────┐
│             系统域 (root)                                           │
│                                                                    │
│  ┌──────────────────────────────────────────────────────────────┐  │
│  │   agent-shell-rootd (systemd system unit)                    │  │
│  │                                                              │  │
│  │  无业务逻辑，薄代理层：                                        │  │
│  │  - 软件包管理 (apt/dnf/pacman/flatpak system)                 │  │
│  │  - 系统服务启停 (systemd system units)                        │  │
│  │  - 系统日志读取 (journalctl 全量)                              │  │
│  │  - 跨用户/root 进程操作                                        │  │
│  │  - 系统配置修改 (sysctl, /etc 白名单文件)                       │  │
│  │  - 挂载操作 (需要时)                                           │  │
│  │  每个方法独立 polkit action，精确到最小权限                     │  │
│  └──────────────────────────────────────────────────────────────┘  │
└────────────────────────────────────────────────────────────────────┘
```

**为什么 rootd 独立而不用 daemon 提权**：
1. **爆炸半径**：daemon 在用户会话里跑（能访问用户文件、portal、IME），若它也持 root，一次 RCE 就整机沦陷。rootd 只有最小特权接口，被攻破也只能做白名单内的事
2. **生命周期**：daemon 随用户会话起停（loginctl stop-user-session 会杀死），系统服务需要比登录会话更长的生命周期；rootd 由系统 init 管理，登录前/退出后都在
3. **审计**：系统域操作单独记账，与用户域操作分开审计
4. **部署**：rootd 可单独打 deb/rpm（可选安装），无 root 权限的用户（受限环境）可以只装 CLI+daemon，特权操作返回 "rootd not installed" 并降级到"只能操作用户态"

### 23.3 能力归属矩阵

> **新模型**：权限划分简化为 2 层——非特权（daemon 域）vs 特权（rootd 域）。CLI 退化为纯 JSON-RPC 客户端，不连接任何系统服务。daemon 持有全部持久化连接（D-Bus session bus、Wayland display、portal 会话、KWin Scripting 等），CLI 通过 JSON-RPC 请求 daemon 执行，查询类操作由 daemon 的 WindowStateCache 事件驱动缓存直接返回。

| 操作类别 | user daemon（CLI 经过 daemon） | rootd (polkit) |
|---------|:----------------------------:|:-------------:|
| **窗口/工作区/监视器** | ✓（缓存 + 写操作走 daemon 持久化连接） | — |
| **输入模拟** | ✓ (portal 会话) | — |
| **截图/录屏** | ✓ (portal 会话) | — |
| **AT-SPI 无障碍** | ✓（daemon 持 AT-SPI 连接） | — |
| **音频/亮度** | ✓（daemon 持 D-Bus 连接） | — |
| **网络 WiFi** | ✓ (NetworkManager user perms) | 系统网络配置可能需要 |
| **应用启动** | ✓（daemon 持 portal/DE 连接） | — |
| **通知** | ✓（daemon 持通知 D-Bus 连接） | — |
| **剪贴板** | ✓ (portal 会话) | — |
| **壁纸/外观** | ✓（daemon 持 portal/DE 连接） | — |
| **systemd user units** | ✓（daemon 持 systemd user D-Bus） | — |
| **systemd system units** | — | **✓** |
| **日志 (user journal)** | ✓（daemon 持 journal D-Bus） | — |
| **日志 (system/full)** | — | **✓** (或 adm 组) |
| **D-Bus 管理** | ✓（daemon 持 session bus） | — |
| **进程 (同用户)** | ✓（daemon 持 /proc 访问） | — |
| **进程 (跨用户/root)** | — | **✓** |
| **文件浏览 (用户态)** | ✓ | — |
| **挂载** | — | **✓** (直接) |
| **软件安装 (用户态 flatpak)** | ✓ (flatpak user) | — |
| **软件安装 (系统)** | — | **✓** |
| **IME 输入法** | ✓ (IBus 连接) | — |
| **蓝牙** | ✓（daemon 持 D-Bus 连接） | — |
| **Secret Service** | ✓（daemon 持 D-Bus 连接） | — |
| **系统信息** | ✓（daemon 持 D-Bus 连接） | — |

### 23.4 提权通道设计

#### 23.4.1 通道优先级

| 优先级 | 通道 | 使用场景 | 说明 |
|:-----:|------|---------|------|
| 1 | **rootd D-Bus + polkit** | 大部分需提权操作 | 无头友好（可配置自动放行）、集中审计 |
| 2 | **polkit 授权 daemon 自身** | 少数受控操作 | 如通过 polkit 授权挂载 (udisks2 自带) |
| 3 | **pkexec 瞬态** | 一次性的、未在 rootd 白名单的命令 | 每次弹窗，仅调试兜底 |

#### 23.4.2 polkit action 定义

```xml
<!-- /usr/share/polkit-1/actions/com.agentshell.policy -->
<?xml version="1.0" encoding="UTF-8"?>
<!DOCTYPE policyconfig PUBLIC
  "-//freedesktop//DTD PolicyKit Policy Configuration 1.0//EN"
  "http://www.freedesktop.org/standards/PolicyKit/1/policyconfig.dtd">
<policyconfig>
  <vendor>Agent Shell</vendor>

  <!-- 软件包 -->
  <action id="com.agentshell.pkexec.install-package">
    <description>Install a software package</description>
    <message>Authentication is required to install packages</message>
    <defaults>
      <allow_any>no</allow_any>
      <allow_inactive>no</allow_inactive>
      <allow_active>auth_admin_keep</allow_active>
    </defaults>
    <annotate key="org.freedesktop.policykit.exec.path">/usr/lib/agent-shell/rootd-pkexec</annotate>
    <annotate key="org.freedesktop.policykit.exec.allow_gui">true</annotate>
  </action>

  <action id="com.agentshell.package.remove">
    <description>Remove a software package</description>
    <defaults>
      <allow_any>no</allow_any>
      <allow_inactive>no</allow_inactive>
      <allow_active>auth_admin_keep</allow_active>
    </defaults>
  </action>

  <action id="com.agentshell.package.update">
    <description>Update software packages</description>
    <defaults>
      <allow_any>no</allow_any>
      <allow_inactive>no</allow_inactive>
      <allow_active>auth_admin_keep</allow_active>
    </defaults>
  </action>

  <action id="com.agentshell.package.refresh">
    <description>Refresh package metadata cache</description>
    <defaults>
      <allow_any>no</allow_any>
      <allow_inactive>no</allow_inactive>
      <allow_active>auth_admin_keep</allow_active>
    </defaults>
  </action>

  <!-- systemd system 单元 -->
  <action id="com.agentshell.service.control">
    <description>Start/stop/restart a system service</description>
    <defaults>
      <allow_any>no</allow_any>
      <allow_inactive>no</allow_inactive>
      <allow_active>auth_admin_keep</allow_active>
    </defaults>
  </action>

  <action id="com.agentshell.systemd.manage">
    <description>Manage systemd system units (enable/disable/daemon-reload)</description>
    <defaults>
      <allow_any>no</allow_any>
      <allow_inactive>no</allow_inactive>
      <allow_active>auth_admin_keep</allow_active>
    </defaults>
  </action>

  <!-- 系统日志 -->
  <action id="com.agentshell.system-log.view">
    <description>View full system journal</description>
    <defaults>
      <allow_any>no</allow_any>
      <allow_inactive>no</allow_inactive>
      <allow_active>auth_admin_keep</allow_active>
    </defaults>
  </action>

  <!-- 系统配置 -->
  <action id="com.agentshell.sysctl.get">
    <description>Read kernel parameters</description>
    <defaults>
      <allow_any>no</allow_any>
      <allow_inactive>no</allow_inactive>
      <allow_active>auth_admin_keep</allow_active>
    </defaults>
  </action>

  <action id="com.agentshell.sysctl.set">
    <description>Modify kernel parameters</description>
    <defaults>
      <allow_any>no</allow_any>
      <allow_inactive>no</allow_inactive>
      <allow_active>auth_admin_keep</allow_active>
    </defaults>
  </action>

  <action id="com.agentshell.hostname.set">
    <description>Set system hostname</description>
    <defaults>
      <allow_any>no</allow_any>
      <allow_inactive>no</allow_inactive>
      <allow_active>auth_admin_keep</allow_active>
    </defaults>
  </action>

  <!-- 进程管理 -->
  <action id="com.agentshell.process.kill">
    <description>Kill a process (cross-user)</description>
    <defaults>
      <allow_any>no</allow_any>
      <allow_inactive>no</allow_inactive>
      <allow_active>auth_admin_keep</allow_active>
    </defaults>
  </action>

  <!-- 挂载 -->
  <action id="com.agentshell.mount">
    <description>Mount or unmount filesystems</description>
    <defaults>
      <allow_any>no</allow_any>
      <allow_inactive>no</allow_inactive>
      <allow_active>auth_admin_keep</allow_active>
    </defaults>
  </action>

  <!-- 只读特例：Job 状态查询无系统副作用 -->
  <action id="com.agentshell.job.status">
    <description>Query package operation job status</description>
    <defaults>
      <allow_any>no</allow_any>
      <allow_inactive>no</allow_inactive>
      <allow_active>yes</allow_active>
    </defaults>
  </action>
</policyconfig>
```

**关键点**：`auth_admin_keep` —— 管理员认证一次后，本次会话内记住授权（不每次弹窗），兼顾安全与可用性。

#### 23.4.3 rootd D-Bus 接口

```xml
<!-- 只暴露必要的白名单接口，方法名即操作名 -->
<node name="/org/agentshell/Rootd">
  <!-- 版本对账：daemon 和 rootd 需匹配安全模型版本 -->
  <interface name="org.agentshell.Rootd">
    <method name="Hello">
      <arg type="s" direction="out"/>
    </method>

    <!-- 软件包 -->
    <method name="PackageInstall">
      <arg type="as" direction="in"/>  <!-- 包名列表 -->
      <arg type="s" direction="out"/>  <!-- job id -->
    </method>
    <method name="PackageRemove">
      <arg type="as" direction="in"/>
      <arg type="s" direction="out"/>
    </method>
    <method name="PackageUpdate">
      <arg type="as" direction="in"/>  <!-- 空 = 全部 -->
      <arg type="s" direction="out"/>
    </method>
    <method name="PackageRefresh">
      <arg type="s" direction="out"/>
    </method>

    <!-- systemd system 单元 -->
    <method name="ServiceStart">
      <arg type="s" direction="in"/>
    </method>
    <method name="ServiceStop">
      <arg type="s" direction="in"/>
    </method>
    <method name="ServiceRestart">
      <arg type="s" direction="in"/>
    </method>
    <method name="ServiceEnable">
      <arg type="s" direction="in"/>
    </method>
    <method name="ServiceDisable">
      <arg type="s" direction="in"/>
    </method>
    <method name="ServiceReload">
      <arg type="s" direction="in"/>
    </method>
    <method name="DaemonReload">

    <!-- 系统日志 -->
    <method name="JournalQuery">
      <arg type="s" direction="in"/>  <!-- 过滤表达式 JSON -->
      <arg type="s" direction="out"/> <!-- JSON 行流 -->
    </method>

    <!-- 系统配置 -->
    <method name="SysctlGet">
      <arg type="s" direction="in"/>
      <arg type="s" direction="out"/>
    </method>
    <method name="SysctlSet">
      <arg type="s" direction="in"/>  <!-- key -->
      <arg type="v" direction="in"/>  <!-- value -->
    </method>
    <method name="HostnameSet">
      <arg type="s" direction="in"/>
    </method>

    <!-- 进程（跨用户） -->
    <method name="ProcessKill">
      <arg type="i" direction="in"/>  <!-- pid -->
      <arg type="i" direction="in"/>  <!-- signal -->
    </method>

    <!-- 挂载 -->
    <method name="Mount">
      <arg type="s" direction="in"/>  <!-- device -->
      <arg type="s" direction="in"/>  <!-- target -->
      <arg type="s" direction="in"/>  <!-- fstype -->
      <arg type="as" direction="in"/> <!-- options -->
    </method>
    <method name="Unmount">
      <arg type="s" direction="in"/>  <!-- target -->
    </method>

    <!-- 会话 Token 管理 -->
    <method name="SetToken">
      <arg type="s" direction="in"/>  <!-- 持久化 token（可选） -->
    </method>
    <!-- Job 状态查询 -->
    <method name="JobStatus">
      <arg type="s" direction="in"/>   <!-- job id -->
      <arg type="s" direction="out"/>  <!-- JSON: {found, method, progress, done, success, exit_code, stderr} -->
    </method>

    <signal name="JobProgress">
      <arg type="s"/>  <!-- job id -->
      <arg type="d"/>  <!-- 0.0-1.0 -->
    </signal>
    <signal name="JobDone">
      <arg type="s"/>
      <arg type="b"/>
    </signal>
  </interface>
</node>

### 23.5 多会话与多用户场景

**设计约束**：每个用户 session 一个 daemon 实例，不跨 session 共享。

```
┌──────────────────────────────────────────────────┐
│  Linux 会话 1（用户 A，tty1）                     │
│  ┌──────┐  ┌──────────┐  ┌───────────┐          │
│  │ CLI  │──│ daemon A │──│ rootd     │          │
│  └──────┘  └──────────┘  └───────────┘          │
│               │ D-Bus session bus (user A)        │
│               │ Wayland display (wayland-0)       │
│               │ systemd --user (user A)           │
├──────────────────────────────────────────────────┤
│  Linux 会话 2（用户 B，tty2，快速用户切换）        │
│  ┌──────┐  ┌──────────┐  ┌───────────┐          │
│  │ CLI  │──│ daemon B │──│ rootd     │          │
│  └──────┘  └──────────┘  └───────────┘          │
│               │ D-Bus session bus (user B)        │
│               │ Wayland display (wayland-1)       │
│               │ systemd --user (user B)           │
└──────────────────────────────────────────────────┘
```

**关键规则**：

| 场景 | 行为 | 说明 |
|------|------|------|
| **同一用户多 TTY** | 每个 tty 启动独立 daemon | systemd --user 服务自动按 session 隔离，每个 session 有独立 D-Bus session bus |
| **快速用户切换** | 用户 B 登录后启动独立 daemon B | daemon A 保持运行但不活跃，agent-shell 操作作用于 active VT |
| **SSH 远程** | 无 Wayland display，无 daemon | `agent-shell` 返回 `BackendUnavailable("no display server")`；仅系统服务（systemd/logind）通过 D-Bus system bus 可用 |
| **同一 seat 多用户** | 不支持同时操作 | 当前仅 active session 可操作窗口/输入/截图；非活跃 session 的 daemon 等待 VT 切换事件 |
| **root 用户** | 不启动 daemon | root 无 session bus，无 Wayland 显示。系统管理操作走 rootd（polkit 授权） |

**多 session 检测**（daemon 启动时）：

```rust
fn check_session_validity() -> Result<SessionContext> {
    if env::var("WAYLAND_DISPLAY").is_err() && env::var("DISPLAY").is_err() {
        return Err(AgentShellError::BackendUnavailable("no display server"));  // SSH/tty 无 DE
    }
    let uid = whoami::uid();
    let session_id = env::var("XDG_SESSION_ID").ok();
    let seat = env::var("XDG_SEAT").unwrap_or("seat0".into());
    Ok(SessionContext { uid, session_id, seat })
}
```

**单实例保护**：同一 session 内不允许多个 daemon 实例。通过 `$XDG_RUNTIME_DIR/agent-shell.lock` 文件锁实现：

```rust
fn acquire_lock() -> Result<File> {
    let path = Path::new(&env::var("XDG_RUNTIME_DIR")?).join("agent-shell.lock");
    let file = std::fs::OpenOptions::new()
        .create(true).write(true).read(true).open(&path)?;
    if file.try_lock_exclusive().is_err() {
        return Err(AgentShellError::BackendUnavailable("daemon already running"));
    }
    Ok(file)  // 持有锁直到进程退出
}
```

**事件分发**：多 session 下事件仅推送到当前 session 内的 daemon，不跨 session 转发。rootd 是系统级单例，处理所有 session 的提权请求，通过 `UID` 参数区分调用者。