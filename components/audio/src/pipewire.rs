//! PipeWire 音频服务器（wpctl CLI 通道）。
//!
//! PipeWire 自身无公共 D-Bus 控制接口；会话管理（音量/默认设备）由 WirePlumber
//! 承担，其稳定控制面是 `wpctl` CLI（design/13 §21.3「跨 DE CLI」行）。
//!
//! 通道选择：`get_volume` 等查询走 `wpctl get-volume` / `wpctl status`；
//! 写操作走 `wpctl set-volume @DEFAULT_AUDIO_SINK@ <v>`。全部命令在
//! `PIPEWIRE_RUNTIME_DIR`/XDG 运行时缺失时返回结构化 [`BackendUnavailable`]，
//! 不 panic（§21 验收：降级明确）。
//!
//! [`BackendUnavailable`]: agent_shell_core::error::AgentShellError::BackendUnavailable

use std::sync::atomic::{AtomicU8, Ordering};

use agent_shell_core::component::{
    AudioServerComponent, ComponentHealth, ComponentType, DesktopComponent,
};
use async_trait::async_trait;
use tokio::process::Command;

use agent_shell_core::error::{AgentShellError, Result};
use agent_shell_core::services::{AudioDevice, AudioDeviceType, AudioState};

/// 默认 sink 的 wpctl 别名（WirePlumber 内建 id 字面量，非节点昵称）。
pub const DEFAULT_SINK: &str = "@DEFAULT_AUDIO_SINK@";
/// 默认 source 的 wpctl 别名。
pub const DEFAULT_SOURCE: &str = "@DEFAULT_AUDIO_SOURCE@";

/// 探测结论缓存（构造时一次，避免每次调用 fork 探测进程）。
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
#[repr(u8)]
enum Probe {
    Unknown = 0,
    Available = 1,
    Missing = 2,
}

impl Probe {
    fn from_u8(v: u8) -> Self {
        match v {
            1 => Self::Available,
            2 => Self::Missing,
            _ => Self::Unknown,
        }
    }

    fn is_available(self) -> bool {
        self == Self::Available
    }
}

/// PipeWire/WirePlumber 音频服务器（wpctl CLI 封装）。
///
/// 不直接实现 trait 方法外的 D-Bus；GNOME/KDE/DDE Wayland 会话默认栈即
/// PipeWire + WirePlumber，故本类型是各 backend 装配清单的音频首选实例。
pub struct PipeWireAudioServer {
    /// wpctl 可用性缓存（0=未探测 1=可用 2=不可用）。原子而非 Mutex：
    /// 探测是幂等只读操作，竞态最多重复一次探测，无需串行化。
    probe: AtomicU8,
}

impl Default for PipeWireAudioServer {
    fn default() -> Self {
        Self::new()
    }
}

impl PipeWireAudioServer {
    /// 构造（不探测——首次调用方法时懒探测，构造保持同步廉价）。
    pub fn new() -> Self {
        Self {
            probe: AtomicU8::new(Probe::Unknown as u8),
        }
    }

    /// wpctl 是否存在（结果缓存）。
    pub async fn available(&self) -> bool {
        let cached = Probe::from_u8(self.probe.load(Ordering::Relaxed));
        if cached != Probe::Unknown {
            return cached.is_available();
        }
        let ok = which::which("wpctl").is_ok();
        self.probe.store(
            if ok { Probe::Available } else { Probe::Missing } as u8,
            Ordering::Relaxed,
        );
        ok
    }

    fn missing_err() -> AgentShellError {
        AgentShellError::BackendUnavailable(
            "wpctl not found (PipeWire/WirePlumber stack absent?)".into(),
        )
    }

