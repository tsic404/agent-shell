//! TC-toolchain-01（AC-100，P1）：工具链版本的唯一真源在仓库根声明文件。
//!
//! - 场景：工具链版本的唯一真源是仓库根声明文件，CI 不另立版本来源。
//! - 判据：根声明文件存在；全仓恰有一个 `rust-toolchain*` 文件；`channel` 为
//!   双引号包裹的精确三段版本（拒绝 stable / beta / nightly 与浮动写法）；
//!   `profile = "minimal"`；`components` 含 `rustfmt` 与 `clippy`；CI 工作流
//!   不含任何版本字面量、浮动工具链动作、工具链配置键与环境变量（F6 四种
//!   禁止形态），`channel` 只由声明文件派生。
//! - 执行：`cargo test -p agent-shell-acceptance --test TC-toolchain-01`
//!
//! 取证全部落在仓内：解析 `rust-toolchain.toml` 文本、递归枚举
//! `rust-toolchain*` 文件（跳过 `target/`、`.git/` 构建与版本库产物）、扫描
//! `.github/workflows/pr-check.yml`。F6 约束的是 CI **声明**版本，因此工作流
//! 的注释行不参与扫描——注释不构成声明；字符串内的 `#` 按引号状态保留。

use acceptance::workspace_root;
use std::fs;
use std::path::{Path, PathBuf};

/// CI 工作流（判据的第二个取证对象，相对工作区根）。
const WORKFLOW: &str = ".github/workflows/pr-check.yml";

fn read_rel(rel: &str) -> String {
    let path = workspace_root().join(rel);
    fs::read_to_string(&path).unwrap_or_else(|e| panic!("read {}: {e}", path.display()))
}

/// 去掉行内注释：`#` 在引号外才起注释作用。
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

/// 取 `key = value` 形态的字段值（已去行内注释、去首尾空白）。
fn field(text: &str, key: &str) -> Option<String> {
    text.lines().find_map(|line| {
        let line = strip_comment(line).trim();
        let (lhs, rhs) = line.split_once('=')?;
        (lhs.trim() == key).then(|| rhs.trim().to_string())
    })
}

/// 递归收集 `rust-toolchain*` 文件；跳过构建产物与版本库元数据。
fn collect_toolchain_files(dir: &Path, found: &mut Vec<PathBuf>) {
    let Ok(entries) = fs::read_dir(dir) else {
        return;
    };
    for entry in entries.flatten() {
        let path = entry.path();
        let name = entry.file_name().to_string_lossy().into_owned();
        if path.is_dir() {
            if matches!(name.as_str(), "target" | ".git" | "node_modules") {
                continue;
            }
            collect_toolchain_files(&path, found);
        } else if name.starts_with("rust-toolchain") {
            found.push(path);
        }
    }
}

/// 工作流的非注释逻辑行：反斜杠续行合并、行内注释剔除，保留缩进。
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

fn is_ident_byte(b: u8) -> bool {
    b.is_ascii_alphanumeric() || b == b'_'
}

/// 从 `i` 起匹配 `\d+\.\d+\.\d+`；尾随字符不得为标识符字符或 `.`。
/// 返回（结束下标，字面量）。
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

/// 把一行切成标识符样 token（字母数字与 `_-./@` 视作同一 token 内）。
fn tokens(line: &str) -> Vec<&str> {
    line.split(|c: char| !(c.is_alphanumeric() || matches!(c, '_' | '-' | '/' | '@' | '.')))
        .filter(|t| !t.is_empty())
        .collect()
}

fn toolchain_field_value(text: &str) -> String {
    let raw = field(text, "channel").expect("rust-toolchain.toml 缺少 channel 字段");
    assert!(
        raw.starts_with('"') && raw.ends_with('"') && raw.len() >= 2,
        "channel 必须用双引号包裹：{raw}"
    );
    raw[1..raw.len() - 1].to_string()
}

