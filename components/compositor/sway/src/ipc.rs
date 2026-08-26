//! Sway IPC 客户端（i3 兼容协议，`ipc.rs`）。
//!
//! Unix domain socket（路径 `$SWAYSOCK`），报文格式为 i3 IPC：
//! 魔数 `"i3-ipc"` + payload 长度(u32 LE) + 类型(u32 LE) + payload。
//! 响应同格式回传（类型字段为对应的 REPLY 值，本层不校验一致性以外的内容）。
//!
//! 连接纪律（hyprland.md §9.2 hyprctl 封装同款约束）：同步求值型命令
//! 即开即关——每次 [`SwayIpc::roundtrip`] 新建连接、发送、读完整响应、
//! 关闭，不在 compositor 主路径上滞留连接。事件订阅是唯一的长连接
//! （[`crate::event`] 自行持有）。

use agent_shell_core::error::AgentShellError;
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::UnixStream;

/// i3 IPC 协议魔数。
const MAGIC: &[u8; 6] = b"i3-ipc";

/// 报文头长度：魔数(6) + 长度(4) + 类型(4)。
const HEADER_LEN: usize = 14;

/// i3 IPC 事件帧类型高位标记（reply_type & EVENT_MASK != 0 即事件帧）。
pub(crate) const EVENT_MASK: u32 = 0x8000_0000;
/// workspace 事件（reply_type = EVENT_MASK | 0）。
pub(crate) const EVENT_WORKSPACE: u32 = 0x8000_0000;
/// window 事件（reply_type = EVENT_MASK | 3）。
pub(crate) const EVENT_WINDOW: u32 = 0x8000_0003;

/// i3 IPC 请求类型（sway-ipc(7)；仅实现本组件用到的子集）。
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum IpcCommand {
    /// 版本探测（REPLY 版本 0）。
    GetVersion,
    /// 工作区列表（REPLY 1）。
    GetWorkspaces,
    /// 输出列表（REPLY 3）。
    GetOutputs,
    /// 窗口树，含 geometry（REPLY 4）。
    GetTree,
    /// dispatch 命令（REPLY 0）。
    RunCommand,
    /// 订阅事件（REPLY 0x8000）。
    Subscribe,
}

impl IpcCommand {
    fn as_u32(self) -> u32 {
        match self {
            Self::GetVersion => 0,
            Self::GetWorkspaces => 1,
            Self::GetOutputs => 3,
            Self::GetTree => 4,
            Self::RunCommand => 0,
            Self::Subscribe => 100,
        }
    }
}

/// Sway IPC 客户端。
///
/// 求值型调用即开即关；构造只解析 socket 路径，不发起连接——
/// 非 Sway 会话下首次调用返回 [`AgentShellError::BackendUnavailable`]。
#[derive(Clone, Debug)]
pub struct SwayIpc {
    socket_path: std::path::PathBuf,
}

impl SwayIpc {
    /// 从 `$SWAYSOCK` 构造客户端。
    ///
    /// 环境变量缺失 → `BackendUnavailable`（非 Sway 会话的正常降级路径，
    /// 不 panic）。`~/.i3.sock` 兜底不做：i3 与 sway 的探测归属各自后端，
    /// 本组件只在明确的 sway 会话装配。
    pub fn from_env() -> Result<Self, AgentShellError> {
        let path = std::env::var_os("SWAYSOCK").ok_or_else(|| {
            AgentShellError::BackendUnavailable("SWAYSOCK not set (not a sway session?)".into())
        })?;
        Ok(Self {
            socket_path: std::path::PathBuf::from(path),
        })
    }

    /// 显式指定 socket 路径（测试与多会话场景）。
    pub fn with_socket_path(path: impl Into<std::path::PathBuf>) -> Self {
        Self {
            socket_path: path.into(),
        }
    }

    /// 发送一条命令并读取完整 JSON 响应（即开即关）。
    pub async fn roundtrip(&self, cmd: IpcCommand, payload: &str) -> Result<serde_json::Value> {
        let mut stream = self.connect().await?;
        write_message(&mut stream, cmd, payload).await?;
        let (_reply_type, body) = read_message(&mut stream).await?;
        serde_json::from_str(&body).map_err(|e| {
            AgentShellError::Other(Box::new(std::io::Error::new(
                std::io::ErrorKind::InvalidData,
                format!("sway ipc: invalid JSON reply: {e}"),
            )))
        })
    }

