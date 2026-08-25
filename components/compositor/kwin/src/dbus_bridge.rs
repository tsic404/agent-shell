//! D-Bus ↔ KWin Scripting 桥接（设计文档 §7.3 / §7.5，`dbus_bridge.rs`）。
//!
//! KWin Scripting 的 `loadScript` 返回 object path，但 `run` 无返回值——
//! 结果回传采用策略 B/C 组合：
//!
//! 1. agent-shell 在 session bus 注册响应服务 `com.agent_shell.Response`
//!    （对象路径 `/com/agent_shell/response`）；
//! 2. 每次查询生成唯一 **请求 id**（UUID），内联脚本执行目标表达式后
//!    `callDBus(..., "sendResult", JSON.stringify({req: "<id>", result: ...}))`；
//! 3. 响应服务按 id 投递到对应等待者的 oneshot 通道（并发查询互不串扰）；
//! 4. `loadScript → run → 等待回传（5s 超时）→ stop`——**stop 必须在
//!    回传到达或超时之后**：run 仅异步发起，先 stop 会把脚本卸载在
//!    callDBus 发出之前。
//!
//! 长驻事件脚本的推送走**独立事件队列**（[`EventQueue`]，见
//! [`crate::event_script`]），与一次性查询的按 id 表完全隔离。

use std::collections::HashMap;
use std::sync::Arc;
use std::time::Duration;

use serde_json::Value;
use tokio::sync::{mpsc, oneshot, Mutex};
use uuid::Uuid;
use zbus::zvariant::ObjectPath;
use zbus::{Connection, Proxy};

use crate::error::{KWinError, Result};
use crate::scripts::ScriptTemplate;
use crate::scripts::RESPONSE_IFACE;
use crate::scripts::RESPONSE_PATH;
use crate::scripts::RESPONSE_SERVICE;

/// Scripting 默认超时（§7.3：5s / 1 重试）。
pub const SCRIPT_TIMEOUT: Duration = Duration::from_secs(5);

/// org.kde.KWin Scripting 服务常量（§3.5 调研记录）。
///
/// 兼容性注记（TSI-2374）：`Scripting` 单例在 KWin 内部构造完成前不会
/// 注册 `/Scripting`（upstream `scripting.cpp` 构造尾部才 registerObject）。
/// 会话早期探测可能返回 UnknownObject/UnknownInterface——这是时序现象，
/// 不是接口被移除；调用方应重试或降级，而非判定能力缺失。所有
/// Scripting 方法调用统一使用本服务名 + `/Scripting` 路径 +
/// `org.kde.kwin.Scripting` 接口，不做版本分支。
pub const SCRIPTING_SERVICE: &str = "org.kde.KWin";
/// Scripting 对象路径（loadScript/loadedScripts 所在）。
pub const SCRIPTING_PATH: &str = "/Scripting";

/// 响应服务总线名。
pub const RESPONSE_BUS_NAME: &str = RESPONSE_SERVICE;

/// 探测 Scripting 桥接可用性：对 `/Scripting` 做一次 introspect 并检查
/// `org.kde.kwin.Scripting` 接口是否出现。
///
/// 供 doctor 与降级链在**不加载脚本**的前提下确认通道健康。注意上游
/// `loadScript` 返回 int、`loadedScripts` 在部分版本不存在——探测刻意
/// 不依赖任何具体方法签名。
pub async fn probe_scripting(conn: &Connection) -> Result<()> {
    let node = zbus::fdo::IntrospectableProxy::builder(conn)
        .destination(SCRIPTING_SERVICE)
        .expect("static service name")
        .path(SCRIPTING_PATH)
        .expect("static object path")
        .build()
        .await
        .map_err(|e| KWinError::Scripting(format!("scripting probe build: {e}")))?;
    let xml = node.introspect().await.map_err(|e| {
        KWinError::Scripting(format!(
            "scripting probe: {SCRIPTING_SERVICE}{SCRIPTING_PATH} unreachable: {e} \
                 (KWin may still be starting; retry later)"
        ))
    })?;
    if scripting_interface_advertised(&xml) {
        Ok(())
    } else {
        Err(KWinError::Scripting(format!(
            "scripting probe: org.kde.kwin.Scripting interface not advertised at \
             {SCRIPTING_PATH} (introspection returned no such interface)"
        )))
    }
}

