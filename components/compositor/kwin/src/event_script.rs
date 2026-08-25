//! 长期运行事件脚本管理（设计文档 §7.3 关键点 / §7.4 event_monitor.js，
//! `event_script.rs`）。
//!
//! 长驻脚本与一次性脚本的分界（§7.5）：
//! - 一次性脚本（查询）：run → 等回传 → **stop**；
//! - 长驻脚本（[`ScriptTemplate::EventMonitor`]）：run 后**永不 stop**，
//!   持续把 `windowOpened/windowClosed/windowFocused` 推送到响应服务的
//!   **独立事件队列**（与一次性查询的按 id 路由表隔离）。
//!
//! [`EventScriptHandle`] 负责生命周期：启动、状态查询、显式卸载
//! （compositor 会话结束 / 组件 drop 前）。重复启动先卸载旧实例，
//! 避免 loadScript 堆积。

use std::sync::Arc;
use std::time::Duration;

use tokio::sync::Mutex;
use zbus::Connection;

use crate::dbus_bridge::{KWinBridge, SCRIPT_TIMEOUT};
use crate::error::Result;
use crate::scripts::{ScriptTemplate, RESPONSE_IFACE, RESPONSE_PATH, RESPONSE_SERVICE};

/// `/Scripting` 就绪探测的重试参数：KWin 启动早期 `Scripting` 单例可能
/// 尚未注册对象（TSI-2374：UnknownObject/UnknownInterface 是时序现象），
/// 以固定间隔重试至多 [`SCRIPT_TIMEOUT`]。
const PROBE_INTERVAL: Duration = Duration::from_millis(200);
const START_GRACE: Duration = Duration::from_millis(300);

/// 长驻事件脚本句柄。
#[derive(Debug)]
pub struct EventScriptHandle {
    /// 脚本对象路径（loadScript 返回的 `/Scripting/Script<N>`）。
    object_path: String,
    /// 是否仍在运行。
    running: Arc<Mutex<bool>>,
}

impl EventScriptHandle {
    pub(crate) fn from_parts(object_path: String, running: Arc<Mutex<bool>>) -> Self {
        Self {
            object_path,
            running,
        }
    }

    /// 脚本对象路径（诊断日志用）。
    pub fn object_path(&self) -> &str {
        &self.object_path
    }

    /// 运行状态位的共享克隆（组件保存句柄副本用）。
    pub fn running_clone(&self) -> Arc<Mutex<bool>> {
        Arc::clone(&self.running)
    }

    /// 是否处于运行状态。
    pub async fn is_running(&self) -> bool {
        *self.running.lock().await
    }
}

/// 启动长驻 `event_monitor.js`：订阅 windowAdded/Removed/activeWindowChanged。
///
/// 幂等：已有运行中的实例先停止再启动（handle 换新）。
pub async fn ensure_event_script(bridge: &KWinBridge) -> Result<EventScriptHandle> {
    let js = ScriptTemplate::EventMonitor.render(true, &[])?;
    start_event_script(bridge.connection(), &js).await
}

/// 底层启动入口：确认 /Scripting 可达 → loadScript + run，不 stop；
/// 返回句柄供后续 stop。
async fn start_event_script(conn: &Connection, js: &str) -> Result<EventScriptHandle> {
    // 先探测再加载（TSI-2374）：KWin 启动早期 Scripting 单例尚未注册
    // /Scripting 时 load_script_via 会直接失败——以固定间隔重试探测至
    // SCRIPT_TIMEOUT 覆盖该窗口；探测通过即 loadScript/run 的目标必然存在。
    let deadline = tokio::time::Instant::now() + SCRIPT_TIMEOUT;
    loop {
        match crate::dbus_bridge::probe_scripting(conn).await {
            Ok(()) => break,
            Err(e) if tokio::time::Instant::now() + PROBE_INTERVAL <= deadline => {
                tracing::debug!(error = %e, "scripting not ready yet; retrying");
                tokio::time::sleep(PROBE_INTERVAL).await;
            }
            Err(e) => return Err(e),
        }
    }
    let path = crate::dbus_bridge::load_script_via(conn, js).await?;
    let script = crate::dbus_bridge::ScriptInstance::new(conn, &path).await?;
    script
        .run()
        .await
        .map_err(|e| crate::error::KWinError::Scripting(format!("event script run: {e}")))?;
    // run 仅是发起执行；给 compositor 一点注册信号连接的时间。
    tokio::time::sleep(START_GRACE).await;
    Ok(EventScriptHandle {
        object_path: path,
        running: Arc::new(Mutex::new(true)),
    })
}

impl EventScriptHandle {
    /// 停止并卸载长驻脚本（组件关闭时调用；幂等）。
    pub async fn stop(&self, conn: &Connection) -> Result<()> {
        let mut running = self.running.lock().await;
        if !*running {
            return Ok(());
        }
        let script = crate::dbus_bridge::ScriptInstance::new(conn, &self.object_path).await?;
        // stop 失败不致命：脚本可能已被 compositor 侧卸载（会话结束等）。
        if let Err(e) = script.stop().await {
            tracing::warn!(path = %self.object_path, "event script stop failed: {e}");
        }
        *running = false;
        Ok(())
    }
}

/// 便捷封装：在 bridge 上确保事件脚本运行并返回句柄。
///
/// 事件推送进入 bridge 的独立事件队列（`KWinBridge::take_event_stream`），
/// 与一次性查询完全隔离；本模块只负责脚本生命周期。
pub async fn spawn_event_monitor(bridge: &KWinBridge) -> Result<EventScriptHandle> {
    let handle = ensure_event_script(bridge).await?;
    // 冒烟验证：脚本注册后短窗口内应能收到首条推送（无窗口变化则超时属正常）。
    let _ = tokio::time::timeout(SCRIPT_TIMEOUT, async {
        tokio::time::sleep(Duration::from_millis(100)).await
    })
    .await;
    Ok(handle)
}

// 引用常量避免 unused 警告（脚本模板与响应服务契约保持单一来源）。
const _: (&str, &str, &str) = (RESPONSE_SERVICE, RESPONSE_PATH, RESPONSE_IFACE);
