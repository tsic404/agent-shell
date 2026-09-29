//! 默认应用与 MIME 类型组件（设计文档 §21.27「默认应用与 MIME 类型」）。
//!
//! [`MimeOps`] 是能力契约（daemon 持 `dyn MimeOps`，测试注入 fake）。公开
//! 实现 [`XdgMimeService`] 走 freedesktop 命令行接口：查询默认应用
//! `xdg-mime query default <mime>`、查询默认浏览器 `xdg-settings get
//! default-web-browser`；二进制缺失时回退解析 `mimeapps.list`。
//!
//! 错误分类：查询二进制缺失（NotFound）→ 回退 `mimeapps.list`，两处皆无 →
//! [`AgentShellError::BackendUnavailable`]；二进制在位但非零退出 → 带底层
//! stderr 的 [`AgentShellError::Other`]；命令卡死 → [`AgentShellError::Timeout`]。
//! 「在位但失败」不得回退，否则会把用户配置错误静默成另一套数据源。

use agent_shell_core::error::{AgentShellError, Result};
use agent_shell_core::services::DefaultAppResolution;
use async_trait::async_trait;
use std::io::ErrorKind;
use std::path::{Path, PathBuf};
use std::time::Duration;

/// 查询命令超时：xdg 脚本会在挂起的 desktop 目录上无限阻塞。
const QUERY_TIMEOUT: Duration = Duration::from_secs(5);

/// `mimeapps.list` 文件名（位于 `$XDG_CONFIG_HOME`，即 `~/.config`）。
const MIMEAPPS_FILE: &str = "mimeapps.list";

/// 仅此段声明默认应用；`[Added Associations]` 等段不参与默认解析。
const DEFAULT_APPLICATIONS_SECTION: &str = "Default Applications";

/// 默认浏览器的 target 固定值（对应 rpc 的 `mime.default_browser`）。
const WEB_BROWSER_TARGET: &str = "web-browser";

/// 浏览器回退键序：先 https 再 http 再 text/html。
const BROWSER_KEYS: &[&str] = &[
    "x-scheme-handler/https",
    "x-scheme-handler/http",
    "text/html",
];

/// 数据来源标识：`xdg-mime query default`。
const XDG_MIME_SOURCE: &str = "xdg-mime";
/// 数据来源标识：`xdg-settings get default-web-browser`。
const XDG_SETTINGS_SOURCE: &str = "xdg-settings";
/// 数据来源标识：`mimeapps.list` 解析回退。
const MIMEAPPS_SOURCE: &str = "mimeapps.list";

/// 默认应用查询契约（§21.27）。
#[async_trait]
pub trait MimeOps: Send + Sync {
    /// 查询 MIME 类型的默认应用；未配置时 `desktop_id` 为 None。
    async fn default_app(&self, mime: &str) -> Result<DefaultAppResolution>;
    /// 查询默认浏览器（`x-scheme-handler/https` → `http` → `text/html`）。
    async fn default_browser(&self) -> Result<DefaultAppResolution>;
}

/// xdg 命令行 + `mimeapps.list` 回退的默认应用服务。
pub struct XdgMimeService {
    /// `xdg-mime` 可执行文件（测试注入脚本路径）。
    xdg_mime: PathBuf,
    /// `xdg-settings` 可执行文件（测试注入脚本路径）。
    xdg_settings: PathBuf,
    /// `mimeapps.list` 所在目录（`$XDG_CONFIG_HOME`）。
    config_home: PathBuf,
}

impl XdgMimeService {
    /// 生产装配：两个二进制按 PATH 查找，配置目录取 `$XDG_CONFIG_HOME`
    /// 或 `$HOME/.config`。构造不探测，后端缺失在各次查询时降级处理。
    pub async fn new() -> Self {
        Self {
            xdg_mime: PathBuf::from("xdg-mime"),
            xdg_settings: PathBuf::from("xdg-settings"),
            config_home: config_home_from_env(),
        }
    }