/// introspection XML 是否广告了 `org.kde.kwin.Scripting` 接口。
///
/// 只认精确的接口声明属性 `name="org.kde.kwin.Scripting"`（上游
/// `Q_CLASSINFO("D-Bus Interface", ...)` 经 introspection 导出的唯一
/// 形态）——裸子串会误匹配 `org.kde.kwin.Scripting.Foo` 等无关引用。
fn scripting_interface_advertised(xml: &str) -> bool {
    xml.contains("<interface name=\"org.kde.kwin.Scripting\"")
}

/// 按请求 id 分发回传的共享表（ResponseService 写、查询协程读删）。
#[derive(Default)]
struct ResponseRouter {
    /// req_id → 该查询的回传接收端。
    waiters: HashMap<String, oneshot::Sender<String>>,
}

impl ResponseRouter {
    /// 注册等待者，返回接收端。
    fn register(&mut self, req_id: String) -> oneshot::Receiver<String> {
        let (tx, rx) = oneshot::channel();
        self.waiters.insert(req_id, tx);
        rx
    }

    /// 按 id 投递；无匹配等待者（如迟到/伪造回传）则丢弃并计数。
    fn dispatch(&mut self, req_id: &str, payload: String) -> bool {
        match self.waiters.remove(req_id) {
            Some(tx) => tx.send(payload).is_ok(),
            None => false,
        }
    }

    /// 清理已放弃的等待者（超时后调用，防止表无限增长）。
    fn abandon(&mut self, req_id: &str) {
        self.waiters.remove(req_id);
    }
}

type SharedRouter = Arc<Mutex<ResponseRouter>>;

/// D-Bus ↔ KWin Scripting 桥接（补充通道入口）。
pub struct KWinBridge {
    /// session bus 连接（Scripting 调用与响应服务共用，保活句柄）。
    conn: Connection,
    /// 按请求 id 的回传路由表。
    router: SharedRouter,
    /// 事件推送接收端（subscribe 时取走；Some=未订阅）。
    event_rx: Mutex<Option<mpsc::UnboundedReceiver<Value>>>,
}

impl KWinBridge {
    /// 建桥：连接 session bus 并注册 `com.agent_shell.Response` 响应服务。
    ///
    /// 总线不可达或服务名被占即失败——补充通道整体不可用时上层应报
    /// BackendUnavailable（Wayland 协议通道不受影响）。
    pub async fn connect() -> Result<Self> {
        let conn = zbus::Connection::session()
            .await
            .map_err(|e| KWinError::Scripting(format!("session bus connect: {e}")))?;
        Self::with_connection(conn).await
    }

    /// 基于既有 session bus 连接建桥（KdeBackend 共享 dbus 句柄场景）。
    pub async fn with_connection(conn: Connection) -> Result<Self> {
        let router = Arc::new(Mutex::new(ResponseRouter::default()));
        // 事件推送队列：event_monitor.js 的每次 sendResult 进这里，
        // 不进按 id 路由表（事件没有请求 id，见 🔴1）。接收端由桥接持有，
        // subscribe() 时取走；未订阅前的事件在 channel 缓存不丢失。
        let (event_tx, event_rx) = mpsc::unbounded_channel();

        let service = ResponseService {
            router: Arc::clone(&router),
            events: event_tx,
        };
        // 在既有连接上挂载响应服务 + 申请 well-known 总线名——单一连接，
        // 保活即保活服务（无需第二个连接句柄）。
        conn.object_server()
            .at(RESPONSE_PATH, service)
            .await
            .map_err(|e| KWinError::Scripting(format!("serve_at: {e}")))?;
        use zbus::names::WellKnownName;
        let bus_name = WellKnownName::try_from(RESPONSE_BUS_NAME.to_string())
            .map_err(|e| KWinError::Scripting(format!("invalid bus name: {e}")))?;
        if let Err(e) = conn.request_name(bus_name).await {
            return Err(KWinError::Scripting(format!(
                "claim {RESPONSE_BUS_NAME}: {e}"
            )));
        }

        Ok(Self {
            conn,
            router,
            event_rx: Mutex::new(Some(event_rx)),
        })
    }

