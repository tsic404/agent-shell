//! 脚本化合成器：无图形 CI 下驱动窗口、工作区与 doctor 语义。
//!
//! 组件以 `Daemon::install_acceptance_compositor` 注入，之后所有请求仍走生产
//! `dispatch` 与 daemon 状态机（缓存、过滤、目标解析、错误映射）；本类型只
//! 替代桌面协议通道，不复制被测逻辑。

use std::sync::{Arc, Mutex};

use agent_shell_core::component::{
    BackendCapabilities, ComponentHealth, ComponentType, CompositorComponent, DesktopComponent,
};
use agent_shell_core::error::{AgentShellError, Result};
use agent_shell_core::event::{DesktopEvent, EventStream};
use agent_shell_core::types::{
    DesktopEnvironment, Rect, WindowId, WindowInfo, WindowState, WindowType, WorkspaceId,
    WorkspaceInfo,
};
use async_trait::async_trait;

/// 合成器侧可变状态：窗口、工作区与调用记录。
#[derive(Default)]
pub struct FakeDesktop {
    /// 当前窗口集（列表与写操作都以此为准）。
    pub windows: Vec<WindowInfo>,
    /// 当前工作区集。
    pub workspaces: Vec<WorkspaceInfo>,
    /// 后端调用记录（`focus:w1`、`move:w1:10,20`、`activate:2`）。
    pub calls: Vec<String>,
    /// 事件流订阅次数（懒启动装配的观测点）。
    pub subscribe_calls: usize,
    /// 最近一次聚焦的窗口。
    pub focused: Option<String>,
}

/// 可克隆的合成器句柄：注入 daemon 后测试仍可读写同一份状态。
#[derive(Clone, Default)]
pub struct FakeCompositor {
    state: Arc<Mutex<FakeDesktop>>,
}

impl FakeCompositor {
    /// 空桌面。
    pub fn new() -> Self {
        Self::default()
    }

    /// 带初始窗口的桌面。
    pub fn with_windows(windows: Vec<WindowInfo>) -> Self {
        let this = Self::default();
        this.state.lock().expect("fake state").windows = windows;
        this
    }

    /// 共享状态句柄（测试读写窗口与调用记录）。
    pub fn state(&self) -> Arc<Mutex<FakeDesktop>> {
        Arc::clone(&self.state)
    }

    /// 最近一次调用记录快照。
    pub fn calls(&self) -> Vec<String> {
        self.state.lock().expect("fake state").calls.clone()
    }

    fn lock(&self) -> std::sync::MutexGuard<'_, FakeDesktop> {
        self.state.lock().expect("fake state")
    }

    fn find(&self, id: &str) -> Result<WindowInfo> {
        self.lock()
            .windows
            .iter()
            .find(|w| w.id.native_id == id)
            .cloned()
            .ok_or_else(|| AgentShellError::WindowNotFound(format!("window not found: {id:?}")))
    }
}

/// 窗口条目构造：几何与边框几何一致，字段取设计载荷里的完整形状。
pub fn window(
    native_id: &str,
    title: &str,
    app_id: &str,
    x: i32,
    y: i32,
    width: i32,
    height: i32,
) -> WindowInfo {
    let geometry = Rect {
        x,
        y,
        width,
        height,
    };
    WindowInfo {
        id: WindowId {
            native_id: native_id.into(),
            de_type: DesktopEnvironment::KDE,
        },
        title: title.into(),
        app_id: app_id.into(),
        pid: 1000 + native_id.len() as u32,
        geometry,
        frame_geometry: geometry,
        states: Vec::new(),
        workspace_id: None,
        monitor_id: None,
        stacking_order: 0,
        desktop_file: None,
        window_type: WindowType::Normal,
        icon_geometry: None,
        keep_above: false,
    }
}

/// 工作区条目构造（序号从 1 开始，`is_active` 由调用方指定）。
pub fn workspace(native_id: &str, name: &str, number: u32, is_active: bool) -> WorkspaceInfo {
    WorkspaceInfo {
        id: WorkspaceId {
            native_id: native_id.into(),
            de_type: DesktopEnvironment::KDE,
        },
        name: name.into(),
        number,
        is_active,
        monitor_ids: Vec::new(),
        window_ids: Vec::new(),
    }
}

#[async_trait]
impl DesktopComponent for FakeCompositor {
    fn name(&self) -> &'static str {
        "fake-desktop"
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
impl CompositorComponent for FakeCompositor {
    fn capabilities(&self) -> BackendCapabilities {
        BackendCapabilities {
            window_management: true,
            workspace_management: true,
            monitor_layout: false,
            window_events: true,
            workspace_events: true,
            native_input: false,
            native_capture: false,
            virtual_desktops: true,
            effects_control: false,
        }
    }

    fn lazy_capabilities(&self) -> &'static [&'static str] {
        &["window_events", "workspace_events"]
    }

    async fn list_windows(&self) -> Result<Vec<WindowInfo>> {
        Ok(self.lock().windows.clone())
    }

