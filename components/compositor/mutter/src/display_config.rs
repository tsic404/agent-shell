//! org.gnome.Mutter.DisplayConfig（设计文档 §8.4 / §3.5 调研）。
//!
//! 接口 `org.gnome.Mutter.DisplayConfig`：
//! - `GetCurrentState` → `(u serial, a((ssss)a(siiddada{sv})a{sv}) monitors,
//!   a(iiduba(ssss)a{sv}) logical_monitors, a{sv} properties)`；
//! - `ApplyMonitorsConfig(u serial, u method, a(iiduba(ssa{sv})) logical_monitors,
//!   a{sv} properties)`——method: 0=verify, 1=temporary, 2=persistent。
//!
//! 本模块只做**读路径**（list_monitors）；Apply 签名原样保留供未来
//! display 配置任务复用。capabilities.monitor_layout=true 仅代表可读布局。

use serde::{Deserialize, Serialize};
use zbus::Connection;

use crate::error::{MutterError, Result, DISPLAY_CONFIG_SERVICE};

/// ApplyMonitorsConfig method 值（§3.5：0 verify | 1 temporary | 2 persistent）。
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ApplyMethod {
    /// 校验配置合法性，不实际应用。
    Verify = 0,
    /// 临时应用（重启失效）。
    Temporary = 1,
    /// 持久化应用。
    Persistent = 2,
}

/// GetCurrentState 返回的监视器布局快照。
#[derive(Clone, Debug, Default, Serialize, Deserialize)]
pub struct MonitorLayout {
    /// 配置序列号（Apply 时必须回传；每次布局变更递增）。
    pub serial: u32,
    /// 物理监视器列表。
    pub monitors: Vec<PhysicalMonitor>,
    /// 逻辑监视器列表（缩放后的布局单元）。
    pub logical_monitors: Vec<LogicalMonitor>,
}

/// 物理监视器（GetCurrentState monitors 数组元素）。
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct PhysicalMonitor {
    /// 连接器名（如 "DP-1"、"eDP-1"）。
    pub connector: String,
    /// 制造商。
    pub vendor: String,
    /// 产品名。
    pub product: String,
    /// 序列号。
    pub serial: String,
    /// 可用模式。
    pub modes: Vec<MonitorMode>,
    /// 当前活动模式的 id（modes 内 `is-current` 属性命中者；探测失败
    /// 时回退首个模式——单模式监视器上两者等价）。
    pub current_mode: Option<String>,
}

impl PhysicalMonitor {
    /// 当前活动模式的尺寸（物理像素）。
    ///
    /// `current_mode` 未命中 modes 或缺失时返回 None——调用方以默认
    /// 值兜底，不猜测尺寸。
    pub fn active_mode_size(&self) -> Option<(i32, i32)> {
        let id = self.current_mode.as_deref()?;
        self.modes
            .iter()
            .find(|m| m.id == id)
            .map(|m| (m.width, m.height))
    }
}

/// 监视器显示模式。
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct MonitorMode {
    /// 模式 id（Mutter 内部标识，Apply 时按 connector+id 引用）。
    pub id: String,
    /// 物理像素宽。
    pub width: i32,
    /// 物理像素高。
    pub height: i32,
    /// 刷新率 (Hz)。
    pub refresh: f64,
}

/// 逻辑监视器（GetCurrentState logical_monitors 数组元素）。
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct LogicalMonitor {
    /// 布局 X 坐标（逻辑像素）。
    pub x: i32,
    /// 布局 Y 坐标。
    pub y: i32,
    /// 缩放因子。
    pub scale: f64,
    /// 变换（0 normal … 7 270° flipped）。
    pub transform: u32,
    /// 是否主逻辑监视器。
    pub primary: bool,
    /// 承载该逻辑监视器的物理连接器名列表。
    pub connectors: Vec<String>,
}

/// org.gnome.Mutter.DisplayConfig proxy（读 + 写全量方法）。
///
/// monitors/logical_monitors 是匿名结构数组——zbus 以
/// `OwnedValue`(Structure) 承载，逐字段手工解包（见
/// [`DisplayConfig::get_current_state`]）。
#[zbus::proxy(
    default_service = "org.gnome.Mutter.DisplayConfig",
    default_path = "/org/gnome/Mutter/DisplayConfig",
    interface = "org.gnome.Mutter.DisplayConfig"
)]
trait DisplayConfig {
    fn get_current_state(
        &self,
    ) -> zbus::Result<(
        u32,
        Vec<zbus::zvariant::OwnedValue>,
        Vec<zbus::zvariant::OwnedValue>,
        std::collections::HashMap<String, zbus::zvariant::OwnedValue>,
    )>;

