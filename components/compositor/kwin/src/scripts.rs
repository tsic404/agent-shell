//! 预置 KWin JS 脚本模板 + KWin 5/6 兼容层（设计文档 §7.4 / §7.5 / §7.7）。
//!
//! 所有脚本通过 callDBus → `com.agent_shell.Response` 回传 JSON 结果
//! （§7.3 策略 B/C）。参数注入采用两段式：
//!
//! 1. 模板中的 `%ARG%` 命名占位符由 [`render`] 按 `args` 表替换
//!    （`%ID%` 窗口 id、`%X%`/`%Y%`/`%W%`/`%H%` 几何、`%NUM%` 整数）；
//! 2. 替换值统一经 [`js_string`]/[`js_int`] 转义，杜绝脚本注入——
//!    KWin 脚本在 compositor 进程内执行，等价于任意代码执行面。
//!
//! KWin 5/6 差异集中在 [`Compat`] 的几何/虚拟桌面表达式上。

use crate::error::KWinError;
use crate::error::Result;
use serde_json::Value;

/// callDBus 回传目标常量（agent-shell 注册的临时响应服务，§7.3 策略 B）。
pub const RESPONSE_SERVICE: &str = "com.agent_shell.Response";
/// 响应服务对象路径。
pub const RESPONSE_PATH: &str = "/com/agent_shell/response";
/// 响应服务接口名。
pub const RESPONSE_IFACE: &str = "com.agent_shell.Response";
/// 回传方法名。
pub const RESPONSE_METHOD: &str = "sendResult";

/// 请求 id 占位符：`subst` 阶段替换为本次查询的 UUID。
///
/// 回传 payload 统一为 `{"req": "<id>", "result": <expr>}`——响应服务按
/// req 路由到对应等待者，并发查询互不串扰（🔴1）。
pub const REQ_ID_TOKEN: &str = "__AGENT_SHELL_REQ_ID__";

/// 脚本尾部：把结果序列化后带请求 id 经 callDBus 回传。
///
/// 统一 `JSON.stringify`——KWin 5 的 callDBus 不支持直接传 JS 对象。
/// payload 形如 `{"req": <token→UUID>, "result": <expr>}`；REQ_ID_TOKEN
/// 由 `KWinBridge::run_script_raw` 替换为真实 UUID，响应服务据此路由
/// 到对应等待者（并发查询互不串扰）。
fn send_result(expr: &str) -> String {
    format!(
        "callDBus(\"{service}\", \"{path}\", \"{iface}\", \"{method}\", \
         JSON.stringify({{ req: \"{token}\", result: {expr} }}));",
        service = RESPONSE_SERVICE,
        path = RESPONSE_PATH,
        iface = RESPONSE_IFACE,
        method = RESPONSE_METHOD,
        token = REQ_ID_TOKEN,
    )
}

/// 长驻事件脚本专用推送尾部：回传**不带 `req` 字段**的裸 JSON。
///
/// 「有 req 即一次性查询、无 req 即事件推送」的分界由此单一来源保证：
/// EventMonitor 模板只经此函数推送，不触碰 REQ_ID_TOKEN，因此不需要
/// token 替换即可被 ResponseService 正确分流到事件队列。
pub fn push_event(expr: &str) -> String {
    format!(
        "callDBus(\"{service}\", \"{path}\", \"{iface}\", \"{method}\", \
         JSON.stringify({expr}));",
        service = RESPONSE_SERVICE,
        path = RESPONSE_PATH,
        iface = RESPONSE_IFACE,
        method = RESPONSE_METHOD,
    )
}

/// 预置脚本清单（14 个，设计文档 §7.4）。
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum ScriptTemplate {
    /// 窗口列表。
    ListWindows,
    /// 当前活动窗口。
    GetActiveWindow,
    /// 聚焦窗口。
    FocusWindow,
    /// 移动窗口。
    MoveWindow,
    /// 缩放窗口。
    ResizeWindow,
    /// 关闭窗口。
    CloseWindow,
    /// 一次设定窗口几何。
    SetWindowGeometry,
    /// 最小化(%NUM%=1)/还原(%NUM%=0)窗口。
    MinimizeWindow,
    /// 最大化(%NUM%=1)/还原(%NUM%=0)窗口。
    MaximizeWindow,
    /// 列出虚拟桌面。
    ListWorkspaces,
    /// 切换虚拟桌面（%ID%）。
    SwitchWorkspace,
    /// 移动窗口到虚拟桌面（%ID% + %WS%）。
    MoveWindowToWorkspace,
    /// 列出显示器。
    ListMonitors,
    /// 长驻事件订阅脚本（不 stop，见 event_script.rs）。
    EventMonitor,
}

