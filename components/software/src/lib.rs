//! 软件管理组件（设计文档 §21.30「Flatpak / PackageKit」）。
//!
//! [`SoftwareOps`] 是能力契约（daemon 持 `dyn SoftwareOps`，测试注入 fake）。
//! 公开实现 [`FlatpakClient`] 走 `flatpak` CLI：`flatpak list --app
//! --columns=…` 是唯一稳定给出「已安装应用 + 来源 remote」的接口，D-Bus
//! 侧 `org.freedesktop.Flatpak` 不暴露安装清单。

use std::path::PathBuf;
use std::time::Duration;

use agent_shell_core::error::{AgentShellError, Result};
use agent_shell_core::services::FlatpakApp;
use async_trait::async_trait;
use tokio::process::Command;

/// `flatpak list` 的列顺序；解析按同一下标取值，改这里必须同步 [`parse_flatpak_list`]。
const LIST_COLUMNS: &str = "application,name,origin,version,branch,installation";

/// 单次 `flatpak list` 的超时秒数（后端挂死时不能无限期占住调用方）。
const LIST_TIMEOUT_S: u64 = 10;

/// 软件管理契约（§21.30；v1 仅只读查询，安装/更新走 rootd 特权链路）。
#[async_trait]
pub trait SoftwareOps: Send + Sync {
    /// 列出已安装 Flatpak 应用。
    async fn list_flatpaks(&self) -> Result<Vec<FlatpakApp>>;
}

/// Flatpak 查询后端：每次调用直接执行一次 `flatpak list`。
pub struct FlatpakClient {
    /// flatpak 可执行路径（生产为 PATH 查找，测试经 `with_bin` 注入 stub）。
    bin: PathBuf,
}

impl Default for FlatpakClient {
    fn default() -> Self {
        Self::new()
    }
}

impl FlatpakClient {
    /// 使用 PATH 中的 `flatpak`。
    pub fn new() -> Self {
        Self {
            bin: PathBuf::from("flatpak"),
        }
    }

    /// 指定 flatpak 可执行路径（测试 seam，非对外 API）。
    #[doc(hidden)]
    pub fn with_bin(bin: PathBuf) -> Self {
        Self { bin }
    }

    /// 执行 `flatpak list --app --columns=…` 并返回 stdout（仅成功时）。
    async fn run_list(&self) -> Result<String> {
        let mut cmd = Command::new(&self.bin);
        cmd.arg("list")
            .arg("--app")
            .arg(format!("--columns={LIST_COLUMNS}"))
            // 超时会丢弃 output() future；不置 kill_on_drop 则挂死的 flatpak 变成孤儿进程。
            .kill_on_drop(true);
        let out = tokio::time::timeout(Duration::from_secs(LIST_TIMEOUT_S), cmd.output())
            .await
            .map_err(|_| {
                AgentShellError::Timeout(format!("flatpak list timed out after {LIST_TIMEOUT_S}s"))
            })?
            .map_err(|e| match e.kind() {
                std::io::ErrorKind::NotFound => {
                    AgentShellError::BackendUnavailable("flatpak not installed".into())
                }
                _ => AgentShellError::Other(format!("flatpak spawn: {e}").into()),
            })?;
        if out.status.success() {
            Ok(String::from_utf8_lossy(&out.stdout).into_owned())
        } else {
            Err(AgentShellError::Other(
                format!(
                    "flatpak list: {}",
                    String::from_utf8_lossy(&out.stderr).trim()
                )
                .into(),
            ))
        }
    }
}

#[async_trait]
impl SoftwareOps for FlatpakClient {
    async fn list_flatpaks(&self) -> Result<Vec<FlatpakApp>> {
        // 空 stdout 表示「flatpak 在但一个应用都没装」，是正常结果而非错误。
        Ok(parse_flatpak_list(&self.run_list().await?))
    }
}