    /// 测试装配：注入两个二进制路径与配置目录，避免测试读写进程环境变量。
    #[doc(hidden)]
    pub fn for_test(
        xdg_mime: impl Into<PathBuf>,
        xdg_settings: impl Into<PathBuf>,
        config_home: impl Into<PathBuf>,
    ) -> Self {
        Self {
            xdg_mime: xdg_mime.into(),
            xdg_settings: xdg_settings.into(),
            config_home: config_home.into(),
        }
    }

    /// 查询二进制缺失时回退 `mimeapps.list`；`bin` 仅用于错误信息中指明缺谁。
    async fn mimeapps_fallback(
        &self,
        bin: &Path,
        target: &str,
        keys: &[&str],
    ) -> Result<DefaultAppResolution> {
        tracing::debug!(
            binary = %bin.display(),
            "query binary missing; falling back to mimeapps.list"
        );
        let file = self.config_home.join(MIMEAPPS_FILE);
        match tokio::fs::read_to_string(&file).await {
            Ok(content) => Ok(DefaultAppResolution {
                target: target.to_string(),
                desktop_id: parse_mimeapps_defaults(&content, keys),
                source: MIMEAPPS_SOURCE.into(),
            }),
            // 两个后端都没有：既不缺库也不缺配置，而是没有可用的数据源。
            Err(e) if e.kind() == ErrorKind::NotFound => Err(AgentShellError::BackendUnavailable(
                format!("{} not installed and no {}", bin.display(), file.display()),
            )),
            Err(e) => Err(AgentShellError::Other(
                format!("read {}: {e}", file.display()).into(),
            )),
        }
    }
}

#[async_trait]
impl MimeOps for XdgMimeService {
    async fn default_app(&self, mime: &str) -> Result<DefaultAppResolution> {
        match run_query(&self.xdg_mime, &["query", "default", mime]).await {
            Ok(stdout) => Ok(DefaultAppResolution {
                target: mime.to_string(),
                desktop_id: non_empty_trimmed(&stdout),
                source: XDG_MIME_SOURCE.into(),
            }),
            Err(QueryError::Missing) => self.mimeapps_fallback(&self.xdg_mime, mime, &[mime]).await,
            Err(QueryError::Failed(e)) => Err(e),
        }
    }

    async fn default_browser(&self) -> Result<DefaultAppResolution> {
        match run_query(&self.xdg_settings, &["get", "default-web-browser"]).await {
            Ok(stdout) => Ok(DefaultAppResolution {
                target: WEB_BROWSER_TARGET.into(),
                desktop_id: non_empty_trimmed(&stdout),
                source: XDG_SETTINGS_SOURCE.into(),
            }),
            Err(QueryError::Missing) => {
                self.mimeapps_fallback(&self.xdg_settings, WEB_BROWSER_TARGET, BROWSER_KEYS)
                    .await
            }
            Err(QueryError::Failed(e)) => Err(e),
        }
    }
}

/// 单次查询的失败归因：只有二进制缺失才允许回退 `mimeapps.list`。
enum QueryError {
    /// 查询二进制不存在（PATH 无此程序，或注入的脚本路径不存在）。
    Missing,
    /// 后端在位但执行失败（非零退出、spawn 失败、超时）。
    Failed(AgentShellError),
}

