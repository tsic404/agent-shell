//! 验收旅程记录、签发门禁裁决与报告模板。
//!
//! 判据落在验收流程本身（AC-90/AC-91）：旅程按计划顺序执行并逐项记录，裁决只由
//! 逐项结果决定；报告必须带齐提交、分支、环境与逐步退出码表。记录器拒绝乱序、
//! 重复与漏项，渲染器拒绝缺失字段——流程判据因此有可执行形式。

use std::fmt::Write;

/// 跳过步骤的退出码：未执行没有进程退出码，用 -1 显式标注。
pub const EXIT_NOT_RUN: i32 = -1;

/// 用例优先级：只有 P0/P1 参与签发门禁。
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Priority {
    /// 阻断发布。
    P0,
    /// 重要。
    P1,
    /// 可选。
    P2,
}

impl Priority {
    /// 是否阻断签发：P0/P1 未通过即失败，P2 只作标注。
    pub fn is_blocking(self) -> bool {
        !matches!(self, Self::P2)
    }

    /// 报告模板中的固定写法。
    pub fn label(self) -> &'static str {
        match self {
            Self::P0 => "P0",
            Self::P1 => "P1",
            Self::P2 => "P2",
        }
    }
}

/// 单步结果：未跑（跳过）不等于通过。
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Outcome {
    /// 该步通过。
    Passed,
    /// 该步失败。
    Failed,
    /// 该步未执行（环境不可用或范围外）。
    Skipped,
}

impl Outcome {
    /// 报告模板中的固定写法。
    pub fn label(self) -> &'static str {
        match self {
            Self::Passed => "通过",
            Self::Failed => "失败",
            Self::Skipped => "跳过",
        }
    }
}

/// 计划中的一步：标识与优先级在旅程开始前固定。
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct StepSpec {
    /// 用例标识。
    pub id: &'static str,
    /// 用例优先级。
    pub priority: Priority,
}

impl StepSpec {
    /// 构造一步计划。
    pub const fn new(id: &'static str, priority: Priority) -> Self {
        Self { id, priority }
    }
}

/// 已记录的一步：退出码按 C-003 语义（0 成功、1 通用错误、2 不健康/未就绪/未授权）。
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct StepRecord {
    /// 用例标识。
    pub id: &'static str,
    /// 用例优先级。
    pub priority: Priority,
    /// 该步结果。
    pub outcome: Outcome,
    /// 该步的退出码；跳过步骤为 [`EXIT_NOT_RUN`]。
    pub exit_code: i32,
}

/// 签发门禁裁决：阻断项非空即失败；P2 的跳过与失败只作标注。
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Verdict {
    /// P0/P1 中未通过的步骤，非空即失败。
    pub blocking: Vec<&'static str>,
    /// P2 未跑的步骤——不算失败，但必须标注跳过。
    pub skipped: Vec<&'static str>,
    /// P2 跑了但失败的步骤——不阻断签发，仍需标注。
    pub non_blocking_failed: Vec<&'static str>,
}

impl Verdict {
    /// 签发门禁结论：P0 与 P1 全部通过才为真。
    pub fn is_pass(&self) -> bool {
        self.blocking.is_empty()
    }

    /// 裁决单行文本（报告模板的「裁决」字段）。
    pub fn summary(&self) -> String {
        if self.is_pass() {
            let mut text = "通过（P0 全过，P1 全过）".to_string();
            if !self.skipped.is_empty() {
                let _ = write!(text, "；P2 跳过: {}", self.skipped.join(", "));
            }
            if !self.non_blocking_failed.is_empty() {
                let _ = write!(
                    text,
                    "；P2 失败（不阻断）: {}",
                    self.non_blocking_failed.join(", ")
                );
            }
            text
        } else {
            format!("失败（P0/P1 未通过: {}）", self.blocking.join(", "))
        }
    }
}

/// 按计划顺序记录旅程结果。
pub struct Journey {
    plan: Vec<StepSpec>,
    records: Vec<StepRecord>,
}

impl Journey {
    /// 以固定计划开一条新旅程。
    pub fn new(plan: Vec<StepSpec>) -> Self {
        Self {
            plan,
            records: Vec::new(),
        }
    }

    /// 逐项记录（顺序即执行顺序）。
    pub fn records(&self) -> &[StepRecord] {
        &self.records
    }

    /// 记录下一步：计划外、乱序与重复都拒绝——记录面本身就是顺序证据。
    pub fn record(&mut self, id: &str, outcome: Outcome, exit_code: i32) -> Result<(), String> {
        if self.records.iter().any(|record| record.id == id) {
            return Err(format!("步骤 {id} 已记录，不得重复"));
        }
        let expected = self
            .plan
            .get(self.records.len())
            .ok_or_else(|| format!("步骤 {id} 超出计划：计划已全部记录"))?;
        let position = self
            .plan
            .iter()
            .position(|step| step.id == id)
            .ok_or_else(|| format!("步骤 {id} 不在计划内"))?;
        if position != self.records.len() {
            return Err(format!("步骤 {id} 乱序：下一步应为 {}", expected.id));
        }
        let (id, priority) = (expected.id, expected.priority);
        self.records.push(StepRecord {
            id,
            priority,
            outcome,
            exit_code,
        });
        Ok(())
    }

