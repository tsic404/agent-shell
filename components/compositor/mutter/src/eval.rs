//! org.gnome.Shell.Eval 路径（设计文档 §8.1 路径 A / §8.2，GNOME <47）。
//!
//! Eval 在 Shell 进程内执行 JS，返回 `(success: bool, result: s)`，result 是
//! `JSON.stringify(...)` 字符串（zbus 反序列化后即裸 JSON 文本）。限制：GNOME 41+
//! 默认禁用（unsafe-mode）、47+ 收紧为不可开启；本模块不改用户设置，探测失败由
//! 上层降级到 Extension。

use serde_json::Value;
use zbus::Connection;

use crate::error::{eval_disabled_hint, MutterError, Result, SHELL_PATH, SHELL_SERVICE};
use crate::version::GnomeVersion;

/// org.gnome.Shell 根接口 proxy（Eval + ShellVersion 属性）。
#[zbus::proxy(
    default_service = "org.gnome.Shell",
    default_path = "/org/gnome/Shell",
    interface = "org.gnome.Shell"
)]
trait Shell {
    fn eval(&self, script: &str) -> zbus::Result<(bool, String)>;
    #[zbus(property)]
    fn shell_version(&self) -> zbus::Result<String>;
}

/// Eval 返回的窗口 JSON 键 → core `WindowInfo` 归一化。
pub(crate) mod parse {
    use agent_shell_core::types::{
        DesktopEnvironment, Rect, WindowId, WindowInfo, WindowState, WindowType, WorkspaceId,
    };
    use serde_json::Value;

    /// 把 Eval/Extension 返回的单窗 JSON 归一化为 core `WindowInfo`。
    ///
    /// GNOME 侧字段：`id`(number|string) / `title` / `appId` / `pid` /
    /// `geometry{x,y,width,height}`（用户可见窗口边缘）/ `frameGeometry`(可选) /
    /// `workspace`(0 基序号，null=全部工作区可见) / `windowType`（契约
    /// 枚举名）/ `keepAbove` / `hasFocus` / `minimized` / `maximized`。
    /// 缺字段按默认值兜底（旧版载荷无后三字段）。
    pub(crate) fn window(v: &Value, stacking_order: u32) -> Option<WindowInfo> {
        let id = match v.get("id") {
            Some(Value::Number(n)) => n.to_string(),
            Some(Value::String(s)) => s.clone(),
            _ => return None,
        };
        let rect = |key: &str| {
            let g = v.get(key).cloned().unwrap_or(Value::Null);
            Rect {
                x: num(&g, "x"),
                y: num(&g, "y"),
                width: num(&g, "width"),
                height: num(&g, "height"),
            }
        };
        let mut states = Vec::new();
        if v.get("minimized").and_then(Value::as_bool).unwrap_or(false) {
            states.push(WindowState::Minimized);
        }
        if v.get("maximized").and_then(Value::as_bool).unwrap_or(false) {
            states.push(WindowState::Maximized);
        }
        if v.get("fullscreen")
            .and_then(Value::as_bool)
            .unwrap_or(false)
        {
            states.push(WindowState::FullScreen);
        }
        if states.is_empty() {
            states.push(WindowState::Normal);
        }
        let content = rect("geometry");
        let frame = rect("frameGeometry");
        // frameGeometry 缺失时退回内容几何——两者至少其一由脚本保证。
        let frame_geometry = if frame == Rect::default() && content != Rect::default() {
            content
        } else {
            frame
        };
        // 0 基工作区序号；null/缺字段 = 全部工作区可见或旧版载荷未携带。
        let workspace_id = v
            .get("workspace")
            .and_then(Value::as_i64)
            .map(|index| WorkspaceId {
                native_id: index.to_string(),
                de_type: DesktopEnvironment::GNOME,
            });
        Some(WindowInfo {
            id: WindowId {
                native_id: id,
                de_type: DesktopEnvironment::GNOME,
            },
            title: v
                .get("title")
                .and_then(Value::as_str)
                .unwrap_or_default()
                .to_string(),
            app_id: v
                .get("appId")
                .and_then(Value::as_str)
                .unwrap_or_default()
                .to_string(),
            pid: v.get("pid").and_then(Value::as_u64).unwrap_or(0) as u32,
            geometry: content,
            frame_geometry,
            states,
            workspace_id,
            monitor_id: None,
            stacking_order,
            desktop_file: None,
            window_type: window_type(v.get("windowType")),
            icon_geometry: None,
            keep_above: v.get("keepAbove").and_then(Value::as_bool).unwrap_or(false),
        })
    }

