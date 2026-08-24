//! DDE 音频封装：`org.deepin.dde.Audio1` / `com.deepin.daemon.Audio`（session bus）。
//!
//! design/13 §21.7（实现示例）+ §21.36.1/§21.36.4（版本兼容实测）：
//! 根对象只暴露属性与 Sink 子对象枚举，音量控制必须落到 Sink 子对象
//! `SetVolume(d)` / `SetMute(b)`。本模块用 zbus 动态代理按小步探测装配，
//! 不生成静态 proxy 宏——两版服务名/路径不同但方法面一致，动态调用点少。

use async_trait::async_trait;
use zbus::zvariant::OwnedObjectPath;

use agent_shell_audio::router::DeAudioWrapper;
use agent_shell_core::component::{
    AudioServerComponent, ComponentHealth, ComponentType, DesktopComponent,
};
use agent_shell_core::error::{AgentShellError, Result};
use agent_shell_core::services::{AudioDevice, AudioDeviceType, AudioState};

/// DDE25 主服务名。
pub const DDE25_AUDIO: &str = "org.deepin.dde.Audio1";
/// DDE20 服务名（DDE25 上为别名）。
pub const DDE20_AUDIO: &str = "com.deepin.daemon.Audio";

/// 两版根对象路径约定：`/{base}/Audio1` 或 `/{base}/Audio`（§21.36.4）。
const ROOT_PATH_CANDIDATES: [&str; 3] = [
    "/org/deepin/dde/Audio1",
    "/com/deepin/daemon/Audio",
    "/org/deepin/daemon/Audio",
];

/// 命中的服务变体（capability 记录实际使用的接口版本，doctor 输出）。
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum AudioServiceVariant {
    /// DDE25 主名 `org.deepin.dde.Audio1`。
    Dde25,
    /// DDE20 名（或 DDE25 别名）`com.deepin.daemon.Audio`。
    Dde20,
}

impl AudioServiceVariant {
    fn service_name(self) -> &'static str {
        match self {
            Self::Dde25 => DDE25_AUDIO,
            Self::Dde20 => DDE20_AUDIO,
        }
    }
}

fn dbus_err(context: &str, e: impl std::fmt::Display) -> AgentShellError {
    AgentShellError::DBus(format!("DdeAudio({context}): {e}"))
}

/// DDE 音频封装实例。
///
/// 装配即探测：[`connect`](Self::connect) 依次尝试两个服务名的全部根路径
/// 组合，首个「属性可读」的组合胜出并缓存；此后所有方法走该组合。
pub struct DdeAudio {
    conn: zbus::Connection,
    variant: AudioServiceVariant,
    root_path: OwnedObjectPath,
}

impl std::fmt::Debug for DdeAudio {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("DdeAudio")
            .field("variant", &self.variant)
            .field("root_path", &self.root_path.as_str())
            .finish()
    }
}

impl DdeAudio {
    /// 连接 session bus 并探测可用服务（小步：服务在 → 对象在 → 属性可读）。
    pub async fn connect() -> Result<Self> {
        let conn = zbus::Connection::session()
            .await
            .map_err(|e| AgentShellError::DBus(format!("session bus connect: {e}")))?;
        Self::with_connection(&conn).await
    }

    /// 基于既有 session-bus 连接探测构造。
    ///
    /// 探测顺序（§21.36.1）：先 `org.deepin.dde.Audio1`（DDE25 主），失败退
    /// `com.deepin.daemon.Audio`；每个名字遍历候选根路径，`DefaultSink`
    /// 属性可读即为命中（该属性两版一致存在）。
    pub async fn with_connection(conn: &zbus::Connection) -> Result<Self> {
        for (variant, service) in [
            (AudioServiceVariant::Dde25, DDE25_AUDIO),
            (AudioServiceVariant::Dde20, DDE20_AUDIO),
        ] {
            for path in ROOT_PATH_CANDIDATES {
                let Ok(proxy) =
                    zbus::Proxy::new(conn, service, path, "org.freedesktop.DBus.Properties").await
                else {
                    continue;
                };
                // 小步第三步：DefaultSink 可读 = 对象与方法面真实在位
                // （未注册的服务名在 Get 时直接返回 ServiceUnknown）。
                let probe: std::result::Result<zbus::zvariant::OwnedValue, _> = proxy
                    .call("Get", &("com.deepin.daemon.Audio", "DefaultSink"))
                    .await;
                if probe.is_ok() {
                    return Ok(Self {
                        conn: conn.clone(),
                        variant,
                        root_path: OwnedObjectPath::try_from(path)
                            .expect("static candidate is a valid object path"),
                    });
                }
            }
        }
        Err(AgentShellError::BackendUnavailable(
            "neither org.deepin.dde.Audio1 nor com.deepin.daemon.Audio reachable".into(),
        ))
    }

