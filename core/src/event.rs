//! 事件流系统。
//!
//! 对应设计文档 §18（事件流系统）：事件模型、事件源标识、EventHub（归一化 +
//! fan-out + 背压）。事件归一化适配器（`EventSource` trait）定义于此，具体后端
//! 的事件源实现在各自的组件 crate 中。

use std::collections::HashMap;
use std::time::Instant;

use serde::{Deserialize, Serialize};
use tokio::sync::{mpsc, oneshot};
use uuid::Uuid;

use crate::services::PowerState;
use crate::types::{
    KeyCombo, MouseButton, Rect, WindowId, WindowInfo, WindowState, WorkspaceId, WorkspaceInfo,
};

// ───────────────────────── 事件优先级 ─────────────────────────

/// 事件优先级（决定背压时的丢弃策略）。
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub enum EventPriority {
    /// 高优先级（窗口开关/聚焦——必须送达）
    High,
    /// 中优先级（工作区切换/监视器变化）
    Medium,
    /// 低优先级（音量/DPMS/外观）
    Low,
}

// ───────────────────────── 事件源 ─────────────────────────

/// 事件源标识（用于过滤和调试）。
#[derive(Clone, Debug, PartialEq, Eq, Hash, Serialize, Deserialize)]
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
#[derive(Clone, Debug)]
pub enum DesktopEvent {
    // ========== 窗口事件（High） ==========
    /// 窗口打开
    WindowOpened {
        /// 完整窗口信息
        info: WindowInfo,
        /// 事件源
        source: EventSource,
        /// 事件发生时间
        occurred_at: Instant,
    },
    /// 窗口关闭
    WindowClosed {
        /// 窗口标识
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
        /// 变化后的完整状态集合
        states: Vec<WindowState>,
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
        /// 从顶到底的窗口 ID 列表
        ids: Vec<WindowId>,
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
        monitor: crate::types::MonitorInfo,
        /// true=接入, false=移除
        added: bool,
        source: EventSource,
        occurred_at: Instant,
    },
    /// 监视器分辨率/缩放变化
    MonitorChanged {
        monitor: crate::types::MonitorInfo,
        source: EventSource,
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
        state: PowerState,
        source: EventSource,
        occurred_at: Instant,
    },
    /// 无障碍树变化（仅 AT-SPI 源）
    AccessibilityTreeChanged {
        app_pid: u32,
        change_type: crate::services::AccessibilityChange,
        source: EventSource,
        occurred_at: Instant,
    },
    /// 保底/无操作（归一化丢弃）
    Noop,
}

impl DesktopEvent {
    /// 事件优先级推导。
    ///
    /// 窗口开关/聚焦 → High；状态变化/移动/工作区/监视器/输入 → Medium；其余 → Low。
    pub fn priority(&self) -> EventPriority {
        match self {
            Self::WindowOpened { .. } | Self::WindowClosed { .. } | Self::WindowFocused { .. } => {
                EventPriority::High
            }
            Self::WindowStateChanged { .. }
            | Self::WindowMoved { .. }
            | Self::WorkspaceChanged { .. }
            | Self::MonitorHotplug { .. }
            | Self::MonitorChanged { .. }
            | Self::PointerButton { .. }
            | Self::KeyComboPressed { .. } => EventPriority::Medium,
            _ => EventPriority::Low,
        }
    }

    /// 事件源。
    pub fn source(&self) -> &EventSource {
        match self {
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
            | Self::AccessibilityTreeChanged { source, .. } => source,
            Self::Noop => &EventSource::Portal,
        }
    }
}

// ───────────────────────── 事件过滤 ─────────────────────────

/// 事件过滤器（订阅者按类别筛选）。
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

    /// 判断事件是否匹配过滤器。
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

/// 事件流（异步迭代器抽象）。
///
/// 合成器的 `subscribe()` 返回此 trait 对象，由 EventNormalizer 消费。
#[async_trait::async_trait]
pub trait EventStream: Send + Sync {
    /// 获取下一个事件（None 表示流结束）。
    async fn next_event(&self) -> Option<DesktopEvent>;
}

// ───────────────────────── 订阅句柄 ─────────────────────────

/// 订阅句柄（`EventHub::subscribe` 的返回值）。
pub struct EventSubscription {
    /// 订阅 ID（用于 `EventHub::unsubscribe`）
    pub id: Uuid,
    pub(crate) rx: mpsc::Receiver<DesktopEvent>,
    pub(crate) filter: EventFilter,
}

