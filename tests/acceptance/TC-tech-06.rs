//! TC-tech-06（AC-70/AC-111，P1）：亮度失败分类三分支的端到端覆盖。
//!
//! - 场景：亮度失败分类三分支——候选后端正常、全部候选后端不可用、候选后端存在
//!   但查询或设置失败
//! - 判据：三分支分别得到成功载荷、后端不可用（含可执行提示）与后端失败（保留
//!   根因），且覆盖由以端到端或 CI 集成级验证承载契约分支的测试实跑承担，按
//!   TC-tech-01 至 TC-tech-05 的可见性与实跑判据判定
//! - 执行：`cargo test -p agent-shell-acceptance --test TC-tech-06`
//!
//! 三分支以真实 `agent-shell-daemon` 装配路径实跑（`Fixture::start_with_env` →
//! `Daemon::connect` → `BrightnessController::new`），请求走真实 `dispatch`
//! （`brightness.get` / `brightness.set`），与生产回执同源：
//! - 候选后端正常：私有 session bus 上的 mock powerdevil 返回亮度，回执为
//!   result（40% 读回、set 50% 落 125 绝对值）。
//! - 全部候选后端不可用：私有 bus 无人占用 powerdevil 名，`brightnessctl` 被
//!   空 PATH 隐去、sysfs 根指空目录——回执 1002 BackendUnavailable 并含安装
//!   指引（可执行提示）。
//! - 候选后端存在但查询/设置失败：mock powerdevil 名字被占用但调用报错——回执
//!   1005 BackendError 且保留 powerdevil 根因与降级链其余层根因，不得折叠为
//!   后端不可用。
//!
//! 环境变量是进程级状态，本目标保持单个 `#[tokio::test]`；三分支按序装配并各自
//! 覆盖 `DBUS_SESSION_BUS_ADDRESS`，互不干扰。

use acceptance::Fixture;
use agent_shell_rpc::{method, RpcErrorCode};
use serde_json::json;
use std::io::{BufReader, Read};
use std::process::{Child, Command, Stdio};
use std::sync::{Arc, Mutex};

/// powerdevil 亮度接口坐标（与 `components/power` 的 proxy 声明一致）。
const POWERDEVIL_SERVICE: &str = "org.kde.Solid.PowerManagement";
const POWERDEVIL_PATH: &str = "/org/kde/Solid/PowerManagement/Actions/BrightnessControl";

/// 私有 session bus：与真机总线（真实 powerdevil）及并行测试隔离。
///
/// socket 落在夹具私有临时目录（`tempfile::tempdir` 创建为 0700，仅本用户可
/// 进入）的 `bus.sock`，不接受多用户机器上其他本地用户的连接与名称抢占；
/// 配置为运行时写出的独立文件（`<type>session</type>` + `<allow own="*"/>`
/// 允许进程占用 well-known 服务名），不依赖宿主机 `/etc/dbus-1/session.conf`
/// 的存在与策略；`XDG_DATA_DIRS` 指向空目录关闭标准 service 激活目录，避免
/// 占用空名时激活宿主机真实 powerdevil。
struct PrivateBus {
    addr: String,
    child: Child,
    _config_dir: tempfile::TempDir,
}

impl PrivateBus {
    fn start() -> Self {
        let config_dir = tempfile::tempdir().expect("dbus config dir");
        std::fs::set_permissions(
            config_dir.path(),
            std::os::unix::fs::PermissionsExt::from_mode(0o700),
        )
        .expect("restrict dbus sandbox permissions");
        let socket = config_dir.path().join("bus.sock");
        let config = config_dir.path().join("bus.conf");
        std::fs::write(
            &config,
            format!(
                r#"<!DOCTYPE busconfig PUBLIC "-//freedesktop//DTD D-Bus Bus Configuration 1.0//EN" "http://www.freedesktop.org/standards/dbus/1.0/busconfig.dtd">
<busconfig>
  <type>session</type>
  <listen>unix:path={socket}</listen>
  <policy context="default">
    <allow own="*"/>
    <allow send_destination="*" eavesdrop="true"/>
    <allow eavesdrop="true"/>
  </policy>
  <limit name="max_incoming_bytes">10000000</limit>
</busconfig>
"#,
                socket = socket.display()
            ),
        )
        .expect("write dbus config");
        let mut child = Command::new("dbus-daemon")
            .args(["--nofork", "--print-address=1", "--config-file"])
            .arg(&config)
            .env("XDG_DATA_DIRS", config_dir.path())
            .stdout(Stdio::piped())
            .stderr(Stdio::null())
            .spawn()
            .expect("dbus-daemon is required for the brightness acceptance e2e");
        let stdout = child.stdout.take().expect("piped stdout");
        let addr = read_address_line(stdout);
        assert!(
            addr.starts_with("unix:"),
            "dbus-daemon printed unexpected address: {addr:?}"
        );
        Self {
            addr,
            child,
            _config_dir: config_dir,
        }
    }

