//! 安全与授权模型（设计文档 §21.21 / §22.7 D6）。
//!
//! 纯逻辑模块：权限分级、配置解析、黑白名单判定、审计旁路。无系统 IO
//! 依赖（除配置读写与审计日志落盘，均以 XDG 路径收敛在 [`super::audit`]），
//! 单元测试无需真实会话。确认 UI / 通知 / 超时交互在 `daemon/safety.rs`
//! （本 issue 纯后端范围，router 层对 `Confirm` 判定返回 pending 占位）。
//!
//! # 判定顺序
//! 1. 黑名单（`permissions.deny`，按操作名 glob 匹配）→ `Deny`；
//! 2. 操作确认覆盖（`operations.confirm`，按操作名匹配）→ `Confirm(mode)`；
//! 3. agent 白名单级别（`permissions.allow`）不足 → `Confirm(Always)`；
//! 4. 否则 → `Allow`。
//!
//! 该顺序按设计 §21.21.3 伪代码：deny 永远最优先；显式 per-op 确认覆盖
//! 优先于泛化的白名单级别。

use crate::audit::{config_dir, AuditLogger};
use serde::{Deserialize, Serialize};
use std::collections::BTreeMap;
use std::fmt;
use std::path::PathBuf;
use std::str::FromStr;
use std::time::Duration;

/// 权限分级（§21.21.1）。`#[repr(u8)]` + derive 使 L0 < L1 < … < L4，
/// `op.level() > agent_level` 的判定直接可用。
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Hash)]
#[repr(u8)]
pub enum PermissionLevel {
    /// 只读：不修改系统状态。
    L0 = 0,
    /// 低风险：可逆、影响小。
    L1 = 1,
    /// 中风险：可逆但影响面大（自动执行 + 审计）。
    L2 = 2,
    /// 高风险：不可逆或影响系统（默认确认，可配白名单）。
    L3 = 3,
    /// 系统级：影响整个会话（必须确认 + polkit）。
    L4 = 4,
}

impl PermissionLevel {
    /// 字符串形态（`L0`..`L4`，TOML 序列化用）。
    pub fn as_str(self) -> &'static str {
        match self {
            Self::L0 => "L0",
            Self::L1 => "L1",
            Self::L2 => "L2",
            Self::L3 => "L3",
            Self::L4 => "L4",
        }
    }

    /// 低一级；`L0` 无更低级别返回 `None`。
    pub fn below(self) -> Option<Self> {
        match self {
            Self::L0 => None,
            Self::L1 => Some(Self::L0),
            Self::L2 => Some(Self::L1),
            Self::L3 => Some(Self::L2),
            Self::L4 => Some(Self::L3),
        }
    }

    /// 0..=self 的完整级别序列（`grant` 授权到某级时展开）。
    pub fn up_to(self) -> Vec<Self> {
        (0..=self as u8).map(Self::from_u8).collect()
    }

    fn from_u8(v: u8) -> Self {
        match v {
            0 => Self::L0,
            1 => Self::L1,
            2 => Self::L2,
            3 => Self::L3,
            _ => Self::L4,
        }
    }
}

impl fmt::Display for PermissionLevel {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.as_str())
    }
}

impl FromStr for PermissionLevel {
    type Err = String;

    fn from_str(s: &str) -> Result<Self, Self::Err> {
        match s.trim().to_ascii_uppercase().as_str() {
            "L0" => Ok(Self::L0),
            "L1" => Ok(Self::L1),
            "L2" => Ok(Self::L2),
            "L3" => Ok(Self::L3),
            "L4" => Ok(Self::L4),
            other => Err(format!(
                "unknown permission level {other:?} (expect L0..L4)"
            )),
        }
    }
}

impl Serialize for PermissionLevel {
    fn serialize<S: serde::Serializer>(&self, s: S) -> Result<S::Ok, S::Error> {
        s.serialize_str(self.as_str())
    }
}

impl<'de> Deserialize<'de> for PermissionLevel {
    fn deserialize<D: serde::Deserializer<'de>>(d: D) -> Result<Self, D::Error> {
        let s = String::deserialize(d)?;
        Self::from_str(&s).map_err(serde::de::Error::custom)
    }
}

