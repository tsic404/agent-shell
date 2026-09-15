---
name: agent-shell-design
description: 实现 agent-shell 后端或模块时加载。含类型系统、组件 trait 契约、合成器继承层次、装配流程、协议映射、TTY 模式、版本兼容矩阵、模块查询指引。
tags: [agent-shell, linux-desktop, compositor, dbus, kwin, dde, gnome, hyprland, wayland, x11]
---

# agent-shell 设计契约

本技能是 **契约摘要**。实现时先看本文件的契约，需要完整详细设计（伪代码/场景/推导/调研记录）时按 **模块查询指引** 去仓库 `docs/design/` 对应目录获取。

## 模块查询指引

| 需要查什么 | 技能内位置（本节） | 仓库详细设计路径 |
|-----------|------------------|-----------------|
| 类型定义 | §类型系统 | `docs/design/01-core-types/` |
| 组件体系/装配契约 | §组件体系、§装配流程 | `docs/design/02-architecture/` |
| Wayland 协议绑定/版本协商 | §合成器、§Wayland 绑定 | `docs/design/03-compositor/wayland-display-server.md` |
| X11 EWMH/XTest | §合成器 | `docs/design/03-compositor/x11-display-server.md` |
| 各 DE 合成器 | §协议映射 | `docs/design/03-compositor/{kwin,mutter,hyprland,dde,fallback}.md` |
| 输入降级链 | §协议映射 | `docs/design/04-input/` |
| 截图捕获 | §协议映射 | `docs/design/05-capture/` |
| AT-SPI 无障碍 | §协议映射 | `docs/design/06-a11y/` |
| 语义路由 | §协议映射 | `docs/design/07-routing/` |
| DE 检测逻辑 | §DE 检测 | `docs/design/08-detection/` |
| CLI/MCP 接口 | §进程模型 | `docs/design/09-agent-interface/` |
| 事件模型/事件源 | §事件 | `docs/design/10-events/` |
| 错误/超时/重试 | §超时与重试 | `docs/design/11-error-handling/` |
| 构建部署 | — | `docs/design/12-build-deploy/` |
| Systemd/Logind 接口 | §系统服务 | `docs/design/13-system-services/` |
| ADR 决策 | §关键架构决策 | `docs/design/14-architecture-decisions/` |
| 进程/提权模型 | §进程模型、§多会话 | `docs/design/15-process-architecture/` |
| 完整单文件版 | — | `docs/agent-shell-design.md`（23 章合并版） |

## 类型系统（core/src/types.rs）

```rust
pub enum DesktopEnvironment {
    KDE, GNOME, Hyprland, DDE, Sway, Budgie, XFCE, Cinnamon, Cosmic, LXQt, MATE,
    WLRWayland, X11Generic, Tty, Unknown,
}
pub enum BackendKind { Kde, Dde, Gnome, Hyprland, Sway, X11Generic, WLRWayland, Tty }

pub struct WindowId { pub native_id: String, pub de_type: DesktopEnvironment }

pub struct WindowInfo {
    pub id: WindowId, pub title: String, pub app_id: String, pub pid: u32,
    pub geometry: Rect, pub frame_geometry: Rect,
    pub states: Vec<WindowState>,   // 多状态可共存（非单值枚举）
    pub workspace_id: Option<WorkspaceId>, pub monitor_id: Option<MonitorId>,
    pub stacking_order: u32, pub desktop_file: Option<String>,
    pub window_type: WindowType, pub icon_geometry: Option<Rect>, pub keep_above: bool,
}

pub enum WindowState { Normal, Minimized, Maximized, FullScreen, Hidden }
pub enum WindowType { Normal, Dialog, Dock, Desktop, DropdownMenu, Tooltip, Notification, Splash, Utility, Unknown }

// 系统服务类型（§21 有完整字段）
pub enum UnitStatus { Active, Reloading, Inactive, Failed, Activating, Deactivating, Unknown }
pub struct SessionInfo { pub id: String, pub uid: u32, pub user_name: String, pub seat: String, pub display: String, pub remote: bool, pub remote_host: Option<String>, pub state: String, pub tty: Option<String> }
```

- 事件也是 `states: Vec<WindowState>`（§18.1 `WindowStateChanged { id, states }`）
- `AgentShellError` 用 thiserror：UnsupportedDE/BackendUnavailable/WindowNotFound/DBus/Input/Capture/Permission/Timeout/NotImplemented/Other

## 组件体系（§3）

**两层**：`components/`（公共组件抽象接口 + 具体实例）+ `backends/`（DE 装配器）。所有组件实现 `DesktopComponent`。

