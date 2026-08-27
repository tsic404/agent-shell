//! 环形缓冲区（§22.5 D4）——保留最近 N=1000 条事件供 CLI `events --replay`。

use std::collections::VecDeque;

const RING_CAPACITY: usize = 1000;

/// 环形缓冲——保留最近 N 条事件。
#[allow(dead_code)]
pub struct RingBuffer<T: Clone> {
    buf: VecDeque<T>,
    cap: usize,
}

#[allow(dead_code)]
impl<T: Clone> RingBuffer<T> {
    pub fn new() -> Self {
        Self::with_capacity(RING_CAPACITY)
    }

    pub fn with_capacity(cap: usize) -> Self {
        Self {
            buf: VecDeque::with_capacity(cap),
            cap,
        }
    }

    pub fn push(&mut self, item: T) {
        if self.buf.len() == self.cap {
            self.buf.pop_front();
        }
        self.buf.push_back(item);
    }

    /// 按时间顺序返回所有缓存事件（旧→新）。
    pub fn replay(&self) -> Vec<T> {
        self.buf.iter().cloned().collect()
    }

    pub fn len(&self) -> usize {
        self.buf.len()
    }

    pub fn is_empty(&self) -> bool {
        self.buf.is_empty()
    }
}

impl<T: Clone> Default for RingBuffer<T> {
    fn default() -> Self {
        Self::new()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_push_and_replay() {
        let mut ring = RingBuffer::<i32>::new();
        ring.push(1);
        ring.push(2);
        ring.push(3);
        assert_eq!(ring.replay(), vec![1, 2, 3]);
    }

    #[test]
    fn test_capacity_evicts_oldest() {
        let mut ring = RingBuffer::<i32>::with_capacity(3);
        ring.push(1);
        ring.push(2);
        ring.push(3);
        ring.push(4); // evicts 1
        assert_eq!(ring.replay(), vec![2, 3, 4]);
        assert_eq!(ring.len(), 3);
    }

    #[test]
    fn test_empty() {
        let ring: RingBuffer<i32> = RingBuffer::new();
        assert!(ring.is_empty());
        assert_eq!(ring.replay(), Vec::<i32>::new());
    }
}
