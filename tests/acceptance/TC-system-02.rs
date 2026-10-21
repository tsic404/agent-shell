//! TC-system-02（AC-71，P2）：文件选择 / 回收站占位回执与零副作用。
//!
//! - 场景：`file.pick` 与 `file.trash` 尚未接线的占位方法，传入真实路径参数。
//! - 判据：两者均成功返回 `{"status":"not_implemented","service":"<方法名>"}`，
//!   不产生 RPC 错误（CLI 退出码因此为 0）；目标文件仍存在、内容与目录项集合
//!   在调用前后完全一致——占位不得偷偷移动/删除/创建文件。
//! - 执行：`cargo test -p agent-shell-acceptance --test TC-system-02`
//!
//! 环境限制：无——占位回执与文件系统副作用都是 headless 可判定的确定性行为。
//! 真实文件选择器/回收站后端属于 Phase 3 接线范围，本用例只钉住当前占位契约。

use std::path::Path;

use acceptance::Fixture;
use agent_shell_rpc::{method, RpcErrorCode};
use serde_json::json;

/// 目录项快照（名字 + 类型 + 文件内容），用于证明占位不产生副作用。
fn snapshot(dir: &Path) -> Vec<(String, bool, Vec<u8>)> {
    let mut entries: Vec<(String, bool, Vec<u8>)> = std::fs::read_dir(dir)
        .unwrap_or_else(|e| panic!("read {}: {e}", dir.display()))
        .map(|entry| {
            let entry = entry.expect("directory entry");
            let path = entry.path();
            let is_dir = entry.file_type().expect("file type").is_dir();
            let content = if is_dir {
                Vec::new()
            } else {
                std::fs::read(&path).unwrap_or_else(|e| panic!("read {}: {e}", path.display()))
            };
            (
                entry.file_name().to_string_lossy().into_owned(),
                is_dir,
                content,
            )
        })
        .collect();
    entries.sort();
    entries
}

/// 占位回执：成功（无 error）且载荷精确。
async fn assert_placeholder(
    fixture: &mut Fixture,
    method_name: &str,
    service: &str,
    params: serde_json::Value,
) {
    let response = fixture.call(method_name, Some(params)).await;
    assert!(
        response.error.is_none(),
        "{method_name} 占位不得返回错误（CLI 退出码必须为 0），got: {:?}",
        response.error
    );
    assert_eq!(
        response.result.expect("placeholder result"),
        json!({ "status": "not_implemented", "service": service }),
        "{method_name} 占位回执必须精确标识未实现服务"
    );
}

#[tokio::test]
async fn file_placeholders_report_not_implemented_without_touching_files() {
    let mut fixture = Fixture::start().await;
    let dir = tempfile::tempdir().expect("temp file dir");
    let file = dir.path().join("acceptance.txt");
    std::fs::write(&file, b"tc-system-02 payload\n").expect("seed file");
    let before = snapshot(dir.path());

    assert_placeholder(
        &mut fixture,
        method::FILE_PICK,
        "file.pick",
        json!({ "title": "pick a file", "types": ["text/plain"] }),
    )
    .await;
    assert_placeholder(
        &mut fixture,
        method::FILE_TRASH,
        "file.trash",
        json!({ "path": file.display().to_string() }),
    )
    .await;

    // 目标文件与目录项集合逐字节不变：占位既没有删除，也没有生成回收站目录。
    assert!(file.is_file(), "file.trash 占位不得移动/删除目标文件");
    assert_eq!(
        std::fs::read(&file).expect("target still readable"),
        b"tc-system-02 payload\n"
    );
    assert_eq!(
        snapshot(dir.path()),
        before,
        "占位调用前后目录项与内容必须完全一致"
    );

    // 重复调用仍是同一回执（幂等占位，不因「已处理」改变语义）。
    assert_placeholder(
        &mut fixture,
        method::FILE_TRASH,
        "file.trash",
        json!({ "path": file.display().to_string() }),
    )
    .await;
    assert_eq!(snapshot(dir.path()), before);

    // 未知方法仍走 MethodNotFound（占位表不是兜底一切的万能回执）。
    let error = fixture
        .call_err("file.does_not_exist", Some(json!({ "path": "/tmp/x" })))
        .await;
    assert_eq!(error.code, RpcErrorCode::MethodNotFound as i32);
}
