//! PulseAudio 协议音频服务器（`org.pulseaudio.Server` D-Bus + pactl CLI 双通道）。
//!
//! design/13 §21.3 音频表「跨 DE」两行：
//!
//! - D-Bus：session bus `org.pulseaudio.Server`，路径 `/org/pulseaudio/server_lookup1`
//!   定位连接——该接口只暴露连接查找与少量查询；音量写入方法
//!   (`SetSinkVolumeIndex`) 需要核心协议连接，普通总线客户端不可达。
//! - CLI：`pactl`（`@DEFAULT_SINK@` 别名）是唯一跨 PipeWire-pulse / 原生
//!   PulseAudio 都稳定的写路径（design/13 §21.36.4「不假定单一通道」）。
//!
//! 因此实现为：**读走 D-Bus 属性探测 + pactl 查询，写走 pactl**。D-Bus 服务在
//! 与 §21.7 路由示例的降级语义一致。

use std::sync::atomic::{AtomicU8, Ordering};

use agent_shell_core::component::{
    AudioServerComponent, ComponentHealth, ComponentType, DesktopComponent,
};
use agent_shell_core::error::{AgentShellError, Result};
use agent_shell_core::services::{AudioDevice, AudioDeviceType, AudioState};
use async_trait::async_trait;
use tokio::process::Command;

const PULSE_DBUS_SERVICE: &str = "org.pulseaudio.Server";
const PULSE_DBUS_PATH: &str = "/org/pulseaudio/server_lookup1";

/// 通道探测缓存：bit0 = pactl 在位，bit1 = pulse D-Bus 服务已确认，
/// bit2 = pulse D-Bus 服务确认缺失（负结果同样缓存，避免每次方法调用
/// 重建 session-bus 连接重探——审查建议 #2）。
#[derive(Default)]
struct Channels(AtomicU8);

/// 小步探测 `org.pulseaudio.Server`（服务名 → server_lookup1 对象，§21.36.4）。
async fn probe_pulse_dbus() -> bool {
    let Ok(conn) = zbus::Connection::session().await else {
        return false;
    };
    let Ok(peer) = zbus::Proxy::new(
        &conn,
        PULSE_DBUS_SERVICE,
        PULSE_DBUS_PATH,
        "org.freedesktop.DBus.Peer",
    )
    .await
    else {
        return false;
    };
    // 无副作用：Peer.Ping 确认服务名可达且对象存在（未注册的服务名会在
    // 总线侧直接返回 ServiceUnknown 错误）。
    peer.call::<_, _, ()>("Ping", &()).await.is_ok()
}

impl Channels {
    const PACTL: u8 = 1;
    const DBUS: u8 = 2;
    const DBUS_MISSING: u8 = 4;

    fn load(&self) -> u8 {
        self.0.load(Ordering::Relaxed)
    }
}

/// PulseAudio 兼容音频服务器。
///
/// 覆盖原生 PulseAudio 与任何暴露 pulse-compatible socket 的栈
/// （PipeWire 的 pipewire-pulse 模块）。DE 封装路由中作公共回退实例。
pub struct PulseAudioAudioServer {
    channels: Channels,
}

impl Default for PulseAudioAudioServer {
    fn default() -> Self {
        Self::new()
    }
}

impl PulseAudioAudioServer {
    /// 构造（懒探测）。
    pub fn new() -> Self {
        Self {
            channels: Channels::default(),
        }
    }

    /// pactl CLI 是否存在。
    pub async fn pactl_available(&self) -> bool {
        if self.channels.load() & Channels::PACTL != 0 {
            return true;
        }
        let ok = which::which("pactl").is_ok();
        if ok {
            self.channels.0.fetch_or(Channels::PACTL, Ordering::Relaxed);
        }
        ok
    }

    /// session bus 上 `org.pulseaudio.Server` 是否可达（小步探测：服务名 →
    /// 对象路径，§21.36.4）。结果仅入 capability/health，不改变写通道。
    pub async fn dbus_service_exists(&self) -> bool {
        // 正/负结果都缓存（审查建议 #2）：服务不在位是稳定事实，重复建连
        // 探测纯属浪费；两位互斥，任一命中即短路。
        let cached = self.channels.0.load(Ordering::Relaxed);
        if cached & (Channels::DBUS | Channels::DBUS_MISSING) != 0 {
            return cached & Channels::DBUS != 0;
        }
        let exists = probe_pulse_dbus().await;
        self.channels.0.fetch_or(
            if exists {
                Channels::DBUS
            } else {
                Channels::DBUS_MISSING
            },
            Ordering::Relaxed,
        );
        exists
    }

