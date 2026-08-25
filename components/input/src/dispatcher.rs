//! `InputService` trait 与 `InputDispatcher`（设计文档 §12.2）。
//!
//! 降级链构造顺序（§12.1/§12.2）：
//! libei（Wayland 首选，非 X11 会话压入）→ ydotool（跨 DE 保底，需
//! `ydotool` 可执行且 `/dev/uinput` 存在）→ XTest（仅原生 X11，连接成功
//! 才压入）→ xdotool（仅 X11 且可执行存在）。随后取第一个
//! `is_available() == true` 的后端为 `active`；全部不可用则 `active = None`
//! （调用返回错误，不 panic）。
//!
//! 超时/重试（§19）：input 注入超时 1s、重试 3 次——由各后端的命令执行层
//! 统一施加（[`super::ydotool`] / [`super::xdotool`]），dispatcher 不重复包装。

use std::time::Duration;

use agent_shell_core::component::ComponentHealth;
use agent_shell_core::error::{AgentShellError, Result};
use agent_shell_core::types::{DesktopEnvironment, KeyCombo, MouseButton};
use async_trait::async_trait;

/// input 注入超时（§19 超时与重试表）。
pub const INPUT_TIMEOUT: Duration = Duration::from_secs(1);
/// input 注入重试次数（§19；首次 + 重试共 3 次尝试由后端命令层实现）。
pub const INPUT_RETRIES: u32 = 3;

/// 单个输入注入后端的统一接口。
#[async_trait]
pub trait InputService: Send + Sync {
    /// 后端名（doctor 输出 / 日志）。
    fn name(&self) -> &'static str;

    /// 异步可用性探测（dispatcher 构造期逐个调用，选中第一个 true）。
    async fn is_available(&self) -> bool;

    /// 构造期之后的健康检查（doctor 命令调用）。
    ///
    /// 默认按 `is_available` 归一化为 Healthy/Degraded；后端可覆盖以给出
    /// 更细的降级原因。
    async fn health(&self) -> ComponentHealth {
        if self.is_available().await {
            ComponentHealth::Healthy
        } else {
            ComponentHealth::Degraded("probe failed at health check".into())
        }
    }

    /// 注入按键组合。
    async fn send_key(&self, combo: &KeyCombo) -> Result<()>;

    /// 键入文本（`delay_ms` 为字符间隔）。
    async fn type_text(&self, text: &str, delay_ms: u32) -> Result<()>;

    /// 移动鼠标到绝对坐标 `(x, y)`。
    async fn mouse_move(&self, x: i32, y: i32) -> Result<()>;

    /// 点击鼠标按键。
    async fn mouse_click(&self, button: MouseButton) -> Result<()>;

    /// 滚动（正值 dy 向下、正值 dx 向右）。
    async fn mouse_scroll(&self, dx: i32, dy: i32) -> Result<()>;
}

/// 带降级链的输入分发器（§12.2）。
pub struct InputDispatcher {
    backends: Vec<Box<dyn InputService>>,
    active: Option<usize>,
}

impl InputDispatcher {
    /// 按 DE 探测候选集合并选出第一个可用的 active 后端。
    pub async fn new(de_type: DesktopEnvironment) -> Result<Self> {
        let mut backends: Vec<Box<dyn InputService>> = Vec::new();

        // 1. libei/EIS（Wayland 首选）：portal RemoteDesktop → EI 协议。
        //    探测失败（无 portal / 用户未授权 / 非 Wayland）不阻塞后续候选。
        if de_type != DesktopEnvironment::X11Generic && !de_type.is_tty() {
            match super::libei::LibeiInput::new().await {
                Ok(libei) => backends.push(Box::new(libei)),
                Err(e) => tracing::debug!("input: libei candidate unavailable: {e}"),
            }
        }

        // 2. ydotool（跨 DE 保底）：需要 ydotool 可执行 + /dev/uinput。
        if which::which("ydotool").is_ok() && std::path::Path::new("/dev/uinput").exists() {
            backends.push(Box::new(super::ydotool::YdotoolInput::new()));
        }

        // 3. XTest 扩展（X11 原生）：连接失败不压入。
        if de_type == DesktopEnvironment::X11Generic {
            if let Ok(xtest) = super::xtest::XTestInput::new() {
                backends.push(Box::new(xtest));
            }
        }

        // 4. xdotool（X11 保底）。
        if de_type.supports_x11()
            && std::env::var_os("DISPLAY").is_some()
            && which::which("xdotool").is_ok()
        {
            backends.push(Box::new(super::xdotool::XdotoolInput::new()));
        }

        // 选择第一个可用后端。
        let mut active = None;
        for (i, b) in backends.iter().enumerate() {
            if b.is_available().await {
                active = Some(i);
                break;
            }
        }
        tracing::info!(
            candidates = backends.len(),
            active = active.map(|i| backends[i].name()).unwrap_or("none"),
            "input dispatcher assembled"
        );
        Ok(Self { backends, active })
    }

