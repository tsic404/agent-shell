//! C-107 回归：`audit_log_path` 配成裸相对文件名时，写入与查询必须解析到
//! 同一目录——写下的记录要能按维度读回，而不是只落盘却查不到。
//!
//! 裸文件名的相对语义本来就相对于进程工作目录，所以本用例把工作目录切到
//! 一次性临时目录。测试二进制只含本用例，不与其它用例共享工作目录。

use agent_shell_core::security::{AgentShellConfig, SecurityManager};

#[test]
fn relative_audit_path_is_written_and_queried_from_the_same_directory() {
    let dir = tempfile::tempdir().expect("tempdir");
    std::env::set_current_dir(dir.path()).expect("chdir to sandbox");

    let mut config = AgentShellConfig::default();
    config.security.audit_log_path = Some("audit.jsonl".into());
    let security = SecurityManager::with_config(config);

    security
        .audit
        .log("relative-probe", "daemon.ping", "deny", false);

    // 断言读回的是那条记录的内容，而不只是「分段文件存在」。
    let entries = security
        .audit
        .query("relative-probe", "daemon.ping", "deny", Some(false));
    assert_eq!(
        entries.len(),
        1,
        "相对路径写下的记录必须能按维度读回：{entries:?}"
    );
    assert_eq!(entries[0].agent_id, "relative-probe");
    assert_eq!(entries[0].op, "daemon.ping");
    assert_eq!(entries[0].decision, "deny");
    assert!(!entries[0].result);

    // 记录落在本进程分段里，且该分段确实在工作目录中。
    let files = security.audit.files();
    assert_eq!(files.len(), 1, "应有一个本进程分段：{files:?}");
    let name = files[0]
        .file_name()
        .expect("segment file name")
        .to_string_lossy()
        .into_owned();
    assert!(
        name.starts_with("audit.jsonl.p"),
        "分段名应带锚点前缀：{name}"
    );
    assert!(
        dir.path().join(&name).is_file(),
        "分段必须落在工作目录：{}",
        dir.path().join(&name).display()
    );
}
