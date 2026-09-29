//! 蓝牙组件（设计文档 §21.29「蓝牙管理 (BlueZ)」）。
//!
//! [`BluetoothOps`] 是能力契约（daemon 持 `dyn BluetoothOps`，测试注入 fake）。
//! 公开实现 [`BluezClient`] 经 system bus 的 `org.bluez` ObjectManager 读取
//! `org.bluez.Device1` 对象属性——BlueZ 未运行/未安装时返回 BackendUnavailable。

use std::collections::HashMap;
use std::time::Duration;

use agent_shell_core::error::{AgentShellError, Result};
use agent_shell_core::services::BtDevice;
use async_trait::async_trait;
use zbus::fdo::ManagedObjects;
use zbus::names::OwnedInterfaceName;
use zvariant::{Array, OwnedValue};

/// 蓝牙设备查询契约（§21.29）。
#[async_trait]
pub trait BluetoothOps: Send + Sync {
    /// 列出已知蓝牙设备（已配对 / 已发现，含未连接）。
    async fn list_devices(&self) -> Result<Vec<BtDevice>>;
}

/// BlueZ 根对象 `/` 的 `org.freedesktop.DBus.ObjectManager` 接口（§21.29）。
///
/// BlueZ 在 `/` 上注册 ObjectManager，一次调用即可取回全部设备属性，
/// 无需先枚举路径再逐个 `Properties.GetAll`。
#[zbus::proxy(
    interface = "org.freedesktop.DBus.ObjectManager",
    default_service = "org.bluez",
    default_path = "/"
)]
trait ObjectManager {
    /// 返回 `/` 下所有对象、接口及属性（`a{oa{sa{sv}}}`）。
    fn get_managed_objects(&self) -> zbus::Result<ManagedObjects>;
}

/// 单个 D-Bus 接口的属性表（`a{sv}`）。
type DeviceProps = HashMap<String, OwnedValue>;

/// 设备对象的接口名：只有带该接口的对象才是蓝牙设备。
const DEVICE1_INTERFACE: &str = "org.bluez.Device1";

/// 设备对象的路径前缀：同一服务下可能混有非 BlueZ 对象，按路径限定。
const BLUEZ_PATH_PREFIX: &str = "/org/bluez/";

/// BlueZ 缺席类错误名：服务未注册 / 无 owner / 总线激活失败。
///
/// 三者都表示「本机没有可用的 BlueZ」，与「BlueZ 在但调用失败」不同，
/// 前者应降级（BackendUnavailable），后者才是错误（DBus）。
const BLUEZ_ABSENT_ERRORS: &[&str] = &[
    "org.freedesktop.DBus.Error.ServiceUnknown",
    "org.freedesktop.DBus.Error.NameHasNoOwner",
    "org.freedesktop.DBus.Error.Spawn.ExecFailed",
    "org.freedesktop.DBus.Error.Spawn.ChildExited",
    "org.freedesktop.DBus.Error.Spawn.Failed",
    "org.freedesktop.DBus.Error.Spawn.ServiceNotFound",
];

/// BlueZ 客户端：经 system bus 的 `org.bluez` ObjectManager 读取设备（§21.29）。
pub struct BluezClient {
    conn: zbus::Connection,
}

impl BluezClient {
    /// D-Bus 方法调用超时：全局默认 5s（design/11 §19.4）。
    const CALL_TIMEOUT: Duration = Duration::from_secs(5);

    /// 连接 system bus（BlueZ 只注册在 system bus，不随会话迁移）。
    ///
    /// 连接失败（无 system bus、socket 无权限）归为 BackendUnavailable：
    /// 环境缺少 BlueZ 后端，由降级链/doctor 处理，而非本次调用失败。
    pub async fn new() -> Result<Self> {
        let conn = zbus::connection::Builder::system()
            .map_err(system_bus_unavailable)?
            .method_timeout(Self::CALL_TIMEOUT)
            .build()
            .await
            .map_err(system_bus_unavailable)?;
        Ok(Self { conn })
    }

