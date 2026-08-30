//! Executor 单元测试——mock CompositorComponent / dispatcher 注入。
//!
//! 覆盖验收标准：
//! - FocusWindow(ByAppId) 命中正确窗口并调用 focus；无命中返回 WindowNotFound
//! - ByTitle 四种 TitleMatchMode 各有正反例；Regex 非法表达式不 panic 且视为不匹配
//! - WaitForWindow：窗口出现前超时返回 Timeout，出现后立即成功（可编程 mock）
//! - TypeText 降级：a11y set_text 失败时最终走 input.type_text 并返回 Success

use parking_lot::Mutex;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::Arc;
use std::time::Duration;

use agent_shell_core::component::{
    BackendCapabilities, ComponentHealth, ComponentType, CompositorComponent, DesktopComponent,
};
use agent_shell_core::error::{AgentShellError, Result};
use agent_shell_core::security::{AgentShellConfig, ConfirmMode, SecurityManager};
use agent_shell_core::types::{
    CaptureTarget, DesktopEnvironment, Key, KeyCombo, MonitorInfo, MouseButton, Rect,
    SemanticTarget, TitleMatchMode, WindowId, WindowInfo, WorkspaceId, WorkspaceInfo,
};
use async_trait::async_trait;

use crate::command::{Command, CommandResult};
use crate::dispatcher::{A11yElement, CaptureDispatcher, ElementActions, InputDispatcher};
use crate::executor::Executor;
use crate::executor::POLL_STEP as _POLL_STEP_WITNESS;

// ───────────────────────── mock 合成器 ─────────────────────────

/// 可编程合成器 mock：窗口列表可运行期切换（WaitForWindow 测试用），
/// focus 调用被记录供断言。
struct MockCompositor {
    windows: Mutex<Vec<WindowInfo>>,
    focused: Arc<Mutex<Vec<String>>>,
}

impl MockCompositor {
    fn new(windows: Vec<WindowInfo>) -> Self {
        Self {
            windows: Mutex::new(windows),
            focused: Arc::new(Mutex::new(Vec::new())),
        }
    }
}

fn win(native_id: &str, title: &str, app_id: &str) -> WindowInfo {
    WindowInfo {
        id: WindowId {
            native_id: native_id.into(),
            de_type: DesktopEnvironment::KDE,
        },
        title: title.into(),
        app_id: app_id.into(),
        pid: 1000,
        geometry: Rect {
            x: 10,
            y: 20,
            width: 800,
            height: 600,
        },
        frame_geometry: Rect {
            x: 8,
            y: 18,
            width: 804,
            height: 604,
        },
        states: vec![],
        workspace_id: None,
        monitor_id: None,
        stacking_order: 0,
        desktop_file: None,
        window_type: agent_shell_core::types::WindowType::Normal,
        icon_geometry: None,
        keep_above: false,
    }
}

#[async_trait]
impl DesktopComponent for MockCompositor {
    fn name(&self) -> &'static str {
        "mock-compositor"
    }
    fn component_type(&self) -> ComponentType {
        ComponentType::Compositor
    }
    fn is_available(&self) -> bool {
        true
    }
    async fn health(&self) -> ComponentHealth {
        ComponentHealth::Healthy
    }
}

#[async_trait]
impl CompositorComponent for MockCompositor {
    fn capabilities(&self) -> BackendCapabilities {
        BackendCapabilities::default()
    }
    async fn list_windows(&self) -> Result<Vec<WindowInfo>> {
        Ok(self.windows.lock().clone())
    }
    async fn get_active_window(&self) -> Result<Option<WindowInfo>> {
        Ok(self.windows.lock().first().cloned())
    }
    async fn focus_window(&self, id: &WindowId) -> Result<()> {
        self.focused.lock().push(id.native_id.clone());
        Ok(())
    }
    async fn move_window(&self, _id: &WindowId, _x: i32, _y: i32) -> Result<()> {
        Ok(())
    }
    async fn resize_window(&self, _id: &WindowId, _w: i32, _h: i32) -> Result<()> {
        Ok(())
    }
    async fn minimize_window(&self, _id: &WindowId) -> Result<()> {
        Ok(())
    }
    async fn unminimize_window(&self, _id: &WindowId) -> Result<()> {
        Ok(())
    }
    async fn maximize_window(&self, _id: &WindowId) -> Result<()> {
        Ok(())
    }
    async fn close_window(&self, _id: &WindowId) -> Result<()> {
        Ok(())
    }
    async fn set_window_geometry(&self, _id: &WindowId, _geo: Rect) -> Result<()> {
        Ok(())
    }
    async fn get_window_info(&self, id: &WindowId) -> Result<WindowInfo> {
        self.windows
            .lock()
            .iter()
            .find(|w| w.id == *id)
            .cloned()
            .ok_or_else(|| AgentShellError::WindowNotFound(id.native_id.clone()))
    }
    async fn list_workspaces(&self) -> Result<Vec<WorkspaceInfo>> {
        Ok(vec![])
    }
    async fn activate_workspace(&self, _id: &WorkspaceId) -> Result<()> {
        Ok(())
    }
    async fn move_window_to_workspace(&self, _wid: &WindowId, _ws: &WorkspaceId) -> Result<()> {
        Ok(())
    }
    async fn list_monitors(&self) -> Result<Vec<MonitorInfo>> {
        Ok(vec![])
    }
    async fn subscribe(&self) -> Result<Box<dyn agent_shell_core::event::EventStream>> {
        Err(AgentShellError::NotImplemented("mock event stream".into()))
    }
}

