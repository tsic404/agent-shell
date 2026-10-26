//! 显式套接字端点的共用判定（命令面与 MCP 入口同源，契约 C-100）。
//!
//! 两个入口都要做同样两件事：空白端点等同于未设置；连上之后先核对对端进程凭据
//! 再写任何请求。判定只有这一份实现，避免安全判据在副本里各自漂移。

use std::path::Path;

/// 端点是否为空：`trim()` 后为空即视为未设置。
///
/// `AGENT_SHELL_SOCKET=` 或纯空格会变成空的相对路径端点，必须在入口处归零，而不是
/// 拿去做一次注定失败的连接。
pub fn is_blank(path: &Path) -> bool {
    path.to_string_lossy().trim().is_empty()
}

/// 本进程真实 uid。
pub fn current_uid() -> u32 {
    // SAFETY: getuid 无参数、无副作用、不会失败（始终返回调用进程的真实 uid）。
    unsafe { libc::getuid() }
}

/// 端点对端 uid 校验：只接受同 uid 的 daemon。
///
/// daemon 侧用 0600 挡住「别人连我」；这里是反向的一半——显式端点路径可能落在共享
/// 目录，别人先绑定同名 socket 就能收到本进程发出的命令负载。路径本身不可信，对端
/// 进程凭据才是判据；校验必须在写入首个请求之前完成。
pub fn verify_peer_uid(path: &Path, peer_uid: u32, our_uid: u32) -> Result<(), String> {
    if peer_uid == our_uid {
        return Ok(());
    }
    Err(format!(
        "refusing daemon socket {}: peer runs as uid {peer_uid}, not ours ({our_uid}) — \
         the path may be a socket planted by another user; keep endpoints under a \
         user-private directory such as $XDG_RUNTIME_DIR",
        path.display()
    ))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn blank_paths_are_unset() {
        assert!(is_blank(Path::new("")));
        assert!(is_blank(Path::new("   ")));
        assert!(is_blank(Path::new("\t")));
        assert!(!is_blank(Path::new("/run/user/1000/agent-shell.sock")));
        // 非空值不 trim：用户给的路径原样使用。
        assert!(!is_blank(Path::new(" /tmp/a.sock ")));
    }

    #[test]
    fn same_uid_is_accepted() {
        assert!(verify_peer_uid(Path::new("/run/user/1000/agent-shell.sock"), 1000, 1000).is_ok());
    }

    #[test]
    fn foreign_uid_names_both_uids_and_the_path() {
        let path = Path::new("/tmp/agent-shell.sock");
        let err = verify_peer_uid(path, 0, 1000).expect_err("foreign uid must be refused");
        assert!(
            err.contains("uid 0") && err.contains("1000") && err.contains("/tmp/agent-shell.sock"),
            "error must name both uids and the path: {err}"
        );
        assert!(
            err.contains("$XDG_RUNTIME_DIR"),
            "error must point at a user-private location: {err}"
        );
    }
}
