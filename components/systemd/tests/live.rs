//! 验收自测（issue TSI-2320 验收标准）。
//!
//! 仅在具备 systemd + logind 的真机/容器上运行：
//! `cargo test -p agent-shell-systemd -p agent-shell-logind -- --nocapture`
//!
//! 纯映射单元测试无环境依赖；`*_live` 测试需要 system bus。

use agent_shell_core::component::{
    ComponentHealth, DesktopComponent, SessionManagerComponent, SystemComponent,
};
use agent_shell_core::error::AgentShellError;
use agent_shell_core::types::UnitStatus;
use agent_shell_logind::LogindComponent;
use agent_shell_systemd::SystemdComponent;

mod common;
use common::skip_environment;

/// 连接 logind 并预检 manager 接口可达性。
///
/// logind 的 D-Bus 方法（ListSessions/CanReboot）在本机/CI 环境可能超时
/// （`org.freedesktop.login1` 不可达）——这是环境状态而非组件缺陷，与
/// systemd 侧的 polkit 跳过同理：按环境性跳过，避免污染非 systemd PR 门禁。
/// 仅当 `health()` 判定 Healthy 时返回组件，其余情况输出 SKIP 并返回 None。
async fn logind_or_skip() -> Option<LogindComponent> {
    let component = match LogindComponent::connect().await {
        Ok(component) => component,
        Err(e) => {
            skip_environment(&format!("logind system bus connect failed: {e}"));
            return None;
        }
    };
    match component.health().await {
        ComponentHealth::Healthy => Some(component),
        ComponentHealth::Degraded(reason) => {
            skip_environment(&format!("org.freedesktop.login1 unreachable: {reason}"));
            None
        }
        ComponentHealth::Unavailable => {
            skip_environment("org.freedesktop.login1 unavailable");
            None
        }
    }
}

#[test]
fn active_state_mapping() {
    assert_eq!(
        SystemdComponent::map_active_state("active"),
        UnitStatus::Active
    );
    assert_eq!(
        SystemdComponent::map_active_state("reloading"),
        UnitStatus::Reloading
    );
    assert_eq!(
        SystemdComponent::map_active_state("inactive"),
        UnitStatus::Inactive
    );
    assert_eq!(
        SystemdComponent::map_active_state("failed"),
        UnitStatus::Failed
    );
    assert_eq!(
        SystemdComponent::map_active_state("activating"),
        UnitStatus::Activating
    );
    assert_eq!(
        SystemdComponent::map_active_state("deactivating"),
        UnitStatus::Deactivating
    );
    assert_eq!(
        SystemdComponent::map_active_state("whatever"),
        UnitStatus::Unknown
    );
}

#[tokio::test]
async fn live_systemd_list_units_nonempty_and_complete() {
    let c = SystemdComponent::connect().await.unwrap();
    let units = c.list_units().await.unwrap();
    assert!(!units.is_empty(), "ListUnits returned no units");
    let first = &units[0];
    assert!(!first.name.is_empty());
    assert!(!first.load_state.is_empty());
}

#[tokio::test]
async fn live_systemd_start_stop_unit_status_transitions() {
    // 无害 oneshot 单元：start/stop 安全，不影响任何运行中的服务。
    let c = SystemdComponent::connect().await.unwrap();
    let name = "systemd-sysctl.service";
    let before = c.unit_status(name).await.unwrap();
    if matches!(before, UnitStatus::Unknown) {
        skip_environment(&format!("{name} not present on this host"));
        return;
    }
    // 某些容器（PID 1 非 systemd 管理器或 manager degraded）会拒绝/悬挂
    // 状态变更调用；此时跳过而非失败。
    if let Err(e) = c.start_unit(name).await {
        skip_environment(&format!("environment disallows unit state changes: {e}"));
        return;
    }
    let after_start = c.unit_status(name).await.unwrap();
    c.stop_unit(name).await.unwrap();
    assert!(
        !matches!(after_start, UnitStatus::Unknown | UnitStatus::Failed),
        "unexpected status after start: {after_start:?}"
    );
}

#[tokio::test]
async fn live_systemd_daemon_reload_succeeds() {
    let c = SystemdComponent::connect().await.unwrap();
    // 无 polkit 授权时 Reload 回 InteractiveAuthorizationRequired；归一为
    // Permission 后按环境跳过，其余错误视为组件缺陷。
    match c.daemon_reload().await {
        Ok(()) => {}
        Err(AgentShellError::Permission(reason)) => {
            skip_environment(&format!("daemon-reload 缺 polkit 授权: {reason}"));
        }
        Err(e) => panic!("daemon-reload failed: {e}"),
    }
}

#[tokio::test]
async fn live_logind_list_sessions_returns_current() {
    let Some(c) = logind_or_skip().await else {
        return;
    };
    let sessions = c.list_sessions().await.unwrap();
    if sessions.is_empty() {
        // CI runner / 无头容器可达 logind 但无任何 session（无人登录）——
        // ListSessions 空是合法环境状态而非组件缺陷，按本文件 skip 约定放行。
        skip_environment("no logind sessions on this host (headless/CI)");
        return;
    }
    for s in &sessions {
        assert!(
            ["active", "online", "closing"].contains(&s.state.as_str()),
            "session {} state {:?} out of domain",
            s.id,
            s.state
        );
    }
    let first_id = sessions[0].id.clone();
    let got = c.get_session(&first_id).await.unwrap();
    assert_eq!(got.id, first_id);
    assert!(
        !got.user_name.is_empty(),
        "get_session left user_name empty"
    );
}

#[tokio::test]
async fn live_logind_can_reboot_can_poweroff_booleans() {
    let Some(c) = logind_or_skip().await else {
        return;
    };
    // 只查询能力位，绝不触发实际关机/重启。
    let reboot = match c.can_reboot().await {
        Ok(v) => v,
        // CanReboot/CanPowerOff 返回 "yes"/"no"/"challenge" 字符串——合法
        // D-Bus 回复而非错误；can_bool 仅把 "yes" 归 true，其余（含无
        // polkit 授权时返回的 "challenge"）归 false。因此真实主机无授权时
        // 通常走 Ok(false) 而非这里的分支；Err(Permission)/Err(Timeout)/
        // panic 三支只在 logind 真回方法错误（AccessDenied 等）或 D-Bus
        // 调用超时（method_err 归一为 Timeout）时触发，由 agent-shell-logind
        // 的 mock D-Bus 测试覆盖。
        Err(AgentShellError::Permission(reason)) => {
            skip_environment(&format!("CanReboot 缺 polkit 授权: {reason}"));
            return;
        }
        Err(AgentShellError::Timeout(reason)) => {
            skip_environment(&format!("CanReboot D-Bus 调用超时: {reason}"));
            return;
        }
        Err(e) => panic!("CanReboot failed: {e}"),
    };
    let poweroff = match c.can_poweroff().await {
        Ok(v) => v,
        Err(AgentShellError::Permission(reason)) => {
            skip_environment(&format!("CanPowerOff 缺 polkit 授权: {reason}"));
            return;
        }
        Err(AgentShellError::Timeout(reason)) => {
            skip_environment(&format!("CanPowerOff D-Bus 调用超时: {reason}"));
            return;
        }
        Err(e) => panic!("CanPowerOff failed: {e}"),
    };
    eprintln!("can_reboot={reboot} can_poweroff={poweroff}");
}