/// 合成器 mock：`list_windows` 恒失败——驱动失败路径的执行审计断言。
struct FailingListCompositor;

#[async_trait]
impl DesktopComponent for FailingListCompositor {
    fn name(&self) -> &'static str {
        "failing-list-compositor"
    }
    fn component_type(&self) -> ComponentType {
        ComponentType::Compositor
    }
    fn is_available(&self) -> bool {
        true
    }
    async fn health(&self) -> ComponentHealth {
        ComponentHealth::Healthy
    }
}

#[async_trait]
impl CompositorComponent for FailingListCompositor {
    fn capabilities(&self) -> BackendCapabilities {
        BackendCapabilities::default()
    }
    async fn list_windows(&self) -> Result<Vec<WindowInfo>> {
        Err(AgentShellError::BackendUnavailable(
            "mock compositor down".into(),
        ))
    }
    async fn get_active_window(&self) -> Result<Option<WindowInfo>> {
        Ok(None)
    }
    async fn focus_window(&self, _id: &WindowId) -> Result<()> {
        Ok(())
    }
    async fn move_window(&self, _id: &WindowId, _x: i32, _y: i32) -> Result<()> {
        Ok(())
    }
    async fn resize_window(&self, _id: &WindowId, _w: i32, _h: i32) -> Result<()> {
        Ok(())
    }
    async fn minimize_window(&self, _id: &WindowId) -> Result<()> {
        Ok(())
    }
    async fn unminimize_window(&self, _id: &WindowId) -> Result<()> {
        Ok(())
    }
    async fn maximize_window(&self, _id: &WindowId) -> Result<()> {
        Ok(())
    }
    async fn close_window(&self, _id: &WindowId) -> Result<()> {
        Ok(())
    }
    async fn set_window_geometry(&self, _id: &WindowId, _geo: Rect) -> Result<()> {
        Ok(())
    }
    async fn get_window_info(&self, id: &WindowId) -> Result<WindowInfo> {
        Err(AgentShellError::WindowNotFound(id.native_id.clone()))
    }
    async fn list_workspaces(&self) -> Result<Vec<WorkspaceInfo>> {
        Ok(vec![])
    }
    async fn activate_workspace(&self, _id: &WorkspaceId) -> Result<()> {
        Ok(())
    }
    async fn move_window_to_workspace(&self, _wid: &WindowId, _ws: &WorkspaceId) -> Result<()> {
        Ok(())
    }
    async fn list_monitors(&self) -> Result<Vec<MonitorInfo>> {
        Ok(vec![])
    }
    async fn subscribe(&self) -> Result<Box<dyn agent_shell_core::event::EventStream>> {
        Err(AgentShellError::NotImplemented("mock event stream".into()))
    }
}

// ───────────────────────── mock 输入 ─────────────────────────

#[derive(Default)]
struct MockInput {
    typed: Mutex<Vec<String>>,
    sent: Mutex<usize>,
}

#[async_trait]
impl InputDispatcher for MockInput {
    async fn type_text(&self, text: &str, _delay_us: u64) -> Result<()> {
        self.typed.lock().push(text.to_string());
        Ok(())
    }
    async fn send_key_combo(&self, _keys: &[Key]) -> Result<()> {
        *self.sent.lock() += 1;
        Ok(())
    }
    async fn mouse_move(&self, _x: i32, _y: i32) -> Result<()> {
        Ok(())
    }
    async fn mouse_click(&self, _button: MouseButton) -> Result<()> {
        Ok(())
    }
    async fn scroll(&self, _dx: i32, _dy: i32) -> Result<()> {
        Ok(())
    }
}

