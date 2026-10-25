//! AC-111/AC-112 的取证判据与折叠判别力。
//!
//! 每条判据分支以 [`Evidence`] 为输入给出 [`Verdict`]：取证目标用真实判定对真实
//! 证据取证，反证入口断言判定必须生效时不得以通过或跳过收场。[`assert_fold_detected`]
//! 把分支结局折叠为恒定通过/跳过、其余输入与执行条件不变，用同一反证入口重跑：
//! 不新增失败即判该分支无判别力（判据语义见设计仓 `03-acceptance.md`）。
//!
//! 取证判定不返回 [`Verdict::Skip`]：依据缺失必须判失败——折叠成跳过正是判别力
//! 用例要检出的静默失效。

use serde_json::Value;

/// 被测包在根工作区清单与构建元数据中的标识。
pub const ACCEPTANCE_PACKAGE: &str = "agent-shell-acceptance";

/// 被测包在仓库内的路径（根清单 `exclude` 以路径列入）。
pub const ACCEPTANCE_PATH: &str = "tests/acceptance";

/// AC-111 取证目标名单（TC-tech 家族）。
///
/// 折叠判别力用例（TC-tech-07/08）在名单内：AC-112 的反证由它们承担，且「受
/// AC-111 的执行判据约束」——可见性、测试腿启用与 CI 执行行的取证必须覆盖它们。
pub const TC_TECH_TARGETS: &[&str] = &[
    "TC-tech-01",
    "TC-tech-02",
    "TC-tech-03",
    "TC-tech-04",
    "TC-tech-05",
    "TC-tech-06",
    "TC-tech-07",
    "TC-tech-08",
];

/// 判据分支的结局：未通过不等于跳过。
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Verdict {
    /// 该项义务成立。
    Pass,
    /// 该项义务不成立，附面向读者的原因。
    Fail(String),
    /// 未取得判定依据——不得据此判通过。
    Skip(String),
}

impl Verdict {
    /// 取证判定必须通过：未通过与不可判定都当场失败（判据标识由调用方给出）。
    pub fn require_pass(self, criterion: &str) {
        match self {
            Self::Pass => {}
            Self::Fail(reason) => panic!("{reason}（判据 {criterion} 不满足）"),
            Self::Skip(reason) => {
                panic!("{criterion} 未取得依据（{reason}）——不得以跳过收场")
            }
        }
    }
}

/// 单点折叠的方向（AC-112 的两种静默失效形态）。
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Fold {
    /// 把分支结局折叠为恒定通过。
    ToPass,
    /// 把分支结局折叠为恒定跳过。
    ToSkip,
}

impl Fold {
    /// 折叠后的恒定结局。
    pub fn verdict(self) -> Verdict {
        match self {
            Self::ToPass => Verdict::Pass,
            Self::ToSkip => Verdict::Skip("折叠".into()),
        }
    }

    /// 报告用标签。
    pub fn label(self) -> &'static str {
        match self {
            Self::ToPass => "恒定通过",
            Self::ToSkip => "恒定跳过",
        }
    }
}

/// 判据事实：分支判定的输入（取证材料或反证夹具）。
#[derive(Clone, Debug)]
pub enum Evidence {
    /// 单个测试源文件全文。
    TestSource(String),
    /// 测试包清单全文。
    PackageManifest(String),
    /// 根工作区清单全文。
    RootManifest(String),
    /// 构建元数据（`cargo metadata --no-deps` 的 JSON）。
    BuildMetadata(String),
    /// 测试目标运行日志（目标名 + 日志全文）。
    RunLog { target: String, text: String },
    /// 取证目标家族（名单声明 + 用例目标全集）。
    TargetFamily {
        declared: Vec<String>,
        discovered: Vec<String>,
    },
}

/// 判定函数：给定取证事实，决定一项契约义务是否成立。
pub type Judge<'a> = dyn Fn(&Evidence) -> Verdict + 'a;

/// 反证入口：断言判定在必须生效时不得以通过或跳过收场。
pub type Control = for<'a, 'b> fn(&'a Judge<'b>) -> Result<(), String>;

/// 折叠：分支照常执行，只把结局替换为恒定值。
pub fn fold<'a, 'b: 'a>(judge: &'a Judge<'b>, folded: Fold) -> impl Fn(&Evidence) -> Verdict + 'a {
    move |evidence| {
        // 分支本体照常执行（其余输入与执行条件不变），只有结局被替换。
        let _ = judge(evidence);
        folded.verdict()
    }
}

