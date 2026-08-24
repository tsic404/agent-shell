//! DE 封装优先路由（design/13 §21.4 `DePriorityRouter`）。
//!
//! 每个能力调用先探测 DE 专有 D-Bus 服务（`org.deepin.dde.*` 等），命中用
//! DE 封装实现，未命中的回退公共组件实例。路由落点是 backend 的
//! `services.rs` / `dde_api.rs`；本 crate 提供可独立装配的路由器与通道记录，
//! DDE 封装本体在 `backends/dde`（此处经 trait 对象注入，避免组件层反向
//! 依赖具体 backend crate）。

use async_trait::async_trait;

use agent_shell_core::component::{
    AudioServerComponent, ComponentHealth, ComponentType, DesktopComponent,
};
use agent_shell_core::error::{AgentShellError, Result};
use agent_shell_core::registry::BackendKind;
use agent_shell_core::services::{AudioDevice, AudioState};

/// 音频能力实际命中的通道（doctor 输出接口通道，§21.36.4）。
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum AudioChannel {
    /// DE 专有封装（DDE25 `org.deepin.dde.Audio1` / DDE20 `com.deepin.daemon.Audio`
    /// 等由 [`DeWrapper`] 实现自报）。
    DeWrapper(&'static str),
    /// 公共 PipeWire/WirePlumber 通道（wpctl）。
    PipeWireCli,
    /// 公共 PulseAudio 兼容通道（pactl，含 pipewire-pulse）。
    PulseAudioCli,
}

impl AudioChannel {
    /// doctor 报告字符串。
    pub fn as_str(self) -> &'static str {
        match self {
            AudioChannel::DeWrapper(_) => "de-wrapper",
            AudioChannel::PipeWireCli => "pipewire-cli",
            AudioChannel::PulseAudioCli => "pulseaudio-cli",
        }
    }
}

/// DE 专有音频封装的回调接口（由各 backend 的 dde_api/services 实现）。
///
/// 与 [`AudioServerComponent`] 同形；单独 trait 以便 router 持有
/// 「探测 + 实现」对而无需知道具体 DE 类型。
#[async_trait]
pub trait DeAudioWrapper: Send + Sync {
    /// 该 DE 服务当前是否可达（小步探测：服务名 → 对象 → 方法签名按实现内部分步）。
    async fn service_exists(&self) -> bool;

    /// 实际使用的服务名（capability 记录，如 `org.deepin.dde.Audio1`）。
    fn service_name(&self) -> &'static str;

    /// 底层实现（委托给 backend 的封装类型）。
    fn inner(&self) -> &dyn AudioServerComponent;
}

/// DE 封装优先音频路由器。
///
/// 自身实现 [`AudioServerComponent`]，可直接装入
/// [`ComponentRegistry::audio`](agent_shell_core::registry::ComponentRegistry)
/// slot；每次方法调用按 backend 探测顺序分发并记录命中通道。
pub struct DePriorityRouter {
    de: BackendKind,
    fallback: Box<dyn AudioServerComponent>,
    de_wrapper: Option<Box<dyn DeAudioWrapper>>,
    last_channel: std::sync::atomic::AtomicU8,
}

impl std::fmt::Debug for DePriorityRouter {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("DePriorityRouter")
            .field("de", &self.de)
            .field("has_de_wrapper", &self.de_wrapper.is_some())
            .finish()
    }
}

// AtomicU8 通道编码：0=pipewire-cli 1=pulseaudio-cli。
const CH_PIPEWIRE: u8 = 0;
const CH_PULSEAUDIO: u8 = 1;

impl DePriorityRouter {
    /// 构造：backend 种类 + 公共降级实例（显式声明其 CLI 通道）+ 可选 DE 封装。
    ///
    /// `fallback_channel` 由调用方按装配事实给出（wpctl→PipeWireCli，
    /// pactl→PulseAudioCli），路由器不做字符串反推。GNOME 特例（design/02
    /// §3.5）：GNOME 无 DE 层音量 D-Bus 接口——传 `de_wrapper: None` 即全部
    /// 流量走公共组件，与装配矩阵一致。
    pub fn new(
        de: BackendKind,
        fallback: Box<dyn AudioServerComponent>,
        fallback_channel: AudioChannel,
        de_wrapper: Option<Box<dyn DeAudioWrapper>>,
    ) -> Self {
        let initial = match fallback_channel {
            AudioChannel::PulseAudioCli => CH_PULSEAUDIO,
            _ => CH_PIPEWIRE,
        };
        Self {
            de,
            fallback,
            de_wrapper,
            last_channel: std::sync::atomic::AtomicU8::new(initial),
        }
    }

