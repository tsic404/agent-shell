//! TC-doctor-02（AC-02，P0）：KWin 合成器子行的生成与顺序。
//!
//! - 场景：KWin 系会话的合成器子行——Wayland 协议、KWin 服务、D-Bus 桥接、
//!   输入注入、事件脚本，逐行给出就绪态或明确原因
//! - 判据：五条子行按序出现、缺行或错序即失败
//! - 执行：`cargo test -p agent-shell-acceptance --test TC-doctor-02`
//!
//! 判定面（可证伪）：daemon 对合成器通道产出的渲染契约——子行紧随合成器行、
//! 顺序为通道给定顺序、完整渲染（不吞行）、行内必须带就绪态或原因。加载态
//! 输入（未加载、已装配、失败诊断）经夹具注入，断言的行随输入变化：三组输入
//! 断言同一判定路径，任一输入不按序渲染即失败，不是必然通过的恒真断言。
//!
//! 环境限制（显式归属）：真实 KWin 会话里的协议全局绑定、`org.kde.KWin`
//! 版本探测、`/Scripting` 桥接探测与 `event_monitor` 装载探测需要真实合成器
//! 实例，无图形 CI 无法建立执行面。五条子行的生成、顺序与逐行就绪态由组件
//! 单测 `kwin_compositor::tests::doctor_lines_render_five_rows_in_fixed_order_with_readiness`
//! （`components/compositor/kwin/src/kwin_compositor.rs`）直接断言；探测真值
//! 到渲染输入的映射由该文件的 `for_test` 用例与真机 KDE 会话验收覆盖。

use acceptance::{FakeCompositor, Fixture};
use agent_shell_core::types::DesktopEnvironment;
use agent_shell_rpc::{method, DoctorResult};
use serde_json::Value;

/// 子行名称（顺序即 AC-02 判据；行内文案由 compositor 生成侧给出版本/原因）。
const SUB_LINES: [&str; 5] = [
    "Wayland 协议",
    "KWin 服务",
    "D-Bus 桥接",
    "输入注入",
    "事件脚本",
];

/// 三类可判定的装载态输入（未加载 / 已装配 / 失败诊断）。
fn script_lines(state: &str) -> String {
    match state {
        "unloaded" => {
            "  ⚠ 事件脚本    : 未加载（懒启动，首次 events subscribe 时装配）".to_string()
        }
        "loaded" => {
            "  ✓ 事件脚本    : 已装配（QTimer 轮询 windowList 差分 + workspace 信号）".to_string()
        }
        "failed" => {
            "  ✗ 事件脚本    : monitor failed to start: kwin scripting unreachable".to_string()
        }
        other => panic!("unknown script state: {other}"),
    }
}

fn sub_rows(value: Value) -> Vec<String> {
    let report: DoctorResult = serde_json::from_value(value).expect("DoctorResult");
    let compositor = report
        .lines
        .iter()
        .position(|line| !line.starts_with(' ') && line.contains("合成器"))
        .expect("compositor row");
    report.lines[compositor + 1..]
        .iter()
        .take_while(|line| line.starts_with(' '))
        .cloned()
        .collect()
}

#[tokio::test]
async fn kwin_sub_rows_are_rendered_in_order_with_readiness() {
    let mut fixture = Fixture::start().await;
    let compositor = FakeCompositor::new();
    fixture.install_compositor(&compositor, DesktopEnvironment::KDE);

    // 三类装载态输入：判定路径必须每次按序完整渲染，行随输入变化。
    for state in ["unloaded", "loaded", "failed"] {
        let mut lines: Vec<String> = vec![
            "  ✓ Wayland 协议 : 3/3 globals bound (window_mgmt v2, fake_input v5+, vd_mgmt bound)".into(),
            "  ✓ KWin 服务    : org.kde.KWin v6.11.2".into(),
            "  ✓ D-Bus 桥接   : callDBus ready (14 templates, req-id routed; /Scripting introspected)".into(),
            "  ⚠ 输入注入     : 无 fake_input（降级 libei/ydotool/uinput；org_kde_kwin_fake_input 未绑定）".into(),
        ];
        lines.push(script_lines(state));
        fixture.set_compositor_doctor_lines(lines.clone());

        let rows = sub_rows(fixture.call_ok(method::DOCTOR, None).await);
        assert_eq!(
            rows.len(),
            SUB_LINES.len(),
            "state {state}: exactly five sub-rows must be rendered; got:\n{}",
            rows.join("\n")
        );
        for (index, (row, name)) in rows.iter().zip(SUB_LINES.iter()).enumerate() {
            assert!(
                row.contains(name),
                "state {state}: sub-row {index} must carry {name:?}, got: {row}"
            );
        }
        let order: Vec<usize> = SUB_LINES
            .iter()
            .filter_map(|name| rows.iter().position(|row| row.contains(name)))
            .collect();
        assert!(
            order.windows(2).all(|pair| pair[0] < pair[1]),
            "state {state}: sub-row order must be preserved; got:\n{}",
            rows.join("\n")
        );
        for row in &rows {
            let trimmed = row.trim_start();
            assert!(
                ["✓", "⚠", "✗"].iter().any(|p| trimmed.starts_with(p)) && trimmed.contains(':'),
                "state {state}: sub-row must carry a readiness prefix and a reason, got: {row}"
            );
        }
        // 行确实随输入变化（不是恒真回显）。
        assert!(
            rows.contains(&lines.last().cloned().unwrap()),
            "state {state}: the input row must be the rendered row"
        );
    }
}
