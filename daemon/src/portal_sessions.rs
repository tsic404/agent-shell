//! Portal 会话管理器（§22.6 D5）。
//!
//! daemon 持有 portal 会话（ScreenCast/RemoteDesktop/Clipboard 等），
//! 持久化 restore_token 到 `~/.local/state/agent-shell/sessions.json`。
//! daemon 重启时静默恢复，失败标记 invalid。

use std::collections::HashMap;
use std::path::PathBuf;

use agent_shell_rpc::SessionEntry;
use serde::{Deserialize, Serialize};

/// sessions.json 格式。
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct SessionsFile {
    pub version: u32,
    pub sessions: HashMap<String, PersistedSession>,
}

impl Default for SessionsFile {
    fn default() -> Self {
        Self {
            version: 1,
            sessions: HashMap::new(),
        }
    }
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct PersistedSession {
    pub kind: String,
    pub restore_token: String,
    pub created_at: String,
    pub persist_mode: u32,
}

/// Portal 会话管理器——daemon 持有，持久化 restore_token。
pub struct PortalSessionManager {
    sessions: std::sync::Mutex<SessionsFile>,
    state_dir: PathBuf,
}

#[allow(dead_code)]
impl PortalSessionManager {
    pub fn new(state_dir: PathBuf) -> Self {
        let mgr = Self {
            sessions: std::sync::Mutex::new(SessionsFile::default()),
            state_dir,
        };
        mgr.ensure_dir();
        mgr.load();
        mgr
    }

    fn ensure_dir(&self) {
        let _ = std::fs::create_dir_all(&self.state_dir);
    }

    fn sessions_path(&self) -> PathBuf {
        self.state_dir.join("sessions.json")
    }

    pub fn load(&self) {
        let path = self.sessions_path();
        if let Ok(content) = std::fs::read_to_string(&path) {
            if let Ok(file) = serde_json::from_str::<SessionsFile>(&content) {
                tracing::debug!(sessions = file.sessions.len(), "loaded sessions.json");
                *self.sessions.lock().unwrap() = file;
            }
        }
    }

    pub fn save(&self) -> Result<(), String> {
        // 审查项 #9：原子写入——先写 .tmp 再 rename，避免崩溃产生半截文件。
        let path = self.sessions_path();
        let tmp = path.with_extension("json.tmp");
        let sf = self.sessions.lock().unwrap();
        let json = serde_json::to_string_pretty(&*sf).map_err(|e| format!("serialize: {e}"))?;
        std::fs::write(&tmp, json + "\n").map_err(|e| format!("write tmp: {e}"))?;
        std::fs::rename(&tmp, &path).map_err(|e| format!("rename: {e}"))?;
        Ok(())
    }

    pub fn set_token(&self, kind: &str, token: String, persist_mode: u32) -> Result<(), String> {
        let mut sf = self.sessions.lock().unwrap();
        sf.sessions.insert(
            kind.to_string(),
            PersistedSession {
                kind: kind.to_string(),
                restore_token: token,
                created_at: now_iso(),
                persist_mode,
            },
        );
        drop(sf);
        self.save()
    }

    pub fn get_token(&self, kind: &str) -> Option<String> {
        self.sessions
            .lock()
            .unwrap()
            .sessions
            .get(kind)
            .map(|s| s.restore_token.clone())
    }

    pub fn remove_token(&self, kind: &str) -> Result<(), String> {
        self.sessions.lock().unwrap().sessions.remove(kind);
        self.save()
    }

    pub fn list_sessions(&self) -> Vec<SessionEntry> {
        self.sessions
            .lock()
            .unwrap()
            .sessions
            .values()
            .map(|s| SessionEntry {
                kind: s.kind.clone(),
                restore_token: s.restore_token.clone(),
                created_at: s.created_at.clone(),
                persist_mode: s.persist_mode,
            })
            .collect()
    }
}

/// ScreenCast restore_token 持久化桥接——供 `CaptureDispatcher` 注入。
///
/// session kind 固定为 `"screencast"`（设计文档 §22.7）；persist_mode=2
/// （persist_until_revoked）。
impl agent_shell_capture::TokenStore for PortalSessionManager {
    fn get_restore_token(&self) -> Option<String> {
        self.get_token(SCREENCAST_SESSION_KIND)
    }

