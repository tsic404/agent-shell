//! 审计日志（设计文档 §21.21.3 / §22.7 D6）。
//!
//! `AuditLogger` 将每次权限判定写为一行 JSON（JSONL），落盘到 XDG 运行时
//! 状态目录（默认 `~/.local/state/agent-shell/audit.jsonl`）。`security.audit`
//! RPC 通过 [`AuditLogger::query`] 读回并按 `agent_id` / `op` / `decision`
//! 过滤。日志只追加、不覆盖；IO 失败只降级为 tracing 告警，不使业务路径
//! 失败——审计是旁路记录，不是权限判定的前置依赖。

use serde::{Deserialize, Serialize};
use std::path::{Path, PathBuf};

/// 单条审计记录（与设计 §21.21.3 `AgentAction` 对应，追加 `decision` 字段
/// 以便 `security.audit` 按判定结果过滤）。
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct AuditEntry {
    /// 记录生成时刻（UTC 秒级时间戳）。
    pub timestamp: u64,
    /// 发起操作的 agent（app_id 或 executable；未识别时为 `"*"`）。
    pub agent_id: String,
    /// 操作名（如 `windows.list`、`input.send`）。
    pub op: String,
    /// 权限判定：`allow` / `confirm` / `deny`。
    pub decision: String,
    /// 最终执行结果（`true` = 已执行，`false` = 未执行）。
    pub result: bool,
}

/// 追加式 JSONL 审计日志。
#[derive(Clone, Debug)]
pub struct AuditLogger {
    path: PathBuf,
}

impl AuditLogger {
    /// 以默认路径构造（`state_dir()/audit.jsonl`）。
    pub fn default_path() -> PathBuf {
        state_dir().join("audit.jsonl")
    }

    /// 在指定路径构造；调用方负责保证父目录存在（见 [`AuditLogger::new`]）。
    pub fn at(path: impl Into<PathBuf>) -> Self {
        Self { path: path.into() }
    }

    /// 创建日志器并确保父目录存在。
    pub fn new(path: impl Into<PathBuf>) -> Self {
        let path = path.into();
        if let Some(parent) = path.parent() {
            let _ = std::fs::create_dir_all(parent);
        }
        Self { path }
    }

    /// 追加一条审计记录。IO 失败仅告警，不影响调用方。
    pub fn log(&self, agent_id: &str, op: &str, decision: &str, result: bool) {
        let entry = AuditEntry {
            timestamp: now_secs(),
            agent_id: agent_id.to_string(),
            op: op.to_string(),
            decision: decision.to_string(),
            result,
        };
        let line = match serde_json::to_string(&entry) {
            Ok(l) => l,
            Err(e) => {
                tracing::warn!("audit serialize failed: {e}");
                return;
            }
        };
        if let Some(parent) = self.path.parent() {
            let _ = std::fs::create_dir_all(parent);
        }
        use std::io::Write;
        match std::fs::OpenOptions::new()
            .create(true)
            .append(true)
            .open(&self.path)
        {
            Ok(mut f) => {
                if writeln!(f, "{line}").is_err() {
                    tracing::warn!("audit write failed: {}", self.path.display());
                }
            }
            Err(e) => tracing::warn!("audit open failed: {e}"),
        }
    }

    /// 读回全部记录；文件不存在视为空日志。
    pub fn read_all(&self) -> Vec<AuditEntry> {
        let Ok(content) = std::fs::read_to_string(&self.path) else {
            return Vec::new();
        };
        content
            .lines()
            .filter_map(|line| serde_json::from_str::<AuditEntry>(line).ok())
            .collect()
    }

    /// 按 `agent_id` / `op` / `decision` 过滤（空串 = 不限制该维度）。
    pub fn query(&self, agent_id: &str, op: &str, decision: &str) -> Vec<AuditEntry> {
        self.read_all()
            .into_iter()
            .filter(|e| agent_id.is_empty() || e.agent_id == agent_id)
            .filter(|e| op.is_empty() || e.op == op)
            .filter(|e| decision.is_empty() || e.decision == decision)
            .collect()
    }

    /// 日志文件路径（诊断/`security.status` 展示用）。
    pub fn path(&self) -> &Path {
        &self.path
    }
}

/// XDG 运行时状态目录（`XDG_STATE_HOME` → `~/.local/state/agent-shell`）。
///
/// 与 daemon `single_instance::state_dir()` 约定一致；core 不依赖 daemon，
/// 故在安全模块内自持一份。
pub fn state_dir() -> PathBuf {
    if let Ok(dir) = std::env::var("XDG_STATE_HOME") {
        if !dir.is_empty() {
            return PathBuf::from(dir).join("agent-shell");
        }
    }
    dirs::home_dir()
        .unwrap_or_else(|| PathBuf::from("."))
        .join(".local")
        .join("state")
        .join("agent-shell")
}

/// 配置目录（`XDG_CONFIG_HOME` → `~/.config/agent-shell`）。
pub fn config_dir() -> PathBuf {
    if let Ok(dir) = std::env::var("XDG_CONFIG_HOME") {
        if !dir.is_empty() {
            return PathBuf::from(dir).join("agent-shell");
        }
    }
    dirs::home_dir()
        .unwrap_or_else(|| PathBuf::from("."))
        .join(".config")
        .join("agent-shell")
}

fn now_secs() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0)
}