    /// 裁决：全部步骤都有记录才可产出；漏项拒绝，不给出部分裁决。
    pub fn verdict(&self) -> Result<Verdict, String> {
        if self.records.len() != self.plan.len() {
            let missing: Vec<&str> = self.plan[self.records.len()..]
                .iter()
                .map(|step| step.id)
                .collect();
            return Err(format!(
                "旅程未完成，缺少 {} 步: {}",
                missing.len(),
                missing.join(", ")
            ));
        }
        let mut verdict = Verdict {
            blocking: Vec::new(),
            skipped: Vec::new(),
            non_blocking_failed: Vec::new(),
        };
        for record in &self.records {
            match (record.priority, record.outcome) {
                (Priority::P2, Outcome::Skipped) => verdict.skipped.push(record.id),
                (Priority::P2, Outcome::Failed) => verdict.non_blocking_failed.push(record.id),
                (_, Outcome::Passed) => {}
                _ => verdict.blocking.push(record.id),
            }
        }
        Ok(verdict)
    }
}

/// 报告必需字段（AC-91 的字段清单，顺序即模板顺序）。
pub const REPORT_FIELDS: [&str; 6] = [
    "提交",
    "分支",
    "桌面环境",
    "会话类型",
    "二进制版本",
    "桌面组件版本",
];

/// 报告的环境记录：全部字段来自实测，不写占位值。
#[derive(Clone, Debug)]
pub struct EnvironmentRecord {
    /// 被测提交（短标识，脏工作区带脏标记）。
    pub commit: String,
    /// 被测分支（无法取名义分支时写明 detached）。
    pub branch: String,
    /// 实际桌面环境。
    pub desktop: String,
    /// 实际会话类型。
    pub session_type: String,
    /// 被测二进制版本（包版本 + 构建期提交）。
    pub binary_version: String,
    /// 桌面组件 → 版本；无版本面的环境写实测的不可用原因。
    pub desktop_components: Vec<(String, String)>,
}

/// 渲染验收报告：字段缺失、环境记录为空或旅程未完成都拒绝产出。
pub fn render_report(env: &EnvironmentRecord, journey: &Journey) -> Result<String, String> {
    let verdict = journey.verdict()?;
    if env.desktop_components.is_empty() {
        return Err("桌面组件版本缺失：至少需要一项组件记录".into());
    }
    let components = env
        .desktop_components
        .iter()
        .map(|(name, version)| {
            if name.trim().is_empty() || version.trim().is_empty() {
                return Err("桌面组件版本缺失：组件名与版本都不得为空".to_string());
            }
            Ok(format!("{name}={version}"))
        })
        .collect::<Result<Vec<_>, String>>()?
        .join(", ");
    let values = [
        &env.commit,
        &env.branch,
        &env.desktop,
        &env.session_type,
        &env.binary_version,
    ];
    for (label, value) in REPORT_FIELDS[..5].iter().zip(values) {
        if value.trim().is_empty() {
            return Err(format!("{label}缺失：报告字段不得为空"));
        }
    }
    let mut out = String::new();
    let _ = writeln!(out, "# 验收报告");
    let _ = writeln!(out);
    let _ = writeln!(out, "- {}: {}", REPORT_FIELDS[0], env.commit);
    let _ = writeln!(out, "- {}: {}", REPORT_FIELDS[1], env.branch);
    let _ = writeln!(out, "- {}: {}", REPORT_FIELDS[2], env.desktop);
    let _ = writeln!(out, "- {}: {}", REPORT_FIELDS[3], env.session_type);
    let _ = writeln!(out, "- {}: {}", REPORT_FIELDS[4], env.binary_version);
    let _ = writeln!(out, "- {}: {components}", REPORT_FIELDS[5]);
    let _ = writeln!(out, "- 裁决: {}", verdict.summary());
    let _ = writeln!(out);
    let _ = writeln!(out, "## 逐步退出码表");
    let _ = writeln!(out);
    let _ = writeln!(out, "| 步骤 | 优先级 | 结果 | 退出码 |");
    let _ = writeln!(out, "| --- | --- | --- | --- |");
    for record in journey.records() {
        let _ = writeln!(
            out,
            "| {} | {} | {} | {} |",
            record.id,
            record.priority.label(),
            record.outcome.label(),
            record.exit_code
        );
    }
    Ok(out)
}