/// 确认方式（§21.21.3）。
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ConfirmMode {
    /// 每次都要确认。
    Always,
    /// 本次会话确认一次。
    Once,
    /// 限时确认（超时后需重新确认）。
    Timeout(Duration),
}

impl ConfirmMode {
    fn as_str(self) -> String {
        match self {
            Self::Always => "always".into(),
            Self::Once => "once".into(),
            Self::Timeout(d) => format!("timeout:{}", d.as_secs()),
        }
    }
}

impl fmt::Display for ConfirmMode {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.as_str())
    }
}

impl FromStr for ConfirmMode {
    type Err = String;

    fn from_str(s: &str) -> Result<Self, Self::Err> {
        let t = s.trim().to_ascii_lowercase();
        match t.as_str() {
            "always" => Ok(Self::Always),
            "once" => Ok(Self::Once),
            _ => {
                if let Some(secs) = t.strip_prefix("timeout:") {
                    let secs = secs
                        .trim_end_matches('s')
                        .parse::<u64>()
                        .map_err(|_| format!("bad timeout in confirm mode {s:?}"))?;
                    Ok(Self::Timeout(Duration::from_secs(secs)))
                } else {
                    Err(format!(
                        "unknown confirm mode {s:?} (expect always|once|timeout:<secs>)"
                    ))
                }
            }
        }
    }
}

impl Serialize for ConfirmMode {
    fn serialize<S: serde::Serializer>(&self, s: S) -> Result<S::Ok, S::Error> {
        s.serialize_str(&self.as_str())
    }
}

impl<'de> Deserialize<'de> for ConfirmMode {
    fn deserialize<D: serde::Deserializer<'de>>(d: D) -> Result<Self, D::Error> {
        let s = String::deserialize(d)?;
        Self::from_str(&s).map_err(serde::de::Error::custom)
    }
}

/// 权限判定结果。
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum PermissionDecision {
    /// 允许执行。
    Allow,
    /// 需要确认（携带确认方式）。
    Confirm(ConfirmMode),
    /// 拒绝执行（携带原因）。
    Deny(String),
}

/// 待检查的操作：名称 + 权限级别。router 层把每个 `Command` 变体映射为
/// 一个 `Operation`，`check_permission` 只依赖 name/level 做纯逻辑判定。
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Operation {
    name: &'static str,
    level: PermissionLevel,
}

impl Operation {
    pub const fn new(name: &'static str, level: PermissionLevel) -> Self {
        Self { name, level }
    }

    pub fn name(&self) -> &'static str {
        self.name
    }

    pub fn level(&self) -> PermissionLevel {
        self.level
    }
}

impl fmt::Display for Operation {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.name)
    }
}

/// `[security]` 配置段（§21.21.2）。
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct SecuritySection {
    /// 默认确认级别：达到该级别的操作需确认（可被白名单覆盖）。
    pub default_confirm_level: PermissionLevel,
    /// 是否写审计日志。
    pub audit_log: bool,
    /// 审计日志路径；`None` = 默认 `state_dir()/audit.jsonl`。
    pub audit_log_path: Option<String>,
}

impl Default for SecuritySection {
    fn default() -> Self {
        Self {
            default_confirm_level: PermissionLevel::L3,
            audit_log: true,
            audit_log_path: None,
        }
    }
}

/// `[permissions]` 配置段。
#[derive(Clone, Debug, Default, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct PermissionsSection {
    /// 白名单：agent（app_id/executable 或 `"*"` 通配）→ 允许的最高级别序列。
    pub allow: BTreeMap<String, Vec<PermissionLevel>>,
    /// 黑名单：操作名 glob 模式 → 是否拒绝（`true` = 拒绝）。
    pub deny: BTreeMap<String, bool>,
}

/// `[operations]` 配置段。
#[derive(Clone, Debug, Default, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct OperationsSection {
    /// 操作确认覆盖：操作名 glob 模式 → 确认方式。
    pub confirm: BTreeMap<String, ConfirmMode>,
}

