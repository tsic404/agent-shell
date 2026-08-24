//! EventHub — 归一化事件的 fan-out 中心（有界通道 + 背压）。
//!
//! 对应设计文档 §18.4 与 D4（§22.5）：每个订阅者一条有界 `tokio::sync::mpsc`
//! 通道（默认容量 1024）；High 事件并发限时投递（停滞订阅者降级丢弃）、Medium/Low 溢出丢弃，不阻塞发布者。

use std::collections::HashMap;
use std::sync::atomic::Ordering;
use std::sync::Arc;

use parking_lot::Mutex;

use tokio::sync::mpsc;
use uuid::Uuid;

use crate::events::{DesktopEvent, EventFilter, EventPriority};

/// High 事件单次投递的等待上限：超时视为订阅者停滞，降级丢弃（审查问题 3）。
const HIGH_SEND_TIMEOUT: std::time::Duration = std::time::Duration::from_millis(250);

/// 进程级 High 事件降级丢弃计数（停滞订阅者诊断）。
static HIGH_DROPPED: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);

/// 订阅句柄（[`EventHub::subscribe`] 的返回值）。
pub struct EventSubscription {
    /// 订阅 ID（用于 [`EventHub::unsubscribe`]）
    pub id: Uuid,
    pub(crate) rx: mpsc::Receiver<DesktopEvent>,
    pub(crate) filter: EventFilter,
}

impl EventSubscription {
    /// 异步接收下一个匹配过滤器的事件。
    ///
    /// 不匹配 `filter` 的事件被静默跳过；hub 关闭或订阅被取消时返回 `None`。
    pub async fn recv(&mut self) -> Option<DesktopEvent> {
        loop {
            let evt = self.rx.recv().await?;
            if self.filter.matches(&evt) {
                return Some(evt);
            }
        }
    }

    /// 非阻塞接收下一个匹配过滤器的事件。
    pub fn try_recv(&mut self) -> Option<DesktopEvent> {
        loop {
            match self.rx.try_recv() {
                Ok(evt) if self.filter.matches(&evt) => return Some(evt),
                Ok(_) => continue,
                Err(_) => return None,
            }
        }
    }

    /// 订阅 ID。
    pub fn id(&self) -> Uuid {
        self.id
    }
}

#[derive(Default)]
struct EventHubInner {
    subscribers: Mutex<HashMap<Uuid, (mpsc::Sender<DesktopEvent>, EventFilter)>>,
}

/// 事件枢纽（所有组件事件归一化后的 fan-out 中心）。
///
/// 可克隆，所有克隆共享同一组订阅者。背压策略（对应设计文档 §18.4
/// 「超限丢弃 Low 优先级事件」的强化版）：
/// - `High` 事件：通道满时等待空闲槽位，保证窗口开关/聚焦等关键事件必达；
/// - `Medium` / `Low` 事件：通道满时 `try_send` 失败即丢弃，不阻塞发布者。
pub struct EventHub {
    inner: Arc<EventHubInner>,
    capacity: usize,
}

impl Clone for EventHub {
    fn clone(&self) -> Self {
        Self {
            inner: Arc::clone(&self.inner),
            capacity: self.capacity,
        }
    }
}

impl std::fmt::Debug for EventHub {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("EventHub")
            .field("capacity", &self.capacity)
            .field("subscribers", &self.inner.subscribers.lock().len())
            .finish()
    }
}

impl Default for EventHub {
    fn default() -> Self {
        Self::new()
    }
}

impl EventHub {
    /// 创建新 EventHub，默认容量 1024。
    pub fn new() -> Self {
        Self::with_capacity(1024)
    }

    /// 创建指定容量的 EventHub。
    pub fn with_capacity(capacity: usize) -> Self {
        Self {
            inner: Arc::new(EventHubInner::default()),
            capacity: capacity.max(1),
        }
    }

    /// 每个订阅者通道容量。
    pub fn capacity(&self) -> usize {
        self.capacity
    }

    /// 订阅事件流。
    ///
    /// 返回订阅句柄；hub 投递前按订阅者的 `filter` 预过滤，
    /// 不匹配的类别不投递（未订阅类别零开销）。
    pub fn subscribe(&self, filter: EventFilter) -> EventSubscription {
        let id = Uuid::new_v4();
        let (tx, rx) = mpsc::channel(self.capacity);
        self.inner
            .subscribers
            .lock()
            .insert(id, (tx, filter.clone()));
        EventSubscription { id, rx, filter }
    }

    /// 发布事件到所有订阅者（fan-out）。
    ///
    /// 并发契约：锁内仅快照订阅者（clone `(Sender, EventFilter)`），立即释放锁后
    /// 在锁外投递——消费者在处理回调中调用 [`EventHub::unsubscribe`] 不会与
    /// 发布者互等死锁。
    ///
    /// 背压策略（审查问题 3 修复）：
    /// - `High` 事件：每个匹配订阅者并发 `tokio::spawn` 投递，单次等待上限
    ///   [`HIGH_SEND_TIMEOUT`]——停滞订阅者超时后降级丢弃并告警计数，
    ///   不阻塞发布者，也不队头阻塞其他订阅者的任何优先级投递；
    /// - `Medium`/`Low` 事件：通道满即丢弃（try_send），不阻塞发布者。
    pub async fn publish(&self, event: DesktopEvent) {
        let snapshot: Vec<(mpsc::Sender<DesktopEvent>, EventFilter)> =
            self.inner.subscribers.lock().values().cloned().collect();

        if event.priority() == EventPriority::High {
            let mut deliveries = Vec::new();
            for (tx, filter) in snapshot {
                // 订阅级预过滤：不匹配该订阅者的类别直接跳过
                if !filter.matches(&event) {
                    continue;
                }
                let evt = event.clone();
                deliveries.push(tokio::spawn(async move {
                    match tokio::time::timeout(HIGH_SEND_TIMEOUT, tx.send(evt)).await {
                        Ok(Ok(())) => {}
                        Ok(Err(_)) => {} // 订阅者已 drop，忽略
                        Err(_) => {
                            // 停滞订阅者：降级丢弃并告警
                            tracing::warn!(
                                timeout_ms = HIGH_SEND_TIMEOUT.as_millis() as u64,
                                "high-priority event dropped: subscriber stalled"
                            );
                            HIGH_DROPPED.fetch_add(1, Ordering::Relaxed);
                        }
                    }
                }));
            }
            for d in deliveries {
                let _ = d.await;
            }
            return;
        }

        for (tx, filter) in snapshot {
            if !filter.matches(&event) {
                continue;
            }
            // Medium/Low 事件：满则丢弃（背压），不阻塞发布者
            let _ = tx.try_send(event.clone());
        }
    }

    /// High 事件因订阅者停滞被降级丢弃的累计次数（诊断用）。
    pub fn high_dropped_count(&self) -> u64 {
        HIGH_DROPPED.load(Ordering::Relaxed)
    }

    /// 主动取消某个订阅。
    pub fn unsubscribe(&self, id: Uuid) {
        self.inner.subscribers.lock().remove(&id);
    }

    /// 当前活跃订阅者数量。
    pub fn subscriber_count(&self) -> usize {
        self.inner.subscribers.lock().len()
    }
}