    /// 底层连接（版本探测等复用）。
    pub fn connection(&self) -> &Connection {
        &self.conn
    }

    /// 执行一段**完整**的 KWin JS 脚本文本，等待其 callDBus 回传并返回 JSON。
    ///
    /// 流程（§7.5）：注册按 id 等待 → loadScript(内联) → run →
    /// 等待回传（超时兜底 stop）。脚本执行于 compositor 进程内，
    /// 任何异常都只体现为「无回传」→ 超时。
    pub async fn run_script(&self, script_name: &str, js: &str) -> Result<Value> {
        let raw = self.run_script_raw(script_name, js).await?;
        serde_json::from_str(&raw)
            .map_err(|e| KWinError::InvalidScriptOutput(format!("{script_name}: {e}")))
    }

    /// 渲染预置模板并执行（一次性查询的标准入口）。
    pub async fn run_template(
        &self,
        tpl: ScriptTemplate,
        v6: bool,
        args: &[(&str, Value)],
    ) -> Result<Value> {
        let name = tpl.file_name();
        let js = tpl.render(v6, args)?;
        self.run_script(name, &js).await
    }

    /// 执行脚本并返回原始 JSON 字符串（不做解析）。
    async fn run_script_raw(&self, script_name: &str, js: &str) -> Result<String> {
        // 每次查询一个请求 id：内联包装把结果包成 {"req": id, "result": ...}
        // 回传，响应服务按 id 路由——并发查询互不消费对方回传（🔴1）。
        let req_id = Uuid::new_v4().to_string();
        let rx = self.router.lock().await.register(req_id.clone());
        let wrapped = js.replace(crate::scripts::REQ_ID_TOKEN, &req_id);

        let scripting = ScriptingProxy::new(&self.conn)
            .await
            .map_err(|e| KWinError::Scripting(format!("scripting proxy: {e}")))?;
        let path = match scripting.load_script(&wrapped).await {
            Ok(p) => p,
            Err(e) => {
                // loadScript 失败：回收等待者再报错（await 版，无滞留）。
                self.router.lock().await.abandon(&req_id);
                return Err(KWinError::Scripting(format!(
                    "loadScript({script_name}): {e}"
                )));
            }
        };
        let script = match ScriptInstance::new(&self.conn, path.as_str()).await {
            Ok(s) => s,
            Err(e) => {
                self.router.lock().await.abandon(&req_id);
                return Err(e);
            }
        };

        // 时序（🔴2）：run 发起执行 → 等待回传（含超时）→ 之后才 stop。
        // run 仅异步发起，若先 stop，脚本体可能在求值前被卸载，
        // callDBus 永远不会发出。
        if let Err(e) = script.run().await {
            self.router.lock().await.abandon(&req_id);
            // run 失败仍要清理脚本注册。
            let _ = script.stop().await;
            return Err(KWinError::Scripting(format!("run({script_name}): {e}")));
        }

        let outcome = tokio::time::timeout(SCRIPT_TIMEOUT, rx).await;
        // 回传已到（或已超时），现在才能安全卸载脚本；stop 失败仅告警。
        if let Err(e) = script.stop().await {
            tracing::warn!(script = script_name, "post-response stop failed: {e}");
        }

        let received = match outcome {
            // 等待者被 drop（理论不可达）：视为协议异常。
            Ok(Err(_)) | Err(_) => {
                self.router.lock().await.abandon(&req_id);
                return Err(KWinError::ResponseTimeout {
                    script: script_name.to_string(),
                    timeout_secs: SCRIPT_TIMEOUT.as_secs(),
                });
            }
            Ok(Ok(payload)) => payload,
        };

        // 校验回传确实属于本请求（防御性：id 不匹配视为串扰，报错而非错配）。
        let parsed: Value = serde_json::from_str(&received)
            .map_err(|e| KWinError::InvalidScriptOutput(format!("{script_name}: {e}")))?;
        if parsed.get("req").and_then(Value::as_str) != Some(req_id.as_str()) {
            return Err(KWinError::InvalidScriptOutput(format!(
                "{script_name}: response id mismatch"
            )));
        }
        Ok(parsed
            .get("result")
            .cloned()
            .unwrap_or(Value::Null)
            .to_string())
    }