    async fn connect(&self) -> zbus::Connection {
        zbus::connection::Builder::address(self.addr.as_str())
            .expect("dbus-daemon address must parse")
            .build()
            .await
            .expect("connect to private session bus")
    }

    /// 在私有 bus 上注册 `iface` 并占用 powerdevil 服务名（服务「存在」）。
    async fn serve<I>(&self, iface: I) -> zbus::Connection
    where
        I: zbus::object_server::Interface + Send + Sync + 'static,
    {
        let server = self.connect().await;
        server
            .object_server()
            .at(POWERDEVIL_PATH, iface)
            .await
            .expect("register mock powerdevil");
        let name =
            zbus::names::WellKnownName::try_from(POWERDEVIL_SERVICE).expect("valid bus name");
        server
            .request_name(name)
            .await
            .expect("claim powerdevil name");
        server
    }
}

impl Drop for PrivateBus {
    fn drop(&mut self) {
        // SIGTERM 让 dbus-daemon 走正常退出路径清理 socket；SIGKILL 会残留 stale 文件。
        // SAFETY: `id()` 是存活子进程 PID，发 SIGTERM 无内存安全风险。
        unsafe { libc::kill(self.child.id() as i32, libc::SIGTERM) };
        let _ = self.child.wait();
    }
}

