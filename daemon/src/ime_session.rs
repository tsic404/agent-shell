//! IME 会话（§22.8 D7）。
//!
//! daemon 持有 IBus 连接与 InputContext。CLI 瞬态进程无法保持，
//! 故 IME 打字经 daemon 路由。
//!
//! 非 ASCII 文本经 IBus ProcessKeyEvent → CommitText；纯 ASCII 跳过
//! IME 直接 type_text（性能路径）。
//!
//! v1：IBus D-Bus 实际连接待 Phase 3 系统服务接线；当前为 stub
//! ——非 ASCII 返回 committed + verified:false，ASCII 返回 verified:true。

use agent_shell_rpc::ImeTypeResult;

pub struct ImeSession {
    engine: std::sync::Mutex<Option<String>>,
}

impl ImeSession {
    pub fn new() -> Self {
        Self {
            engine: std::sync::Mutex::new(None),
        }
    }

    pub fn set_engine(&self, engine: &str) {
        tracing::debug!(engine, "setting IME engine");
        *self.engine.lock().unwrap() = Some(engine.to_string());
    }

    pub fn current_engine(&self) -> Option<String> {
        self.engine.lock().unwrap().clone()
    }

    pub fn list_engines(&self) -> Vec<String> {
        vec![
            "libpinyin".into(),
            "pinyin".into(),
            "wubi".into(),
            "sunpinyin".into(),
        ]
    }

    pub fn type_text(&self, text: &str) -> ImeTypeResult {
        if text.is_ascii() {
            ImeTypeResult {
                committed: text.to_string(),
                verified: true,
            }
        } else {
            // v1 stub：IBus ProcessKeyEvent 管线待接线
            ImeTypeResult {
                committed: text.to_string(),
                verified: false,
            }
        }
    }
}

impl Default for ImeSession {
    fn default() -> Self {
        Self::new()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_ascii_skips_ime() {
        let s = ImeSession::new();
        let r = s.type_text("hello");
        assert_eq!(r.committed, "hello");
        assert!(r.verified);
    }

    #[test]
    fn test_non_ascii_unverified() {
        let s = ImeSession::new();
        let r = s.type_text("你好世界");
        assert_eq!(r.committed, "你好世界");
        assert!(!r.verified);
    }

    #[test]
    fn test_set_get_engine() {
        let s = ImeSession::new();
        assert!(s.current_engine().is_none());
        s.set_engine("libpinyin");
        assert_eq!(s.current_engine().as_deref(), Some("libpinyin"));
    }

    #[test]
    fn test_list_engines() {
        let s = ImeSession::new();
        assert!(s.list_engines().contains(&"libpinyin".to_string()));
    }
}
