//! Treeland Wayland 协议客户端（设计文档 §10.3 / §10.4 `treeland.rs`）。
//!
//! deepin 新一代合成器 **Treeland**（wlroots 系，deepin 25+ 过渡）提供
//! treeland_* 私有协议族。当前 deepin 25 的 deepin-kwin **未启用** Treeland
//! （走 KWin 通道）；本模块按设计决策 D9 为未来迁移预留基础通道——
//! 绑定在共享的 [`WaylandDisplayServer`] 连接上，与 org_kde_* 通道互斥
//! 探测、不冲突共存。
//!
//! 协议状态：上游 EXPERIMENTAL——接口可能在不升 major 的情况下变更，
//! XML vendored 自 linuxdeepin/treeland-protocols 并随上游跟踪更新。
//!
//! 窗口操作语义：treeland_foreign_toplevel_handle_v1 的请求全部为
//! 「发完即忘」（wayland 请求无回执），与 KWin 协议通道同款乐观发送；
//! 状态回读依赖 toplevel state/done 事件聚合（T3b 事件任务统一落地，
//! 本层 inert 不消费事件）。

use std::collections::HashMap;

use wayland_client::protocol::wl_registry::WlRegistry;
use wayland_client::protocol::wl_seat::WlSeat;
use wayland_client::{Connection, Dispatch, QueueHandle};

use agent_shell_core::error::{AgentShellError, Result};
use agent_shell_displayserver_wayland::WaylandDisplayServer;

use crate::protocol_gen::client::treeland_foreign_toplevel_handle_v1::TreelandForeignToplevelHandleV1;
use crate::protocol_gen::client::treeland_foreign_toplevel_manager_v1::TreelandForeignToplevelManagerV1;
use crate::protocol_gen::client::treeland_window_management_v1::TreelandWindowManagementV1;
use crate::protocol_gen::client::{
    treeland_foreign_toplevel_handle_v1, treeland_window_management_v1,
};

/// 本模块的 registry/协议派发状态：不消费任何协议事件（窗口状态推送
/// 归 T3b 事件任务；桌面状态变化同理）。
#[derive(Debug)]
pub struct TreelandState;