/// 逐字节读地址行：`dbus-daemon --print-address=1` 恰好一行。
fn read_address_line(stdout: std::process::ChildStdout) -> String {
    let mut reader = BufReader::new(stdout);
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

/// 正常 powerdevil：记录 `setBrightness` 入参，供断言百分比换算。
#[derive(Clone, Default)]
struct WorkingPowerdevil {
    sets: Arc<Mutex<Vec<i32>>>,
}

#[zbus::interface(name = "org.kde.Solid.PowerManagement.Actions.BrightnessControl")]
impl WorkingPowerdevil {
    #[zbus(name = "brightness")]
    fn brightness(&self) -> zbus::fdo::Result<i32> {
        Ok(100)
    }

    #[zbus(name = "brightnessMax")]
    fn brightness_max(&self) -> zbus::fdo::Result<i32> {
        Ok(250)
    }

    #[zbus(name = "setBrightness")]
    fn set_brightness(&self, value: i32) -> zbus::fdo::Result<()> {
        self.sets.lock().expect("sets mutex").push(value);
        Ok(())
    }
}

/// 调用必失败的 powerdevil：名字被占用（服务在）但查询/设置报错。
struct FailingPowerdevil;

#[zbus::interface(name = "org.kde.Solid.PowerManagement.Actions.BrightnessControl")]
impl FailingPowerdevil {
    #[zbus(name = "brightness")]
    fn brightness(&self) -> zbus::fdo::Result<i32> {
        Err(zbus::fdo::Error::Failed("powerdevil query failed".into()))
    }

    #[zbus(name = "brightnessMax")]
    fn brightness_max(&self) -> zbus::fdo::Result<i32> {
        Ok(250)
    }

    #[zbus(name = "setBrightness")]
    fn set_brightness(&self, value: i32) -> zbus::fdo::Result<()> {
        Err(zbus::fdo::Error::Failed(format!(
            "powerdevil set {value} failed"
        )))
    }
}

/// 环境覆盖公共部分：sysfs 根指空目录（宿主真实背光设备不得抢先成功）、
/// PATH 置空（隐去 `brightnessctl`）、XDG_DATA_DIRS 指空（关闭标准 session
/// service 目录，避免激活宿主机真实 powerdevil）。分类只由 powerdevil 层决定。
fn isolate_env(
    sysfs_root: &std::path::Path,
    data_dir: &std::path::Path,
) -> Vec<(&'static str, String)> {
    vec![
        (
            "AGENT_SHELL_BACKLIGHT_SYSFS_ROOT",
            sysfs_root.display().to_string(),
        ),
        ("PATH", String::new()),
        ("XDG_DATA_DIRS", data_dir.display().to_string()),
    ]
}

#[tokio::test]
async fn brightness_failure_classification_three_branches() {
    // 隔离目录：sysfs 空根 + 空 data dir。
    let sandbox = tempfile::tempdir().expect("tempdir");
    let sysfs_root = sandbox.path().join("sysfs");
    let data_dir = sandbox.path().join("data");
    std::fs::create_dir_all(&sysfs_root).expect("create sysfs root");
    std::fs::create_dir_all(&data_dir).expect("create data dir");

    // 三个私有 bus 先全部启动：`Fixture::start_with_env` 会把 PATH 置空（隐去
    // `brightnessctl`），此后进程内无法再 spawn `dbus-daemon`；bus 是独立
    // 进程，不受夹具环境覆盖影响。
    let bus_working = PrivateBus::start();
    let bus_failing = PrivateBus::start();
    let bus_absent = PrivateBus::start();
    let mock_working = WorkingPowerdevil::default();
    let _server_working = bus_working.serve(mock_working.clone()).await;
    let _server_failing = bus_failing.serve(FailingPowerdevil).await;

    // ── 分支 1：候选后端正常 ──
    let mut extra = isolate_env(&sysfs_root, &data_dir);
    extra.push(("DBUS_SESSION_BUS_ADDRESS", bus_working.addr.clone()));
    let mut fixture = Fixture::start_with_env(&extra).await;

    let states = fixture.call_ok(method::BRIGHTNESS_GET, None).await;
    assert_eq!(states[0]["monitor"], json!("default"), "{states}");
    assert_eq!(states[0]["brightness"], json!(40), "{states}");
    let result = fixture
        .call_ok(method::BRIGHTNESS_SET, Some(json!({"value": 50})))
        .await;
    assert_eq!(result["status"], json!("ok"), "{result}");
    assert_eq!(
        *mock_working.sets.lock().expect("sets mutex"),
        vec![125],
        "50% × 250 = 125 必须到达 powerdevil"
    );
    drop(fixture);

    // ── 分支 3：候选后端存在但查询/设置失败 ──
    let mut extra = isolate_env(&sysfs_root, &data_dir);
    extra.push(("DBUS_SESSION_BUS_ADDRESS", bus_failing.addr.clone()));
    let mut fixture = Fixture::start_with_env(&extra).await;

    let error = fixture.call_err(method::BRIGHTNESS_GET, None).await;
    assert_eq!(
        error.code,
        RpcErrorCode::BackendError as i32,
        "powerdevil 存在但查询失败应为 BackendError（exit 1），got {error:?}"
    );
    assert!(
        error.message.contains("powerdevil brightness"),
        "{}",
        error.message
    );
    // 降级链合并上报：其余层根因与 powerdevil 根因一并保留。
    assert!(
        error.message.contains("brightnessctl not installed"),
        "{}",
        error.message
    );
    let error = fixture
        .call_err(method::BRIGHTNESS_SET, Some(json!({"value": 50})))
        .await;
    assert_eq!(
        error.code,
        RpcErrorCode::BackendError as i32,
        "powerdevil 存在但设置失败应为 BackendError（exit 1），got {error:?}"
    );
    assert!(
        error.message.contains("powerdevil setBrightness"),
        "{}",
        error.message
    );
    drop(fixture);

    // ── 分支 2：全部候选后端不可用（bus_absent 无人占用 powerdevil 名）──
    let mut extra = isolate_env(&sysfs_root, &data_dir);
    extra.push(("DBUS_SESSION_BUS_ADDRESS", bus_absent.addr.clone()));
    let mut fixture = Fixture::start_with_env(&extra).await;

    let error = fixture.call_err(method::BRIGHTNESS_GET, None).await;
    assert_eq!(
        error.code,
        RpcErrorCode::BackendUnavailable as i32,
        "全部候选后端不可用应为 BackendUnavailable（exit 2），got {error:?}"
    );
    assert!(
        error.message.contains("pacman -S brightnessctl"),
        "{}",
        error.message
    );
    assert!(!error.message.contains("powerdevil"), "{}", error.message);
    drop(fixture);
    drop(bus_absent);
    drop(bus_failing);
    drop(bus_working);
}