    /// 当前 backend 种类。
    pub fn backend(&self) -> BackendKind {
        self.de
    }

    /// 最近一次成功调用实际使用的通道（doctor 输出）。
    pub fn last_channel(&self) -> AudioChannel {
        let v = self.last_channel.load(std::sync::atomic::Ordering::Relaxed);
        match v {
            CH_PIPEWIRE => AudioChannel::PipeWireCli,
            CH_PULSEAUDIO => AudioChannel::PulseAudioCli,
            _ => AudioChannel::DeWrapper(
                self.de_wrapper
                    .as_ref()
                    .map(|w| w.service_name())
                    .unwrap_or("unknown-de"),
            ),
        }
    }

    /// 路由决策核心：返回本次应服务的实现视图。
    ///
    /// - 有 DE 封装且服务在位 → DE 封装（记通道）
    /// - 否则 → 公共降级实例（按其类型记 wpctl/pactl 通道）
    async fn resolve(&self) -> (&dyn AudioServerComponent, bool) {
        if let Some(w) = &self.de_wrapper {
            if w.service_exists().await {
                self.last_channel
                    .store(2, std::sync::atomic::Ordering::Relaxed);
                return (w.inner(), true);
            }
        }
        // 公共回退：通道已在构造时显式声明。
        (self.fallback.as_ref(), false)
    }
}

#[async_trait]
impl DesktopComponent for DePriorityRouter {
    fn name(&self) -> &'static str {
        "DePriorityRouter"
    }

    fn component_type(&self) -> ComponentType {
        ComponentType::AudioServer
    }

    fn is_available(&self) -> bool {
        self.fallback.is_available() || self.de_wrapper.is_some()
    }

    async fn health(&self) -> ComponentHealth {
        // DE 封装在位 → Healthy；否则继承公共实例健康度（Unavailable 原样透出）。
        if let Some(w) = &self.de_wrapper {
            if w.service_exists().await {
                return ComponentHealth::Healthy;
            }
        }
        self.fallback.health().await
    }
}

#[async_trait]
impl AudioServerComponent for DePriorityRouter {
    async fn get_volume(&self) -> Result<AudioState> {
        let (impl_, _) = self.resolve().await;
        impl_.get_volume().await
    }

    async fn set_volume(&self, volume: f64) -> Result<()> {
        let (impl_, _) = self.resolve().await;
        impl_.set_volume(volume).await
    }

    async fn set_mute(&self, muted: bool) -> Result<()> {
        let (impl_, _) = self.resolve().await;
        impl_.set_mute(muted).await
    }

    async fn list_audio_devices(&self) -> Result<Vec<AudioDevice>> {
        let (impl_, _) = self.resolve().await;
        impl_.list_audio_devices().await
    }

    async fn set_default_sink(&self, sink_name: &str) -> Result<()> {
        let (impl_, _) = self.resolve().await;
        impl_.set_default_sink(sink_name).await
    }
}

/// 便捷构造：按 backend 组装默认路由（公共栈探测链 wpctl→pactl）。
///
/// 返回 `None` 表示两个公共服务器都不可用且无 DE 封装（TTY/无音频栈），
/// 装配清单应将 audio slot 置空而非塞入必失败的实例。
pub async fn assemble_audio_router(de: BackendKind) -> Result<Option<DePriorityRouter>> {
    // 探测顺序 design/02 §4.2：pipewire → pulseaudio → None。
    if which::which("wpctl").is_ok() {
        return Ok(Some(DePriorityRouter::new(
            de,
            Box::new(crate::pipewire::PipeWireAudioServer::new()),
            AudioChannel::PipeWireCli,
            None,
        )));
    }
    if which::which("pactl").is_ok() {
        return Ok(Some(DePriorityRouter::new(
            de,
            Box::new(crate::pulseaudio::PulseAudioAudioServer::new()),
            AudioChannel::PulseAudioCli,
            None,
        )));
    }
    Ok(None)
}

