//! TC-system-07（AC-76，P2）：全局快捷键绑定的规范组合键与占位触发。
//!
//! - 场景：真实 `ShortcutBinder`（Hyprland 后端 + 可执行 `hyprctl` stub）经
//!   daemon 的 `shortcut` 装配字段注入，`shortcut.bind` 走真实 `dispatch`。
//! - 判据：回执组合键为规范形式——修饰键按 `ctrl+alt+shift+meta` 顺序排列、
//!   字符键小写、命名键用长名（`return`/`escape`）；后端为 `hyprland` 且片段
//!   文件写入对应 `bind` 行；多键序列与语法错误在触达后端前返回 `-32602`；
//!   `shortcut.trigger` 返回 `{"status":"not_implemented","service":...}`。
//! - 执行：`cargo test -p agent-shell-acceptance --test TC-system-07`
//!
//! 环境限制：验收机没有运行中的 Hyprland 实例；测试创建退出码 0 的
//! `hyprctl` stub，并让 binder 的配置目录落在夹具沙箱内。真实 `hyprctl`
//! 交互（运行期下发）不在 headless 环境可判定，回执规范形式与片段落盘仍由
//! 生产 `ShortcutBinder` 产生，不是 fake。

use std::os::unix::fs::PermissionsExt;
use std::path::{Path, PathBuf};

use acceptance::Fixture;
use agent_shell_core::types::DesktopEnvironment;
use agent_shell_rpc::{method, RpcErrorCode};
use agent_shell_shortcut::{ShortcutBinder, ShortcutOps};
use serde_json::json;

/// 写一个对 `version` / `keyword bind` 都退出 0 的 hyprctl stub。
fn hyprctl_stub(path: &Path) {
    std::fs::write(
        path,
        "#!/bin/sh\n\
         # TC-system-07: 只满足 ready() 与 bind 的退出码语义。\n\
         if [ \"$1\" = \"version\" ]; then echo 'Hyprland 0.40.0'; fi\n\
         exit 0\n",
    )
    .expect("write hyprctl stub");
    let mut permissions = std::fs::metadata(path)
        .expect("stub metadata")
        .permissions();
    permissions.set_mode(0o755);
    std::fs::set_permissions(path, permissions).expect("chmod hyprctl stub");
}

fn assert_invalid(error: agent_shell_rpc::RpcError, what: &str) {
    assert_eq!(
        error.code,
        RpcErrorCode::InvalidParams as i32,
        "{what} 必须返回 -32602，got: {error:?}"
    );
}

#[tokio::test]
async fn shortcut_bind_canonicalizes_real_backend_and_trigger_is_placeholder() {
    let mut fixture = Fixture::start().await;
    let sandbox = fixture.sandbox_root().to_path_buf();
    let stub = sandbox.join("bin").join("hyprctl");
    std::fs::create_dir_all(stub.parent().expect("stub parent")).expect("create bin dir");
    hyprctl_stub(&stub);
    let config_home = sandbox.join("shortcut-config");
    let binder = ShortcutBinder::for_test(
        DesktopEnvironment::Hyprland,
        None,
        sandbox.join("shortcut-data"),
        config_home.clone(),
        PathBuf::from("/nonexistent/gsettings"),
        stub,
    );
    fixture.daemon_mut().shortcut =
        Some(std::sync::Arc::new(binder) as std::sync::Arc<dyn ShortcutOps>);

    // 1. 语法错误先于后端：全部 -32602，且不写片段文件。
    for (params, what) in [
        (json!({ "combo": "meta+t" }), "缺少 action"),
        (json!({ "combo": "meta+t", "action": "   " }), "空白 action"),
        (json!({ "combo": "", "action": "x" }), "空 combo"),
        (json!({ "combo": "nosuchkey", "action": "x" }), "未知键名"),
        (json!({ "combo": "ctrl+alt", "action": "x" }), "仅修饰键"),
        (json!({ "combo": "ctrl+k+c", "action": "x" }), "多键序列"),
        (
            json!({ "combo": "meta+t", "action": "touch a\nexec-once = evil" }),
            "action 含控制字符",
        ),
    ] {
        let error = fixture.call_err(method::SHORTCUT_BIND, Some(params)).await;
        assert_invalid(error, what);
    }
    let fragment = config_home.join("hypr").join("agent-shell.conf");
    assert!(
        !fragment.exists(),
        "非法组合键/动作不得写入快捷键片段：{}",
        fragment.display()
    );

    // 2. 规范形式：修饰键固定顺序 ctrl+alt+shift+meta，字符键小写。
    let binding = fixture
        .call_ok(
            method::SHORTCUT_BIND,
            Some(json!({
                "combo": "CTRL+ALT+SHIFT+META+T",
                "action": "notify-send bound"
            })),
        )
        .await;
    assert_eq!(binding["backend"], "hyprland");
    assert_eq!(binding["combo"], "ctrl+alt+shift+meta+t");
    assert_eq!(binding["action"], "notify-send bound");
    assert_eq!(
        binding["component"],
        fragment.display().to_string(),
        "Hyprland 回执的 component 必须是指向片段的真实路径"
    );
    let body = std::fs::read_to_string(&fragment).expect("fragment written by real binder");
    assert!(
        body.contains("bind = SUPER SHIFT CTRL ALT, T, exec, notify-send bound"),
        "片段必须写入生产 binder 生成的 bind 行，got:\n{body}"
    );
    let main = std::fs::read_to_string(config_home.join("hypr").join("hyprland.conf"))
        .expect("hyprland.conf source line written");
    assert!(
        main.contains(&format!("source = {}", fragment.display())),
        "主配置必须 source 片段，got:\n{main}"
    );

    // 3. 命名键用长名；字符键大小写归一。
    for (combo, canonical) in [
        ("meta+return", "meta+return"),
        ("alt+escape", "alt+escape"),
        ("Shift+Meta+A", "shift+meta+a"),
    ] {
        let binding = fixture
            .call_ok(
                method::SHORTCUT_BIND,
                Some(json!({ "combo": combo, "action": "true" })),
            )
            .await;
        assert_eq!(
            binding["combo"], canonical,
            "{combo} 的规范形式必须是 {canonical}"
        );
    }

    // 4. shortcut.trigger 是占位：成功 + 精确回执，不触达后端。
    let triggered = fixture
        .call_ok(
            method::SHORTCUT_TRIGGER,
            Some(json!({ "combo": "ctrl+alt+shift+meta+t" })),
        )
        .await;
    assert_eq!(
        triggered,
        json!({ "status": "not_implemented", "service": "shortcut.trigger" })
    );

    // 5. 无后端（占位之外的真实 bind）→ 1002。
    fixture.daemon_mut().shortcut = None;
    let error = fixture
        .call_err(
            method::SHORTCUT_BIND,
            Some(json!({ "combo": "meta+t", "action": "true" })),
        )
        .await;
    assert_eq!(
        error.code,
        RpcErrorCode::BackendUnavailable as i32,
        "无快捷键后端时必须返回 1002，got: {error:?}"
    );
}