    /// 契约枚举名 → core `WindowType`。未知/缺失一律 `Unknown`。
    fn window_type(v: Option<&Value>) -> WindowType {
        match v.and_then(Value::as_str) {
            Some("Normal") => WindowType::Normal,
            Some("Dialog") => WindowType::Dialog,
            Some("Dock") => WindowType::Dock,
            Some("Desktop") => WindowType::Desktop,
            Some("DropdownMenu") => WindowType::DropdownMenu,
            Some("Tooltip") => WindowType::Tooltip,
            Some("Notification") => WindowType::Notification,
            Some("Splash") => WindowType::Splash,
            Some("Utility") => WindowType::Utility,
            _ => WindowType::Unknown,
        }
    }

    fn num(g: &Value, key: &str) -> i32 {
        g.get(key).and_then(Value::as_i64).unwrap_or(0) as i32
    }
}

/// org.gnome.Shell.Eval 桥接（路径 A）。
///
/// 单一 proxy 复用一条 session bus 连接；JS 片段由本模块常量提供，
/// 不接受调用方注入任意 JS（Eval 本身即任意代码执行面，收敛入口）。
pub struct GnomeEvalBridge {
    conn: Connection,
    /// 构造期探测到的 GNOME 版本——Eval 禁用提示按版本分支（§8.4）。
    version: GnomeVersion,
}

impl GnomeEvalBridge {
    /// 建桥：复用既有 session bus 连接（与 DisplayConfig 共享）。
    ///
    /// `version` 用于 Eval 禁用提示的版本分支：47+ 已移除
    /// `developer-tools` key，提示改指向 Extension 安装路径。
    pub fn new(conn: Connection, version: GnomeVersion) -> Self {
        Self { conn, version }
    }

    /// 执行 JS 表达式并解析返回值：`(true, '"JSON"')` → `Value`。
    ///
    /// `success=false` 表示 Shell 内执行抛异常或 Eval 被禁用——两者都
    /// 报 [`MutterError::Eval`]，由上层降级链统一处理。
    pub async fn eval_js(&self, js: &str) -> Result<Value> {
        let shell = ShellProxy::new(&self.conn)
            .await
            .map_err(|e| MutterError::Eval(format!("{SHELL_SERVICE} unreachable: {e}")))?;
        let (success, result_json) = shell
            .eval(js)
            .await
            .map_err(|e| MutterError::Eval(format!("call failed at {SHELL_PATH}: {e}")))?;
        if !success {
            return Err(MutterError::Eval(format!(
                "shell-side failure: {}",
                eval_disabled_hint(&self.version)
            )));
        }
        serde_json::from_str(&result_json)
            .map_err(|e| MutterError::Eval(format!("invalid JSON from eval: {e}")))
    }

    /// 探测 Eval 可用性：发一个恒真表达式。
    ///
    /// 成功即通道可用；失败（服务拒绝 / success=false）返回 Err——
    /// doctor 与双路径选择据此降级到 Extension。**只读探测，无副作用**。
    pub async fn probe(&self) -> Result<()> {
        self.eval_js("1 + 1").await.map(|_| ())
    }

