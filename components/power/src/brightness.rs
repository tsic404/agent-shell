//! 屏幕亮度控制器（设计文档 §21.25 屏幕亮度）。
//!
//! 「DE 封装优先 → 公共降级」：KDE 会话走 powerdevil
//! `org.kde.Solid.PowerManagement.Actions.BrightnessControl`；其余环境依次回退
//! `brightnessctl` CLI 与内核 backlight sysfs（`/sys/class/backlight` 直读直写）。
//! 各路径统一以 0-100 百分比对外，屏蔽绝对值与多背光设备的差异。

use agent_shell_core::error::{dbus_error, AgentShellError, Result};
use agent_shell_core::services::BrightnessState;
use async_trait::async_trait;
use std::path::{Path, PathBuf};
use zbus::proxy;

/// 内核 backlight sysfs 根目录（`brightnessctl` 缺失或失败时的兜底数据源）。
const SYSFS_BACKLIGHT_ROOT: &str = "/sys/class/backlight";

/// `brightnessctl` 缺失时的错误正文：按包管理器给出可直接执行的安装命令。
const BRIGHTNESSCTL_MISSING: &str = "brightnessctl not installed — install it (Arch: \
     `pacman -S brightnessctl`; Debian/Ubuntu: `apt install brightnessctl`; Fedora: \
     `dnf install brightnessctl`)";

/// sysfs 直写被拒（会话用户无 udev/组授权）时的可操作指引。
const BACKLIGHT_PERMISSION_HINT: &str =
    "grant write access to /sys/class/backlight (add user to `video` group or add a udev rule)";

/// sysfs 直写撞上只读挂载（EROFS：容器或加固挂载）时的可操作指引。
const BACKLIGHT_READONLY_HINT: &str = "the sysfs mount is read-only — remount it read-write \
     (`mount -o remount,rw /sys`) or run the container/host with /sys writable";

/// sysfs 路径组件非目录（ENOTDIR：sysfs 未挂载或路径被同名文件占用）的指引。
const BACKLIGHT_SYSFS_UNMOUNTED_HINT: &str = "a path component is not a directory — sysfs is \
     probably not mounted at /sys, or the backlight device vanished (check `mount | grep sysfs`)";

/// 亮度能力契约（daemon 持 `dyn BrightnessOps`，测试注入 fake）。
#[async_trait]
pub trait BrightnessOps: Send + Sync {
    /// 查询当前亮度状态列表。
    async fn get(&self) -> Result<Vec<BrightnessState>>;
    /// 设置亮度（0-100 百分比）。
    async fn set(&self, value: u8) -> Result<()>;
}

/// powerdevil 亮度子接口（session bus）。
///
/// 方法名显式声明：powerdevil 导出的是 camelCase（`brightness`/`brightnessMax`/
/// `setBrightness`），zbus 默认按 PascalCase 推导会调成不存在的 `Brightness`，
/// 使该层恒报 UnknownMethod（详见设计文档 §21.25 接口表）。
#[proxy(
    interface = "org.kde.Solid.PowerManagement.Actions.BrightnessControl",
    default_service = "org.kde.Solid.PowerManagement",
    default_path = "/org/kde/Solid/PowerManagement/Actions/BrightnessControl"
)]
trait BrightnessControl {
    /// 当前亮度（绝对值，0..=brightness_max）。
    #[zbus(name = "brightness")]
    fn brightness(&self) -> zbus::Result<i32>;
    /// 最大亮度值。
    #[zbus(name = "brightnessMax")]
    fn brightness_max(&self) -> zbus::Result<i32>;
    /// 设置亮度（绝对值）。
    #[zbus(name = "setBrightness")]
    fn set_brightness(&self, value: i32) -> zbus::Result<()>;
}

/// 亮度控制器：KDE powerdevil 优先，`brightnessctl` 与内核 sysfs 依次降级。
pub struct BrightnessController {
    conn: Option<zbus::Connection>,
    /// `brightnessctl` 可执行文件（测试注入不存在路径以覆盖缺失分支）。
    brightnessctl_bin: PathBuf,
    /// 内核 backlight sysfs 根目录（测试注入临时目录）。
    sysfs_root: PathBuf,
}

impl BrightnessController {
    /// 构造：探测 session bus。bus 不可达时 `conn=None`，仅保留
    /// `brightnessctl`/sysfs 降级路径——构造永不失败（headless 亦可用）。
    pub async fn new() -> Self {
        let conn = zbus::Connection::session().await.ok();
        Self {
            conn,
            brightnessctl_bin: PathBuf::from("brightnessctl"),
            sysfs_root: PathBuf::from(SYSFS_BACKLIGHT_ROOT),
        }
    }