    /// 发送 RUN_COMMAND 并校验每条命令的成功标记。
    ///
    /// sway 的 RUN_COMMAND 成功时返回 `[{ "success": true }]`；失败条目
    /// 携带 `success: false` + `error`。任一失败即报错并携带 sway 原文。
    pub async fn run_command(&self, command: &str) -> Result<()> {
        let reply = self.roundtrip(IpcCommand::RunCommand, command).await?;
        // 单命令成功时可能是空数组（如纯 `nop`）；有对象则逐条检查 success。
        if let Some(entries) = reply.as_array() {
            for entry in entries {
                if entry.get("success").and_then(serde_json::Value::as_bool) == Some(false) {
                    let err = entry
                        .get("error")
                        .and_then(serde_json::Value::as_str)
                        .unwrap_or("unknown error");
                    return Err(AgentShellError::Other(Box::new(std::io::Error::other(
                        format!("sway ipc: `{command}` failed: {err}"),
                    ))));
                }
            }
        }
        Ok(())
    }

    /// IPC roundtrip 时延（doctor 输出项）。
    pub async fn ping_ms(&self) -> Result<f64> {
        let start = std::time::Instant::now();
        self.roundtrip(IpcCommand::GetVersion, "").await?;
        Ok(start.elapsed().as_secs_f64() * 1000.0)
    }

    /// 打开一条长连接（事件订阅专用；求值型命令走 [`Self::roundtrip`]）。
    pub(crate) async fn connect_stream(&self) -> Result<UnixStream> {
        self.connect().await
    }

    async fn connect(&self) -> Result<UnixStream> {
        UnixStream::connect(&self.socket_path).await.map_err(|e| {
            AgentShellError::BackendUnavailable(format!(
                "sway ipc connect {}: {e}",
                self.socket_path.display()
            ))
        })
    }
}

type Result<T, E = AgentShellError> = std::result::Result<T, E>;

/// 编码并写出一条 i3 IPC 报文。
pub(crate) async fn write_message(
    stream: &mut UnixStream,
    cmd: IpcCommand,
    payload: &str,
) -> std::result::Result<(), AgentShellError> {
    let mut buf = Vec::with_capacity(HEADER_LEN + payload.len());
    buf.extend_from_slice(MAGIC);
    buf.extend_from_slice(&(payload.len() as u32).to_le_bytes());
    buf.extend_from_slice(&cmd.as_u32().to_le_bytes());
    buf.extend_from_slice(payload.as_bytes());
    stream
        .write_all(&buf)
        .await
        .map_err(|e| AgentShellError::DBus(format!("sway ipc write: {e}")))?;
    stream
        .flush()
        .await
        .map_err(|e| AgentShellError::DBus(format!("sway ipc flush: {e}")))
}

