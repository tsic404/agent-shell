//! Wayland 私有协议通道（设计文档 §7.6，`wayland.rs`）。
//!
//! 在共享的 [`WaylandDisplayServer`]（同一 `wl_display`）之上绑定 KWin
//! 私有协议族：
//!
//! | 协议 | 接口版本上限 | 用途 |
//! |------|:----:|------|
//! | `org_kde_plasma_window_management` | 18（生成绑定上限；运行时要求 ≥12） | 窗口管理（单客户端，短绑 D8） |
//! | `org_kde_kwin_fake_input` | 5（生成绑定上限） | 输入注入（需 authenticate） |
//! | `org_kde_plasma_virtual_desktop_management` | 2 | 虚拟桌面 |
//!
//! 致命错误——对应字段为 `None`，上层按通道选择矩阵回退 Scripting。
//! window_management 的 uuid/stacking-order 能力自 v12/v17 起，
//! fake_input 的 keyboard_key 自 v4 起，运行时按公布版本门控请求。
//!
//! # 曝露条件（KWin global 过滤）
//!
//! KWin 服务端**始终创建**这三个协议 global（`WaylandServer::start()` 无条件
//! new `PlasmaWindowManagementInterface`；`FakeInputBackend::initialize()` 无条件
//! `init`），但 `KWinDisplay::allowInterface`（wayland_server.cpp）按客户端
//! 过滤 registry 广告：
//!
//! - **KWin ≤ 6.7.x**：`window_management` / `fake_input` 在
//!   `interfacesBlackList` 中，仅当客户端可执行文件匹配某个 .desktop 且其
//!   `X-KDE-Wayland-Interfaces=` 声明了该接口（KApplicationTrader 按
//!   Exec 规范路径匹配）才广告；未声明 → registry 不出现（即
//!   [`BindError::NotPresent`]）。无 .desktop 的裸进程一律被拒。
//! - **KWin ≥ master（6.8+）**：commit f9bf0ee6 起改为仅按 systemd cgroup
//!   判定沙箱（app.slice 下 flatpak/snap）才隐藏；普通进程全部可见。
//! - `virtual_desktop_management` 从不在黑名单中——任何客户端可见。
//!
//! 因此「global 缺失」≠「协议不存在」：诊断时优先怀疑过滤而非版本。
//! 测试/部署侧对策：将启动器 .desktop 声明 `X-KDE-Wayland-Interfaces=`，
//! 或设 `KWIN_WAYLAND_NO_PERMISSION_CHECKS=1`（仅 ≤6.7.x 生效）。
//! 绑定策略不变（§7.2 / 决策 D8）：任一协议缺失/版本过低/被占用都不视为
//! 致命错误——对应字段为 `None`，上层按通道选择矩阵回退 Scripting。

use wayland_client::globals::GlobalList;
use wayland_client::protocol::wl_registry::WlRegistry;
use wayland_client::Proxy as _;
use wayland_client::{Connection, Dispatch, QueueHandle};
use wayland_protocols_plasma::fake_input::client::org_kde_kwin_fake_input::OrgKdeKwinFakeInput;
use wayland_protocols_plasma::plasma_window_management::client::org_kde_plasma_stacking_order::OrgKdePlasmaStackingOrder;
use wayland_protocols_plasma::plasma_window_management::client::org_kde_plasma_window::OrgKdePlasmaWindow;
use wayland_protocols_plasma::plasma_window_management::client::org_kde_plasma_window_management::{
    OrgKdePlasmaWindowManagement, State as PwmState, ShowDesktop,
};
use wayland_protocols_plasma::plasma_virtual_desktop::client::org_kde_plasma_virtual_desktop::OrgKdePlasmaVirtualDesktop;
use wayland_protocols_plasma::plasma_virtual_desktop::client::org_kde_plasma_virtual_desktop_management::OrgKdePlasmaVirtualDesktopManagement;

use crate::error::KWinError;
use agent_shell_core::error::Result;
use agent_shell_displayserver_wayland::WaylandDisplayServer;