// ───────────────────────── mock 截图 ─────────────────────────

struct MockCapture;

#[async_trait]
impl CaptureDispatcher for MockCapture {
    async fn capture_png(&self, target: &CaptureTarget) -> Result<Vec<u8>> {
        assert!(matches!(target, CaptureTarget::Screen));
        Ok(b"png-bytes".to_vec())
    }
}

// ───────────────────────── mock a11y ─────────────────────────

/// set_text 永远失败的元素——驱动 TypeText 降级路径。
struct FailingElement;

#[async_trait]
impl A11yElement for FailingElement {
    async fn set_text(&self, _text: &str) -> Result<()> {
        Err(AgentShellError::Input("set_text rejected".into()))
    }
    async fn text(&self) -> Result<String> {
        Ok(String::new())
    }
    async fn find_parent_window_name(&self) -> Result<String> {
        Ok("mock-window".into())
    }
}

/// 定位成功但 set_text 失败的 a11y 封装。
struct FailingSetTextA11y;

#[async_trait]
impl ElementActions for FailingSetTextA11y {
    async fn locate(&self, _target: &SemanticTarget) -> Result<Vec<Arc<dyn A11yElement>>> {
        Ok(vec![Arc::new(FailingElement)])
    }
}

// ───────────────────────── 构造辅助 ─────────────────────────

fn default_security() -> Arc<SecurityManager> {
    Arc::new(SecurityManager::with_config(AgentShellConfig::default()))
}

fn make_executor(
    compositor: MockCompositor,
) -> (Executor, Arc<Mutex<Vec<String>>>, Arc<MockInput>) {
    let input = Arc::new(MockInput::default());
    let focused = compositor.focused.clone();
    let ex = Executor::new(
        Box::new(compositor),
        input.clone(),
        Arc::new(MockCapture),
        Arc::new(FailingSetTextA11y),
        default_security(),
    );
    (ex, focused, input)
}

// ───────────────────────── FocusWindow(ByAppId) ─────────────────────────

#[tokio::test]
async fn focus_window_by_app_id_hits_and_focuses() {
    let comp = MockCompositor::new(vec![
        win("w1", "Editor", "org.kde.kate"),
        win("w2", "终端", "konsole"),
    ]);
    let (ex, focused, _) = make_executor(comp);

    let res = ex
        .execute(Command::FocusWindow {
            target: SemanticTarget::ByAppId("konsole".into()),
        })
        .await
        .expect("focus should succeed");

    match res {
        CommandResult::Window(w) => {
            assert_eq!(w.id.native_id, "w2");
            assert_eq!(w.title, "终端");
        }
        other => panic!("expected Window, got {other:?}"),
    }
    assert_eq!(focused.lock().as_slice(), ["w2"]);
}

#[tokio::test]
async fn focus_window_by_app_id_miss_returns_window_not_found() {
    let comp = MockCompositor::new(vec![win("w1", "Editor", "kate")]);
    let (ex, _, _) = make_executor(comp);

    let err = ex
        .execute(Command::FocusWindow {
            target: SemanticTarget::ByAppId("nonexistent".into()),
        })
        .await
        .expect_err("should fail");

    assert!(
        matches!(&err, AgentShellError::WindowNotFound(m) if m.contains("nonexistent")),
        "got {err:?}"
    );
}

// ───────────────────────── ByTitle 四种模式正反例 ─────────────────────────

