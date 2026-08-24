//! 环形事件缓冲 — 保留最近 N 条事件，供 `--replay` 与订阅者断线补发。
//!
//! 对应架构决策 D4（§22.5）：环形队列保留最近 N=1000 条；
//! 订阅者断开自动移除，重连恢复时补发最近事件。

use std::collections::VecDeque;
use std::sync::Arc;

use parking_lot::Mutex;

use crate::DesktopEvent;

/// 固定容量的线程安全环形事件缓冲。
///
/// 满后新事件挤掉最旧事件；快照返回从旧到新的顺序。
#[derive(Clone)]
pub struct EventRing {
    inner: Arc<Mutex<VecDeque<DesktopEvent>>>,
    capacity: usize,
}

impl std::fmt::Debug for EventRing {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("EventRing")
            .field("len", &self.len())
            .finish()
    }
}

impl Default for EventRing {
    fn default() -> Self {
        Self::new(1000)
    }
}

impl EventRing {
    /// 创建保留最近 `capacity` 条事件的环形缓冲（D4 默认 1000）。
    pub fn new(capacity: usize) -> Self {
        Self {
            inner: Arc::new(Mutex::new(VecDeque::with_capacity(capacity.max(1)))),
            capacity: capacity.max(1),
        }
    }

    /// 追加事件；超出容量时丢弃最旧的一条。
    pub fn push(&self, event: DesktopEvent) {
        let mut ring = self.inner.lock();
        if ring.len() == self.capacity {
            ring.pop_front();
        }
        ring.push_back(event);
    }

    /// 快照：从旧到新的最近事件（最多 `capacity` 条）。
    pub fn snapshot(&self) -> Vec<DesktopEvent> {
        self.inner.lock().iter().cloned().collect()
    }

    /// 缓冲中的事件数。
    pub fn len(&self) -> usize {
        self.inner.lock().len()
    }

    /// 是否为空。
    pub fn is_empty(&self) -> bool {
        self.len() == 0
    }

    /// 容量。
    pub fn capacity(&self) -> usize {
        self.capacity
    }
}

/// WindowMoved 合并窗口（对应 D4「去重/节流」）。
pub mod throttle {
    use std::time::Duration;

    /// 连续 [`DesktopEvent::WindowMoved`] 在此窗口内合并，仅推送最终位置。
    pub const WINDOW_MERGE: Duration = Duration::from_millis(100);
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::EventSource;
    use agent_shell_core::types::WindowId;
    use std::time::Instant;

    fn closed(id: u32) -> DesktopEvent {
        DesktopEvent::WindowClosed {
            id: WindowId {
                native_id: id.to_string(),
                de_type: agent_shell_core::types::DesktopEnvironment::Unknown,
            },
            source: EventSource::Hyprland,
            occurred_at: Instant::now(),
        }
    }

    #[test]
    fn ring_evicts_oldest() {
        let ring = EventRing::new(3);
        for i in 0..5 {
            ring.push(closed(i));
        }
        assert_eq!(ring.len(), 3);
        let snap = ring.snapshot();
        // 最旧的 0、1 被挤出，剩 2、3、4
        assert!(matches!(&snap[0], DesktopEvent::WindowClosed { id, .. } if id.native_id == "2"));
        assert!(
            matches!(snap.last(), Some(DesktopEvent::WindowClosed { id, .. }) if id.native_id == "4")
        );
    }

    #[test]
    fn default_capacity_is_1000() {
        assert_eq!(EventRing::default().capacity(), 1000);
    }
}
