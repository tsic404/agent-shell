//! systemd init 系统组件（设计文档 §21.13「systemd 服务管理」）。
//!
//! [`SystemdComponent`] 封装 system bus 的 `org.freedesktop.systemd1.Manager`，实现 core
//! 的 [`SystemComponent`] 与 [`DesktopComponent`]；不依赖 DE，是 TTY 后端必选组件之一。
//! zbus 连接构造时建立并持有；D-Bus 错误经 [`agent_shell_core::error::dbus_error`]
//! 归一化（§19）：权限类 → Permission，其余 → DBus。

use agent_shell_core::component::{
    ComponentHealth, ComponentType, DesktopComponent, SystemComponent,
};
use agent_shell_core::error::{AgentShellError, Result};
use agent_shell_core::services::{SystemdTimer, SystemdUnit};
use agent_shell_core::types::UnitStatus;
use async_trait::async_trait;
use std::fmt::Display;
use tracing::debug;
use zbus::zvariant::OwnedObjectPath;

/// D-Bus 错误归一化：委托共享 helper [`agent_shell_core::error::dbus_error`]，
/// 权限类错误名 → [`AgentShellError::Permission`]，其余 → [`AgentShellError::DBus`]。
fn dbus_err<E: Display>(e: E) -> AgentShellError {
    agent_shell_core::error::dbus_error(e)
}

/// `org.freedesktop.systemd1.Manager` 的 zbus proxy。
///
/// 方法签名与 D-Bus 接口逐字对应（§21.13 接口表）：
/// https://www.freedesktop.org/wiki/Software/systemd/dbus/
#[zbus::proxy(
    interface = "org.freedesktop.systemd1.Manager",
    default_service = "org.freedesktop.systemd1",
    default_path = "/org/freedesktop/systemd1"
)]
trait Systemd1Manager {
    // → a(ssssssouso)：name, description, load_state, active_state,
    //   sub_state, follow_unit, object_path, job_id, job_type, job_object_path
    #[allow(clippy::type_complexity)] // D-Bus 签名原样，无法拆分
    fn list_units(
        &self,
    ) -> zbus::Result<
        Vec<(
            String,
            String,
            String,
            String,
            String,
            String,
            OwnedObjectPath,
            u32,
            String,
            OwnedObjectPath,
        )>,
    >;

    fn start_unit(&self, name: &str, mode: &str) -> zbus::Result<OwnedObjectPath>;
    fn stop_unit(&self, name: &str, mode: &str) -> zbus::Result<OwnedObjectPath>;
    fn restart_unit(&self, name: &str, mode: &str) -> zbus::Result<OwnedObjectPath>;
    fn reload_unit(&self, name: &str, mode: &str) -> zbus::Result<OwnedObjectPath>;

    // (asbb) → (ba(sss))
    #[allow(clippy::type_complexity)]
    fn enable_unit_files(
        &self,
        files: &[&str],
        runtime: bool,
        force: bool,
    ) -> zbus::Result<(bool, Vec<(String, String, String)>)>;
    // (asb) → a(sss)
    fn disable_unit_files(
        &self,
        files: &[&str],
        runtime: bool,
    ) -> zbus::Result<Vec<(String, String, String)>>;

    fn reset_failed_unit(&self, name: &str) -> zbus::Result<()>;
    fn reload(&self) -> zbus::Result<()>;

    // (as states, as patterns) → a(ssssssouso)：元组同 list_units。
    // ListTimers 对非特权调用方被 polkit 拒绝（systemd 未在 dbus 策略中放行），
    // ListUnitsByPatterns 是策略放行的等价只读路径（§21.34）。
    #[allow(clippy::type_complexity)] // D-Bus 签名原样，无法拆分
    fn list_units_by_patterns(
        &self,
        states: &[&str],
        patterns: &[&str],
    ) -> zbus::Result<
        Vec<(
            String,
            String,
            String,
            String,
            String,
            String,
            OwnedObjectPath,
            u32,
            String,
            OwnedObjectPath,
        )>,
    >;
}