#[tokio::test]
async fn by_title_four_match_modes_positive_and_negative() {
    let windows = vec![
        win("a", "Untitled Document — Kate", "kate"),
        win("b", "终端 — Konsole", "konsole"),
    ];

    struct Case {
        pattern: &'static str,
        mode: TitleMatchMode,
        hit: &'static str,
        miss_pattern: &'static str,
    }
    let cases = [
        Case {
            pattern: "Konsole",
            mode: TitleMatchMode::Substring,
            hit: "b",
            miss_pattern: "GIMP",
        },
        Case {
            pattern: "终端 — Konsole",
            mode: TitleMatchMode::Exact,
            hit: "b",
            miss_pattern: "终端",
        },
        Case {
            pattern: "^Unt.*Kate$",
            mode: TitleMatchMode::Regex,
            hit: "a",
            miss_pattern: "^\\d+$",
        },
        Case {
            pattern: "*Document*",
            mode: TitleMatchMode::Glob,
            hit: "a",
            miss_pattern: "*.pdf",
        },
    ];

    for c in cases {
        let (ex, _, _) = make_executor(MockCompositor::new(windows.clone()));
        let got = ex
            .resolve_target(&SemanticTarget::ByTitle(c.pattern.into(), c.mode))
            .await
            .unwrap_or_else(|e| panic!("'{}' ({:?}) should hit: {e}", c.pattern, c.mode));
        assert_eq!(got.id.native_id, c.hit, "'{}' {:?}", c.pattern, c.mode);

        let err = ex
            .resolve_target(&SemanticTarget::ByTitle(c.miss_pattern.into(), c.mode))
            .await
            .expect_err("negative case should miss");
        assert!(
            matches!(err, AgentShellError::WindowNotFound(_)),
            "got {err:?}"
        );
    }
}

#[tokio::test]
async fn regex_invalid_expression_no_panic_treated_as_mismatch() {
    // 非法正则 `([` 不 panic，按「不匹配」处理 → WindowNotFound。
    let comp = MockCompositor::new(vec![win("a", "anything", "app")]);
    let (ex, _, _) = make_executor(comp);

    let result = ex
        .resolve_target(&SemanticTarget::ByTitle(
            "([ bad".into(),
            TitleMatchMode::Regex,
        ))
        .await;

    match result {
        Err(AgentShellError::WindowNotFound(m)) => assert!(m.contains("([")),
        other => panic!("expected WindowNotFound, got {other:?}"),
    }
}

// ───────────────────────── WaitForWindow ─────────────────────────

#[tokio::test]
async fn wait_for_window_times_out_when_absent() {
    // 步进 100ms，250ms 预算内至少轮询两次，保证覆盖「出现前」路径。
    assert!(Duration::from_millis(250) > _POLL_STEP_WITNESS);
    let comp = MockCompositor::new(vec![win("a", "other", "other-app")]);
    let (ex, _, _) = make_executor(comp);

    let err = ex
        .execute(Command::WaitForWindow {
            app_id: "ghost".into(),
            timeout: Duration::from_millis(250),
        })
        .await
        .expect_err("absent window should time out");

    assert!(
        matches!(&err, AgentShellError::Timeout(m) if m.contains("ghost")),
        "got {err:?}"
    );
}

#[tokio::test]
async fn wait_for_window_succeeds_immediately_when_present() {
    let comp = MockCompositor::new(vec![win("a", "term", "konsole")]);
    let (ex, _, _) = make_executor(comp);

    let res = ex
        .execute(Command::WaitForWindow {
            app_id: "konsole".into(),
            timeout: Duration::from_secs(1),
        })
        .await
        .expect("present window should succeed immediately");

    assert!(matches!(res, CommandResult::Success));
}

/// 可编程 mock：list_windows 结果在 N 次调用后切换——验证「出现即成功」。
struct AppearingCompositor {
    inner: MockCompositor,
    calls: AtomicUsize,
    appear_after: usize,
}

#[async_trait]
impl DesktopComponent for AppearingCompositor {
    fn name(&self) -> &'static str {
        "appearing-compositor"
    }
    fn component_type(&self) -> ComponentType {
        ComponentType::Compositor
    }
    fn is_available(&self) -> bool {
        true
    }
    async fn health(&self) -> ComponentHealth {
        ComponentHealth::Healthy
    }
}