/// 一条判据分支：真实判定与它的反证入口。
pub struct Branch {
    /// 判定点标识（归属用例 + 判定点）。
    pub id: &'static str,
    /// 真实判定——与取证目标使用的判定同源。
    pub judge: fn(&Evidence) -> Verdict,
    /// 反证入口——取证目标在基线上也执行它。
    pub control: Control,
}

/// 基线反证：真实判定必须通过各分支的反证入口。
///
/// 取证目标在真实证据上调用它——判据分支必须能对违规事实判失败，否则该分支
/// 在未变异状态下就已静默失效。
pub fn assert_baseline(branches: &[Branch]) -> Result<(), String> {
    for branch in branches {
        let id = branch.id;
        let judge: &Judge = &branch.judge;
        (branch.control)(judge).map_err(|reason| {
            format!("{id}：基线反证未通过——真实判定必须能对该分支判失败：{reason}")
        })?;
    }
    Ok(())
}

/// 折叠判别力裁决：逐分支先跑基线反证，再跑折叠后的同一反证。
///
/// 基线反证必须通过（真实判定能判失败）；折叠后反证必须失败。返回每条分支在
/// 折叠后观测到的新增失败文本；任何分支折叠后仍通过即整体判失败。
pub fn assert_fold_detected(branches: &[Branch], folded: Fold) -> Result<Vec<String>, String> {
    assert_baseline(branches)?;
    let mut detected = Vec::new();
    let mut unguarded = Vec::new();
    for branch in branches {
        let id = branch.id;
        let judge: &Judge = &branch.judge;
        match (branch.control)(&fold(judge, folded)) {
            Err(reason) => detected.push(format!("{id}：{reason}")),
            Ok(()) => unguarded.push(id),
        }
    }
    if !unguarded.is_empty() {
        return Err(format!(
            "折叠为{}后反证仍通过——以下分支无判别力：{}",
            folded.label(),
            unguarded.join("、")
        ));
    }
    Ok(detected)
}

/// 断言判定为失败（反证的判据形态）。
fn must_fail(judge: &Judge, evidence: Evidence, case: &str) -> Result<(), String> {
    match judge(&evidence) {
        Verdict::Fail(_) => Ok(()),
        other => Err(format!("{case}必须判失败，实际为 {other:?}")),
    }
}

/// 断言判定为通过（反证的另一侧：判定不得恒失败）。
fn must_pass(judge: &Judge, evidence: Evidence, case: &str) -> Result<(), String> {
    match judge(&evidence) {
        Verdict::Pass => Ok(()),
        other => Err(format!("{case}必须判通过，实际为 {other:?}")),
    }
}

// ── TC-tech-02：跳过标记与整文件条件编译豁免 ──

/// `#[<name>` 属性头且后继 `]` / `=`（可夹空白）。
///
/// 用于把 `#[ignore]` / `#[ignore = "…"]` 与 `#[ignore_something]` 区分开；
/// `name = "!cfg("` 时命中整文件豁免 `#![cfg(…)]` 的头。
fn attribute_head(line: &str, name: &str) -> bool {
    let needle = format!("#[{name}");
    line.find(&needle).is_some_and(|pos| {
        let after = line[pos + needle.len()..].trim_start();
        after.starts_with(']') || after.starts_with('=')
    })
}

/// 行内是否含独立 `ignore` 词元（`#[cfg_attr(…, ignore)]` 的参数判定）。
fn contains_ignore_token(line: &str) -> bool {
    line.match_indices("ignore").any(|(idx, _)| {
        let before_ok = line[..idx]
            .chars()
            .next_back()
            .is_none_or(|c| !c.is_alphanumeric() && c != '_');
        let after_ok = line[idx + "ignore".len()..]
            .chars()
            .next()
            .is_none_or(|c| !c.is_alphanumeric() && c != '_');
        before_ok && after_ok
    })
}

/// 一条完整属性的 `[` / `]` 是否配平（字符串字面量内的括号不计数）。
fn is_bracket_balanced(text: &str) -> bool {
    let mut depth = 0i32;
    let mut in_string = false;
    let mut escaped = false;
    for ch in text.chars() {
        if in_string {
            if escaped {
                escaped = false;
            } else if ch == '\\' {
                escaped = true;
            } else if ch == '"' {
                in_string = false;
            }
            continue;
        }
        match ch {
            '"' => in_string = true,
            '[' => depth += 1,
            ']' => depth -= 1,
            _ => {}
        }
    }
    depth == 0
}

