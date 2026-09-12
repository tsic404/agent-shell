//! 事件归一化桥接 — 各后端原始事件流 → 统一 [`DesktopEvent`]。
//!
//! 对应设计文档 §18.2–18.3：每个后端实现 [`EventSource`] 提供原始事件流，
//! [`EventNormalizer`] 为每源 spawn 一个 tokio task，归一化（含 WindowMoved
//! 100ms 合并）后 publish 到 EventHub。

use std::collections::HashMap;
use std::time::{Duration, Instant};

use futures::stream::BoxStream;
use futures::StreamExt;

use crate::hub::EventHub;
use crate::ring::throttle;
use crate::DesktopEvent;

/// 后端原始事件（未归一化，各 DE 语义不同）。
///
/// 归一化映射见 [`normalize::normalize`]; 无法识别的变体归一化为
/// [`DesktopEvent::Noop`] 丢弃。
#[derive(Clone, Debug)]
pub enum RawEvent {
    // KWin Scripting / org_kde_* 协议
    KWinWindowAdded { id: String },
    KWinWindowRemoved { id: String },
    KWinActiveWindowChanged { id: Option<String> },

    // Hyprland .socket2.sock
    HyprlandOpenWindow { address: String },
    HyprlandCloseWindow { address: String },
    HyprlandActiveWindow { address: String },
    HyprlandMoveWindow { address: String },

    // Sway IPC window::/workspace:: 事件
    SwayWindowClose { id: String },
    SwayWindowFocus { id: String },

    // DDE / Treeland treeland_events 通道
    DdeWindowOpened { id: String },

    // 输入（libei）
    PointerButtonPressed { x: i32, y: i32 },
    PointerButtonReleased { x: i32, y: i32 },

    // 电源（UPower/logind）
    PowerStateChanged(agent_shell_core::services::PowerState),

    // AT-SPI 应用启停
    AtSpiAppLaunched { app_id: String, pid: u32 },
    AtSpiAppExited { pid: u32 },
}

/// 事件源适配器（各 backend 的原始事件订阅接口，§18.2）。
///
/// 合成器侧的 [`agent_shell_core::event::EventStream`]（`CompositorComponent::
/// subscribe()` 出口）由具体组件包装为本 trait 的实现。
pub trait RawSource: Send + Sync {
    /// 源名（调试/日志用，如 `"kwin-scripting"`、`"hyprland-socket2"`）。
    fn source_name(&self) -> &'static str;

    /// 该源对应的事件源标识。
    fn source_kind(&self) -> crate::EventSource;

    /// 原始事件流。流结束（`None`）即该源的 task 退出。
    fn events(&self) -> BoxStream<'static, RawEvent>;
}

/// 设计文档 §18.2 中 `EventSource` trait 的别名（crate 内 `EventSource`
/// 名称已被事件源标识枚举占用，与 §18.1 同名）。
pub type SourceAdapter = dyn RawSource;

/// 事件归一化器：为每个源 spawn 一个 tokio task，归一化后 publish + 写入环形缓冲。
///
/// [`EventRing`]（默认 1000 条）由本结构持有并接入发布路径：所有经
/// `run()` 发布的事件自动入 ring，`ring()` 取快照供 `--replay` 与断线补发，
/// 调用方无需手动双写。
pub struct EventNormalizer {
    hub: EventHub,
    sources: Vec<Box<dyn RawSource>>,
    resolve: Option<crate::normalize::WindowResolver>,
    ring: crate::ring::EventRing,
}

impl std::fmt::Debug for EventNormalizer {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("EventNormalizer")
            .field(
                "sources",
                &self
                    .sources
                    .iter()
                    .map(|s| s.source_name())
                    .collect::<Vec<_>>(),
            )
            .finish_non_exhaustive()
    }
}

impl EventNormalizer {
    /// 创建归一化器（环形缓冲容量取 D4 默认 1000）。
    pub fn new(hub: EventHub) -> Self {
        Self {
            hub,
            sources: Vec::new(),
            resolve: None,
            ring: crate::ring::EventRing::default(),
        }
    }

    /// 已集成的环形事件缓冲（最近 1000 条，供 `--replay` / 断线补发）。
    pub fn ring(&self) -> &crate::ring::EventRing {
        &self.ring
    }

    /// 注册窗口信息解析器（§18.3 `resolve_window_info_by_id`）。
    ///
    /// 提供后，需要完整 `WindowInfo` 的原始事件（open/focus）先解析再入事件。
    pub fn with_resolver(mut self, resolve: crate::normalize::WindowResolver) -> Self {
        self.resolve = Some(resolve);
        self
    }

    /// 注入外部环形缓冲（与 daemon 的 `events --replay` 数据源共享）。
    ///
    /// 默认新建 [`EventRing`]；装配层（daemon）需要归一化事件进入其既有
    /// replay 缓冲时注入共享环，避免维护双份 ring 导致 replay 漏掉事件。
    pub fn with_ring(mut self, ring: crate::ring::EventRing) -> Self {
        self.ring = ring;
        self
    }

    /// 注册一个事件源。
    pub fn add_source(&mut self, source: Box<dyn RawSource>) -> &mut Self {
        self.sources.push(source);
        self
    }

    /// 已注册源的数量。
    pub fn source_count(&self) -> usize {
        self.sources.len()
    }