#[async_trait]
impl CompositorComponent for AppearingCompositor {
    fn capabilities(&self) -> BackendCapabilities {
        BackendCapabilities::default()
    }
    async fn list_windows(&self) -> Result<Vec<WindowInfo>> {
        let n = self.calls.fetch_add(1, Ordering::SeqCst) + 1;
        if n >= self.appear_after {
            Ok(vec![win("late", "late term", "konsole")])
        } else {
            self.inner.list_windows().await
        }
    }
    async fn get_active_window(&self) -> Result<Option<WindowInfo>> {
        self.inner.get_active_window().await
    }
    async fn focus_window(&self, id: &WindowId) -> Result<()> {
        self.inner.focus_window(id).await
    }
    async fn move_window(&self, id: &WindowId, x: i32, y: i32) -> Result<()> {
        self.inner.move_window(id, x, y).await
    }
    async fn resize_window(&self, id: &WindowId, w: i32, h: i32) -> Result<()> {
        self.inner.resize_window(id, w, h).await
    }
    async fn minimize_window(&self, id: &WindowId) -> Result<()> {
        self.inner.minimize_window(id).await
    }
    async fn unminimize_window(&self, id: &WindowId) -> Result<()> {
        self.inner.unminimize_window(id).await
    }
    async fn maximize_window(&self, id: &WindowId) -> Result<()> {
        self.inner.maximize_window(id).await
    }
    async fn close_window(&self, id: &WindowId) -> Result<()> {
        self.inner.close_window(id).await
    }
    async fn set_window_geometry(&self, id: &WindowId, geo: Rect) -> Result<()> {
        self.inner.set_window_geometry(id, geo).await
    }
    async fn get_window_info(&self, id: &WindowId) -> Result<WindowInfo> {
        self.inner.get_window_info(id).await
    }
    async fn list_workspaces(&self) -> Result<Vec<WorkspaceInfo>> {
        self.inner.list_workspaces().await
    }
    async fn activate_workspace(&self, id: &WorkspaceId) -> Result<()> {
        self.inner.activate_workspace(id).await
    }
    async fn move_window_to_workspace(&self, wid: &WindowId, ws: &WorkspaceId) -> Result<()> {
        self.inner.move_window_to_workspace(wid, ws).await
    }
    async fn list_monitors(&self) -> Result<Vec<MonitorInfo>> {
        self.inner.list_monitors().await
    }
    async fn subscribe(&self) -> Result<Box<dyn agent_shell_core::event::EventStream>> {
        self.inner.subscribe().await
    }
}

#[tokio::test]
async fn wait_for_window_succeeds_once_window_appears() {
    let comp = AppearingCompositor {
        inner: MockCompositor::new(vec![win("x", "other", "other")]),
        calls: AtomicUsize::new(0),
        appear_after: 3,
    };
    let input = Arc::new(MockInput::default());
    let ex = Executor::new(
        Box::new(comp),
        input.clone(),
        Arc::new(MockCapture),
        Arc::new(FailingSetTextA11y),
        default_security(),
    );

    let res = ex
        .execute(Command::WaitForWindow {
            app_id: "konsole".into(),
            timeout: Duration::from_secs(5),
        })
        .await
        .expect("window appears after 3 polls");

    assert!(matches!(res, CommandResult::Success));
}

// ───────────────────────── TypeText 降级 ─────────────────────────

#[tokio::test]
async fn type_text_falls_back_to_input_when_a11y_set_text_fails() {
    let comp = MockCompositor::new(vec![win("a", "t", "app")]);
    let input = Arc::new(MockInput::default());
    let ex = Executor::new(
        Box::new(comp),
        input.clone(),
        Arc::new(MockCapture),
        Arc::new(FailingSetTextA11y),
        default_security(),
    );

    let res = ex
        .execute(Command::TypeText {
            text: "hello fallback".into(),
            target: Some(SemanticTarget::ByTitle("t".into(), TitleMatchMode::Exact)),
        })
        .await
        .expect("degraded type should succeed");

    assert!(matches!(res, CommandResult::Success));
    assert_eq!(input.typed.lock().as_slice(), ["hello fallback"]);
}

#[tokio::test]
async fn denied_command_short_circuits_before_execution() {
    // 黑名单命中 → execute 入口返回 Permission，不触达 backend。
    let mut config = AgentShellConfig::default();
    config.permissions.deny.insert("input.send".into(), true);
    let security = Arc::new(SecurityManager::with_config(config));

    let comp = MockCompositor::new(vec![win("w", "t", "app")]);
    let input = Arc::new(MockInput::default());
    let ex = Executor::new(
        Box::new(comp),
        input.clone(),
        Arc::new(MockCapture),
        Arc::new(FailingSetTextA11y),
        security,
    );

    let err = ex
        .execute(Command::SendKey {
            combo: KeyCombo {
                keys: vec![Key::Char('a')],
                modifiers: Default::default(),
            },
        })
        .await
        .expect_err("denied command must fail");
    assert!(matches!(err, AgentShellError::Permission(_)));
    assert_eq!(*input.sent.lock(), 0, "no key must be injected");
}