#[test]
fn toolchain_version_has_single_in_repo_source() {
    // ---- (1) 声明文件存在、全仓唯一 ----
    let toolchain_path = workspace_root().join("rust-toolchain.toml");
    assert!(
        toolchain_path.is_file(),
        "仓库根缺少工具链声明文件：{}",
        toolchain_path.display()
    );
    let mut found = Vec::new();
    collect_toolchain_files(&workspace_root(), &mut found);
    assert_eq!(
        found.len(),
        1,
        "全仓必须恰有一个 rust-toolchain* 文件，实得：{found:?}"
    );
    assert_eq!(
        found[0].canonicalize().unwrap(),
        toolchain_path.canonicalize().unwrap(),
        "唯一的工具链声明文件必须在仓库根"
    );
    let toolchain = fs::read_to_string(&toolchain_path)
        .unwrap_or_else(|e| panic!("read {}: {e}", toolchain_path.display()));

    // ---- (2) channel：双引号精确三段版本 ----
    let channel = toolchain_field_value(&toolchain);
    let parts: Vec<&str> = channel.split('.').collect();
    assert_eq!(
        parts.len(),
        3,
        "channel 必须为三段精确版本，拒绝浮动通道：{channel}"
    );
    assert!(
        parts
            .iter()
            .all(|p| !p.is_empty() && p.bytes().all(|b| b.is_ascii_digit())),
        "channel 三段必须全为数字（拒绝 stable / beta / nightly / 缺 patch）：{channel}"
    );

    // ---- (3) profile 与 components ----
    assert_eq!(
        field(&toolchain, "profile").as_deref(),
        Some("\"minimal\""),
        "工具链 profile 必须为 minimal"
    );
    let components = field(&toolchain, "components").expect("缺少 components 字段");
    for required in ["rustfmt", "clippy"] {
        assert!(
            components.contains(&format!("\"{required}\"")),
            "components 缺少 {required}：{components}"
        );
    }

    // ---- (4) CI 工作流的 F6 四种禁止形态 ----
    let workflow = read_rel(WORKFLOW);
    let lines = logical_lines(&workflow);

    // 形态一：任何版本字面量。
    let mut literals = Vec::new();
    for (n, line) in lines.iter().enumerate() {
        for lit in semver_literals(line) {
            literals.push(format!("{}:{}:{}", WORKFLOW, n + 1, lit));
        }
    }
    assert!(
        literals.is_empty(),
        "工作流不得声明版本字面量（版本唯一来自 rust-toolchain.toml）：{literals:?}"
    );

    // 形态二：浮动通道名（stable / beta / nightly）作为工具链选型。
    let floating: Vec<String> = lines
        .iter()
        .enumerate()
        .flat_map(|(n, line)| {
            tokens(line)
                .into_iter()
                .filter(|t| matches!(*t, "stable" | "beta" | "nightly"))
                .map(move |t| format!("{}:{}:{t}", WORKFLOW, n + 1))
        })
        .collect();
    assert!(
        floating.is_empty(),
        "工作流不得以浮动通道名选型工具链：{floating:?}"
    );

    // 形态三：第三方工具链动作（会自带版本来源）。
    let actions: Vec<String> = lines
        .iter()
        .filter_map(|line| {
            let trimmed = line.trim_start();
            let rest = trimmed.strip_prefix("- uses:")?.trim();
            Some(
                rest.split_whitespace()
                    .next()
                    .unwrap_or_default()
                    .to_string(),
            )
        })
        .chain(lines.iter().filter_map(|line| {
            let rest = line.trim().strip_prefix("uses:")?.trim();
            Some(
                rest.split_whitespace()
                    .next()
                    .unwrap_or_default()
                    .to_string(),
            )
        }))
        .collect();
    assert!(!actions.is_empty(), "工作流应至少有一个 uses 动作");
    let toolchain_actions: Vec<&String> = actions
        .iter()
        .filter(|a| {
            let repo = a.split('@').next().unwrap_or(a).to_ascii_lowercase();
            repo.contains("toolchain") || repo.contains("setup-rust")
        })
        .collect();
    assert!(
        toolchain_actions.is_empty(),
        "工作流不得使用第三方工具链动作（会另立版本来源）：{toolchain_actions:?}"
    );

    // 形态四：工具链配置键 / 环境变量（名称含 toolchain 即命中）。
    let mut config_hits = Vec::new();
    for (n, line) in lines.iter().enumerate() {
        let trimmed = line.trim_start();
        for key in [
            "toolchain:",
            "rust-version:",
            "rustup-toolchain:",
            "channel:",
        ] {
            if trimmed.starts_with(key) {
                config_hits.push(format!("{}:{}:{key}", WORKFLOW, n + 1));
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
                config_hits.push(format!("{}:{}:{token}", WORKFLOW, n + 1));
            }
        }
    }
    assert!(
        config_hits.is_empty(),
        "工作流不得以配置键/环境变量声明工具链版本：{config_hits:?}"
    );

    // ---- (5) channel 只由声明文件派生，从不写死 ----
    for (n, line) in lines.iter().enumerate() {
        let trimmed = line.trim_start();
        if let Some(rhs) = trimmed.strip_prefix("channel=") {
            assert!(
                rhs.contains("rust-toolchain.toml") && rhs.contains("sed"),
                "工作流第 {} 行的 channel 赋值未从 rust-toolchain.toml 派生：{line}",
                n + 1
            );
        }
        for needle in ["channel:", "channel ="] {
            assert!(
                !trimmed.starts_with(needle),
                "工作流第 {} 行以配置键写死 channel：{line}",
                n + 1
            );
        }
    }
}
