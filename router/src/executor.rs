//! 执行引擎（design/07 §15.2）。
//!
//! ⚠️ 测试用实现：本模块无任何生产调用点。全仓 `Executor::new` 仅命中
//! `router/src/executor/tests.rs`；daemon 的 `windows.list` /
//! `screenshot.capture` 及其余命令 handler 均直调 `Daemon` / `CaptureDispatcher`
//! 与合成器后端，不经 [`Executor::execute`]。
//!
//! 因此本文件的 `record_execution` 审计落盘只覆盖单元测试路径，不构成生产
//! 审计入口；不得以「router 统一安全检查（D6）」为由省略 daemon 侧 gate
//! 或审计。若日后 router 演变为真实分派层（daemon handler 改走
//! `Executor::execute`），移除本标注并同步接线与审计契约。
//!
//! # caller 注入点（TSI-2515）
//! router 层不自知会话身份：安全判定的 `caller_id` 由装配方经
//! [`Executor::new`] 末位参数注入，存于 [`Executor::caller_id`]。当前无
//! 生产调用点，恒注入 `"*"`（走默认策略）；演进为真实分派层时把 daemon
//! 的 `caller_id`（`daemon/src/state.rs` 经 `AGENT_SHELL_AGENT_ID` 解析）
//! 传到该注入点，勿再改回字面量。
//!
//! [`Executor`] 持有合成器后端与四个能力 dispatcher；`execute` 把
//! [`Command`] 翻译为具体调用。带 `SemanticTarget` 的窗口类命令一律先过
//! [`Executor::resolve_target`] 再调 backend。

use std::sync::Arc;
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

use agent_shell_core::component::CompositorComponent;
use agent_shell_core::error::{AgentShellError, Result};
use agent_shell_core::security::{PermissionDecision, SecurityManager};
use agent_shell_core::types::{SemanticTarget, TitleMatchMode, WindowInfo};

use crate::command::{Command, CommandResult};
use crate::dispatcher::{not_implemented, CaptureDispatcher, ElementActions, InputDispatcher};

/// 等待类命令的轮询步进（design/11 §19.3 默认值配套）。
const POLL_STEP: Duration = Duration::from_millis(100);
pub struct Executor {
    backend: Box<dyn CompositorComponent>,
    input: Arc<dyn InputDispatcher>,
    capture: Arc<dyn CaptureDispatcher>,
    a11y: Arc<dyn ElementActions>,
    /// 安全判定入口（§22.7 D6）：`execute()` 在分派前统一调用。
    security: Arc<SecurityManager>,
    /// 会话身份（app_id/executable）——本层不自知 caller，由装配方经
    /// [`Executor::new`] 注入（TSI-2515 注入点）。router 无生产调用点时
    /// 恒为 `"*"`（走默认策略）；演进为真实分派层时在此接线 daemon 的
    /// `caller_id`。
    caller_id: String,
}

impl Executor {
    pub fn new(
        backend: Box<dyn CompositorComponent>,
        input: Arc<dyn InputDispatcher>,
        capture: Arc<dyn CaptureDispatcher>,
        a11y: Arc<dyn ElementActions>,
        security: Arc<SecurityManager>,
        caller_id: String,
    ) -> Self {
        Self {
            backend,
            input,
            capture,
            a11y,
            security,
            caller_id,
        }
    }
    pub async fn execute(&self, cmd: Command) -> Result<CommandResult> {
        // §22.7 D6：SecurityManager 是唯一入口，全部命令必经。caller_id
        // 由装配方经 `Executor::new` 注入（见 `Executor::caller_id` 字段与
        // 模块头「TSI-2515 注入点」标注）；本层不自知 caller，router 无
        // 生产调用点时恒为 `"*"` 走默认策略——daemon 层持有真实 agent
        // 身份时在调用前以显式 id 复核。
        let op = cmd.operation();
        match self.security.check_permission(&self.caller_id, &op) {
            PermissionDecision::Allow => {}
            PermissionDecision::Deny(reason) => return Err(AgentShellError::Permission(reason)),
            PermissionDecision::Confirm(mode) => {
                return Err(AgentShellError::ConfirmationRequired(format!(
                    "{op} ({mode})"
                )))
            }
        }
        // 执行审计统一在收尾：`?` 只能在 `run` 内部提前返回，任何成败路径
        // 都会流出到此处回写执行态（TSI-2659 复审：失败路径同样落 2 条记录，
        // 与 daemon 侧契约一致）。
        let result = self.run(cmd).await;
        self.security
            .record_execution(&self.caller_id, &op, result.is_ok());
        result
    }

