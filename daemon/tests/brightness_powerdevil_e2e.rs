//! 真实 daemon 二进制的 powerdevil 亮度层三分支集成测试（§21.25）。
//!
//! 真机 powerdevil 只在有背光硬件的会话导出亮度接口，「服务在但调用失败」在
//! CI/无背光机不可达；本测试用私有 session bus + mock powerdevil 驱动真实
//! `agent-shell-daemon`（`CARGO_BIN_EXE_*` 保证二进制已构建），断言回执的 RPC
//! 错误码：正常 → result；服务在但失败 → 1005 BackendError + 根因；服务不在 →
//! 1002 BackendUnavailable + 安装指引（CLI 退出码映射由 `brightness_rpc_error_*`
//! 单测锚定）。空 PATH 隐去 brightnessctl、sysfs 根指空目录，分类只由 powerdevil 层决定。

use agent_shell_rpc::{method, Request, Response, RpcError, RpcErrorCode};
use serde_json::{json, Value};
use std::io::{BufRead, BufReader, Read, Write};
use std::path::{Path, PathBuf};
use std::process::{Child, ChildStdin, ChildStdout, Command, Stdio};
use std::sync::{Arc, Mutex};

/// powerdevil 亮度接口坐标（与 `components/power` 的 proxy 声明一致）。
const POWERDEVIL_SERVICE: &str = "org.kde.Solid.PowerManagement";
const POWERDEVIL_PATH: &str = "/org/kde/Solid/PowerManagement/Actions/BrightnessControl";

/// 隔离沙箱：HOME/XDG/PATH/DBUS 全指向临时目录，宿主会话（真实 powerdevil、
/// 背光设备、`brightnessctl`、用户配置、其他任务残留的 daemon 单实例锁）不得
/// 参与本次分类。
struct Sandbox {
    dir: tempfile::TempDir,
}

impl Sandbox {
    fn new() -> Self {
        let dir = tempfile::tempdir().expect("tempdir");
        for sub in ["bin", "config", "data", "runtime", "state", "sysfs"] {
            std::fs::create_dir(dir.path().join(sub)).expect("create sandbox dir");
        }
        Self { dir }
    }

    fn path(&self, sub: &str) -> PathBuf {
        self.dir.path().join(sub)
    }
}

/// 真实 daemon 子进程（stdio 承载 JSON-RPC，与 CLI 激活形态一致）。
struct DaemonProcess {
    child: Child,
    stdin: ChildStdin,
    stdout: BufReader<ChildStdout>,
    stderr: Option<std::process::ChildStderr>,
}

impl DaemonProcess {
    fn start(sandbox: &Sandbox, bus: &MockBus) -> Self {
        let mut child = Command::new(env!("CARGO_BIN_EXE_agent-shell-daemon"))
            .env_clear()
            .env("PATH", sandbox.path("bin"))
            .env("HOME", sandbox.dir.path())
            .env("XDG_CONFIG_HOME", sandbox.path("config"))
            .env("XDG_DATA_DIRS", sandbox.path("data"))
            .env("XDG_RUNTIME_DIR", sandbox.path("runtime"))
            .env("XDG_STATE_HOME", sandbox.path("state"))
            .env("DBUS_SESSION_BUS_ADDRESS", &bus.addr)
            .env("AGENT_SHELL_BACKLIGHT_SYSFS_ROOT", sandbox.path("sysfs"))
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .spawn()
            .expect("spawn agent-shell-daemon");
        let stdin = child.stdin.take().expect("piped stdin");
        let stdout = BufReader::new(child.stdout.take().expect("piped stdout"));
        let stderr = child.stderr.take();
        Self {
            child,
            stdin,
            stdout,
            stderr,
        }
    }

    /// 发送一条请求并读回响应（同一连接、同一 daemon 进程）。
    fn call(&mut self, request: &Request) -> Response {
        self.stdin
            .write_all(request.to_line().as_bytes())
            .expect("write request");
        self.stdin.flush().expect("flush request");
        let mut line = String::new();
        let n = self.stdout.read_line(&mut line).expect("read response");
        assert!(
            n != 0,
            "daemon closed stdout without responding [daemon stderr: {}]",
            self.stderr_tail()
        );
        Response::from_line(&line).expect("daemon must answer with a response line")
    }

    /// daemon 已退出时的 stderr 诊断（进程结束 → 管道 EOF，读取不阻塞）。
    fn stderr_tail(&mut self) -> String {
        let _ = self.child.kill();
        let _ = self.child.wait();
        let Some(mut stderr) = self.stderr.take() else {
            return String::new();
        };
        let mut buf = String::new();
        let _ = stderr.read_to_string(&mut buf);
        buf.trim().to_string()
    }
}

impl Drop for DaemonProcess {
    fn drop(&mut self) {
        // 瞬态 daemon：stdin EOF 即退出；kill 兜底，不给 CI 留孤儿进程。
        let _ = self.child.kill();
        let _ = self.child.wait();
    }
}

/// 私有 session bus：避免与真机总线（真实 powerdevil）及并行测试竞争。
struct MockBus {
    addr: String,
    child: Child,
}

