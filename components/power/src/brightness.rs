//! 屏幕亮度控制器（设计文档 §21.25 屏幕亮度）。
//!
//! 「DE 封装优先 → 公共降级」：KDE 会话走 powerdevil
//! `org.kde.Solid.PowerManagement.Actions.BrightnessControl`；其余环境回退
//! `brightnessctl` CLI（sysfs backlight，跨 DE 保底）。二者统一以 0-100 百分比
//! 对外，屏蔽绝对值与多背光设备的差异。

use agent_shell_core::error::{AgentShellError, Result};
use agent_shell_core::services::BrightnessState;
use async_trait::async_trait;
use zbus::proxy;

/// 亮度能力契约（daemon 持 `dyn BrightnessOps`，测试注入 fake）。
#[async_trait]
pub trait BrightnessOps: Send + Sync {
    /// 查询当前亮度状态列表。
    async fn get(&self) -> Result<Vec<BrightnessState>>;
    /// 设置亮度（0-100 百分比）。
    async fn set(&self, value: u8) -> Result<()>;
}

/// powerdevil 亮度子接口（session bus）。
#[proxy(
    interface = "org.kde.Solid.PowerManagement.Actions.BrightnessControl",
    default_service = "org.kde.Solid.PowerManagement",
    default_path = "/org/kde/Solid/PowerManagement/Actions/BrightnessControl"
)]
trait BrightnessControl {
    /// 当前亮度（绝对值，0..=brightness_max）。
    fn brightness(&self) -> zbus::Result<i32>;
    /// 最大亮度值。
    fn brightness_max(&self) -> zbus::Result<i32>;
    /// 设置亮度（绝对值）。
    fn set_brightness(&self, value: i32) -> zbus::Result<()>;
}

/// 亮度控制器：KDE powerdevil 优先，`brightnessctl` 公共降级。
pub struct BrightnessController {
    conn: Option<zbus::Connection>,
}

impl BrightnessController {
    /// 构造：探测 session bus。bus 不可达时 `conn=None`，仅保留
    /// `brightnessctl` 降级路径——构造永不失败（headless 亦可用）。
    pub async fn new() -> Self {
        let conn = zbus::Connection::session().await.ok();
        Self { conn }
    }

    async fn kde(&self) -> Option<BrightnessControlProxy<'static>> {
        let conn = self.conn.as_ref()?;
        BrightnessControlProxy::builder(conn)
            .cache_properties(zbus::proxy::CacheProperties::No)
            .build()
            .await
            .ok()
    }

    /// 查询当前亮度。
    async fn kde_get(&self) -> Option<BrightnessState> {
        let kde = self.kde().await?;
        let current = kde.brightness().await.ok()?;
        let max = kde.brightness_max().await.ok()?;
        Some(BrightnessState {
            monitor: "default".into(),
            brightness: normalize_to_percent(current, max),
            max_brightness: 100,
            adaptive: false,
        })
    }

    async fn kde_set(&self, value: u8) -> bool {
        let Some(kde) = self.kde().await else {
            return false;
        };
        let Ok(max) = kde.brightness_max().await else {
            return false;
        };
        let target = percent_to_absolute(value, max);
        match kde.set_brightness(target).await {
            Ok(()) => true,
            Err(e) => {
                tracing::debug!(
                    error = %e,
                    "powerdevil setBrightness failed; falling back to brightnessctl"
                );
                false
            }
        }
    }

    async fn brightnessctl_get(&self) -> Result<Vec<BrightnessState>> {
        let out = run_brightnessctl(&["-m"]).await?;
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
        run_brightnessctl(&["set", &spec]).await?;
        Ok(())
    }
}

#[async_trait]
impl BrightnessOps for BrightnessController {
    async fn get(&self) -> Result<Vec<BrightnessState>> {
        if let Some(state) = self.kde_get().await {
            return Ok(vec![state]);
        }
        self.brightnessctl_get().await
    }

    async fn set(&self, value: u8) -> Result<()> {
        if self.kde_set(value).await {
            return Ok(());
        }
        self.brightnessctl_set(value).await
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
/// 错误分类：二进制缺失（NotFound）→ BackendUnavailable（无后端，CLI exit 2）；
/// 其余 spawn 失败与非零退出（如 sysfs 写权限不足）→ BackendError（后端在但
/// 执行失败，CLI exit 1），二者不可混淆。
async fn run_brightnessctl(args: &[&str]) -> Result<String> {
    let out = tokio::process::Command::new("brightnessctl")
        .args(args)
        .output()
        .await
        .map_err(|e| match e.kind() {
            std::io::ErrorKind::NotFound => {
                AgentShellError::BackendUnavailable("brightnessctl not installed".into())
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