#[tokio::test]
async fn confirm_override_returns_confirmation_required() {
    // 操作确认覆盖命中 → 纯后端阶段返回 ConfirmationRequired 占位。
    let mut config = AgentShellConfig::default();
    config
        .operations
        .confirm
        .insert("input.send".into(), ConfirmMode::Always);
    let security = Arc::new(SecurityManager::with_config(config));

    let comp = MockCompositor::new(vec![win("w", "t", "app")]);
    let ex = Executor::new(
        Box::new(comp),
        Arc::new(MockInput::default()),
        Arc::new(MockCapture),
        Arc::new(FailingSetTextA11y),
        security,
    );

    let err = ex
        .execute(Command::SendKey {
            combo: KeyCombo {
                keys: vec![Key::Char('b')],
                modifiers: Default::default(),
            },
        })
        .await
        .expect_err("confirm override must gate");
    assert!(matches!(err, AgentShellError::ConfirmationRequired(_)));
}

#[tokio::test]
async fn allow_command_records_execution_outcome() {
    // TSI-2659 回归锚定：router 门禁放行后须在命令返回后落执行结果审计——
    // 门禁判定记录 result=false + 执行结果记录 result=true，二者独立有序。
    let audit_path = std::env::temp_dir().join(format!(
        "agent-shell-router-audit-{}.jsonl",
        std::process::id()
    ));
    let _ = std::fs::remove_file(&audit_path);
    let mut cfg = AgentShellConfig::default();
    cfg.security.audit_log_path = Some(audit_path.to_string_lossy().into_owned());
    let security = Arc::new(SecurityManager::with_config(cfg));

    let ex = Executor::new(
        Box::new(MockCompositor::new(vec![])),
        Arc::new(MockInput::default()),
        Arc::new(MockCapture),
        Arc::new(FailingSetTextA11y),
        security.clone(),
    );

    ex.execute(Command::GetDesktopInfo)
        .await
        .expect("system.desktop_info must succeed");

    let op_entries: Vec<_> = security
        .audit
        .read_all()
        .into_iter()
        .filter(|e| e.op == "system.desktop_info")
        .collect();
    assert_eq!(
        op_entries.len(),
        2,
        "gate allow + execution outcome: {op_entries:?}"
    );
    assert!(
        op_entries.iter().any(|e| !e.result),
        "gate record must be result=false: {op_entries:?}"
    );
    assert!(
        op_entries.iter().any(|e| e.result),
        "execution record must be result=true: {op_entries:?}"
    );
    let _ = std::fs::remove_file(&audit_path);
}

#[tokio::test]
async fn allow_command_failure_still_records_execution_outcome() {
    // TSI-2659 复审：`?` 提前返回不得绕过执行结果审计——失败路径同样落
    // 门禁 allow+false + 执行结果 false 两条记录，与 daemon 侧契约一致。
    let audit_path = std::env::temp_dir().join(format!(
        "agent-shell-router-audit-fail-{}.jsonl",
        std::process::id()
    ));
    let _ = std::fs::remove_file(&audit_path);
    let mut cfg = AgentShellConfig::default();
    cfg.security.audit_log_path = Some(audit_path.to_string_lossy().into_owned());
    let security = Arc::new(SecurityManager::with_config(cfg));

    let ex = Executor::new(
        Box::new(FailingListCompositor),
        Arc::new(MockInput::default()),
        Arc::new(MockCapture),
        Arc::new(FailingSetTextA11y),
        security.clone(),
    );

    let err = ex
        .execute(Command::ListWindows { filter: None })
        .await
        .expect_err("backend failure must propagate");
    assert!(matches!(err, AgentShellError::BackendUnavailable(_)));

    let op_entries: Vec<_> = security
        .audit
        .read_all()
        .into_iter()
        .filter(|e| e.op == "windows.list")
        .collect();
    assert_eq!(
        op_entries.len(),
        2,
        "gate allow + failed execution outcome: {op_entries:?}"
    );
    assert!(
        op_entries.iter().all(|e| !e.result),
        "both records must be result=false on failure: {op_entries:?}"
    );
    let _ = std::fs::remove_file(&audit_path);
}

#[tokio::test]
async fn resolve_target_by_id_direct_query() {
    let comp = MockCompositor::new(vec![win("direct", "d", "app")]);
    let (ex, _, _) = make_executor(comp);

    let got = ex
        .resolve_target(&SemanticTarget::ById(WindowId {
            native_id: "direct".into(),
            de_type: DesktopEnvironment::KDE,
        }))
        .await
        .expect("ById direct query");

    assert_eq!(got.app_id, "app");
}
