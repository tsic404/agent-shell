//! hyprctl socket IPC 通道（设计文档 §9.2）。
//!
//! Hyprland 同步求值：每个请求「连接即开即关」——写完命令立即 shutdown
//! 写端再读完整响应，绝不复用长连接（长连不关会让 compositor 等待 EOF
//! 而阻塞整个会话）。
//!
//! 响应形态：`-j` 后缀命令返回 JSON；`dispatch ...`/`/keyword` 返回 `ok`
//! 纯文本。[`Hyprctl::request`] 只负责 JSON 形态，非 JSON 应答经
//! [`Hyprctl::request_raw`]。

use std::path::{Path, PathBuf};

use serde_json::Value;
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::UnixStream;

use agent_shell_core::error::{AgentShellError, Result};

/// hyprctl 默认超时（秒）：同步求值下超时即视为会话不可达。
const REQUEST_TIMEOUT_SECS: u64 = 2;
/// 瞬时错误重试次数（Agent Identity 重试节律 0s/5s/15s 的收敛版：IPC 内
/// 只做短间隔重试，跨调用退避由上层降级链负责）。
const REQUEST_RETRIES: usize = 2;

/// hyprctl 同步请求通道（`.socket.sock`）。
#[derive(Clone, Debug)]
pub struct Hyprctl {
    /// hyprctl 请求 socket 路径。
    socket_path: PathBuf,
    /// 事件流 socket 路径（`.socket2.sock`，doctor / [`super::event_socket`] 用）。
    event_socket_path: PathBuf,
    /// HYPRLAND_INSTANCE_SIGNATURE（doctor 输出）。
    his: String,
}

impl Hyprctl {
    /// 从会话环境构造（`HYPRLAND_INSTANCE_SIGNATURE` + `XDG_RUNTIME_DIR`）。
    ///
    /// 非 Hyprland 会话返回 [`AgentShellError::BackendUnavailable`]——
    /// doctor 与降级链据此判定本后端不适用。
    pub fn new() -> Result<Self> {
        let his = std::env::var("HYPRLAND_INSTANCE_SIGNATURE")
            .map_err(|_| AgentShellError::BackendUnavailable("NOT in Hyprland session".into()))?;
        let runtime = std::env::var("XDG_RUNTIME_DIR").map_err(|_| {
            AgentShellError::BackendUnavailable(
                "XDG_RUNTIME_DIR not set (no Hyprland session?)".into(),
            )
        })?;
        let base = Path::new(&runtime).join("hypr").join(&his);
        Ok(Self {
            socket_path: base.join(".socket.sock"),
            event_socket_path: base.join(".socket2.sock"),
            his,
        })
    }

    /// 发送命令并读回原始文本（即开即关 + 超时 + 瞬时错误重试）。
    async fn request_raw(&self, cmd: &str) -> Result<String> {
        let mut last_err = None;
        for _ in 0..=REQUEST_RETRIES {
            match self.request_once(cmd).await {
                Ok(text) => return Ok(text),
                Err(e) => last_err = Some(e),
            }
        }
        Err(last_err.unwrap_or_else(|| {
            AgentShellError::BackendUnavailable("hyprctl request failed".into())
        }))
    }

    /// 测试注入路径构造（socket 探测单测用）。
    #[cfg(test)]
    fn with_paths(socket_path: PathBuf, event_socket_path: PathBuf, his: String) -> Self {
        Self {
            socket_path,
            event_socket_path,
            his,
        }
    }

    /// hyprctl 请求 socket 路径（doctor 输出）。
    pub fn socket_path(&self) -> &Path {
        &self.socket_path
    }

    /// 事件流 socket 路径（`.socket2.sock`）。
    pub fn event_socket_path(&self) -> &Path {
        &self.event_socket_path
    }

    /// HYPRLAND_INSTANCE_SIGNATURE。
    pub fn instance_signature(&self) -> &str {
        &self.his
    }

    /// 单次请求：connect → write → shutdown 写端 → read_to_string。
    ///
    /// shutdown 是关键步骤：Hyprland 以 EOF 判定命令发送完毕并开始求值，
    /// 不关闭写端会双向死锁（§9.2「连接必须即开即关」）。
    async fn request_once(&self, cmd: &str) -> Result<String> {
        let mut stream = tokio::time::timeout(
            std::time::Duration::from_secs(REQUEST_TIMEOUT_SECS),
            UnixStream::connect(&self.socket_path),
        )
        .await
        .map_err(|_| AgentShellError::BackendUnavailable("hyprctl connect timeout".into()))?
        .map_err(|e| AgentShellError::BackendUnavailable(format!("hyprctl connect: {e}")))?;

        tokio::time::timeout(
            std::time::Duration::from_secs(REQUEST_TIMEOUT_SECS),
            async {
                stream.write_all(cmd.as_bytes()).await.map_err(|e| {
                    AgentShellError::BackendUnavailable(format!("hyprctl write: {e}"))
                })?;
                stream.shutdown().await.map_err(|e| {
                    AgentShellError::BackendUnavailable(format!("hyprctl shutdown: {e}"))
                })?;
                let mut buf = String::new();
                stream.read_to_string(&mut buf).await.map_err(|e| {
                    AgentShellError::BackendUnavailable(format!("hyprctl read: {e}"))
                })?;
                Ok::<String, AgentShellError>(buf)
            },
        )
        .await
        .map_err(|_| AgentShellError::BackendUnavailable("hyprctl request timeout".into()))?
    }