    async fn get_active_window(&self) -> Result<Option<WindowInfo>> {
        let focused = self.lock().focused.clone();
        Ok(focused.and_then(|id| {
            self.lock()
                .windows
                .iter()
                .find(|w| w.id.native_id == id)
                .cloned()
        }))
    }

    async fn focus_window(&self, id: &WindowId) -> Result<()> {
        self.find(&id.native_id)?;
        let mut state = self.lock();
        state.focused = Some(id.native_id.clone());
        state.calls.push(format!("focus:{}", id.native_id));
        Ok(())
    }

    async fn move_window(&self, id: &WindowId, x: i32, y: i32) -> Result<()> {
        self.find(&id.native_id)?;
        let mut state = self.lock();
        if let Some(w) = state
            .windows
            .iter_mut()
            .find(|w| w.id.native_id == id.native_id)
        {
            w.geometry.x = x;
            w.geometry.y = y;
            w.frame_geometry.x = x;
            w.frame_geometry.y = y;
        }
        state.calls.push(format!("move:{}:{x},{y}", id.native_id));
        Ok(())
    }

    async fn resize_window(&self, id: &WindowId, w: i32, h: i32) -> Result<()> {
        self.find(&id.native_id)?;
        let mut state = self.lock();
        if let Some(win) = state
            .windows
            .iter_mut()
            .find(|w| w.id.native_id == id.native_id)
        {
            win.geometry.width = w;
            win.geometry.height = h;
            win.frame_geometry.width = w;
            win.frame_geometry.height = h;
        }
        state.calls.push(format!("resize:{}:{w},{h}", id.native_id));
        Ok(())
    }

    async fn minimize_window(&self, id: &WindowId) -> Result<()> {
        self.find(&id.native_id)?;
        let mut state = self.lock();
        if let Some(w) = state
            .windows
            .iter_mut()
            .find(|w| w.id.native_id == id.native_id)
        {
            if !w.states.contains(&WindowState::Minimized) {
                w.states.push(WindowState::Minimized);
            }
        }
        state.calls.push(format!("minimize:{}", id.native_id));
        Ok(())
    }

    async fn unminimize_window(&self, id: &WindowId) -> Result<()> {
        self.find(&id.native_id)?;
        let mut state = self.lock();
        if let Some(w) = state
            .windows
            .iter_mut()
            .find(|w| w.id.native_id == id.native_id)
        {
            w.states.retain(|s| *s != WindowState::Minimized);
        }
        state.calls.push(format!("restore:{}", id.native_id));
        Ok(())
    }

    async fn maximize_window(&self, id: &WindowId) -> Result<()> {
        self.find(&id.native_id)?;
        self.lock().calls.push(format!("maximize:{}", id.native_id));
        Ok(())
    }

    async fn close_window(&self, id: &WindowId) -> Result<()> {
        self.find(&id.native_id)?;
        let mut state = self.lock();
        state.windows.retain(|w| w.id.native_id != id.native_id);
        state.calls.push(format!("close:{}", id.native_id));
        Ok(())
    }

    async fn set_window_geometry(&self, id: &WindowId, geo: Rect) -> Result<()> {
        self.find(&id.native_id)?;
        let mut state = self.lock();
        if let Some(w) = state
            .windows
            .iter_mut()
            .find(|w| w.id.native_id == id.native_id)
        {
            w.geometry = geo;
            w.frame_geometry = geo;
        }
        state.calls.push(format!("geometry:{}", id.native_id));
        Ok(())
    }

    async fn get_window_info(&self, id: &WindowId) -> Result<WindowInfo> {
        self.find(&id.native_id)
    }

    async fn list_workspaces(&self) -> Result<Vec<WorkspaceInfo>> {
        Ok(self.lock().workspaces.clone())
    }

    async fn activate_workspace(&self, id: &WorkspaceId) -> Result<()> {
        let mut state = self.lock();
        if !state
            .workspaces
            .iter()
            .any(|w| w.id.native_id == id.native_id)
        {
            return Err(AgentShellError::WindowNotFound(format!(
                "workspace not found: {:?}",
                id.native_id
            )));
        }
        for ws in &mut state.workspaces {
            ws.is_active = ws.id.native_id == id.native_id;
        }
        state.calls.push(format!("activate:{}", id.native_id));
        Ok(())
    }

    async fn move_window_to_workspace(&self, wid: &WindowId, ws: &WorkspaceId) -> Result<()> {
        self.lock()
            .calls
            .push(format!("move-ws:{}:{}", wid.native_id, ws.native_id));
        Ok(())
    }

    async fn list_monitors(&self) -> Result<Vec<agent_shell_core::types::MonitorInfo>> {
        Ok(Vec::new())
    }

    async fn subscribe(&self) -> Result<Box<dyn EventStream>> {
        self.lock().subscribe_calls += 1;
        Ok(Box::new(EmptyStream))
    }
}

/// 空事件流：daemon 的原始事件源由夹具的 `RawSource` 提供，本流只满足契约。
struct EmptyStream;

#[async_trait]
impl EventStream for EmptyStream {
    async fn next_event(&self) -> Option<DesktopEvent> {
        None
    }
}