    async fn kde(&self) -> Option<BrightnessControlProxy<'static>> {
        let conn = self.conn.as_ref()?;
        BrightnessControlProxy::builder(conn)
            .cache_properties(zbus::proxy::CacheProperties::No)
            .build()
            .await
            .ok()
    }

    /// 查询当前亮度：powerdevil 服务/接口不存在 → `Ok(None)`（该层不可用，静默
    /// 下沉）；服务在但调用失败 → `Err`（保留根因，交由降级链合并上报）。
    async fn kde_get(&self) -> Result<Option<BrightnessState>> {
        let Some(kde) = self.kde().await else {
            return Ok(None);
        };
        let Some(current) = kde_call("brightness", kde.brightness().await)? else {
            return Ok(None);
        };
        let Some(max) = kde_call("brightnessMax", kde.brightness_max().await)? else {
            return Ok(None);
        };
        Ok(Some(BrightnessState {
            monitor: "default".into(),
            brightness: normalize_to_percent(current, max),
            max_brightness: 100,
            adaptive: false,
        }))
    }

    /// 设置亮度：不可用/无亮度接口 → `Ok(false)` 下沉；调用失败 → `Err`。
    async fn kde_set(&self, value: u8) -> Result<bool> {
        let Some(kde) = self.kde().await else {
            return Ok(false);
        };
        let Some(max) = kde_call("brightnessMax", kde.brightness_max().await)? else {
            return Ok(false);
        };
        let target = percent_to_absolute(value, max);
        Ok(kde_call("setBrightness", kde.set_brightness(target).await)?.is_some())
    }

    async fn brightnessctl_get(&self) -> Result<Vec<BrightnessState>> {
        let out = run_brightnessctl(&self.brightnessctl_bin, &["-m"]).await?;
        let states = parse_brightnessctl_machine_output(&out);
        if states.is_empty() {
            Err(AgentShellError::BackendUnavailable(
                "brightnessctl reported no backlight devices".into(),
            ))
        } else {
            Ok(states)
        }
    }

    async fn brightnessctl_set(&self, value: u8) -> Result<()> {
        let spec = format!("{value}%");
        run_brightnessctl(&self.brightnessctl_bin, &["set", &spec]).await?;
        Ok(())
    }

    /// sysfs 兜底读：设备全部读失败时保留读取错误（后端已发现但执行失败 →
    /// exit 1，错误详情不丢）；部分成功即返回成功项（单设备热插拔竞态不拖垮整体）。
    fn sysfs_get(&self) -> Result<Vec<BrightnessState>> {
        let devices = sysfs_devices(&self.sysfs_root);
        if devices.is_empty() {
            return Err(self.no_sysfs_device_error());
        }
        let mut states = Vec::new();
        let mut failures = Vec::new();
        for name in &devices {
            match read_sysfs_state(&self.sysfs_root, name) {
                Ok(state) => states.push(state),
                Err(e) => failures.push(e),
            }
        }
        if states.is_empty() {
            Err(combine_failures(&failures))
        } else {
            Ok(states)
        }
    }

    /// sysfs 兜底写：多背光设备取排序首个，行为在设备枚举顺序上保持确定。
    fn sysfs_set(&self, value: u8) -> Result<()> {
        match sysfs_devices(&self.sysfs_root).first() {
            Some(name) => write_sysfs_brightness(&self.sysfs_root, name, value),
            None => Err(self.no_sysfs_device_error()),
        }
    }

    fn no_sysfs_device_error(&self) -> AgentShellError {
        AgentShellError::BackendUnavailable(format!(
            "no readable backlight device under {}",
            self.sysfs_root.display()
        ))
    }
}

#[async_trait]
impl BrightnessOps for BrightnessController {
    /// 降级链：powerdevil → `brightnessctl` → 内核 sysfs。某层「不可用」静默
    /// 下沉；「存在但执行失败」的根因保留，全层失败时合并上报（exit 1）。
    async fn get(&self) -> Result<Vec<BrightnessState>> {
        let mut failures = Vec::new();
        match self.kde_get().await {
            Ok(Some(state)) => return Ok(vec![state]),
            Ok(None) => {}
            Err(e) => failures.push(e),
        }
        match self.brightnessctl_get().await {
            Ok(states) => return Ok(states),
            Err(e) => failures.push(e),
        }
        match self.sysfs_get() {
            Ok(states) => Ok(states),
            Err(e) => {
                failures.push(e);
                Err(combine_failures(&failures))
            }
        }
    }

