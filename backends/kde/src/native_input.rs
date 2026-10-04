//! KDE Wayland 原生注入后端：`org_kde_kwin_fake_input`（设计文档 §7.6 通道矩阵
//! 「输入注入」行）。
//!
//! 该通道是 KDE Wayland 唯一保证生效的注入路径：协议直达 KWin 输入栈，不经
//! portal 授权弹窗、不依赖 `/dev/uinput` 权限，也不受「KWin 忽略 XWayland XTEST
//! 注入」影响（链上 X11 注入候选在 KDE Wayland 已被排除）。协议要求先
//! `authenticate` 再注入——认证在 [`InputService::ensure_ready`] 完成，即
//! `input.send` 路径首次注入前，而非只在 doctor 探测时。
//!
//! 键码空间：`keyboard_key` 收 evdev 键码（与 libei/ydotool 共用
//! [`agent_shell_input::keymap`] 的 US QWERTY 映射）；指针坐标为合成器像素坐标，
//! 与 CLI `--at X,Y` 契约一致。
//!
//! 每次注入以协议队列 roundtrip 收尾（[`KWinProtocols::flush_queue`]）：裸 `flush`
//! 只把请求推出本地缓冲，compositor 丢弃请求（未认证/权限过滤）时调用方仍看到
//! 成功——roundtrip 让协议错误在调用点变成错误返回。roundtrip 是阻塞调用，统一
//! 经 `spawn_blocking` + §19 注入超时执行：KWin 主线程卡死时不得让 `input.send`
//! 无限挂起。

use agent_shell_core::component::ComponentHealth;
use agent_shell_core::error::{AgentShellError, Result};
use agent_shell_core::types::{KeyCombo, MouseButton};
use agent_shell_input::{keymap, InputService, Op};
use async_trait::async_trait;

use agent_shell_compositor_kwin::{FakeInput, KWinProtocols};

/// 注入用途声明（协议字段，透传给 compositor 的授权判定与用户提示）。
const AUTH_REASON: &str = "agent-shell 输入注入（键鼠自动化，仅本机会话内）";

/// 单次注入提交的阻塞预算（§19 输入注入：1s）。
///
/// 超时后那个仍卡在 roundtrip 的阻塞线程会继续持有协议队列锁，后续注入按同样
/// 预算显式失败——宁可显式报错，也不让 `input.send` 无限挂起（既无退出码也无
/// 错误信息，正是本单要消除的失败形态）。
const COMMIT_TIMEOUT: std::time::Duration = agent_shell_core::fallback::INPUT_INJECT_TIMEOUT;

/// `org_kde_kwin_fake_input` 注入后端。
///
/// 持有协议句柄 + 协议队列句柄：注入请求经同一队列提交并 roundtrip 收尾，
/// 保证「返回成功 == compositor 已处理」。
pub struct KWinNativeInput {
    fake_input: FakeInput,
    protocols: std::sync::Arc<KWinProtocols>,
}

impl KWinNativeInput {
    /// 由 KWin 合成器的 fake_input 句柄与协议队列句柄构造（链首候选）。
    ///
    /// 两者来自同一 `wl_display`（`KWinCompositor::fake_input_handle` /
    /// `protocols_handle`）。
    pub fn new(fake_input: FakeInput, protocols: std::sync::Arc<KWinProtocols>) -> Self {
        Self {
            fake_input,
            protocols,
        }
    }

    /// 提交请求并等待 compositor 处理（协议错误在此变成错误返回）。
    ///
    /// `flush_queue` 内部是 `EventQueue::roundtrip`：同步读写 wl_display，必须在
    /// `spawn_blocking` 里执行（async 上下文直接调用会占住 tokio worker，且 KWin
    /// 主线程卡住时永不返回）。超时归一为 `Timeout`：调用方拿到显式错误而非挂起。
    async fn commit(&self) -> Result<()> {
        let protocols = std::sync::Arc::clone(&self.protocols);
        run_blocking_with_timeout(COMMIT_TIMEOUT, "kwin fake_input roundtrip", move || {
            protocols.flush_queue()
        })
        .await
    }

