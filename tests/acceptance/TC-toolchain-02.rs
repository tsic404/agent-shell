//! TC-toolchain-02（AC-101，P1）：CI 不声明版本，安装动作全部取自声明文件。
//!
//! - 场景：CI 工作流的每个 job 都从仓库根声明文件取版本，安装参数显式带
//!   `rustfmt`/`clippy` 组件，并校验激活工具链与声明一致。
//! - 判据：工作流零版本字面量（四种形态：三段版本字面量、浮动通道名、工具链
//!   配置键/环境变量、`rustup toolchain install` 后的非变量实参）；每个 job 的
//!   初始化步骤以 `"$channel"` 安装并带 `--profile minimal --component
//!   rustfmt,clippy`；校验激活前缀 `grep -qF "$channel-"`；不存在第二个取值源。
//! - 执行：`cargo test -p agent-shell-acceptance --test TC-toolchain-02`
//!
//! 取证对象是 `.github/workflows/pr-check.yml` 文本：按 `jobs:` 下缩进为 2 的
//! 键切分 job，再按缩进为 6 的 `- ` 行切分步骤。注释行不参与判定（F6 约束的是
//! 声明）；反斜杠续行先合并成逻辑行，避免安装命令被断行掩盖。运行期由 CI 真实
//! 执行，本用例只校验可静态判定的工作流契约。

use acceptance::workspace_root;
use std::fs;
use std::path::PathBuf;

const WORKFLOW: &str = ".github/workflows/pr-check.yml";

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

/// 切分 job：`jobs:` 之后缩进恰为 2 的 `name:` 行开始一个 job。
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

/// 取 job 内的工具链初始化步骤：以缩进 6、名称含 `Setup Rust` 的 `- ` 行起始，
/// 到下一个同缩进步骤为止。
fn setup_step(lines: &[String]) -> Option<Vec<String>> {
    let start = lines.iter().position(|l| {
        indent(l) == 6 && l.trim_start().starts_with("- ") && l.contains("Setup Rust")
    })?;
    let end = lines[start + 1..]
        .iter()
        .position(|l| indent(l) == 6 && l.trim_start().starts_with("- "))
        .map(|offset| start + 1 + offset)
        .unwrap_or(lines.len());
    Some(lines[start..end].to_vec())
}

fn is_ident_byte(b: u8) -> bool {
    b.is_ascii_alphanumeric() || b == b'_'
}

fn match_semver(bytes: &[u8], i: usize) -> Option<(usize, String)> {
    let mut pos = i;
    for segment in 0..3 {
        if segment > 0 {
            if bytes.get(pos) != Some(&b'.') {
                return None;
            }
            pos += 1;
        }
        let start = pos;
        while bytes.get(pos).is_some_and(u8::is_ascii_digit) {
            pos += 1;
        }
        if pos == start {
            return None;
        }
    }
    if bytes
        .get(pos)
        .is_some_and(|b| is_ident_byte(*b) || *b == b'.')
    {
        return None;
    }
    Some((pos, String::from_utf8_lossy(&bytes[i..pos]).into_owned()))
}

fn semver_literals(text: &str) -> Vec<String> {
    let bytes = text.as_bytes();
    let mut hits = Vec::new();
    let mut i = 0;
    while i < bytes.len() {
        let boundary = i == 0 || !is_ident_byte(bytes[i - 1]);
        if boundary && bytes[i].is_ascii_digit() {
            if let Some((end, lit)) = match_semver(bytes, i) {
                hits.push(lit);
                i = end;
                continue;
            }
        }
        i += 1;
    }
    hits
}

fn tokens(line: &str) -> Vec<&str> {
    line.split(|c: char| !(c.is_alphanumeric() || matches!(c, '_' | '-' | '/' | '@' | '.')))
        .filter(|t| !t.is_empty())
        .collect()
}