impl ScriptTemplate {
    /// 脚本文件名（日志与 loadScript 标识用）。
    pub fn file_name(&self) -> &'static str {
        match self {
            Self::ListWindows => "list_windows.js",
            Self::GetActiveWindow => "get_active_window.js",
            Self::FocusWindow => "focus_window.js",
            Self::MoveWindow => "move_window.js",
            Self::ResizeWindow => "resize_window.js",
            Self::CloseWindow => "close_window.js",
            Self::SetWindowGeometry => "set_window_geometry.js",
            Self::MinimizeWindow => "minimize_window.js",
            Self::MaximizeWindow => "maximize_window.js",
            Self::ListWorkspaces => "list_workspaces.js",
            Self::SwitchWorkspace => "switch_workspace.js",
            Self::MoveWindowToWorkspace => "move_window_to_workspace.js",
            Self::ListMonitors => "list_monitors.js",
            Self::EventMonitor => "event_monitor.js",
        }
    }

    /// 渲染为可直接 loadScript/run 的完整脚本文本。
    ///
    /// - `v6`: 按 KWin 6 API 生成；false 走 v5 兼容表达式；
    /// - `args`: 占位符取值表（键名不带百分号）。
    pub fn render(&self, v6: bool, args: &[(&str, Value)]) -> Result<String> {
        let body = Compat { v6 }.body(*self);
        subst(&body, args)
    }
}

/// KWin 5/6 Scripting API 兼容层（§7.5 版本表）：
///
/// | API | KWin 5 | KWin 6 |
/// |-----|--------|--------|
/// | 窗口几何 | w.geometry | w.frameGeometry |
/// | 虚拟桌面 | w.desktop (int) | w.desktops (VirtualDesktop 数组) |
/// | 当前桌面 | workspace.currentDesktop (int) | workspace.currentDesktop (VirtualDesktop) |
/// | 窗口 ID | w.internalId / w.id 兜底 | w.internalId |
struct Compat {
    v6: bool,
}

impl Compat {
    /// 窗口 id 表达式（v5 老版本无 internalId 时兜底 w.id）。
    fn window_id(&self) -> String {
        "(w.internalId !== undefined ? w.internalId.toString() : String(w.id))".into()
    }

    /// 几何读取表达式：v6 优先 frameGeometry，v5 用 geometry。
    fn frame_geometry(&self) -> String {
        if self.v6 {
            "(w.frameGeometry || w.geometry)".into()
        } else {
            "w.geometry".into()
        }
    }

    /// 桌面 id 读取表达式。
    fn desktop_id(&self) -> String {
        if self.v6 {
            "(w.desktops && w.desktops.length ? w.desktops[0].id : -1)".into()
        } else {
            "(typeof w.desktop === \"number\" ? w.desktop : -1)".into()
        }
    }