/// 错误别名再导出（保持模块内 use 简洁）。
pub type AudioResult<T> = std::result::Result<T, AgentShellError>;

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::atomic::{AtomicBool, AtomicUsize};

    /// 计数假实现：记录每次方法被谁调用。
    struct FakeServer {
        tag: &'static str,
        calls: AtomicUsize,
        available: bool,
    }

    impl FakeServer {
        fn new(tag: &'static str, available: bool) -> Self {
            Self {
                tag,
                calls: AtomicUsize::new(0),
                available,
            }
        }
    }

    #[async_trait]
    impl DesktopComponent for FakeServer {
        fn name(&self) -> &'static str {
            self.tag
        }

        fn component_type(&self) -> ComponentType {
            ComponentType::AudioServer
        }

        fn is_available(&self) -> bool {
            self.available
        }

        async fn health(&self) -> ComponentHealth {
            if self.available {
                ComponentHealth::Healthy
            } else {
                ComponentHealth::Unavailable
            }
        }
    }

    #[async_trait]
    impl AudioServerComponent for FakeServer {
        async fn get_volume(&self) -> Result<AudioState> {
            self.calls
                .fetch_add(1, std::sync::atomic::Ordering::Relaxed);
            Ok(AudioState {
                volume: 0.5,
                muted: false,
                default_sink: format!("{}-sink", self.tag),
            })
        }

        async fn set_volume(&self, _: f64) -> Result<()> {
            Ok(())
        }

        async fn set_mute(&self, _: bool) -> Result<()> {
            Ok(())
        }

        async fn list_audio_devices(&self) -> Result<Vec<AudioDevice>> {
            Ok(vec![])
        }

        async fn set_default_sink(&self, _: &str) -> Result<()> {
            Ok(())
        }
    }

    struct FakeDeWrapper {
        exists: AtomicBool,
        server: FakeServer,
    }

    impl FakeDeWrapper {
        fn new(exists: bool) -> Self {
            Self {
                exists: AtomicBool::new(exists),
                server: FakeServer::new("dde-audio", true),
            }
        }
    }

    #[async_trait]
    impl DeAudioWrapper for FakeDeWrapper {
        async fn service_exists(&self) -> bool {
            self.exists.load(std::sync::atomic::Ordering::Relaxed)
        }

        fn service_name(&self) -> &'static str {
            "org.deepin.dde.Audio1"
        }

        fn inner(&self) -> &dyn AudioServerComponent {
            &self.server
        }
    }

    fn fallback() -> Box<FakeServer> {
        Box::new(FakeServer::new("PipeWireAudioServer", true))
    }

    #[tokio::test]
    async fn de_wrapper_hit_routes_to_wrapper() {
        let router = DePriorityRouter::new(
            BackendKind::Dde,
            fallback(),
            AudioChannel::PipeWireCli,
            Some(Box::new(FakeDeWrapper::new(true))),
        );
        let state = router.get_volume().await.unwrap();
        assert_eq!(state.default_sink, "dde-audio-sink");
        assert_eq!(
            router.last_channel(),
            AudioChannel::DeWrapper("org.deepin.dde.Audio1")
        );
    }

    #[tokio::test]
    async fn missing_de_service_falls_back_to_common() {
        let router = DePriorityRouter::new(
            BackendKind::Dde,
            fallback(),
            AudioChannel::PipeWireCli,
            Some(Box::new(FakeDeWrapper::new(false))),
        );
        let state = router.get_volume().await.unwrap();
        assert_eq!(state.default_sink, "PipeWireAudioServer-sink");
        assert_eq!(router.last_channel(), AudioChannel::PipeWireCli);
    }

    #[tokio::test]
    async fn gnome_without_wrapper_always_uses_common() {
        // GNOME 无 DE 层音量接口（design/02 §3.5）：wrapper 必须为 None。
        let router = DePriorityRouter::new(
            BackendKind::Gnome,
            fallback(),
            AudioChannel::PipeWireCli,
            None,
        );
        router.set_volume(0.1).await.unwrap();
        let state = router.get_volume().await.unwrap();
        assert_eq!(state.default_sink, "PipeWireAudioServer-sink");
        assert_eq!(router.last_channel(), AudioChannel::PipeWireCli);
    }

    #[tokio::test]
    async fn health_reflects_fallback_when_no_wrapper() {
        let router = DePriorityRouter::new(
            BackendKind::Tty,
            Box::new(FakeServer::new("x", false)),
            AudioChannel::PipeWireCli,
            None,
        );
        assert_eq!(router.health().await, ComponentHealth::Unavailable);
    }
}