```rust
pub trait DesktopComponent: Send + Sync {
    fn name(&self) -> &'static str;
    fn component_type(&self) -> ComponentType;
    fn is_available(&self) -> bool;
    async fn health(&self) -> ComponentHealth;
}
```

`ComponentRegistry` 所有字段为 `Option<Box<dyn ...>>`：

```rust
pub struct ComponentRegistry {
    pub compositor: Option<Box<dyn CompositorComponent>>,   // TTY 为 None
    pub audio: Option<Box<dyn AudioServerComponent>>,
    pub network: Option<Box<dyn NetworkComponent>>,
    pub input: Option<Box<dyn InputComponent>>,
    pub capture: Option<Box<dyn CaptureComponent>>,
    pub a11y: Option<Box<dyn A11yComponent>>,
    pub clipboard: Option<Box<dyn ClipboardComponent>>,
    pub power: Option<Box<dyn PowerComponent>>,
    pub notification: Option<Box<dyn NotificationComponent>>,
    pub appearance: Option<Box<dyn AppearanceComponent>>,
    pub launcher: Option<Box<dyn LauncherComponent>>,
    pub init_system: Option<Box<dyn SystemComponent>>,          // systemd
    pub session_manager: Option<Box<dyn SessionManagerComponent>>,  // logind
}
```

系统服务是唯一"通用必有"组件族：

```rust
pub trait SystemComponent: DesktopComponent {
    async fn daemon_reload(&self) -> Result<(), AgentShellError>;
    async fn list_units(&self) -> Result<Vec<SystemdUnit>, AgentShellError>;
    async fn start_unit(&self, name: &str) -> Result<(), AgentShellError>;
    async fn stop_unit(&self, name: &str) -> Result<(), AgentShellError>;
    async fn enable_unit(&self, name: &str) -> Result<(), AgentShellError>;
    async fn disable_unit(&self, name: &str) -> Result<(), AgentShellError>;
    async fn unit_status(&self, name: &str) -> Result<UnitStatus, AgentShellError>;
}
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
```

## 装配流程（§4）

```
detect_and_assemble():
  XDG_SESSION_TYPE × XDG_CURRENT_DESKTOP → DesktopEnvironment
  → match DE → Backend::assemble(event_hub) → ComponentRegistry
  Tty → TtyBackend（仅 Systemd + Logind，其余 None）
```

- 合成器必选按 DE 装配（KDE→KWinCompositor，TTY→None）；音频探测 pipewire→pulseaudio→None
- **DE 封装优先**：能力调用先走 DE 接口（org.kde.* / org.deepin.dde.* / org.gnome.*），无接口回退公共服务（portal / freedesktop / 公共组件通用实例）。实现于 backend 的 `services.rs` / `dde_api.rs`
- 完整装配伪代码见仓库 `docs/design/02-architecture/`

## 合成器继承层次（§3.3/§5–11）

```
CompositorComponent（trait——所有合成器统一接口）
├── WaylandCompositor（抽象类，组合 WaylandDisplayServer）
│   ├── WlrWaylandCompositor（wlroots 基类，自身完整实现）
│   │   ├── TreelandCompositor（+ treeland_* 私有协议）
│   │   ├── HyprlandCompositor（+ hyprland_* 私有协议）
│   │   └── SwayCompositor（+ Sway IPC）
│   ├── KWinCompositor（直接继承，org_kde_* 私有协议）
│   └── MutterCompositor（直接继承，D-Bus Eval/Extension）
└── X11Compositor（直接实现 CompositorComponent，组合 X11DisplayServer，EWMH/XTest）
```

## Wayland 绑定（§5）

- 协议集合：foreign-toplevel v3 / ext-workspace / virtual-pointer v2 / virtual-keyboard / screencopy v3 / output-mgmt / data-control
- registry 遍历：`global.interface` 匹配 + `ver >= min`；请求版本 = min(规范最低, global 公布)；绑定失败→None 不阻塞
- `org_kde_plasma_window_management` 单客户端绑定限制：仅 daemon 绑定，不与 Plasma 面板冲突
- 降级：foreign-toplevel 缺失→D-Bus/Scripting/hyprctl/AT-SPI；virtual-pointer 缺失→libei→ydotool→XTest；screencopy 缺失→portal ScreenCast

## 协议映射（§5–11）

| 能力 | KWin | DDE | GNOME | Hyprland | X11 |
|------|------|-----|-------|----------|-----|
| 窗口列表 | foreign-toplevel → Scripting | 同 KWin / treeland_* | Eval / Extension → AT-SPI | foreign-toplevel → hyprctl | EWMH `_NET_CLIENT_LIST` |
| 输入注入 | org_kde_kwin_fake_input | 同 KWin | 无（libei/portal） | zwlr-virtual-pointer | XTest |
| 截图 | zwlr-screencopy → portal | zwlr-screencopy → treeland_capture | portal ScreenCast(PipeWire) | toplevel_export → portal | MIT-SHM |
| 工作区 | org_kde_plasma_virtual_desktop_management | 同 KWin | D-Bus | ext-workspace → hyprctl | `_NET_CURRENT_DESKTOP` |

