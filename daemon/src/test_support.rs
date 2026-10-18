//! 单测基座：把 daemon 的状态目录钉在一次性沙箱里。
//!
//! 单测直接调 `dispatch()` 时，门禁审计与 portal 会话状态按生产 XDG 解析落盘：
//! 不隔离就会写进被测主机（`~/.local/state/agent-shell`），每跑一次 `cargo test`
//! 即向宿主追加一批审计记录，既污染主机又无法与其它运行区分、更无法回滚。
//!
//! 只换目录位置，不换行为：装配仍走生产 [`Daemon::connect_with_state_dir`]，审计与
//! portal 会话仍走各自的生产写入路径；隔离不走环境变量——`set_var` 是进程级状态，
//! 与本 crate 并行执行的单测天然冲突。

use std::ops::{Deref, DerefMut};
use std::path::PathBuf;
use std::sync::Arc;
use std::time::Duration;

use agent_shell_core::security::{AgentShellConfig, SecurityManager};

use crate::portal_sessions::PortalSessionManager;
use crate::state::Daemon;

/// 状态目录沙箱：生产 `state_dir()` 的替身，随测试结束整目录删除。
///
/// 审计记录、portal 会话状态（`sessions.json`，含注入 `capture` 的 restore_token
/// 后端）都落在其内，宿主状态目录不写任何记录；装配期 `SecurityManager` 仍会按
/// 生产路径确保状态目录存在（空目录，幂等）。
pub(crate) struct Sandbox {
    root: tempfile::TempDir,
}

impl Sandbox {
    pub(crate) fn new() -> Self {
        Self {
            root: tempfile::tempdir().expect("test sandbox"),
        }
    }

    /// 状态目录替身（与生产同构：沙箱根下的 `agent-shell`）。
    pub(crate) fn state_dir(&self) -> PathBuf {
        self.root.path().join("agent-shell")
    }

    /// 审计日志替身（生产为 `state_dir()/audit.jsonl`）。
    pub(crate) fn audit_path(&self) -> PathBuf {
        self.state_dir().join("audit.jsonl")
    }

    /// 单测专用安全配置：默认配置 + 审计落沙箱。
    ///
    /// 用默认配置而非宿主 `config.toml`：宿主已授予的级别与自定义审计路径不得
    /// 左右单测判定（需要具名 caller 或自定义路径的测试在基座返回值上显式覆盖）。
    pub(crate) fn security(&self) -> SecurityManager {
        let mut config = AgentShellConfig::default();
        config.security.audit_log_path = Some(self.audit_path().to_string_lossy().into_owned());
        SecurityManager::with_config(config)
    }

    /// portal 会话管理器：状态目录指向沙箱。
    pub(crate) fn portal_sessions(&self) -> Arc<PortalSessionManager> {
        Arc::new(PortalSessionManager::new(self.state_dir()))
    }
}

/// 单测基座装配出的 daemon：与其状态目录沙箱同生死。
///
/// 解引用到 [`Daemon`]，测试体按 daemon 使用即可（`&mut d` 经解引用传给
/// `dispatch`）。需要裸 `Daemon`（如 `Arc<Mutex<Daemon>>`）时用
/// [`TestDaemon::into_parts`]，并在测试体内自行持有沙箱，使其活过测试。
pub(crate) struct TestDaemon {
    daemon: Daemon,
    sandbox: Sandbox,
}

impl TestDaemon {
    /// 装配：状态根在装配期就钉在沙箱上，portal 会话与注入 `capture` 的
    /// restore_token 后端因此共用同一个沙箱实例，无需事后替换。
    ///
    /// `caller_id` 回落 `"*"`——调用方身份来自环境注入，不参与单测判定。
    pub(crate) async fn start(idle_timeout: Duration) -> Self {
        let sandbox = Sandbox::new();
        let mut daemon = Daemon::connect_with_state_dir(idle_timeout, sandbox.state_dir()).await;
        daemon.caller_id = "*".into();
        daemon.security = sandbox.security();
        Self { daemon, sandbox }
    }

    /// 沙箱（断言审计落点等场景用）。
    pub(crate) fn sandbox(&self) -> &Sandbox {
        &self.sandbox
    }

    /// 拆成裸 daemon 与沙箱：调用方负责让沙箱活过测试（Drop 即清除）。
    pub(crate) fn into_parts(self) -> (Daemon, Sandbox) {
        (self.daemon, self.sandbox)
    }
}

impl Deref for TestDaemon {
    type Target = Daemon;

    fn deref(&self) -> &Daemon {
        &self.daemon
    }
}

impl DerefMut for TestDaemon {
    fn deref_mut(&mut self) -> &mut Daemon {
        &mut self.daemon
    }
}
