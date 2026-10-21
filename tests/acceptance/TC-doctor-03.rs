//! TC-doctor-03（AC-03，P1）：桌面环境检测与实际环境及会话类型一致。
//!
//! - 场景：五种目标组合与兜底分支
//! - 判据：检测行与环境实际值一致；兜底差异按已登记环境差异判定
//! - 执行：`cargo test -p agent-shell-acceptance --test TC-doctor-03`
//!
//! 检测顺序由设计固定（环境变量 → 合成器签名 → 会话总线 → 显示变量 → 终端），
//! 本用例逐组合设置环境变量后取 `doctor.run` 的检测行：环境变量命中时检测行
//! 必须报出对应桌面、会话类型与信号来源。兜底分支（无任何信号）只断言不误报
//! 已登记桌面。

use acceptance::Fixture;
use agent_shell_rpc::{method, DoctorResult};
use serde_json::Value;

const ENV_KEYS: [&str; 5] = [
    "XDG_CURRENT_DESKTOP",
    "XDG_SESSION_DESKTOP",
    "WAYLAND_DISPLAY",
    "DISPLAY",
    "HYPRLAND_INSTANCE_SIGNATURE",
];

fn clear_env() {
    for key in ENV_KEYS {
        std::env::remove_var(key);
    }
}

/// 一条检测用例：`(环境覆盖, 期望桌面, 期望会话类型)`。
type DetectionCase<'a> = (&'a [(&'a str, &'a str)], &'a str, &'a str);

async fn detection_row(fixture: &mut Fixture) -> String {
    let value: Value = fixture.call_ok(method::DOCTOR, None).await;
    let report: DoctorResult = serde_json::from_value(value).expect("DoctorResult");
    report
        .lines
        .into_iter()
        .next()
        .expect("detection row is the first line")
}

#[tokio::test]
async fn detection_row_matches_five_target_combinations_and_fallback() {
    let mut fixture = Fixture::start().await;

    // (环境覆盖, 期望桌面, 期望会话类型)
    let cases: [DetectionCase; 5] = [
        (
            &[
                ("XDG_CURRENT_DESKTOP", "KDE"),
                ("WAYLAND_DISPLAY", "wayland-0"),
            ],
            "KDE",
            "Wayland",
        ),
        (
            &[("XDG_CURRENT_DESKTOP", "DDE"), ("DISPLAY", ":0")],
            "DDE",
            "X11",
        ),
        (
            &[("XDG_CURRENT_DESKTOP", "Deepin"), ("DISPLAY", ":0")],
            "DDE",
            "X11",
        ),
        (
            &[
                ("XDG_CURRENT_DESKTOP", "Deepin:KDE"),
                ("WAYLAND_DISPLAY", "wayland-0"),
            ],
            "DDE",
            "Wayland",
        ),
        (
            &[
                ("XDG_CURRENT_DESKTOP", "GNOME"),
                ("WAYLAND_DISPLAY", "wayland-0"),
            ],
            "GNOME",
            "Wayland",
        ),
    ];

    for (env, expected_de, expected_session) in cases {
        clear_env();
        for (key, value) in env {
            std::env::set_var(key, value);
        }
        let row = detection_row(&mut fixture).await;
        assert!(
            row.starts_with("✓ DE 检测"),
            "detection row must be ready: {row}"
        );
        assert!(
            row.contains(expected_de) && row.contains(expected_session),
            "environment {env:?} must detect {expected_de} ({expected_session}); got: {row}"
        );
        assert!(
            row.contains("XDG_CURRENT_DESKTOP"),
            "detection row must name the signal source; got: {row}"
        );
    }

    // 兜底分支：无任何会话信号，不得误报已登记桌面。
    clear_env();
    let row = detection_row(&mut fixture).await;
    assert!(
        row.contains("TTY") || row.contains("unknown-session"),
        "fallback must report TTY or unknown session; got: {row}"
    );
    for de in ["KDE", "DDE", "GNOME", "Hyprland", "Sway"] {
        assert!(
            !row.split(':').next().unwrap_or_default().contains(de),
            "fallback must not claim {de}; got: {row}"
        );
    }
}