    /// 设置：降级与合并规则同 [`Self::get`]。
    async fn set(&self, value: u8) -> Result<()> {
        let mut failures = Vec::new();
        match self.kde_set(value).await {
            Ok(true) => return Ok(()),
            Ok(false) => {}
            Err(e) => failures.push(e),
        }
        match self.brightnessctl_set(value).await {
            Ok(()) => return Ok(()),
            Err(e) => failures.push(e),
        }
        match self.sysfs_set(value) {
            Ok(()) => Ok(()),
            Err(e) => {
                failures.push(e);
                Err(combine_failures(&failures))
            }
        }
    }
}

/// 绝对值 → 0-100 百分比（钳制）。中间积提升 i64 避免 i32 溢出
/// （current 或 max 超过 ~21M 时 i32 乘法会回绕/panic）。
fn normalize_to_percent(current: i32, max: i32) -> u8 {
    if max <= 0 {
        return 0;
    }
    (current as i64 * 100 / max as i64).clamp(0, 100) as u8
}

/// 0-100 百分比 → 绝对值（钳制到 [0, max]）。中间积提升 i64 避免溢出。
fn percent_to_absolute(value: u8, max: i32) -> i32 {
    if max <= 0 {
        return 0;
    }
    (value as i64 * max as i64 / 100).clamp(0, max as i64) as i32
}

/// 执行 brightnessctl 并返回 stdout（仅成功时）。
///
/// 错误分类：二进制缺失（NotFound）→ BackendUnavailable（无后端，CLI exit 2，
/// 正文含安装指引）；其余 spawn 失败与非零退出（如 sysfs 写权限不足）→
/// BackendError（后端在但执行失败，CLI exit 1），二者不可混淆。
async fn run_brightnessctl(bin: &Path, args: &[&str]) -> Result<String> {
    let out = tokio::process::Command::new(bin)
        .args(args)
        .output()
        .await
        .map_err(|e| match e.kind() {
            std::io::ErrorKind::NotFound => {
                AgentShellError::BackendUnavailable(BRIGHTNESSCTL_MISSING.into())
            }
            _ => AgentShellError::Other(format!("brightnessctl spawn: {e}").into()),
        })?;
    if out.status.success() {
        Ok(String::from_utf8_lossy(&out.stdout).into_owned())
    } else {
        Err(AgentShellError::Other(
            format!(
                "brightnessctl {}: {}",
                args.join(" "),
                String::from_utf8_lossy(&out.stderr).trim()
            )
            .into(),
        ))
    }
}

/// powerdevil 调用结果分类：服务/对象/接口不存在 → `Ok(None)`（该层不可用，
/// 静默下沉）；其余错误（权限、超时、方法内部失败）→ `Err`（后端存在但执行
/// 失败，保留根因并入降级链合并上报，CLI exit 1）。
fn kde_call<T>(method: &str, result: zbus::Result<T>) -> Result<Option<T>> {
    match result {
        Ok(value) => Ok(Some(value)),
        Err(e) if kde_service_absent(&e) => Ok(None),
        Err(e) => Err(dbus_error(format!("powerdevil {method}: {e}"))),
    }
}

/// powerdevil 是否「不存在」：无 session bus / powerdevil 未运行 / 该对象或
/// 接口未导出（无背光设备的机器即此形态）——这些不是执行失败。
fn kde_service_absent(e: &zbus::Error) -> bool {
    match e {
        zbus::Error::MethodError(name, ..) => KDE_ABSENT_ERROR_NAMES.contains(&name.as_str()),
        _ => false,
    }
}

/// powerdevil「不存在」的 D-Bus 错误名（错误名全量比对）。
const KDE_ABSENT_ERROR_NAMES: &[&str] = &[
    "org.freedesktop.DBus.Error.ServiceUnknown",
    "org.freedesktop.DBus.Error.NameHasNoOwner",
    "org.freedesktop.DBus.Error.UnknownObject",
    "org.freedesktop.DBus.Error.UnknownInterface",
    "org.freedesktop.DBus.Error.UnknownMethod",
    "org.freedesktop.DBus.Error.UnknownProperty",
];