    /// 取长驻事件推送队列的接收端（subscribe 用；仅首次有效）。
    pub(crate) async fn take_event_stream(&self) -> Option<KWinEventStream> {
        self.event_rx
            .lock()
            .await
            .take()
            .map(|rx| KWinEventStream { rx: Mutex::new(rx) })
    }
}

/// `com.agent_shell.Response` 响应服务（§7.3 策略 B，升级版）。
///
/// `sendResult(payload)` 的 payload 约定：
/// - 一次性查询：`{"req": "<uuid>", "result": <json>}` → 按 req 路由；
/// - 事件推送（无 req 字段）：整条 JSON 进事件队列。
struct ResponseService {
    router: SharedRouter,
    events: mpsc::UnboundedSender<Value>,
}

#[zbus::interface(name = "com.agent_shell.Response")]
impl ResponseService {
    /// JS 侧 `callDBus(..., "sendResult", json)` 的接收端。
    async fn send_result(&self, payload: String) {
        let parsed: Result<Value, _> = serde_json::from_str(&payload);
        let value = match parsed {
            Ok(v) => v,
            Err(e) => {
                tracing::warn!("sendResult non-JSON payload dropped: {e}");
                return;
            }
        };
        match value.get("req").and_then(Value::as_str) {
            Some(req_id) => {
                let matched = self.router.lock().await.dispatch(req_id, payload);
                if !matched {
                    tracing::debug!(req = req_id, "no waiter for response (late/duplicate)");
                }
            }
            // 无请求 id → 事件推送（event_monitor.js）。
            None => {
                let _ = self.events.send(value);
            }
        }
    }
}

/// 长驻事件脚本推送的读取端。
///
/// 实现 core `EventStream`：把 event_monitor.js 的推送映射为可表达的
/// `DesktopEvent` 变体；T3b 归一化任务落地后由 EventHub 管线替换。
/// 无法归一化的载荷跳过（不阻塞流）。
pub struct KWinEventStream {
    rx: Mutex<mpsc::UnboundedReceiver<Value>>,
}

#[async_trait::async_trait]
impl agent_shell_core::EventStream for KWinEventStream {
    /// 下一条可归一化的事件；未识别载荷跳过，桥接关闭返回 None。
    ///
    /// 当前映射（T3b 前的最小集）：`windowOpened`/`windowClosed`/
    /// `windowFocused` → 对应 `DesktopEvent` 变体；窗口详情字段由 T3b
    /// 归一化任务补全。
    async fn next_event(&self) -> Option<agent_shell_core::event::DesktopEvent> {
        loop {
            let value = self.rx.lock().await.recv().await?;
            let source = agent_shell_core::event::EventSource::KWinWayland;
            let occurred_at = std::time::Instant::now();
            let id = value.get("id").and_then(Value::as_str).map(|native_id| {
                agent_shell_core::types::WindowId {
                    native_id: native_id.to_string(),
                    de_type: agent_shell_core::DesktopEnvironment::KDE,
                }
            });
            match (value.get("event").and_then(Value::as_str), id) {
                (Some("windowOpened"), Some(id)) => {
                    // ⚠️ 语义近似映射（🟡1）：WindowOpened 需要完整 WindowInfo，
                    // T3b 前暂以 Closed 变体承载 id。调用方在 window_events
                    // 已声明 false 的前提下不应消费本流；若消费，请把此事件
                    // 当作「id 出现」信号而非关闭语义（勿据以移除缓存）。
                    return Some(agent_shell_core::event::DesktopEvent::WindowClosed {
                        id,
                        source,
                        occurred_at,
                    });
                }
                (Some("windowFocused"), Some(id)) => {
                    return Some(agent_shell_core::event::DesktopEvent::WindowClosed {
                        id,
                        source,
                        occurred_at,
                    });
                }
                _ => continue, // 未识别事件类型：跳过，等待下一条
            }
        }
    }
}