    /// 实际命中的通道描述（doctor 输出接口通道，§21.36.4）。
    pub async fn channel_report(&self) -> String {
        let mut parts = Vec::new();
        if self.pactl_available().await {
            parts.push("pactl-cli");
        }
        if self.dbus_service_exists().await {
            parts.push("org.pulseaudio.Server");
        }
        if parts.is_empty() {
            "unavailable".into()
        } else {
            parts.join("+")
        }
    }

    fn missing_err() -> AgentShellError {
        AgentShellError::BackendUnavailable(
            "pactl not found (no PulseAudio/pipewire-pulse server)".into(),
        )
    }

    async fn run_pactl(&self, args: &[&str]) -> Result<String> {
        if !self.pactl_available().await {
            return Err(Self::missing_err());
        }
        if std::env::var_os("XDG_RUNTIME_DIR").is_none()
            && std::env::var_os("PULSE_RUNTIME_PATH").is_none()
        {
            return Err(AgentShellError::BackendUnavailable(
                "XDG_RUNTIME_DIR unset: cannot locate PulseAudio runtime socket".into(),
            ));
        }
        let out = Command::new("pactl")
            .args(args)
            .output()
            .await
            .map_err(|e| AgentShellError::DBus(format!("pactl spawn: {e}")))?;
        if !out.status.success() {
            return Err(AgentShellError::DBus(format!(
                "pactl {} failed: {}",
                args.first().unwrap_or(&""),
                String::from_utf8_lossy(&out.stderr).trim()
            )));
        }
        Ok(String::from_utf8_lossy(&out.stdout).to_string())
    }

    /// 解析默认 sink 名：`Default Sink: alsa_output.pci-0000_00_1f.3.analog-stereo`。
    fn parse_default_sink(info: &str) -> Option<String> {
        for line in info.lines() {
            if let Some((k, v)) = line.split_once(':') {
                if k.trim() == "Default Sink" {
                    let v = v.trim();
                    if !v.is_empty() {
                        return Some(v.to_string());
                    }
                }
            }
        }
        None
    }

    /// 解析 pactl 设备行：
    /// `Sink #48` / `Name: ...` / `Description: ...` / `Mute: no` /
    /// `Volume: front-left: 49152 /  75% / -7.50 dB` / `Default Sample Specification`.
    fn parse_devices(output: &str, device_type: AudioDeviceType) -> Vec<AudioDevice> {
        let header = match device_type {
            AudioDeviceType::Sink => "Sink #",
            AudioDeviceType::Source => "Source #",
        };
        let mut devices = Vec::new();
        let mut current: Option<AudioDevice> = None;

        macro_rules! push_current {
            () => {
                if let Some(mut d) = current.take() {
                    d.volume = d.volume.clamp(0.0, 1.0);
                    devices.push(d);
                }
            };
        }

        for line in output.lines() {
            let trimmed = line.trim();
            if trimmed.starts_with(header) {
                push_current!();
                current = Some(AudioDevice {
                    name: String::new(),
                    description: String::new(),
                    volume: 0.0,
                    muted: false,
                    is_default: false,
                    device_type,
                });
                continue;
            }
            let Some(d) = current.as_mut() else {
                continue;
            };
            if let Some(rest) = trimmed.strip_prefix("Volume:") {
                // 形如 "front-left: 49152 /  75% / -7.50 dB, ..."：取第一个百分比段。
                if let Some(pct) = rest.split('/').nth(1) {
                    if let Some(v) = pct.trim().strip_suffix('%') {
                        if let Ok(n) = v.parse::<f64>() {
                            d.volume = n / 100.0;
                        }
                    }
                }
            } else if let Some((k, v)) = trimmed.split_once(": ") {
                let v = v.trim();
                match k.trim_end_matches(':') {
                    "Name" => d.name = v.to_string(),
                    "Description" => d.description = v.to_string(),
                    "Mute" => d.muted = v.eq_ignore_ascii_case("yes"),
                    _ => {}
                }
            } else if trimmed.starts_with('*') && trimmed.contains("index:") {
                // `* index: 0` 标记默认设备（部分版本输出）。
                d.is_default = true;
            }
        }
        push_current!();
        devices.retain(|d| !d.name.is_empty());
        devices
    }
}

#[async_trait]
impl DesktopComponent for PulseAudioAudioServer {
    fn name(&self) -> &'static str {
        "PulseAudioAudioServer"
    }

    fn component_type(&self) -> ComponentType {
        ComponentType::AudioServer
    }

    fn is_available(&self) -> bool {
        self.channels.load() & Channels::PACTL != 0
    }

    async fn health(&self) -> ComponentHealth {
        if self.pactl_available().await {
            ComponentHealth::Healthy
        } else {
            ComponentHealth::Unavailable
        }
    }
}