/// 归并降级链各层根因：只要有任一层「后端存在但执行失败」（非
/// `BackendUnavailable`），最终错误即保持 BackendError 语义（CLI exit 1）；
/// 全层均无后端才 BackendUnavailable（exit 2）——退出码不因新增兜底层漂移。
fn combine_failures(failures: &[AgentShellError]) -> AgentShellError {
    let detail = failures
        .iter()
        .map(error_detail)
        .collect::<Vec<_>>()
        .join("; ");
    if failures
        .iter()
        .all(|e| matches!(e, AgentShellError::BackendUnavailable(_)))
    {
        AgentShellError::BackendUnavailable(detail)
    } else {
        AgentShellError::Other(detail.into())
    }
}

/// 错误正文：`BackendUnavailable` 取内层消息（其 `Display` 带
/// "Backend not available: " 前缀，合并多层时会重复堆叠）。
fn error_detail(e: &AgentShellError) -> String {
    match e {
        AgentShellError::BackendUnavailable(message) => message.clone(),
        other => other.to_string(),
    }
}

/// 枚举 sysfs 背光设备名：目录项名且 `max_brightness` 可读 > 0。
/// 名称排序——`read_dir` 顺序不保证稳定，多设备输出需可复现。
fn sysfs_devices(root: &Path) -> Vec<String> {
    let Ok(entries) = std::fs::read_dir(root) else {
        return Vec::new();
    };
    let mut names: Vec<String> = entries
        .flatten()
        .filter_map(|entry| entry.file_name().into_string().ok())
        .filter(|name| sysfs_max_brightness(root, name).is_some_and(|max| max > 0))
        .collect();
    names.sort();
    names
}

/// 读设备 `max_brightness`（不可读或非数字 → None）。
fn sysfs_max_brightness(root: &Path, name: &str) -> Option<u32> {
    read_sysfs_u32(&root.join(name).join("max_brightness"))
}

fn read_sysfs_u32(path: &Path) -> Option<u32> {
    std::fs::read_to_string(path).ok()?.trim().parse().ok()
}

/// 读单个设备当前亮度：优先 `actual_brightness`（硬件实际值，ACPI 平滑后可能
/// 与请求值不同），缺失时回退 `brightness`。
fn read_sysfs_state(root: &Path, name: &str) -> Result<BrightnessState> {
    let dir = root.join(name);
    let Some(max) = sysfs_max_brightness(root, name) else {
        return Err(AgentShellError::Other(
            format!("read {}/max_brightness failed", dir.display()).into(),
        ));
    };
    let Some(current) = read_sysfs_u32(&dir.join("actual_brightness"))
        .or_else(|| read_sysfs_u32(&dir.join("brightness")))
    else {
        return Err(AgentShellError::Other(
            format!("read {}/brightness failed", dir.display()).into(),
        ));
    };
    Ok(BrightnessState {
        monitor: name.to_string(),
        brightness: normalize_to_percent(saturating_i32(current), saturating_i32(max)),
        max_brightness: 100,
        adaptive: false,
    })
}

/// 直写设备 `brightness`（内核立即生效，无需外部二进制）。
fn write_sysfs_brightness(root: &Path, name: &str, value: u8) -> Result<()> {
    let dir = root.join(name);
    let Some(max) = sysfs_max_brightness(root, name) else {
        return Err(AgentShellError::Other(
            format!("read {}/max_brightness failed", dir.display()).into(),
        ));
    };
    let path = dir.join("brightness");
    let target = percent_to_absolute(value, saturating_i32(max));
    std::fs::write(&path, target.to_string()).map_err(|e| sysfs_write_error(&path, &e))
}

/// sysfs 写失败的报错：按内核错误类别追加可操作指引，其余透出路径与 OS 错误。
fn sysfs_write_error(path: &Path, e: &std::io::Error) -> AgentShellError {
    let mut detail = format!("write {}: {e}", path.display());
    if let Some(hint) = sysfs_write_hint(e.kind()) {
        detail.push_str(" — ");
        detail.push_str(hint);
    }
    AgentShellError::Other(detail.into())
}

/// 内核错误类别 → 下一步指引；未归类（如设备热插拔的 NotFound）返回 None，不猜原因。
fn sysfs_write_hint(kind: std::io::ErrorKind) -> Option<&'static str> {
    match kind {
        std::io::ErrorKind::PermissionDenied => Some(BACKLIGHT_PERMISSION_HINT),
        std::io::ErrorKind::ReadOnlyFilesystem => Some(BACKLIGHT_READONLY_HINT),
        std::io::ErrorKind::NotADirectory => Some(BACKLIGHT_SYSFS_UNMOUNTED_HINT),
        _ => None,
    }
}