#[test]
fn ci_declares_no_version_and_installs_from_declaration() {
    let path = workflow_path();
    let text = fs::read_to_string(&path).unwrap_or_else(|e| panic!("read {}: {e}", path.display()));
    let lines = logical_lines(&text);

    // ---- (1) 零版本字面量：形态一（三段版本）、形态二（浮动通道名） ----
    let literals: Vec<String> = lines.iter().flat_map(|l| semver_literals(l)).collect();
    assert!(
        literals.is_empty(),
        "工作流出现版本字面量（版本必须唯一来自 rust-toolchain.toml）：{literals:?}"
    );
    let floating: Vec<&str> = lines
        .iter()
        .flat_map(|l| tokens(l))
        .filter(|t| matches!(*t, "stable" | "beta" | "nightly"))
        .collect();
    assert!(floating.is_empty(), "工作流出现浮动通道名：{floating:?}");

    // ---- (2) 零版本字面量：形态三（配置键 / 环境变量）、形态四（install 实参） ----
    let mut config_hits: Vec<String> = Vec::new();
    for (n, line) in lines.iter().enumerate() {
        let trimmed = line.trim_start();
        for key in [
            "toolchain:",
            "rust-version:",
            "channel:",
            "rustup-toolchain:",
        ] {
            if trimmed.starts_with(key) {
                config_hits.push(format!("第 {} 行 {key}", n + 1));
            }
        }
        for token in tokens(line) {
            let upper = token.to_ascii_uppercase();
            let env_shaped = token == upper
                && !token.is_empty()
                && token
                    .chars()
                    .all(|c| c.is_ascii_uppercase() || c.is_ascii_digit() || c == '_');
            if env_shaped && (upper.contains("TOOLCHAIN") || upper == "RUST_VERSION") {
                config_hits.push(format!("第 {} 行 {token}", n + 1));
            }
        }
        if let Some(idx) = line.find("rustup toolchain install") {
            let rest = &line[idx + "rustup toolchain install".len()..];
            let arg = rest.split_whitespace().next().unwrap_or_default();
            assert_eq!(
                arg,
                "\"$channel\"",
                "第 {} 行 rustup toolchain install 的版本实参必须是 \"$channel\"，实得 {arg}",
                n + 1
            );
        }
    }
    assert!(
        config_hits.is_empty(),
        "工作流以配置键/环境变量声明工具链版本：{config_hits:?}"
    );

    // ---- (3) 每个 job 的初始化步骤 ----
    let jobs = split_jobs(&lines);
    assert!(
        jobs.len() >= 2,
        "工作流应至少包含 check 与 check-arm64 两个 job，实得：{:?}",
        jobs.iter().map(|(n, _)| n).collect::<Vec<_>>()
    );
    for (name, body) in &jobs {
        let step = setup_step(body)
            .unwrap_or_else(|| panic!("job {name} 缺少工具链初始化步骤（Setup Rust）"));
        let step_text = step.join("\n");
        assert!(
            step_text.contains("rust-toolchain.toml"),
            "job {name} 的初始化步骤未从 rust-toolchain.toml 取值：{step_text}"
        );
        assert!(
            step_text.contains("sed"),
            "job {name} 的初始化步骤未解析声明文件：{step_text}"
        );
        assert!(
            step_text.contains("rustup toolchain install \"$channel\""),
            "job {name} 未以 \"$channel\" 安装工具链：{step_text}"
        );
        assert!(
            step_text.contains("--profile minimal"),
            "job {name} 安装缺 --profile minimal：{step_text}"
        );
        assert!(
            step_text.contains("--component rustfmt,clippy"),
            "job {name} 安装缺 --component rustfmt,clippy：{step_text}"
        );
        assert!(
            step_text.contains("rustup show active-toolchain"),
            "job {name} 未打印激活工具链：{step_text}"
        );
        assert!(
            step_text.contains("grep -qF \"$channel-\""),
            "job {name} 未校验激活前缀 \"$channel-\"：{step_text}"
        );
    }

    // ---- (4) 不存在第二个取值源 ----
    for forbidden in [
        "rustup override set",
        "rustup default",
        "+stable",
        "+beta",
        "+nightly",
    ] {
        assert!(
            !text.contains(forbidden),
            "工作流出现第二个工具链取值源：{forbidden}"
        );
    }
    for (n, line) in lines.iter().enumerate() {
        if let Some(rhs) = line.trim_start().strip_prefix("channel=") {
            assert!(
                rhs.contains("rust-toolchain.toml") && rhs.contains("sed"),
                "第 {} 行 channel 赋值未从 rust-toolchain.toml 派生：{line}",
                n + 1
            );
        }
    }
}
