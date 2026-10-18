//! 单测基座：把审计日志钉在一次性沙箱里，并守住整轮的宿主零写入。
//!
//! router 单测经 [`crate::Executor`] 走生产安全判定与审计写入；若用默认配置，审计
//! 按 XDG 解析落进宿主状态目录（`~/.local/state/agent-shell/audit.jsonl`），每跑一轮
//! `cargo test -p agent-shell-router --lib` 即追加一批记录，污染被测主机且无法回滚。
//! 与 daemon 单测基座同一机制：只换目录位置，不换行为——判定与审计仍走生产
//! `SecurityManager`。逐用例断言只看得到自己那次写入，故另设套件级守卫
//! [`suite_guard_host_state_stays_empty`]；隔离全程不碰环境变量（`set_var` 是进程级
//! 状态，与本 crate 并行执行的单测天然冲突）。

use std::path::{Path, PathBuf};
use std::process::Command;
use std::sync::Arc;

use agent_shell_core::security::{AgentShellConfig, SecurityManager};

/// 状态目录沙箱：生产 `state_dir()` 的替身，随测试结束整目录删除。
pub(crate) struct Sandbox {
    root: tempfile::TempDir,
}

impl Sandbox {
    pub(crate) fn new() -> Self {
        Self {
            root: tempfile::tempdir().expect("test sandbox"),
        }
    }

    /// 审计日志替身（生产为 `state_dir()/audit.jsonl`）。
    pub(crate) fn audit_path(&self) -> PathBuf {
        self.root.path().join("agent-shell").join("audit.jsonl")
    }

    /// 单测专用安全配置：默认配置 + 审计落沙箱。
    ///
    /// 用默认配置而非宿主 `config.toml`：宿主已授予的级别与自定义审计路径不得
    /// 左右单测判定。模块私有——取安全入口只能经 [`Sandbox::security`] /
    /// [`Sandbox::security_with`]，绕开沙箱得显式引 `agent_shell_core` 的配置类型。
    fn config(&self) -> AgentShellConfig {
        let mut config = AgentShellConfig::default();
        config.security.audit_log_path = Some(self.audit_path().to_string_lossy().into_owned());
        config
    }

    /// 单测专用安全入口：审计落沙箱。
    pub(crate) fn security(&self) -> SecurityManager {
        SecurityManager::with_config(self.config())
    }

    /// 单测专用安全入口：审计落沙箱，另按用例覆盖策略（黑名单 / 确认覆盖）。
    pub(crate) fn security_with(
        &self,
        edit: impl FnOnce(&mut AgentShellConfig),
    ) -> SecurityManager {
        let mut config = self.config();
        edit(&mut config);
        SecurityManager::with_config(config)
    }
}

/// 沙箱与安全入口同生死的单测夹具。
///
/// 测试体持有本值即保证沙箱活过测试；[`TestSecurity::security`] 交给
/// [`crate::Executor`]，需要读回审计时复用同一个 `Arc`。
pub(crate) struct TestSecurity {
    sandbox: Sandbox,
    security: Arc<SecurityManager>,
}

impl TestSecurity {
    pub(crate) fn new() -> Self {
        let sandbox = Sandbox::new();
        let security = Arc::new(sandbox.security());
        Self { sandbox, security }
    }

    pub(crate) fn security(&self) -> Arc<SecurityManager> {
        Arc::clone(&self.security)
    }

    pub(crate) fn sandbox(&self) -> &Sandbox {
        &self.sandbox
    }
}

// ───────────────────────── 套件级守卫 ─────────────────────────

/// 本守卫自身的用例名：子进程用 `--skip` 排除它，避免递归。
const GUARD_TEST: &str = "suite_guard_host_state_stays_empty";