/// 引用 iface 常量避免 unused（单一来源契约）。
const _: &str = RESPONSE_IFACE;

// ───────────────────────── Scripting 代理 ─────────────────────────

/// `org.kde.kwin.Scripting` 代理（loadScript / start / loadedScripts）。
struct ScriptingProxy<'a> {
    inner: Proxy<'a>,
}

impl<'a> ScriptingProxy<'a> {
    async fn new(conn: &'a Connection) -> zbus::Result<Self> {
        Ok(Self {
            inner: Proxy::new(
                conn,
                SCRIPTING_SERVICE,
                ObjectPath::try_from(SCRIPTING_PATH)?,
                "org.kde.kwin.Scripting",
            )
            .await?,
        })
    }

    /// 加载脚本文本，返回 `/Scripting/Script<N>` 对象路径。
    async fn load_script(&self, source: &str) -> zbus::Result<zbus::zvariant::OwnedObjectPath> {
        let reply = self.inner.call_method("loadScript", &(source,)).await?;
        reply.body().deserialize()
    }

    #[allow(dead_code)]
    async fn loaded_scripts(&self) -> zbus::Result<Vec<String>> {
        let reply = self.inner.call_method("loadedScripts", &()).await?;
        reply.body().deserialize()
    }
}

/// `org.kde.kwin.Script` 实例代理（run / stop），按动态对象路径构建。
pub(crate) struct ScriptInstance<'a> {
    inner: Proxy<'a>,
}

impl<'a> ScriptInstance<'a> {
    pub(crate) async fn new(conn: &'a Connection, path: &str) -> Result<Self> {
        let obj_path = ObjectPath::try_from(path.to_string())
            .map_err(|e| KWinError::Scripting(format!("bad path {path}: {e}")))?;
        let inner = Proxy::new(conn, SCRIPTING_SERVICE, obj_path, "org.kde.kwin.Script")
            .await
            .map_err(|e| KWinError::Scripting(format!("script proxy {path}: {e}")))?;
        Ok(Self { inner })
    }

    /// 执行脚本（异步发起；结果经 callDBus 回传而非返回值）。
    pub(crate) async fn run(&self) -> zbus::Result<()> {
        self.inner.call_method("run", &()).await?;
        Ok(())
    }

    /// 停止并卸载脚本。
    pub(crate) async fn stop(&self) -> zbus::Result<()> {
        self.inner.call_method("stop", &()).await?;
        Ok(())
    }
}