impl MockBus {
    /// 起 `dbus-daemon`。`XDG_DATA_DIRS` 指向空目录，关闭标准 session service
    /// 目录——否则调用无人占用的 `org.kde.Solid.PowerManagement` 会激活宿主机
    /// 真实 powerdevil，「服务不存在」分支随之失真。
    fn start(data_dir: &Path) -> Self {
        let mut child = Command::new("dbus-daemon")
            .args(["--session", "--nofork", "--print-address=1"])
            .env("XDG_DATA_DIRS", data_dir)
            .stdout(Stdio::piped())
            .stderr(Stdio::null())
            .spawn()
            .expect("dbus-daemon is required for the powerdevil e2e");
        let stdout = child.stdout.take().expect("piped stdout");
        let addr = read_address_line(stdout);
        assert!(
            addr.starts_with("unix:"),
            "dbus-daemon printed unexpected address: {addr:?}"
        );
        Self { addr, child }
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

impl Drop for MockBus {
    fn drop(&mut self) {
        // SIGTERM 让 dbus-daemon 走正常退出路径清理 socket；SIGKILL 会残留
        // stale 文件（与 components/power 的 mock bus 同理由）。
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

fn error_of(resp: &Response) -> &RpcError {
    resp.error.as_ref().expect("response must carry an error")
}

#[test]
fn powerdevil_brightness_is_served_end_to_end() {
    // 服务在且调用成功：亮度取自 powerdevil（100/250 → 40%），设置走绝对值。
    // 这条基线同时证明 harness 真的打通了 powerdevil 层——否则下面的失败分支
    // 断言可能因「压根没调到 powerdevil」而失真。
    let sandbox = Sandbox::new();
    let bus = MockBus::start(&sandbox.path("data"));
    let mock = WorkingPowerdevil::default();
    let rt = tokio::runtime::Runtime::new().expect("tokio runtime");
    let _server = rt.block_on(bus.serve(mock.clone()));
    let mut daemon = DaemonProcess::start(&sandbox, &bus);

    let resp = daemon.call(&Request::without_params(1, method::BRIGHTNESS_GET));
    assert!(resp.error.is_none(), "{resp:?}");
    let states: Value = resp.result.expect("brightness.get must return states");
    assert_eq!(states[0]["monitor"], json!("default"), "{states}");
    assert_eq!(states[0]["brightness"], json!(40), "{states}");

    let resp = daemon.call(&Request::new(
        2,
        method::BRIGHTNESS_SET,
        json!({"value": 50}),
    ));
    assert!(resp.error.is_none(), "{resp:?}");
    assert_eq!(
        *mock.sets.lock().expect("sets mutex"),
        vec![125],
        "50% × 250 = 125 must reach powerdevil"
    );
}

#[test]
fn failing_powerdevil_is_reported_as_execution_failure() {
    // 服务在但调用失败（brightnessctl/sysfs 均不可用）：必须报 1005
    // BackendError（CLI exit 1）并带 powerdevil 根因，不得吞成 1002（exit 2）。
    let sandbox = Sandbox::new();
    let bus = MockBus::start(&sandbox.path("data"));
    let rt = tokio::runtime::Runtime::new().expect("tokio runtime");
    let _server = rt.block_on(bus.serve(FailingPowerdevil));
    let mut daemon = DaemonProcess::start(&sandbox, &bus);

    let resp = daemon.call(&Request::without_params(1, method::BRIGHTNESS_GET));
    let err = error_of(&resp);
    assert_eq!(
        err.code,
        RpcErrorCode::BackendError as i32,
        "powerdevil 存在但查询失败应为 BackendError（exit 1），got {err:?}"
    );
    assert!(
        err.message.contains("powerdevil brightness"),
        "{}",
        err.message
    );
    // 降级链合并上报：其余层根因与 powerdevil 根因一并保留。
    assert!(
        err.message.contains("brightnessctl not installed"),
        "{}",
        err.message
    );

    let resp = daemon.call(&Request::new(
        2,
        method::BRIGHTNESS_SET,
        json!({"value": 50}),
    ));
    let err = error_of(&resp);
    assert_eq!(
        err.code,
        RpcErrorCode::BackendError as i32,
        "powerdevil 存在但设置失败应为 BackendError（exit 1），got {err:?}"
    );
    assert!(
        err.message.contains("powerdevil setBrightness"),
        "{}",
        err.message
    );
}

#[test]
fn absent_powerdevil_falls_through_to_install_hint() {
    // 私有 bus 上无人占用 powerdevil 名（无背光硬件的 KDE 会话同形态）：该层
    // 属「不可用」静默下沉，最终 1002 BackendUnavailable（exit 2）+ 安装指引。
    let sandbox = Sandbox::new();
    let bus = MockBus::start(&sandbox.path("data"));
    let mut daemon = DaemonProcess::start(&sandbox, &bus);

    let resp = daemon.call(&Request::without_params(1, method::BRIGHTNESS_GET));
    let err = error_of(&resp);
    assert_eq!(
        err.code,
        RpcErrorCode::BackendUnavailable as i32,
        "powerdevil 不在应为 BackendUnavailable（exit 2），got {err:?}"
    );
    assert!(
        err.message.contains("pacman -S brightnessctl"),
        "{}",
        err.message
    );
    assert!(!err.message.contains("powerdevil"), "{}", err.message);
}