    /// 为每个源 spawn 归一化 task（每源一个），返回已启动源名列表。
    ///
    /// task 循环消费原始事件 → 归一化（含 WindowMoved 100ms 延迟合并）→
    /// publish；流结束或 hub 无订阅者时自然退出。
    pub fn run(self) -> Vec<&'static str> {
        let names: Vec<&'static str> = self.sources.iter().map(|s| s.source_name()).collect();
        let resolve = self.resolve;
        let ring = self.ring.clone();
        for source in self.sources {
            let resolve = resolve.clone();
            let hub = self.hub.clone();
            let ring = ring.clone();
            tokio::spawn(async move {
                let name = source.source_name();
                let kind = source.source_kind();
                let mut merger = MoveMerger::default();
                let mut events = source.events();
                loop {
                    // 先 flush 已过窗口期的 pending 移动（最终几何），再取新事件
                    for (id, geo) in merger.drain_expired() {
                        let evt = move_event(&id, geo, &kind, name, &resolve).await;
                        if let Some(evt) = evt {
                            ring.push(evt.clone());
                            hub.publish(evt).await;
                        }
                    }
                    match events.next().await {
                        Some(raw) => {
                            let evt = if resolve.is_some() {
                                crate::normalize::normalize_with_resolver(
                                    name,
                                    kind.clone(),
                                    raw,
                                    &mut merger,
                                    &resolve,
                                )
                                .await
                            } else {
                                crate::normalize::normalize(name, kind.clone(), raw, &mut merger)
                            };
                            if let Some(evt) = evt {
                                ring.push(evt.clone());
                                hub.publish(evt).await;
                            }
                        }
                        None => {
                            // 流结束：flush 残余 pending 后退出
                            for (id, geo) in std::mem::take(&mut merger.pending) {
                                let evt = move_event(&id, geo.1, &kind, name, &resolve).await;
                                if let Some(evt) = evt {
                                    ring.push(evt.clone());
                                    hub.publish(evt).await;
                                }
                            }
                            break;
                        }
                    }
                }
                tracing::debug!(source = name, "event source stream ended");
            });
        }
        names
    }
}

/// 构造 [`DesktopEvent::WindowMoved`]：move 事件必须携带真实几何。
///
/// 有 resolver 时查询窗口当前几何；无 resolver 或窗口已消失 → 不发布
/// （禁止广播零值几何，见审查问题 2）。
async fn move_event(
    id: &str,
    _pending_geo: agent_shell_core::types::Rect,
    source: &crate::EventSource,
    source_name: &'static str,
    resolve: &Option<crate::normalize::WindowResolver>,
) -> Option<DesktopEvent> {
    let resolve = resolve.as_ref()?;
    let info = resolve(id).await?;
    let _ = source_name; // tracing 关联字段，见 normalize::normalize 同款约定
    Some(DesktopEvent::WindowMoved {
        id: info.id,
        geometry: info.geometry,
        source: source.clone(),
        occurred_at: Instant::now(),
    })
}

/// 跨事件的合并状态：100ms 内同一窗口的连续移动只保留最终几何。
///
/// 语义（对应设计 §18.2「合并 100ms 内连续 WindowMoved，仅推送最终位置」）：
/// 首条移动**不发布**而是记为 pending；窗口内后续移动覆盖 pending；
/// 窗口过期后（或显式 flush）pending 才以**最终几何**发布。
#[derive(Default)]
pub struct MoveMerger {
    pending: HashMap<String, (Instant, agent_shell_core::types::Rect)>,
}

impl MoveMerger {
    /// 记录一次移动。返回 `Some(rect)` 表示此前该窗口有已过期的 pending
    /// 需要先 flush 发布（其最终几何）；当前移动成为新的 pending，暂不发布。
    pub fn observe(
        &mut self,
        id: &str,
        geo: agent_shell_core::types::Rect,
    ) -> Option<agent_shell_core::types::Rect> {
        let now = Instant::now();
        let expired = match self.pending.get(id) {
            Some((t, _)) if now.duration_since(*t) < throttle::WINDOW_MERGE => None,
            Some((_, old_geo)) => Some(*old_geo),
            None => None,
        };
        self.pending.insert(id.to_string(), (now, geo));
        expired
    }

    /// flush 所有已过窗口期的 pending 移动，返回待发布的最终几何列表
    /// （`(window_id, geometry)`）。归一化循环在每次取事件前调用，
    /// 保证风暴平息后最终位置一定送达。
    pub fn drain_expired(&mut self) -> Vec<(String, agent_shell_core::types::Rect)> {
        let now = Instant::now();
        let mut out = Vec::new();
        self.pending.retain(|id, (t, geo)| {
            if now.duration_since(*t) >= throttle::WINDOW_MERGE {
                out.push((id.clone(), *geo));
                false
            } else {
                true
            }
        });
        out
    }

    /// 清理过期条目（与 [`MoveMerger::drain_expired`] 二选一使用）。
    pub fn gc(&mut self) {
        let cutoff = Instant::now() - throttle::WINDOW_MERGE;
        self.pending.retain(|_, (t, _)| *t >= cutoff);
    }

    /// 当前 pending 的窗口数（测试/诊断用）。
    pub fn pending_count(&self) -> usize {
        self.pending.len()
    }
}

/// 便捷重导出：测试与调用方常用 `Duration`。
pub const MOVE_MERGE_WINDOW: Duration = throttle::WINDOW_MERGE;