/// u32 → i32 饱和转换（sysfs 值可超 i32::MAX；截断会污染百分比计算）。
fn saturating_i32(value: u32) -> i32 {
    value.min(i32::MAX as u32) as i32
}

/// 解析 `brightnessctl -m` 机器可读输出为亮度状态列表。
///
/// `-m` 每设备一行、逗号分隔：`dev,class,curr,pct%,max`
/// （如 `intel_backlight,backlight,1200,50%,2400`）。百分比取第 4 列（0 基下标 3）。
fn parse_brightnessctl_machine_output(out: &str) -> Vec<BrightnessState> {
    let mut states = Vec::new();
    for line in out.lines() {
        let fields: Vec<&str> = line.trim().split(',').collect();
        let name = fields.first().map(|s| s.trim()).unwrap_or("");
        if name.is_empty() {
            continue;
        }
        let Some(pct) = fields
            .get(3)
            .and_then(|s| s.trim().trim_end_matches('%').parse::<u8>().ok())
        else {
            continue;
        };
        states.push(BrightnessState {
            monitor: name.to_string(),
            brightness: pct,
            max_brightness: 100,
            adaptive: false,
        });
    }
    states
}

#[cfg(test)]
mod tests {
    use super::*;

    /// 不存在的 brightnessctl 路径：覆盖依赖缺失分支，无需真实二进制。
    const MISSING_CTL: &str = "/nonexistent/brightnessctl";

    /// 离线控制器：不探 bus（`conn=None`）、brightnessctl 与 sysfs 根均可注入，
    /// 三段降级链（powerdevil/brightnessctl/sysfs）全部可控。
    fn controller(brightnessctl_bin: &str, sysfs_root: &Path) -> BrightnessController {
        BrightnessController {
            conn: None,
            brightnessctl_bin: PathBuf::from(brightnessctl_bin),
            sysfs_root: sysfs_root.to_path_buf(),
        }
    }

    /// 绑定给定连接的控制器（私有 mock bus 覆盖 powerdevil 层）。
    fn controller_on_bus(
        conn: zbus::Connection,
        brightnessctl_bin: &str,
        sysfs_root: &Path,
    ) -> BrightnessController {
        BrightnessController {
            conn: Some(conn),
            ..controller(brightnessctl_bin, sysfs_root)
        }
    }

    /// 造一个背光设备目录（`brightness` + `max_brightness` 两个 sysfs 属性）。
    fn write_device(root: &Path, name: &str, current: u32, max: u32) -> PathBuf {
        let dir = root.join(name);
        std::fs::create_dir_all(&dir).expect("create backlight device dir");
        std::fs::write(dir.join("max_brightness"), max.to_string()).expect("write max_brightness");
        std::fs::write(dir.join("brightness"), current.to_string()).expect("write brightness");
        dir
    }

    #[tokio::test]
    async fn missing_brightnessctl_reports_install_hint_and_tried_fallback() {
        // 依赖缺失且无 sysfs 背光设备（台式机/VM）：报错须带安装命令与已尝试的
        // 兜底路径，而非裸 "brightnessctl not installed"。
        let dir = tempfile::tempdir().expect("tempdir");
        let ctl = controller(MISSING_CTL, dir.path());
        let err = ctl.get().await.expect_err("no brightness backend");
        assert!(
            matches!(err, AgentShellError::BackendUnavailable(_)),
            "无后端应为 BackendUnavailable（CLI exit 2），got {err:?}"
        );
        let msg = err.to_string();
        for needle in [
            "brightnessctl not installed",
            "pacman -S brightnessctl",
            "apt install brightnessctl",
            "no readable backlight device",
        ] {
            assert!(msg.contains(needle), "missing {needle:?} in: {msg}");
        }
    }

    #[tokio::test]
    async fn sysfs_backlight_read_when_brightnessctl_missing() {
        // brightnessctl 缺失 → sysfs 兜底读生效：`actual_brightness` 优先于
        // `brightness`，多设备按名称排序输出。
        let dir = tempfile::tempdir().expect("tempdir");
        let intel = write_device(dir.path(), "intel_backlight", 2400, 2400);
        std::fs::write(intel.join("actual_brightness"), "1200").expect("write actual_brightness");
        write_device(dir.path(), "amdgpu_bl1", 100, 250);

        let states = controller(MISSING_CTL, dir.path())
            .get()
            .await
            .expect("sysfs fallback");
        assert_eq!(states.len(), 2, "{states:?}");
        assert_eq!(states[0].monitor, "amdgpu_bl1");
        assert_eq!(states[0].brightness, 40);
        assert_eq!(states[0].max_brightness, 100);
        assert_eq!(states[1].monitor, "intel_backlight");
        assert_eq!(states[1].brightness, 50);
    }