/// 逐行校验报告模板：必需字段齐全且顺序固定，退出码表与逐项记录逐字段一致。
pub fn validate_report(report: &str, journey: &Journey) -> Result<(), String> {
    let verdict = journey.verdict()?;
    let expected_labels: Vec<&str> = REPORT_FIELDS.iter().copied().chain(["裁决"]).collect();
    let mut first_content = true;
    let mut fields: Vec<(&str, &str)> = Vec::new();
    let mut table: Vec<&str> = Vec::new();
    let mut in_table = false;
    for line in report.lines() {
        let trimmed = line.trim();
        if trimmed.is_empty() {
            continue;
        }
        if first_content {
            if trimmed != "# 验收报告" {
                return Err(format!("报告首行必须是模板标题，实际为 {trimmed:?}"));
            }
            first_content = false;
            continue;
        }
        if trimmed == "## 逐步退出码表" {
            in_table = true;
            continue;
        }
        if let Some(rest) = trimmed.strip_prefix("- ") {
            if in_table {
                return Err(format!("逐步退出码表内出现字段行：{trimmed:?}"));
            }
            let (label, value) = rest
                .split_once(": ")
                .ok_or_else(|| format!("字段行缺少 `: ` 分隔：{trimmed:?}"))?;
            fields.push((label, value.trim()));
            continue;
        }
        if in_table {
            table.push(trimmed);
        }
    }
    let labels: Vec<&str> = fields.iter().map(|(label, _)| *label).collect();
    if labels != expected_labels {
        return Err(format!(
            "字段集合或顺序不符合模板：期望 {expected_labels:?}，实际 {labels:?}"
        ));
    }
    for (label, value) in &fields {
        if value.is_empty() {
            return Err(format!("{label} 字段为空"));
        }
    }
    if fields[6].1 != verdict.summary() {
        return Err(format!(
            "裁决字段与逐项结果不一致：报告为 {:?}，应为 {:?}",
            fields[6].1,
            verdict.summary()
        ));
    }
    if table.len() < 2 {
        return Err("逐步退出码表缺失表头或分隔行".into());
    }
    if table[0] != "| 步骤 | 优先级 | 结果 | 退出码 |" {
        return Err(format!("逐步退出码表表头不符合模板：{:?}", table[0]));
    }
    if table[1] != "| --- | --- | --- | --- |" {
        return Err(format!("逐步退出码表分隔行不符合模板：{:?}", table[1]));
    }
    let rows = &table[2..];
    if rows.len() != journey.records().len() {
        return Err(format!(
            "逐步退出码表行数与逐项记录不一致：表格 {} 行，记录 {} 条",
            rows.len(),
            journey.records().len()
        ));
    }
    for (row, record) in rows.iter().zip(journey.records()) {
        let cells = parse_row(row)?;
        let exit_code = record.exit_code.to_string();
        let expected = [
            record.id,
            record.priority.label(),
            record.outcome.label(),
            exit_code.as_str(),
        ];
        if cells != expected {
            return Err(format!(
                "退出码表行与逐项记录不一致：{cells:?} != {expected:?}"
            ));
        }
    }
    Ok(())
}

/// 解析一行表格：必须以 `|` 首尾闭合，单元格按 `|` 切分并去空白。
fn parse_row(line: &str) -> Result<Vec<&str>, String> {
    if !(line.starts_with('|') && line.ends_with('|')) {
        return Err(format!("表格行必须以 `|` 首尾闭合：{line:?}"));
    }
    Ok(line.trim_matches('|').split('|').map(str::trim).collect())
}

/// 从 doctor 行提取带版本号的桌面组件；没有版本面的行不产出条目。
///
/// 版本取行内首个 `主.次` 形态的数值串（可带 `v` 前缀），因此
/// `org.kde.KWin v6.2.5` 与 `1.10 (SWAYSOCK, roundtrip 0.31ms)` 都取到真实版本，
/// 而 `2/3 globals bound`、`rt 0.31ms` 不会被误判。
pub fn component_versions(doctor_lines: &[String]) -> Vec<(String, String)> {
    doctor_lines
        .iter()
        .filter_map(|line| {
            let (label, detail) = line.split_once(':')?;
            let label = label
                .trim_start_matches(|c: char| !c.is_alphanumeric())
                .trim();
            let version = detail
                .split_whitespace()
                .map(|token| token.trim_matches(|c: char| !c.is_ascii_alphanumeric() && c != '.'))
                .map(|token| token.strip_prefix('v').unwrap_or(token))
                .find(|token| is_dotted_version(token))?;
            Some((label.to_string(), version.to_string()))
        })
        .collect()
}

/// 是否为 `主.次[.补丁]` 形态的版本串。
fn is_dotted_version(token: &str) -> bool {
    let mut parts = token.split('.');
    let major = parts.next().unwrap_or_default();
    let minor = parts.next();
    let all_digits = |part: &str| !part.is_empty() && part.chars().all(|c| c.is_ascii_digit());
    all_digits(major) && minor.is_some_and(all_digits) && parts.all(all_digits)
}