    /// 发送命令并把响应解析为 JSON（`-j` 命令族）。
    pub async fn request(&self, cmd: &str) -> Result<Value> {
        let text = self.request_raw(cmd).await?;
        serde_json::from_str(&text)
            .map_err(|e| AgentShellError::BackendUnavailable(format!("hyprctl bad json: {e}")))
    }

    /// 执行 dispatch 动作（`ok` 应答即成功）。
    pub async fn dispatch(&self, action: &str) -> Result<()> {
        let text = self.request_raw(&format!("dispatch {action}")).await?;
        expect_ok(&text, action)
    }

    /// 设置 keyword 配置项（如 `misc:focus_follows_mouse 1`）。
    pub async fn set_keyword(&self, keyword: &str) -> Result<()> {
        let text = self.request_raw(&format!("/keyword {keyword}")).await?;
        expect_ok(&text, keyword)
    }

    /// 活动窗口列表（`clients -j`，含 geometry 的完整 JSON）。
    pub async fn clients(&self) -> Result<Value> {
        self.request("clients -j").await
    }

    /// 监视器列表（`monitors -j`）。
    pub async fn monitors(&self) -> Result<Value> {
        self.request("monitors -j").await
    }

    /// 工作区列表（`workspaces -j`）。
    pub async fn workspaces(&self) -> Result<Value> {
        self.request("workspaces -j").await
    }

    /// 版本信息（`version -j`；版本探测后备通道）。
    pub async fn version(&self) -> Result<Value> {
        self.request("version -j").await
    }

    /// 聚焦窗口（address 为不带 0x 前缀的十六进制地址）。
    pub async fn focus_window(&self, address: &str) -> Result<()> {
        self.dispatch(&format!("focuswindow address:0x{address}"))
            .await
    }

    /// 移动窗口到精确坐标（先浮窗化再 movewindowpixel，§9.2 同款语义）。
    pub async fn move_window(&self, address: &str, x: i32, y: i32) -> Result<()> {
        self.dispatch(&format!("setfloating address:0x{address}"))
            .await?;
        self.dispatch(&format!(
            "movewindowpixel exact {x} {y},address:0x{address}"
        ))
        .await
    }

    /// 缩放窗口到精确尺寸（操作选择矩阵：缩放走 hyprctl）。
    pub async fn resize_window(&self, address: &str, w: i32, h: i32) -> Result<()> {
        self.dispatch(&format!("setfloating address:0x{address}"))
            .await?;
        self.dispatch(&format!(
            "resizewindowpixel exact {w} {h},address:0x{address}"
        ))
        .await
    }

    /// 关闭窗口。
    pub async fn close_window(&self, address: &str) -> Result<()> {
        self.dispatch(&format!("closewindow address:0x{address}"))
            .await
    }

    /// 激活工作区。
    pub async fn activate_workspace(&self, id: i32) -> Result<()> {
        self.dispatch(&format!("workspace {id}")).await
    }

    /// 连通性探测（doctor）：一次最小往返，返回耗时毫秒数。
    pub async fn ping_ms(&self) -> Result<f64> {
        let start = std::time::Instant::now();
        // activewindow -j 是最小开销的合法 JSON 命令。
        self.request("activewindow -j").await?;
        Ok(start.elapsed().as_secs_f64() * 1000.0)
    }
}

