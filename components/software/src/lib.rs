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
    use std::os::unix::fs::PermissionsExt;

    /// 写入可执行的 stub 脚本，返回其路径（临时目录由调用方持有生命周期）。
    fn write_stub(dir: &std::path::Path, body: &str) -> PathBuf {
        let path = dir.join("flatpak");
        std::fs::write(&path, body).expect("write stub");
        let mut perms = std::fs::metadata(&path).expect("stat stub").permissions();
        perms.set_mode(0o755);
        std::fs::set_permissions(&path, perms).expect("chmod stub");
        path
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
        let dir = tempfile::tempdir().expect("tempdir");
        let bin = write_stub(
            dir.path(),
            "#!/bin/sh\nprintf 'org.mozilla.firefox\\tFirefox\\tflathub\\t128.0.3\\tstable\\tsystem\\n'\nprintf 'org.videolan.VLC\\tVLC\\tflathub\\t3.0.21\\tstable\\tsystem\\n'\n",
        );
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
        let dir = tempfile::tempdir().expect("tempdir");
        let bin = write_stub(dir.path(), "#!/bin/sh\nexit 0\n");
        let apps = FlatpakClient::with_bin(bin)
            .list_flatpaks()
            .await
            .expect("list");
        assert!(apps.is_empty());
    }

    #[tokio::test]
    async fn stub_binary_nonzero_exit_surfaces_stderr() {
        let dir = tempfile::tempdir().expect("tempdir");
        let bin = write_stub(
            dir.path(),
            "#!/bin/sh\necho 'error: failed to load /var/lib/flatpak' >&2\nexit 1\n",
        );
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
        let dir = tempfile::tempdir().expect("tempdir");
        let bin = dir.path().join("flatpak");
        let err = FlatpakClient::with_bin(bin)
            .list_flatpaks()
            .await
            .expect_err("missing binary must fail");
        assert!(matches!(err, AgentShellError::BackendUnavailable(_)));
        assert!(err.to_string().contains("flatpak not installed"));
    }
}
