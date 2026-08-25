//! daemon 核心状态：全部持久化连接的持有者（设计文档 §22.2 D1 / §23.2）。
//!
//! daemon 常驻用户会话，持有：
//! - KWin 合成器通道（Wayland org_kde_* + Scripting 桥，含事件脚本）
//! - X11 通道（EWMH/ICCCM/XTest；XWayland 会话下输入注入降级用）
//! - WindowStateCache（查询走缓存；T3b 事件归一化落地后改为事件驱动刷新，
//!   当前以 TTL 短缓存近似——如实标注 `from_cache` 语义）

use agent_shell_compositor_kwin::KWinCompositor;
use agent_shell_core::component::CompositorComponent;
use agent_shell_core::types::WindowInfo;
use std::time::{Duration, Instant};

/// 缓存条目有效期。T3b（EventHub 归一化）落地后由事件失效替代。
const CACHE_TTL: Duration = Duration::from_secs(2);

/// daemon 会话状态。
pub struct Daemon {
    compositor: Option<KWinCompositor>,
    cache: Vec<WindowInfo>,
    cached_at: Option<Instant>,
    /// 空闲退出时限（§22.2：默认 30min，可配置）。
    pub idle_timeout: Duration,
}

impl Daemon {
    /// 装配：连接当前会话合成器。失败不 panic——doctor 场景需要 daemon
    /// 存活以报告诊断细节，各方法在 `compositor=None` 时返回明确错误。
    pub async fn connect(idle_timeout: Duration) -> Self {
        let compositor = match session_kind().as_str() {
            "kde" | "dde" => if std::env::var("WAYLAND_DISPLAY").is_ok() {
                KWinCompositor::new_wayland().await
            } else {
                KWinCompositor::new_x11().await
            }
            .map(Some)
            .map_err(|e| tracing::warn!("compositor assemble failed: {e}"))
            .unwrap_or(None),
            other => {
                tracing::warn!("no compositor component for {other:?} (implemented: KDE, DDE)");
                None
            }
        };
        Self {
            compositor,
            cache: Vec::new(),
            cached_at: None,
            idle_timeout,
        }
    }

    fn require_compositor(
        &self,
    ) -> Result<&KWinCompositor, (agent_shell_rpc::RpcErrorCode, String)> {
        self.compositor.as_ref().ok_or_else(|| {
            (
                agent_shell_rpc::RpcErrorCode::BackendUnavailable,
                "compositor channel unavailable in this session".into(),
            )
        })
    }

    /// 窗口列表：命中未过期缓存直接返回（零系统调用），否则经 KWin 刷新。
    ///
    /// 返回 `(列表, 是否来自缓存)`。
    pub async fn list_windows(
        &mut self,
    ) -> Result<(Vec<WindowInfo>, bool), (agent_shell_rpc::RpcErrorCode, String)> {
        if let Some(at) = self.cached_at {
            if at.elapsed() < CACHE_TTL {
                return Ok((self.cache.clone(), true));
            }
        }
        let wins = self
            .require_compositor()?
            .list_windows()
            .await
            .map_err(|e| (agent_shell_rpc::RpcErrorCode::BackendError, e.to_string()))?;
        self.cache = wins;
        self.cached_at = Some(Instant::now());
        Ok((self.cache.clone(), false))
    }

    /// 单窗查询（复用窗口列表缓存）。
    pub async fn window_info(
        &mut self,
        native_id: &str,
    ) -> Result<WindowInfo, (agent_shell_rpc::RpcErrorCode, String)> {
        let (wins, _) = self.list_windows().await?;
        wins.into_iter()
            .find(|w| w.id.native_id == native_id)
            .ok_or_else(|| {
                (
                    agent_shell_rpc::RpcErrorCode::NotFound,
                    format!("window not found: {native_id:?}"),
                )
            })
    }

