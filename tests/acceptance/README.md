# 验收用例目录

设计仓 `03-acceptance.md` 的用例标识与本目录一一对应：一个 TC 一个测试目标
（`TC-<id>.rs` + `Cargo.toml` 的 `[[test]]`），执行
`cargo test -p agent-shell-acceptance --test <TC-id>`。夹具与隔离约束见 `src/lib.rs`。

本清单不关闭自动测试发现（`autotests = true`，TC-tech-03 判据）；测试目标的启用
状态由构建元数据确认（`cargo metadata` 的 `targets[].test == true`），TC-tech-01、
TC-tech-03、TC-tech-04 据此取证。

判据落在验收流程本身的两条用例（TC-journey-01 签发门禁裁决、TC-journey-02 报告
格式）由 `src/journey.rs` 提供可执行形式：旅程记录器与报告渲染器实现流程判据，
两个测试目标分别驱动真实 `dispatch` 旅程与实测环境记录。

AC-111 的判据分支与反证入口集中在 `src/apparatus.rs`：TC-tech-02/03/04/05 用它
取证，TC-tech-07/08 把分支结局单点折叠为恒定通过／跳过、以同一反证重跑，验证
兜底与不可判定分支确有判别力（AC-112）。

## 无执行形式的用例（豁免声明）

缺陷提交判据（AC-94）落在验收协作流程，被测产品无从提供可执行面，在本目录以本
声明代替执行形式；该用例不得因缺少 `TC-<id>.rs` 被记作缺测试证据。

| 用例 | 归属 | 无可执行面的判据 |
|---|---|---|
| TC-journey-03 内的 AC-94 | AC-94 | 缺陷报告按模板提交；声明见 `TC-journey-03.rs` 顶部文档块，与本表同口径 |

豁免既不计通过也不计失败：逐次裁决仍按 `03-acceptance.md` 的「验收范围与裁决」
执行，本声明不改动用例语义与判据。