    /// 当前激活的后端名（doctor：Healthy/Degraded 标明实际选中后端）。
    pub fn active_backend_name(&self) -> Option<&'static str> {
        self.active.map(|i| self.backends[i].name())
    }

    /// 候选后端名列表（诊断/测试用，按降级链顺序）。
    pub fn backend_names(&self) -> Vec<&'static str> {
        self.backends.iter().map(|b| b.name()).collect()
    }

    /// active 后端健康检查。
    pub async fn active_health(&self) -> ComponentHealth {
        match self.active {
            Some(i) => self.backends[i].health().await,
            None => ComponentHealth::Unavailable,
        }
    }

    fn active_backend(&self) -> Result<&dyn InputService> {
        let i = self.active.ok_or_else(|| {
            AgentShellError::BackendUnavailable(
                "input: no available backend in this session".into(),
            )
        })?;
        Ok(self.backends[i].as_ref())
    }

    /// 注入按键组合（委托 active 后端）。
    pub async fn send_key(&self, combo: &KeyCombo) -> Result<()> {
        self.active_backend()?.send_key(combo).await
    }

    /// 键入文本（委托 active 后端）。
    pub async fn type_text(&self, text: &str, delay_ms: u32) -> Result<()> {
        self.active_backend()?.type_text(text, delay_ms).await
    }

    /// 移动鼠标（委托 active 后端）。
    pub async fn mouse_move(&self, x: i32, y: i32) -> Result<()> {
        self.active_backend()?.mouse_move(x, y).await
    }

    /// 点击鼠标（委托 active 后端）。
    pub async fn mouse_click(&self, button: MouseButton) -> Result<()> {
        self.active_backend()?.mouse_click(button).await
    }

    /// 滚动（委托 active 后端）。
    pub async fn mouse_scroll(&self, dx: i32, dy: i32) -> Result<()> {
        self.active_backend()?.mouse_scroll(dx, dy).await
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use agent_shell_core::types::{KeyName, ModifierMask};
    use std::sync::atomic::{AtomicUsize, Ordering};
    use std::sync::Arc;

    /// 记录探测顺序的假后端：第 N 个实例前 N-1 次探测返回 false。
    #[derive(Clone)]
    struct FakeBackend {
        name: &'static str,
        fail_first: Arc<AtomicUsize>,
        probed: Arc<AtomicUsize>,
        inject_ok: bool,
    }

    impl FakeBackend {
        fn new(name: &'static str, fail_count: usize) -> Self {
            Self {
                name,
                fail_first: Arc::new(AtomicUsize::new(fail_count)),
                probed: Arc::new(AtomicUsize::new(0)),
                inject_ok: true,
            }
        }
    }

    /// 注入即失败的假后端（验证错误透传）。
    struct FailingBackend;

    #[async_trait]
    impl InputService for FailingBackend {
        fn name(&self) -> &'static str {
            "failing"
        }
        async fn is_available(&self) -> bool {
            true
        }
        async fn send_key(&self, _combo: &KeyCombo) -> Result<()> {
            Err(AgentShellError::Input("injection refused".into()))
        }
        async fn type_text(&self, _text: &str, _delay_ms: u32) -> Result<()> {
            unimplemented!()
        }
        async fn mouse_move(&self, _x: i32, _y: i32) -> Result<()> {
            unimplemented!()
        }
        async fn mouse_click(&self, _button: MouseButton) -> Result<()> {
            unimplemented!()
        }
        async fn mouse_scroll(&self, _dx: i32, _dy: i32) -> Result<()> {
            unimplemented!()
        }
    }

    #[async_trait]
    impl InputService for FakeBackend {
        fn name(&self) -> &'static str {
            self.name
        }
        async fn is_available(&self) -> bool {
            let n = self.probed.fetch_add(1, Ordering::SeqCst);
            n >= self.fail_first.load(Ordering::SeqCst)
        }
        async fn send_key(&self, _combo: &KeyCombo) -> Result<()> {
            if self.inject_ok {
                Ok(())
            } else {
                Err(AgentShellError::Input("no".into()))
            }
        }
        async fn type_text(&self, _text: &str, _delay_ms: u32) -> Result<()> {
            Ok(())
        }
        async fn mouse_move(&self, _x: i32, _y: i32) -> Result<()> {
            Ok(())
        }
        async fn mouse_click(&self, _button: MouseButton) -> Result<()> {
            Ok(())
        }
        async fn mouse_scroll(&self, _dx: i32, _dy: i32) -> Result<()> {
            Ok(())
        }
    }

    fn combo(ctrl: bool) -> KeyCombo {
        KeyCombo {
            keys: vec![agent_shell_core::types::Key::Named(KeyName::Return)],
            modifiers: if ctrl {
                ModifierMask::CTRL
            } else {
                ModifierMask::NONE
            },
        }
    }

    /// 用假后端组装 dispatcher 的测试通道（绕过真机探测）。
    fn dispatch_with(
        backends: Vec<Box<dyn InputService>>,
        active: Option<usize>,
    ) -> InputDispatcher {
        // 通过模块内私有字段构造——同 crate 测试可见。
        InputDispatcher { backends, active }
    }

    #[tokio::test]
    async fn picks_first_available_backend_in_chain_order() {
        let a = FakeBackend::new("a", usize::MAX); // 永不可用
        let b = FakeBackend::new("b", 0); // 首次即可用
        let c = FakeBackend::new("c", 0);
        let d = dispatch_with(vec![Box::new(a), Box::new(b), Box::new(c)], None);
        let d = reselect(d).await;
        assert_eq!(d.active_backend_name(), Some("b"));
    }

    #[tokio::test]
    async fn no_available_backend_yields_none_not_panic() {
        let a = FakeBackend::new("a", usize::MAX);
        let d = dispatch_with(vec![Box::new(a)], None);
        let d = reselect(d).await;
        assert_eq!(d.active_backend_name(), None);
        let err = d.send_key(&combo(true)).await.unwrap_err();
        assert!(matches!(err, AgentShellError::BackendUnavailable(_)));
    }

    #[tokio::test]
    async fn delegation_reaches_active_backend() {
        let dead = FakeBackend::new("dead", usize::MAX);
        let live = FakeBackend::new("live", 0);
        let d = dispatch_with(vec![Box::new(dead), Box::new(live)], None);
        let d = reselect(d).await;
        assert_eq!(d.active_backend_name(), Some("live"));
        d.send_key(&combo(false)).await.expect("send_key delegates");
        d.type_text("hi", 10).await.expect("type_text delegates");
        d.mouse_move(5, 6).await.expect("mouse_move delegates");
        d.mouse_click(MouseButton::Left)
            .await
            .expect("mouse_click delegates");
        d.mouse_scroll(0, -3).await.expect("mouse_scroll delegates");
    }

    #[tokio::test]
    async fn backend_error_surfaces_as_input_error() {
        let d = dispatch_with(vec![Box::new(FailingBackend)], Some(0));
        let err = d.send_key(&combo(true)).await.unwrap_err();
        assert!(matches!(err, AgentShellError::Input(msg) if msg.contains("refused")));
    }

    #[test]
    fn timeout_constants_match_design() {
        // §19：input 注入超时 1s、重试 3 次。
        assert_eq!(INPUT_TIMEOUT, Duration::from_secs(1));
        assert_eq!(INPUT_RETRIES, 3);
    }

    /// 测试辅助：对已构造的 dispatcher 重新执行「选第一个可用」逻辑，
    /// 使 FakeBackend 的探测计数语义生效。
    async fn reselect(mut d: InputDispatcher) -> InputDispatcher {
        d.active = None;
        for (i, b) in d.backends.iter().enumerate() {
            if b.is_available().await {
                d.active = Some(i);
                break;
            }
        }
        d
    }
}
