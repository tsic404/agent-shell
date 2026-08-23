//! 系统服务组件数据类型。
//!
//! 对应设计文档 §21（系统服务组件）：音频/网络/启动器/通知/电源/剪贴板/外观/
//! 显示布局/蓝牙/系统服务等组件的数据结构。本模块仅定义数据类型，trait 契约在
//! `crate::component` 中定义。

use serde::{Deserialize, Serialize};

use crate::types::{MonitorId, UnitStatus};

// ───────────────────────── systemd / logind ─────────────────────────

/// systemd 单元信息。
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct SystemdUnit {
    /// 单元名称（如 "nginx.service"）
    pub name: String,
    /// 单元描述
    pub description: String,
    /// 当前状态
    pub status: UnitStatus,
    /// 是否 enabled（开机自启）
    pub enabled: bool,
    /// 主 PID
    pub main_pid: Option<u32>,
    /// 加载状态（"loaded" / "not-found" / "masked" / "error"）
    pub load_state: String,
}

/// systemd timer 单元信息。
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct SystemdTimer {
    /// timer 单元名称（如 "backup.timer"）
    pub name: String,
    /// 下次触发时间（ISO 时间字符串，realtime）
    pub next_elapse_real: String,
    /// 上次触发时间（ISO 时间字符串，realtime）
    pub last_trigger_real: String,
    /// 单元文件路径
    pub unit_path: String,
    /// 下次触发时间（monotonic，μs）
    pub next_elapse_monotonic: u64,
    /// 上次触发时间（monotonic，μs）
    pub last_trigger_monotonic: u64,
    /// 是否正在运行
    pub running: bool,
}

// ───────────────────────── 显示布局 ─────────────────────────

/// 监视器配置（用于 apply_monitor_layout）。
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct MonitorConfig {
    /// 监视器标识
    pub id: MonitorId,
    /// 是否启用
    pub enabled: bool,
    /// 分辨率 (width, height)
    pub resolution: (u32, u32),
    /// 位置 (x, y)
    pub position: (i32, i32),
    /// 缩放因子
    pub scale: f64,
    /// 刷新率（Hz）
    pub refresh_rate: f64,
    /// 是否主显示器
    pub primary: bool,
    /// 变换（旋转/翻转）
    pub transform: MonitorTransform,
    /// DPI
    pub dpi: u32,
}

/// 监视器变换（旋转/翻转）。
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub enum MonitorTransform {
    /// 正常
    Normal,
    /// 顺时针 90°
    Rot90,
    /// 180°
    Rot180,
    /// 顺时针 270°
    Rot270,
    /// 水平翻转
    FlipX,
    /// 垂直翻转
    FlipY,
}

/// 监视器布局（当前状态快照）。
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct MonitorLayout {
    /// 配置序列号（用于 ApplyMonitorsConfig 的乐观锁）
    pub serial: u32,
    /// 监视器配置列表
    pub monitors: Vec<MonitorConfig>,
}

// ───────────────────────── 音频 ─────────────────────────

/// 音频状态。
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct AudioState {
    /// 主音量（0.0 - 1.0）
    pub volume: f64,
    /// 是否静音
    pub muted: bool,
    /// 默认输出设备名
    pub default_sink: String,
}

/// 音频设备。
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct AudioDevice {
    /// 设备名
    pub name: String,
    /// 描述
    pub description: String,
    /// 音量（0.0 - 1.0）
    pub volume: f64,
    /// 是否静音
    pub muted: bool,
    /// 是否为默认设备
    pub is_default: bool,
    /// 设备类型
    pub device_type: AudioDeviceType,
}

/// 音频设备类型。
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub enum AudioDeviceType {
    /// 输出（sink）
    Sink,
    /// 输入（source）
    Source,
}

// ───────────────────────── 网络 ─────────────────────────