macro_rules! inert_dispatch {
    ($ty:ty) => {
        impl Dispatch<$ty, ()> for TreelandState {
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

inert_dispatch!(TreelandForeignToplevelManagerV1);
inert_dispatch!(TreelandForeignToplevelHandleV1);
inert_dispatch!(TreelandWindowManagementV1);

impl Dispatch<WlRegistry, wayland_client::globals::GlobalListContents> for TreelandState {
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

/// treeland_foreign_toplevel_handle_v1 State 枚举的语义重导出别名。
pub use treeland_foreign_toplevel_handle_v1::State as ToplevelState;

/// Treeland 协议绑定集合（§10.3 表中当前可生成绑定的两个通道）。
///
/// 全字段可选：绑定失败记入 [`bind_failures`](Self::bind_failures) 并置
/// `None`（回退语义，与 KWinProtocols 同款）。
#[derive(Debug)]
pub struct TreelandBindings {
    /// 私有协议派发队列（probe 时创建，组件生命周期内保活）。
    ///
    /// Option 仅服务测试构造（doctor_line 不读队列）；生产路径恒 `Some`，
    /// 访问器在 `None` 时 panic——那是测试专用状态的误用。
    queue: std::sync::Mutex<Option<wayland_client::EventQueue<TreelandState>>>,
    /// 外部窗口管理（完整窗口操作：activate/close/maximize/minimize/fullscreen）。
    pub foreign_toplevel: Option<TreelandForeignToplevelManagerV1>,
    /// 桌面状态控制（normal/show/preview_show）。
    pub window_management: Option<TreelandWindowManagementV1>,
    /// 绑定失败明细（doctor 报告）。
    bind_failures: Vec<(&'static str, String)>,
}

impl TreelandBindings {
    /// 测试专用构造（doctor_line 格式测试用）：跳过 Wayland 探测，
    /// 直接以 bind_failures 组装（协议代理位恒 None——见函数体说明）。
    #[cfg(test)]
    fn for_doctor_test(failures: Vec<(&'static str, String)>) -> Self {
        // 协议代理对象无法脱离真实 wl_display 构造，绑定状态位与队列
        // 恒 None；doctor_line 只读 bound_count()（布尔加法）与
        // bind_failures，测试态覆盖 0-bound 渲染路径。
        Self {
            queue: std::sync::Mutex::new(None),
            foreign_toplevel: None,
            window_management: None,
            bind_failures: failures,
        }
    }

    /// 失败才报错（由调用方经 [`WaylandDisplayServer`] 构造路径承担）。
    pub fn probe(wl: &WaylandDisplayServer) -> Result<Self> {
        let globals = wl.globals();
        let mut queue = wl.connection().new_event_queue::<TreelandState>();
        let qh = &queue.handle();

        let mut bind_failures: Vec<(&'static str, String)> = Vec::new();

        // 上游接口版本 v2/v1——区间写死为规范版本，越界会让 GlobalList::bind
        // 在取 min 前断言失败 panic（wayland-client 0.31 globals.rs:167，
        // 与 kwin/wlr 通道同款约束）。公布版本低于下界时 bind 报错 → 记录回退。
        let foreign_toplevel =
            match globals.bind::<TreelandForeignToplevelManagerV1, _, _>(qh, 1..=2, ()) {
                Ok(m) => Some(m),
                Err(e) => {
                    tracing::info!("treeland foreign-toplevel not bound: {e}");
                    bind_failures.push(("treeland_foreign_toplevel_manager_v1", e.to_string()));
                    None
                }
            };
        let window_management =
            match globals.bind::<TreelandWindowManagementV1, _, _>(qh, 1..=1, ()) {
                Ok(m) => Some(m),
                Err(e) => {
                    tracing::info!("treeland window-management not bound: {e}");
                    bind_failures.push(("treeland_window_management_v1", e.to_string()));
                    None
                }
            };

        // 冲刷初始 bind 序列并等待 compositor 确认（错误对象在此浮现）。
        queue
            .roundtrip(&mut TreelandState)
            .map_err(|e| AgentShellError::BackendUnavailable(format!("treeland roundtrip: {e}")))?;

        Ok(Self {
            queue: std::sync::Mutex::new(Some(queue)),
            foreign_toplevel,
            window_management,
            bind_failures,
        })
    }

    /// 协议队列句柄（请求发送用）。
    pub fn queue_handle(&self) -> QueueHandle<TreelandState> {
        self.queue
            .lock()
            .expect("treeland queue poisoned")
            .as_ref()
            .expect("queue absent: for_doctor_test state used outside doctor_line")
            .handle()
    }

    /// 冲刷新发出的请求到 compositor。
    pub fn flush_queue(&self) -> Result<()> {
        let mut guard = self.queue.lock().expect("treeland queue poisoned");
        let queue = guard
            .as_mut()
            .expect("queue absent: for_doctor_test state used outside doctor_line");
        queue
            .roundtrip(&mut TreelandState)
            .map_err(|e| AgentShellError::BackendUnavailable(format!("treeland roundtrip: {e}")))?;
        Ok(())
    }

    /// 绑定失败明细（doctor 报告）。
    pub fn bind_failures(&self) -> &[(&'static str, String)] {
        &self.bind_failures
    }

    /// 已绑定协议计数（doctor「N/2」）。
    pub fn bound_count(&self) -> usize {
        usize::from(self.foreign_toplevel.is_some()) + usize::from(self.window_management.is_some())
    }
}

/// 窗口操作集（对指定 toplevel handle 发「乐观发送」请求）。
pub struct ToplevelOps<'a> {
    handle: &'a TreelandForeignToplevelHandleV1,
    seat: WlSeat,
}

impl<'a> ToplevelOps<'a> {
    /// 由既有 handle + seat 构造（事件聚合层或装配方提供）。
    pub fn new(handle: &'a TreelandForeignToplevelHandleV1, seat: WlSeat) -> Self {
        Self { handle, seat }
    }

    /// 聚焦窗口（activate(seat)）。
    pub fn activate(&self) {
        self.handle.activate(&self.seat);
    }

    /// 关闭窗口。
    pub fn close(&self) {
        self.handle.close();
    }

    /// 最大化(true)/还原(false)。
    pub fn set_maximized(&self, maximized: bool) {
        if maximized {
            self.handle.set_maximized();
        } else {
            self.handle.unset_maximized();
        }
    }

    /// 最小化(true)/还原(false)。
    pub fn set_minimized(&self, minimized: bool) {
        if minimized {
            self.handle.set_minimized();
        } else {
            self.handle.unset_minimized();
        }
    }

    /// 全屏(true)/还原(false)；output 传 None 由 compositor 决定。
    pub fn set_fullscreen(
        &self,
        fullscreen: bool,
        output: Option<&wayland_client::protocol::wl_output::WlOutput>,
    ) {
        if fullscreen {
            self.handle.set_fullscreen(output);
        } else {
            self.handle.unset_fullscreen();
        }
    }
}

/// treeland_window_management_v1 句柄：桌面状态控制。
#[derive(Debug)]
pub struct DesktopStateControl {
    wm: TreelandWindowManagementV1,
}

/// 桌面状态（协议 enum desktop_state）。
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum DesktopState {
    /// 正常。
    Normal,
    /// 显示桌面。
    Show,
    /// 预览显示。
    PreviewShow,
}

impl DesktopStateControl {
    /// 设置桌面状态（set_desktop(u)——「发完即忘」请求，无回执）。
    pub fn set_desktop(&self, _qh: &QueueHandle<TreelandState>, state: DesktopState) {
        self.wm.set_desktop(state.into());
    }
}

impl From<DesktopState> for u32 {
    fn from(s: DesktopState) -> u32 {
        // 与 XML enum desktop_state 一致：normal=0, show=1, preview_show=2。
        use treeland_window_management_v1::DesktopState as E;
        (match s {
            DesktopState::Normal => E::Normal,
            DesktopState::Show => E::Show,
            DesktopState::PreviewShow => E::PreviewShow,
        }) as u32
    }
}

/// doctor 输出：treeland 通道摘要行。
pub fn doctor_line(bindings: &TreelandBindings) -> String {
    format!(
        "{} Treeland 协议 : {}/2 globals bound{}",
        if bindings.foreign_toplevel.is_some() && bindings.window_management.is_some() {
            "✓"
        } else if bindings.bound_count() > 0 {
            "⚠"
        } else {
            "✗"
        },
        bindings.bound_count(),
        if bindings.bind_failures.is_empty() {
            String::new()
        } else {
            format!(
                " ({})",
                bindings
                    .bind_failures
                    .iter()
                    .map(|(n, e)| format!("{n}: {e}"))
                    .collect::<Vec<_>>()
                    .join("; ")
            )
        }
    )
}

/// 便捷构造：identifier → handle 映射占位类型（T3b 事件聚合落地前的
/// 显式未完成标记，防止调用方误以为映射已可用）。
#[derive(Debug, Default)]
pub struct ToplevelRegistry {
    #[allow(dead_code)]
    by_identifier: HashMap<u32, TreelandForeignToplevelHandleV1>,
}

impl ToplevelRegistry {
    /// T3b 前显式 NotImplemented（事件聚合未落地，映射恒空）。
    pub fn lookup(&self, identifier: u32) -> Result<&TreelandForeignToplevelHandleV1> {
        let _ = identifier;
        Err(AgentShellError::NotImplemented(
            "treeland toplevel event aggregation lands with TSI-2314 (T3b events)".into(),
        ))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// doctor_line 纯函数的格式测试：协议代理对象无法脱离真实
    /// wl_display 构造，`bound_count()` 恒为 0 的测试态——因此本组
    /// 用例覆盖「0/2 + 各类 bind_failures 文案」的渲染路径；
    /// 「1/2、2/2」分支由 `bound_count()` 的布尔加法保证（一行逻辑，
    /// 由编译器与类型系统背书），不在此重复。
    mod doctor_line_format {
        use super::*;

        #[test]
        fn none_bound_clean_renders_cross() {
            let b = TreelandBindings::for_doctor_test(vec![]);
            let line = doctor_line(&b);
            assert!(line.starts_with("✗"), "got: {line}");
            assert!(line.contains("0/2 globals bound"), "got: {line}");
            assert!(
                !line.contains('('),
                "clean state must not list failures: {line}"
            );
        }

        #[test]
        fn failures_are_listed_in_parens() {
            let b = TreelandBindings::for_doctor_test(vec![
                (
                    "treeland_foreign_toplevel_manager_v1",
                    "global not advertised".to_string(),
                ),
                (
                    "treeland_window_management_v1",
                    "version below minimum".to_string(),
                ),
            ]);
            let line = doctor_line(&b);
            assert!(line.starts_with("✗"), "0 bound stays ✗: {line}");
            assert!(
                line.contains(
                    "(treeland_foreign_toplevel_manager_v1: global not advertised; \
                     treeland_window_management_v1: version below minimum)"
                ),
                "failure details must be preserved in order: {line}"
            );
        }

        #[test]
        fn single_failure_renders_without_separator() {
            let b = TreelandBindings::for_doctor_test(vec![(
                "treeland_window_management_v1",
                "bind failed".to_string(),
            )]);
            let line = doctor_line(&b);
            assert!(line.contains("(treeland_window_management_v1: bind failed)"));
            assert!(
                !line.contains(';'),
                "single failure needs no separator: {line}"
            );
        }
    }
}