    /// 供测试注入私有总线（system bus 在测试环境不可控）。
    #[doc(hidden)]
    pub fn with_connection(conn: zbus::Connection) -> Self {
        Self { conn }
    }

    /// ObjectManager proxy（destination `org.bluez`、path `/`）。
    async fn object_manager(&self) -> zbus::Result<ObjectManagerProxy<'_>> {
        ObjectManagerProxy::new(&self.conn).await
    }
}

#[async_trait]
impl BluetoothOps for BluezClient {
    async fn list_devices(&self) -> Result<Vec<BtDevice>> {
        let manager = self.object_manager().await.map_err(map_call_error)?;
        let objects = manager
            .get_managed_objects()
            .await
            .map_err(map_call_error)?;
        Ok(map_devices(&objects))
    }
}

/// system bus 不可达的统一错误（组装层据此判定蓝牙后端缺失）。
fn system_bus_unavailable<E: std::fmt::Display>(e: E) -> AgentShellError {
    AgentShellError::BackendUnavailable(format!("system bus unavailable: {e}"))
}

/// BlueZ 缺席的统一错误：本机 BlueZ 未运行/未安装。
fn bluez_unavailable() -> AgentShellError {
    AgentShellError::BackendUnavailable("org.bluez not available (BlueZ not running)".into())
}

/// 错误消息是否表示 BlueZ 缺席。
///
/// 错误名按 `:` 分段全量比对：zbus 将错误名渲染为 `<错误名>: <描述>`，且调用方
/// 可能再加前缀（与 [`agent_shell_core::error::dbus_error`] 同法，避免子串误判）。
fn is_bluez_absent(msg: &str) -> bool {
    msg.split(':')
        .map(str::trim)
        .any(|segment| BLUEZ_ABSENT_ERRORS.contains(&segment))
}

/// ObjectManager 调用错误 → core 错误（§19 归一化）。
///
/// 三类区分：方法超时（zbus 以 io `TimedOut` 表示）→ Timeout；BlueZ 缺席 →
/// BackendUnavailable；其余（含权限类）交共享 helper 归一化。
fn map_call_error(e: zbus::Error) -> AgentShellError {
    if let zbus::Error::InputOutput(io) = &e {
        if io.kind() == std::io::ErrorKind::TimedOut {
            return AgentShellError::Timeout(format!("org.bluez GetManagedObjects timed out: {e}"));
        }
    }
    if is_bluez_absent(&e.to_string()) {
        return bluez_unavailable();
    }
    agent_shell_core::error::dbus_error(e)
}

/// GetManagedObjects 结果 → 设备列表。
///
/// 只取路径在 `/org/bluez/` 下、且带 `org.bluez.Device1` 接口的对象（适配器、
/// 代理对象等一律跳过）。地址是设备唯一标识，无 `Address` 的对象不是可用设备，
/// 跳过而非产出空地址条目。输出按地址排序：ObjectManager 的映射无序，
/// 排序让同一总线状态产生稳定输出。
fn map_devices(objects: &ManagedObjects) -> Vec<BtDevice> {
    let mut devices: Vec<BtDevice> = objects
        .iter()
        .filter(|(path, _)| path.as_str().contains(BLUEZ_PATH_PREFIX))
        .filter_map(|(_, interfaces)| device_props(interfaces))
        .filter_map(map_device)
        .collect();
    devices.sort_by(|a, b| a.address.cmp(&b.address));
    devices
}

/// 取对象上的 `org.bluez.Device1` 属性表（非设备对象 → None）。
fn device_props(interfaces: &HashMap<OwnedInterfaceName, DeviceProps>) -> Option<&DeviceProps> {
    interfaces
        .iter()
        .find(|(name, _)| name.as_str() == DEVICE1_INTERFACE)
        .map(|(_, props)| props)
}