    /// 注入一个键：press + release（含可选的临时 shift 修复）。
    fn key_press_release(&self, code: u32) -> Result<()> {
        self.fake_input.keyboard_key(code, 1)?;
        self.fake_input.keyboard_key(code, 0)?;
        Ok(())
    }

    /// 单字符注入（大写/符号补临时左 Shift）。
    fn type_char(&self, c: char) -> Result<()> {
        let (code, shift) = keymap::char_to_evdev(c)
            .ok_or_else(|| AgentShellError::Input(format!("cannot type character {c:?}")))?;
        if shift {
            self.fake_input.keyboard_key(SHIFT_LEFT, 1)?;
        }
        let pressed = self.key_press_release(code);
        if shift {
            // 释放必须在实体键之后，且无论按键注入成败都要补发：shift 卡住会让
            // 后续所有字符变成大写/符号（比丢掉一个字符更难诊断）。
            let released = self.fake_input.keyboard_key(SHIFT_LEFT, 0);
            pressed?;
            released?;
            return Ok(());
        }
        pressed
    }
}

/// evdev `KEY_LEFTSHIFT`（临时 shift 合成大写/符号）。
const SHIFT_LEFT: u32 = 42;

#[async_trait]
impl InputService for KWinNativeInput {
    fn name(&self) -> &'static str {
        "kwin-fake-input"
    }

    async fn is_available(&self) -> bool {
        true
    }

    async fn health(&self) -> ComponentHealth {
        // 协议已绑定即健康：认证在首次注入时幂等完成，无需预先探测。
        ComponentHealth::Healthy
    }

    fn supports(&self, op: Op<'_>) -> bool {
        match op {
            // 协议 v3 起有绝对指针；keyboard_key 自 v4 起（绑定区间 4..=5）。
            Op::Move(..) => self.fake_input.version() >= 3,
            Op::Key(..) | Op::Text(..) => self.fake_input.version() >= 4,
            Op::Click(..) | Op::Scroll(..) => true,
        }
    }

    async fn ensure_ready(&self, _op: Op<'_>) -> Result<()> {
        // 协议要求 authenticate 先于任何注入请求；幂等，重复调用无副作用。
        // 认证状态只在 roundtrip 确认送达**之后**置位：compositor 未处理（超时/
        // 协议错误）时保持未认证，后续注入会显式报错，而不是"打印 injected 但
        // 桌面无效果"的假成功（审查建议 3）。
        self.fake_input.authenticate(AUTH_REASON);
        self.commit().await?;
        self.fake_input.confirm_authenticated();
        Ok(())
    }

    async fn send_key(&self, combo: &KeyCombo) -> Result<()> {
        let order = keymap::combo_to_press_sequence(combo).map_err(AgentShellError::Input)?;
        for (code, _) in &order {
            self.fake_input.keyboard_key(*code, 1)?;
        }
        for (code, _) in order.iter().rev() {
            self.fake_input.keyboard_key(*code, 0)?;
        }
        self.commit().await
    }

    async fn type_text(&self, text: &str, delay_ms: u32) -> Result<()> {
        for c in text.chars() {
            self.type_char(c)?;
            if delay_ms > 0 {
                tokio::time::sleep(std::time::Duration::from_millis(u64::from(delay_ms))).await;
            }
        }
        self.commit().await
    }

    async fn mouse_move(&self, x: i32, y: i32) -> Result<()> {
        self.fake_input.pointer_move_absolute(x as f64, y as f64)?;
        self.commit().await
    }

    async fn mouse_click(&self, button: MouseButton) -> Result<()> {
        // 协议 button 用 linux/input-event-codes.h 编码（BTN_LEFT=0x110…）。
        let code = keymap::evdev_mouse_button(button);
        self.fake_input.button(code, 1)?;
        self.fake_input.button(code, 0)?;
        self.commit().await
    }

    async fn mouse_scroll(&self, dx: i32, dy: i32) -> Result<()> {
        // 协议 axis：0=垂直、1=水平。value 是"度"——KWin 内部以 `delta / 15` 折算
        // 滚轮格数（`MouseWheelAccumulator`），且 `deltaV120 == 0` 时把该值原样作为
        // wl_pointer.axis 转发给客户端。1 格 = 15° 与 ydotool/xdotool/uinput 后端
        // 「1 单位 = 1 次滚轮点击」的操作契约对齐。
        const AXIS_VERTICAL: u32 = 0;
        const AXIS_HORIZONTAL: u32 = 1;
        const DEGREES_PER_WHEEL_CLICK: f64 = 15.0;
        if dy != 0 {
            self.fake_input
                .axis(AXIS_VERTICAL, f64::from(dy) * DEGREES_PER_WHEEL_CLICK)?;
        }
        if dx != 0 {
            self.fake_input
                .axis(AXIS_HORIZONTAL, f64::from(dx) * DEGREES_PER_WHEEL_CLICK)?;
        }
        self.commit().await
    }
}