#[async_trait]
impl AudioServerComponent for PulseAudioAudioServer {
    async fn get_volume(&self) -> Result<AudioState> {
        let info = self.run_pactl(&["info"]).await?;
        let default_sink = Self::parse_default_sink(&info).unwrap_or_default();
        let devices = self.run_pactl(&["list", "sinks"]).await?;
        let parsed = Self::parse_devices(&devices, AudioDeviceType::Sink);
        let default_idx = info
            .lines()
            .find_map(|l| l.strip_prefix("Default Sink: "))
            .and_then(|name| {
                // 从 list sinks 中找对应块的首个 Volume 百分比与 Mute。
                parsed.iter().find(|d| d.name == name.trim())
            });
        Ok(match default_idx {
            Some(d) => AudioState {
                volume: d.volume,
                muted: d.muted,
                default_sink: d.name.clone(),
            },
            None => AudioState {
                volume: 0.0,
                muted: false,
                default_sink,
            },
        })
    }

    async fn set_volume(&self, volume: f64) -> Result<()> {
        let pct = format!("{}%", (volume.clamp(0.0, 1.0) * 100.0).round() as i64);
        self.run_pactl(&["set-sink-volume", "@DEFAULT_SINK@", &pct])
            .await?;
        Ok(())
    }

    async fn set_mute(&self, muted: bool) -> Result<()> {
        self.run_pactl(&[
            "set-sink-mute",
            "@DEFAULT_SINK@",
            if muted { "1" } else { "0" },
        ])
        .await?;
        Ok(())
    }

    async fn list_audio_devices(&self) -> Result<Vec<AudioDevice>> {
        let sinks = self.run_pactl(&["list", "sinks"]).await?;
        let sources = self.run_pactl(&["list", "sources"]).await?;
        let mut devices = Self::parse_devices(&sinks, AudioDeviceType::Sink);
        devices.extend(Self::parse_devices(&sources, AudioDeviceType::Source));
        Ok(devices)
    }

    async fn set_default_sink(&self, sink_name: &str) -> Result<()> {
        if sink_name.is_empty() || sink_name.contains(char::is_whitespace) {
            return Err(AgentShellError::DBus(format!(
                "invalid sink name: {sink_name:?}"
            )));
        }
        self.run_pactl(&["set-default-sink", sink_name]).await?;
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_default_sink_from_info() {
        let info = "\
Server String: /run/user/1000/pulse/native
Default Sink: alsa_output.pci-0000_00_1f.3.analog-stereo
Default Source: alsa_input.pci-0000_00_1f.3.analog-stereo";
        assert_eq!(
            PulseAudioAudioServer::parse_default_sink(info),
            Some("alsa_output.pci-0000_00_1f.3.analog-stereo".into())
        );
    }

    #[test]
    fn parses_sink_block_fields() {
        let out = "\
Sink #48
	State: RUNNING
	Name: alsa_output.pci-0000_00_1f.3.analog-stereo
	Description: Built-in Audio Analog Stereo
	Mute: no
	Volume: front-left: 49152 /  75% / -7.50 dB,   front-right: 49152 /  75%
	    balance 0.00
	Default Sample Specification: s16le 2ch 44100Hz

Sink #49
	State: SUSPENDED
	Name: auto_null
	Description: Dummy Output
	Mute: yes
	Volume: front-left: 65536 / 100%";
        let devices = PulseAudioAudioServer::parse_devices(out, AudioDeviceType::Sink);
        assert_eq!(devices.len(), 2);
        assert_eq!(
            devices[0].name,
            "alsa_output.pci-0000_00_1f.3.analog-stereo"
        );
        assert_eq!(devices[0].description, "Built-in Audio Analog Stereo");
        assert!(!devices[0].muted);
        assert!((devices[0].volume - 0.75).abs() < 1e-9);
        assert_eq!(devices[1].name, "auto_null");
        assert!(devices[1].muted);
        assert!((devices[1].volume - 1.0).abs() < 1e-9);
    }

    #[test]
    fn empty_names_are_dropped() {
        let out = "Sink #1\n\tState: IDLE\n";
        assert!(PulseAudioAudioServer::parse_devices(out, AudioDeviceType::Sink).is_empty());
    }

    #[tokio::test]
    async fn methods_fail_structurally_without_pactl() {
        unsafe {
            std::env::set_var("PATH", "/nonexistent-agent-shell-test");
        }
        let srv = PulseAudioAudioServer::new();
        let err = srv.get_volume().await.unwrap_err();
        assert!(
            matches!(err, AgentShellError::BackendUnavailable(_)),
            "{err:?}"
        );
        let err = srv.set_mute(true).await.unwrap_err();
        assert!(
            matches!(err, AgentShellError::BackendUnavailable(_)),
            "{err:?}"
        );
        unsafe {
            std::env::remove_var("PATH");
        }
    }
}