/// `org.bluez.Device1` 属性 → [`BtDevice`]；缺 `Address` 返回 None。
///
/// 名称优先级 `Alias` → `Name` → 地址：Alias 是用户重命名结果，最贴近用户预期。
fn map_device(props: &DeviceProps) -> Option<BtDevice> {
    let address = prop_str(props, "Address")?;
    let name = prop_str(props, "Alias")
        .or_else(|| prop_str(props, "Name"))
        .unwrap_or_else(|| address.clone());
    Some(BtDevice {
        address,
        name,
        paired: prop_bool(props, "Paired"),
        connected: prop_bool(props, "Connected"),
        trusted: prop_bool(props, "Trusted"),
        rssi: prop_i16(props, "RSSI"),
        uuids: prop_strings(props, "UUIDs"),
    })
}

/// 字符串属性（缺失或类型不符 → None）。
fn prop_str(props: &DeviceProps, key: &str) -> Option<String> {
    props
        .get(key)
        .and_then(|value| <&str>::try_from(value).ok())
        .map(str::to_string)
}

/// 布尔属性；缺失或类型不符按 false（未提供即未配对/未连接/不受信）。
fn prop_bool(props: &DeviceProps, key: &str) -> bool {
    props
        .get(key)
        .and_then(|value| bool::try_from(value).ok())
        .unwrap_or(false)
}

/// 有符号 16 位属性（`RSSI` 只在设备可见时有值，缺失 → None）。
fn prop_i16(props: &DeviceProps, key: &str) -> Option<i16> {
    props.get(key).and_then(|value| i16::try_from(value).ok())
}