/// 执行查询命令并返回 stdout（仅成功时）。
///
/// `kill_on_drop` 保证超时后子进程被回收，避免留下孤儿进程卡住调用方退出。
async fn run_query(bin: &Path, args: &[&str]) -> std::result::Result<String, QueryError> {
    let mut cmd = tokio::process::Command::new(bin);
    cmd.args(args).kill_on_drop(true);
    match tokio::time::timeout(QUERY_TIMEOUT, cmd.output()).await {
        Err(_) => Err(QueryError::Failed(AgentShellError::Timeout(format!(
            "{} {} timed out after {QUERY_TIMEOUT:?}",
            bin.display(),
            args.join(" ")
        )))),
        Ok(Err(e)) if e.kind() == ErrorKind::NotFound => Err(QueryError::Missing),
        Ok(Err(e)) => Err(QueryError::Failed(AgentShellError::Other(
            format!("{} spawn: {e}", bin.display()).into(),
        ))),
        Ok(Ok(out)) if out.status.success() => {
            Ok(String::from_utf8_lossy(&out.stdout).into_owned())
        }
        Ok(Ok(out)) => Err(QueryError::Failed(AgentShellError::Other(
            format!(
                "{} {}: {}",
                bin.display(),
                args.join(" "),
                String::from_utf8_lossy(&out.stderr).trim()
            )
            .into(),
        ))),
    }
}

/// 解析 `mimeapps.list`，按 `keys` 的给定优先级返回 `[Default Applications]` 段
/// 中命中的 desktop id。
///
/// 取值取 `;` 分隔列表的第一个非空项（GNOME/KDE 写出的值常带尾分号）。键序优先
/// 于文件行序：浏览器回退 https → http → text/html 不能因文件排布而改变。文件、
/// 段、键缺失，或值全为空列表 → None（表示"查得到、但未配置"）。
fn parse_mimeapps_defaults(content: &str, keys: &[&str]) -> Option<String> {
    let mut entries: Vec<(&str, &str)> = Vec::new();
    let mut in_defaults = false;
    for line in content.lines() {
        let line = line.trim();
        if line.is_empty() || line.starts_with('#') {
            continue;
        }
        if let Some(section) = line.strip_prefix('[').and_then(|l| l.strip_suffix(']')) {
            in_defaults = section.trim() == DEFAULT_APPLICATIONS_SECTION;
            continue;
        }
        if !in_defaults {
            continue;
        }
        let Some((key, value)) = line.split_once('=') else {
            continue;
        };
        let Some(id) = value.split(';').map(str::trim).find(|s| !s.is_empty()) else {
            continue;
        };
        entries.push((key.trim(), id));
    }
    keys.iter().find_map(|want| {
        entries
            .iter()
            .find(|(key, _)| key == want)
            .map(|(_, id)| (*id).to_string())
    })
}

/// 生产配置目录：`$XDG_CONFIG_HOME`，否则 `$HOME/.config`。
///
/// 两者都缺失时退化为空路径（相对名 `mimeapps.list`），不 panic。
fn config_home_from_env() -> PathBuf {
    if let Some(dir) = non_empty_env("XDG_CONFIG_HOME") {
        return PathBuf::from(dir);
    }
    match non_empty_env("HOME") {
        Some(home) => PathBuf::from(home).join(".config"),
        None => PathBuf::new(),
    }
}

/// 读取环境变量，空串按未设置处理（`XDG_CONFIG_HOME=` 会把配置目录拼成相对名）。
fn non_empty_env(key: &str) -> Option<std::ffi::OsString> {
    std::env::var_os(key).filter(|v| !v.is_empty())
}