/// 从测试源里找首个跳过/豁免属性：折行属性按 `[` 平衡拼接后再判。
///
/// 格式化器会把超长属性折行；按行扫描会整条漏掉。返回起始行号（1 起）与拼接后
/// 的属性文本；属性未收口（源码残缺）也按已拼内容判定，避免静默放行。
fn find_skip_or_exemption(source: &str) -> Option<(usize, String)> {
    let mut lines = source.lines().enumerate();
    while let Some((index, line)) = lines.next() {
        let trimmed = line.trim();
        if !(trimmed.starts_with("#[") || trimmed.starts_with("#![")) {
            continue;
        }
        let mut attribute = trimmed.to_string();
        while !is_bracket_balanced(&attribute) {
            let Some((_, next)) = lines.next() else {
                break;
            };
            attribute.push(' ');
            attribute.push_str(next.trim());
        }
        if is_skip_or_exemption(&attribute) {
            return Some((index + 1, attribute));
        }
    }
    None
}

/// 一条完整属性是否构成跳过/豁免标记（判据逐字核对）。
///
/// 只认以 `#[` 开头的属性：注释（`//` / `//!` / `///`）与函数体内的普通断言
/// 不以属性形态出现，不在扫描范围——用例源文件文档里对标记的字面举例不构成
/// 自身漏检。折行形态由 [`find_skip_or_exemption`] 拼接后传入。
pub fn is_skip_or_exemption(line: &str) -> bool {
    let trimmed = line.trim();
    if !(trimmed.starts_with("#[") || trimmed.starts_with("#![")) {
        return false;
    }
    // 忽略标记：`#[ignore]` / `#[ignore = "…"]`（属性头后紧跟 `]` / `=`）。
    let ignore = attribute_head(trimmed, "ignore");
    // `#[cfg_attr(…, ignore)]`：带参属性头 `#[cfg_attr(` + 参数含 `ignore` 词元。
    let cfg_attr_ignore = trimmed.contains("#[cfg_attr(") && contains_ignore_token(trimmed);
    // 整文件条件编译豁免：内层属性 `#![cfg(…)]` / `#![cfg_attr(…)]` 作用于整个
    // crate（文件）；外层 `#[cfg(…)]` 只作用于单个条目，不在本判据内。
    let file_level = trimmed.starts_with("#![cfg(") || trimmed.starts_with("#![cfg_attr(");
    ignore || cfg_attr_ignore || file_level
}

/// 判定：测试源不含跳过标记与整文件条件编译豁免（TC-tech-02）。
pub fn judge_skip_markers(evidence: &Evidence) -> Verdict {
    let Evidence::TestSource(source) = evidence else {
        return Verdict::Fail("取证事实不是测试源全文".into());
    };
    match find_skip_or_exemption(source) {
        Some((lineno, attribute)) => {
            Verdict::Fail(format!("第 {lineno} 行含跳过/豁免标记：{attribute}"))
        }
        None => Verdict::Pass,
    }
}