/// `org.freedesktop.systemd1.Timer` 单元属性（§21.34 触发时间）。
#[zbus::proxy(
    interface = "org.freedesktop.systemd1.Timer",
    default_service = "org.freedesktop.systemd1"
)]
trait Systemd1Timer {
    /// 下次触发（realtime，μs epoch；0 = 未安排）。
    #[zbus(property, name = "NextElapseUSecRealtime")]
    fn next_elapse_usec_realtime(&self) -> zbus::Result<u64>;
    /// 下次触发（monotonic，μs）。
    #[zbus(property, name = "NextElapseUSecMonotonic")]
    fn next_elapse_usec_monotonic(&self) -> zbus::Result<u64>;
    /// 上次触发（realtime，μs epoch；0 = 从未触发）。
    #[zbus(property, name = "LastTriggerUSec")]
    fn last_trigger_usec(&self) -> zbus::Result<u64>;
    /// 上次触发（monotonic，μs）。
    #[zbus(property, name = "LastTriggerUSecMonotonic")]
    fn last_trigger_usec_monotonic(&self) -> zbus::Result<u64>;
}

/// `org.freedesktop.systemd1.Unit` 单元的通用属性（只取 timer 报表所需字段）。
#[zbus::proxy(
    interface = "org.freedesktop.systemd1.Unit",
    default_service = "org.freedesktop.systemd1"
)]
trait Systemd1Unit {
    /// 单元文件路径（如 `/usr/lib/systemd/system/fstrim.timer`）。
    #[zbus(property, name = "FragmentPath")]
    fn fragment_path(&self) -> zbus::Result<String>;
}

/// systemd init 系统组件实例。
pub struct SystemdComponent {
    conn: zbus::Connection,
}

impl SystemdComponent {
    /// D-Bus 方法调用超时：全局默认 5s（design/11 §19.4）。
    const CALL_TIMEOUT: std::time::Duration = std::time::Duration::from_secs(5);
    /// 连接 system bus（systemd 是 Linux 标配；连接失败由装配层降级）。
    pub async fn connect() -> Result<Self> {
        let conn = zbus::connection::Builder::system()
            .map_err(dbus_err)?
            .method_timeout(Self::CALL_TIMEOUT)
            .build()
            .await
            .map_err(|e| AgentShellError::DBus(format!("connect system bus: {e}")))?;
        Ok(Self { conn })
    }

    async fn manager(&self) -> zbus::Result<Systemd1ManagerProxy<'_>> {
        Systemd1ManagerProxy::new(&self.conn).await
    }

    /// 注入既有连接（集成测试用私有总线；生产走 [`Self::connect`]）。
    #[doc(hidden)]
    pub fn with_connection(conn: zbus::Connection) -> Self {
        Self { conn }
    }

    /// 供集成测试验证状态映射域。
    #[doc(hidden)]
    pub fn map_active_state(state: &str) -> UnitStatus {
        match state {
            "active" => UnitStatus::Active,
            "reloading" => UnitStatus::Reloading,
            "inactive" => UnitStatus::Inactive,
            "failed" => UnitStatus::Failed,
            "activating" => UnitStatus::Activating,
            "deactivating" => UnitStatus::Deactivating,
            _ => UnitStatus::Unknown,
        }
    }
    /// ListUnits 元组 `(name, description, load_state, active_state, sub_state,
    /// follow_unit, object_path, job_id, job_type, job_object_path)`
    /// → core [`SystemdUnit`]。
    ///
    /// `enabled`/`main_pid` 不在 ListUnits 元组内：`enabled` 需
    /// GetUnitFileState/ListUnitFiles，保守置 false；`main_pid` 同理为 None。
    fn parse_unit(
        unit: (
            String,
            String,
            String,
            String,
            String,
            String,
            OwnedObjectPath,
            u32,
            String,
            OwnedObjectPath,
        ),
    ) -> SystemdUnit {
        let (
            name,
            description,
            load_state,
            active_state,
            _sub_state,
            _follow_unit,
            _obj_path,
            _job_id,
            _job_type,
            _job_object_path,
        ) = unit;
        debug!(unit = %name, active = %active_state, "parsed unit");
        SystemdUnit {
            name,
            description,
            status: Self::map_active_state(&active_state),
            enabled: false,
            main_pid: None,
            load_state,
        }
    }

    /// 读 timer 单元触发时间属性。单个属性读取失败记 `None`（值未知），与
    /// systemd 的「未安排」（realtime 属性为 0）区分——单元列表仍完整返回，
    /// 不因单点异常吞掉整张表。
    async fn timer_props(&self, path: &str) -> TimerProps {
        let builder = match Systemd1TimerProxy::builder(&self.conn).path(path) {
            Ok(b) => b.cache_properties(zbus::proxy::CacheProperties::No),
            Err(e) => {
                debug!(path, error = %e, "timer proxy path rejected");
                return TimerProps::default();
            }
        };
        let Ok(proxy) = builder.build().await else {
            debug!(path, "timer proxy build failed");
            return TimerProps::default();
        };
        TimerProps {
            next_elapse_real: proxy.next_elapse_usec_realtime().await.ok(),
            next_elapse_monotonic: proxy.next_elapse_usec_monotonic().await.ok(),
            last_trigger_real: proxy.last_trigger_usec().await.ok(),
            last_trigger_monotonic: proxy.last_trigger_usec_monotonic().await.ok(),
        }
    }

    /// 读单元文件路径；读取失败记 `None`（值未知，如运行时单元无 FragmentPath）。
    async fn unit_fragment(&self, path: &str) -> Option<String> {
        let builder = Systemd1UnitProxy::builder(&self.conn).path(path).ok()?;
        let proxy = builder
            .cache_properties(zbus::proxy::CacheProperties::No)
            .build()
            .await
            .ok()?;
        proxy.fragment_path().await.ok()
    }
}

