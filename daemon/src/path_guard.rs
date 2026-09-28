//! 用户给定路径的类型守卫（§22.2 显式端点 / 单实例锁路径）。
//!
//! `--socket`、`--lock-path` / `AGENT_SHELL_LOCK` 都可能落在共享目录（`/tmp` 等），
//! 那里同机他人可以预埋符号链接或占位文件。绑定/打开之前先按类型分流：
//! 只接受预期类型的既有对象，其余一律拒绝并报出实际类型——既避免跟随符号链接
//! 写到别处（截断他人文件），也避免为了「占位」而删除他人的文件。

use std::fs::FileType;
use std::os::unix::fs::FileTypeExt;

/// 路径类型描述，用于拒绝既有路径时的报错（两处守卫共用同一套措辞）。
pub(crate) fn describe_file_type(file_type: &FileType) -> &'static str {
    if file_type.is_dir() {
        "directory"
    } else if file_type.is_symlink() {
        "symlink"
    } else if file_type.is_file() {
        "regular file"
    } else if file_type.is_socket() {
        "unix socket"
    } else {
        "non-regular file"
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn describe_file_type_names_unix_types() {
        let dir = tempfile::tempdir().expect("tempdir");
        let file = dir.path().join("f");
        std::fs::write(&file, "x").expect("write");
        let link = dir.path().join("l");
        std::os::unix::fs::symlink(&file, &link).expect("symlink");
        let sock = dir.path().join("s");
        let _listener = std::os::unix::net::UnixListener::bind(&sock).expect("bind");

        let kind = |p: &std::path::Path| {
            describe_file_type(&std::fs::symlink_metadata(p).expect("metadata").file_type())
        };
        assert_eq!(kind(dir.path()), "directory");
        assert_eq!(kind(&file), "regular file");
        assert_eq!(kind(&link), "symlink");
        assert_eq!(kind(&sock), "unix socket");
        assert!(std::fs::symlink_metadata(&sock)
            .expect("metadata")
            .file_type()
            .is_socket());
    }
}