/// 完整配置模型（§21.21.2，TOML 序列化）。
#[derive(Clone, Debug, Default, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct AgentShellConfig {
    pub security: SecuritySection,
    pub permissions: PermissionsSection,
    pub operations: OperationsSection,
}

impl AgentShellConfig {
    /// 序列化为 TOML 文本。
    pub fn to_toml(&self) -> Result<String, String> {
        toml::to_string_pretty(self).map_err(|e| format!("config serialize failed: {e}"))
    }

    /// 从 TOML 文本解析（缺失字段回落到默认值）。
    pub fn from_toml(text: &str) -> Result<Self, String> {
        toml::from_str(text).map_err(|e| format!("config parse failed: {e}"))
    }

    /// 加载 `config_dir()/config.toml`；文件不存在返回默认配置，解析失败
    /// 返回错误（不静默吞掉损坏的配置）。
    pub fn load() -> Result<Self, String> {
        let path = config_dir().join("config.toml");
        if !path.exists() {
            return Ok(Self::default());
        }
        let text = std::fs::read_to_string(&path)
            .map_err(|e| format!("read {} failed: {e}", path.display()))?;
        Self::from_toml(&text)
    }

    /// 写回 `config_dir()/config.toml`。先写同目录临时文件再 rename 原子替换，
    /// 避免写半截配置（审查项 #7）。
    pub fn save(&self) -> Result<(), String> {
        let dir = config_dir();
        std::fs::create_dir_all(&dir)
            .map_err(|e| format!("create {} failed: {e}", dir.display()))?;
        let path = dir.join("config.toml");
        let tmp = dir.join(format!("config.toml.{}.tmp", std::process::id()));
        std::fs::write(&tmp, self.to_toml()?)
            .map_err(|e| format!("write {} failed: {e}", tmp.display()))?;
        std::fs::rename(&tmp, &path)
            .map_err(|e| format!("rename {} -> {} failed: {e}", tmp.display(), path.display()))
    }
}

/// 安全判定核心（§21.21.3）。持配置 + 审计日志器，router 层唯一入口。
#[derive(Clone, Debug)]
pub struct SecurityManager {
    pub config: AgentShellConfig,
    pub audit: AuditLogger,
}

impl SecurityManager {
    /// 从默认路径加载配置 + 审计日志器。
    pub fn load_default() -> Result<Self, String> {
        let config = AgentShellConfig::load()?;
        Ok(Self::with_config(config))
    }

    /// 以给定配置构造（审计路径取 `[security].audit_log_path` 或默认）。
    pub fn with_config(config: AgentShellConfig) -> Self {
        let path = config
            .security
            .audit_log_path
            .as_deref()
            .map(expand_home)
            .unwrap_or_else(AuditLogger::default_path);
        Self {
            config,
            audit: AuditLogger::new(path),
        }
    }

    /// 判定入口（§21.21.3 伪代码顺序）。判定结果落审计旁路；执行结果由
    /// 调用方在 handler 返回后经 [`SecurityManager::record_execution`] 落盘。
    pub fn check_permission(&self, agent_id: &str, op: &Operation) -> PermissionDecision {
        let decision = self.decide(agent_id, op);
        self.record(agent_id, op, &decision);
        decision
    }

    /// 纯判定（不落审计；单元测试/复用用）。
    pub fn decide(&self, agent_id: &str, op: &Operation) -> PermissionDecision {
        // 1. 黑名单 → Deny（永远最优先）。
        if let Some(reason) = self.is_denied(op) {
            return PermissionDecision::Deny(reason);
        }
        // 2. 操作确认覆盖 → Confirm。
        if let Some(mode) = self.confirm_mode(op) {
            return PermissionDecision::Confirm(mode);
        }
        // 3. 白名单级别不足 → Confirm。
        if op.level() > self.agent_level(agent_id) {
            return PermissionDecision::Confirm(ConfirmMode::Always);
        }
        // 4. 默认允许。
        PermissionDecision::Allow
    }