    async fn run_wpctl(&self, args: &[&str]) -> Result<String> {
        if !self.available().await {
            return Err(Self::missing_err());
        }
        // wpctl 需要会话运行时目录定位 PipeWire socket；缺失时命令必然失败，
        // 提前给出结构化错误而非解析 stderr。
        if std::env::var_os("XDG_RUNTIME_DIR").is_none()
            && std::env::var_os("PIPEWIRE_RUNTIME_DIR").is_none()
        {
            return Err(AgentShellError::BackendUnavailable(
                "XDG_RUNTIME_DIR unset: cannot locate PipeWire runtime socket".into(),
            ));
        }
        let out = Command::new("wpctl")
            .args(args)
            .output()
            .await
            .map_err(|e| AgentShellError::DBus(format!("wpctl spawn: {e}")))?;
        if !out.status.success() {
            return Err(AgentShellError::DBus(format!(
                "wpctl {} failed: {}",
                args.first().unwrap_or(&""),
                String::from_utf8_lossy(&out.stderr).trim()
            )));
        }
        Ok(String::from_utf8_lossy(&out.stdout).trim().to_string())
    }

    /// 解析 `wpctl get-volume` 输出："Volume: 0.53" 或 "Volume: 0.53 [MUTED]"/"[MUTED]" 单独成行。
    fn parse_get_volume(output: &str) -> Option<(f64, bool)> {
        let mut volume = None;
        let mut muted = false;
        for token in output.split_whitespace() {
            match token.parse::<f64>() {
                // 统一钳到 AudioState 契约量程（wpctl 可报 >1.0 过增益，读数按上限截断）。
                Ok(v) => volume = Some(v.clamp(0.0, 1.0)),
                Err(_) => {
                    let t = token.trim_matches(|c| c == '[' || c == ']');
                    if t.eq_ignore_ascii_case("MUTED") {
                        muted = true;
                    }
                }
            }
        }
        volume.map(|v| (v, muted))
    }

    /// 从 `wpctl status` 抽取默认 sink 名（`*` 标记行 → 括号内昵称前的节点描述）。
    ///
    /// status 树形文本脆弱，仅用于补全 `default_sink` 展示名；解析失败返回空串
    /// 由调用方回退为字面别名，不作为错误路径。
    fn parse_default_sink(status: &str) -> String {
        let mut in_sinks = false;
        for line in status.lines() {
            let indent_marks_sink = line.contains("Sinks:");
            if indent_marks_sink {
                in_sinks = true;
                continue;
            }
            if in_sinks {
                let trimmed = line.trim_start();
                // 进入下一区块（Sources/…）或缩浅到区块标题层级即结束。
                if trimmed.starts_with("Sources:")
                    || (!line.starts_with("        │")
                        && !trimmed.starts_with('*')
                        && trimmed.contains(':')
                        && !trimmed.contains("│"))
                {
                    break;
                }
                if let Some(rest) = trimmed.strip_prefix('*') {
                    // 形如 "* 42. 名称 [vol: 0.50]"；名称取首个 '.' 后、'[' 前的段。
                    let rest = rest.trim_start_matches([' ', '.']);
                    let name = rest
                        .split_once('.')
                        .map(|(_, tail)| tail)
                        .unwrap_or(rest)
                        .split('[')
                        .next()
                        .unwrap_or("")
                        .trim();
                    if !name.is_empty() {
                        return name.to_string();
                    }
                }
            }
        }
        String::new()
    }
}

/// 解析 `wpctl status` 条目尾部的状态括号段（`[vol: 0.52] [MUTED]` 等）。
///
/// 输入为首个 `[` 之后的原文（可含多个 `]` 分隔的标签）。返回
/// `(volume, muted)`：`vol: <f64>` 钳到 0.0–1.0；`MUTED` 大小写不敏感；
/// 无标签/不可解析时落 `(0.0, false)` 缺省。
fn parse_status_tags(bracket: &str) -> (f64, bool) {
    let mut volume = 0.0;
    let mut muted = false;
    for tag in bracket.split(']') {
        // 片段可能残留「名称 + 前导 '['」（如 "Family 17h [vol: 1.50"）——
        // 标签名取最后一个 '[' 之后的段，再剥空格。
        let tag = tag.trim_end();
        let tag = tag.rsplit_once('[').map(|(_, t)| t).unwrap_or(tag).trim();
        if let Some(v) = tag.strip_prefix("vol:") {
            volume = v.trim().parse::<f64>().unwrap_or(0.0).clamp(0.0, 1.0);
        }
        if tag.eq_ignore_ascii_case("MUTED") {
            muted = true;
        }
    }
    (volume, muted)
}