/// 各私有协议的最低可用版本与生成绑定版本上限（§7.6 版本表 × scanner 上界）。
///
/// 上界必须 ≤ 生成接口 version（`GlobalList::bind` 在取 min 前断言，
/// 越界 panic——见 wayland-client 0.31 globals.rs:167）。
pub mod min_versions {
    /// window_management：get_window_by_uuid 需 v12。
    pub const WINDOW_MANAGEMENT_MIN: u32 = 12;
    /// window_management 生成接口最高 v18。
    pub const WINDOW_MANAGEMENT_MAX: u32 = 18;
    /// fake_input：keyboard_key 需 v4。
    pub const FAKE_INPUT_MIN: u32 = 4;
    /// fake_input 生成接口最高 v5。
    pub const FAKE_INPUT_MAX: u32 = 5;
    /// virtual_desktop_management：基础能力 v1 即可。
    pub const VIRTUAL_DESKTOP_MIN: u32 = 1;
    /// virtual_desktop_management 生成接口最高 v2。
    pub const VIRTUAL_DESKTOP_MAX: u32 = 2;
}

/// 本 crate 的 registry 派发状态：不消费任何协议事件
/// （窗口状态推送走 event_monitor.js 长驻脚本，§7.2「事件订阅」行）。
#[derive(Debug)]
pub struct KWinWaylandState;

macro_rules! inert_dispatch {
    ($ty:ty) => {
        impl Dispatch<$ty, ()> for KWinWaylandState {
            fn event(
                _: &mut Self,
                _: &$ty,
                _: <$ty as wayland_client::Proxy>::Event,
                _: &(),
                _: &Connection,
                _: &QueueHandle<Self>,
            ) {
            }
        }
    };
}

inert_dispatch!(OrgKdePlasmaWindowManagement);
inert_dispatch!(OrgKdePlasmaWindow);
inert_dispatch!(OrgKdePlasmaStackingOrder);
inert_dispatch!(OrgKdeKwinFakeInput);
inert_dispatch!(OrgKdePlasmaVirtualDesktopManagement);
inert_dispatch!(OrgKdePlasmaVirtualDesktop);

impl Dispatch<WlRegistry, wayland_client::globals::GlobalListContents> for KWinWaylandState {
    fn event(
        _: &mut Self,
        _: &WlRegistry,
        _: <WlRegistry as wayland_client::Proxy>::Event,
        _: &wayland_client::globals::GlobalListContents,
        _: &Connection,
        _: &QueueHandle<Self>,
    ) {
    }
}

