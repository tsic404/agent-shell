//! TC-journey-02（AC-91，P1）：环境记录与裁决/报告格式。
//!
//! - 场景：环境记录与裁决/报告格式
//! - 判据：必需字段齐全；格式符合模板
//! - 执行：`cargo test -p agent-shell-acceptance --test TC-journey-02`
//!
//! 判定面是验收报告本身：提交、分支、桌面环境、会话类型、二进制版本与桌面组件
//! 版本全部实测（git、doctor 健康行、CLI 自报版本），逐步退出码表逐行对应旅程
//! 记录。缺字段、错序、空值与表格错行都必须被拒绝——报告模板因此可执行。

use std::path::{Path, PathBuf};
use std::process::Command;

use acceptance::journey::{
    component_versions, render_report, validate_report, EnvironmentRecord, Journey, Outcome,
    Priority, StepSpec, EXIT_NOT_RUN, REPORT_FIELDS,
};
use acceptance::{workspace_root, Fixture};
use agent_shell_core::de_detection;
use agent_shell_rpc::{method, DoctorResult};

/// 设计仓 AC-91 的必需字段清单（顺序即报告模板顺序）。
const REQUIRED_FIELDS: [&str; 6] = [
    "提交",
    "分支",
    "桌面环境",
    "会话类型",
    "二进制版本",
    "桌面组件版本",
];

/// 运行 git 并要求成功；空输出是 `git status` 干净树的合法结果。
fn git_output(root: &Path, args: &[&str]) -> String {
    let output = Command::new("git")
        .current_dir(root)
        .args(args)
        .output()
        .unwrap_or_else(|error| panic!("run git {args:?}: {error}"));
    assert!(
        output.status.success(),
        "git {args:?} failed:\n{}",
        String::from_utf8_lossy(&output.stderr)
    );
    String::from_utf8_lossy(&output.stdout).trim().to_string()
}

/// 被测提交：短哈希 + 工作区脏标记（与构建期注入规则一致）。
fn tested_commit(root: &Path) -> String {
    let sha = git_output(root, &["rev-parse", "--short=7", "HEAD"]);
    assert!(
        sha.len() >= 7 && sha.chars().all(|c| c.is_ascii_hexdigit()),
        "被测提交必须是短哈希，实际 {sha:?}"
    );
    let dirty = !git_output(root, &["status", "--porcelain", "--untracked-files=no"]).is_empty();
    if dirty {
        format!("{sha}-dirty")
    } else {
        sha
    }
}

/// 被测分支：CI 的 PR 检出是 detached，取平台给出的源分支名，否则写明 detached。
fn tested_branch(root: &Path) -> String {
    if let Ok(head_ref) = std::env::var("GITHUB_HEAD_REF") {
        if !head_ref.trim().is_empty() {
            return head_ref.trim().to_string();
        }
    }
    let name = git_output(root, &["rev-parse", "--abbrev-ref", "HEAD"]);
    if name != "HEAD" {
        return name;
    }
    let sha = git_output(root, &["rev-parse", "--short=7", "HEAD"]);
    format!("detached@{sha}")
}

/// 构建真实 CLI 二进制并取其 `--version` 行（`agent-shell <版本> (<提交>)`）。
fn cli_version() -> String {
    let root = workspace_root();
    let cargo = std::env::var("CARGO").unwrap_or_else(|_| "cargo".into());
    let build = Command::new(&cargo)
        .current_dir(&root)
        .args([
            "build",
            "-q",
            "-p",
            "agent-shell-cli",
            "--bin",
            "agent-shell",
        ])
        .output()
        .expect("run cargo build for the CLI binary");
    assert!(
        build.status.success(),
        "building agent-shell must succeed; stderr:\n{}",
        String::from_utf8_lossy(&build.stderr)
    );
    let profile = if cfg!(debug_assertions) {
        "debug"
    } else {
        "release"
    };
    let target_dir = std::env::var_os("CARGO_TARGET_DIR")
        .map(PathBuf::from)
        .unwrap_or_else(|| root.join("target"));
    let binary = target_dir.join(profile).join("agent-shell");
    assert!(
        binary.is_file(),
        "expected CLI binary at {}",
        binary.display()
    );
    let output = Command::new(&binary)
        .arg("--version")
        .output()
        .expect("run agent-shell --version");
    assert!(
        output.status.success(),
        "--version must exit 0; stderr:\n{}",
        String::from_utf8_lossy(&output.stderr)
    );
    String::from_utf8_lossy(&output.stdout).trim().to_string()
}

/// 版本行中的构建期提交短标识。
fn embedded_commit(version: &str) -> String {
    let prefix = format!("agent-shell {} (", env!("CARGO_PKG_VERSION"));
    assert!(
        version.starts_with(&prefix) && version.ends_with(')'),
        "version line must be `agent-shell <version> (<commit>)`, got: {version:?}"
    );
    version[prefix.len()..version.len() - 1].to_string()
}