impl EventSubscription {
    /// 异步接收下一个匹配过滤器的事件。
    ///
    /// 不匹配 `filter` 的事件被静默跳过；hub 关闭或订阅被取消时返回 None。
    pub async fn recv(&mut self) -> Option<DesktopEvent> {
        loop {
            let evt = self.rx.recv().await?;
            if self.filter.matches(&evt) {
                return Some(evt);
            }
        }
    }

    /// 订阅 ID。
    pub fn id(&self) -> Uuid {
        self.id
    }
}

// ───────────────────────── EventHub ─────────────────────────

/// 事件枢纽（所有组件事件归一化后的 fan-out 中心）。
///
/// 对应设计文档 §18.4：有界通道（默认容量 1024），超限时丢弃低优先级事件。
/// `EventHub` 可克隆，所有克隆共享同一组订阅者。
#[derive(Clone)]
pub struct EventHub {
    inner: std::sync::Arc<EventHubInner>,
}

struct EventHubInner {
    subscribers: std::sync::Mutex<HashMap<Uuid, (mpsc::Sender<DesktopEvent>, EventFilter)>>,
    capacity: usize,
}

impl EventHub {
    /// 创建新 EventHub，默认容量 1024。
    pub fn new() -> Self {
        Self::with_capacity(1024)
    }

    /// 创建指定容量的 EventHub。
    pub fn with_capacity(capacity: usize) -> Self {
        Self {
            inner: std::sync::Arc::new(EventHubInner {
                subscribers: std::sync::Mutex::new(HashMap::new()),
                capacity: capacity.max(1),
            }),
        }
    }

    /// 订阅事件流。
    ///
    /// 返回订阅句柄；hub 不做预过滤，`filter` 由订阅者通过
    /// `EventSubscription::recv()` 内部匹配（未匹配的事件被静默跳过）。
    pub fn subscribe(&self, filter: EventFilter) -> EventSubscription {
        let id = Uuid::new_v4();
        let (tx, rx) = mpsc::channel(self.inner.capacity);
        self.inner
            .subscribers
            .lock()
            .expect("subscriber map lock poisoned")
            .insert(id, (tx, filter.clone()));
        EventSubscription { id, rx, filter }
    }

    /// 发布事件到所有订阅者（异步：高优先级事件保证送达）。
    ///
    /// 背压策略（对应设计文档 §18.4「丢弃低优先级事件」）：
    /// - `High` 事件：通道满时等待空闲槽位，保证窗口开关/聚焦等关键事件必达；
    /// - `Medium`/`Low` 事件：通道满时 `try_send` 失败即丢弃，不阻塞发布者。
    ///
    /// 并发契约：锁内仅快照订阅者（clone `(Sender, EventFilter)`），立即释放锁后
    /// 在锁外遍历投递——消费者回调中调用 `unsubscribe` 不会与发布者互等死锁，
    /// 单个慢订阅者也不会阻塞其他订阅者的投递或订阅管理。
    pub async fn publish(&self, event: DesktopEvent) {
        // 快照：锁内 clone，出作用域即释放锁；投递全程无锁。
        let snapshot: Vec<(mpsc::Sender<DesktopEvent>, EventFilter)> = self
            .inner
            .subscribers
            .lock()
            .expect("subscriber map lock poisoned")
            .values()
            .cloned()
            .collect();

        let high = event.priority() == EventPriority::High;
        for (tx, filter) in snapshot {
            // 订阅级预过滤：不匹配该订阅者的类别直接跳过
            if !filter.matches(&event) {
                continue;
            }
            if high {
                // 关键事件必须送达：等通道有空位（订阅者被 drop 时返回 Err，忽略）
                let _ = tx.send(event.clone()).await;
            } else {
                // 低优事件：满则丢弃（背压），不阻塞发布者
                let _ = tx.try_send(event.clone());
            }
        }
    }

    /// 主动取消某个订阅。
    pub fn unsubscribe(&self, id: Uuid) {
        self.inner
            .subscribers
            .lock()
            .expect("subscriber map lock poisoned")
            .remove(&id);
    }

    /// 当前活跃订阅者数量。
    pub fn subscriber_count(&self) -> usize {
        self.inner
            .subscribers
            .lock()
            .expect("subscriber map lock poisoned")
            .len()
    }
}

impl Default for EventHub {
    fn default() -> Self {
        Self::new()
    }
}