    /// 窗口列表 JS（§8.1 设计文档原文）。
    ///
    /// `Meta.Rectangle` 是 boxed struct，直接 `JSON.stringify` 得到 `{}`
    /// （几何退化为 0×0）——四边须显式取值。工作区/类型/置顶同理须显式读，
    /// 不能留给 Rust 侧默认值。几何取 `get_frame_rect`（用户可见的窗口
    /// 边缘）：Wayland CSD 的 buffer rect 含客户端阴影，不能当内容几何。
    /// 类型表按 `MetaWindowType` 的稳定 ABI 编号；契约无对应取值的编号
    /// 不上报臆造名。
    pub(crate) const LIST_WINDOWS_JS: &'static str = r#"JSON.stringify(global.get_window_actors().map(a => {
            let mw = a.meta_window;
            let rect = f => { let g = f();
                return { x: g.x, y: g.y, width: g.width, height: g.height }; };
            let types = { 0: 'Normal', 1: 'Desktop', 2: 'Dock', 3: 'Dialog',
                4: 'Dialog', 5: 'Utility', 6: 'DropdownMenu', 7: 'Utility',
                8: 'Splash', 9: 'DropdownMenu', 10: 'DropdownMenu',
                11: 'Tooltip', 12: 'Notification', 13: 'DropdownMenu' };
            let ws = mw.get_workspace();
            return { id: mw.get_id(), title: mw.get_title() || '',
                     appId: mw.get_wm_class() || '', pid: mw.get_pid(),
                     minimized: mw.minimized, maximized: mw.maximized,
                     fullscreen: mw.fullscreen,
                     geometry: rect(() => mw.get_frame_rect()),
                     workspace: ws ? ws.index() : null,
                     windowType: types[mw.get_window_type()] || 'Unknown',
                     keepAbove: mw.is_above(),
                     hasFocus: mw.has_focus() };
        }))"#;

    /// 列出全部窗口。
    pub async fn list_windows(&self) -> Result<Vec<agent_shell_core::types::WindowInfo>> {
        let result = self.eval_js(Self::LIST_WINDOWS_JS).await?;
        let arr = result.as_array().cloned().unwrap_or_default();
        Ok(arr
            .iter()
            .enumerate()
            .filter_map(|(i, w)| parse::window(w, i as u32))
            .collect())
    }

    /// 当前活动窗口（hasFocus 位筛选；桌面无焦点时 None）。
    pub async fn get_active_window(&self) -> Result<Option<agent_shell_core::types::WindowInfo>> {
        let result = self.eval_js(Self::LIST_WINDOWS_JS).await?;
        Ok(result
            .as_array()
            .into_iter()
            .flatten()
            .find(|w| w.get("hasFocus").and_then(Value::as_bool).unwrap_or(false))
            .and_then(|w| parse::window(w, 0)))
    }

    /// 聚焦窗口（meta_window.activate()）。窗口不存在返回错误。
    pub async fn focus_window(&self, native_id: &str) -> Result<()> {
        let id = js_number(native_id)?;
        let found = self
            .eval_js(&format!(
                "(function() {{
                    let a = global.get_window_actors()
                        .find(a => a.meta_window.get_id() == {id});
                    if (!a) return 'false';
                    a.meta_window.activate(global.get_current_time());
                    return 'true';
                }})()"
            ))
            .await?;
        if found.as_str() != Some("true") {
            return Err(MutterError::Eval(format!("window `{native_id}` not found")));
        }
        Ok(())
    }

    /// 关闭窗口（meta_window.delete()）。delete 无回执——乐观语义，
    /// 窗口不存在静默（与 KWin 协议通道同口径）。
    pub async fn close_window(&self, native_id: &str) -> Result<()> {
        let id = js_number(native_id)?;
        self.eval_js(&format!(
            "(global.get_window_actors().find(a => a.meta_window.get_id() == {id})
              ?.meta_window.delete(), null)"
        ))
        .await?;
        Ok(())
    }

    /// 最小化(true)/还原(false)。
    pub async fn minimize_window(&self, native_id: &str, minimize: bool) -> Result<()> {
        let id = js_number(native_id)?;
        let flag = if minimize { "true" } else { "false" };
        self.eval_js(&format!(
            "(global.get_window_actors().find(a => a.meta_window.get_id() == {id})
              ?.meta_window.minimized = {flag}, null)"
        ))
        .await?;
        Ok(())
    }

    /// 最大化(true)/取消最大化(false)。
    pub async fn maximize_window(&self, native_id: &str, maximize: bool) -> Result<()> {
        let id = js_number(native_id)?;
        let flag = if maximize {
            "Meta.MaximizeFlags.BOTH"
        } else {
            "0"
        };
        self.eval_js(&format!(
            "(global.get_window_actors().find(a => a.meta_window.get_id() == {id})
              ?.meta_window.maximize({flag}), null)"
        ))
        .await?;
        Ok(())
    }
}

