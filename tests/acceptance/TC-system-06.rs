//! TC-system-06（AC-75，P2）：密钥写入/读取占位回执与磁盘零明文。
//!
//! - 场景：`secret.set` / `secret.get` 尚未接线，调用时经 L3 门禁的 Confirm
//!   分支短路为占位回执。
//! - 判据：两者均成功返回 `{"status":"not_implemented","service":"secret.xxx"}`，
//!   不产生 RPC 错误；调用后遍历夹具沙箱（HOME / XDG_CONFIG_HOME /
//!   XDG_STATE_HOME 全在其内）的所有文件，密钥明文一次也不出现——占位不得把
//!   密钥落到磁盘（审计只记录方法名，不记录值）。
//! - 执行：`cargo test -p agent-shell-acceptance --test TC-system-06`
//!
//! 环境限制：真实 Secret Service（libsecret / gnome-keyring）不在验收环境内，
//! 本用例判定的是「未接线时不得伪存明文」这一安全边界；接线后本用例应改为
//! 读取真实密钥环并断言明文仍不落盘。

use std::path::{Path, PathBuf};

use acceptance::Fixture;
use agent_shell_rpc::method;
use serde_json::json;

/// 唯一密钥值：只在本次进程内出现，命中沙箱文件即证明发生了明文落盘。
const SECRET: &str = "tc-system-06-hunter2-9f3a7c";

/// 递归收集沙箱内的普通文件（跳过目录与非常规条目）。
fn collect_files(dir: &Path, files: &mut Vec<PathBuf>) {
    let entries = match std::fs::read_dir(dir) {
        Ok(entries) => entries,
        Err(error) => panic!("read {}: {error}", dir.display()),
    };
    for entry in entries {
        let entry = entry.expect("directory entry");
        let path = entry.path();
        let file_type = entry.file_type().expect("file type");
        if file_type.is_dir() {
            collect_files(&path, files);
        } else if file_type.is_file() {
            files.push(path);
        }
    }
}

/// 沙箱内所有文件里出现密钥明文的路径（空 = 未落盘）。
fn files_containing(root: &Path, needle: &[u8]) -> Vec<PathBuf> {
    let mut files = Vec::new();
    collect_files(root, &mut files);
    files
        .into_iter()
        .filter(|path| {
            std::fs::read(path)
                .map(|bytes| bytes.windows(needle.len()).any(|window| window == needle))
                .unwrap_or(false)
        })
        .collect()
}

#[tokio::test]
async fn secret_placeholders_never_persist_plaintext() {
    let mut fixture = Fixture::start().await;

    let set = fixture
        .call_ok(
            method::SECRET_SET,
            Some(json!({ "key": "db-password", "value": SECRET })),
        )
        .await;
    assert_eq!(
        set,
        json!({ "status": "not_implemented", "service": "secret.set" }),
        "secret.set 占位回执必须精确（L3 门禁不得改写成 ConfirmationRequired）"
    );

    let get = fixture
        .call_ok(method::SECRET_GET, Some(json!({ "key": "db-password" })))
        .await;
    assert_eq!(
        get,
        json!({ "status": "not_implemented", "service": "secret.get" }),
        "secret.get 占位回执必须精确"
    );

    // 占位回执是成功语义：两次调用都没有 RPC 错误（call_ok 已断言），
    // 且不得把值/键写进任何沙箱文件。
    let offenders = files_containing(fixture.sandbox_root(), SECRET.as_bytes());
    assert!(
        offenders.is_empty(),
        "密钥明文不得落盘，但以下文件包含它：{offenders:?}"
    );

    // 审计留痕只含方法名：确认审计确实执行（否则上一条断言空转）。
    let mut files = Vec::new();
    collect_files(fixture.sandbox_root(), &mut files);
    let audit: Vec<String> = files
        .iter()
        .filter_map(|path| std::fs::read_to_string(path).ok())
        .filter(|text| text.contains("secret.set"))
        .collect();
    assert!(
        !audit.is_empty(),
        "L3 门禁必须留下 secret.set 的审计痕迹（仅方法名，不含值）"
    );
    assert!(
        audit.iter().all(|text| !text.contains(SECRET)),
        "审计记录不得包含密钥明文"
    );
}