/// 套件级守卫：整轮单测重放进状态根被钉死的子进程，跑完哨兵必须空无一物。
///
/// 逐用例断言只看得到自己那个标记，别的用例漏沙箱它查不到；本守卫抓的正是
/// 「任何一个装配点回退默认审计路径」——那会写进哨兵，本轮即转红，不必等第二轮
/// 跨轮计数。哨兵只改子进程的状态根（`Command::env`），不动本进程环境。
#[test]
fn suite_guard_host_state_stays_empty() {
    let sentinel = tempfile::tempdir().expect("guard sentinel");
    let exe = std::env::current_exe().expect("test binary path");
    let listed = count_listed_tests(&exe);
    assert!(
        listed > 1,
        "`--list` 只列出 {listed} 个用例，守卫判定失去意义"
    );

    let child = Command::new(&exe)
        .args(["--test-threads=1", "--skip", GUARD_TEST])
        // 宿主写入面全钉进哨兵：状态/配置/运行时/数据根与 HOME（C-109 的四类根）。
        .env("XDG_STATE_HOME", sentinel.path().join("state"))
        .env("XDG_CONFIG_HOME", sentinel.path().join("config"))
        .env("XDG_RUNTIME_DIR", sentinel.path().join("runtime"))
        .env("XDG_DATA_HOME", sentinel.path().join("data"))
        .env("HOME", sentinel.path().join("home"))
        .output()
        .expect("spawn guard child");
    let output = format!(
        "{}{}",
        String::from_utf8_lossy(&child.stdout),
        String::from_utf8_lossy(&child.stderr)
    );
    let (passed, failed, ignored) = summarize(&output)
        .unwrap_or_else(|| panic!("子进程未给出 libtest 汇总行，整轮结果不可判定：\n{output}"));
    assert!(
        child.status.success() && failed == 0,
        "子进程整轮单测未全绿（{failed} failed）：\n{output}"
    );
    assert_eq!(
        passed + ignored,
        listed - 1,
        "子进程须跑完整轮（仅排除本守卫自身）：\n{output}"
    );

    let leaks = sentinel_writes(sentinel.path());
    assert!(
        leaks.is_empty(),
        "整轮单测写进了宿主状态目录，应只落各自的一次性沙箱；哨兵 = 子进程的 XDG 状态根：\n{leaks:#?}"
    );
}

/// 子进程 `--list` 的用例数（`libtest` 每行形如 `<模块>::<用例>: test`）。
fn count_listed_tests(exe: &Path) -> usize {
    let out = Command::new(exe)
        .arg("--list")
        .output()
        .expect("list tests in guard child");
    String::from_utf8_lossy(&out.stdout)
        .lines()
        .filter(|line| line.ends_with(": test"))
        .count()
}

/// 取汇总行的（通过, 失败, 忽略）；找不到汇总行返回 `None`。
fn summarize(output: &str) -> Option<(usize, usize, usize)> {
    let line = output
        .lines()
        .rev()
        .find(|line| line.starts_with("test result:"))?;
    let count = |key: &str| -> Option<usize> {
        let head = &line[..line.find(key)?];
        head.split_whitespace().last()?.parse().ok()
    };
    Some((count("passed;")?, count("failed;")?, count("ignored;")?))
}

/// 哨兵目录下的写入痕迹：相对路径 + 文本内容，失败时用于定位漏沙箱的用例。
fn sentinel_writes(root: &Path) -> Vec<String> {
    let mut entries = Vec::new();
    let mut dirs = vec![root.to_path_buf()];
    while let Some(dir) = dirs.pop() {
        let Ok(read) = std::fs::read_dir(&dir) else {
            continue;
        };
        for entry in read.flatten() {
            let path = entry.path();
            if path.is_dir() {
                dirs.push(path.clone());
            }
            entries.push(path);
        }
    }
    entries.sort();
    entries
        .into_iter()
        .map(|path| {
            let rel = path.strip_prefix(root).unwrap_or(&path).display();
            match std::fs::read_to_string(&path) {
                Ok(text) => format!("{rel}: {}", text.trim().replace('\n', " ⏎ ")),
                Err(_) => format!("{rel}: <目录>"),
            }
        })
        .collect()
}