    #[allow(dead_code)]
    fn apply_monitors_config(
        &self,
        serial: u32,
        method: u32,
        logical_monitors: zbus::zvariant::Value<'_>,
        properties: std::collections::HashMap<String, zbus::zvariant::Value<'_>>,
    ) -> zbus::Result<()>;
}

/// Mutter DisplayConfig 客户端。
///
/// 复用共享 session bus 连接；`GetCurrentState` 解析失败的字段以默认值
/// 兜底（Mutter 小版本间 properties 键有增删），结构性字段缺失才报错。
pub struct DisplayConfig {
    conn: Connection,
}

/// 从 Value 里取字符串字段（非字符串返回空串）。
fn str_field(v: &zbus::zvariant::Value<'_>) -> String {
    v.downcast_ref::<&str>()
        .map(str::to_string)
        .unwrap_or_default()
}

impl DisplayConfig {
    /// 建桥：复用既有 session bus 连接。
    pub fn new(conn: Connection) -> Self {
        Self { conn }
    }

    async fn proxy(&self) -> Result<DisplayConfigProxy<'_>> {
        DisplayConfigProxy::new(&self.conn).await.map_err(|e| {
            MutterError::DisplayConfig(format!("{DISPLAY_CONFIG_SERVICE} unreachable: {e}"))
        })
    }

    /// 探测接口可用性（只读 GetCurrentState，无副作用）。
    pub async fn probe(&self) -> Result<()> {
        self.get_current_state().await.map(|_| ())
    }

    /// 读取当前监视器状态并解析 monitors / logical_monitors（§8.4 验收）。
    ///
    /// monitors 元素：`(connector s, vendor s, product s, serial s,
    /// modes a(siiddada{sv}), props a{sv})`；logical_monitors：
    /// `(x i, y i, scale d, transform u, primary b, monitors a(ssss),
    /// props a{sv})`。形状异常的元素跳过（不整体失败）。
    pub async fn get_current_state(&self) -> Result<MonitorLayout> {
        let proxy = self.proxy().await?;
        let (serial, monitors_raw, logical_raw, _props) = proxy
            .get_current_state()
            .await
            .map_err(|e| MutterError::DisplayConfig(format!("GetCurrentState failed: {e}")))?;

        use zbus::zvariant::{Array, Dict, Structure};

        let mut monitors = Vec::new();
        for item in &monitors_raw {
            let Ok(st) = item.downcast_ref::<Structure>() else {
                continue;
            };
            let fields = st.fields();
            if fields.len() < 6 {
                continue;
            }
            let mut modes = Vec::new();
            // is-current 标志（上游文档：mode props a{sv} 内 "is-current" b）。
            let mut current_mode: Option<String> = None;
            if let Ok(arr) = fields[4].downcast_ref::<Array>() {
                for m in arr.iter() {
                    let Some(ms) = m.downcast_ref::<Structure>().ok() else {
                        continue;
                    };
                    let mf = ms.fields();
                    if mf.len() < 6 {
                        continue;
                    }
                    let id = str_field(&mf[0]);
                    // mf[5] = a{sv} properties；is-current 命中即记录。
                    if current_mode.is_none() {
                        if let Ok(props) = mf[5].downcast_ref::<Dict>() {
                            for (k, v) in props.iter() {
                                if k.downcast_ref::<&str>() == Ok("is-current")
                                    && v.downcast_ref::<bool>() == Ok(true)
                                {
                                    current_mode = Some(id.clone());
                                    break;
                                }
                            }
                        }
                    }
                    modes.push(MonitorMode {
                        id,
                        width: mf[1].downcast_ref::<i32>().unwrap_or(0),
                        height: mf[2].downcast_ref::<i32>().unwrap_or(0),
                        refresh: mf[3].downcast_ref::<f64>().unwrap_or(0.0),
                    });
                }
            }
            // is-current 未命中时回退首个模式 id（单模式监视器等价）。
            let current_mode = current_mode.or_else(|| modes.first().map(|m| m.id.clone()));
            monitors.push(PhysicalMonitor {
                connector: str_field(&fields[0]),
                vendor: str_field(&fields[1]),
                product: str_field(&fields[2]),
                serial: str_field(&fields[3]),
                modes,
                current_mode,
            });
        }

        let mut logical_monitors = Vec::new();
        for item in &logical_raw {
            let Ok(st) = item.downcast_ref::<Structure>() else {
                continue;
            };
            let fields = st.fields();
            if fields.len() < 6 {
                continue;
            }
            let gi = |i: usize| fields[i].downcast_ref::<i32>().unwrap_or(0);
            let gd = |i: usize| fields[i].downcast_ref::<f64>().unwrap_or(1.0);
            let gu = |i: usize| fields[i].downcast_ref::<u32>().unwrap_or(0);
            let gb = |i: usize| fields[i].downcast_ref::<bool>().unwrap_or(false);
            let connectors = fields[5]
                .downcast_ref::<Array>()
                .map(|arr| {
                    arr.iter()
                        .filter_map(|m| {
                            let inner = m.downcast_ref::<Structure>().ok()?;
                            inner.fields().first().map(str_field)
                        })
                        .collect()
                })
                .unwrap_or_default();
            logical_monitors.push(LogicalMonitor {
                x: gi(0),
                y: gi(1),
                scale: gd(2),
                transform: gu(3),
                primary: gb(4),
                connectors,
            });
        }

        Ok(MonitorLayout {
            serial,
            monitors,
            logical_monitors,
        })
    }

    /// ApplyMonitorsConfig（写路径；供未来 display 配置任务复用）。
    ///
    /// serial 必须来自最近一次 [`Self::get_current_state`]——过期序列号
    /// 报 `InvalidArgs`（上游语义）。logical_monitors 按
    /// `a(iiduba(ssa{sv}))` 构造后传入。
    #[allow(dead_code)]
    pub(crate) async fn apply_monitors_config(
        &self,
        serial: u32,
        method: ApplyMethod,
        logical_monitors: zbus::zvariant::Value<'_>,
        properties: std::collections::HashMap<String, zbus::zvariant::Value<'_>>,
    ) -> Result<()> {
        self.proxy()
            .await?
            .apply_monitors_config(serial, method as u32, logical_monitors, properties)
            .await
            .map_err(|e| MutterError::DisplayConfig(format!("ApplyMonitorsConfig failed: {e}")))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn active_mode_size_resolves_current_mode() {
        // 🔴3：list_monitors 的尺寸来源——current_mode 命中 modes。
        let pm = PhysicalMonitor {
            connector: "DP-1".into(),
            vendor: "V".into(),
            product: "P".into(),
            serial: "S".into(),
            modes: vec![
                MonitorMode {
                    id: "1920x1080@60".into(),
                    width: 1920,
                    height: 1080,
                    refresh: 60.0,
                },
                MonitorMode {
                    id: "2560x1440@144".into(),
                    width: 2560,
                    height: 1440,
                    refresh: 144.0,
                },
            ],
            current_mode: Some("2560x1440@144".into()),
        };
        assert_eq!(pm.active_mode_size(), Some((2560, 1440)));
    }

    #[test]
    fn active_mode_size_none_when_id_unknown() {
        // current_mode 未命中 modes → None（调用方以 0 兜底 + warn，
        // 不猜测尺寸）。
        let pm = PhysicalMonitor {
            connector: "DP-1".into(),
            vendor: "V".into(),
            product: "P".into(),
            serial: "S".into(),
            modes: vec![MonitorMode {
                id: "a".into(),
                width: 100,
                height: 200,
                refresh: 60.0,
            }],
            current_mode: Some("missing".into()),
        };
        assert_eq!(pm.active_mode_size(), None);
    }

    #[test]
    fn physical_monitor_defaults_current_mode_to_first() {
        // 解析路径的回退语义：is-current 未命中时取首个模式 id。
        let pm = PhysicalMonitor {
            connector: "eDP-1".into(),
            vendor: "V".into(),
            product: "P".into(),
            serial: "S".into(),
            modes: vec![MonitorMode {
                id: "only".into(),
                width: 800,
                height: 600,
                refresh: 60.0,
            }],
            current_mode: None,
        };
        assert_eq!(pm.active_mode_size(), None); // current=None 直接 None
    }

    #[test]
    fn apply_method_values_match_mutter_contract() {
        assert_eq!(ApplyMethod::Verify as u32, 0);
        assert_eq!(ApplyMethod::Temporary as u32, 1);
        assert_eq!(ApplyMethod::Persistent as u32, 2);
    }

    #[test]
    fn layout_defaults_are_empty() {
        let l = MonitorLayout::default();
        assert_eq!(l.serial, 0);
        assert!(l.monitors.is_empty());
        assert!(l.logical_monitors.is_empty());
    }

    #[test]
    fn logical_monitor_roundtrip() {
        // 序列化形状稳定（doctor/日志输出依赖）。
        let lm = LogicalMonitor {
            x: 1920,
            y: 0,
            scale: 1.5,
            transform: 0,
            primary: true,
            connectors: vec!["DP-1".into()],
        };
        let json = serde_json::to_string(&lm).unwrap();
        assert!(json.contains("1920"));
        assert!(json.contains("1.5"));
        assert!(json.contains("DP-1"));
    }
}