/// org_kde_* 私有协议绑定集合（§7.6）。
///
/// 全字段可选：`None` 表示该通道不可用，操作按矩阵回退 Scripting。
#[derive(Debug)]
pub struct KWinProtocols {
    /// 私有协议派发队列（probe 时创建，组件生命周期内保活）。
    ///
    /// 共享的 `wl_display` 由 `KWinCompositor` 的 `WaylandCompositor`
    /// 基类通道持有（`wayland_core` 字段）——本结构只叠加 org_kde_*
    /// 绑定，不拥有连接。
    queue: std::sync::Mutex<wayland_client::EventQueue<KWinWaylandState>>,
    /// 窗口管理（**仅一个客户端可绑定**——短绑失败即回退 Scripting，D8）。
    pub window_mgmt: Option<WindowManagement>,
    /// 假输入注入（authenticate 后可用）。
    pub fake_input: Option<FakeInput>,
    /// 虚拟桌面管理。
    pub vd_mgmt: Option<VirtualDesktopManagement>,
    /// 绑定失败明细（doctor 报告 §7.7 验证输出）。
    bind_failures: Vec<(&'static str, String)>,
}

impl KWinProtocols {
    /// 在既有 WaylandDisplayServer 连接上探测并绑定全部 org_kde_* globals。
    /// 不返回错误：单协议失败记录进 `bind_failures` 并置对应字段 `None`
    /// （回退语义），只有 registry 初始化本身失败才报错。
    ///
    /// 借用基类通道而非持有 `Arc`——连接生命周期归
    /// `KWinCompositor::wayland_core`（WaylandCompositor 基类字段）所有。
    pub fn probe(wl: &WaylandDisplayServer) -> Result<Self> {
        let globals = wl.globals();

        // 独立派发队列：私有协议的事件本层不消费；EventQueue 由 probe
        // 持有到函数结束，绑定请求随队列建立发出（bind 失败即记录回退）。
        let queue = make_queue(wl.connection());
        let queue_handle = queue.handle();

        let mut bind_failures: Vec<(&'static str, String)> = Vec::new();

        let window_mgmt = WindowManagement::bind(globals, &queue_handle, &mut bind_failures);
        let fake_input = FakeInput::bind(globals, &queue_handle, &mut bind_failures);
        let vd_mgmt = VirtualDesktopManagement::bind(globals, &queue_handle, &mut bind_failures);

        Ok(Self {
            queue: std::sync::Mutex::new(queue),
            window_mgmt,
            fake_input,
            vd_mgmt,
            bind_failures,
        })
    }

    /// 私有协议队列句柄（请求发送用；QueueHandle 为轻量克隆）。
    pub fn queue_handle(&self) -> QueueHandle<KWinWaylandState> {
        self.queue.lock().expect("protocol queue poisoned").handle()
    }

    /// 冲刷请求队列（发出 set_state/close 等"发完即忘"的请求后调用）。
    pub fn flush_queue(&self) -> Result<()> {
        self.queue
            .lock()
            .expect("protocol queue poisoned")
            .roundtrip(&mut KWinWaylandState)
            .map_err(|e| KWinError::Scripting(format!("protocol roundtrip: {e}")))?;
        Ok(())
    }

    /// 绑定失败明细（doctor 报告）。
    pub fn bind_failures(&self) -> &[(&'static str, String)] {
        &self.bind_failures
    }

    /// 已绑定协议计数（doctor 报告「3/5 globals bound」）。
    pub fn bound_count(&self) -> usize {
        usize::from(self.window_mgmt.is_some())
            + usize::from(self.fake_input.is_some())
            + usize::from(self.vd_mgmt.is_some())
    }
}

/// 在共享连接上创建派发队列（私有协议事件本层不消费）。
fn make_queue(conn: &Connection) -> wayland_client::EventQueue<KWinWaylandState> {
    conn.new_event_queue()
}

/// 绑定失败原因分类（doctor 报告与诊断日志用）。
///
/// KWin ≤6.7.x 的 `interfacesBlackList` 过滤（见模块文档）会让未声明
/// `X-KDE-Wayland-Interfaces` 的客户端看到 `NotPresent`——这与「协议根本
/// 不存在」同形不同因，必须区分，否则会把权限过滤误诊为版本/环境缺失。
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum BindFailureKind {
    /// registry 中无此 global：≤6.7.x 下最常见为黑名单过滤（缺 .desktop 声明）。
    NotAdvertised,
    /// global 存在但公布版本低于运行时最低要求（`min_versions`）。
    VersionTooLow,
}

impl BindFailureKind {
    /// 从 wayland-client 的 [`BindError`] 归类失败原因。
    fn of(e: wayland_client::globals::BindError) -> Self {
        use wayland_client::globals::BindError as E;
        match e {
            E::NotPresent => Self::NotAdvertised,
            E::UnsupportedVersion => Self::VersionTooLow,
        }
    }

    /// 人类可读描述（含对策提示）。
    pub fn describe(&self, interface: &str) -> String {
        match self {
            Self::NotAdvertised => format!(
                "{interface}: global not advertised — KWin ≤6.7 hides blacklisted \
                 interfaces (window_management/fake_input) from clients without an \
                 X-KDE-Wayland-Interfaces .desktop entry; not a missing protocol"
            ),
            Self::VersionTooLow => format!("{interface}: advertised version below runtime minimum"),
        }
    }
}

/// window_management 协议句柄（v12+；uuid 寻址窗口）。
#[derive(Debug)]
pub struct WindowManagement {
    manager: OrgKdePlasmaWindowManagement,
    /// 公布版本（决定可用请求集）。
    pub advertised_version: u32,
}

impl WindowManagement {
    fn bind(
        globals: &GlobalList,
        qh: &QueueHandle<KWinWaylandState>,
        failures: &mut Vec<(&'static str, String)>,
    ) -> Option<Self> {
        match globals.bind::<OrgKdePlasmaWindowManagement, KWinWaylandState, _>(
            qh,
            min_versions::WINDOW_MANAGEMENT_MIN..=min_versions::WINDOW_MANAGEMENT_MAX,
            (),
        ) {
            Ok(manager) => {
                let advertised_version = manager.version();
                Some(Self {
                    manager,
                    advertised_version,
                })
            }
            Err(e) => {
                // 单客户端被占用（任务栏已绑）是最常见路径——降级而非报错（D8）。
                // NotPresent 在 ≤6.7.x 多为黑名单过滤而非协议缺失，分类记录。
                let kind = BindFailureKind::of(e);
                tracing::info!(
                    interface = "org_kde_plasma_window_management",
                    kind = ?kind,
                    "window management not bound, falling back to Scripting"
                );
                failures.push((
                    "org_kde_plasma_window_management",
                    kind.describe("org_kde_plasma_window_management"),
                ));
                None
            }
        }
    }

    /// 按 uuid 取窗口对象（协议 v12+）。
    ///
    /// new_id 请求直接返回代理对象；对象有效性由后续事件/请求体现，
    /// uuid 不存在时 compositor 静默忽略——上层以 Scripting 查询兜底。
    pub fn get_window_by_uuid(
        &self,
        qh: &QueueHandle<KWinWaylandState>,
        uuid: &str,
    ) -> OrgKdePlasmaWindow {
        self.manager.get_window_by_uuid(uuid.to_string(), qh, ())
    }

    /// 聚焦窗口：activate = set_state(active) 位。
    ///
    /// 注意：state 请求是「位域赋值」而非 toggle——flags 标明要改哪些位，
    /// state 给出这些位的目标值。聚焦 = 只动 active 位且置 1。
    pub fn activate(&self, window: &OrgKdePlasmaWindow) {
        window.set_state(PwmState::Active.into(), PwmState::Active.into());
    }

    /// 最小化(true)/还原(false)。
    pub fn set_minimized(&self, window: &OrgKdePlasmaWindow, minimized: bool) {
        if minimized {
            window.set_state(PwmState::Minimized.into(), PwmState::Minimized.into());
        } else {
            window.set_state(PwmState::Minimized.into(), 0u32);
        }
    }

    /// 关闭窗口。
    pub fn close(&self, window: &OrgKdePlasmaWindow) {
        window.close();
    }

    /// 显示桌面(true)/还原(false)。
    pub fn show_desktop(&self, enabled: bool) {
        self.manager.show_desktop(if enabled {
            ShowDesktop::Enabled as u32
        } else {
            ShowDesktop::Disabled as u32
        });
    }

    /// 读取 stacking order（协议 v17+；旧版本返回 None 由上层回退 Scripting）。
    pub fn get_stacking_order_uuids(
        &self,
        qh: &QueueHandle<KWinWaylandState>,
    ) -> Option<StackingOrderReader> {
        if self.advertised_version < 17 {
            return None;
        }
        Some(StackingOrderReader {
            order: self.manager.get_stacking_order(qh, ()),
            uuids: Vec::new(),
            done: false,
        })
    }
}

/// stacking order 对象的一次性读取器（v17+：Window{uuid} * N → Done）。
///
/// 事件在 roundtrip 时填充 `uuids`；调用方持有 reader 直到 `done()`。
#[derive(Debug)]
pub struct StackingOrderReader {
    #[allow(dead_code)]
    order: OrgKdePlasmaStackingOrder,
    uuids: Vec<String>,
    done: bool,
}

impl StackingOrderReader {
    /// 已收集的窗口 uuid 列表（栈底 → 栈顶）。
    pub fn uuids(&self) -> &[String] {
        &self.uuids
    }

    /// 服务器是否已发完列表。
    pub fn done(&self) -> bool {
        self.done
    }
}

/// fake_input 句柄：构造后必须先 [`FakeInput::authenticate`] 再注入。
#[derive(Debug)]
pub struct FakeInput {
    input: OrgKdeKwinFakeInput,
    authenticated: std::sync::atomic::AtomicBool,
}

impl FakeInput {
    fn bind(
        globals: &GlobalList,
        qh: &QueueHandle<KWinWaylandState>,
        failures: &mut Vec<(&'static str, String)>,
    ) -> Option<Self> {
        match globals.bind::<OrgKdeKwinFakeInput, KWinWaylandState, _>(
            qh,
            min_versions::FAKE_INPUT_MIN..=min_versions::FAKE_INPUT_MAX,
            (),
        ) {
            Ok(input) => Some(Self {
                input,
                authenticated: std::sync::atomic::AtomicBool::new(false),
            }),
            Err(e) => {
                // ≤6.7.x 黑名单同样过滤 fake_input——NotPresent ≠ 协议缺失。
                let kind = BindFailureKind::of(e);
                tracing::info!(interface = "org_kde_kwin_fake_input", kind = ?kind, "fake_input not bound");
                failures.push((
                    "org_kde_kwin_fake_input",
                    kind.describe("org_kde_kwin_fake_input"),
                ));
                None
            }
        }
    }

    /// 向 compositor 声明注入用途（协议要求 authenticate 先于任何注入请求）。
    pub fn authenticate(&self, reason: &str) {
        self.input
            .authenticate("agent-shell".to_string(), reason.to_string());
        self.authenticated
            .store(true, std::sync::atomic::Ordering::Relaxed);
    }

    /// 是否已 authenticate。
    pub fn is_authenticated(&self) -> bool {
        self.authenticated
            .load(std::sync::atomic::Ordering::Relaxed)
    }

    fn ensure_authenticated(&self) -> Result<()> {
        if self
            .authenticated
            .load(std::sync::atomic::Ordering::Relaxed)
        {
            Ok(())
        } else {
            Err(KWinError::FakeInputNotAuthenticated.into())
        }
    }

    /// 相对指针移动。
    pub fn pointer_motion(&self, dx: f64, dy: f64) -> Result<()> {
        self.ensure_authenticated()?;
        self.input.pointer_motion(dx, dy);
        Ok(())
    }

    /// 绝对指针移动（屏幕坐标）。
    pub fn pointer_move_absolute(&self, x: f64, y: f64) -> Result<()> {
        self.ensure_authenticated()?;
        self.input.pointer_motion_absolute(x, y);
        Ok(())
    }

    /// 指针按键（button 为 linux/input-event-codes.h 编码，state 1=按下 0=释放）。
    pub fn button(&self, button: u32, state: u32) -> Result<()> {
        self.ensure_authenticated()?;
        self.input.button(button, state);
        Ok(())
    }

    /// 键盘按键（key 为 linux keycode，state 1=按下 0=释放）。
    pub fn keyboard_key(&self, key: u32, state: u32) -> Result<()> {
        self.ensure_authenticated()?;
        self.input.keyboard_key(key, state);
        Ok(())
    }
}

/// 虚拟桌面管理句柄（v1+ 基础能力）。
#[derive(Debug)]
pub struct VirtualDesktopManagement {
    manager: OrgKdePlasmaVirtualDesktopManagement,
}

impl VirtualDesktopManagement {
    fn bind(
        globals: &GlobalList,
        qh: &QueueHandle<KWinWaylandState>,
        failures: &mut Vec<(&'static str, String)>,
    ) -> Option<Self> {
        match globals.bind::<OrgKdePlasmaVirtualDesktopManagement, KWinWaylandState, _>(
            qh,
            min_versions::VIRTUAL_DESKTOP_MIN..=min_versions::VIRTUAL_DESKTOP_MAX,
            (),
        ) {
            Ok(manager) => Some(Self { manager }),
            Err(e) => {
                // 该协议从不在 KWin 黑名单中——NotPresent 在此即真缺失。
                let kind = BindFailureKind::of(e);
                tracing::info!(interface = "org_kde_plasma_virtual_desktop_management", kind = ?kind, "virtual desktop management not bound");
                failures.push((
                    "org_kde_plasma_virtual_desktop_management",
                    kind.describe("org_kde_plasma_virtual_desktop_management"),
                ));
                None
            }
        }
    }

    /// 按 id 取虚拟桌面对象。
    pub fn get_virtual_desktop(
        &self,
        qh: &QueueHandle<KWinWaylandState>,
        id: &str,
    ) -> OrgKdePlasmaVirtualDesktop {
        self.manager.get_virtual_desktop(id.to_string(), qh, ())
    }

    /// 创建虚拟桌面（position 为插入序号）。
    pub fn create_virtual_desktop(&self, name: &str, position: u32) {
        self.manager
            .request_create_virtual_desktop(name.to_string(), position);
    }

    /// 删除虚拟桌面。
    pub fn remove_virtual_desktop(&self, id: &str) {
        self.manager.request_remove_virtual_desktop(id.to_string());
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use wayland_client::globals::BindError;

    #[test]
    fn not_present_maps_to_not_advertised() {
        // KWin ≤6.7.x 黑名单过滤与真缺失同形——都走 BindError::NotPresent。
        assert_eq!(
            BindFailureKind::of(BindError::NotPresent),
            BindFailureKind::NotAdvertised
        );
    }

    #[test]
    fn unsupported_version_maps_to_version_too_low() {
        assert_eq!(
            BindFailureKind::of(BindError::UnsupportedVersion),
            BindFailureKind::VersionTooLow
        );
    }

    #[test]
    fn not_advertised_description_names_the_permission_filter() {
        let s = BindFailureKind::NotAdvertised.describe("org_kde_plasma_window_management");
        assert!(s.contains("org_kde_plasma_window_management"));
        assert!(s.contains("not advertised"), "got: {s}");
        assert!(
            s.contains("X-KDE-Wayland-Interfaces"),
            "diagnosis must point at the .desktop entitlement, got: {s}"
        );
    }

    #[test]
    fn version_too_low_description_does_not_mention_entitlement() {
        let s = BindFailureKind::VersionTooLow.describe("org_kde_kwin_fake_input");
        assert!(s.contains("below runtime minimum"), "got: {s}");
        assert!(!s.contains(".desktop"), "got: {s}");
    }
}