/// `ok` 应答校验：hyprctl 对 dispatch/keyword 成功返回字面量 ok，
/// 失败返回 `invalid dispatcher` 等诊断文本。
fn expect_ok(text: &str, what: &str) -> Result<()> {
    if text.trim().eq_ignore_ascii_case("ok") {
        Ok(())
    } else {
        Err(AgentShellError::BackendUnavailable(format!(
            "hyprctl `{what}` rejected: {}",
            text.trim()
        )))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Unix socket `sockaddr_un.sun_path` 容量（含结尾 NUL）：Linux 为 108
    /// 字节，macOS 为 104。按平台取值，避免 macOS 上 104–107 字节路径漏检
    /// 后在 `bind` 才报 SUN_LEN 超限。
    #[cfg(target_os = "macos")]
    const SUN_PATH_LEN: usize = 104;
    #[cfg(not(target_os = "macos"))]
    const SUN_PATH_LEN: usize = 108;

    /// 构造测试 socket 目录：返回「最长 socket 路径（`.socket2.sock`）短于
    /// [`SUN_PATH_LEN`] 且可写」的目录。
    ///
    /// CI/真机的 `TMPDIR` 常指向极深的任务工作目录，直接拼接会顶爆
    /// `sun_path` 让 `bind` 报 "path must be shorter than SUN_LEN"。这里
    /// 优先 `temp_dir()`，超限时依次探测 `/tmp`、`target/test-tmp`，取首个
    /// 「长度达标且可创建」者——不假设任一候选必然存在或可写。
    fn temp_his(tag: &str) -> (PathBuf, String) {
        let leaf = format!("agent-shell-hyprctl-{tag}-{}", std::process::id());
        let his = String::from("t1");

        let mut candidates: Vec<PathBuf> = vec![std::env::temp_dir()];
        let tmp = PathBuf::from("/tmp");
        if candidates[0] != tmp {
            candidates.push(tmp);
        }
        if let Ok(cwd) = std::env::current_dir() {
            candidates.push(cwd.join("target").join("test-tmp"));
        }

        for base in candidates {
            let dir = base.join(&leaf);
            // 以最长的 socket 名 `.socket2.sock`（13 字节）衡量，兼顾更短的
            // `.socket.sock`（12 字节），避免 107 字节边界的 off-by-one。
            let socket2 = dir.join("hypr").join(&his).join(".socket2.sock");
            if socket2.as_os_str().as_encoded_bytes().len() >= SUN_PATH_LEN {
                continue;
            }
            if std::fs::create_dir_all(dir.join("hypr").join(&his)).is_ok() {
                return (dir, his);
            }
        }

        panic!(
            "no usable short socket dir (tried temp_dir, /tmp, target/test-tmp); \
             socket path must stay below {SUN_PATH_LEN} bytes"
        );
    }

    /// 验收标准：非 Hyprland 会话下 new() 报 BackendUnavailable。
    ///
    /// 不操作进程级环境变量（竞态风险），而是验证 new() 的错误变体类型：
    /// 在测试环境若有 HIS 则 new() 可能成功，此时跳过断言；
    /// 若无 HIS 则必须报 BackendUnavailable。
    #[test]
    fn missing_his_is_backend_unavailable() {
        match std::env::var("HYPRLAND_INSTANCE_SIGNATURE") {
            Ok(_) => {
                // 测试环境恰在 Hyprland 会话——new() 成功，跳过断言。
            }
            Err(_) => {
                let r = Hyprctl::new();
                assert!(matches!(
                    &r,
                    Err(AgentShellError::BackendUnavailable(m)) if m.contains("Hyprland")
                ));
            }
        }
    }

    /// socket 路径拼装：$XDG_RUNTIME_DIR/hypr/<HIS>/.socket(.2).sock。
    #[test]
    fn socket_paths_follow_instance_layout() {
        let (dir, his) = temp_his("paths");
        let ctl = Hyprctl::with_paths(
            dir.join("hypr").join(&his).join(".socket.sock"),
            dir.join("hypr").join(&his).join(".socket2.sock"),
            his,
        );
        assert!(ctl.socket_path().ends_with("hypr/t1/.socket.sock"));
        assert!(ctl.event_socket_path().ends_with("hypr/t1/.socket2.sock"));
        assert_eq!(ctl.instance_signature(), "t1");
    }

    /// dispatch 非 ok 应答必须报错（防把诊断文本当成功吞掉）。
    #[test]
    fn non_ok_dispatch_text_is_rejected() {
        assert!(expect_ok("ok\n", "workspace 2").is_ok());
        assert!(expect_ok("invalid dispatcher\n", "nope").is_err());
    }

    /// 即开即关行为验证：起本地 UDS 服务端，断言服务端读到 EOF（客户端
    /// 已 shutdown 写端）后才回写响应，且客户端拿到 JSON。
    #[tokio::test]
    async fn request_shuts_down_write_side_then_reads() {
        let (dir, his) = temp_his("echo");
        let sock = dir.join("hypr").join(&his).join(".socket.sock");
        let listener = tokio::net::UnixListener::bind(&sock).expect("bind");
        let server = tokio::spawn(async move {
            let (mut s, _) = listener.accept().await.expect("accept");
            let mut buf = Vec::new();
            // read_to_end 直到客户端 shutdown——这正是 Hyprland 的求值时机。
            s.read_to_end(&mut buf).await.expect("read");
            assert_eq!(buf, b"clients -j");
            s.write_all(b"[{\"address\":\"0x1\"}]").await.ok();
        });

        let ctl = Hyprctl::with_paths(sock.clone(), sock.with_file_name(".socket2.sock"), his);
        let v = ctl.request("clients -j").await.expect("request");
        assert_eq!(v[0]["address"], "0x1");
        server.await.expect("server task");
    }

    /// 无服务端时请求在超时/重试预算内失败为 BackendUnavailable（不 panic、
    /// 不无限挂起）。
    #[tokio::test]
    async fn unreachable_socket_fails_cleanly() {
        let (dir, his) = temp_his("dead");
        let ctl = Hyprctl::with_paths(
            dir.join("hypr").join(&his).join(".socket.sock"),
            dir.join("hypr").join(&his).join(".socket2.sock"),
            his,
        );
        let r = ctl.request("clients -j").await;
        assert!(r.is_err());
    }
}