/// 网络状态。
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct NetworkState {
    /// 连通性
    pub connectivity: Connectivity,
    /// WiFi 是否启用
    pub wifi_enabled: bool,
    /// 当前活动 SSID
    pub active_ssid: Option<String>,
    /// IP 地址
    pub ip_address: Option<String>,
    /// 是否计费网络
    pub metered: bool,
}

/// 网络连通性。
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub enum Connectivity {
    /// 完全连通
    Full,
    /// 受限
    Limited,
    /// 仅本地
    Local,
    /// 无连接
    None,
}

/// WiFi 网络（扫描结果）。
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct WifiNetwork {
    /// SSID
    pub ssid: String,
    /// 信号强度（0-100）
    pub strength: u8,
    /// 是否加密
    pub secured: bool,
    /// 频率（MHz）
    pub frequency: u32,
    /// 是否已知网络
    pub known: bool,
}

/// 蓝牙设备。
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct BtDevice {
    /// MAC 地址
    pub address: String,
    /// 设备名
    pub name: String,
    /// 是否已配对
    pub paired: bool,
    /// 是否已连接
    pub connected: bool,
    /// 是否受信任
    pub trusted: bool,
    /// 信号强度（dBm）
    pub rssi: Option<i16>,
    /// 服务 UUID 列表
    pub uuids: Vec<String>,
}

// ───────────────────────── 应用启动器 ─────────────────────────

/// 应用信息。
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct AppInfo {
    /// 应用 ID（.desktop 文件名去扩展名）
    pub app_id: String,
    /// 显示名
    pub name: String,
    /// 图标名
    pub icon: Option<String>,
    /// 分类列表
    pub categories: Vec<String>,
    /// .desktop 文件路径
    pub desktop_file: String,
    /// 执行命令
    pub exec: String,
    /// 是否 GUI 应用
    pub is_gui: bool,
}

/// 应用启动目标。
#[derive(Clone, Debug, Serialize, Deserialize)]
pub enum AppTarget {
    /// 按 .desktop 文件路径或 ID 启动
    ByDesktopFile(String),
    /// 按 app_id 启动
    ByAppId(String),
    /// 直接执行命令
    ByCommand(String),
    /// 用默认应用打开 URI
    OpenUri(String),
}

// ───────────────────────── 通知 ─────────────────────────

/// 通知规格。
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct NotificationSpec {
    /// 摘要（标题）
    pub summary: String,
    /// 正文
    pub body: Option<String>,
    /// 紧急程度
    pub urgency: NotificationUrgency,
    /// 图标名或路径
    pub icon: Option<String>,
    /// 超时（ms，None = 服务默认）
    pub timeout_ms: Option<i32>,
    /// 操作按钮列表（action_key, label）
    pub actions: Vec<(String, String)>,
}

/// 通知紧急程度。
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub enum NotificationUrgency {
    /// 低
    Low,
    /// 普通
    Normal,
    /// 紧急
    Critical,
}

// ───────────────────────── 电源 ─────────────────────────

/// 电池状态。
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct BatteryState {
    /// 电量百分比（0.0 - 100.0）
    pub percentage: f64,
    /// 是否正在充电
    pub charging: bool,
    /// 距离耗尽时间（秒）
    pub time_to_empty: Option<u32>,
    /// 距离充满时间（秒）
    pub time_to_full: Option<u32>,
    /// 电池是否存在
    pub is_present: bool,
}

/// 电源状态。
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub enum PowerState {
    /// 开机
    On,
    /// 挂起
    Suspend,
    /// 休眠
    Hibernate,
    /// 关机
    Off,
}

// ───────────────────────── 外观 ─────────────────────────

/// 配色方案。
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub enum ColorScheme {
    /// 无偏好
    NoPreference,
    /// 深色
    Dark,
    /// 浅色
    Light,
}

// ───────────────────────── 无障碍 ─────────────────────────

/// 无障碍树变化类型。
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub enum AccessibilityChange {
    /// 节点添加
    NodeAdded,
    /// 节点移除
    NodeRemoved,
    /// 属性变化
    PropertyChanged,
}