    /// 按模板返回脚本体（含 %…% 占位符）。
    fn body(&self, tpl: ScriptTemplate) -> String {
        let gid = self.window_id();
        match tpl {
            ScriptTemplate::ListWindows => format!(
                "var windows = workspace.windowList();\n\
                 var result = [];\n\
                 for (var i = 0; i < windows.length; i++) {{\n\
                 \x20   var w = windows[i];\n\
                 \x20   var g = {geo};\n\
                 \x20   result.push({{\n\
                 \x20       id: {gid},\n\
                 \x20       title: w.caption,\n\
                 \x20       appId: w.resourceClass || w.resourceName || \"\",\n\
                 \x20       pid: w.pid,\n\
                 \x20       geometry: {{ x: g.x, y: g.y, width: g.width, height: g.height }},\n\
                 \x20       frameGeometry: {{ x: g.x, y: g.y, width: g.width, height: g.height }},\n\
                 \x20       minimized: w.minimized === true,\n\
                 \x20       maximized: (w.maximizeMode === 3),\n\
                 \x20       fullscreen: w.fullScreen === true,\n\
                 \x20       keepAbove: w.keepAbove === true,\n\
                 \x20       desktop: {did},\n\
                 \x20       stackingOrder: i,\n\
                 \x20       windowType: w.windowType,\n\
                 \x20       desktopFile: w.desktopFileName || \"\"\n\
                 \x20   }});\n\
                 }}\n{send}",
                gid = gid,
                geo = self.frame_geometry(),
                did = self.desktop_id(),
                send = send_result("result"),
            ),
            ScriptTemplate::GetActiveWindow => format!(
                "var w = workspace.activeWindow;\n\
                 var result = null;\n\
                 if (w) {{ result = {{ id: {gid}, title: w.caption,\n\
                 \x20   appId: w.resourceClass || w.resourceName || \"\", pid: w.pid }}; }}\n{send}",
                gid = gid,
                send = send_result("result"),
            ),
            ScriptTemplate::FocusWindow => format!(
                "var w = workspace.windowList().find(function(w) {{ return {gid} === %ID%; }});\n\
                 if (w) {{ workspace.activeWindow = w;\n\
                 \x20   {ok}\n}} else {{ {err}\n}}",
                gid = gid,
                ok = send_result("{ success: true }"),
                err = send_result("{ success: false, error: \"Window not found\" }"),
            ),
            // 协议不支持 set_geometry（§7.2），移动/缩放始终走本通道。
            ScriptTemplate::MoveWindow => format!(
                "var w = workspace.windowList().find(function(w) {{ return {gid} === %ID%; }});\n\
                 if (w) {{\n\
                 \x20   var g = {geo};\n\
                 \x20   w.frameGeometry = {{ x: %X%, y: %Y%, width: g.width, height: g.height }};\n\
                 \x20   {ok}\n}} else {{ {err}\n}}",
                gid = gid,
                geo = self.frame_geometry(),
                ok = send_result("{ success: true }"),
                err = send_result("{ success: false, error: \"Window not found\" }"),
            ),
            ScriptTemplate::ResizeWindow => format!(
                "var w = workspace.windowList().find(function(w) {{ return {gid} === %ID%; }});\n\
                 if (w) {{\n\
                 \x20   var g = {geo};\n\
                 \x20   w.frameGeometry = {{ x: g.x, y: g.y, width: %W%, height: %H% }};\n\
                 \x20   {ok}\n}} else {{ {err}\n}}",
                gid = gid,
                geo = self.frame_geometry(),
                ok = send_result("{ success: true }"),
                err = send_result("{ success: false, error: \"Window not found\" }"),
            ),
            ScriptTemplate::CloseWindow => format!(
                "var w = workspace.windowList().find(function(w) {{ return {gid} === %ID%; }});\n\
                if (w) {{ w.closeWindow();\n\
                 \x20   {ok}\n}} else {{ {err}\n}}",
                gid = gid,
                ok = send_result("{ success: true }"),
                err = send_result("{ success: false, error: \"Window not found\" }"),
            ),
            ScriptTemplate::SetWindowGeometry => format!(
                "var w = workspace.windowList().find(function(w) {{ return {gid} === %ID%; }});\n\
                 if (w) {{\n\
                 \x20   w.frameGeometry = {{ x: %X%, y: %Y%, width: %W%, height: %H% }};\n\
                 \x20   {ok}\n}} else {{ {err}\n}}",
                gid = gid,
                ok = send_result("{ success: true }"),
                err = send_result("{ success: false, error: \"Window not found\" }"),
            ),
            ScriptTemplate::MinimizeWindow => format!(
                "var w = workspace.windowList().find(function(w) {{ return {gid} === %ID%; }});\n\
                 if (w) {{ w.minimized = (%NUM% === 1);\n\
                 \x20   {ok}\n}} else {{ {err}\n}}",
                gid = gid,
                ok = send_result("{ success: true }"),
                err = send_result("{ success: false, error: \"Window not found\" }"),
            ),
            ScriptTemplate::MaximizeWindow => format!(
                "var w = workspace.windowList().find(function(w) {{ return {gid} === %ID%; }});\n\
                 if (w) {{\n\
                 \x20   if (%NUM% === 1) {{ if (w.setMaximize !== undefined) {{ w.setMaximize(true, true); }} else {{ w.maximizeMode = 3; }} }}\n\
                 \x20   else {{ if (w.setMaximize !== undefined) {{ w.setMaximize(false, false); }} else {{ w.maximizeMode = 0; }} }}\n\
                 \x20   {ok}\n}} else {{ {err}\n}}",
                gid = gid,
                ok = send_result("{ success: true }"),
                err = send_result("{ success: false, error: \"Window not found\" }"),
            ),
            ScriptTemplate::ListWorkspaces => {
                if self.v6 {
                    format!(
                        "var result = [];\n\
                         var dts = workspace.desktops;\n\
                         for (var i = 0; i < dts.length; i++) {{\n\
                         \x20   var d = dts[i];\n\
                         \x20   result.push({{ id: String(d.id), name: d.name || (\"Desktop \" + (i + 1)),\n\
                         \x20           number: i + 1, isActive: false }});\n\
                         }}\n\
                         var cur = workspace.currentDesktop;\n\
                         for (var j = 0; j < result.length; j++) {{ if (String(cur.id) === result[j].id) {{ result[j].isActive = true; }} }}\n{send}",
                        send = send_result("result"),
                    )
                } else {
                    format!(
                        "var n = workspace.desktops;\n\
                         var result = [];\n\
                         for (var k = 1; k <= n; k++) {{\n\
                         \x20   result.push({{ id: String(k), name: \"Desktop \" + k, number: k,\n\
                         \x20           isActive: (workspace.currentDesktop === k) }});\n\
                         }}\n{send}",
                        send = send_result("result"),
                    )
                }
            }
            ScriptTemplate::SwitchWorkspace => {
                if self.v6 {
                    format!(
                        "var d = workspace.desktops.find(function(d) {{ return String(d.id) === %WS%; }});\n\
                         if (d) {{ workspace.currentDesktop = d;\n\
                         \x20   {ok}\n}} else {{ {err}\n}}",
                        ok = send_result("{ success: true }"),
                        err = send_result("{ success: false, error: \"Workspace not found\" }"),
                    )
                } else {
                    // v5：桌面号为 int——校验范围后再赋值，无效输入报错
                    // 而非假成功（🟡4）。
                    format!(
                        "var n = parseInt(%WS%, 10);\n\
                         if (isNaN(n) || n < 1 || n > workspace.desktops) {{ {err}\n}} else {{\n\
                         \x20   workspace.currentDesktop = n;\n\
                         \x20   {ok}\n}}",
                        ok = send_result("{ success: true }"),
                        err = send_result("{ success: false, error: \"Workspace not found\" }"),
                    )
                }
            }
            ScriptTemplate::MoveWindowToWorkspace => {
                if self.v6 {
                    format!(
                        "var w = workspace.windowList().find(function(w) {{ return {gid} === %ID%; }});\n\
                         var d = workspace.desktops.find(function(d) {{ return String(d.id) === %WS%; }});\n\
                         if (w && d) {{ w.desktops = [d];\n\
                         \x20   {ok}\n}} else {{ {err}\n}}",
                        gid = gid,
                        ok = send_result("{ success: true }"),
                        err = send_result("{ success: false, error: \"Window or workspace not found\" }"),
                    )
                } else {
                    format!(
                        "var w = workspace.windowList().find(function(w) {{ return {gid} === %ID%; }});\n\
                         if (w) {{ w.desktop = parseInt(%WS%, 10);\n\
                         \x20   {ok}\n}} else {{ {err}\n}}",
                        gid = gid,
                        ok = send_result("{ success: true }"),
                        err = send_result("{ success: false, error: \"Window not found\" }"),
                    )
                }
            }
            ScriptTemplate::ListMonitors => format!(
                "var screens = workspace.screens;\n\
                 var result = [];\n\
                 for (var m = 0; m < screens.length; m++) {{\n\
                 \x20   var s = screens[m];\n\
                 \x20   result.push({{ id: s.name, name: s.name,\n\
                 \x20           geometry: {{ x: s.geometry.x, y: s.geometry.y,\n\
                 \x20                       width: s.geometry.width, height: s.geometry.height }},\n\
                 \x20           scale: s.scale, isPrimary: (m === 0) }});\n\
                 }}\n{send}",
                send = send_result("result"),
            ),
            // 长驻事件脚本（§7.4）：只注册信号连接，每次事件触发推送一条；不 stop（event_script.rs）。
            ScriptTemplate::EventMonitor => {
                // 🔴1：必须走 push_event（无 req 字段）——send_result 会包上
                // REQ_ID_TOKEN 且长驻脚本不经 token 替换，推送会被按查询
                // 路由而静默丢弃。
                let push = push_event("payload");
                format!(
                    "function __push(payload) {{\n{push}\n}}\n\
                     function __wid(w) {{\n\
                     \x20   return w.internalId !== undefined ? w.internalId.toString() : String(w.id);\n\
                     }}\n\
                     workspace.windowAdded.connect(function(w) {{ __push({{ event: \"windowOpened\", id: __wid(w) }}); }});\n\
                     workspace.windowRemoved.connect(function(w) {{ __push({{ event: \"windowClosed\", id: __wid(w) }}); }});\n\
                     workspace.activeWindowChanged.connect(function() {{\n\
                     \x20   var w = workspace.activeWindow;\n\
                     \x20   if (w) __push({{ event: \"windowFocused\", id: __wid(w) }});\n\
                     }});\n"
                )
            }
        }
    }
}