/// 解析 doctor 首行的桌面环境与会话类型：`<桌面环境> (<会话类型>, <信号来源>)`。
fn detection_summary(line: &str) -> Result<(String, String), String> {
    let (_, summary) = line
        .split_once(':')
        .ok_or_else(|| format!("DE 检测行缺少冒号: {line:?}"))?;
    let summary = summary.trim();
    let open = summary
        .find('(')
        .ok_or_else(|| format!("DE 检测行缺少会话括注: {line:?}"))?;
    let close = summary
        .rfind(')')
        .ok_or_else(|| format!("DE 检测行缺少会话括注: {line:?}"))?;
    let desktop = summary[..open].trim().to_string();
    let session = summary[open + 1..close]
        .split(',')
        .next()
        .unwrap_or_default()
        .trim()
        .to_string();
    if desktop.is_empty() || session.is_empty() {
        return Err(format!("DE 检测行的桌面环境或会话类型为空: {line:?}"));
    }
    Ok((desktop, session))
}

/// 采集环境记录：全部值实测，组件无版本面时写明不可用原因。
async fn collect_environment(fixture: &mut Fixture) -> EnvironmentRecord {
    let root = workspace_root();
    let commit = tested_commit(&root);
    let branch = tested_branch(&root);
    let doctor: DoctorResult = serde_json::from_value(fixture.call_ok(method::DOCTOR, None).await)
        .expect("DoctorResult deserializes");
    let (desktop, session_type) =
        detection_summary(&doctor.lines[0]).expect("doctor first line carries detection summary");
    assert_eq!(
        desktop,
        de_detection::detect_desktop_environment().to_string(),
        "报告记录的桌面环境必须与产品检测一致"
    );
    let mut desktop_components = component_versions(&doctor.lines);
    if desktop_components.is_empty() {
        desktop_components.push(("合成器".into(), "不可用（本会话无合成器）".into()));
    }
    let binary_version = tokio::task::spawn_blocking(cli_version)
        .await
        .expect("join CLI version probe");
    assert_eq!(
        embedded_commit(&binary_version),
        commit,
        "报告的提交字段必须与被测二进制自报的构建提交一致"
    );
    EnvironmentRecord {
        commit,
        branch,
        desktop,
        session_type,
        binary_version,
        desktop_components,
    }
}

/// 报告要裁决的旅程：环境记录步真实通过（doctor.run 已成功），特权链路记跳过。
fn recorded_journey() -> Journey {
    let mut journey = Journey::new(vec![
        StepSpec::new("environment", Priority::P1),
        StepSpec::new("privileged", Priority::P2),
    ]);
    journey
        .record("environment", Outcome::Passed, 0)
        .expect("record environment step");
    journey
        .record("privileged", Outcome::Skipped, EXIT_NOT_RUN)
        .expect("record skipped privileged step");
    journey
}