/// timer 单元触发时间属性（μs；`Some(0)` = 未安排，`None` = 读取失败）。
#[derive(Clone, Copy, Default, Debug, PartialEq, Eq)]
struct TimerProps {
    next_elapse_real: Option<u64>,
    next_elapse_monotonic: Option<u64>,
    last_trigger_real: Option<u64>,
    last_trigger_monotonic: Option<u64>,
}

/// timer 单元元组 + 属性 → core [`SystemdTimer`]。
///
/// `active_state == "active"` 即「已装载待触发」——timer 无独立运行态，
/// 该字段是唯一可观测的运行标志。realtime 0 经 [`format_epoch_usec`] 渲染为空串，
/// 属性读取失败保持 `None`（渲染为 `null`）而不是折叠成 0。
fn parse_timer(
    name: String,
    active_state: &str,
    unit_path: Option<String>,
    props: TimerProps,
) -> SystemdTimer {
    SystemdTimer {
        name,
        next_elapse_real: props
            .next_elapse_real
            .map(agent_shell_core::time::format_epoch_usec),
        last_trigger_real: props
            .last_trigger_real
            .map(agent_shell_core::time::format_epoch_usec),
        unit_path,
        next_elapse_monotonic: props.next_elapse_monotonic,
        last_trigger_monotonic: props.last_trigger_monotonic,
        running: active_state == "active",
    }
}

#[async_trait]
impl DesktopComponent for SystemdComponent {
    fn name(&self) -> &'static str {
        "systemd"
    }

    fn component_type(&self) -> ComponentType {
        ComponentType::InitSystem
    }

    /// 同步可用性：system bus 连接已在 [`Self::connect`] 建立即视为可用。
    fn is_available(&self) -> bool {
        true
    }

    /// doctor 健康检查：以 Manager 接口实际可调为准。
    async fn health(&self) -> ComponentHealth {
        let Ok(manager) = self.manager().await else {
            return ComponentHealth::Degraded("org.freedesktop.systemd1 unreachable".to_string());
        };
        match manager.list_units().await {
            Ok(_) => ComponentHealth::Healthy,
            Err(e) => {
                ComponentHealth::Degraded(format!("org.freedesktop.systemd1 unreachable: {e}"))
            }
        }
    }
}

#[async_trait]
impl SystemComponent for SystemdComponent {
    async fn daemon_reload(&self) -> Result<()> {
        self.manager()
            .await
            .map_err(dbus_err)?
            .reload()
            .await
            .map_err(|e| dbus_err(format!("Reload: {e}")))
    }