/// 校验并透传窗口 id：Eval 路径的 meta_window id 是数字。
///
/// 非法输入（含注入向量）直接报错，不进 JS——Eval 是 Shell 进程内
/// 执行面，等价于任意代码执行。
fn js_number(native_id: &str) -> Result<&str> {
    if !native_id.is_empty() && native_id.bytes().all(|b| b.is_ascii_digit()) {
        Ok(native_id)
    } else {
        Err(MutterError::Eval(format!(
            "non-numeric window id `{native_id}` rejected (meta_window ids are numeric)"
        )))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use agent_shell_core::types::{DesktopEnvironment, Rect, WindowState, WindowType, WorkspaceId};
    use serde_json::json;

    #[test]
    fn parses_eval_window_payload() {
        let w = json!({
            "id": 42u64, "title": "Editor", "appId": "org.gnome.TextEditor",
            "pid": 1234, "minimized": false, "maximized": true, "fullscreen": false,
            "geometry": {"x": 10, "y": 20, "width": 800, "height": 600},
            "hasFocus": true
        });
        let info = parse::window(&w, 3).expect("window parses");
        assert_eq!(info.id.native_id, "42");
        assert_eq!(info.title, "Editor");
        assert_eq!(info.app_id, "org.gnome.TextEditor");
        assert_eq!(info.pid, 1234);
        assert_eq!(info.stacking_order, 3);
        assert!(matches!(info.states.as_slice(), [WindowState::Maximized]));
        assert_eq!(
            info.geometry,
            Rect {
                x: 10,
                y: 20,
                width: 800,
                height: 600
            }
        );
    }

    #[test]
    fn string_ids_accepted_for_extension_compat() {
        let w = json!({"id": "win-7", "title": "", "appId": "", "pid": 0});
        let info = parse::window(&w, 0).expect("string id parses");
        assert_eq!(info.id.native_id, "win-7");
    }

    #[test]
    fn missing_id_is_rejected() {
        assert!(parse::window(&json!({"title": "x"}), 0).is_none());
    }

    #[test]
    fn missing_frame_geometry_falls_back_to_content() {
        let w = json!({
            "id": 2u64,
            "geometry": {"x": 5, "y": 5, "width": 300, "height": 200}
        });
        let info = parse::window(&w, 0).unwrap();
        assert_eq!(info.frame_geometry.width, 300);
    }

    #[test]
    fn explicit_frame_geometry_wins_over_content() {
        let w = json!({
            "id": 1u64,
            "geometry": {"x": 5, "y": 5, "width": 300, "height": 200},
            "frameGeometry": {"x": 0, "y": 0, "width": 320, "height": 240}
        });
        let info = parse::window(&w, 0).unwrap();
        assert_eq!(info.geometry.width, 300);
        assert_eq!(info.frame_geometry.width, 320);
    }

    #[test]
    fn parses_workspace_window_type_and_keep_above() {
        // GNOME 51 扩展载荷形状：工作区是 0 基序号，类型是同名字符串。
        let w = json!({
            "id": 1211664006u64, "title": "New Document", "appId": "org.gnome.TextEditor",
            "pid": 1472, "workspace": 0, "windowType": "Dialog", "keepAbove": true,
            "geometry": {"x": 120, "y": 90, "width": 900, "height": 600},
            "frameGeometry": {"x": 120, "y": 60, "width": 900, "height": 630}
        });
        let info = parse::window(&w, 0).expect("payload parses");
        assert_eq!(
            info.workspace_id,
            Some(WorkspaceId {
                native_id: "0".into(),
                de_type: DesktopEnvironment::GNOME,
            })
        );
        assert_eq!(info.window_type, WindowType::Dialog);
        assert!(info.keep_above);
        assert_eq!(info.geometry.height, 600);
        assert_eq!(info.frame_geometry.height, 630);
    }

    #[test]
    fn sticky_window_has_no_workspace() {
        // null = 全部工作区可见；不得伪造 0 号工作区。
        let w = json!({"id": 5u64, "workspace": null, "windowType": "Normal"});
        let info = parse::window(&w, 0).unwrap();
        assert!(info.workspace_id.is_none());
        assert_eq!(info.window_type, WindowType::Normal);
        assert!(!info.keep_above);
    }

    #[test]
    fn unknown_window_type_name_is_unknown() {
        let w = json!({"id": 6u64, "windowType": "OverrideOther"});
        assert_eq!(
            parse::window(&w, 0).unwrap().window_type,
            WindowType::Unknown
        );
    }

    #[test]
    fn numeric_ids_only_reach_eval_js() {
        assert_eq!(js_number("12345").unwrap(), "12345");
        assert!(js_number("").is_err());
        assert!(js_number("12; rm -rf /").is_err());
        assert!(js_number("abc").is_err());
        assert!(js_number("-1").is_err());
    }

    #[test]
    fn list_windows_js_mentions_required_api_surface() {
        // 守住 §8.4 操作面：actor 遍历 + 矩形 + 工作区/类型/置顶 + has_focus。
        for needle in [
            "get_window_actors",
            "get_frame_rect",
            "get_workspace",
            "get_window_type",
            "is_above",
            "has_focus",
        ] {
            assert!(
                GnomeEvalBridge::LIST_WINDOWS_JS.contains(needle),
                "LIST_WINDOWS_JS missing `{needle}`"
            );
        }
        // 回归守卫：矩形不得整对象序列化（boxed struct 会变成 `{}`），
        // 工作区取序号而不是把 Meta.Workspace 塞进载荷。
        assert!(GnomeEvalBridge::LIST_WINDOWS_JS.contains("rect(() => mw.get_frame_rect())"));
        assert!(GnomeEvalBridge::LIST_WINDOWS_JS.contains("ws.index()"));
        assert!(!GnomeEvalBridge::LIST_WINDOWS_JS.contains("geometry: mw.get_frame_rect()"));
    }
}