    /// 命中的服务变体（capability / doctor 报告）。
    pub fn variant(&self) -> AudioServiceVariant {
        self.variant
    }

    fn iface(&self) -> &'static str {
        // 接口名与服务名同域：DDE25 org.deepin.dde.Audio / com.deepin.daemon.Audio
        // 的接口名跟随服务基名（§21.36.1 方法集一致、命名空间不同）。
        match self.variant {
            AudioServiceVariant::Dde25 => "org.deepin.dde.Audio",
            AudioServiceVariant::Dde20 => "com.deepin.daemon.Audio",
        }
    }

    async fn root_proxy(&self) -> Result<zbus::Proxy<'_>> {
        zbus::Proxy::new(
            &self.conn,
            self.variant.service_name(),
            &self.root_path,
            self.iface(),
        )
        .await
        .map_err(|e| dbus_err("root proxy", e))
    }

    async fn sink_proxy<'s>(&self, sink_path: &'s OwnedObjectPath) -> Result<zbus::Proxy<'s>> {
        zbus::Proxy::new(
            &self.conn,
            self.variant.service_name(),
            sink_path,
            self.iface(),
        )
        .await
        .map_err(|e| dbus_err("sink proxy", e))
    }

    /// DefaultSink 属性 → Sink 子对象路径（两版一致的入口）。
    async fn default_sink_path(&self) -> Result<OwnedObjectPath> {
        let p = self.root_proxy().await?;
        let v: zbus::zvariant::OwnedValue = p
            .get_property("DefaultSink")
            .await
            .map_err(|e| dbus_err("get DefaultSink", e))?;
        use std::convert::TryFrom as _;
        let owned = OwnedObjectPath::try_from(v).map_err(|e| dbus_err("sink path decode", e))?;
        Ok(owned)
    }

    /// 默认 Sink 的音量/静音状态。
    async fn sink_state(&self) -> Result<(f64, bool)> {
        let path = self.default_sink_path().await?;
        let p = self.sink_proxy(&path).await?;
        let volume: f64 = p
            .get_property("Volume")
            .await
            .map_err(|e| dbus_err("Volume", e))?;
        let mute: bool = p
            .get_property("Mute")
            .await
            .map_err(|e| dbus_err("Mute", e))?;
        Ok((volume.clamp(0.0, 1.0), mute))
    }

    /// Sinks 属性 → 全部 Sink 子对象路径。
    async fn sink_paths(&self) -> Result<Vec<OwnedObjectPath>> {
        let p = self.root_proxy().await?;
        let v: Vec<OwnedObjectPath> = p
            .get_property("Sinks")
            .await
            .map_err(|e| dbus_err("Sinks", e))?;
        Ok(v)
    }

    /// Sink 子对象短名（路径尾段，如 `/org/deepin/dde/Audio1/Sink0` → `Sink0`）。
    fn sink_name(path: &OwnedObjectPath) -> String {
        path.rsplit('/').next().unwrap_or("").to_string()
    }
}

#[async_trait]
impl DesktopComponent for DdeAudio {
    fn name(&self) -> &'static str {
        "DdeAudio"
    }

    fn component_type(&self) -> ComponentType {
        ComponentType::AudioServer
    }

    fn is_available(&self) -> bool {
        // 构造即探测成功——同步视图恒真（失败者根本拿不到实例）。
        true
    }

    async fn health(&self) -> ComponentHealth {
        match self.sink_state().await {
            Ok(_) => ComponentHealth::Healthy,
            Err(_) => ComponentHealth::Degraded(format!(
                "service {:?} reachable but sink state unreadable",
                self.variant.service_name()
            )),
        }
    }
}