impl std::fmt::Debug for EventHub {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("EventHub")
            .field("capacity", &self.inner.capacity)
            .finish_non_exhaustive()
    }
}

/// 事件源适配器（各 backend 的原始事件订阅接口）。
///
/// 实现者将 backend 原始事件流转换为 `DesktopEvent`，供 `EventNormalizer` 消费。
pub trait RawEventSource: Send + Sync {
    /// 事件源名称（用于调试/日志）
    fn source_name(&self) -> &'static str;

    /// 启动事件源，将事件推入给定 channel。
    ///
    /// 实现应 spawn 异步任务持续读取后端事件，归一化后发送到 `tx`。
    /// 返回一个 oneshot receiver，调用 `.send(())` 时停止事件源。
    fn spawn(
        &self,
        tx: mpsc::Sender<DesktopEvent>,
        stop: oneshot::Receiver<()>,
    ) -> std::pin::Pin<Box<dyn std::future::Future<Output = ()> + Send>>;
}

// tests ----------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;
    use crate::types::{DesktopEnvironment, WindowId};

    fn win_id() -> WindowId {
        WindowId {
            native_id: "1".into(),
            de_type: DesktopEnvironment::KDE,
        }
    }

    #[test]
    fn priority_high_for_window_lifecycle() {
        let id = win_id();
        let now = Instant::now();
        let e = DesktopEvent::WindowClosed {
            id,
            source: EventSource::KWinWayland,
            occurred_at: now,
        };
        assert_eq!(e.priority(), EventPriority::High);
        let e = DesktopEvent::WindowOpened {
            info: crate::types::WindowInfo {
                id: win_id(),
                title: "t".into(),
                app_id: "a".into(),
                pid: 1,
                geometry: Rect::default(),
                frame_geometry: Rect::default(),
                states: vec![],
                workspace_id: None,
                monitor_id: None,
                stacking_order: 0,
                desktop_file: None,
                window_type: crate::types::WindowType::Normal,
                icon_geometry: None,
                keep_above: false,
            },
            source: EventSource::KWinWayland,
            occurred_at: now,
        };
        assert_eq!(e.priority(), EventPriority::High);
    }

    #[test]
    fn filter_matches_by_category() {
        let f = EventFilter::windows_only();
        let id = win_id();
        let now = Instant::now();
        let win_evt = DesktopEvent::WindowClosed {
            id: id.clone(),
            source: EventSource::Hyprland,
            occurred_at: now,
        };
        let ws_evt = DesktopEvent::WorkspaceListChanged {
            workspaces: vec![],
            source: EventSource::Sway,
            occurred_at: now,
        };
        assert!(f.matches(&win_evt));
        assert!(!f.matches(&ws_evt));
    }

    #[tokio::test]
    async fn hub_publish_and_subscribe() {
        let hub = EventHub::with_capacity(8);
        let mut sub = hub.subscribe(EventFilter::all());
        let id = win_id();
        hub.publish(DesktopEvent::WindowClosed {
            id,
            source: EventSource::KWinWayland,
            occurred_at: Instant::now(),
        })
        .await;
        let evt = sub.recv().await;
        assert!(matches!(evt, Some(DesktopEvent::WindowClosed { .. })));
    }

    #[tokio::test]
    async fn hub_subscription_filter_skips_mismatched() {
        let hub = EventHub::new();
        let mut sub = hub.subscribe(EventFilter::windows_only());
        hub.publish(DesktopEvent::WorkspaceListChanged {
            workspaces: vec![],
            source: EventSource::Sway,
            occurred_at: Instant::now(),
        })
        .await;
        hub.publish(DesktopEvent::WindowClosed {
            id: win_id(),
            source: EventSource::KWinWayland,
            occurred_at: Instant::now(),
        })
        .await;
        // 工作区事件被过滤，直接收到窗口事件
        let evt = sub.recv().await;
        assert!(matches!(evt, Some(DesktopEvent::WindowClosed { .. })));
    }

    #[tokio::test]
    async fn hub_backpressure_drops_low_keeps_high() {
        // 容量 1：先塞满一个 Low 事件
        let hub = EventHub::with_capacity(1);
        let mut sub = hub.subscribe(EventFilter::all());
        let low = DesktopEvent::AppExited {
            pid: 1,
            source: EventSource::Portal,
            occurred_at: Instant::now(),
        };
        hub.publish(low).await;
        assert_eq!(sub.rx.capacity(), 0, "Low 事件应已入队占满通道");

        // 再发一个 Low：应被丢弃；再发一个 High：应等待并入队（替换消费后可达）
        let low2 = DesktopEvent::AppLaunched {
            app_id: "x".into(),
            pid: 2,
            desktop_file: None,
            source: EventSource::Portal,
            occurred_at: Instant::now(),
        };
        hub.publish(low2).await; // try_send 失败即丢弃，不阻塞
        let high = DesktopEvent::WindowFocused {
            info: sample_window_info(),
            source: EventSource::KWinWayland,
            occurred_at: Instant::now(),
        };
        let publish_high = hub.publish(high);
        // 消费一个腾出空位，High 投递完成
        tokio::join!(publish_high, async {
            assert!(matches!(
                sub.recv().await,
                Some(DesktopEvent::AppExited { .. })
            ));
        });
        // High 事件必达
        let evt = sub.recv().await;
        assert!(matches!(evt, Some(DesktopEvent::WindowFocused { .. })));
    }

    #[tokio::test]
    async fn hub_unsubscribe_removes_subscriber() {
        let hub = EventHub::new();
        let sub = hub.subscribe(EventFilter::all());
        assert_eq!(hub.subscriber_count(), 1);
        hub.unsubscribe(sub.id);
        assert_eq!(hub.subscriber_count(), 0);
    }

    /// 回归测试（审查发现 #8）：消费者在处理回调中 unsubscribe，
    /// 发布者同时投递 High 事件——快照式投递保证不互等死锁。
    ///
    /// 修复前的实现持锁跨 `send().await`：此场景发布者持锁等待消费者
    /// 腾出通道空位，而消费者卡在等锁退订，互等永不返回。
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn publish_high_with_concurrent_consumer_unsubscribe_no_deadlock() {
        use std::time::Duration;

        let hub = EventHub::with_capacity(1);
        let mut sub = hub.subscribe(EventFilter::all());

        // 先塞满通道，使 High 投递必须等待空位
        hub.publish(DesktopEvent::AppExited {
            pid: 1,
            source: EventSource::Portal,
            occurred_at: Instant::now(),
        })
        .await;

        let hub2 = hub.clone();
        // 发布任务：投递 High 事件（通道满，需等待）
        let publisher = tokio::spawn(async move {
            hub2.publish(DesktopEvent::WindowFocused {
                info: sample_window_info(),
                source: EventSource::KWinWayland,
                occurred_at: Instant::now(),
            })
            .await;
        });

        // 消费任务：recv 到事件后立刻在"处理回调"中退订自己
        let hub3 = hub.clone();
        let consumer = tokio::spawn(async move {
            if let Some(_evt) = sub.recv().await {
                hub3.unsubscribe(sub.id);
            }
        });

        // 任一方死锁都会超时 panic
        tokio::time::timeout(Duration::from_secs(5), async {
            let _ = tokio::join!(publisher, consumer);
        })
        .await
        .expect("publish/consumer 不应死锁");
    }

    /// 回归测试（审查发现 #8 队头阻塞面）：一个慢订阅者不阻塞其他订阅者的投递。
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn slow_subscriber_does_not_block_others() {
        use std::time::Duration;

        let hub = EventHub::with_capacity(1);
        // 慢订阅者：不消费，通道保持满
        let _slow = hub.subscribe(EventFilter::all());
        let mut fast = hub.subscribe(EventFilter::all());

        let hub2 = hub.clone();
        let publisher = tokio::spawn(async move {
            for i in 0..8 {
                hub2.publish(DesktopEvent::AppExited {
                    pid: i,
                    source: EventSource::Portal,
                    occurred_at: Instant::now(),
                })
                .await;
            }
        });

        tokio::time::timeout(Duration::from_secs(5), publisher)
            .await
            .expect("慢订阅者不应阻塞发布循环")
            .unwrap();
        // 快订阅者仍收到事件（至少首个入队的）
        assert!(fast.recv().await.is_some());
    }

    fn sample_window_info() -> crate::types::WindowInfo {
        crate::types::WindowInfo {
            id: win_id(),
            title: "t".into(),
            app_id: "a".into(),
            pid: 1,
            geometry: Rect::default(),
            frame_geometry: Rect::default(),
            states: vec![],
            workspace_id: None,
            monitor_id: None,
            stacking_order: 0,
            desktop_file: None,
            window_type: crate::types::WindowType::Normal,
            icon_geometry: None,
            keep_above: false,
        }
    }
}