#[async_trait]
impl DesktopComponent for PipeWireAudioServer {
    fn name(&self) -> &'static str {
        "PipeWireAudioServer"
    }

    fn component_type(&self) -> ComponentType {
        ComponentType::AudioServer
    }

    fn is_available(&self) -> bool {
        // 同步视图：懒探测前的保守值。doctor 走异步 health()。
        Probe::from_u8(self.probe.load(Ordering::Relaxed)).is_available()
    }

    async fn health(&self) -> ComponentHealth {
        if self.available().await {
            ComponentHealth::Healthy
        } else {
            ComponentHealth::Unavailable
        }
    }
}

#[async_trait]
impl AudioServerComponent for PipeWireAudioServer {
    async fn get_volume(&self) -> Result<AudioState> {
        let vol_out = self.run_wpctl(&["get-volume", DEFAULT_SINK]).await?;
        let (volume, muted) = Self::parse_get_volume(&vol_out).ok_or_else(|| {
            AgentShellError::DBus(format!("wpctl get-volume unparseable: {vol_out}"))
        })?;
        let default_sink = match self.run_wpctl(&["status"]).await {
            Ok(status) => {
                let parsed = Self::parse_default_sink(&status);
                if parsed.is_empty() {
                    DEFAULT_SINK.to_string()
                } else {
                    parsed
                }
            }
            // status 仅用于展示名，失败不拖垮查询主路径。
            Err(_) => DEFAULT_SINK.to_string(),
        };
        Ok(AudioState {
            volume,
            muted,
            default_sink,
        })
    }

    async fn set_volume(&self, volume: f64) -> Result<()> {
        let v = volume.clamp(0.0, 1.0);
        self.run_wpctl(&["set-volume", DEFAULT_SINK, &format!("{v}")])
            .await?;
        Ok(())
    }

    async fn set_mute(&self, muted: bool) -> Result<()> {
        self.run_wpctl(&["set-mute", DEFAULT_SINK, if muted { "1" } else { "0" }])
            .await?;
        Ok(())
    }

    async fn list_audio_devices(&self) -> Result<Vec<AudioDevice>> {
        // wpctl 无机器可读设备列表；status 树是唯一来源。Sink 区块逐行解析，
        // Source 区块同理。解析不出结构时返回空表而非错误（CLI-only 环境常见）。
        let status = self.run_wpctl(&["status"]).await?;
        let mut devices = Vec::new();
        let mut section: Option<AudioDeviceType> = None;
        for line in status.lines() {
            let trimmed = line.trim_start();
            if trimmed.starts_with("Sinks:") {
                section = Some(AudioDeviceType::Sink);
                continue;
            }
            if trimmed.starts_with("Sources:") {
                section = Some(AudioDeviceType::Source);
                continue;
            }
            if let Some(ty) = section {
                // 区块结束：出现非条目行（新顶层区块或空行后缩进消失）。
                if trimmed.is_empty()
                    || (!trimmed.contains("│")
                        && !trimmed.starts_with('*')
                        && !trimmed.chars().next().is_some_and(|c| c.is_ascii_digit()))
                {
                    section = None;
                    continue;
                }
                let entry = trimmed.trim_start_matches('*').trim_start();
                let Some((id_rest, meta)) = entry.split_once('.') else {
                    continue;
                };
                let _id: u32 = id_rest.trim().parse().unwrap_or(0);
                // meta 尾部带状态括号：[vol: 0.52] / [MUTED] / [alsa] 等。
                let (name, bracket) = meta.split_once('[').unwrap_or((meta.trim(), ""));
                let name = name.trim();
                if name.is_empty() {
                    continue;
                }
                // 从括号段解析真实音量/静音，缺省（如纯 [alsa] 标签）才落 0/false。
                let (volume, muted) = parse_status_tags(bracket);
                let is_default = trimmed.trim_start().starts_with('*');
                devices.push(AudioDevice {
                    name: name.to_string(),
                    description: name.to_string(),
                    volume,
                    muted,
                    is_default,
                    device_type: ty,
                });
            }
        }
        Ok(devices)
    }

