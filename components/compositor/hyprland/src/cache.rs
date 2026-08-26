//! 窗口缓存（设计文档 §9.5：事件流持续更新，查询优先读缓存）。
//!
//! hyprctl `clients -j` 是全量快照；`.socket2.sock` 事件是增量。缓存把
//! 两者接起来：快照填充 + 增量维护（open/close/focus/fullscreen），
//! `CompositorComponent` 查询方法在缓存非空时零 IPC 返回。
//!
//! 一致性策略：closewindow 事件直接移除条目；openwindow 以最小信息落位
//! （title/class/workspace 已知，geometry 待下一次快照校准）；`refresh`
//! 用 clients -j 快照整体替换并去重。

use std::sync::Arc;

use agent_shell_core::types::{Rect, WindowId, WindowInfo, WindowState, WindowType};
use agent_shell_core::DesktopEnvironment;

/// 线程安全的窗口缓存句柄（事件 task 与组件查询共享）。
#[derive(Clone, Default)]
pub struct WindowCache {
    inner: Arc<parking_lot::RwLock<Vec<WindowInfo>>>,
}

impl std::fmt::Debug for WindowCache {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        let guard = self.inner.read();
        f.debug_struct("WindowCache")
            .field("len", &guard.len())
            .finish()
    }
}

impl WindowCache {
    /// 全量替换（clients -j 快照校准）。
    pub fn replace_all(&self, windows: Vec<WindowInfo>) {
        *self.inner.write() = windows;
    }

    /// 当前快照（克隆；查询路径的拷贝成本可接受——窗口数 ≤ 数十）。
    pub fn snapshot(&self) -> Vec<WindowInfo> {
        self.inner.read().clone()
    }

    /// 按 id 查找（克隆单个条目）。
    pub fn get(&self, id: &WindowId) -> Option<WindowInfo> {
        self.inner.read().iter().find(|w| &w.id == id).cloned()
    }

    /// 焦点窗口（stacking_order 最大者；Hyprland 无显式 focus 位，
    /// activewindowv2 事件只提升 z 序语义由快照保证）。
    pub fn focused(&self) -> Option<WindowInfo> {
        self.inner
            .read()
            .iter()
            .max_by_key(|w| w.stacking_order)
            .cloned()
    }

    /// 窗口打开（openwindow 事件）：已存在则更新标题/工作区，否则插入。
    pub fn upsert_minimal(
        &self,
        address: &str,
        workspace: u32,
        class: &str,
        title: &str,
        stacking_order: u32,
    ) {
        let id = window_id(address);
        let mut guard = self.inner.write();
        if let Some(existing) = guard.iter_mut().find(|w| w.id == id) {
            if !title.is_empty() {
                existing.title = title.to_string();
            }
            existing.app_id = class.to_string();
            return;
        }
        guard.push(WindowInfo {
            id,
            title: title.to_string(),
            app_id: class.to_string(),
            pid: 0,
            geometry: Rect::default(),
            frame_geometry: Rect::default(),
            states: Vec::new(),
            workspace_id: Some(agent_shell_core::types::WorkspaceId {
                native_id: workspace.to_string(),
                de_type: DesktopEnvironment::Hyprland,
            }),
            monitor_id: None,
            stacking_order,
            desktop_file: None,
            window_type: WindowType::Normal,
            icon_geometry: None,
            keep_above: false,
        });
    }

    /// 窗口关闭（closewindow 事件）：移除条目。
    pub fn remove(&self, address: &str) {
        let id = window_id(address);
        self.inner.write().retain(|w| w.id != id);
    }

    /// 全屏状态变化（fullscreen 事件）：切换目标窗口 FullScreen 态。
    ///
    /// Hyprland 的 fullscreen 事件不带地址（广播当前焦点窗口），调用方
    /// 传入事件时刻的焦点地址；`on` 为 None 表示退出全屏。
    pub fn set_fullscreen(&self, address: &str, on: bool) {
        let id = window_id(address);
        let mut guard = self.inner.write();
        if let Some(w) = guard.iter_mut().find(|w| w.id == id) {
            w.states.retain(|s| *s != WindowState::FullScreen);
            if on {
                w.states.push(WindowState::FullScreen);
            }
        }
    }

    /// 缓存条目数（doctor 输出）。
    pub fn len(&self) -> usize {
        self.inner.read().len()
    }

    /// 缓存是否为空。
    pub fn is_empty(&self) -> bool {
        self.inner.read().is_empty()
    }
}

/// Hyprland 窗口地址 → [`WindowId`]（native_id 统一保留十六进制文本）。
pub fn window_id(address: &str) -> WindowId {
    WindowId {
        native_id: address.trim_start_matches("0x").to_string(),
        de_type: DesktopEnvironment::Hyprland,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const ADDR: &str = "abc123";

    #[test]
    fn upsert_then_remove_lifecycle() {
        let cache = WindowCache::default();
        assert!(cache.is_empty());
        cache.upsert_minimal(ADDR, 2, "foot", "terminal", 1);
        assert_eq!(cache.len(), 1);
        // 同地址二次 openwindow 是更新而非重复插入。
        cache.upsert_minimal(ADDR, 2, "foot", "vim", 2);
        assert_eq!(cache.len(), 1);
        let w = cache.get(&window_id(ADDR)).expect("present");
        assert_eq!(w.title, "vim");
        assert_eq!(w.workspace_id.unwrap().native_id, "2");
        cache.remove(ADDR);
        assert!(cache.is_empty());
        assert_eq!(window_id("0xabc123"), window_id(ADDR));
    }

    #[test]
    fn fullscreen_toggle_and_focus_pick() {
        let cache = WindowCache::default();
        cache.upsert_minimal("aa01", 1, "a", "A", 1);
        cache.upsert_minimal("bb02", 1, "b", "B", 5);
        assert_eq!(
            cache.focused().unwrap().id.native_id,
            "bb02",
            "stacking_order 最大者为焦点"
        );
        cache.set_fullscreen("aa01", true);
        let a = cache.get(&window_id("aa01")).unwrap();
        assert!(a.states.contains(&WindowState::FullScreen));
        cache.set_fullscreen("aa01", false);
        let a = cache.get(&window_id("aa01")).unwrap();
        assert!(!a.states.contains(&WindowState::FullScreen));
    }

    #[test]
    fn replace_all_reconciles_stale_entries() {
        let cache = WindowCache::default();
        cache.upsert_minimal("dead", 1, "x", "gone", 1);
        cache.upsert_minimal(ADDR, 1, "live", "Live", 2);
        let live = WindowInfo {
            id: window_id(ADDR),
            title: "Live".into(),
            app_id: "live".into(),
            pid: 42,
            geometry: Rect {
                x: 10,
                y: 20,
                width: 800,
                height: 600,
            },
            frame_geometry: Rect::default(),
            states: vec![],
            workspace_id: None,
            monitor_id: None,
            stacking_order: 7,
            desktop_file: None,
            window_type: WindowType::Normal,
            icon_geometry: None,
            keep_above: false,
        };
        cache.replace_all(vec![live.clone()]);
        assert_eq!(cache.len(), 1, "快照替换清掉失效条目");
        assert_eq!(cache.get(&window_id(ADDR)).unwrap().pid, 42);
    }
}