    async fn list_units(&self) -> Result<Vec<SystemdUnit>> {
        let units = self
            .manager()
            .await
            .map_err(dbus_err)?
            .list_units()
            .await
            .map_err(|e| dbus_err(format!("ListUnits: {e}")))?;
        Ok(units.into_iter().map(Self::parse_unit).collect())
    }

    async fn list_timers(&self) -> Result<Vec<SystemdTimer>> {
        // ListTimers 对非特权调用方恒被 polkit 拒绝，ListUnitsByPatterns 是
        // systemd dbus 策略放行的等价只读入口（§21.34；实测 2026-09）。
        let units = self
            .manager()
            .await
            .map_err(dbus_err)?
            .list_units_by_patterns(&[], &["*.timer"])
            .await
            .map_err(|e| dbus_err(format!("ListUnitsByPatterns(*.timer): {e}")))?;
        let mut timers = Vec::with_capacity(units.len());
        for (name, _desc, _load, active, _sub, _follow, path, _job, _job_type, _job_path) in units {
            let props = self.timer_props(path.as_str()).await;
            let fragment = self.unit_fragment(path.as_str()).await;
            timers.push(parse_timer(name, &active, fragment, props));
        }
        Ok(timers)
    }

    async fn start_unit(&self, name: &str) -> Result<()> {
        // mode "replace"：排队替换既有 job（§21.13 默认模式）。
        self.manager()
            .await
            .map_err(dbus_err)?
            .start_unit(name, "replace")
            .await
            .map(|_| ())
            .map_err(|e| dbus_err(format!("StartUnit({name}): {e}")))
    }

    async fn stop_unit(&self, name: &str) -> Result<()> {
        self.manager()
            .await
            .map_err(dbus_err)?
            .stop_unit(name, "replace")
            .await
            .map(|_| ())
            .map_err(|e| dbus_err(format!("StopUnit({name}): {e}")))
    }

    async fn enable_unit(&self, name: &str) -> Result<()> {
        self.manager()
            .await
            .map_err(dbus_err)?
            .enable_unit_files(&[name], false, false)
            .await
            .map(|_| ())
            .map_err(|e| dbus_err(format!("EnableUnitFiles({name}): {e}")))
    }

    async fn disable_unit(&self, name: &str) -> Result<()> {
        self.manager()
            .await
            .map_err(dbus_err)?
            .disable_unit_files(&[name], false)
            .await
            .map(|_| ())
            .map_err(|e| dbus_err(format!("DisableUnitFiles({name}): {e}")))
    }

    async fn unit_status(&self, name: &str) -> Result<UnitStatus> {
        let units = self.list_units().await?;
        Ok(units
            .into_iter()
            .find(|u| u.name == name)
            .map(|u| u.status)
            .unwrap_or(UnitStatus::Unknown))
    }
}

