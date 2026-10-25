//! TC-tech-07（AC-112，P1）：兜底判定分支的判别力——折叠为恒定通过。
//!
//! - 场景：把兜底判定分支的结局折叠为恒定通过
//! - 判据：折叠后相对未变异基线新增至少一条失败，且该失败来自断言兜底结果不得
//!   冒充通过的用例；新增失败为零即判该装置对该分支无判别力
//! - 执行：`cargo test -p agent-shell-acceptance --test TC-tech-07`；基线反证由
//!   TC-tech-02/03/04/05 取证时执行，变异体只替换结局、同一反证必须失败。

use acceptance::apparatus::{self, Branch, Evidence, Fold, Judge, Verdict};

/// 恒定通过的判定（自检用）。
fn always_pass(_: &Evidence) -> Verdict {
    Verdict::Pass
}

/// 恒成功的反证（自检用）：任何判定都放行。
fn never_fails(_: &Judge<'_>) -> Result<(), String> {
    Ok(())
}

/// 恒定通过且反证恒成功的分支。
fn blind_branch() -> Branch {
    Branch {
        id: "自检：恒定通过的分支",
        judge: always_pass,
        control: never_fails,
    }
}

#[test]
fn fallback_branches_reject_constant_pass() {
    let detected = apparatus::assert_fold_detected(apparatus::FALLBACK_BRANCHES, Fold::ToPass)
        .unwrap_or_else(|e| panic!("{e}"));
    assert_eq!(
        detected.len(),
        apparatus::FALLBACK_BRANCHES.len(),
        "每条兜底分支折叠后都必须新增失败（来自断言该分支的反证）：{detected:?}"
    );

    let blind = blind_branch();
    assert!(
        apparatus::assert_fold_detected(std::slice::from_ref(&blind), Fold::ToPass).is_err(),
        "反证恒成功的分支必须被裁决报为无判别力——否则本裁决恒真"
    );
}