/// 占位符替换：`%KEY%` → args 中对应值（字符串转义 / 数值直写）。
///
/// 未提供的占位符保持原样并报错——带未替换占位符的脚本是 bug，
/// 宁可失败也不静默注入字面量 `%ID%` 到 JS。
fn subst(body: &str, args: &[(&str, Value)]) -> Result<String> {
    let mut out = body.to_string();
    for (key, val) in args {
        let token = format!("%{key}%");
        let replacement = match val {
            Value::String(s) => js_string(s),
            Value::Number(n) => n.to_string(),
            other => {
                return Err(KWinError::InvalidScriptOutput(format!(
                    "unsupported arg type for %{key}%: {other}"
                )))
            }
        };
        if !out.contains(&token) {
            return Err(KWinError::Scripting(format!(
                "template has no placeholder {token}"
            )));
        }
        out = out.replace(&token, &replacement);
    }
    // 剩余任何 %XXX% 占位符都说明调用方漏传参数（req id 占位符不含 '%'，
    // 不受此检查影响，最后统一替换——避免用户值被二次替换的顺序问题）。
    if out.contains('%') {
        return Err(KWinError::Scripting(format!(
            "unresolved placeholder in rendered script: {}",
            out.split('%').nth(1).unwrap_or("?")
        )));
    }
    // REQ_ID_TOKEN 原样保留在输出里，由 KWinBridge::run_script_raw 注入真实 UUID。
    Ok(out)
}