/// 装配系统服务组件族（systemd + logind），返回 `(init_system, session_manager)`。
///
/// 所有 backend 共享的公共装配逻辑（§3.3 装配矩阵「系统服务」行）：
/// systemd 与 logind 是唯一在所有 Linux 环境（含 TTY）均「必选」的组件族。
/// 探测失败不 panic——返回 `None` 对应 slot，由 doctor 报告 Degraded/Unavailable。
pub async fn assemble_system_services() -> (
    Option<Box<dyn agent_shell_core::component::SystemComponent>>,
    Option<Box<dyn agent_shell_core::component::SessionManagerComponent>>,
) {
    let init_system = match SystemdComponent::connect().await {
        Ok(s) => Some(Box::new(s) as Box<dyn agent_shell_core::component::SystemComponent>),
        Err(e) => {
            tracing::warn!(error = %e, "systemd assembly failed");
            None
        }
    };
    let session_manager = match agent_shell_logind::LogindComponent::connect().await {
        Ok(s) => Some(Box::new(s) as Box<dyn agent_shell_core::component::SessionManagerComponent>),
        Err(e) => {
            tracing::warn!(error = %e, "logind assembly failed");
            None
        }
    };
    (init_system, session_manager)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn dbus_err_maps_permission_error_names() {
        for msg in [
            "Reload: org.freedesktop.DBus.Error.AccessDenied: Permission denied",
            "StartUnit(x): org.freedesktop.DBus.Error.InteractiveAuthorizationRequired: interactive auth required",
            "org.freedesktop.DBus.Error.AuthenticationRequisite: Authentication is required",
            "org.freedesktop.DBus.Error.UnixFD.AccessDenied: fd passing denied",
        ] {
            assert!(
                matches!(dbus_err(msg), AgentShellError::Permission(_)),
                "expected Permission for {msg:?}"
            );
        }
    }

    #[test]
    fn dbus_err_keeps_other_errors_as_dbus() {
        for msg in [
            "Reload: failed",
            "StartUnit(x): Access denied as the requested operation requires interactive authentication",
        ] {
            assert!(
                matches!(dbus_err(msg), AgentShellError::DBus(_)),
                "expected DBus for {msg:?}"
            );
        }
    }

    /// 假 systemd Manager：只需 ListUnitsByPatterns（ListTimers 权限被拒的
    /// 等价只读入口），返回固定元组。
    struct FakeManager {
        unit_path: OwnedObjectPath,
    }

    #[zbus::interface(name = "org.freedesktop.systemd1.Manager")]
    impl FakeManager {
        #[allow(clippy::type_complexity)] // D-Bus 签名原样
        fn list_units_by_patterns(
            &self,
            _states: Vec<String>,
            _patterns: Vec<String>,
        ) -> Vec<(
            String,
            String,
            String,
            String,
            String,
            String,
            OwnedObjectPath,
            u32,
            String,
            OwnedObjectPath,
        )> {
            vec![(
                "backup.timer".to_string(),
                "Nightly backup".to_string(),
                "loaded".to_string(),
                "active".to_string(),
                "waiting".to_string(),
                String::new(),
                self.unit_path.clone(),
                0,
                String::new(),
                OwnedObjectPath::try_from("/").expect("root path"),
            )]
        }
    }

    /// 假 timer 单元：Timer 接口（触发时间）；Unit 接口见 [`FakeUnitProps`]。
    struct FakeTimerUnit {
        next_elapse_real: u64,
        next_elapse_monotonic: u64,
        last_trigger_real: u64,
        last_trigger_monotonic: u64,
    }

    #[zbus::interface(name = "org.freedesktop.systemd1.Timer")]
    impl FakeTimerUnit {
        #[zbus(property, name = "NextElapseUSecRealtime")]
        fn next_elapse_usec_realtime(&self) -> u64 {
            self.next_elapse_real
        }

        #[zbus(property, name = "NextElapseUSecMonotonic")]
        fn next_elapse_usec_monotonic(&self) -> u64 {
            self.next_elapse_monotonic
        }

        #[zbus(property, name = "LastTriggerUSec")]
        fn last_trigger_usec(&self) -> u64 {
            self.last_trigger_real
        }

        #[zbus(property, name = "LastTriggerUSecMonotonic")]
        fn last_trigger_usec_monotonic(&self) -> u64 {
            self.last_trigger_monotonic
        }
    }

    #[zbus::interface(name = "org.freedesktop.systemd1.Unit")]
    impl FakeUnitProps {
        #[zbus(property, name = "FragmentPath")]
        fn fragment_path(&self) -> String {
            self.0.clone()
        }
    }

    /// Unit 接口包装（zbus 一个类型只能实现一个接口，同一路径按类型注册多个）。
    struct FakeUnitProps(String);

    /// 独立私有 session bus：避免并行测试间 `Connection::session()` 环境竞争。
    struct TestBus {
        addr: String,
        _child: std::process::Child,
    }

    impl TestBus {
        async fn start() -> Self {
            use std::process::Stdio;
            let mut child = std::process::Command::new("dbus-daemon")
                .args(["--session", "--nofork", "--print-address=1"])
                .stdout(Stdio::piped())
                .stderr(Stdio::null())
                .spawn()
                .expect("dbus-daemon must be installed for systemd timer tests");
            let stdout = child.stdout.take().expect("piped stdout");
            let addr = read_address_line(stdout);
            assert!(
                addr.starts_with("unix:"),
                "dbus-daemon printed unexpected address: {addr:?}"
            );
            Self {
                addr,
                _child: child,
            }
        }

        async fn connect(&self) -> zbus::Connection {
            zbus::connection::Builder::address(self.addr.as_str())
                .expect("dbus-daemon address must parse")
                .build()
                .await
                .expect("connect to private session bus")
        }
    }

    impl Drop for TestBus {
        fn drop(&mut self) {
            // SIGTERM 而非 SIGKILL：让 dbus-daemon 走正常退出路径清理 socket。
            // SAFETY: `_child.id()` 是存活的子进程 PID，发信号无内存安全风险。
            unsafe { libc::kill(self._child.id() as i32, libc::SIGTERM) };
            let _ = self._child.wait();
        }
    }

    /// 逐字节读 `dbus-daemon --print-address=1` 的地址行（恰好一行）。
    fn read_address_line(stdout: std::process::ChildStdout) -> String {
        use std::io::Read as _;
        let mut reader = std::io::BufReader::new(stdout);
        let mut bytes = Vec::new();
        loop {
            let mut buf = [0u8; 1];
            match reader.read_exact(&mut buf) {
                Ok(()) => {}
                Err(e) if e.kind() == std::io::ErrorKind::UnexpectedEof => break,
                Err(e) => panic!("read dbus-daemon address: {e}"),
            }
            bytes.push(buf[0]);
            if buf[0] == b'\n' {
                break;
            }
        }
        let line = String::from_utf8(bytes).expect("dbus-daemon address must be UTF-8");
        assert!(!line.is_empty(), "dbus-daemon printed no address line");
        line.trim_end_matches('\n').to_string()
    }

    #[test]
    fn parse_timer_maps_trigger_times_and_running_flag() {
        let props = TimerProps {
            next_elapse_real: Some(1_790_521_200_000_000),
            next_elapse_monotonic: Some(42_000_000),
            last_trigger_real: Some(1_790_393_550_992_423),
            last_trigger_monotonic: Some(692_180_889),
        };
        let timer = parse_timer(
            "shadow.timer".into(),
            "active",
            Some("/usr/lib/systemd/system/shadow.timer".into()),
            props,
        );
        assert_eq!(timer.name, "shadow.timer");
        assert_eq!(
            timer.next_elapse_real.as_deref(),
            Some("2026-09-27T15:00:00Z")
        );
        assert_eq!(
            timer.last_trigger_real.as_deref(),
            Some("2026-09-26T03:32:30Z")
        );
        assert_eq!(timer.next_elapse_monotonic, Some(42_000_000));
        assert_eq!(timer.last_trigger_monotonic, Some(692_180_889));
        assert_eq!(
            timer.unit_path.as_deref(),
            Some("/usr/lib/systemd/system/shadow.timer")
        );
        assert!(timer.running);
    }

    #[test]
    fn parse_timer_renders_unscheduled_as_empty_and_inactive_as_not_running() {
        // realtime 属性为 0（未安排）→ Some("")；属性读不到 → None（不得混同）。
        let unscheduled = TimerProps {
            next_elapse_real: Some(0),
            last_trigger_real: Some(0),
            ..TimerProps::default()
        };
        let timer = parse_timer("oneshot.timer".into(), "inactive", None, unscheduled);
        assert_eq!(timer.next_elapse_real.as_deref(), Some(""));
        assert_eq!(timer.last_trigger_real.as_deref(), Some(""));
        assert_eq!(timer.next_elapse_monotonic, None);
        assert_eq!(timer.unit_path, None);
        assert!(!timer.running);

        // 属性读取失败（None）必须保持 None，不能折叠成「未安排」的空串/0。
        let unreadable = parse_timer("broken.timer".into(), "active", None, TimerProps::default());
        assert_eq!(unreadable.next_elapse_real, None);
        assert_eq!(unreadable.last_trigger_real, None);
        assert_eq!(unreadable.next_elapse_monotonic, None);
        assert_eq!(unreadable.last_trigger_monotonic, None);
        assert!(unreadable.running);
    }

    #[tokio::test]
    async fn list_timers_reads_units_and_properties_over_the_bus() {
        let bus = TestBus::start().await;
        let server = bus.connect().await;
        let unit_path = OwnedObjectPath::try_from("/org/freedesktop/systemd1/unit/backup_2etimer")
            .expect("object path");
        server
            .object_server()
            .at(
                "/org/freedesktop/systemd1",
                FakeManager {
                    unit_path: unit_path.clone(),
                },
            )
            .await
            .expect("register fake manager");
        server
            .object_server()
            .at(
                unit_path.as_str(),
                FakeTimerUnit {
                    next_elapse_real: 1_790_521_200_000_000,
                    next_elapse_monotonic: 7_000,
                    last_trigger_real: 1_790_393_550_992_423,
                    last_trigger_monotonic: 9_000,
                },
            )
            .await
            .expect("register fake timer unit");
        server
            .object_server()
            .at(
                unit_path.as_str(),
                FakeUnitProps("/usr/lib/systemd/system/backup.timer".into()),
            )
            .await
            .expect("register fake unit properties");
        use zbus::names::WellKnownName;
        let name =
            WellKnownName::try_from("org.freedesktop.systemd1".to_string()).expect("valid name");
        server.request_name(name).await.expect("claim systemd1");

        let component = SystemdComponent::with_connection(bus.connect().await);
        let timers = component.list_timers().await.expect("list_timers");
        assert_eq!(timers.len(), 1);
        let timer = &timers[0];
        assert_eq!(timer.name, "backup.timer");
        assert_eq!(
            timer.unit_path.as_deref(),
            Some("/usr/lib/systemd/system/backup.timer")
        );
        assert_eq!(
            timer.next_elapse_real.as_deref(),
            Some("2026-09-27T15:00:00Z")
        );
        assert_eq!(
            timer.last_trigger_real.as_deref(),
            Some("2026-09-26T03:32:30Z")
        );
        assert_eq!(timer.next_elapse_monotonic, Some(7_000));
        assert_eq!(timer.last_trigger_monotonic, Some(9_000));
        assert!(timer.running);
    }

    #[tokio::test]
    async fn list_timers_marks_unreadable_timer_properties_as_null() {
        // 单元在列但 Timer 接口不可读（属性调用失败）→ 触发时间必须是 None，
        // 不得折叠成 0/空串（那会被读成「确实没有下次触发」）。
        let bus = TestBus::start().await;
        let server = bus.connect().await;
        let unit_path = OwnedObjectPath::try_from("/org/freedesktop/systemd1/unit/broken_2etimer")
            .expect("object path");
        server
            .object_server()
            .at(
                "/org/freedesktop/systemd1",
                FakeManager {
                    unit_path: unit_path.clone(),
                },
            )
            .await
            .expect("register fake manager");
        use zbus::names::WellKnownName;
        let name =
            WellKnownName::try_from("org.freedesktop.systemd1".to_string()).expect("valid name");
        server.request_name(name).await.expect("claim systemd1");

        let component = SystemdComponent::with_connection(bus.connect().await);
        let timers = component.list_timers().await.expect("list_timers");
        assert_eq!(timers.len(), 1);
        let timer = &timers[0];
        assert_eq!(timer.name, "backup.timer");
        assert_eq!(
            timer.next_elapse_real, None,
            "unreadable property is unknown"
        );
        assert_eq!(timer.last_trigger_real, None);
        assert_eq!(timer.next_elapse_monotonic, None);
        assert_eq!(timer.last_trigger_monotonic, None);
        assert_eq!(timer.unit_path, None);
        assert!(timer.running);
    }

    #[tokio::test]
    async fn list_timers_surfaces_missing_manager_as_error() {
        let bus = TestBus::start().await;
        let component = SystemdComponent::with_connection(bus.connect().await);
        let err = component
            .list_timers()
            .await
            .expect_err("no systemd1 on this bus");
        assert!(
            err.to_string().contains("ListUnitsByPatterns"),
            "error must name the failed call: {err}"
        );
    }
}
