//! 记录型输入后端：断言 `input.send` 落到后端的操作与参数。
//!
//! 生产候选链的环境探测是真实 I/O（portal/libei、`/dev/uinput`、X server），
//! 无图形 CI 无法复现；验收经 [`InputComponentHandle::from_candidates`] 注入
//! 确定性候选链，请求仍走真实 `dispatch` 与真实 dispatcher（降级、能力缺口、
//! 错误映射都在生产路径上）。

use std::sync::{Arc, Mutex};

use agent_shell_core::component::ComponentHealth;
use agent_shell_core::error::{AgentShellError, Result};
use agent_shell_core::types::{KeyCombo, MouseButton};
use agent_shell_input::{InputService, Op, SetupFailure};
use async_trait::async_trait;

/// 一次注入操作的观测记录。
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum RecordedOp {
    /// 按键组合（修饰键与键序列原样）。
    Key(KeyCombo),
    /// 文本与字符间隔。
    Text(String, u32),
    /// 绝对坐标移动。
    Move(i32, i32),
    /// 指针按键。
    Click(MouseButton),
    /// 滚轮增量（dx, dy）。
    Scroll(i32, i32),
}

/// 通道建立行为：模拟可安全降级与不可降级的失败。
#[derive(Clone, Copy, Debug)]
pub enum SetupBehaviour {
    /// 通道就绪。
    Ready,
    /// 仅缺本次操作能力（能力缺口，不摘除候选）。
    CapabilityGap(&'static str),
    /// 通道不可用：授权被拒（不重试，直接降级）。
    Permission,
    /// 通道不可用：后端结构性缺失（直接降级）。
    BackendUnavailable,
}

/// 记录型输入后端。
#[derive(Clone)]
pub struct RecordingInput {
    name: &'static str,
    available: bool,
    setup: SetupBehaviour,
    calls: Arc<Mutex<Vec<RecordedOp>>>,
}

impl RecordingInput {
    /// 就绪后端。
    pub fn new(name: &'static str) -> Self {
        Self {
            name,
            available: true,
            setup: SetupBehaviour::Ready,
            calls: Arc::new(Mutex::new(Vec::new())),
        }
    }

    /// 探测不可用的候选（仍留在链上，用于断言「能力缺口不摘除候选」）。
    pub fn unavailable(name: &'static str) -> Self {
        Self {
            name,
            available: false,
            setup: SetupBehaviour::Ready,
            calls: Arc::new(Mutex::new(Vec::new())),
        }
    }

    /// 覆盖通道建立行为。
    pub fn with_setup(mut self, setup: SetupBehaviour) -> Self {
        self.setup = setup;
        self
    }

    /// 已记录的注入操作。
    pub fn calls(&self) -> Vec<RecordedOp> {
        self.calls.lock().expect("input call log").clone()
    }

    /// 清空记录（分阶段断言）。
    pub fn clear(&self) {
        self.calls.lock().expect("input call log").clear();
    }

    fn record(&self, op: RecordedOp) {
        self.calls.lock().expect("input call log").push(op);
    }
}

#[async_trait]
impl InputService for RecordingInput {
    fn name(&self) -> &'static str {
        self.name
    }

    async fn is_available(&self) -> bool {
        self.available
    }

    async fn health(&self) -> ComponentHealth {
        if self.available {
            ComponentHealth::Healthy
        } else {
            ComponentHealth::Degraded("fixture candidate unavailable".into())
        }
    }

    async fn ensure_ready(&self, _op: Op<'_>) -> std::result::Result<(), SetupFailure> {
        match self.setup {
            SetupBehaviour::Ready => Ok(()),
            SetupBehaviour::CapabilityGap(reason) => {
                Err(SetupFailure::CapabilityGap(reason.to_string()))
            }
            SetupBehaviour::Permission => Err(SetupFailure::Unavailable(
                AgentShellError::Permission("fixture authorization denied".into()),
            )),
            SetupBehaviour::BackendUnavailable => Err(SetupFailure::Unavailable(
                AgentShellError::BackendUnavailable("fixture channel unavailable".into()),
            )),
        }
    }

    async fn send_key(&self, combo: &KeyCombo) -> Result<()> {
        self.record(RecordedOp::Key(combo.clone()));
        Ok(())
    }

    async fn type_text(&self, text: &str, delay_ms: u32) -> Result<()> {
        self.record(RecordedOp::Text(text.to_string(), delay_ms));
        Ok(())
    }

    async fn mouse_move(&self, x: i32, y: i32) -> Result<()> {
        self.record(RecordedOp::Move(x, y));
        Ok(())
    }

    async fn mouse_click(&self, button: MouseButton) -> Result<()> {
        self.record(RecordedOp::Click(button));
        Ok(())
    }

    async fn mouse_scroll(&self, dx: i32, dy: i32) -> Result<()> {
        self.record(RecordedOp::Scroll(dx, dy));
        Ok(())
    }
}