/// Rust 字符串 → 安全 JS 字符串字面量（引号/反斜杠/控制字符转义）。
fn js_string(s: &str) -> String {
    let mut out = String::with_capacity(s.len() + 2);
    out.push('"');
    for c in s.chars() {
        match c {
            '"' => out.push_str("\\\""),
            '\\' => out.push_str("\\\\"),
            '\n' => out.push_str("\\n"),
            '\r' => out.push_str("\\r"),
            '\t' => out.push_str("\\t"),
            c if (c as u32) < 0x20 => out.push_str(&format!("\\u{:04x}", c as u32)),
            c => out.push(c),
        }
    }
    out.push('"');
    out
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn render_v6(tpl: ScriptTemplate, args: &[(&str, Value)]) -> String {
        tpl.render(true, args).expect("render should succeed")
    }

    #[test]
    fn all_templates_render_without_args_or_with_expected_args() {
        // 无参模板必须零参可渲染。
        for tpl in [
            ScriptTemplate::ListWindows,
            ScriptTemplate::GetActiveWindow,
            ScriptTemplate::ListWorkspaces,
            ScriptTemplate::ListMonitors,
            ScriptTemplate::EventMonitor,
        ] {
            let _ = tpl
                .render(true, &[])
                .unwrap_or_else(|_| panic!("{}", tpl.file_name()));
            let _ = tpl
                .render(false, &[])
                .unwrap_or_else(|_| panic!("{}", tpl.file_name()));
        }
        // 带参模板缺参时报错而非渲染出残留占位符。
        assert!(ScriptTemplate::FocusWindow.render(true, &[]).is_err());
    }

    #[test]
    fn list_windows_uses_desktops_array_on_v6_and_int_on_v5() {
        let v6 = render_v6(ScriptTemplate::ListWindows, &[]);
        assert!(v6.contains("w.desktops && w.desktops.length"));
        let v5 = ScriptTemplate::ListWindows.render(false, &[]).unwrap();
        assert!(v5.contains("typeof w.desktop === \"number\""));
    }

    #[test]
    fn move_window_substitutes_geometry() {
        let script = render_v6(
            ScriptTemplate::MoveWindow,
            &[("ID", json!("abcd")), ("X", json!(100)), ("Y", json!(50))],
        );
        assert!(script.contains("\"abcd\""));
        assert!(script.contains("x: 100, y: 50"));
        assert!(!script.contains("%ID%"));
    }

    #[test]
    fn window_id_is_injection_safe() {
        let payload = "x\"); malicious(); (\"";
        let script = render_v6(ScriptTemplate::FocusWindow, &[("ID", json!(payload))]);
        // js_string 必须把 payload 的每个 `"` 转义为 `\"`，使恶意片段
        // 停留在字符串字面量内。期望 needle：`"x\"); malicious(); (\""`。
        let mut needle = String::from("\"");
        for c in payload.chars() {
            if c == '"' {
                needle.push_str("\\\"");
            } else {
                needle.push(c);
            }
        }
        needle.push('"');
        assert!(
            script.contains(&needle),
            "escaped payload not found verbatim; script: {script}"
        );
    }

    #[test]
    fn event_monitor_never_sends_final_result_but_registers_signals() {
        let script = render_v6(ScriptTemplate::EventMonitor, &[]);
        assert!(script.contains("workspace.windowAdded.connect"));
        assert!(script.contains("workspace.windowRemoved.connect"));
        assert!(script.contains("workspace.activeWindowChanged.connect"));
        // 长驻脚本没有一次性结果回传语句。
        assert!(!script.contains("JSON.stringify(result)"));
    }

    #[test]
    fn event_monitor_pushes_bare_json_without_req_field() {
        // 跨模块契约（复审 🔴1）：event_monitor.js 不经 token 替换直接加载，
        // 其推送 payload 绝不能含 req 字段——否则 ResponseService 会把它
        // 当一次性查询路由而静默丢弃。
        let script = render_v6(ScriptTemplate::EventMonitor, &[]);
        assert!(!script.contains(REQ_ID_TOKEN));
        assert!(!script.contains("req:"));
        // 推送语句是裸 JSON 序列化。
        assert!(script.contains("JSON.stringify(payload)"));

        // 对照：一次性模板的回传必含 req 字段与待替换 token。
        let query = render_v6(ScriptTemplate::ListWindows, &[]);
        assert!(query.contains("req:"));
        assert!(query.contains(REQ_ID_TOKEN));
    }

    #[test]
    fn maximize_uses_setmaximize_when_available() {
        let script = render_v6(
            ScriptTemplate::MaximizeWindow,
            &[("ID", json!("u1")), ("NUM", json!(1))],
        );
        assert!(script.contains("setMaximize(true, true)"));
        assert!(script.contains("maximizeMode = 3"));
    }

    #[test]
    fn unknown_arg_key_errors() {
        assert!(ScriptTemplate::ListWindows
            .render(true, &[("NOPE", json!("x"))])
            .is_err());
    }

    /// TSI-2445 回归：模板 `"%ID%"` 外层引号 + `js_string` 二次引号 →
    /// `""uuid""` JS 解析错误 → callDBus 永不发出 → 5s 超时。
    /// 修复后 String 参数只出现一次引号（由 js_string 统一负责）。
    #[test]
    fn string_arg_not_double_quoted() {
        let script = render_v6(ScriptTemplate::FocusWindow, &[("ID", json!("abc-123"))]);
        // 期望 `=== "abc-123"`，而非 `=== ""abc-123""`。
        assert!(
            script.contains("=== \"abc-123\""),
            "single-quoted string literal not found; script: {script}"
        );
        assert!(
            !script.contains("\"\"abc-123\"\""),
            "double-quoted string literal leaked through; script: {script}"
        );
    }

    /// TSI-2445 回归：%WS% 同样受双重引号影响——v6 桌面 id 为字符串。
    #[test]
    fn workspace_arg_not_double_quoted_v6() {
        let script = render_v6(ScriptTemplate::SwitchWorkspace, &[("WS", json!("desk-42"))]);
        assert!(
            script.contains("=== \"desk-42\""),
            "single-quoted workspace id not found; script: {script}"
        );
        assert!(
            !script.contains("\"\"desk-42\"\""),
            "double-quoted workspace id leaked through; script: {script}"
        );
    }

    /// TSI-2445 回归：v5 `parseInt("%WS%")` 同样被双重引号影响。
    #[test]
    fn workspace_arg_not_double_quoted_v5() {
        let script = ScriptTemplate::SwitchWorkspace
            .render(false, &[("WS", json!("3"))])
            .unwrap();
        assert!(
            script.contains("parseInt(\"3\", 10)"),
            "single-quoted parseInt arg not found; script: {script}"
        );
        assert!(
            !script.contains("parseInt(\"\"3\"\", 10)"),
            "double-quoted parseInt arg leaked through; script: {script}"
        );
    }

    /// TSI-2445 回归：KWin 6 loadScript 上下文无 `Qt` 全局对象，
    /// `Qt.rect(...)` 报 `Qt is not defined` → 脚本异常 → 5s 超时。
    /// 修复后用纯 JS 对象 `{x, y, width, height}` 赋值 frameGeometry，
    /// KWin 6 注册了 QJSValue→RectF 转换器读取这四个属性。
    #[test]
    fn geometry_templates_use_plain_object_not_qt_rect() {
        let move_script = render_v6(
            ScriptTemplate::MoveWindow,
            &[("ID", json!("w1")), ("X", json!(10)), ("Y", json!(20))],
        );
        assert!(
            move_script.contains("frameGeometry = { x: 10, y: 20"),
            "move must use plain JS object; script: {move_script}"
        );
        assert!(
            !move_script.contains("Qt.rect"),
            "move must not use Qt.rect; script: {move_script}"
        );

        let resize_script = render_v6(
            ScriptTemplate::ResizeWindow,
            &[("ID", json!("w1")), ("W", json!(800)), ("H", json!(600))],
        );
        assert!(
            resize_script.contains("frameGeometry = { x: g.x, y: g.y, width: 800, height: 600 }"),
            "resize must use plain JS object; script: {resize_script}"
        );
        assert!(
            !resize_script.contains("Qt.rect"),
            "resize must not use Qt.rect; script: {resize_script}"
        );

        let set_geom = render_v6(
            ScriptTemplate::SetWindowGeometry,
            &[
                ("ID", json!("w1")),
                ("X", json!(0)),
                ("Y", json!(0)),
                ("W", json!(1920)),
                ("H", json!(1080)),
            ],
        );
        assert!(
            set_geom.contains("frameGeometry = { x: 0, y: 0, width: 1920, height: 1080 }"),
            "set_geometry must use plain JS object; script: {set_geom}"
        );
        assert!(
            !set_geom.contains("Qt.rect"),
            "set_geometry must not use Qt.rect; script: {set_geom}"
        );
    }

    /// TSI-2445 回归：KWin 6 `XdgToplevelWindow` 无 `close()` 方法，
    /// 报 `Property 'close' is not a function` → 5s 超时。
    /// KWin 6 `Window` 基类的 `public Q_SLOTS` 中是 `closeWindow()`。
    #[test]
    fn close_template_uses_closewindow_not_close() {
        let script = render_v6(ScriptTemplate::CloseWindow, &[("ID", json!("w1"))]);
        assert!(
            script.contains("w.closeWindow()"),
            "close must use closeWindow(); script: {script}"
        );
        assert!(
            !script.contains("w.close()"),
            "close must not use w.close(); script: {script}"
        );
    }
}