/// 反证：合法跳过/豁免写法必须被判失败（含折行属性），近似串与注释必须放行。
fn control_skip_markers(judge: &Judge) -> Result<(), String> {
    for (case, marker) in [
        ("`#[ignore]`", "#[ignore]"),
        ("`#[ignore = \"flaky\"]`", r#"#[ignore = "flaky"]"#),
        ("`#[cfg_attr(unix, ignore)]`", "#[cfg_attr(unix, ignore)]"),
        (
            "`#[cfg_attr(feature = \"x\", ignore)]`",
            r#"#[cfg_attr(feature = "x", ignore)]"#,
        ),
        ("`#![cfg(unix)]`", "#![cfg(unix)]"),
        (
            "`#![cfg_attr(unix, allow(dead_code))]`",
            "#![cfg_attr(unix, allow(dead_code))]",
        ),
        // 格式化器折行后的属性：按行扫描会整条漏掉。
        (
            "折行的 `#[cfg_attr(…, ignore)]`",
            "#[cfg_attr(\n    all(unix, feature = \"x\"),\n    ignore\n)]",
        ),
        (
            "折行的 `#![cfg(…)]`",
            "#![cfg(\n    all(unix, target_os = \"linux\")\n)]",
        ),
    ] {
        must_fail(judge, source_with(marker), &format!("含{case}的测试源"))?;
    }
    for allowed in [
        "#[ignore_something]",
        "#[cfg(unix)]",
        "#[cfg_attr(unix, ignored)]",
        "#[allow(\n    dead_code\n)]",
        "#[cfg(\n    unix\n)]",
        "// #[ignore]",
        "/// #[ignore] 的字面举例",
        "assert!(!value);",
        "let ignore = 1;",
    ] {
        must_pass(
            judge,
            source_with(allowed),
            &format!("非标记行 `{allowed}` 的测试源"),
        )?;
    }
    Ok(())
}

/// 以给定行构造单行测试源，验证标记的行首形态。
fn source_with(line: &str) -> Evidence {
    Evidence::TestSource(format!("fn one() -> u8 {{ 1 }}\n{line}\n"))
}

// ── TC-tech-02：取证目标家族 ──

/// 判定：取证目标名单等于家族全集（TC-tech-02）。
///
/// 名单是单点收敛：删去任一成员，该目标退出其余判据的取证范围，逐项判据却
/// 全部通过。家族全集来自环节内的用例目标本身，名单与它不一致即判不完整。
pub fn judge_target_family(evidence: &Evidence) -> Verdict {
    let Evidence::TargetFamily {
        declared,
        discovered,
    } = evidence
    else {
        return Verdict::Fail("取证事实不是取证目标家族".into());
    };
    let mut declared_sorted = declared.clone();
    declared_sorted.sort();
    let mut discovered_sorted = discovered.clone();
    discovered_sorted.sort();
    if declared_sorted == discovered_sorted {
        return Verdict::Pass;
    }
    let missing: Vec<&str> = discovered_sorted
        .iter()
        .map(String::as_str)
        .filter(|case| !declared_sorted.iter().any(|entry| entry.as_str() == *case))
        .collect();
    let extra: Vec<&str> = declared_sorted
        .iter()
        .map(String::as_str)
        .filter(|case| {
            !discovered_sorted
                .iter()
                .any(|entry| entry.as_str() == *case)
        })
        .collect();
    if missing.is_empty() && extra.is_empty() {
        return Verdict::Fail(format!("取证目标名单含重复成员：{declared:?}"));
    }
    Verdict::Fail(format!(
        "取证目标名单与家族全集不一致：缺 {missing:?}，多 {extra:?}"
    ))
}

/// 反证：名单缺成员、含家族外成员与完全一致三种事实必须按语义判定。
fn control_target_family(judge: &Judge) -> Result<(), String> {
    fn family(cases: &[&str]) -> Vec<String> {
        cases.iter().map(|case| (*case).to_string()).collect()
    }
    let all = ["TC-tech-01", "TC-tech-02", "TC-tech-07"];
    must_pass(
        judge,
        Evidence::TargetFamily {
            declared: family(&all),
            discovered: family(&all),
        },
        "名单与家族全集一致时",
    )?;
    must_fail(
        judge,
        Evidence::TargetFamily {
            declared: family(&["TC-tech-01", "TC-tech-02"]),
            discovered: family(&all),
        },
        "名单缺家族成员时",
    )?;
    must_fail(
        judge,
        Evidence::TargetFamily {
            declared: family(&["TC-tech-01", "TC-tech-02", "TC-tech-07", "TC-tech-09"]),
            discovered: family(&all),
        },
        "名单含家族外成员时",
    )?;
    must_fail(
        judge,
        Evidence::TargetFamily {
            declared: Vec::new(),
            discovered: family(&all),
        },
        "名单为空时",
    )?;
    must_fail(
        judge,
        Evidence::TargetFamily {
            declared: family(&["TC-tech-01", "TC-tech-02", "TC-tech-07", "TC-tech-07"]),
            discovered: family(&all),
        },
        "名单含重复成员时",
    )?;
    Ok(())
}

// ── TC-tech-04：根清单排除清单与默认成员集合 ──

/// 根清单的 `exclude` 列表（缺失 = 空）。
///
/// 同时识别单行 `exclude = ["tests/acceptance"]` 与多行数组写法；行内注释与
/// 数组后的残留文本不参与取值。
pub fn excluded_packages(root_manifest: &str) -> Vec<String> {
    let mut excluded = Vec::new();
    let mut lines = root_manifest.lines();
    while let Some(line) = lines.next() {
        let trimmed = line.trim();
        // `exclude` 键（排除 `excluded = …` 这类同前缀键）。
        let Some(rest) = trimmed.strip_prefix("exclude") else {
            continue;
        };
        if !rest.starts_with(|c: char| c.is_whitespace() || c == '=') {
            continue;
        }
        let Some(rest) = rest.trim_start().strip_prefix('=') else {
            continue;
        };
        // 收集数组体直到匹配的 `]`（单行或多行）。
        let mut body = rest.trim().to_string();
        while !body.contains(']') {
            match lines.next() {
                Some(next) => {
                    body.push(' ');
                    body.push_str(next.trim());
                }
                None => break,
            }
        }
        let body = body.split(']').next().unwrap_or_default();
        // 去掉数组开括号：单行 `["x"]` 与多行 `[\n "x",\n]` 的首项都因此干净。
        let body = body.trim_start().trim_start_matches('[');
        for entry in body.split(',') {
            let entry = entry
                .split('#')
                .next()
                .unwrap_or_default()
                .trim()
                .trim_matches('"')
                .trim_matches('\'');
            if !entry.is_empty() && entry != "[" {
                excluded.push(entry.to_string());
            }
        }
    }
    excluded
}

/// 判定：根清单未把被测包列入排除清单（TC-tech-04）。
pub fn judge_no_exclusion(evidence: &Evidence) -> Verdict {
    let Evidence::RootManifest(manifest) = evidence else {
        return Verdict::Fail("取证事实不是根清单全文".into());
    };
    for entry in excluded_packages(manifest) {
        if entry == ACCEPTANCE_PATH {
            return Verdict::Fail(format!("根清单 exclude 剔除了被测包：{entry}"));
        }
    }
    Verdict::Pass
}

/// 反证：四种 `exclude` 形态必须按语义判定。
fn control_no_exclusion(judge: &Judge) -> Result<(), String> {
    must_fail(
        judge,
        Evidence::RootManifest("exclude = [\"tests/acceptance\"]\n".into()),
        "单行 exclude 列入被测包时",
    )?;
    must_fail(
        judge,
        Evidence::RootManifest(
            "[workspace]\nexclude = [\n    \"a\",\n    \"tests/acceptance\",\n]\n".into(),
        ),
        "多行 exclude 列入被测包时",
    )?;
    must_pass(
        judge,
        Evidence::RootManifest("[workspace]\nmembers = [\"tests/acceptance\"]\n".into()),
        "无 exclude 键时",
    )?;
    must_pass(
        judge,
        Evidence::RootManifest("excluded = [\"tests/acceptance\"]\n".into()),
        "同前缀键 excluded 出现时",
    )?;
    must_pass(
        judge,
        Evidence::RootManifest("exclude = [\"another/member\"]\n".into()),
        "排除清单不含被测包时",
    )?;
    Ok(())
}

/// 判定：被测包属于默认成员集合（TC-tech-04）。
pub fn judge_default_members(evidence: &Evidence) -> Verdict {
    let Evidence::BuildMetadata(metadata) = evidence else {
        return Verdict::Fail("取证事实不是构建元数据".into());
    };
    let meta: Value = match serde_json::from_str(metadata) {
        Ok(meta) => meta,
        Err(error) => return Verdict::Fail(format!("构建元数据不可解析：{error}")),
    };
    let Some(members) = meta["workspace_default_members"].as_array() else {
        return Verdict::Fail("构建元数据缺 workspace_default_members（不可判定）".into());
    };
    if members.iter().any(|member| {
        member
            .as_str()
            .is_some_and(|id| id.contains(ACCEPTANCE_PACKAGE))
    }) {
        Verdict::Pass
    } else {
        Verdict::Fail(format!("被测包不在默认成员集合中：{members:?}"))
    }
}

/// 反证：默认成员集合缺失或未含被测包时必须判失败。
fn control_default_members(judge: &Judge) -> Result<(), String> {
    must_fail(
        judge,
        Evidence::BuildMetadata("{\"packages\": []}".into()),
        "构建元数据缺 workspace_default_members 时",
    )?;
    must_fail(
        judge,
        Evidence::BuildMetadata("{\"workspace_default_members\": []}".into()),
        "默认成员集合为空时",
    )?;
    let member = "path+file:///repo/tests/acceptance#agent-shell-acceptance@0.1.0";
    must_pass(
        judge,
        Evidence::BuildMetadata(format!("{{\"workspace_default_members\": [\"{member}\"]}}")),
        "被测包在默认成员集合中时",
    )?;
    Ok(())
}

// ── TC-tech-03：包清单开关与测试腿启用状态 ──

/// 清单里一行是否把某个键设为 `false`（等价排版与行尾注释按语义归一）。
///
/// Cargo 对 `key=false`、`key  =  false`、`key = false # 说明` 三种写法等价；
/// 整行精确串比对只认第一种，后两种会静默放行。
fn is_disabled_flag(line: &str, key: &str) -> bool {
    let declaration = line.split('#').next().unwrap_or_default();
    let Some((name, value)) = declaration.split_once('=') else {
        return false;
    };
    name.trim() == key && value.trim() == "false"
}

/// 判定：包清单未关闭自动测试发现、目标未被显式排除出测试腿（TC-tech-03）。
pub fn judge_package_manifest(evidence: &Evidence) -> Verdict {
    let Evidence::PackageManifest(text) = evidence else {
        return Verdict::Fail("取证事实不是包清单全文".into());
    };
    if text.lines().any(|line| is_disabled_flag(line, "autotests")) {
        return Verdict::Fail("包清单关闭了自动测试发现（autotests = false）".into());
    }
    for declaration in ["test", "harness"] {
        if text.lines().any(|line| is_disabled_flag(line, declaration)) {
            return Verdict::Fail(format!(
                "包清单以 `{declaration} = false` 把测试目标排除出测试腿"
            ));
        }
    }
    Verdict::Pass
}

/// 反证：三种关闭开关及其等价排版逐条必须判失败。
fn control_package_manifest(judge: &Judge) -> Result<(), String> {
    must_fail(
        judge,
        Evidence::PackageManifest("[package]\nautotests = false\n".into()),
        "autotests = false 时",
    )?;
    must_fail(
        judge,
        Evidence::PackageManifest("[package]\nautotests=false\n".into()),
        "无空格 autotests=false 时",
    )?;
    must_fail(
        judge,
        Evidence::PackageManifest("[package]\nautotests  =  false\n".into()),
        "多空格 autotests  =  false 时",
    )?;
    must_fail(
        judge,
        Evidence::PackageManifest("[package]\nautotests = false # 关闭发现\n".into()),
        "行尾注释 autotests = false 时",
    )?;
    must_fail(
        judge,
        Evidence::PackageManifest("[[test]]\nname = \"TC-tech-03\"\ntest = false\n".into()),
        "test = false 时",
    )?;
    must_fail(
        judge,
        Evidence::PackageManifest("[[test]]\nname = \"TC-tech-03\"\ntest=false\n".into()),
        "无空格 test=false 时",
    )?;
    must_fail(
        judge,
        Evidence::PackageManifest("[[test]]\nname = \"TC-tech-03\"\nharness = false\n".into()),
        "harness = false 时",
    )?;
    must_fail(
        judge,
        Evidence::PackageManifest(
            "[[test]]\nname = \"TC-tech-03\"\nharness = false # 自管\n".into(),
        ),
        "行尾注释 harness = false 时",
    )?;
    must_pass(
        judge,
        Evidence::PackageManifest(
            "[package]\nautotests = true\n\n[[test]]\nname = \"TC-tech-03\"\n".into(),
        ),
        "未关闭任何开关时",
    )?;
    must_pass(
        judge,
        Evidence::PackageManifest("[package]\n# autotests = false\nautotests = true\n".into()),
        "注释里的关闭写法不作数时",
    )?;
    Ok(())
}

/// 判定：TC-tech 目标在构建元数据的测试腿中启用（TC-tech-03）。
///
/// 目标缺失是「不可判定」，必须判失败——把它折叠成跳过会静默失去可见性判据。
pub fn judge_test_leg(evidence: &Evidence) -> Verdict {
    let Evidence::BuildMetadata(metadata) = evidence else {
        return Verdict::Fail("取证事实不是构建元数据".into());
    };
    let meta: Value = match serde_json::from_str(metadata) {
        Ok(meta) => meta,
        Err(error) => return Verdict::Fail(format!("构建元数据不可解析：{error}")),
    };
    let Some(package) = meta["packages"]
        .as_array()
        .and_then(|packages| packages.iter().find(|p| p["name"] == ACCEPTANCE_PACKAGE))
    else {
        return Verdict::Fail(format!("被测包不在构建元数据中（不可判定）：{meta}"));
    };
    let Some(targets) = package["targets"].as_array() else {
        return Verdict::Fail("被测包在构建元数据中缺 targets（不可判定）".into());
    };
    for target in TC_TECH_TARGETS {
        let Some(entry) = targets.iter().find(|entry| entry["name"] == *target) else {
            return Verdict::Fail(format!("目标 {target} 不在构建元数据中（不可判定）"));
        };
        if entry["test"].as_bool() != Some(true) {
            return Verdict::Fail(format!("目标 {target} 未在测试腿中启用：{entry}"));
        }
    }
    Verdict::Pass
}

/// 反证：目标缺失、未启用与齐备三种元数据必须按语义判定。
fn control_test_leg(judge: &Judge) -> Result<(), String> {
    must_pass(
        judge,
        metadata_with(|targets| targets),
        "全部目标在测试腿启用时",
    )?;
    must_fail(
        judge,
        metadata_with(|targets| {
            targets
                .into_iter()
                .filter(|target| target["name"] != "TC-tech-03")
                .collect()
        }),
        "目标缺失于构建元数据时",
    )?;
    must_fail(
        judge,
        metadata_with(|targets| {
            targets
                .into_iter()
                .map(|mut target| {
                    if target["name"] == "TC-tech-03" {
                        target["test"] = Value::Bool(false);
                    }
                    target
                })
                .collect()
        }),
        "目标未在测试腿启用时",
    )?;
    must_fail(
        judge,
        Evidence::BuildMetadata("{\"packages\": []}".into()),
        "构建元数据缺被测包时",
    )?;
    Ok(())
}

/// 构造被测包的元数据：目标集合由 `shape` 变换。
fn metadata_with(shape: impl FnOnce(Vec<Value>) -> Vec<Value>) -> Evidence {
    let targets: Vec<Value> = TC_TECH_TARGETS
        .iter()
        .map(|target| serde_json::json!({"name": target, "test": true}))
        .collect();
    Evidence::BuildMetadata(
        serde_json::json!({"packages": [{"name": ACCEPTANCE_PACKAGE, "targets": shape(targets)}]})
            .to_string(),
    )
}

// ── TC-tech-05：运行日志证据 ──

/// 逐目标核对运行日志：执行行、通过计数、零忽略计数（判据逐字核对）。
///
/// 按目标切段：以 `Running <name>.rs` 起、到该段首个 `test result:` 行为止。
/// `Running <name>.rs (target/...)`：目标源文件在包根时 cargo 打印目标名
/// （不带 `tests/` 前缀）；计数只在该目标自己的结果行里核对——其他目标打印
/// 的 `1 passed` 不能替当前目标作证，被过滤到零用例（`0 passed … N filtered
/// out`）即判失败。
pub fn check_target_log_evidence(log: &str, target: &str) -> Result<(), String> {
    let run_line = format!("Running {target}.rs");
    let Some(start) = log.find(&run_line) else {
        return Err(format!("日志缺 {target} 的执行行（{run_line}）：\n{log}"));
    };
    let segment = &log[start..];
    let Some(result_pos) = segment.find("test result:") else {
        return Err(format!("{target} 的执行段缺 test result 行：\n{segment}"));
    };
    let result_line = segment[result_pos..].lines().next().unwrap_or_default();
    let section = &segment[..result_pos + result_line.len()];
    if !result_line.contains("test result: ok.") {
        return Err(format!("{target} 的结果行不是通过形态：{result_line}"));
    }
    // 通过计数与零忽略计数绑定到本目标：恰好 1 个用例通过、0 失败、0 忽略、
    // 0 被过滤（`--test` 过滤到零用例时 cargo 以退出码 0 打印 `0 passed`）。
    for needle in ["1 passed", "0 failed", "0 ignored", "0 filtered out"] {
        if !result_line.contains(needle) {
            return Err(format!(
                "{target} 的结果行缺「{needle}」（通过计数须绑定到该目标）：{result_line}"
            ));
        }
    }
    if section.contains("test result: FAILED") {
        return Err(format!(
            "日志出现失败结果（test result: FAILED）：判据不满足。段：\n{section}"
        ));
    }
    Ok(())
}

/// 判定：该目标在日志中被执行且全部通过（TC-tech-05）。
pub fn judge_log_evidence(evidence: &Evidence) -> Verdict {
    let Evidence::RunLog { target, text } = evidence else {
        return Verdict::Fail("取证事实不是运行日志".into());
    };
    match check_target_log_evidence(text, target) {
        Ok(()) => Verdict::Pass,
        Err(reason) => Verdict::Fail(reason),
    }
}

/// 一条目标的通过段（计数与目标名由调用方给出）。
fn passing_segment(target: &str) -> String {
    format!(
        "     Running {target}.rs (target/debug/deps/{target}-aaaa)\n\
         running 1 test\n\
         test case_pass ... ok\n\n\
         test result: ok. 1 passed; 0 failed; 0 ignored; 0 measured; 0 filtered out; finished in 0.01s\n"
    )
}

/// 一条目标被过滤到零用例的段（cargo 退出码为零的静默形态）。
fn zero_case_segment(target: &str) -> String {
    format!(
        "     Running {target}.rs (target/debug/deps/{target}-bbbb)\n\
         running 0 tests\n\n\
         test result: ok. 0 passed; 0 failed; 0 ignored; 0 measured; 1 filtered out; finished in 0.00s\n"
    )
}

/// 反证：证据存在但义务不成立时必须判失败，通过段必须判通过。
fn control_log_evidence(judge: &Judge) -> Result<(), String> {
    let mixed = format!(
        "{}{}",
        passing_segment("TC-tech-01"),
        zero_case_segment("TC-tech-06")
    );
    must_fail(
        judge,
        log_evidence("TC-tech-06", &mixed),
        "目标被过滤到零用例时",
    )?;
    must_pass(
        judge,
        log_evidence("TC-tech-01", &mixed),
        "目标通过且零忽略时",
    )?;
    let failed = passing_segment("TC-tech-02").replace(
        "test result: ok. 1 passed; 0 failed",
        "test result: FAILED. 0 passed; 1 failed",
    );
    must_fail(judge, log_evidence("TC-tech-02", &failed), "目标出现失败时")?;
    let ignored = passing_segment("TC-tech-04").replace(" 0 ignored", " 1 ignored");
    must_fail(
        judge,
        log_evidence("TC-tech-04", &ignored),
        "目标出现忽略用例时",
    )?;
    Ok(())
}

/// 反证：依据缺失（日志为空、无执行行、无结果行）必须判失败，不得折叠为跳过。
fn control_log_evidence_absent(judge: &Judge) -> Result<(), String> {
    must_fail(judge, log_evidence("TC-tech-02", ""), "日志为空时")?;
    must_fail(
        judge,
        log_evidence("TC-tech-02", "running 0 tests\n"),
        "日志无该目标执行行时",
    )?;
    must_fail(
        judge,
        log_evidence(
            "TC-tech-02",
            "     Running TC-tech-02.rs (target/debug/deps/TC-tech-02-cccc)\nrunning 1 test\n",
        ),
        "执行段无 test result 行时",
    )?;
    must_pass(
        judge,
        log_evidence("TC-tech-02", &passing_segment("TC-tech-02")),
        "完整通过日志出现时",
    )?;
    Ok(())
}

/// 构造运行日志事实。
fn log_evidence(target: &str, text: &str) -> Evidence {
    Evidence::RunLog {
        target: target.to_string(),
        text: text.to_string(),
    }
}

// ── 分支清单 ──

/// TC-tech-02 兜底判定分支。
pub const SKIP_MARKERS_BRANCH: Branch = Branch {
    id: "TC-tech-02 兜底（无跳过标记即通过）",
    judge: judge_skip_markers,
    control: control_skip_markers,
};

/// TC-tech-02 取证目标家族判定分支。
pub const TARGET_FAMILY_BRANCH: Branch = Branch {
    id: "TC-tech-02 兜底（名单与家族全集一致即通过）",
    judge: judge_target_family,
    control: control_target_family,
};

/// TC-tech-04 兜底判定分支。
pub const NO_EXCLUSION_BRANCH: Branch = Branch {
    id: "TC-tech-04 兜底（未列入排除清单即通过）",
    judge: judge_no_exclusion,
    control: control_no_exclusion,
};

/// TC-tech-03 兜底判定分支。
pub const PACKAGE_MANIFEST_BRANCH: Branch = Branch {
    id: "TC-tech-03 兜底（未关闭发现与测试腿即通过）",
    judge: judge_package_manifest,
    control: control_package_manifest,
};

/// TC-tech-05 兜底判定分支。
pub const LOG_EVIDENCE_BRANCH: Branch = Branch {
    id: "TC-tech-05 兜底（日志证据齐全即通过）",
    judge: judge_log_evidence,
    control: control_log_evidence,
};

/// TC-tech-03 不可判定分支。
pub const TEST_LEG_BRANCH: Branch = Branch {
    id: "TC-tech-03 不可判定（目标缺失于构建元数据）",
    judge: judge_test_leg,
    control: control_test_leg,
};

/// TC-tech-04 不可判定分支。
pub const DEFAULT_MEMBERS_BRANCH: Branch = Branch {
    id: "TC-tech-04 不可判定（默认成员集合缺失）",
    judge: judge_default_members,
    control: control_default_members,
};

/// TC-tech-05 不可判定分支。
pub const LOG_EVIDENCE_ABSENT_BRANCH: Branch = Branch {
    id: "TC-tech-05 不可判定（日志缺执行行）",
    judge: judge_log_evidence,
    control: control_log_evidence_absent,
};

/// 兜底判定分支：义务成立以「未发现违规」为兜底，折叠为恒定通过即静默失效。
pub const FALLBACK_BRANCHES: &[Branch] = &[
    SKIP_MARKERS_BRANCH,
    TARGET_FAMILY_BRANCH,
    NO_EXCLUSION_BRANCH,
    PACKAGE_MANIFEST_BRANCH,
    LOG_EVIDENCE_BRANCH,
];

/// 不可判定分支：依据缺失必须判失败，折叠为恒定跳过即静默失效。
pub const UNDETERMINABLE_BRANCHES: &[Branch] = &[
    TEST_LEG_BRANCH,
    DEFAULT_MEMBERS_BRANCH,
    LOG_EVIDENCE_ABSENT_BRANCH,
];
