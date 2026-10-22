//! 私有 session bus：验收夹具与真机总线、并行测试隔离。
//!
//! socket 落在 0700 临时目录；配置允许占用任意 well-known 名，但
//! `XDG_DATA_DIRS` 指向空目录，关闭标准 service 激活——占用空名时不会拉起
//! 宿主机真实服务。用法见 capture / AT-SPI 用例。

use std::io::{BufReader, Read};
use std::process::{Child, Command, Stdio};
use std::sync::Arc;

/// 私有 dbus-daemon 句柄；`Drop` 发 SIGTERM 让 socket 正常回收。
pub struct PrivateBus {
    addr: String,
    child: Child,
    _config_dir: tempfile::TempDir,
}

impl PrivateBus {
    /// 启动独立 `dbus-daemon` 并返回地址。
    pub fn start() -> Self {
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
            .expect("dbus-daemon is required for the headless acceptance fixture");
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

    /// bus 地址（`DBUS_SESSION_BUS_ADDRESS` / `AT_SPI_BUS_ADDRESS` 取值）。
    pub fn address(&self) -> &str {
        &self.addr
    }

    /// 连接到本 bus。
    pub async fn connect(&self) -> zbus::Connection {
        zbus::connection::Builder::address(self.addr.as_str())
            .expect("dbus-daemon address must parse")
            .build()
            .await
            .expect("connect to private session bus")
    }

    /// 在指定路径注册接口，返回承载它的连接（可继续占用服务名）。
    pub async fn serve<I>(&self, path: &str, iface: I) -> zbus::Connection
    where
        I: zbus::object_server::Interface + Send + Sync + 'static,
    {
        let server = self.connect().await;
        server
            .object_server()
            .at(path, iface)
            .await
            .expect("register mock interface");
        server
    }
}

impl Drop for PrivateBus {
    fn drop(&mut self) {
        // SAFETY: `id()` 是存活子进程 PID，发 SIGTERM 无内存安全风险。
        unsafe { libc::kill(self.child.id() as i32, libc::SIGTERM) };
        let _ = self.child.wait();
    }
}

/// 占用 well-known 服务名（同名可重复调用，zbus 幂等）。
pub async fn claim(conn: &zbus::Connection, service: &str) {
    let name = zbus::names::WellKnownName::try_from(service).expect("valid bus name");
    conn.request_name(name).await.expect("claim service name");
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

/// AT-SPI 总线 + Registry 夹具坐标（与 daemon 探测链同源）。
pub const A11Y_BUS_SERVICE: &str = "org.a11y.Bus";
pub const A11Y_STATUS_PATH: &str = "/org/a11y/bus";
pub const A11Y_STATUS_IFACE: &str = "org.a11y.Status";
pub const ATSPI_REGISTRY_SERVICE: &str = "org.a11y.atspi.Registry";
pub const ATSPI_ROOT_PATH: &str = "/org/a11y/atspi/accessible/root";

/// 会话 launcher 的启用位（`org.a11y.Status.IsEnabled`）。
pub struct A11yStatus {
    enabled: bool,
}

impl A11yStatus {
    /// 启用或关闭。
    pub fn new(enabled: bool) -> Self {
        Self { enabled }
    }
}

#[zbus::interface(name = "org.a11y.Status")]
impl A11yStatus {
    #[zbus(property)]
    fn is_enabled(&self) -> bool {
        self.enabled
    }
}

/// Registry 桌面根节点：`ChildCount` = 已注册应用数。
pub struct AtspiRegistry {
    applications: Arc<std::sync::atomic::AtomicI32>,
}

impl AtspiRegistry {
    /// 以给定应用数构造。
    pub fn new(applications: i32) -> Self {
        Self {
            applications: Arc::new(std::sync::atomic::AtomicI32::new(applications)),
        }
    }
}

#[zbus::interface(name = "org.a11y.atspi.Accessible")]
impl AtspiRegistry {
    #[zbus(property)]
    fn child_count(&self) -> i32 {
        self.applications.load(std::sync::atomic::Ordering::Relaxed)
    }
}
