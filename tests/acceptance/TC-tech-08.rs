//! TC-tech-08（AC-112，P1）：不可判定或不可用结果的判别力——折叠为跳过。
//!
//! - 场景：把不可判定或不可用结果的结局折叠为跳过（含移除强制判定生效的门控）
//! - 判据：折叠后相对未变异基线新增至少一条失败，且该失败来自断言判定必须生效
//!   时缺少依据不得以跳过收场的用例；新增失败为零即判该装置对该分支无判别力
//! - 执行：`cargo test -p agent-shell-acceptance --test TC-tech-08`；基线反证由
//!   TC-tech-03/04/05 取证时执行，变异体只替换结局、同一反证必须失败。

use acceptance::apparatus::{self, Branch, Evidence, Fold, Judge, Verdict};

/// 恒定跳过的判定（自检用）。
fn always_skip(_: &Evidence) -> Verdict {
    Verdict::Skip("自检".into())
}

/// 恒成功的反证（自检用）：任何判定都放行。
fn never_fails(_: &Judge<'_>) -> Result<(), String> {
    Ok(())
}

/// 恒定跳过且反证恒成功的分支。
fn blind_branch() -> Branch {
    Branch {
        id: "自检：恒定跳过的分支",
        judge: always_skip,
        control: never_fails,
    }
}

#[test]
fn undeterminable_branches_reject_constant_skip() {
    let detected =
        apparatus::assert_fold_detected(apparatus::UNDETERMINABLE_BRANCHES, Fold::ToSkip)
            .unwrap_or_else(|e| panic!("{e}"));
    assert_eq!(
        detected.len(),
        apparatus::UNDETERMINABLE_BRANCHES.len(),
        "每条不可判定分支折叠后都必须新增失败（来自断言该分支的反证）：{detected:?}"
    );

    let blind = blind_branch();
    assert!(
        apparatus::assert_fold_detected(std::slice::from_ref(&blind), Fold::ToSkip).is_err(),
        "反证恒成功的分支必须被裁决报为无判别力——否则本裁决恒真"
    );
}