/// 在阻塞线程池上执行 `work`，最多等待 `budget`（§19 注入预算）。
///
/// 协议 roundtrip 是同步阻塞调用：放 `spawn_blocking` 避免占住 tokio worker，
/// 加超时避免 compositor 无应答时调用方无限挂起（`input.send` 既无退出码也无
/// 错误信息，是本单要消除的失败形态）。超时后阻塞线程仍会跑完（Rust 无法抢占），
/// 但调用方立即拿到 `Timeout` 显式错误。
async fn run_blocking_with_timeout<T, F>(
    budget: std::time::Duration,
    what: &str,
    work: F,
) -> Result<T>
where
    F: FnOnce() -> Result<T> + Send + 'static,
    T: Send + 'static,
{
    match tokio::time::timeout(budget, tokio::task::spawn_blocking(work)).await {
        Ok(Ok(result)) => result,
        Ok(Err(join_error)) => Err(AgentShellError::Input(format!(
            "{what} 阻塞任务 join 失败: {join_error}"
        ))),
        Err(_elapsed) => Err(AgentShellError::Timeout(format!(
            "{what} 未在 {}ms 内完成（compositor 无应答）",
            budget.as_millis()
        ))),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn blocking_work_within_budget_returns_result() {
        let out =
            run_blocking_with_timeout(std::time::Duration::from_secs(5), "fast work", || Ok(7u32))
                .await
                .expect("fast work succeeds");
        assert_eq!(out, 7);
    }

    #[tokio::test]
    async fn blocking_work_error_propagates_unchanged() {
        let err = run_blocking_with_timeout(
            std::time::Duration::from_secs(5),
            "failing work",
            || -> Result<()> { Err(AgentShellError::Input("protocol error".into())) },
        )
        .await
        .unwrap_err();
        assert!(matches!(err, AgentShellError::Input(m) if m.contains("protocol error")));
    }

    #[tokio::test]
    async fn hung_blocking_work_times_out_instead_of_hanging() {
        // 审查阻塞项 1 的回归锚定：compositor 卡住（roundtrip 永不返回）时，
        // 调用方必须在预算内拿到显式 Timeout，而不是无限挂起。
        //
        // 阻塞任务无法被超时抢占，故用通道主动放行而非 sleep：worker 在预算窗口内
        // 一直卡着，断言完成后立即释放，测试二进制不留墙钟尾巴；外层 timeout 兜底
        // ——超时机制若回归，测试以失败结束而非永久挂起。
        let (release_tx, release_rx) = std::sync::mpsc::channel::<()>();
        let started = std::time::Instant::now();
        let outcome = tokio::time::timeout(
            std::time::Duration::from_secs(5),
            run_blocking_with_timeout(
                std::time::Duration::from_millis(50),
                "hung work",
                move || {
                    let _ = release_rx.recv(); // 卡住直到断言侧放行
                    Ok(())
                },
            ),
        )
        .await;
        // 无论断言结果如何都放行 worker（否则 runtime 关闭时会等它）。
        let _ = release_tx.send(());
        let err = outcome
            .expect("timeout wrapper must return instead of awaiting a hung worker")
            .unwrap_err();
        assert!(matches!(err, AgentShellError::Timeout(_)), "{err:?}");
        assert!(
            started.elapsed() < std::time::Duration::from_secs(2),
            "timeout must fire near the budget, elapsed {:?}",
            started.elapsed()
        );
    }
}
