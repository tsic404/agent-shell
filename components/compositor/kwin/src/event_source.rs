//! KWin 长驻事件脚本 → [`RawEvent`] 适配器（设计文档 §18.2）。
//!
//! `event_monitor.js` 的推送（`{"event": "windowOpened", "id": …}` 等）映射为
//! [`RawEvent::KWinWindowAdded`] / [`KWinWindowRemoved`] /
//! [`KWinActiveWindowChanged`]，交由 daemon 侧的
//! [`EventNormalizer`](event::EventNormalizer) 归一化为统一
//! [`DesktopEvent`](event::DesktopEvent) 后 fan-out + 入 ring。
//! 无法识别的载荷跳过（不阻塞流）。

use event::{EventSource, RawEvent, RawSource};
use futures::stream::BoxStream;
use futures::StreamExt;
use serde_json::Value;
use std::sync::Mutex;
use tokio::sync::mpsc;

/// KWin 事件脚本推送的原始事件源（§18.2 [`RawSource`] 的 KWin 实现）。
pub struct KWinRawSource {
    /// 原始推送接收端；`events()` 首次调用时取走（惰性），与一次性查询的
    /// 按 id 路由表完全隔离（见 `dbus_bridge`）。
    rx: Mutex<Option<mpsc::UnboundedReceiver<Value>>>,
    /// 事件源标识（Wayland/X11 会话细分）。
    kind: EventSource,
}

impl KWinRawSource {
    /// 构造：接收端由桥接在 subscribe 时交还，此处惰性持有。
    pub(crate) fn new(rx: mpsc::UnboundedReceiver<Value>, kind: EventSource) -> Self {
        Self {
            rx: Mutex::new(Some(rx)),
            kind,
        }
    }
}

/// `event_monitor.js` 推送 → [`RawEvent`]；未识别事件类型或缺少 `id` 返回
/// `None`（由 [`RawSource::events`] 的流跳过）。
fn map_raw(value: Value) -> Option<RawEvent> {
    let id = value.get("id").and_then(Value::as_str).map(str::to_string);
    match value.get("event").and_then(Value::as_str) {
        Some("windowOpened") => id.map(|id| RawEvent::KWinWindowAdded { id }),
        Some("windowClosed") => id.map(|id| RawEvent::KWinWindowRemoved { id }),
        Some("windowFocused") => id.map(|id| RawEvent::KWinActiveWindowChanged { id: Some(id) }),
        _ => None,
    }
}

impl RawSource for KWinRawSource {
    fn source_name(&self) -> &'static str {
        "kwin-scripting"
    }

    fn source_kind(&self) -> EventSource {
        self.kind.clone()
    }

    fn events(&self) -> BoxStream<'static, RawEvent> {
        let mut rx = self
            .rx
            .lock()
            .expect("kwin raw source lock poisoned")
            .take();
        let Some(rx) = rx.take() else {
            // 已取走过（重复调用）——空流，与 `EventNormalizer` 的单次
            // `events()` 调用契约一致。
            return futures::stream::empty().boxed();
        };
        futures::stream::unfold(rx, |mut rx| async move {
            loop {
                match rx.recv().await {
                    Some(value) => {
                        if let Some(raw) = map_raw(value) {
                            return Some((raw, rx));
                        }
                    }
                    None => return None,
                }
            }
        })
        .boxed()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn maps_script_pushes_to_raw_events() {
        use serde_json::json;
        assert!(matches!(
            map_raw(json!({"event": "windowOpened", "id": "7"})),
            Some(RawEvent::KWinWindowAdded { id }) if id == "7"
        ));
        assert!(matches!(
            map_raw(json!({"event": "windowClosed", "id": "7"})),
            Some(RawEvent::KWinWindowRemoved { id }) if id == "7"
        ));
        assert!(matches!(
            map_raw(json!({"event": "windowFocused", "id": "7"})),
            Some(RawEvent::KWinActiveWindowChanged { id: Some(id) }) if id == "7"
        ));
        // 未识别类型 / 缺少 id → None（跳过）。
        assert!(map_raw(json!({"event": "windowMoved", "id": "7"})).is_none());
        assert!(map_raw(json!({"event": "windowOpened"})).is_none());
    }
}