    #[tokio::test]
    async fn sysfs_backlight_set_writes_scaled_absolute_value() {
        // brightnessctl 缺失 → set 落 sysfs `brightness`：50% × 250 = 125。
        let dir = tempfile::tempdir().expect("tempdir");
        let dev = write_device(dir.path(), "amdgpu_bl1", 40, 250);
        controller(MISSING_CTL, dir.path())
            .set(50)
            .await
            .expect("sysfs write");
        assert_eq!(
            std::fs::read_to_string(dev.join("brightness")).expect("read back"),
            "125"
        );
    }

    #[tokio::test]
    async fn sysfs_device_without_readable_brightness_reports_execution_failure() {
        // 设备已发现（max_brightness 可读）但亮度属性读不出：后端存在仅执行
        // 失败 → 保留读取详情报 BackendError（exit 1），不得退化成 exit 2。
        let dir = tempfile::tempdir().expect("tempdir");
        let dev = write_device(dir.path(), "amdgpu_bl1", 100, 250);
        std::fs::remove_file(dev.join("brightness")).expect("drop brightness attr");

        let err = controller(MISSING_CTL, dir.path())
            .get()
            .await
            .expect_err("unreadable device");
        assert!(
            !matches!(err, AgentShellError::BackendUnavailable(_)),
            "已发现的设备读失败应为 BackendError（exit 1），got {err:?}"
        );
        let msg = err.to_string();
        assert!(msg.contains("amdgpu_bl1/brightness"), "{msg}");
    }

    #[tokio::test]
    async fn sysfs_partial_read_keeps_readable_devices() {
        // 多设备中单个读取失败（热插拔竞态）不拖垮整体：可读设备照常返回。
        let dir = tempfile::tempdir().expect("tempdir");
        write_device(dir.path(), "intel_backlight", 1200, 2400);
        let broken = write_device(dir.path(), "amdgpu_bl1", 100, 250);
        std::fs::remove_file(broken.join("brightness")).expect("drop brightness attr");

        let states = controller(MISSING_CTL, dir.path())
            .get()
            .await
            .expect("readable device wins");
        assert_eq!(states.len(), 1, "{states:?}");
        assert_eq!(states[0].monitor, "intel_backlight");
        assert_eq!(states[0].brightness, 50);
    }

    // ── powerdevil（私有 mock bus）────────────────────────────────────
    // 真机 powerdevil 只在有背光设备的会话导出亮度接口，CI/本机无法覆盖
    // 「服务在但调用失败」与「服务不在」两条分类路径，故用私有 bus 上的
    // mock 服务逐一验证退出码语义。

    /// 调用必失败的 powerdevil：名字已被占用（服务在）但查询/设置报错。
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

    /// 独立私有 session bus（避免与真机/并行测试竞争 `Connection::session()`）。
    struct TestBus {
        addr: String,
        _child: std::process::Child,
    }