    /// 审计旁路：只记门禁判定——`allow` / `confirm` / `deny`，`result` 恒为
    /// `false`（未执行态）。真实执行结果由 [`SecurityManager::record_execution`]
    /// 在 handler 返回后追加，二者独立、先后有序。
    fn record(&self, agent_id: &str, op: &Operation, decision: &PermissionDecision) {
        if !self.config.security.audit_log {
            return;
        }
        let decision_str = match decision {
            PermissionDecision::Allow => "allow",
            PermissionDecision::Confirm(_) => "confirm",
            PermissionDecision::Deny(_) => "deny",
        };
        self.audit.log(agent_id, op.name(), decision_str, false);
    }

    /// 执行结果审计：handler 返回后由 dispatch 层调用，按 RPC 结果回写
    /// `result`（成功 `true`，失败 `false`），使 `decision=allow` 不再与
    /// 「已执行」划等号——后端不可用等运行期失败同样留下 allow+false 痕迹
    /// （TSI-2659）。
    pub fn record_execution(&self, agent_id: &str, op: &Operation, success: bool) {
        if !self.config.security.audit_log {
            return;
        }
        self.audit.log(agent_id, op.name(), "allow", success);
    }

    /// agent 的最高自动放行级别：白名单精确匹配 > `"*"` 通配 > 默认
    /// （`default_confirm_level` 低一级——达到默认级别即需确认）。
    pub fn agent_level(&self, agent_id: &str) -> PermissionLevel {
        let allow = &self.config.permissions.allow;
        let granted = allow
            .get(agent_id)
            .or_else(|| allow.get("*"))
            .and_then(|levels| levels.iter().max().copied());
        granted.unwrap_or_else(|| {
            self.config
                .security
                .default_confirm_level
                .below()
                .unwrap_or(PermissionLevel::L0)
        })
    }

    /// 黑名单匹配：任一 `true` 模式命中操作名即拒绝。
    fn is_denied(&self, op: &Operation) -> Option<String> {
        for (pattern, deny) in &self.config.permissions.deny {
            if *deny && glob_matches(pattern, op.name()) {
                return Some(format!("operation {op} denied by policy"));
            }
        }
        None
    }

    /// 操作确认覆盖：先精确名匹配，再 glob 模式匹配（字典序确定性）。
    fn confirm_mode(&self, op: &Operation) -> Option<ConfirmMode> {
        let confirm = &self.config.operations.confirm;
        if let Some(mode) = confirm.get(op.name()) {
            return Some(*mode);
        }
        confirm
            .iter()
            .find(|(pattern, _)| glob_matches(pattern, op.name()))
            .map(|(_, mode)| *mode)
    }

    /// 授权：把 agent 的白名单设为 L0..=level（覆盖旧值）并持久化。
    pub fn grant(&mut self, agent_id: &str, level: PermissionLevel) -> Result<(), String> {
        let old = self.config.permissions.allow.get(agent_id).cloned();
        self.config
            .permissions
            .allow
            .insert(agent_id.to_string(), level.up_to());
        if let Err(e) = self.config.save() {
            // 持久化失败回滚内存态，保持配置真值不变。
            match old {
                Some(v) => {
                    self.config
                        .permissions
                        .allow
                        .insert(agent_id.to_string(), v);
                }
                None => {
                    self.config.permissions.allow.remove(agent_id);
                }
            }
            return Err(e);
        }
        Ok(())
    }

    /// 撤销：移除 agent 的白名单条目并持久化。
    pub fn revoke(&mut self, agent_id: &str) -> Result<(), String> {
        let old = self.config.permissions.allow.remove(agent_id);
        if let Err(e) = self.config.save() {
            // 持久化失败回滚内存态，保持配置真值不变。
            if let Some(v) = old {
                self.config
                    .permissions
                    .allow
                    .insert(agent_id.to_string(), v);
            }
            return Err(e);
        }
        Ok(())
    }
}

/// `~` 开头的路径展开为 HOME（TOML 示例 `audit_log_path` 允许 `~`）。
fn expand_home(path: &str) -> PathBuf {
    if let Some(rest) = path.strip_prefix("~/") {
        if let Some(home) = dirs::home_dir() {
            return home.join(rest);
        }
    }
    if path == "~" {
        if let Some(home) = dirs::home_dir() {
            return home;
        }
    }
    PathBuf::from(path)
}