/// 解析 `flatpak list --columns=application,name,origin,version,branch,installation`
/// 的制表符分隔输出。
///
/// flatpak 会省略全空的尾列（如 version/branch 缺失时该行只到 origin），故下标
/// 越界补空串；application 为空的行是空行或噪声行，跳过。
fn parse_flatpak_list(out: &str) -> Vec<FlatpakApp> {
    let mut apps = Vec::new();
    for line in out.split('\n') {
        let line = line.strip_suffix('\r').unwrap_or(line);
        let fields: Vec<&str> = line.split('\t').collect();
        let field = |idx: usize| fields.get(idx).copied().unwrap_or("").trim();
        let app_id = field(0);
        if app_id.is_empty() {
            continue;
        }
        apps.push(FlatpakApp {
            app_id: app_id.to_string(),
            name: field(1).to_string(),
            origin: field(2).to_string(),
            version: field(3).to_string(),
            branch: field(4).to_string(),
            installation: field(5).to_string(),
        });
    }
    apps
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::ErrorKind;
    use std::os::unix::fs::PermissionsExt;
    use std::path::Path;
    use std::sync::atomic::{AtomicUsize, Ordering};
    use std::sync::Arc;

    /// 并发回归用例的 worker 数（与 `#[tokio::test]` 的 worker 线程数一致）。
    const STRESS_WORKERS: usize = 8;

    /// 每个 worker 的「写 → chmod → spawn」轮次。该规模下闸门缺失必然命中 ETXTBSY。
    const STRESS_ROUNDS_PER_WORKER: usize = 300;

    /// 全局 fork 闸门。fork 出的子进程会复制其它线程打开着的写 fd（CLOEXEC 要到
    /// execve 之后才收尾，而 ETXTBSY 检查在前），并行测试里写 stub 的线程就可能
    /// exec 到自己那份「还有写者」的文件而偶发 `Text file busy`。任何会 fork 的
    /// 用例（含 spawn 不存在的二进制）都必须经 [`Sandbox`] 取锁。
    static FORK_GUARD: tokio::sync::Mutex<()> = tokio::sync::Mutex::const_new(());

    /// 测试沙箱：持 fork 闸门 + 临时目录。会 fork 的用例一律经它搭建，
    /// 「会 fork 必持闸门」由构造保证而非人工约定。
    struct Sandbox {
        _fork_guard: tokio::sync::MutexGuard<'static, ()>,
        dir: Arc<tempfile::TempDir>,
    }

    impl Sandbox {
        /// 独占临时目录的沙箱。
        async fn new() -> Self {
            Self::with_dir(Arc::new(tempfile::tempdir().expect("tempdir"))).await
        }

        /// 复用已有目录的沙箱：并发回归用例的 worker 共享同一 stub 路径。
        async fn with_dir(dir: Arc<tempfile::TempDir>) -> Self {
            Self {
                _fork_guard: FORK_GUARD.lock().await,
                dir,
            }
        }

        fn path(&self) -> &Path {
            self.dir.path()
        }

        /// 写可执行 stub 脚本，代替真实 flatpak 二进制。
        ///
        /// 返回错误而非 panic：闸门失效时写入自身会命中 ETXTBSY，
        /// 回归用例需要计数而不是中断。
        fn stub(&self, name: &str, body: &str) -> std::io::Result<PathBuf> {
            let path = self.dir.path().join(name);
            std::fs::write(&path, format!("#!/bin/sh\n{body}\n"))?;
            let mut perms = std::fs::metadata(&path)?.permissions();
            perms.set_mode(0o755);
            std::fs::set_permissions(&path, perms)?;
            Ok(path)
        }
    }

    /// ETXTBSY：写入正被执行的二进制，或 exec 一个仍有写者的文件。
    fn is_text_file_busy(err: &std::io::Error) -> bool {
        err.kind() == ErrorKind::ExecutableFileBusy
    }

    #[test]
    fn parses_realistic_list_output() {
        // 混合形态：完整行、name/version 空列、CRLF 行、缺尾列行、缺 application 行。
        let out = "org.mozilla.firefox\tFirefox\tflathub\t128.0.3\tstable\tsystem\n\
                   org.example.Headless\t\tflathub\t\tstable\tuser\n\
                   org.videolan.VLC\tVLC\tflathub\t3.0.21\tstable\tsystem\r\n\
                   io.github.Short\tShort\tflathub\n\
                   \tGhost\tflathub\t1.0\tstable\tsystem\n";
        let apps = parse_flatpak_list(out);
        assert_eq!(apps.len(), 4);
        assert_eq!(apps[0].app_id, "org.mozilla.firefox");
        assert_eq!(apps[0].name, "Firefox");
        assert_eq!(apps[0].origin, "flathub");
        assert_eq!(apps[0].version, "128.0.3");
        assert_eq!(apps[0].branch, "stable");
        assert_eq!(apps[0].installation, "system");
        // name/version 空列保留为空串，其余列仍按位对齐。
        assert_eq!(apps[1].app_id, "org.example.Headless");
        assert_eq!(apps[1].name, "");
        assert_eq!(apps[1].version, "");
        assert_eq!(apps[1].installation, "user");
        // CRLF：末尾列不得残留 '\r'。
        assert_eq!(apps[2].app_id, "org.videolan.VLC");
        assert_eq!(apps[2].installation, "system");
        // 缺尾列：补空串而非错位或丢行。
        assert_eq!(apps[3].app_id, "io.github.Short");
        assert_eq!(apps[3].origin, "flathub");
        assert_eq!(apps[3].branch, "");
        assert_eq!(apps[3].installation, "");
    }

    #[test]
    fn empty_output_yields_no_apps() {
        assert!(parse_flatpak_list("").is_empty());
        // 仅一个换行：flatpak 在但无应用安装。
        assert!(parse_flatpak_list("\n").is_empty());
    }

    #[test]
    fn skips_rows_without_application_id() {
        // application 为空的整行无法归属到任何应用，丢弃而不是产出空 id 条目。
        assert!(parse_flatpak_list("\tGhost\tflathub\t1.0\tstable\tsystem\n").is_empty());
        // 空白行（含只有空格的行）没有应用 id，同样丢弃。
        assert!(parse_flatpak_list("   \n\n").is_empty());
    }

    #[tokio::test]
    async fn stub_binary_output_is_parsed() {
        let sandbox = Sandbox::new().await;
        let bin = sandbox
            .stub(
                "flatpak",
                "printf 'org.mozilla.firefox\\tFirefox\\tflathub\\t128.0.3\\tstable\\tsystem\\n'\nprintf 'org.videolan.VLC\\tVLC\\tflathub\\t3.0.21\\tstable\\tsystem\\n'",
            )
            .expect("write stub");
        let apps = FlatpakClient::with_bin(bin)
            .list_flatpaks()
            .await
            .expect("list");
        assert_eq!(apps.len(), 2);
        assert_eq!(apps[1].app_id, "org.videolan.VLC");
        assert_eq!(apps[1].version, "3.0.21");
    }

    #[tokio::test]
    async fn stub_binary_empty_stdout_is_empty_list() {
        let sandbox = Sandbox::new().await;
        let bin = sandbox.stub("flatpak", "exit 0").expect("write stub");
        let apps = FlatpakClient::with_bin(bin)
            .list_flatpaks()
            .await
            .expect("list");
        assert!(apps.is_empty());
    }

    #[tokio::test]
    async fn stub_binary_nonzero_exit_surfaces_stderr() {
        let sandbox = Sandbox::new().await;
        let bin = sandbox
            .stub(
                "flatpak",
                "echo 'error: failed to load /var/lib/flatpak' >&2\nexit 1",
            )
            .expect("write stub");
        let err = FlatpakClient::with_bin(bin)
            .list_flatpaks()
            .await
            .expect_err("non-zero exit must fail");
        // 后端存在但执行失败：必须是普通错误（非 BackendUnavailable），且带上 stderr。
        assert!(!matches!(err, AgentShellError::BackendUnavailable(_)));
        let msg = err.to_string();
        assert!(
            msg.contains("error: failed to load /var/lib/flatpak"),
            "{msg}"
        );
    }

    #[tokio::test]
    async fn missing_binary_is_backend_unavailable() {
        let sandbox = Sandbox::new().await;
        let bin = sandbox.path().join("flatpak");
        let err = FlatpakClient::with_bin(bin)
            .list_flatpaks()
            .await
            .expect_err("missing binary must fail");
        assert!(matches!(err, AgentShellError::BackendUnavailable(_)));
        assert!(err.to_string().contains("flatpak not installed"));
    }

    /// 回归用例：闸门一旦从 [`Sandbox`] 的构造入口消失，同一 stub 路径上的并发
    /// 「写 → chmod → spawn」就会命中 ETXTBSY。只断言持闸门轮次的命中计数为 0，
    /// 不以无闸门基线为断言。
    #[tokio::test(flavor = "multi_thread", worker_threads = 8)]
    async fn concurrent_stub_writes_never_hit_text_file_busy() {
        let shared = Arc::new(tempfile::tempdir().expect("tempdir"));
        let hits = Arc::new(AtomicUsize::new(0));
        let mut workers = Vec::with_capacity(STRESS_WORKERS);
        for _ in 0..STRESS_WORKERS {
            let shared = Arc::clone(&shared);
            let hits = Arc::clone(&hits);
            workers.push(tokio::spawn(async move {
                for _ in 0..STRESS_ROUNDS_PER_WORKER {
                    // 每轮经构造入口取闸门：闸门缺失时不同 worker 的写与执行重叠。
                    let sandbox = Sandbox::with_dir(Arc::clone(&shared)).await;
                    let bin = match sandbox.stub("flatpak", "exit 0") {
                        Ok(bin) => bin,
                        Err(e) if is_text_file_busy(&e) => {
                            hits.fetch_add(1, Ordering::Relaxed);
                            continue;
                        }
                        Err(e) => panic!("write stub: {e}"),
                    };
                    if let Err(e) = tokio::process::Command::new(&bin)
                        .arg("list")
                        .output()
                        .await
                    {
                        assert!(is_text_file_busy(&e), "spawn stub: {e}");
                        hits.fetch_add(1, Ordering::Relaxed);
                    }
                }
            }));
        }
        for worker in workers {
            worker.await.expect("stress worker panicked");
        }
        assert_eq!(
            hits.load(Ordering::Relaxed),
            0,
            "持闸门时同一 stub 路径的并发写与执行仍命中 ETXTBSY"
        );
    }
}
