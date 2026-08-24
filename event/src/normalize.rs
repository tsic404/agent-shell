//! 归一化 — 各后端原始事件 → 统一 [`DesktopEvent`]（§18.3）。
//!
//! 各 DE 的 open/close/focus 语义不同但语义一致：`RawEvent::KWinWindowAdded`
//! / `HyprlandOpenWindow` / `DdeWindowOpened` 统一为 `DesktopEvent::WindowOpened`。
//! 无法识别的原始事件 → `Noop`（丢弃）。
//!
//! 窗口信息解析（§18.3 `resolve_window_info_by_id`）由调用方注入
//! [`Resolver`]——event crate 协议无关，不直接持有合成器连接。

use std::sync::Arc;

use agent_shell_core::types::Rect;

use crate::adapter::{MoveMerger, RawEvent};
use crate::{DesktopEvent, EventSource};

/// 原始窗口 id → 完整 [`WindowInfo`](agent_shell_core::types::WindowInfo) 解析器。
///
/// 由装配层提供（通常包装 `CompositorComponent::get_window_info`）；
/// 返回 `None` 表示窗口已消失，事件降级丢弃。
pub type WindowResolver = Arc<
    dyn Fn(&str) -> futures::future::BoxFuture<'static, Option<agent_shell_core::types::WindowInfo>>
        + Send
        + Sync,
>;

/// 归一化一个原始事件。
///
/// - 需要完整 `WindowInfo` 的事件走 `resolve`（异步解析；未提供或解析失败
///   时降级为 `Noop`）；
/// - 输入事件按 §18.3「点击某窗口 = 聚焦该窗口」映射；
/// - 同窗口 100ms 内连续移动经 `merger` 合并，仅最终位置发布。
pub fn normalize(
    source_name: &'static str,
    source: EventSource,
    raw: RawEvent,
    merger: &mut MoveMerger,
) -> Option<DesktopEvent> {
    // source_name 接入 tracing（审查建议 10）：无事件时零开销，发布时带源关联。
    tracing::trace!(source = source_name, ?raw, "normalizing raw event");
    // occurred_at 取归一化时刻而非事件发生时刻（审查建议 6）：各 DE 原始
    // 事件流均不携带原始时间戳，上游积压时时间戳会系统性偏移——已知限制，
    // 待协议层提供原始时间后替换。
    let now = std::time::Instant::now();
    let evt = match raw {
        // 窗口打开/聚焦需要完整 WindowInfo → 走 normalize_with_resolver；
        // 同步路径（无 resolver）直接丢弃。
        RawEvent::KWinWindowAdded { .. }
        | RawEvent::HyprlandOpenWindow { .. }
        | RawEvent::DdeWindowOpened { .. }
        | RawEvent::KWinActiveWindowChanged { .. }
        | RawEvent::HyprlandActiveWindow { .. }
        | RawEvent::SwayWindowFocus { .. } => {
            return None;
        }
        RawEvent::KWinWindowRemoved { id }
        | RawEvent::HyprlandCloseWindow { address: id }
        | RawEvent::SwayWindowClose { id } => DesktopEvent::WindowClosed {
            id: win_id(&id),
            source,
            occurred_at: now,
        },

        // ── 窗口移动 ──
        // HyprlandMoveWindow 原始事件不携带坐标（审查问题 2）：此处仅记入
        // merger 的 pending，由 EventNormalizer 的 flush 路径经 resolver 查询
        // 真实几何后以最终位置发布；无 resolver 时不发布零值几何事件。
        RawEvent::HyprlandMoveWindow { address } => {
            let _expired = merger.observe(&address, Rect::default());
            return None;
        }

        // ── 输入 → 窗口聚焦（点击 = 聚焦，§18.3） ──
        // 设计偏差标注：PointerButton 的 window_id/button 解析依赖
        // window_at(x,y) 命中测试，本期 RawEvent 未携带该信息，故恒为
        // None/Left；待 Input 组件提供命中查询后接入。
        RawEvent::PointerButtonPressed { x, y } => DesktopEvent::PointerButton {
            window_id: None,
            button: agent_shell_core::types::MouseButton::Left,
            pressed: true,
            position: (x, y),
            source,
            occurred_at: now,
        },
        RawEvent::PointerButtonReleased { x, y } => DesktopEvent::PointerButton {
            window_id: None,
            button: agent_shell_core::types::MouseButton::Left,
            pressed: false,
            position: (x, y),
            source,
            occurred_at: now,
        },

        // ── 电源 / AT-SPI ──
        RawEvent::PowerStateChanged(state) => DesktopEvent::PowerStateChanged {
            state,
            source,
            occurred_at: now,
        },
        RawEvent::AtSpiAppLaunched { app_id, pid } => DesktopEvent::AppLaunched {
            app_id,
            pid,
            desktop_file: None,
            source,
            occurred_at: now,
        },
        RawEvent::AtSpiAppExited { pid } => DesktopEvent::AppExited {
            pid,
            source,
            occurred_at: now,
        },
    };
    Some(evt)
}

/// 带解析器的归一化入口：需要 `WindowInfo` 的原始事件先解析再映射。
///
/// 解析失败（窗口已消失/无解析器）→ `None`，事件丢弃。
pub async fn normalize_with_resolver(
    source_name: &'static str,
    source: EventSource,
    raw: RawEvent,
    merger: &mut MoveMerger,
    resolve: &Option<WindowResolver>,
) -> Option<DesktopEvent> {
    let needs_resolve = matches!(
        raw,
        RawEvent::KWinWindowAdded { .. }
            | RawEvent::HyprlandOpenWindow { .. }
            | RawEvent::DdeWindowOpened { .. }
            | RawEvent::KWinActiveWindowChanged { id: Some(_) }
            | RawEvent::HyprlandActiveWindow { .. }
            | RawEvent::SwayWindowFocus { .. }
    );
    if !needs_resolve {
        return normalize(source_name, source, raw, merger);
    }
    let resolve = resolve.as_ref()?;
    let id = match &raw {
        RawEvent::KWinWindowAdded { id } | RawEvent::DdeWindowOpened { id } => id.clone(),
        RawEvent::HyprlandOpenWindow { address }
        | RawEvent::HyprlandActiveWindow { address }
        | RawEvent::SwayWindowFocus { id: address }
        | RawEvent::KWinActiveWindowChanged { id: Some(address) } => address.clone(),
        _ => unreachable!("needs_resolve guarantees a resolving variant"),
    };
    let info = resolve(&id).await?;
    let now = std::time::Instant::now();
    let evt = match raw {
        RawEvent::KWinWindowAdded { .. }
        | RawEvent::HyprlandOpenWindow { .. }
        | RawEvent::DdeWindowOpened { .. } => DesktopEvent::WindowOpened {
            info,
            source,
            occurred_at: now,
        },
        _ => DesktopEvent::WindowFocused {
            info,
            source,
            occurred_at: now,
        },
    };
    Some(evt)
}

/// 由原生 id 构造 [`WindowId`](agent_shell_core::types::WindowId)。
///
/// DE 类型未知（协议层尚未判定），以 `Unknown` 占位（审查建议 5）：
/// `WindowId` 的跨 DE 去重键在此路径退化为「原生 id 单维度」，仅在同一
/// backend 会话内唯一；resolver 路径产出的 open/focus/move 事件携带真实
/// DE 标识，不受影响。
fn win_id(native: &str) -> agent_shell_core::types::WindowId {
    agent_shell_core::types::WindowId {
        native_id: native.to_string(),
        de_type: agent_shell_core::types::DesktopEnvironment::Unknown,
    }
}