#[async_trait]
impl AudioServerComponent for DdeAudio {
    async fn get_volume(&self) -> Result<AudioState> {
        let (volume, muted) = self.sink_state().await?;
        Ok(AudioState {
            volume,
            muted,
            default_sink: Self::sink_name(&self.default_sink_path().await?),
        })
    }

    async fn set_volume(&self, volume: f64) -> Result<()> {
        // §21.36.1：控制统一走 Sink 子对象 SetVolume(d)，clamp 到 0.0-1.0。
        let path = self.default_sink_path().await?;
        let p = self.sink_proxy(&path).await?;
        let _: () = p
            .call("SetVolume", &(volume.clamp(0.0, 1.0),))
            .await
            .map_err(|e| dbus_err("SetVolume", e))?;
        Ok(())
    }

    async fn set_mute(&self, muted: bool) -> Result<()> {
        let path = self.default_sink_path().await?;
        let p = self.sink_proxy(&path).await?;
        let _: () = p
            .call("SetMute", &(muted,))
            .await
            .map_err(|e| dbus_err("SetMute", e))?;
        Ok(())
    }

    async fn list_audio_devices(&self) -> Result<Vec<AudioDevice>> {
        let paths = self.sink_paths().await?;
        let default_path = self.default_sink_path().await?;
        let mut devices = Vec::with_capacity(paths.len());
        for path in &paths {
            let Ok(p) = self.sink_proxy(path).await else {
                continue;
            };
            let volume: f64 = p.get_property("Volume").await.unwrap_or(0.0);
            let mute: bool = p.get_property("Mute").await.unwrap_or(false);
            let name = Self::sink_name(path);
            devices.push(AudioDevice {
                description: format!("DDE {}", name),
                is_default: *path == default_path,
                device_type: AudioDeviceType::Sink,
                volume: volume.clamp(0.0, 1.0),
                muted: mute,
                name,
            });
        }
        Ok(devices)
    }

    async fn set_default_sink(&self, sink_name: &str) -> Result<()> {
        // 在 Sinks 列表中按子对象短名匹配后调根对象 SetDefaultSink(o)；
        // 未匹配返回结构化错误（不猜路径）。
        let paths = self.sink_paths().await?;
        let target = paths
            .iter()
            .find(|p| Self::sink_name(p) == sink_name)
            .ok_or_else(|| {
                AgentShellError::DBus(format!("DdeAudio: unknown sink {sink_name:?}"))
            })?;
        let p = self.root_proxy().await?;
        let _: () = p
            .call("SetDefaultSink", &(target,))
            .await
            .map_err(|e| dbus_err("SetDefaultSink", e))?;
        Ok(())
    }
}

#[async_trait]
impl DeAudioWrapper for DdeAudio {
    async fn service_exists(&self) -> bool {
        // 已装配实例：以 DefaultSink 仍可读为「服务仍在」判据。
        match self.root_proxy().await {
            Ok(p) => p
                .get_property::<zbus::zvariant::OwnedValue>("DefaultSink")
                .await
                .is_ok(),
            Err(_) => false,
        }
    }

    fn service_name(&self) -> &'static str {
        self.variant.service_name()
    }

    fn inner(&self) -> &dyn AudioServerComponent {
        self
    }
}

impl DdeAudio {
    /// doctor 输出：实际命中的通道描述。
    pub fn channel_report(&self) -> String {
        match self.variant {
            AudioServiceVariant::Dde25 => {
                format!("{DDE25_AUDIO} @ {} (DDE25)", self.root_path.as_str())
            }
            AudioServiceVariant::Dde20 => {
                format!(
                    "{DDE20_AUDIO} @ {} (DDE20-compatible)",
                    self.root_path.as_str()
                )
            }
        }
    }
}