/// 读取一条完整 i3 IPC 报文，返回 `(响应类型, payload 字符串)`。
///
/// 长度上界 16 MiB：GET_TREE 在超大窗口树下的实际量级远低于此，
/// 超界视为协议错位而非合法载荷，立即断开防止内存放大。
pub(crate) async fn read_message(
    stream: &mut UnixStream,
) -> std::result::Result<(u32, String), AgentShellError> {
    const MAX_PAYLOAD: u32 = 16 * 1024 * 1024;
    let mut header = [0u8; HEADER_LEN];
    stream
        .read_exact(&mut header)
        .await
        .map_err(|e| AgentShellError::DBus(format!("sway ipc read header: {e}")))?;
    if &header[..6] != MAGIC {
        return Err(AgentShellError::DBus(
            "sway ipc: bad magic (protocol desync)".into(),
        ));
    }
    let len = u32::from_le_bytes(header[6..10].try_into().expect("fixed slice"));
    let reply_type = u32::from_le_bytes(header[10..14].try_into().expect("fixed slice"));
    if len > MAX_PAYLOAD {
        return Err(AgentShellError::DBus(format!(
            "sway ipc: payload {len} exceeds sanity limit"
        )));
    }
    let mut payload = vec![0u8; len as usize];
    stream
        .read_exact(&mut payload)
        .await
        .map_err(|e| AgentShellError::DBus(format!("sway ipc read payload: {e}")))?;
    String::from_utf8(payload)
        .map(|s| (reply_type, s))
        .map_err(|e| AgentShellError::DBus(format!("sway ipc: non-utf8 payload: {e}")))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn request_type_values_match_i3_spec() {
        // sway-ipc(7)：GET_VERSION=0 GET_WORKSPACES=1 GET_OUTPUTS=3
        // GET_TREE=4 RUN_COMMAND=0 SUBSCRIBE=100。
        assert_eq!(IpcCommand::GetVersion.as_u32(), 0);
        assert_eq!(IpcCommand::GetWorkspaces.as_u32(), 1);
        assert_eq!(IpcCommand::GetOutputs.as_u32(), 3);
        assert_eq!(IpcCommand::GetTree.as_u32(), 4);
        assert_eq!(IpcCommand::RunCommand.as_u32(), 0);
        assert_eq!(IpcCommand::Subscribe.as_u32(), 100);
    }

    #[tokio::test]
    async fn message_roundtrip_over_socketpair() {
        // 用 UnixStream::pair 模拟 sway 端：收请求、回一条合法响应。
        let (mut client_side, mut server_side) = UnixStream::pair().expect("socketpair");
        write_message(&mut client_side, IpcCommand::GetVersion, "{\"\":1}")
            .await
            .expect("write");

        let mut header = [0u8; HEADER_LEN];
        server_side.read_exact(&mut header).await.expect("hdr");
        assert_eq!(&header[..6], MAGIC);
        let len = u32::from_le_bytes(header[6..10].try_into().unwrap()) as usize;
        let mut payload = vec![0u8; len];
        server_side.read_exact(&mut payload).await.expect("payload");
        assert_eq!(payload, b"{\"\":1}");

        // 回包：魔数 + 长度 + 类型(REPLY_GET_VERSION=0) + 体。
        let body = br#"{"major":1}"#;
        let mut reply = Vec::new();
        reply.extend_from_slice(MAGIC);
        reply.extend_from_slice(&(body.len() as u32).to_le_bytes());
        reply.extend_from_slice(&0u32.to_le_bytes());
        reply.extend_from_slice(body);
        server_side.write_all(&reply).await.expect("reply");
        drop(server_side);

        let (rtype, text) = read_message(&mut client_side).await.expect("read");
        assert_eq!(rtype, 0);
        assert_eq!(text, "{\"major\":1}");
    }

    #[tokio::test]
    async fn bad_magic_is_rejected() {
        let (mut a, mut b) = UnixStream::pair().expect("socketpair");
        b.write_all(b"XXXXXX\0\0\0\0\0\0\0\0").await.expect("w");
        drop(b);
        let err = read_message(&mut a).await.expect_err("must fail");
        assert!(err.to_string().contains("bad magic"));
    }

    #[tokio::test]
    async fn oversized_payload_rejected() {
        let (mut a, mut b) = UnixStream::pair().expect("socketpair");
        let huge = (16 * 1024 * 1024 + 1u32).to_le_bytes();
        let mut msg = Vec::new();
        msg.extend_from_slice(MAGIC);
        msg.extend_from_slice(&huge);
        msg.extend_from_slice(&0u32.to_le_bytes());
        b.write_all(&msg).await.expect("w");
        drop(b);
        let err = read_message(&mut a).await.expect_err("must fail");
        assert!(err.to_string().contains("sanity limit"));
    }

    #[test]
    fn missing_swaysock_maps_to_backend_unavailable() {
        // 临时清除 SWAYSOCK，保证断言在任何 CI 环境下都执行（即便外部
        // 注入了该变量）。env mutate 在单线程 test 下安全；本测试不与
        // 其他并发读取 SWAYSOCK 的测试同帧运行。
        let prev = std::env::var_os("SWAYSOCK");
        std::env::remove_var("SWAYSOCK");
        let err = SwayIpc::from_env().expect_err("must fail");
        assert!(matches!(err, AgentShellError::BackendUnavailable(_)));
        // 恢复以避免污染同进程后续测试。
        if let Some(v) = prev {
            std::env::set_var("SWAYSOCK", v);
        }
    }
}