    /// 窗口写操作（写操作直通持久化连接，不经缓存）。
    pub async fn window_op(
        &self,
        native_id: &str,
        op: agent_shell_rpc::WindowOpKind,
        x: i32,
        y: i32,
        w: i32,
        h: i32,
    ) -> Result<(), (agent_shell_rpc::RpcErrorCode, String)> {
        use agent_shell_rpc::WindowOpKind as K;
        let comp = self.require_compositor()?;
        let id = agent_shell_core::types::WindowId {
            native_id: native_id.to_string(),
            de_type: agent_shell_core::types::DesktopEnvironment::KDE,
        };
        // 写前校验目标存在（避免协议通道乐观发送吞掉无效 uuid）。
        let windows = comp
            .list_windows()
            .await
            .map_err(|e| (agent_shell_rpc::RpcErrorCode::BackendError, e.to_string()))?;
        if !windows.iter().any(|win| win.id.native_id == native_id) {
            return Err((
                agent_shell_rpc::RpcErrorCode::NotFound,
                format!("window not found: {native_id:?}"),
            ));
        }
        let r = match op {
            K::Focus => comp.focus_window(&id).await,
            K::Move => comp.move_window(&id, x, y).await,
            K::Resize => comp.resize_window(&id, w, h).await,
            K::Minimize => comp.minimize_window(&id).await,
            K::Close => comp.close_window(&id).await,
        };
        r.map_err(|e| (agent_shell_rpc::RpcErrorCode::BackendError, e.to_string()))
    }

    /// KWin 合成器 doctor 输出行（dispatch 层异步收集用）。
    pub fn doctor_lines(&self) -> Vec<String> {
        match self.compositor.as_ref() {
            Some(c) => c.doctor_lines(),
            None => Vec::new(),
        }
    }

    /// 工作区切换。
    pub async fn activate_workspace(
        &self,
        ws: &str,
    ) -> Result<(), (agent_shell_rpc::RpcErrorCode, String)> {
        let comp = self.require_compositor()?;
        let list = comp
            .list_workspaces()
            .await
            .map_err(|e| (agent_shell_rpc::RpcErrorCode::BackendError, e.to_string()))?;
        let target = list
            .iter()
            .find(|w| w.id.native_id == ws || w.number.to_string() == ws)
            .ok_or_else(|| {
                (
                    agent_shell_rpc::RpcErrorCode::NotFound,
                    format!("workspace not found: {ws:?}"),
                )
            })?;
        comp.activate_workspace(&target.id)
            .await
            .map_err(|e| (agent_shell_rpc::RpcErrorCode::BackendError, e.to_string()))
    }

    /// 工作区列表（无缓存——低频操作）。
    pub async fn list_workspaces(
        &self,
    ) -> Result<Vec<String>, (agent_shell_rpc::RpcErrorCode, String)> {
        let comp = self.require_compositor()?;
        let list = comp
            .list_workspaces()
            .await
            .map_err(|e| (agent_shell_rpc::RpcErrorCode::BackendError, e.to_string()))?;
        Ok(list
            .into_iter()
            .map(|w| format!("{} {} active={}", w.number, w.name, w.is_active))
            .collect())
    }

    /// 组件健康摘要（doctor 数据源之一）。
    pub fn compositor_health(&self) -> Result<agent_shell_core::component::ComponentHealth, ()> {
        // health() 是 async；此处仅暴露可用性。完整 doctor 行在 dispatch 层异步收集。
        match &self.compositor {
            Some(_) => Ok(agent_shell_core::component::ComponentHealth::Healthy),
            None => Err(()),
        }
    }

    pub fn has_compositor(&self) -> bool {
        self.compositor.is_some()
    }
}

/// 当前会话的 DE 归类（与 core 检测同口径）。
fn session_kind() -> String {
    use agent_shell_core::de_detection::detect_desktop_environment;
    use agent_shell_core::types::DesktopEnvironment::*;
    match detect_desktop_environment() {
        KDE | DDE => "kde".into(),
        GNOME => "gnome".into(),
        Tty | Unknown => "none".into(),
        _ => "other".into(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn connect_survives_without_compositor() {
        // daemon 契约：装配失败不 panic（doctor 场景需要 daemon 存活报告诊断）。
        let d = Daemon::connect(Duration::from_secs(1)).await;
        // 在无合成器会话/CI 环境下 has_compositor 可能为 false，但结构必须完整。
        assert_eq!(d.has_compositor(), d.compositor.is_some());
    }

    #[tokio::test]
    async fn require_compositor_errors_gracefully() {
        let d = Daemon::connect(Duration::from_secs(1)).await;
        if !d.has_compositor() {
            let (code, msg) = match d.require_compositor() {
                Err(e) => e,
                Ok(_) => panic!("expected error without compositor"),
            };
            assert_eq!(code, agent_shell_rpc::RpcErrorCode::BackendUnavailable);
            assert!(msg.contains("unavailable"));
        }
    }
}
