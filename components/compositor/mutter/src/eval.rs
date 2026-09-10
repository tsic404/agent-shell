//! org.gnome.Shell.Eval 路径（设计文档 §8.1 路径 A / §8.2，GNOME <47）。
//!
//! Eval 即 `gdbus call -e -d org.gnome.Shell -o /org/gnome/Shell
//! -m org.gnome.Shell.Eval '<js>'`：在 Shell 进程内执行 JS，返回
//! `(success: bool, result: s)`——result 是 `JSON.stringify(...)` 的
//! 字符串（外层再包一层引号，zbus 反序列化后即裸 JSON 文本）。
//!
//! 限制：GNOME 41+ 默认禁用（unsafe-mode），47+ 收紧为不可开启；
//! 本模块不代改用户设置，探测失败由上层降级到 Extension。

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
        DesktopEnvironment, Rect, WindowId, WindowInfo, WindowState, WindowType,
    };
    use serde_json::Value;

    /// 把 Eval/Extension 返回的单窗 JSON 归一化为 core `WindowInfo`。
    ///
    /// GNOME 侧字段：`id`(number|string) / `title` / `appId` / `pid` /
    /// `geometry{x,y,width,height}` / `frameGeometry`(可选) /
    /// `hasFocus` / `minimized` / `maximized`。缺字段按默认值兜底。
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
            workspace_id: None,
            monitor_id: None,
            stacking_order,
            desktop_file: None,
            window_type: WindowType::Unknown,
            icon_geometry: None,
            keep_above: false,
        })
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
    pub(crate) const LIST_WINDOWS_JS: &'static str = r#"JSON.stringify(global.get_window_actors().map(a => {
            let mw = a.meta_window;
            return { id: mw.get_id(), title: mw.get_title() || '',
                     appId: mw.get_wm_class() || '', pid: mw.get_pid(),
                     minimized: mw.minimized, maximized: mw.maximized,
                     fullscreen: mw.fullscreen,
                     geometry: mw.get_frame_rect(),
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
    use agent_shell_core::types::{Rect, WindowState};
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
    fn numeric_ids_only_reach_eval_js() {
        assert_eq!(js_number("12345").unwrap(), "12345");
        assert!(js_number("").is_err());
        assert!(js_number("12; rm -rf /").is_err());
        assert!(js_number("abc").is_err());
        assert!(js_number("-1").is_err());
    }

    #[test]
    fn list_windows_js_mentions_required_api_surface() {
        // 守住 §8.4 操作面：actor 遍历 + frame_rect + has_focus。
        for needle in ["get_window_actors", "get_frame_rect", "has_focus"] {
            assert!(
                GnomeEvalBridge::LIST_WINDOWS_JS.contains(needle),
                "LIST_WINDOWS_JS missing `{needle}`"
            );
        }
    }
}
