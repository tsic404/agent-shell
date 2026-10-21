//! 记录型原始事件源：断言首次订阅确实装配了事件管线。
//!
//! `EventNormalizer::run()` 会调用一次 `RawSource::events()` 取流；调用次数因此
//! 是「管线已装配」的可判据证据，而不是测试自报状态。

use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::Arc;

use event::{EventSource, RawEvent, RawSource};
use futures::stream::BoxStream;
use futures::StreamExt;

/// 被订阅装配取流的原始事件源。
#[derive(Clone)]
pub struct RecordingRawSource {
    calls: Arc<AtomicUsize>,
    kind: EventSource,
}

impl RecordingRawSource {
    /// 以给定事件来源构造空流。
    pub fn new(kind: EventSource) -> Self {
        Self {
            calls: Arc::new(AtomicUsize::new(0)),
            kind,
        }
    }

    /// `events()` 被调用的次数（装配证据）。
    pub fn events_calls(&self) -> usize {
        self.calls.load(Ordering::SeqCst)
    }

    /// 等待装配任务真正取流（`EventNormalizer::run` 异步调度）。
    pub async fn wait_events_calls(&self, expected: usize) {
        for _ in 0..200 {
            if self.events_calls() >= expected {
                assert_eq!(
                    self.events_calls(),
                    expected,
                    "raw source must be consumed exactly {expected} time(s)"
                );
                return;
            }
            tokio::time::sleep(std::time::Duration::from_millis(5)).await;
        }
        panic!(
            "raw source was consumed {} time(s), expected {expected}",
            self.events_calls()
        );
    }
}

impl RawSource for RecordingRawSource {
    fn source_name(&self) -> &'static str {
        "acceptance-recording-source"
    }

    fn source_kind(&self) -> EventSource {
        self.kind.clone()
    }

    fn events(&self) -> BoxStream<'static, RawEvent> {
        self.calls.fetch_add(1, Ordering::SeqCst);
        futures::stream::empty().boxed()
    }
}