/// 字符串数组属性（缺失或类型不符 → 空列表）。
fn prop_strings(props: &DeviceProps, key: &str) -> Vec<String> {
    let Some(value) = props.get(key) else {
        return Vec::new();
    };
    let Ok(items) = <&Array>::try_from(value) else {
        return Vec::new();
    };
    items
        .iter()
        .filter_map(|item| <&str>::try_from(item).ok().map(str::to_string))
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::process::Stdio;

    /// 构造 `a{sv}` 中的一个属性值。
    fn value(value: impl Into<zvariant::Value<'static>>) -> OwnedValue {
        OwnedValue::try_from(value.into()).expect("value must be ownable")
    }

    /// 构造属性表。
    fn props(entries: &[(&str, OwnedValue)]) -> DeviceProps {
        entries
            .iter()
            .map(|(key, value)| ((*key).to_string(), value.clone()))
            .collect()
    }

    /// 构造对象的接口表。
    fn interfaces(entries: &[(&str, DeviceProps)]) -> HashMap<OwnedInterfaceName, DeviceProps> {
        entries
            .iter()
            .map(|(name, props)| {
                (
                    OwnedInterfaceName::try_from(*name).expect("valid interface name"),
                    props.clone(),
                )
            })
            .collect()
    }

    /// 构造 GetManagedObjects 的返回映射。
    fn objects(entries: &[(&str, HashMap<OwnedInterfaceName, DeviceProps>)]) -> ManagedObjects {
        entries
            .iter()
            .map(|(path, interfaces)| {
                (
                    zvariant::OwnedObjectPath::try_from(*path).expect("valid object path"),
                    interfaces.clone(),
                )
            })
            .collect()
    }

    /// 设备对象：Alias/Name 并存（验证 Alias 优先）。
    fn device_with(entries: &[(&str, OwnedValue)]) -> HashMap<OwnedInterfaceName, DeviceProps> {
        interfaces(&[(DEVICE1_INTERFACE, props(entries))])
    }

    #[test]
    fn maps_devices_and_skips_adapters_and_incomplete_objects() {
        let objects = objects(&[
            // 适配器：非设备对象，跳过。
            (
                "/org/bluez/hci0",
                interfaces(&[(
                    "org.bluez.Adapter1",
                    props(&[("Address", value("AA:BB:CC:DD:EE:FF"))]),
                )]),
            ),
            // 设备 2 先于设备 1 出现：映射输出仍按地址升序，与 hash 顺序无关。
            (
                "/org/bluez/hci0/dev_AA_BB_CC_DD_EE_02",
                device_with(&[
                    ("Address", value("AA:BB:CC:DD:EE:02")),
                    ("Name", value("Keyboard")),
                ]),
            ),
            (
                "/org/bluez/hci0/dev_AA_BB_CC_DD_EE_01",
                device_with(&[
                    ("Address", value("AA:BB:CC:DD:EE:01")),
                    ("Alias", value("Headset")),
                    ("Name", value("WH-1000XM4")),
                    ("Paired", value(true)),
                    ("Connected", value(true)),
                    ("Trusted", value(true)),
                    ("RSSI", value(-42i16)),
                    (
                        "UUIDs",
                        value(vec![
                            "0000110b-0000-1000-8000-00805f9b34fb".to_string(),
                            "0000110e-0000-1000-8000-00805f9b34fb".to_string(),
                        ]),
                    ),
                ]),
            ),
            // 缺 Address：不是可用设备，跳过。
            (
                "/org/bluez/hci0/dev_AA_BB_CC_DD_EE_04",
                device_with(&[("Name", value("NoAddress"))]),
            ),
            // 路径不在 /org/bluez/ 下：即使带 Device1 接口也不属于 BlueZ 设备树。
            (
                "/com/example/hci0/dev_AA_BB_CC_DD_EE_05",
                device_with(&[("Address", value("AA:BB:CC:DD:EE:05"))]),
            ),
        ]);

        let devices = map_devices(&objects);
        assert_eq!(
            devices.len(),
            2,
            "只有 hci0 下两台带 Address 的设备应入选：{devices:?}"
        );

        assert_eq!(devices[0].address, "AA:BB:CC:DD:EE:01");
        assert_eq!(devices[0].name, "Headset", "Alias 优先于 Name");
        assert!(devices[0].paired);
        assert!(devices[0].connected);
        assert!(devices[0].trusted);
        assert_eq!(devices[0].rssi, Some(-42));
        assert_eq!(
            devices[0].uuids,
            vec![
                "0000110b-0000-1000-8000-00805f9b34fb".to_string(),
                "0000110e-0000-1000-8000-00805f9b34fb".to_string(),
            ]
        );

        assert_eq!(devices[1].address, "AA:BB:CC:DD:EE:02");
        assert_eq!(devices[1].name, "Keyboard", "无 Alias 时回退 Name");
        assert!(!devices[1].paired);
        assert!(!devices[1].connected);
        assert!(!devices[1].trusted);
        assert_eq!(devices[1].rssi, None);
        assert!(devices[1].uuids.is_empty());
    }

    #[test]
    fn falls_back_to_address_when_alias_and_name_absent() {
        let objects = objects(&[(
            "/org/bluez/hci0/dev_AA_BB_CC_DD_EE_07",
            device_with(&[("Address", value("AA:BB:CC:DD:EE:07"))]),
        )]);

        let devices = map_devices(&objects);
        assert_eq!(devices.len(), 1);
        assert_eq!(devices[0].name, "AA:BB:CC:DD:EE:07");
    }

    #[test]
    fn empty_object_map_yields_no_devices() {
        assert!(map_devices(&ManagedObjects::new()).is_empty());
    }

    #[test]
    fn detects_bluez_absent_error_names() {
        for msg in [
            "org.freedesktop.DBus.Error.ServiceUnknown: The name org.bluez was not provided by any .service files",
            "org.freedesktop.DBus.Error.NameHasNoOwner: Could not activate remote peer 'org.bluez'",
            "org.freedesktop.DBus.Error.Spawn.ExecFailed: Failed to execute program org.bluez",
            "get org.bluez objects: org.freedesktop.DBus.Error.ServiceUnknown: no such name",
        ] {
            assert!(is_bluez_absent(msg), "expected absent for {msg:?}");
        }

        for msg in [
            "org.freedesktop.DBus.Error.AccessDenied: Rejected send message",
            "org.freedesktop.DBus.Error.ServiceUnknownExtra: not a BlueZ error",
            "I/O error: timed out",
            "",
        ] {
            assert!(!is_bluez_absent(msg), "unexpected absent for {msg:?}");
        }
    }

    #[test]
    fn maps_method_timeout_to_timeout_error() {
        // zbus 的方法超时以 io `TimedOut` 形式返回（见 zbus::abstractions::timeout）：
        // 必须归为 Timeout，而非普通 DBus 错误，降级链/doctor 据此区分。
        let elapsed = zbus::Error::InputOutput(std::sync::Arc::new(std::io::Error::new(
            std::io::ErrorKind::TimedOut,
            "timed out",
        )));
        assert!(matches!(
            map_call_error(elapsed),
            AgentShellError::Timeout(_)
        ));
    }

    /// 独立私有 session bus：测试机 system bus 不可控，且私有总线隔离并行测试。
    struct TestBus {
        addr: String,
        _child: std::process::Child,
    }

    impl TestBus {
        async fn start() -> Self {
            let mut child = std::process::Command::new("dbus-daemon")
                .args(["--session", "--nofork", "--print-address=1"])
                .stdout(Stdio::piped())
                .stderr(Stdio::null())
                .spawn()
                .expect("dbus-daemon must be installed for bluetooth tests");
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
            // `kill()` 发 SIGKILL，跳过 dbus-daemon 正常退出路径，其 /tmp/dbus-*
            // socket 不 unlink、累积 stale 文件；SIGTERM 让其自行清理。
            // SAFETY: `_child.id()` 是存活的子进程 PID，发 SIGTERM 无内存安全风险。
            unsafe { libc::kill(self._child.id() as i32, libc::SIGTERM) };
            let _ = self._child.wait();
        }
    }

    /// 逐字节读地址行：`dbus-daemon --print-address=1` 恰好一行。
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

    /// fake `org.bluez.Device1`：全部可选属性齐全。
    ///
    /// `RSSI`/`UUIDs` 必须显式指定 D-Bus 名：zbus 默认把方法名转 PascalCase
    /// （会得到 `Rssi`/`Uuids`），而 BlueZ 的属性名是全大写/全小写混排。
    struct FullDevice {
        address: String,
        alias: String,
        name: String,
        paired: bool,
        connected: bool,
        trusted: bool,
        rssi: i16,
        uuids: Vec<String>,
    }

    #[zbus::interface(name = "org.bluez.Device1")]
    impl FullDevice {
        #[zbus(property)]
        fn address(&self) -> String {
            self.address.clone()
        }

        #[zbus(property)]
        fn alias(&self) -> String {
            self.alias.clone()
        }

        #[zbus(property)]
        fn name(&self) -> String {
            self.name.clone()
        }

        #[zbus(property)]
        fn paired(&self) -> bool {
            self.paired
        }

        #[zbus(property)]
        fn connected(&self) -> bool {
            self.connected
        }

        #[zbus(property)]
        fn trusted(&self) -> bool {
            self.trusted
        }

        #[zbus(property, name = "RSSI")]
        fn rssi(&self) -> i16 {
            self.rssi
        }

        #[zbus(property, name = "UUIDs")]
        fn uuids(&self) -> Vec<String> {
            self.uuids.clone()
        }
    }

    /// fake `org.bluez.Device1`：既无 Alias 也无 Name，且无 Connected/Trusted/RSSI。
    struct BareDevice {
        address: String,
        paired: bool,
    }

    #[zbus::interface(name = "org.bluez.Device1")]
    impl BareDevice {
        #[zbus(property)]
        fn address(&self) -> String {
            self.address.clone()
        }

        #[zbus(property)]
        fn paired(&self) -> bool {
            self.paired
        }
    }

    /// fake `org.bluez.Adapter1`：非设备对象，必须被忽略。
    struct FakeAdapter;

    #[zbus::interface(name = "org.bluez.Adapter1")]
    impl FakeAdapter {
        #[zbus(property)]
        fn address(&self) -> String {
            "AA:BB:CC:DD:EE:FF".to_string()
        }
    }

    /// 在私有总线上注册 fake `org.bluez` 服务（ObjectManager 挂在 `/`）。
    async fn spawn_fake_bluez(bus: &TestBus) -> zbus::Connection {
        let conn = bus.connect().await;
        conn.object_server()
            .at("/", zbus::fdo::ObjectManager)
            .await
            .expect("register ObjectManager at /");
        conn.object_server()
            .at("/org/bluez/hci0", FakeAdapter)
            .await
            .expect("register adapter");
        conn.object_server()
            .at(
                "/org/bluez/hci0/dev_AA_BB_CC_DD_EE_01",
                FullDevice {
                    address: "AA:BB:CC:DD:EE:01".to_string(),
                    alias: "Headset".to_string(),
                    name: "WH-1000XM4".to_string(),
                    paired: true,
                    connected: true,
                    trusted: true,
                    rssi: -42,
                    uuids: vec!["0000110b-0000-1000-8000-00805f9b34fb".to_string()],
                },
            )
            .await
            .expect("register first device");
        conn.object_server()
            .at(
                "/org/bluez/hci0/dev_AA_BB_CC_DD_EE_02",
                BareDevice {
                    address: "AA:BB:CC:DD:EE:02".to_string(),
                    paired: true,
                },
            )
            .await
            .expect("register second device");
        // 同一服务树内、但不在 /org/bluez/ 下的设备对象：client 必须按路径过滤掉。
        conn.object_server()
            .at(
                "/com/example/hci0/dev_AA_BB_CC_DD_EE_03",
                FullDevice {
                    address: "AA:BB:CC:DD:EE:03".to_string(),
                    alias: "Foreign".to_string(),
                    name: "Foreign".to_string(),
                    paired: false,
                    connected: false,
                    trusted: false,
                    rssi: -70,
                    uuids: Vec::new(),
                },
            )
            .await
            .expect("register foreign-path device");

        use zbus::names::WellKnownName;
        let name = WellKnownName::try_from("org.bluez".to_string()).expect("valid bus name");
        conn.request_name(name).await.expect("claim org.bluez");
        conn
    }

    #[tokio::test]
    async fn lists_devices_from_private_bluez_service() {
        let bus = TestBus::start().await;
        let _service = spawn_fake_bluez(&bus).await;
        let client = BluezClient::with_connection(bus.connect().await);

        let devices = client.list_devices().await.expect("list devices");
        assert_eq!(
            devices.len(),
            2,
            "适配器与外部路径对象应被过滤：{devices:?}"
        );

        assert_eq!(devices[0].address, "AA:BB:CC:DD:EE:01");
        assert_eq!(devices[0].name, "Headset");
        assert!(devices[0].paired);
        assert!(devices[0].connected);
        assert!(devices[0].trusted);
        assert_eq!(devices[0].rssi, Some(-42));
        assert_eq!(
            devices[0].uuids,
            vec!["0000110b-0000-1000-8000-00805f9b34fb".to_string()]
        );

        assert_eq!(devices[1].address, "AA:BB:CC:DD:EE:02");
        assert_eq!(
            devices[1].name, "AA:BB:CC:DD:EE:02",
            "无 Alias/Name 回退地址"
        );
        assert!(devices[1].paired);
        assert!(!devices[1].connected);
        assert!(!devices[1].trusted);
        assert_eq!(devices[1].rssi, None);
        assert!(devices[1].uuids.is_empty());
    }

    #[tokio::test]
    async fn reports_backend_unavailable_when_bluez_not_running() {
        let bus = TestBus::start().await;
        let client = BluezClient::with_connection(bus.connect().await);

        let err = client
            .list_devices()
            .await
            .expect_err("no org.bluez owner on this bus");
        assert!(
            matches!(
                &err,
                AgentShellError::BackendUnavailable(msg)
                    if msg == "org.bluez not available (BlueZ not running)"
            ),
            "expected BackendUnavailable, got {err:?}"
        );
    }
}