    /// 门禁放行后的命令执行体。`execute` 已完成权限判定；本函数内允许
    /// `?` 提前返回，执行结果审计由 `execute` 统一落盘。
    async fn run(&self, cmd: Command) -> Result<CommandResult> {
        match cmd {
            // ── 窗口 ──
            Command::ListWindows { filter } => {
                let windows = self.backend.list_windows().await?;
                let filtered = match filter {
                    Some(f) => windows
                        .into_iter()
                        .filter(|w| match (&f.app_id, &f.workspace) {
                            (Some(app), _) if w.app_id != *app => false,
                            (_, Some(ws)) => w
                                .workspace_id
                                .as_ref()
                                .map(|id| id.native_id == ws.to_string())
                                .unwrap_or(false),
                            _ => true,
                        })
                        .collect::<Vec<_>>(),
                    None => windows,
                };
                // 已知限制：CommandResult 最小三形态（§15.1 备注）无列表变体，
                // 此处仅返回过滤后首条 WindowInfo；全量列表查询由 daemon 侧
                // 直接调用 backend.list_windows()。列表形态随 CommandResult
                // 扩展时补齐（演进点，非语义缺陷）。
                filtered
                    .into_iter()
                    .next()
                    .map(|w| CommandResult::Window(Box::new(w)))
                    .ok_or_else(|| {
                        AgentShellError::WindowNotFound("no window matched filter".into())
                    })
            }
            Command::GetActiveWindow => self
                .backend
                .get_active_window()
                .await?
                .map(|w| CommandResult::Window(Box::new(w)))
                .ok_or_else(|| AgentShellError::WindowNotFound("no active".into())),
            Command::FocusWindow { target } => {
                let win = self.resolve_target(&target).await?;
                self.backend.focus_window(&win.id).await?;
                Ok(CommandResult::Window(Box::new(win)))
            }
            Command::MoveWindow { target, x, y } => {
                let win = self.resolve_target(&target).await?;
                self.backend.move_window(&win.id, x, y).await?;
                Ok(CommandResult::Window(Box::new(win)))
            }
            Command::ResizeWindow { target, w, h } => {
                let win = self.resolve_target(&target).await?;
                self.backend.resize_window(&win.id, w, h).await?;
                Ok(CommandResult::Window(Box::new(win)))
            }
            Command::MinimizeWindow { target } => {
                let win = self.resolve_target(&target).await?;
                self.backend.minimize_window(&win.id).await?;
                Ok(CommandResult::Success)
            }
            Command::CloseWindow { target } => {
                let win = self.resolve_target(&target).await?;
                self.backend.close_window(&win.id).await?;
                Ok(CommandResult::Success)
            }
            Command::SetWindowGeometry { target, rect } => {
                let win = self.resolve_target(&target).await?;
                self.backend.set_window_geometry(&win.id, rect).await?;
                Ok(CommandResult::Window(Box::new(win)))
            }

            // ── 工作区 ──
            Command::ListWorkspaces => {
                // 语义偏移说明：File 形态此处承载「标识字符串」而非文件路径
                // ——CommandResult 最小三形态无专用变体，工作区 id 为唯一可
                // 返回的标识。扩展 Workspace 变体时迁移（演进点）。
                let workspaces = self.backend.list_workspaces().await?;
                workspaces
                    .into_iter()
                    .next()
                    .map(|ws| CommandResult::File(ws.id.native_id))
                    .ok_or_else(|| AgentShellError::Other("no workspace".into()))
            }
            Command::SwitchWorkspace { id } => {
                self.backend.activate_workspace(&id).await?;
                Ok(CommandResult::Success)
            }
            Command::MoveWindowToWorkspace { window, workspace } => {
                let win = self.resolve_target(&window).await?;
                self.backend
                    .move_window_to_workspace(&win.id, &workspace)
                    .await?;
                Ok(CommandResult::Window(Box::new(win)))
            }

            // ── 输入 ──
            Command::SendKey { combo } => {
                self.input.send_key_combo(&combo.keys).await?;
                Ok(CommandResult::Success)
            }
            Command::TypeText { text, target } => self.execute_type_text(text, target).await,
            Command::MouseClick { button, target } => {
                if let Some(target) = &target {
                    // 点击语义目标：先解析到窗口，再点击其内容几何中心。
                    let win = self.resolve_target(target).await?;
                    let cx = win.geometry.x + win.geometry.width / 2;
                    let cy = win.geometry.y + win.geometry.height / 2;
                    self.input.mouse_move(cx, cy).await?;
                }
                self.input.mouse_click(button).await?;
                Ok(CommandResult::Success)
            }
            Command::MouseMove { x, y } => {
                self.input.mouse_move(x, y).await?;
                Ok(CommandResult::Success)
            }
            Command::Scroll { dx, dy } => {
                self.input.scroll(dx, dy).await?;
                Ok(CommandResult::Success)
            }

            Command::Screenshot { target, output } => {
                let image = self.capture.capture_png(&target).await?;
                let path =
                    output.unwrap_or_else(|| format!("screenshot_{}.png", unix_timestamp_ns()));
                tracing::debug!(target = %path, bytes = image.len(), "screenshot saved");
                tokio::fs::write(&path, &image)
                    .await
                    .map_err(|e| AgentShellError::Other(Box::new(e)))?;
                Ok(CommandResult::File(path))
            }

            // ── 无障碍 ──
            Command::GetElementText { target } => {
                let elements = self.a11y.locate(&target).await?;
                let el = elements.first().ok_or_else(|| {
                    AgentShellError::WindowNotFound(format!("a11y element '{target:?}'"))
                })?;
                Ok(CommandResult::File(el.text().await?))
            }
            Command::GetA11yTree { .. } => Err(not_implemented("a11y tree query")),

            // ── 等待 ──
            Command::WaitForWindow { app_id, timeout } => {
                self.wait_for_window(&app_id, timeout).await
            }
            Command::WaitForText { text, timeout } => {
                // 轮询 A11y 树文本匹配（事件驱动等待为后续演进点，见 wait_for_window 注释）。
                let start = Instant::now();
                'poll: loop {
                    if let Ok(elements) = self
                        .a11y
                        .locate(&SemanticTarget::ByAccessibility {
                            role: None,
                            name: Some(text.clone()),
                            parent_role: None,
                            parent_name: None,
                        })
                        .await
                    {
                        for el in &elements {
                            if el.text().await.map(|t| t.contains(&text)).unwrap_or(false) {
                                break 'poll Ok(CommandResult::Success);
                            }
                        }
                    }
                    if start.elapsed() > timeout {
                        break Err(AgentShellError::Timeout(format!(
                            "text '{}' not appeared",
                            text
                        )));
                    }
                    tokio::time::sleep(POLL_STEP).await;
                }
            }

            // ── 系统 ──
            Command::GetDesktopInfo => {
                let caps = self.backend.capabilities();
                Ok(CommandResult::File(format!("{caps:?}")))
            }
            Command::GetBackendStatus => {
                let health = self.backend.health().await;
                Ok(CommandResult::File(format!("{health:?}")))
            }
        }
    }

    /// `TypeText` 标准降级模式（design/11 §19.2「输入文本」链）：
    /// AT-SPI 语义输入成功即返回；失败静默降级键盘模拟。
    async fn execute_type_text(
        &self,
        text: String,
        target: Option<SemanticTarget>,
    ) -> Result<CommandResult> {
        if let Some(target) = &target {
            // 优先 AT-SPI 语义输入
            if let Ok(elements) = self.a11y.locate(target).await {
                if let Some(el) = elements.first() {
                    if el.set_text(&text).await.is_ok() {
                        return Ok(CommandResult::Success);
                    }
                }
            }
        }
        // 降级：键盘模拟
        self.input.type_text(&text, 0).await?;
        Ok(CommandResult::Success)
    }

    /// 等待 app_id 对应窗口出现：轮询实现，100ms 步进。
    ///
    /// 演进点：设计要求事件驱动等待优先于轮询等待——事件流就绪后应改为
    /// 订阅 `DesktopEvent::WindowCreated` 优先、本轮询逻辑作兜底。
    async fn wait_for_window(&self, app_id: &str, timeout: Duration) -> Result<CommandResult> {
        let start = Instant::now();
        loop {
            let windows = self.backend.list_windows().await?;
            if windows.iter().any(|w| w.app_id == app_id) {
                return Ok(CommandResult::Success);
            }
            if start.elapsed() > timeout {
                return Err(AgentShellError::Timeout(format!("'{app_id}' not appeared")));
            }
            tokio::time::sleep(POLL_STEP).await;
        }
    }

    /// 解析语义目标为具体 [`WindowInfo`]（§15.2 下半 + §15.3）。
    ///
    /// 未在 match 中显式实现的分支（ByPid / ByDesktopFile / ByCoordinate /
    /// ByRegion）按设计文档占位行为返回 `NotImplemented`；
    /// ById 因 WindowId 已可直查，实现为 `get_window_info` 直查。
    pub async fn resolve_target(&self, target: &SemanticTarget) -> Result<WindowInfo> {
        match target {
            SemanticTarget::Active => self
                .backend
                .get_active_window()
                .await?
                .ok_or_else(|| AgentShellError::WindowNotFound("no active".into())),

            SemanticTarget::ById(id) => self.backend.get_window_info(id).await,

            SemanticTarget::ByAppId(id) => {
                let windows = self.backend.list_windows().await?;
                windows
                    .into_iter()
                    .find(|w| w.app_id == *id)
                    .ok_or_else(|| AgentShellError::WindowNotFound(format!("app '{id}'")))
            }

            SemanticTarget::ByTitle(title, mode) => {
                let windows = self.backend.list_windows().await?;
                windows
                    .into_iter()
                    .find(|w| title_matches(mode, title, &w.title))
                    .ok_or_else(|| AgentShellError::WindowNotFound(format!("title '{title}'")))
            }

            SemanticTarget::ByAccessibility { .. } => {
                // AT-SPI 定位 → 找到所属窗口 → 匹配 backend 窗口列表。
                //
                // 匹配语义（双向包含，有意为之）：AT-SPI 返回的窗口名与
                // 合成器侧的 title/app_id 无规范映射——Wayland 下 AT-SPI
                // window name 常等于 app_id（如 "org.kde.konsole"），X11 下
                // 常为标题的子串。因此对 backend 每个窗口做双向子串匹配：
                // `title.contains(&w.app_id)` 覆盖 Wayland app_id 形态，
                // `w.title.contains(&title)` 覆盖 X11 标题形态。误配风险由
                // 首个命中的 z-order 排序兜底（list_windows 按 stacking 返回）。
                let elements = self.a11y.locate(target).await?;
                let el = elements
                    .first()
                    .ok_or_else(|| AgentShellError::WindowNotFound("AT-SPI element".into()))?;
                let title = el.find_parent_window_name().await?;
                let windows = self.backend.list_windows().await?;
                windows
                    .into_iter()
                    .find(|w| title.contains(&w.app_id) || w.title.contains(&title))
                    .ok_or_else(|| AgentShellError::WindowNotFound("AT-SPI window".into()))
            }

            other => Err(AgentShellError::NotImplemented(format!(
                "router: target resolution for {other:?}"
            ))),
        }
    }
}

/// 标题匹配（§15.3 第 3 级：子串 → 精确 → 正则 → glob）。
///
/// 正则编译失败按「不匹配」处理，不向上抛错。
fn title_matches(mode: &TitleMatchMode, pattern: &str, title: &str) -> bool {
    match mode {
        TitleMatchMode::Substring => title.contains(pattern),
        TitleMatchMode::Exact => title == pattern,
        TitleMatchMode::Regex => regex::Regex::new(pattern)
            .map(|r| r.is_match(title))
            .unwrap_or(false),
        TitleMatchMode::Glob => glob_match::glob_match(pattern, title),
    }
}

/// 纳秒时间戳——默认截图文件名用，避免同秒覆盖。
fn unix_timestamp_ns() -> u128 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_nanos()
}

#[cfg(test)]
mod tests;
