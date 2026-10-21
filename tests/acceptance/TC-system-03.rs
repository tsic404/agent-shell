//! TC-system-03（AC-72，P2）：默认应用查询与 MIME 占位回执。
//!
//! - 场景：`mime.get` / `mime.default_browser` 经注入的 fake MimeOps 返回
//!   `default_app`/`default_browser` 解析结果；`mime.set` 尚未接线。
//! - 判据：`mime.get` 回执携带 `mime`/`default_app`/`source` 三键、mime 串
//!   经裁剪后再查后端；缺参/空白 mime 在触达后端之前返回 `-32602`；`mime.set`
//!   返回 `{"status":"not_implemented","service":"mime.set"}` 且后端零调用。
//! - 执行：`cargo test -p agent-shell-acceptance --test TC-system-03`
//!
//! 环境限制：验收机没有真实 xdg 工具链，MIME 解析由 fake 注入 daemon 的公开
//! 装配字段（`Daemon::mime`）；请求仍走真实 `dispatch`。真实 `xdg-mime` /
//! `xdg-settings` / `mimeapps.list` 回退链的端到端行为由组件单测覆盖，此处
//! 只钉协议面。

use std::sync::{Arc, Mutex};

use acceptance::Fixture;
use agent_shell_core::error::Result;
use agent_shell_core::services::DefaultAppResolution;
use agent_shell_mime::MimeOps;
use agent_shell_rpc::{method, RpcErrorCode};
use async_trait::async_trait;
use serde_json::{json, Value};

/// 记录型 MIME fake：`default_app` 记录收到的查询串（验证裁剪）。
struct RecordingMime {
    default_app: Mutex<Vec<String>>,
}

impl RecordingMime {
    fn new() -> Self {
        Self {
            default_app: Mutex::new(Vec::new()),
        }
    }

    fn queries(&self) -> Vec<String> {
        self.default_app.lock().unwrap().clone()
    }
}

#[async_trait]
impl MimeOps for RecordingMime {
    async fn default_app(&self, mime: &str) -> Result<DefaultAppResolution> {
        self.default_app.lock().unwrap().push(mime.to_string());
        Ok(DefaultAppResolution {
            target: mime.to_string(),
            desktop_id: Some("firefox.desktop".into()),
            source: "fake-mime".into(),
        })
    }

    async fn default_browser(&self) -> Result<DefaultAppResolution> {
        Ok(DefaultAppResolution {
            target: "web-browser".into(),
            desktop_id: Some("chromium.desktop".into()),
            source: "fake-browser".into(),
        })
    }
}

fn keys(value: &Value) -> Vec<String> {
    let mut keys: Vec<String> = value
        .as_object()
        .unwrap_or_else(|| panic!("expected a JSON object, got: {value}"))
        .keys()
        .cloned()
        .collect();
    keys.sort();
    keys
}

#[tokio::test]
async fn mime_query_default_browser_and_placeholder_set() {
    let mut fixture = Fixture::start().await;
    let backend = Arc::new(RecordingMime::new());
    fixture.daemon_mut().mime = Some(Arc::clone(&backend) as Arc<dyn MimeOps>);

    // 1. mime.get：三键形状 + 值透传 + 裁剪后查询后端。
    let resolved = fixture
        .call_ok(method::MIME_GET, Some(json!({ "mime": "  text/html " })))
        .await;
    assert_eq!(
        keys(&resolved),
        vec![
            "default_app".to_string(),
            "mime".to_string(),
            "source".to_string()
        ]
    );
    assert_eq!(resolved["mime"], "text/html");
    assert_eq!(resolved["default_app"], "firefox.desktop");
    assert_eq!(resolved["source"], "fake-mime");
    assert_eq!(
        backend.queries(),
        vec!["text/html".to_string()],
        "空白必须裁剪后再查后端"
    );

    // 2. 未配置的默认应用以 null 表达，不降级成错误。
    fixture.daemon_mut().mime = Some(Arc::new(NullDefaultBrowser) as Arc<dyn MimeOps>);
    let browser = fixture.call_ok(method::MIME_DEFAULT_BROWSER, None).await;
    assert_eq!(browser["default_browser"], Value::Null);
    assert_eq!(browser["source"], "fake-unset");
    assert_eq!(
        keys(&browser),
        vec!["default_browser".to_string(), "source".to_string()]
    );

    // 3. 参数校验先于后端：缺参/空串/空白串 → -32602，后端零查询。
    let backend = Arc::new(RecordingMime::new());
    fixture.daemon_mut().mime = Some(Arc::clone(&backend) as Arc<dyn MimeOps>);
    for params in [
        Some(json!({})),
        Some(json!({ "mime": "" })),
        Some(json!({ "mime": "   " })),
        None,
    ] {
        let error = fixture.call_err(method::MIME_GET, params.clone()).await;
        assert_eq!(
            error.code,
            RpcErrorCode::InvalidParams as i32,
            "缺参/空 mime {params:?} 必须返回 -32602，got: {error:?}"
        );
    }
    assert!(backend.queries().is_empty(), "非法参数不得触达 MIME 后端");

    // 4. 无后端：mime.get 与 default_browser 都显式 1002。
    fixture.daemon_mut().mime = None;
    for method_name in [method::MIME_GET, method::MIME_DEFAULT_BROWSER] {
        let params = if method_name == method::MIME_GET {
            Some(json!({ "mime": "text/html" }))
        } else {
            None
        };
        let error = fixture.call_err(method_name, params).await;
        assert_eq!(
            error.code,
            RpcErrorCode::BackendUnavailable as i32,
            "无后端时 {method_name} 必须返回 1002，got: {error:?}"
        );
    }

    // 5. mime.set 是占位：回执 not_implemented，后端零调用。
    fixture.daemon_mut().mime = Some(Arc::clone(&backend) as Arc<dyn MimeOps>);
    let response = fixture
        .call_ok(
            method::MIME_SET,
            Some(json!({ "mime": "text/html", "app": "firefox.desktop" })),
        )
        .await;
    assert_eq!(
        response,
        json!({ "status": "not_implemented", "service": "mime.set" })
    );
    assert!(
        backend.queries().is_empty(),
        "mime.set 占位不得触达 MIME 后端"
    );
}

/// 未配置默认浏览器的 fake（`desktop_id = None` → wire 上为 null）。
struct NullDefaultBrowser;

#[async_trait]
impl MimeOps for NullDefaultBrowser {
    async fn default_app(&self, _mime: &str) -> Result<DefaultAppResolution> {
        Ok(DefaultAppResolution {
            target: _mime.to_string(),
            desktop_id: None,
            source: "fake-unset".into(),
        })
    }

    async fn default_browser(&self) -> Result<DefaultAppResolution> {
        Ok(DefaultAppResolution {
            target: "web-browser".into(),
            desktop_id: None,
            source: "fake-unset".into(),
        })
    }
}
