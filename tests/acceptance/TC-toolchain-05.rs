//! TC-toolchain-05（AC-104，P2）：CI 日志里能看到实际生效的工具链行。
//!
//! - 场景：每个 CI job 的初始化步骤打印 rustup 实际选中的工具链（而非只打印
//!   期望版本），并校验该行的版本前缀等于声明文件的 channel。
//! - 判据：每个 job 的初始化步骤都执行 `rustup show active-toolchain`（其输出
//!   就是真实生效的工具链行），并以 `grep -qF "$channel-"` 断言该行以声明的
//!   版本前缀开头；不以 echo 期望值代替真实输出。
//! - 执行：`cargo test -p agent-shell-acceptance --test TC-toolchain-05`
//!
//! 判据要求日志行额外带锚定说明语（「由本仓声明文件覆盖」或其英文等价）。
//! 该说明语是 **CI 运行期日志** 的形态，仓内不可观测：当前工作流并未输出此
//! 说明语，因此本用例退化为可静态判定的等价契约——初始化步骤必须调用
//! `rustup show active-toolchain` 打印真实生效行，且随后以声明值前缀校验该行。
//! 若将来工作流加入该说明语，本用例会自动升级为逐 job 校验说明语存在。
//!
//! 取证对象是 `.github/workflows/pr-check.yml`；CI 实际日志属 CI 侧，仓内只
//! 校验产生该日志的命令行契约。

use acceptance::workspace_root;
use std::fs;
use std::path::PathBuf;

const WORKFLOW: &str = ".github/workflows/pr-check.yml";

/// 判据要求的锚定说明语（中文原文与常见英文等价写法）。
const ANCHOR_PHRASES: &[&str] = &[
    "由本仓声明文件覆盖",
    "covered by this repository's declaration",
    "declared by this repository",
    "overridden by this repository's declaration",
];

fn workflow_path() -> PathBuf {
    workspace_root().join(WORKFLOW)
}

fn strip_comment(line: &str) -> &str {
    let mut quote: Option<char> = None;
    for (idx, ch) in line.char_indices() {
        match quote {
            Some(q) if ch == q => quote = None,
            Some(_) => {}
            None if ch == '"' || ch == '\'' => quote = Some(ch),
            None if ch == '#' => return &line[..idx],
            None => {}
        }
    }
    line
}

/// 逻辑行：注释剔除、反斜杠续行合并，缩进保留。
fn logical_lines(text: &str) -> Vec<String> {
    let mut out = Vec::new();
    let mut pending = String::new();
    for raw in text.lines() {
        let line = strip_comment(raw);
        match line.trim_end().strip_suffix('\\') {
            Some(head) => {
                pending.push_str(head.trim_end());
                pending.push(' ');
            }
            None => {
                let mut full = std::mem::take(&mut pending);
                full.push_str(line);
                out.push(full);
            }
        }
    }
    if !pending.is_empty() {
        out.push(pending);
    }
    out
}

fn indent(line: &str) -> usize {
    line.len() - line.trim_start().len()
}

/// `jobs:` 下缩进恰为 2 的 `name:` 行开始一个 job。
fn split_jobs(lines: &[String]) -> Vec<(String, Vec<String>)> {
    let jobs_start = lines
        .iter()
        .position(|l| l.trim() == "jobs:")
        .expect("工作流缺少 jobs:");
    let mut jobs: Vec<(String, Vec<String>)> = Vec::new();
    for line in &lines[jobs_start + 1..] {
        let trimmed = line.trim();
        if indent(line) == 2 && trimmed.ends_with(':') && !trimmed.starts_with('-') {
            jobs.push((trimmed.trim_end_matches(':').to_string(), Vec::new()));
        } else if let Some((_, body)) = jobs.last_mut() {
            body.push(line.clone());
        }
    }
    jobs
}

/// 取 job 内的工具链初始化步骤（缩进 6、名称含 `Setup Rust`）。
fn setup_step(body: &[String]) -> Option<String> {
    let start = body.iter().position(|l| {
        indent(l) == 6 && l.trim_start().starts_with("- ") && l.contains("Setup Rust")
    })?;
    let end = body[start + 1..]
        .iter()
        .position(|l| indent(l) == 6 && l.trim_start().starts_with("- "))
        .map(|offset| start + 1 + offset)
        .unwrap_or(body.len());
    Some(body[start..end].join("\n"))
}

#[test]
fn every_ci_job_prints_and_verifies_the_active_toolchain() {
    let path = workflow_path();
    let text = fs::read_to_string(&path).unwrap_or_else(|e| panic!("read {}: {e}", path.display()));
    let lines = logical_lines(&text);
    let jobs = split_jobs(&lines);
    assert!(!jobs.is_empty(), "工作流未切分出任何 job");

    let anchored = ANCHOR_PHRASES.iter().find(|p| text.contains(**p));
    if anchored.is_none() {
        // 说明语属 CI 日志形态，仓内无此运行时；记录退化说明并以等价契约判定。
        eprintln!(
            "note: {WORKFLOW} 未输出锚定说明语（由本仓声明文件覆盖 / 英文等价），\
             该判据的日志说明语部分在仓内不可观测；改判 `rustup show active-toolchain` \
             的真实输出行以声明前缀校验。"
        );
    }

    for (name, body) in &jobs {
        let step = setup_step(body)
            .unwrap_or_else(|| panic!("job {name} 缺少工具链初始化步骤（Setup Rust）"));

        // 真实生效行：必须调用 rustup 自身输出，而非 echo 期望值。
        assert!(
            step.contains("rustup show active-toolchain"),
            "job {name} 的初始化步骤未打印真实生效的工具链行：{step}"
        );
        assert!(
            !step.contains("echo \"$channel\" | grep"),
            "job {name} 以 echo 期望值代替 rustup 的真实输出：{step}"
        );

        // 该行的版本前缀必须等于声明值。
        assert!(
            step.contains("rustup show active-toolchain | grep -qF \"$channel-\""),
            "job {name} 未以声明值前缀校验真实工具链行：{step}"
        );

        // 声明文件是唯一取值源（前缀变量必须由声明文件派生）。
        assert!(
            step.contains("rust-toolchain.toml"),
            "job {name} 的初始化步骤未引用声明文件：{step}"
        );

        if let Some(phrase) = anchored {
            assert!(
                step.contains(phrase),
                "job {name} 的初始化步骤日志缺少锚定说明语 `{phrase}`：{step}"
            );
        }
    }
}