/// 简单 glob 匹配（`*` 跨任意段；无 `?`/字符类，够权限模式用）。
fn glob_matches(pattern: &str, value: &str) -> bool {
    glob_match::glob_match(pattern, value)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn op(name: &'static str, level: PermissionLevel) -> Operation {
        Operation::new(name, level)
    }

    fn default_manager() -> SecurityManager {
        SecurityManager::with_config(AgentShellConfig::default())
    }

    #[test]
    fn level_ordering_and_strings() {
        assert!(PermissionLevel::L0 < PermissionLevel::L4);
        assert_eq!(PermissionLevel::L2.below(), Some(PermissionLevel::L1));
        assert_eq!(PermissionLevel::L0.below(), None);
        assert_eq!(PermissionLevel::L3.as_str(), "L3");
        assert_eq!(
            "L2".parse::<PermissionLevel>().unwrap(),
            PermissionLevel::L2
        );
        assert_eq!(
            PermissionLevel::L3.up_to(),
            vec![
                PermissionLevel::L0,
                PermissionLevel::L1,
                PermissionLevel::L2,
                PermissionLevel::L3
            ]
        );
    }

    #[test]
    fn default_config_auto_allows_below_confirm_level() {
        // default_confirm_level = L3 → 未白名单 agent 自动放行 L0-L2，L3+ 确认。
        let m = default_manager();
        assert_eq!(m.agent_level("anyone"), PermissionLevel::L2);
        assert_eq!(
            m.decide("anyone", &op("windows.list", PermissionLevel::L0)),
            PermissionDecision::Allow
        );
        assert_eq!(
            m.decide("anyone", &op("windows.op", PermissionLevel::L2)),
            PermissionDecision::Allow
        );
        assert_eq!(
            m.decide("anyone", &op("service.control", PermissionLevel::L3)),
            PermissionDecision::Confirm(ConfirmMode::Always)
        );
    }

    #[test]
    fn deny_wins_over_everything() {
        let mut m = default_manager();
        m.config.permissions.deny.insert("input.*".into(), true);
        m.config
            .operations
            .confirm
            .insert("input.send".into(), ConfirmMode::Once);
        assert!(matches!(
            m.decide("anyone", &op("input.send", PermissionLevel::L1)),
            PermissionDecision::Deny(_)
        ));
        // 不匹配的仍正常允许。
        assert_eq!(
            m.decide("anyone", &op("windows.list", PermissionLevel::L0)),
            PermissionDecision::Allow
        );
    }

    #[test]
    fn confirm_override_precedes_whitelist() {
        let mut m = default_manager();
        m.config
            .permissions
            .allow
            .insert("trusted".into(), vec![PermissionLevel::L4]);
        m.config
            .operations
            .confirm
            .insert("poweroff".into(), ConfirmMode::Always);
        assert_eq!(
            m.decide("trusted", &op("poweroff", PermissionLevel::L4)),
            PermissionDecision::Confirm(ConfirmMode::Always)
        );
    }

    #[test]
    fn whitelist_raises_agent_level() {
        let mut m = default_manager();
        m.config
            .permissions
            .allow
            .insert("trusted".into(), vec![PermissionLevel::L3]);
        assert_eq!(
            m.decide("trusted", &op("service.control", PermissionLevel::L3)),
            PermissionDecision::Allow
        );
        // 未列名 agent 仍走默认 L2。
        assert_eq!(
            m.decide("other", &op("service.control", PermissionLevel::L3)),
            PermissionDecision::Confirm(ConfirmMode::Always)
        );
    }

    #[test]
    fn confirm_mode_roundtrip() {
        for mode in [
            ConfirmMode::Always,
            ConfirmMode::Once,
            ConfirmMode::Timeout(Duration::from_secs(60)),
        ] {
            let s = mode.to_string();
            assert_eq!(s.parse::<ConfirmMode>().unwrap(), mode);
        }
    }

    #[test]
    fn config_toml_roundtrip() {
        let cfg = AgentShellConfig {
            security: SecuritySection {
                audit_log_path: Some("~/audit.jsonl".into()),
                ..Default::default()
            },
            permissions: PermissionsSection {
                allow: BTreeMap::from([
                    ("*".into(), vec![PermissionLevel::L0, PermissionLevel::L1]),
                    (
                        "trusted".into(),
                        vec![
                            PermissionLevel::L0,
                            PermissionLevel::L1,
                            PermissionLevel::L2,
                            PermissionLevel::L3,
                        ],
                    ),
                ]),
                deny: BTreeMap::from([("service.*".into(), true)]),
            },
            operations: OperationsSection {
                confirm: BTreeMap::from([("poweroff".into(), ConfirmMode::Always)]),
            },
        };
        let text = cfg.to_toml().expect("serialize");
        let parsed = AgentShellConfig::from_toml(&text).expect("parse");
        assert_eq!(parsed, cfg);
    }

    #[test]
    fn audit_path_uses_config_override() {
        let cfg = AgentShellConfig {
            security: SecuritySection {
                audit_log_path: Some("/tmp/agent-shell-audit-test.jsonl".into()),
                ..Default::default()
            },
            ..Default::default()
        };
        let m = SecurityManager::with_config(cfg);
        assert_eq!(
            m.audit.path(),
            std::path::Path::new("/tmp/agent-shell-audit-test.jsonl")
        );
    }

    #[test]
    fn audit_default_path_ends_with_audit_jsonl() {
        assert!(AuditLogger::default_path().ends_with("audit.jsonl"));
    }

    /// 并发测试共享环境变量，串行化避免互相污染。
    static ENV_LOCK: std::sync::Mutex<()> = std::sync::Mutex::new(());

    /// 把 XDG_CONFIG_HOME 指向一个普通文件，使 `config_dir()` 的
    /// `create_dir_all` 必然失败。返回 (旧值, 阻塞文件路径)。
    fn block_config_writes() -> (Option<String>, PathBuf) {
        let old = std::env::var("XDG_CONFIG_HOME").ok();
        let blocker =
            std::env::temp_dir().join(format!("agent-shell-xdg-block-{}", std::process::id()));
        std::fs::write(&blocker, "not-a-directory").expect("write blocker");
        std::env::set_var("XDG_CONFIG_HOME", &blocker);
        (old, blocker)
    }

    fn restore_config_writes(old: Option<String>, blocker: &PathBuf) {
        match old {
            Some(v) => std::env::set_var("XDG_CONFIG_HOME", v),
            None => std::env::remove_var("XDG_CONFIG_HOME"),
        }
        let _ = std::fs::remove_file(blocker);
    }

    #[test]
    fn grant_rolls_back_on_save_failure() {
        let _guard = ENV_LOCK.lock().unwrap();
        let (old, blocker) = block_config_writes();
        let mut m = default_manager();

        // 无旧值：授权失败后不得残留白名单条目。
        let err = m.grant("agent-x", PermissionLevel::L3).unwrap_err();
        assert!(err.contains("create"), "unexpected error: {err}");
        assert!(!m.config.permissions.allow.contains_key("agent-x"));

        // 有旧值：授权失败后旧值必须还原。
        m.config
            .permissions
            .allow
            .insert("agent-x".into(), vec![PermissionLevel::L1]);
        let err = m.grant("agent-x", PermissionLevel::L3).unwrap_err();
        assert!(err.contains("create"), "unexpected error: {err}");
        assert_eq!(
            m.config.permissions.allow.get("agent-x"),
            Some(&vec![PermissionLevel::L1])
        );

        restore_config_writes(old, &blocker);
    }

    #[test]
    fn revoke_rolls_back_on_save_failure() {
        let _guard = ENV_LOCK.lock().unwrap();
        let (old, blocker) = block_config_writes();
        let mut m = default_manager();
        m.config
            .permissions
            .allow
            .insert("agent-y".into(), vec![PermissionLevel::L2]);

        let err = m.revoke("agent-y").unwrap_err();
        assert!(err.contains("create"), "unexpected error: {err}");
        assert_eq!(
            m.config.permissions.allow.get("agent-y"),
            Some(&vec![PermissionLevel::L2])
        );

        restore_config_writes(old, &blocker);
    }
}