    async fn set_default_sink(&self, sink_name: &str) -> Result<()> {
        if sink_name.is_empty() {
            return Err(AgentShellError::DBus("empty sink name".into()));
        }
        self.run_wpctl(&["set-default", sink_name]).await?;
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_plain_volume_output() {
        let (v, m) = PipeWireAudioServer::parse_get_volume("Volume: 0.53").unwrap();
        assert_eq!(v, 0.53);
        assert!(!m);
    }

    #[test]
    fn parses_muted_volume_output() {
        let (v, m) = PipeWireAudioServer::parse_get_volume("Volume: 0.00 [MUTED]").unwrap();
        assert_eq!(v, 0.0);
        assert!(m);
    }

    #[test]
    fn rejects_unparseable_output() {
        assert!(PipeWireAudioServer::parse_get_volume("error: no such object").is_none());
    }

    #[test]
    fn extracts_default_sink_from_status_tree() {
        let status = "\
Info on utility objects:
Set default devices via e.g. `wpctl set-default 42`.

Audio
 ├─ Devices:
 │      47. Built-in Audio                    [alsa]
 │
 └─ Sinks:
         * 42. Family 17h/19h HD Audio Controller [vol: 0.52]
           43. Monitor of Family 17h             [vol: 1.00]
 │
 ├─ Sources:
 │  ├─ Streams:";
        assert_eq!(
            PipeWireAudioServer::parse_default_sink(status),
            "Family 17h/19h HD Audio Controller"
        );
    }

    #[test]
    fn empty_status_yields_empty_default() {
        assert_eq!(PipeWireAudioServer::parse_default_sink("garbage"), "");
    }

    // ───────────── parse_status_tags（审查要求的四类输入） ─────────────

    #[test]
    fn status_tags_vol_tag() {
        // [vol: 0.52]：真实音量读出，非静音。
        assert_eq!(parse_status_tags("[vol: 0.52]"), (0.52, false));
    }

    #[test]
    fn status_tags_muted_only() {
        // 仅 [MUTED]：无 vol 标签 → 音量缺省 0.0，静音为真。
        assert_eq!(parse_status_tags("[MUTED]"), (0.0, true));
    }

    #[test]
    fn status_tags_no_tags() {
        // 无标签 / 纯设备类标签（[alsa]）：均落缺省 (0.0, false)。
        assert_eq!(parse_status_tags(""), (0.0, false));
        assert_eq!(parse_status_tags("[alsa]"), (0.0, false));
    }

    #[test]
    fn status_tags_combined_and_out_of_range() {
        // 组合标签 + 过增益钳制：>1.0 读数截到 1.0。
        assert_eq!(
            parse_status_tags("Family 17h [vol: 1.50] [MUTED]"),
            (1.0, true)
        );
    }

    #[test]
    fn status_entry_with_dotted_name() {
        // 名称含点号：split_once('.') 取首个 '.'，id 段为纯数字，
        // 名称 "17h/19h v2.1 Controller" 的点不参与 id 切分。
        let line = "* 42. 17h/19h v2.1 Controller [vol: 0.30]";
        let entry = line.trim_start_matches('*').trim_start();
        let (id_rest, meta) = entry.split_once('.').unwrap();
        assert!(id_rest.trim().parse::<u32>().is_ok());
        let (name, bracket) = meta.split_once('[').unwrap();
        assert_eq!(name.trim(), "17h/19h v2.1 Controller");
        assert_eq!(parse_status_tags(bracket), (0.30, false));
    }

    #[tokio::test]
    async fn methods_fail_structurally_without_wpctl() {
        // PATH 注入假目录屏蔽真实 wpctl：所有方法必须返回 BackendUnavailable 而非 panic。
        unsafe {
            std::env::set_var("PATH", "/nonexistent-agent-shell-test");
        }
        let srv = PipeWireAudioServer::new();
        let err = srv.get_volume().await.unwrap_err();
        assert!(
            matches!(err, AgentShellError::BackendUnavailable(_)),
            "{err:?}"
        );
        let err = srv.set_volume(0.4).await.unwrap_err();
        assert!(
            matches!(err, AgentShellError::BackendUnavailable(_)),
            "{err:?}"
        );
        unsafe {
            std::env::remove_var("PATH");
        }
    }
}