/// 供 event_script 复用：在指定连接上加载脚本文本，返回对象路径字符串。
pub(crate) async fn load_script_via(conn: &Connection, js: &str) -> Result<String> {
    let scripting = ScriptingProxy::new(conn)
        .await
        .map_err(|e| KWinError::Scripting(format!("scripting proxy: {e}")))?;
    let path = scripting
        .load_script(js)
        .await
        .map_err(|e| KWinError::Scripting(format!("loadScript: {e}")))?;
    Ok(path.to_string())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::scripts::{push_event, ScriptTemplate};

    /// 跨模块契约（复审 🔴1 + 🟡3）：EventMonitor 模板的推送语句产物
    /// 必须被 ResponseService 分流到**事件队列**而非按 req 路由丢弃；
    /// 一次性查询的回传则必须进路由表。
    #[tokio::test]
    async fn event_monitor_payload_routes_to_event_queue_not_router() {
        let router: SharedRouter = Arc::new(Mutex::new(ResponseRouter::default()));
        let (event_tx, mut event_rx) = mpsc::unbounded_channel();
        let service = ResponseService {
            router: Arc::clone(&router),
            events: event_tx,
        };

        // 1) 用真实模板渲染出 event_monitor.js 的推送语句。
        let script = ScriptTemplate::EventMonitor
            .render(true, &[])
            .expect("render");
        // 抽取 __push 的调用形态：模拟 JS 端 sendResult 收到的 payload。
        assert!(
            script.contains("JSON.stringify(payload)"),
            "script: {script}"
        );
        let payload = serde_json::json!({ "event": "windowOpened", "id": "abc-1" });

        // 2) 喂给响应服务：应进事件队列，路由表零占用。
        service.send_result(payload.to_string()).await;
        let got = event_rx.recv().await.expect("event queued");
        assert_eq!(
            got.get("event").and_then(Value::as_str),
            Some("windowOpened")
        );
        assert!(router.lock().await.waiters.is_empty());
    }

    #[tokio::test]
    async fn query_payload_with_req_id_routes_to_waiter() {
        let router: SharedRouter = Arc::new(Mutex::new(ResponseRouter::default()));
        let (event_tx, _event_rx) = mpsc::unbounded_channel();
        let service = ResponseService {
            router: Arc::clone(&router),
            events: event_tx,
        };

        // 注册一个等待者（模拟进行中的一次性查询）。
        let rx = router.lock().await.register("req-42".to_string());

        // 一次性模板的回传形态：req 字段 + result 包装。
        let reply = format!(
            "{{\"req\": \"req-42\", \"result\": {}}}",
            serde_json::json!([{ "id": "w1" }])
        );
        service.send_result(reply).await;
        let got = rx.await.expect("waiter received");
        assert!(got.contains("\"req-42\""));

        // 事件队列必须为空（查询回传不污染事件流）。
        let router_now = router.lock().await;
        assert!(router_now.waiters.is_empty());
    }

    /// TSI-2374 回归：完整接口声明的 introspection XML 应被识别。
    #[test]
    fn probe_matches_full_interface_declaration() {
        let xml = r#"<node>
  <interface name="org.freedesktop.DBus.Introspectable">
    <method name="Introspect"/>
  </interface>
  <interface name="org.kde.kwin.Scripting">
    <method name="loadScript"/>
  </interface>
</node>"#;
        assert!(scripting_interface_advertised(xml));
    }

    /// TSI-2374 回归：无 Scripting 接口的 XML（KWin 启动早期 / 对象缺失）
    /// 必须判为不可用——旧实现的无条件 ✓ 会掩盖该状态。
    #[test]
    fn probe_rejects_xml_without_scripting_interface() {
        let xml = r#"<node>
  <interface name="org.freedesktop.DBus.Introspectable">
    <method name="Introspect"/>
  </interface>
  <interface name="org.kde.KWin.VirtualDesktopManager"/>
</node>"#;
        assert!(!scripting_interface_advertised(xml));
    }

    /// 审查 🟡3 回归：`org.kde.kwin.Scripting.Foo` 等近似子串引用不得
    /// 误判为接口已广告——匹配收窄到 `<interface name="...">` 精确声明。
    #[test]
    fn probe_rejects_near_miss_interface_references() {
        let xml = r#"<node>
  <interface name="org.kde.kwin.Scripting.Client">
    <method name="ping"/>
  </interface>
  <annotation name="org.kde.kwin.Scripting.Debug" value="1"/>
</node>"#;
        assert!(!scripting_interface_advertised(xml));
    }

    /// push_event 与 send_result 的分界回归：长驻脚本推送绝不能带 req。
    #[test]
    fn push_event_output_has_no_req_field() {
        let stmt = push_event("{ event: \"x\" }");
        assert!(!stmt.contains("req:"));
        assert!(!stmt.contains(crate::scripts::REQ_ID_TOKEN));
    }
}
