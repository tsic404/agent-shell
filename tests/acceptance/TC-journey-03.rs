//! TC-journey-03（AC-92/AC-93，P1）：旅程轮次可重复且不污染宿主，失败信息面向用户。
//!
//! - 场景：清理可重复、错误信息可理解
//! - 判据：重复执行结果一致；失败路径的信息能说明发生了什么
//! - 执行：`cargo test -p agent-shell-acceptance --test TC-journey-03`
//!
//! 判定面是一轮旅程留下的痕迹：审计留痕、portal 会话状态（capture 的
//! `restore_token` 后端即该会话管理器）与状态目录解析都必须落在本轮沙箱内，宿主
//! 状态目录零记录；同一旅程步跑两轮给出同一结果，且该比较有判别力——控制轮在状态
//! 目录内多落一个文件时集合必须不同。AC-94 的判据是报告流程（按缺陷模板提交），
//! 实现仓无可执行面，不在本用例内。

use std::path::{Path, PathBuf};

use acceptance::Fixture;
use agent_shell_rpc::{method, RpcError, RpcErrorCode};

/// 控制轮注入的状态目录产物名：只用于自检产物集合比较的判别力，不参与旅程语义。
const CONTROL_EXTRA: &str = "journey-control-extra";

/// 宿主状态目录基线：在任何夹具启动前取——此刻进程环境仍是宿主真值，夹具启动后
/// `XDG_STATE_HOME` 才指向沙箱。
struct HostState {
    audit: PathBuf,
    sessions: PathBuf,
}

impl HostState {
    /// 按生产解析取宿主落点（daemon 装配与安全模块用同一 XDG 约定）。
    fn capture() -> Self {
        Self {
            audit: agent_shell_core::audit::state_dir().join("audit.jsonl"),
            sessions: agent_shell_daemon::single_instance::state_dir().join("sessions.json"),
        }
    }

    fn audit_text(&self) -> String {
        read_text(&self.audit)
    }

    fn sessions_text(&self) -> String {
        read_text(&self.sessions)
    }
}

/// 一轮旅程的可观察结果。
struct Round {
    /// 本轮状态目录：生产解析结果，须落在沙箱内。
    state_dir: PathBuf,
    /// 本轮状态目录内的产物清单（相对路径、排序）：两轮产物集合的比较面。
    ///
    /// 必须在沙箱存活期内采集：沙箱随 [`Fixture`] 析构即删除，此后目录不存在，
    /// 清单只会退化成空集合。
    files: Vec<String>,
    /// 本轮审计留痕全文。
    audit: String,
    /// 本轮会话状态全文。
    sessions: String,
    /// 本轮失败的旅程步响应。
    failure: RpcError,
    /// 本轮唯一的审计探针（调用方身份，C-107 按调用方留痕）。
    audit_probe: String,
    /// 本轮唯一的会话探针（ScreenCast 恢复令牌）。
    session_probe: String,
}

/// 跑一轮旅程：一条失败的真实请求 + 一次真实会话状态写入，读回本轮落点。
///
/// `extra` 是控制轮注入的文件名：在状态目录内额外落一个文件，用于自检产物集合
/// 比较的判别力；正常轮次传 `None`。
async fn run_round(tag: &str, extra: Option<&str>) -> Round {
    let mut fixture = Fixture::start().await;
    let sandbox = fixture.sandbox_root().to_path_buf();
    let state_dir = agent_shell_daemon::single_instance::state_dir();
    assert!(
        state_dir.starts_with(&sandbox),
        "本轮状态目录必须落在本轮沙箱内：{} 不在 {} 内",
        state_dir.display(),
        sandbox.display()
    );
    // 审计与会话状态同根（C-109）：单点断言，避免两处解析悄悄分叉。
    assert_eq!(
        state_dir,
        agent_shell_core::audit::state_dir(),
        "审计与会话状态必须共用同一状态根"
    );

    // 旅程步：无显示会话下的窗口列表。门禁留痕（审计通道），并给出失败路径信息。
    let audit_probe = format!("journey-audit-probe-{tag}");
    fixture.set_caller_id(&audit_probe);
    let failure = fixture.call_err(method::WINDOWS_LIST, None).await;

    // 会话通道：capture 的 restore_token 后端就是该会话管理器，写入走同一落点。
    let session_probe = format!("journey-session-probe-{tag}");
    fixture
        .save_screencast_token(&session_probe)
        .expect("会话状态写入必须成功");
    if let Some(name) = extra {
        std::fs::write(state_dir.join(name), b"").expect("控制轮注入产物必须写入成功");
    }

    // 沙箱随 `fixture` 存活：产物清单在此处采齐。返回后再采只会读到已删除目录。
    let files = state_files(&state_dir);
    for product in ["audit.jsonl", "sessions.json"] {
        assert!(
            files.iter().any(|file| file == product),
            "本轮状态目录内必须留有产物 {product}：{files:?}"
        );
    }

    let audit = read_text(&state_dir.join("audit.jsonl"));
    let sessions = read_text(&state_dir.join("sessions.json"));

    Round {
        state_dir,
        files,
        audit,
        sessions,
        failure,
        audit_probe,
        session_probe,
    }
}