    fn save_restore_token(&self, token: Option<String>) {
        match token {
            Some(t) => {
                if let Err(e) = self.set_token(SCREENCAST_SESSION_KIND, t, 2) {
                    tracing::warn!("failed to persist screencast restore_token: {e}");
                }
            }
            None => {
                if let Err(e) = self.remove_token(SCREENCAST_SESSION_KIND) {
                    tracing::warn!("failed to remove screencast restore_token: {e}");
                }
            }
        }
    }
}

/// ScreenCast 会话在 sessions.json 中的 kind 键。
const SCREENCAST_SESSION_KIND: &str = "screencast";

fn now_iso() -> String {
    use std::time::{SystemTime, UNIX_EPOCH};
    let secs = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0);
    // 转换为 ISO 8601 (YYYY-MM-DDTHH:MM:SSZ)。
    let days = (secs / 86400) as i64;
    let rem = secs % 86400;
    let hour = rem / 3600;
    let min = (rem % 3600) / 60;
    let sec = rem % 60;
    let (year, month, day) = days_to_ymd(days);
    format!("{year:04}-{month:02}-{day:02}T{hour:02}:{min:02}:{sec:02}Z")
}

/// days since 1970-01-01 → (year, month, day)。
/// Algorithm from http://howardhinnant.github.io/date_algorithms.html。
fn days_to_ymd(days: i64) -> (i64, u32, u32) {
    let z = days + 719468;
    let era = if z >= 0 { z } else { z - 146096 } / 146097;
    let doe = z - era * 146097;
    let yoe = (doe - doe / 1460 + doe / 36524 - doe / 146096) / 365;
    let y = yoe + era * 400;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let d = doy - (153 * mp + 2) / 5 + 1;
    let m = if mp < 10 { mp + 3 } else { mp - 9 };
    (if m <= 2 { y + 1 } else { y }, m as u32, d as u32)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_set_get_token() {
        let tmp = std::env::temp_dir().join(format!("as-test-{}", std::process::id()));
        let mgr = PortalSessionManager::new(tmp.clone());
        mgr.set_token("screencast", "token123".into(), 3).unwrap();
        assert_eq!(mgr.get_token("screencast").as_deref(), Some("token123"));
        let _ = std::fs::remove_dir_all(&tmp);
    }

    #[test]
    fn test_persist_and_reload() {
        let tmp = std::env::temp_dir().join(format!("as-test-reload-{}", std::process::id()));
        let mgr = PortalSessionManager::new(tmp.clone());
        mgr.set_token("remotedesktop", "tok".into(), 3).unwrap();
        let mgr2 = PortalSessionManager::new(tmp.clone());
        assert_eq!(mgr2.get_token("remotedesktop").as_deref(), Some("tok"));
        let _ = std::fs::remove_dir_all(&tmp);
    }

    #[test]
    fn test_remove_token() {
        let tmp = std::env::temp_dir().join(format!("as-test-rm-{}", std::process::id()));
        let mgr = PortalSessionManager::new(tmp.clone());
        mgr.set_token("clipboard", "c".into(), 3).unwrap();
        assert!(mgr.get_token("clipboard").is_some());
        mgr.remove_token("clipboard").unwrap();
        assert!(mgr.get_token("clipboard").is_none());
        let _ = std::fs::remove_dir_all(&tmp);
    }

    #[test]
    fn test_list_sessions() {
        let tmp = std::env::temp_dir().join(format!("as-test-ls-{}", std::process::id()));
        let mgr = PortalSessionManager::new(tmp.clone());
        mgr.set_token("screencast", "t1".into(), 3).unwrap();
        mgr.set_token("clipboard", "t2".into(), 3).unwrap();
        assert_eq!(mgr.list_sessions().len(), 2);
        let _ = std::fs::remove_dir_all(&tmp);
    }

    #[test]
    fn test_now_iso_year_is_real() {
        // 审查项 #5：now_iso 必须产生真实时间戳（年份 >= 2026）。
        let iso = now_iso();
        let year: i64 = iso[..4].parse().expect("year prefix");
        assert!(year >= 2026, "expected year >= 2026, got {iso}");
        // 格式校验：YYYY-MM-DDTHH:MM:SSZ
        assert!(iso.ends_with('Z'));
        assert_eq!(iso.len(), 20);
    }
}