- **KWin 双通道**：org_kde_* 协议优先（窗口管理/输入/截图），Scripting D-Bus 降级（几何操作）
- **Mutter 无 Wayland 私有协议**：D-Bus Eval（<47）/ Extension（47+）+ portal

## DE 检测（§16）

- 判据：`XDG_SESSION_TYPE`（wayland/x11）+ `XDG_CURRENT_DESKTOP`（KDE/GNOME/Hyprland/dde…）
- 无显示服务器（SSH/TTY）：`is_tty_session() && 无 WAYLAND_DISPLAY/DISPLAY` → `DesktopEnvironment::Tty`
- 未知 compositor：WLRWayland 保底；未知 X11 WM：X11Generic
- 探测逻辑代码见仓库 `docs/design/08-detection/`

## 系统服务（§21，实测 2026-08）

- systemd：`org.freedesktop.systemd1` Manager 接口 + unit 属性
- logind：`org.freedesktop.login1` Manager/Session/Seat
- **DDE 版本差异**：DDE25 = `org.deepin.dde.*`（主）+ `com.deepin.daemon.*`（别名）；DDE20 仅 `com.deepin.daemon.*`
- **KWin 版本**：KWin6 用 `w.frameGeometry` / `w.desktops`（数组）；KWin5 用 `w.geometry` / `w.desktop`
- 详细接口签名矩阵见仓库 `docs/design/13-system-services/`

## 超时与重试（§19）

| 通道 | 超时 | 重试 |
|------|------|------|
| D-Bus | 5s | 2 |
| KWin Scripting | 5s | 1 |
| hyprctl | 2s | 2 |
| ScreenCast | 10s | 1 |
| input 注入 | 1s | 3 |
| screenshot | 8s | 2 |

## 进程模型（§23）

- **CLI**：纯 RPC 客户端，只连 user daemon（stdio JSON-RPC），不直连显示服务器/D-Bus/portal
- **daemon**：持有全部持久化连接（显示服务器/D-Bus/portal）+ `WindowStateCache`（事件驱动，类似任务栏）；单实例 `$XDG_RUNTIME_DIR/agent-shell.lock`；权限：非特权 daemon 域
- **rootd**：系统级单例，polkit（`auth_admin_keep`）授权；特权 rootd 域（软件包/系统服务/系统配置）
- 多会话：每 session 一个 daemon（systemd --user 隔离）；SSH 场景 `BackendUnavailable("no display server")`，仅系统服务可用

## 事件（§18）

- 统一携带 `source: EventSource` + `occurred_at: Instant`
- 18 类事件：WindowOpened/Closed/Focused/Moved/StateChanged/MetadataChanged/StackingChanged、WorkspaceChanged/ListChanged/WindowMoved、MonitorHotplug/Changed、PointerButton/KeyComboPressed、AppLaunched/Exited、FullscreenChanged、PowerStateChanged、AccessibilityTreeChanged
- 优先级：High（窗口开关/聚焦）> Medium（工作区/监视器/输入）> Low（音量/DPMS/外观）
- EventHub 有界通道 capacity=1024，WindowMoved 100ms 合并
- 事件源映射表见仓库 `docs/design/10-events/`

## 关键架构决策（§22 ADR）

| # | 决策 |
|---|------|
| D1 | 三进程 CLI+daemon+rootd；CLI 纯 RPC 客户端，daemon 持全部连接 |
| D2 | 编译期 features + 运行时检测 |
| D3 | 组件体系是唯一契约：DesktopComponent trait + backend 装配清单 |
| D4 | EventHub 归一化 + JSON-RPC |
| D5 | daemon 持有 portal 会话 + restore_token |
| D6 | router 统一安全检查 |
| D7 | daemon 内 ImeSession + IBus |
| D8 | portal 优先 |
| D9 | XDG 规范路径 |
| D10 | 组件体系 + 运行时装配；跨 DE 路由 |

## 实施层级（依赖顺序）

```
T0 核心类型 + trait 契约
 ├─ T1 合成器后端（KWin/Mutter/Hyprland/DDE/WlrWayland+X11/Sway/TTY）
 │   ├─ T2 功能模块（input/capture/a11y/clipboard/power/notification/appearance/launcher）
 │   └─ T3 系统服务 + 事件 + 路由 + 检测
 └──── T4 Agent 接口 + 进程 + 构建部署
```