#[tokio::test]
async fn journey_rounds_are_repeatable_and_leave_no_host_residue() {
    let host = HostState::capture();

    let first = run_round("r1", None).await;
    let second = run_round("r2", None).await;
    let control = run_round("r3", Some(CONTROL_EXTRA)).await;

    // ── 场景 1：宿主状态目录零记录、本轮写入只落本轮沙箱（AC-92）──
    let host_audit = host.audit_text();
    for probe in [&first.audit_probe, &second.audit_probe] {
        assert!(
            !host_audit.contains(probe),
            "旅程不得向宿主审计文件写记录：{}",
            host.audit.display()
        );
    }
    let host_sessions = host.sessions_text();
    for probe in [&first.session_probe, &second.session_probe] {
        assert!(
            !host_sessions.contains(probe),
            "旅程不得向宿主会话文件写记录：{}",
            host.sessions.display()
        );
    }
    // 宿主零记录不是「哪里都没写」：两条通道的真实写入都必须在本轮状态目录内。
    assert!(
        first.audit.contains(&first.audit_probe),
        "审计留痕必须落在本轮状态目录：{}",
        first.state_dir.display()
    );
    assert!(
        first.sessions.contains(&first.session_probe),
        "会话状态必须落在本轮状态目录：{}",
        first.state_dir.display()
    );

    // ── 场景 2：跨轮次无污染、重复执行结果一致（AC-92）──
    assert_ne!(
        first.state_dir, second.state_dir,
        "每轮必须是独立沙箱（跨轮次不共享状态根）"
    );
    assert!(
        !first.audit.contains(&second.audit_probe)
            && !first.sessions.contains(&second.session_probe),
        "第一轮的产物不得出现第二轮的探针"
    );
    assert!(
        !second.audit.contains(&first.audit_probe)
            && !second.sessions.contains(&first.session_probe),
        "第二轮的产物不得出现第一轮的探针"
    );
    assert_eq!(
        first.failure.code, second.failure.code,
        "同一旅程步两轮必须给出同一错误码"
    );
    assert_eq!(
        first.failure.message, second.failure.message,
        "同一旅程步两轮必须给出同一信息"
    );
    assert_eq!(
        first.files, second.files,
        "两轮留在状态目录内的产物集合必须一致"
    );
    // 反证：控制轮与正常轮的唯一差别是状态目录内多了一个注入文件，集合因此必须
    // 不同——「两轮一致」若靠空集合成立，这里就会红。
    assert_ne!(
        control.files, first.files,
        "状态目录内多一个产物时，产物集合比较必须给出不同结果"
    );
    assert!(
        control.files.contains(&CONTROL_EXTRA.to_string()),
        "控制轮的注入产物必须进入清单：{:?}",
        control.files
    );

    // ── 场景 3：失败信息面向用户（AC-93，C-103）──
    assert_eq!(
        first.failure.code,
        RpcErrorCode::BackendUnavailable as i32,
        "无显示会话的窗口能力必须报后端不可用（C-033）"
    );
    assert_user_facing(&first.failure);
}

/// 失败路径的信息必须说明发生了什么，且不是裸错误码或堆栈（C-103）。
fn assert_user_facing(failure: &RpcError) {
    let message = failure.message.trim();
    assert!(!message.is_empty(), "失败信息不得为空");
    assert_ne!(message, failure.code.to_string(), "失败信息不得是裸错误码");
    for artifact in ["panicked at", "RUST_BACKTRACE", "Backtrace", "RpcError {"] {
        assert!(
            !message.contains(artifact),
            "失败信息不得是调试/堆栈片段：{message}"
        );
    }
    assert!(
        ["compositor", "显示服务器"]
            .iter()
            .any(|cause| message.contains(cause)),
        "失败信息必须点名缺失的能力面：{message}"
    );
}

/// 状态目录内的产物清单（相对路径、排序）：两轮产物集合的比较面。
fn state_files(dir: &Path) -> Vec<String> {
    let mut files = Vec::new();
    collect_files(dir, dir, &mut files);
    files.sort();
    files
}

fn collect_files(root: &Path, dir: &Path, files: &mut Vec<String>) {
    // 目录不存在或不可读时显式失败：静默归零会让产物集合断言退化成空集合比较。
    let entries = std::fs::read_dir(dir)
        .unwrap_or_else(|error| panic!("状态目录不可读（{}）：{error}", dir.display()));
    for entry in entries {
        let entry = entry
            .unwrap_or_else(|error| panic!("状态目录条目不可读（{}）：{error}", dir.display()));
        let path = entry.path();
        if path.is_dir() {
            collect_files(root, &path, files);
        } else if let Ok(relative) = path.strip_prefix(root) {
            files.push(relative.display().to_string());
        }
    }
}

/// 缺失即空文本：判据是「宿主侧不得出现本轮探针」，文件不存在同样满足。
fn read_text(path: &Path) -> String {
    std::fs::read_to_string(path).unwrap_or_default()
}