/// 查询输出归一化：trim 后为空 → None（命令成功但未配置默认应用）。
fn non_empty_trimmed(stdout: &str) -> Option<String> {
    let trimmed = stdout.trim();
    (!trimmed.is_empty()).then(|| trimmed.to_string())
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::os::unix::fs::PermissionsExt;

    /// 全局 fork 互斥：fork 会复制其它线程持有的写 fd，若此时某 stub 尚被写入，
    /// 别的线程 execve 它就会间歇性失败（ETXTBSY）。该锁覆盖"写脚本 → 跑查询"
    /// 全程，于是任何 fork 发生时都不存在指向 stub 的写 fd。
    static FORK_GUARD: tokio::sync::Mutex<()> = tokio::sync::Mutex::const_new(());

    /// 测试沙箱：持 fork 互斥锁 + 私有临时目录，需要 fork 的用例一律经它搭建。
    struct Sandbox {
        _fork_guard: tokio::sync::MutexGuard<'static, ()>,
        dir: tempfile::TempDir,
    }

    impl Sandbox {
        async fn new() -> Self {
            Self {
                _fork_guard: FORK_GUARD.lock().await,
                dir: tempfile::tempdir().unwrap(),
            }
        }

        fn path(&self) -> &Path {
            self.dir.path()
        }

        /// 沙箱内建子目录（同一用例需要多个配置目录时用）。
        fn subdir(&self, name: &str) -> PathBuf {
            let path = self.dir.path().join(name);
            std::fs::create_dir_all(&path).unwrap();
            path
        }

        /// 写可执行 stub 脚本，代替真实 xdg 二进制。
        ///
        /// 先写临时名再 rename：最终路径出现时写 fd 已关闭，消除"路径可见但仍
        /// 被写打开"的窗口。rename 不改 inode，跨线程 fork 复制该 fd 的风险由
        /// [`FORK_GUARD`] 兜住。
        fn stub(&self, name: &str, body: &str) -> PathBuf {
            let path = self.dir.path().join(name);
            let staged = self.dir.path().join(format!("{name}.staged"));
            std::fs::write(&staged, format!("#!/bin/sh\n{body}\n")).unwrap();
            std::fs::set_permissions(&staged, std::fs::Permissions::from_mode(0o755)).unwrap();
            std::fs::rename(&staged, &path).unwrap();
            path
        }

        /// 在给定配置目录写 `mimeapps.list`。
        fn write_mimeapps(&self, config_home: &Path, content: &str) {
            std::fs::write(config_home.join(MIMEAPPS_FILE), content).unwrap();
        }

        /// 两个二进制都不存在的服务（回退路径专用）。
        fn service_without_binaries(&self, cfg: &Path) -> XdgMimeService {
            XdgMimeService::for_test(cfg.join("no-xdg-mime"), cfg.join("no-xdg-settings"), cfg)
        }
    }

    #[test]
    fn parse_defaults_takes_first_non_empty_entry() {
        let content = "[Default Applications]\ntext/html=a.desktop;b.desktop;\n";
        assert_eq!(
            parse_mimeapps_defaults(content, &["text/html"]).as_deref(),
            Some("a.desktop")
        );
        // 空列表值等同于未配置该键。
        assert_eq!(
            parse_mimeapps_defaults("[Default Applications]\ntext/html=;;\n", &["text/html"]),
            None
        );
    }

    #[test]
    fn parse_defaults_only_reads_default_applications_section() {
        // 关联段里的同名键不是默认应用，误取会给出错误答案。
        let added_only = "[Added Associations]\ntext/html=wrong.desktop;\n";
        assert_eq!(parse_mimeapps_defaults(added_only, &["text/html"]), None);
        let mixed = "[Default Applications]\ntext/html=right.desktop;\n\
                     [Added Associations]\ntext/html=wrong.desktop;\n";
        assert_eq!(
            parse_mimeapps_defaults(mixed, &["text/html"]).as_deref(),
            Some("right.desktop")
        );
    }

    #[test]
    fn parse_defaults_absent_key_or_content_is_none() {
        let content = "[Default Applications]\ntext/plain=editor.desktop;\n";
        assert_eq!(parse_mimeapps_defaults(content, &["text/html"]), None);
        assert_eq!(parse_mimeapps_defaults("", &["text/html"]), None);
        assert_eq!(
            parse_mimeapps_defaults("# 只有注释\n", &["text/html"]),
            None
        );
    }

    #[test]
    fn parse_defaults_tolerates_crlf_comments_and_blank_lines() {
        let content = "# 头部注释\r\n\r\n[Default Applications]\r\n# 段内注释\r\n\
                       \r\ntext/html=firefox.desktop;\r\n";
        assert_eq!(
            parse_mimeapps_defaults(content, &["text/html"]).as_deref(),
            Some("firefox.desktop")
        );
    }

    #[test]
    fn parse_defaults_applies_browser_fallback_order() {
        let all = "[Default Applications]\nx-scheme-handler/https=https.desktop;\n\
                   x-scheme-handler/http=http.desktop;\ntext/html=html.desktop;\n";
        assert_eq!(
            parse_mimeapps_defaults(all, BROWSER_KEYS).as_deref(),
            Some("https.desktop")
        );
        let no_https = "[Default Applications]\nx-scheme-handler/http=http.desktop;\n\
                        text/html=html.desktop;\n";
        assert_eq!(
            parse_mimeapps_defaults(no_https, BROWSER_KEYS).as_deref(),
            Some("http.desktop")
        );
        let html_only = "[Default Applications]\ntext/html=html.desktop;\n";
        assert_eq!(
            parse_mimeapps_defaults(html_only, BROWSER_KEYS).as_deref(),
            Some("html.desktop")
        );
        // 键序优先于文件行序：text/html 虽在文件前面也排在 https 之后。
        let inverted = "[Default Applications]\ntext/html=html.desktop;\n\
                        x-scheme-handler/https=primary.desktop;\n";
        assert_eq!(
            parse_mimeapps_defaults(inverted, BROWSER_KEYS).as_deref(),
            Some("primary.desktop")
        );
        // 高优先键值为空列表时不吞掉后续键。
        let empty_https = "[Default Applications]\nx-scheme-handler/https=;\n\
                           x-scheme-handler/http=http.desktop;\n";
        assert_eq!(
            parse_mimeapps_defaults(empty_https, BROWSER_KEYS).as_deref(),
            Some("http.desktop")
        );
    }

    #[tokio::test]
    async fn default_app_reports_xdg_mime_output() {
        let sb = Sandbox::new().await;
        let bin = sb.stub("xdg-mime", "echo firefox.desktop");
        let svc = XdgMimeService::for_test(&bin, sb.path().join("missing"), sb.path());
        let res = svc.default_app("text/html").await.unwrap();
        assert_eq!(res.target, "text/html");
        assert_eq!(res.desktop_id.as_deref(), Some("firefox.desktop"));
        assert_eq!(res.source, "xdg-mime");
    }

    #[tokio::test]
    async fn default_app_empty_output_means_unconfigured() {
        let sb = Sandbox::new().await;
        let bin = sb.stub("xdg-mime", "exit 0");
        let svc = XdgMimeService::for_test(&bin, sb.path().join("missing"), sb.path());
        let res = svc.default_app("text/html").await.unwrap();
        assert_eq!(res.desktop_id, None);
        assert_eq!(res.source, "xdg-mime");
    }

    #[tokio::test]
    async fn default_app_non_zero_exit_reports_stderr() {
        let sb = Sandbox::new().await;
        // 存在可用的 mimeapps.list，用来证明"在位但失败"不回退。
        sb.write_mimeapps(
            sb.path(),
            "[Default Applications]\ntext/html=fallback.desktop;\n",
        );
        let bin = sb.stub("xdg-mime", "echo 'boom: no such mime' >&2; exit 1");
        let svc = XdgMimeService::for_test(&bin, sb.path().join("missing"), sb.path());
        let err = svc.default_app("text/html").await.unwrap_err();
        assert!(matches!(&err, AgentShellError::Other(_)), "got {err:?}");
        assert!(err.to_string().contains("boom: no such mime"), "got {err}");
    }

    #[tokio::test]
    async fn default_app_missing_binary_falls_back_to_mimeapps_list() {
        let sb = Sandbox::new().await;
        sb.write_mimeapps(
            sb.path(),
            "[Default Applications]\ntext/html=chromium.desktop;\n",
        );
        let res = sb
            .service_without_binaries(sb.path())
            .default_app("text/html")
            .await
            .unwrap();
        assert_eq!(res.target, "text/html");
        assert_eq!(res.desktop_id.as_deref(), Some("chromium.desktop"));
        assert_eq!(res.source, "mimeapps.list");
    }

    #[tokio::test]
    async fn default_app_missing_binary_and_missing_file_is_backend_unavailable() {
        let sb = Sandbox::new().await;
        let err = sb
            .service_without_binaries(sb.path())
            .default_app("text/html")
            .await
            .unwrap_err();
        let AgentShellError::BackendUnavailable(msg) = &err else {
            panic!("expected BackendUnavailable, got {err:?}");
        };
        assert!(msg.contains("no-xdg-mime"), "got {msg}");
        assert!(msg.contains("mimeapps.list"), "got {msg}");
    }

    #[tokio::test]
    async fn default_browser_reports_xdg_settings_output() {
        let sb = Sandbox::new().await;
        let bin = sb.stub("xdg-settings", "echo firefox.desktop");
        let svc = XdgMimeService::for_test(sb.path().join("missing"), &bin, sb.path());
        let res = svc.default_browser().await.unwrap();
        assert_eq!(res.target, "web-browser");
        assert_eq!(res.desktop_id.as_deref(), Some("firefox.desktop"));
        assert_eq!(res.source, "xdg-settings");
    }

    #[tokio::test]
    async fn default_browser_empty_output_means_unconfigured() {
        let sb = Sandbox::new().await;
        let bin = sb.stub("xdg-settings", "echo ''");
        let svc = XdgMimeService::for_test(sb.path().join("missing"), &bin, sb.path());
        let res = svc.default_browser().await.unwrap();
        assert_eq!(res.desktop_id, None);
        assert_eq!(res.source, "xdg-settings");
    }

    #[tokio::test]
    async fn default_browser_missing_binary_walks_scheme_handler_keys() {
        let sb = Sandbox::new().await;
        let http_only = sb.subdir("http_only");
        sb.write_mimeapps(
            &http_only,
            "[Default Applications]\nx-scheme-handler/http=chromium.desktop;\n\
             text/html=html.desktop;\n",
        );
        let res = sb
            .service_without_binaries(&http_only)
            .default_browser()
            .await
            .unwrap();
        assert_eq!(res.target, "web-browser");
        assert_eq!(res.desktop_id.as_deref(), Some("chromium.desktop"));
        assert_eq!(res.source, "mimeapps.list");

        // https 键存在时优先，即使 text/html 在文件里更靠前。
        let with_https = sb.subdir("with_https");
        sb.write_mimeapps(
            &with_https,
            "[Default Applications]\ntext/html=html.desktop;\n\
             x-scheme-handler/https=firefox.desktop;\n",
        );
        let res = sb
            .service_without_binaries(&with_https)
            .default_browser()
            .await
            .unwrap();
        assert_eq!(res.desktop_id.as_deref(), Some("firefox.desktop"));

        // 只剩 text/html 时也能解析出浏览器。
        let html_only = sb.subdir("html_only");
        sb.write_mimeapps(
            &html_only,
            "[Default Applications]\ntext/html=html.desktop;\n",
        );
        let res = sb
            .service_without_binaries(&html_only)
            .default_browser()
            .await
            .unwrap();
        assert_eq!(res.desktop_id.as_deref(), Some("html.desktop"));
    }

    #[tokio::test]
    async fn default_browser_missing_binary_and_missing_file_is_backend_unavailable() {
        let sb = Sandbox::new().await;
        let err = sb
            .service_without_binaries(sb.path())
            .default_browser()
            .await
            .unwrap_err();
        let AgentShellError::BackendUnavailable(msg) = &err else {
            panic!("expected BackendUnavailable, got {err:?}");
        };
        assert!(msg.contains("no-xdg-settings"), "got {msg}");
        assert!(msg.contains("mimeapps.list"), "got {msg}");
    }
}