    impl TestBus {
        async fn start() -> Self {
            let mut child = std::process::Command::new("dbus-daemon")
                .args(["--session", "--nofork", "--print-address=1"])
                .stdout(std::process::Stdio::piped())
                .stderr(std::process::Stdio::null())
                .spawn()
                .expect("dbus-daemon must be installed for powerdevil mock tests");
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

        /// 在私有 bus 上注册失败的 powerdevil，占用其服务名。
        async fn spawn_failing_powerdevil(&self) -> zbus::Connection {
            let server = self.connect().await;
            server
                .object_server()
                .at(
                    "/org/kde/Solid/PowerManagement/Actions/BrightnessControl",
                    FailingPowerdevil,
                )
                .await
                .expect("register mock powerdevil");
            let name = zbus::names::WellKnownName::try_from("org.kde.Solid.PowerManagement")
                .expect("valid bus name");
            server
                .request_name(name)
                .await
                .expect("claim org.kde.Solid.PowerManagement");
            server
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
        use std::io::Read;
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

    #[tokio::test]
    async fn failing_powerdevil_is_reported_as_execution_failure() {
        // powerdevil 服务在但调用失败，且 brightnessctl/sysfs 均不可用：
        // 必须报 BackendError（exit 1）并带 powerdevil 根因，不得吞成 exit 2。
        let bus = TestBus::start().await;
        let _server = bus.spawn_failing_powerdevil().await;
        let dir = tempfile::tempdir().expect("tempdir");
        let ctl = controller_on_bus(bus.connect().await, MISSING_CTL, dir.path());

        let err = ctl.get().await.expect_err("powerdevil query fails");
        assert!(
            !matches!(err, AgentShellError::BackendUnavailable(_)),
            "powerdevil 存在但失败应为 BackendError（exit 1），got {err:?}"
        );
        let msg = err.to_string();
        assert!(msg.contains("powerdevil brightness"), "{msg}");
        assert!(msg.contains("brightnessctl not installed"), "{msg}");

        let err = ctl.set(50).await.expect_err("powerdevil set fails");
        assert!(
            !matches!(err, AgentShellError::BackendUnavailable(_)),
            "powerdevil 存在但设置失败应为 BackendError（exit 1），got {err:?}"
        );
        assert!(
            err.to_string().contains("powerdevil setBrightness"),
            "{err}"
        );
    }

    #[tokio::test]
    async fn absent_powerdevil_falls_through_to_install_hint() {
        // 私有 bus 上无人占用 powerdevil 名（台式机 KDE 无亮度接口同形态）：
        // 该层属「不可用」静默下沉，最终仍是 BackendUnavailable（exit 2）+
        // 安装指引，不因 KDE 层新增分类而变成 exit 1。
        let bus = TestBus::start().await;
        let dir = tempfile::tempdir().expect("tempdir");
        let ctl = controller_on_bus(bus.connect().await, MISSING_CTL, dir.path());

        let err = ctl.get().await.expect_err("no brightness backend");
        assert!(
            matches!(err, AgentShellError::BackendUnavailable(_)),
            "powerdevil 不在应为 BackendUnavailable（exit 2），got {err:?}"
        );
        let msg = err.to_string();
        assert!(msg.contains("pacman -S brightnessctl"), "{msg}");
        assert!(!msg.contains("powerdevil"), "{msg}");
    }

    #[test]
    fn sysfs_permission_denied_error_carries_grant_hint() {
        let path = Path::new("/sys/class/backlight/amdgpu_bl1/brightness");
        let denied = std::io::Error::from(std::io::ErrorKind::PermissionDenied);
        let msg = sysfs_write_error(path, &denied).to_string();
        assert!(
            msg.contains("/sys/class/backlight/amdgpu_bl1/brightness"),
            "{msg}"
        );
        assert!(msg.contains("video"), "{msg}");
    }

    #[test]
    fn sysfs_readonly_mount_error_carries_remount_hint() {
        // 只读挂载下内核返回 EROFS（容器内 /sys ro 的真实形态）：正文不能只剩
        // "Read-only file system"，须带路径与恢复动作。
        let path = Path::new("/sys/class/backlight/amdgpu_bl1/brightness");
        let e = std::io::Error::from_raw_os_error(libc::EROFS);
        let msg = sysfs_write_error(path, &e).to_string();
        assert!(msg.contains("Read-only file system"), "{msg}");
        assert!(msg.contains(path.to_str().expect("ascii path")), "{msg}");
        assert!(msg.contains("read-only"), "{msg}");
        assert!(msg.contains("remount"), "{msg}");
    }

    #[test]
    fn sysfs_not_a_directory_error_carries_mount_hint() {
        // sysfs 未挂载（或路径被同名文件占用）时内核返回 ENOTDIR：需指出 sysfs
        // 可能没挂上，而非只透传 "Not a directory"。
        let path = Path::new("/sys/class/backlight/amdgpu_bl1/brightness");
        let e = std::io::Error::from_raw_os_error(libc::ENOTDIR);
        let msg = sysfs_write_error(path, &e).to_string();
        assert!(msg.contains("Not a directory"), "{msg}");
        assert!(msg.contains(path.to_str().expect("ascii path")), "{msg}");
        assert!(msg.contains("not mounted"), "{msg}");
    }

    #[test]
    fn combined_failures_keep_backend_error_semantics() {
        // CLI 退出码契约：全层无后端 → BackendUnavailable（exit 2）；任一层
        // 「后端在但执行失败」→ BackendError（exit 1），不因新增兜底层降级。
        let no_backend = combine_failures(&[
            AgentShellError::BackendUnavailable("brightnessctl not installed".into()),
            AgentShellError::BackendUnavailable("no readable backlight device".into()),
        ]);
        assert!(matches!(no_backend, AgentShellError::BackendUnavailable(_)));
        let detail = no_backend.to_string();
        // 内层正文即报错正文；合并结果只保留一层 "Backend not available:" 前缀。
        assert_eq!(
            detail.matches("Backend not available").count(),
            1,
            "{detail}"
        );
        assert!(detail.contains("brightnessctl not installed"), "{detail}");
        assert!(detail.contains("no readable backlight device"), "{detail}");

        let ran_and_failed = combine_failures(&[
            AgentShellError::BackendUnavailable("brightnessctl not installed".into()),
            AgentShellError::Other("write /sys/class/backlight/x/brightness: denied".into()),
        ]);
        assert!(matches!(ran_and_failed, AgentShellError::Other(_)));
    }

    #[test]
    fn normalize_maps_absolute_to_percent_and_clamps() {
        assert_eq!(normalize_to_percent(50, 100), 50);
        assert_eq!(normalize_to_percent(0, 100), 0);
        assert_eq!(normalize_to_percent(100, 100), 100);
        assert_eq!(normalize_to_percent(2400, 4800), 50);
        // 超界钳制
        assert_eq!(normalize_to_percent(150, 100), 100);
        assert_eq!(normalize_to_percent(-5, 100), 0);
        // 非法 max 退化为 0，不除零
        assert_eq!(normalize_to_percent(10, 0), 0);
        assert_eq!(normalize_to_percent(10, -1), 0);
        // i32 极值：i64 中间积不溢出、百分比仍钳制到 [0, 100]。
        assert_eq!(normalize_to_percent(i32::MAX, 100), 100);
        assert_eq!(normalize_to_percent(i32::MAX, i32::MAX), 100);
    }

    #[test]
    fn percent_to_absolute_scales_and_clamps() {
        assert_eq!(percent_to_absolute(50, 100), 50);
        assert_eq!(percent_to_absolute(0, 100), 0);
        assert_eq!(percent_to_absolute(100, 4800), 4800);
        assert_eq!(percent_to_absolute(50, 4800), 2400);
        assert_eq!(percent_to_absolute(200, 100), 100);
        assert_eq!(percent_to_absolute(50, 0), 0);
        // i32::MAX max：i64 中间积不溢出、结果钳制到 max。
        assert_eq!(percent_to_absolute(100, i32::MAX), i32::MAX);
    }

    #[test]
    fn parses_single_backlight_device() {
        let out = "intel_backlight,backlight,1200,50%,2400\n";
        let states = parse_brightnessctl_machine_output(out);
        assert_eq!(states.len(), 1);
        assert_eq!(states[0].monitor, "intel_backlight");
        assert_eq!(states[0].brightness, 50);
        assert_eq!(states[0].max_brightness, 100);
    }

    #[test]
    fn parses_multiple_backlight_devices() {
        let out = "amdgpu_bl1,backlight,100,40%,250\n\
                   intel_backlight,backlight,60,25%,240\n";
        let states = parse_brightnessctl_machine_output(out);
        assert_eq!(states.len(), 2);
        assert_eq!(states[0].monitor, "amdgpu_bl1");
        assert_eq!(states[0].brightness, 40);
        assert_eq!(states[1].monitor, "intel_backlight");
        assert_eq!(states[1].brightness, 25);
    }

    #[test]
    fn ignores_malformed_or_missing_brightness_lines() {
        assert!(parse_brightnessctl_machine_output("").is_empty());
        // 列数不足（缺百分比列）→ 不产出状态。
        assert!(parse_brightnessctl_machine_output("intel_backlight,backlight,1200\n").is_empty());
        // 百分比非数字 → 丢弃。
        assert!(
            parse_brightnessctl_machine_output("intel_backlight,backlight,1200,xx%,2400\n")
                .is_empty()
        );
        // 设备名为空 → 丢弃。
        assert!(parse_brightnessctl_machine_output(",backlight,1200,50%,2400\n").is_empty());
        // 非逗号分隔的旧人类可读格式不再被解析（防回归到错误格式）。
        assert!(parse_brightnessctl_machine_output(
            "Device 'intel_backlight' of class 'backlight':\n\tCurrent brightness: 1200 (50%)\n"
        )
        .is_empty());
    }
}
