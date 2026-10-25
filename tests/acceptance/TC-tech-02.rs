//! TC-tech-02（AC-111，P1）：测试源不含跳过标记与整文件条件编译豁免。
//!
//! - 场景：测试源不含跳过标记与整文件条件编译豁免
//! - 判据：测试源无忽略标记、无整文件条件编译豁免；出现任一即判失败；同文件内
//!   的普通条件断言不在此列。取证目标名单与用例目标全集不一致即判失败
//! - 执行：`cargo test -p agent-shell-acceptance --test TC-tech-02`
//!
//! 取证对象由包清单的 `[[test]]` 声明与目录遍历**双向核对**得到：清单声明的
//! 每个目标路径都必须是目录内的源文件，目录内每个 `*.rs`（排除 `src/`）也都
//! 必须在清单中声明——新增用例目标漏声明或漏扫描都判失败。名单
//! （`apparatus::TC_TECH_TARGETS`）另与目录内 `TC-tech-*` 用例目标全集核对：
//! 删去任一成员即判取证家族不完整，其余目标全部通过也不构成覆盖证据。匹配器
//! 与反证入口在 `acceptance::apparatus`（折行属性按括号平衡拼接后判定），与
//! 折叠判别力用例（TC-tech-07）同源。
//!
//! 夹具的 `headless_session` 是进程级环境修改，本目标不装配 daemon，无副作用。

use acceptance::apparatus::{
    self, Evidence, SKIP_MARKERS_BRANCH, TARGET_FAMILY_BRANCH, TC_TECH_TARGETS,
};
use acceptance::workspace_root;
use std::path::{Path, PathBuf};

/// 包清单里 `[[test]]` 声明的目标源文件（相对包根，保持声明顺序）。
fn manifest_test_paths(manifest: &str) -> Vec<String> {
    let mut paths = Vec::new();
    let mut in_test = false;
    for line in manifest.lines() {
        let trimmed = line.trim();
        if trimmed.starts_with('[') {
            in_test = trimmed == "[[test]]";
            continue;
        }
        if !in_test {
            continue;
        }
        let Some(rest) = trimmed.strip_prefix("path") else {
            continue;
        };
        let Some(rest) = rest.trim_start().strip_prefix('=') else {
            continue;
        };
        let value = rest.trim().trim_matches('"');
        if !value.is_empty() {
            paths.push(value.to_string());
        }
    }
    paths
}

/// 目录内直接子级的 `*.rs` 文件（`src/` 是子目录，不在其列），排序后返回。
fn directory_sources(dir: &Path) -> Vec<String> {
    let mut names: Vec<String> = std::fs::read_dir(dir)
        .unwrap_or_else(|e| panic!("read {}: {e}", dir.display()))
        .flatten()
        .filter_map(|entry| entry.file_name().into_string().ok())
        .filter(|name| name.ends_with(".rs"))
        .collect();
    names.sort();
    names
}

/// 目录内 `TC-tech-*.rs` 对应的用例标识全集。
fn tc_tech_family(sources: &[String]) -> Vec<String> {
    sources
        .iter()
        .filter_map(|name| name.strip_suffix(".rs"))
        .filter(|case| case.starts_with("TC-tech-"))
        .map(str::to_string)
        .collect()
}

#[test]
fn no_ignore_markers_or_file_level_cfg_in_acceptance_sources() {
    // 基线反证：合法标记必须被判失败，近似串与注释必须放行；名单缺成员或含
    // 家族外成员必须被判失败，完全一致必须放行。
    apparatus::assert_baseline(&[SKIP_MARKERS_BRANCH, TARGET_FAMILY_BRANCH])
        .unwrap_or_else(|e| panic!("{e}"));

    let dir = workspace_root().join("tests/acceptance");
    let manifest = std::fs::read_to_string(dir.join("Cargo.toml"))
        .unwrap_or_else(|e| panic!("read {}: {e}", dir.join("Cargo.toml").display()));
    let declared = manifest_test_paths(&manifest);
    let discovered = directory_sources(&dir);

    // 双向核对：清单与目录必须一一对应（新增目标漏声明 / 漏扫描都判失败）。
    let mut declared_sorted = declared.clone();
    declared_sorted.sort();
    assert_eq!(
        declared_sorted, discovered,
        "tests/acceptance/Cargo.toml 的 [[test]] 声明与目录内 *.rs 源文件不一一对应：\
         声明 {declared:?}，目录 {discovered:?}"
    );
    assert!(
        !declared.is_empty(),
        "tests/acceptance/Cargo.toml 未声明任何 [[test]] 目标"
    );

    // 取证目标家族不得收窄：名单必须含目录内全部 TC-tech 用例目标。
    let family: Vec<String> = TC_TECH_TARGETS
        .iter()
        .map(|target| (*target).to_string())
        .collect();
    apparatus::judge_target_family(&Evidence::TargetFamily {
        declared: family,
        discovered: tc_tech_family(&discovered),
    })
    .require_pass("TC-tech-02 · 取证目标家族");

    for relative in &declared {
        let path: PathBuf = dir.join(relative);
        let text = std::fs::read_to_string(&path)
            .unwrap_or_else(|e| panic!("read {}: {e}", path.display()));
        // 判据标识带上文件路径；失败与「不可判定」都必须当场失败，不得以跳过收场。
        let criterion = format!("TC-tech-02 · {}", path.display());
        apparatus::judge_skip_markers(&Evidence::TestSource(text)).require_pass(&criterion);
    }
}