#[tokio::test]
async fn report_carries_measured_environment_and_exact_exit_code_table() {
    let mut fixture = Fixture::start().await;
    let env = collect_environment(&mut fixture).await;
    let journey = recorded_journey();

    // ── 场景 1：字段清单与设计一致，实测值进入报告 ──
    assert_eq!(
        REPORT_FIELDS, REQUIRED_FIELDS,
        "报告模板字段必须与 AC-91 的清单一致"
    );
    let report = render_report(&env, &journey).expect("complete environment must render");
    validate_report(&report, &journey).expect("rendered report must match the template");
    for (label, value) in [
        (REQUIRED_FIELDS[0], env.commit.as_str()),
        (REQUIRED_FIELDS[1], env.branch.as_str()),
        (REQUIRED_FIELDS[2], env.desktop.as_str()),
        (REQUIRED_FIELDS[3], env.session_type.as_str()),
        (REQUIRED_FIELDS[4], env.binary_version.as_str()),
    ] {
        let line = format!("- {label}: {value}");
        assert_eq!(
            report
                .lines()
                .filter(|candidate| *candidate == line)
                .count(),
            1,
            "报告必须恰好一行 `{line}`"
        );
    }
    let components = env
        .desktop_components
        .iter()
        .map(|(name, version)| format!("{name}={version}"))
        .collect::<Vec<_>>()
        .join(", ");
    assert!(
        report.contains(&format!("- 桌面组件版本: {components}")),
        "桌面组件版本必须原样进入报告"
    );
    assert!(
        report.contains("| environment | P1 | 通过 | 0 |"),
        "通过步骤的退出码必须为 0"
    );
    assert!(
        report.contains("| privileged | P2 | 跳过 | -1 |"),
        "跳过步骤必须标注未执行"
    );
    let verdict_line = format!("- 裁决: {}", journey.verdict().expect("complete").summary());
    assert!(
        report.contains(&verdict_line),
        "裁决行必须来自逐项结果: {}",
        journey.verdict().expect("complete").summary()
    );
    assert!(
        verdict_line.contains("P2 跳过: privileged"),
        "P2 未跑必须在裁决中标注跳过"
    );

    // ── 场景 2：组件版本提取以 doctor 行为据，无版本面的行不误报 ──
    let extracted = component_versions(&[
        "✓ KWin 服务   : org.kde.KWin v6.2.5".to_string(),
        "✓ Sway IPC     : 1.10 (SWAYSOCK, roundtrip 0.31ms)".to_string(),
        "✓ 合成器         : KWin".to_string(),
        "✗ 合成器         : unavailable in this session".to_string(),
        "✓ Wayland 协议 : 2/3 globals bound".to_string(),
    ]);
    assert_eq!(
        extracted,
        vec![
            ("KWin 服务".to_string(), "6.2.5".to_string()),
            ("Sway IPC".to_string(), "1.10".to_string()),
        ],
        "只提取带版本面的组件行，计数与耗时不得误判为版本"
    );

    // ── 场景 3：渲染拒绝缺失/空字段，不写占位值 ──
    let mut broken = env.clone();
    broken.commit.clear();
    let error = render_report(&broken, &journey).expect_err("空提交必须拒绝");
    assert!(error.contains(REQUIRED_FIELDS[0]), "{error}");
    let mut broken = env.clone();
    broken.branch.clear();
    let error = render_report(&broken, &journey).expect_err("空分支必须拒绝");
    assert!(error.contains(REQUIRED_FIELDS[1]), "{error}");
    let mut broken = env.clone();
    broken.desktop.clear();
    let error = render_report(&broken, &journey).expect_err("空桌面环境必须拒绝");
    assert!(error.contains(REQUIRED_FIELDS[2]), "{error}");
    let mut broken = env.clone();
    broken.session_type.clear();
    let error = render_report(&broken, &journey).expect_err("空会话类型必须拒绝");
    assert!(error.contains(REQUIRED_FIELDS[3]), "{error}");
    let mut broken = env.clone();
    broken.binary_version.clear();
    let error = render_report(&broken, &journey).expect_err("空二进制版本必须拒绝");
    assert!(error.contains(REQUIRED_FIELDS[4]), "{error}");
    let mut broken = env.clone();
    broken.desktop_components.clear();
    let error = render_report(&broken, &journey).expect_err("空组件版本必须拒绝");
    assert!(error.contains(REQUIRED_FIELDS[5]), "{error}");
    let mut broken = env.clone();
    broken.desktop_components = vec![("合成器".into(), String::new())];
    let error = render_report(&broken, &journey).expect_err("空组件版本值必须拒绝");
    assert!(error.contains(REQUIRED_FIELDS[5]), "{error}");

    // ── 场景 4：模板校验器逐行把关，缺行、错序、空值与漏项都判失败 ──
    let without_branch = report
        .lines()
        .filter(|line| !line.starts_with("- 分支: "))
        .collect::<Vec<_>>()
        .join("\n");
    let error = validate_report(&without_branch, &journey).expect_err("缺分支字段必须拒绝");
    assert!(error.contains("分支"), "{error}");

    let swapped = report.replace("- 提交: ", "- 提交占位: ");
    let error = validate_report(&swapped, &journey).expect_err("字段名漂移必须拒绝");
    assert!(error.contains("字段集合或顺序"), "{error}");

    let empty_value = report.replacen(
        &format!("- 二进制版本: {}", env.binary_version),
        "- 二进制版本: ",
        1,
    );
    let error = validate_report(&empty_value, &journey).expect_err("空字段值必须拒绝");
    assert!(error.contains("二进制版本"), "{error}");

    let without_row = report
        .lines()
        .filter(|line| !line.starts_with("| privileged |"))
        .collect::<Vec<_>>()
        .join("\n");
    let error = validate_report(&without_row, &journey).expect_err("漏行必须拒绝");
    assert!(error.contains("行数"), "{error}");

    let wrong_exit = report.replace(
        "| environment | P1 | 通过 | 0 |",
        "| environment | P1 | 通过 | 2 |",
    );
    let error = validate_report(&wrong_exit, &journey).expect_err("退出码错行必须拒绝");
    assert!(error.contains("不一致"), "{error}");

    let incomplete = Journey::new(vec![
        StepSpec::new("environment", Priority::P1),
        StepSpec::new("privileged", Priority::P2),
    ]);
    let error = render_report(&env, &incomplete).expect_err("漏项旅程不得产出报告");
    assert!(error.contains("缺少 2 步"), "{error}");
    let error = validate_report(&report, &incomplete).expect_err("漏项旅程不得通过模板校验");
    assert!(error.contains("缺少 2 步"), "{error}");
}
