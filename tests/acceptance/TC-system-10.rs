//! TC-system-10（AC-105，P2）：全部 14 个未实现占位方法的统一回执。
//!
//! - 场景：遍历 `PLACEHOLDER_SERVICES` 的全部条目（文件 / MIME / 蓝牙 / 软件 /
//!   外设 / 密钥 / 快捷键触发），并注入带调用计数的组件 fake。
//! - 判据：每个方法都成功返回精确载荷
//!   `{"status":"not_implemented","service":"<方法名>"}`（无 RPC 错误，CLI 退出
//!   码为 0），且注入的 MIME/外设/蓝牙/软件/快捷键组件调用计数保持 0——占位
//!   不得偷偷触达任何后端；未登记的方法仍返回 `1008 MethodNotFound`。
//! - 执行：`cargo test -p agent-shell-acceptance --test TC-system-10`
//!
//! 环境限制：无——占位表是 daemon 内的编译期常量，回执与「不触达组件」都是
//! headless 可判定的确定性行为。条目数与 service 名在此独立钉住：漏登记或
//! 笔误会以 `1008`/载荷不等而非静默通过。

use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::Arc;

use acceptance::Fixture;
use agent_shell_bluetooth::BluetoothOps;
use agent_shell_core::error::Result;
use agent_shell_core::services::{
    BtDevice, DefaultAppResolution, FlatpakApp, KeyboardLayout, KeyboardLayouts, ShortcutBinding,
    TouchpadDevice, TouchpadStatus,
};
use agent_shell_core::types::KeyCombo;
use agent_shell_mime::MimeOps;
use agent_shell_peripherals::PeripheralsOps;
use agent_shell_rpc::{method, RpcErrorCode};
use agent_shell_shortcut::ShortcutOps;
use agent_shell_software::SoftwareOps;
use async_trait::async_trait;
use serde_json::json;

/// 占位方法表和期望的 service 名（与设计 §21.35 的 14 个条目一一对应）。
const PLACEHOLDERS: [(&str, &str); 14] = [
    (method::FILE_PICK, "file.pick"),
    (method::FILE_TRASH, "file.trash"),
    (method::FILE_OPEN_DIR, "file.open_directory"),
    (method::MIME_SET, "mime.set"),
    (method::BLUETOOTH_SCAN, "bluetooth.scan"),
    (method::BLUETOOTH_CONNECT, "bluetooth.connect"),
    (method::BLUETOOTH_DISCONNECT, "bluetooth.disconnect"),
    (method::FLATPAK_INSTALL, "flatpak.install"),
    (method::SOFTWARE_UPDATES, "software.updates"),
    (method::TOUCHPAD_SET, "touchpad.set"),
    (method::KBD_LAYOUT_SET, "kbd.layout.set"),
    (method::SECRET_SET, "secret.set"),
    (method::SECRET_GET, "secret.get"),
    (method::SHORTCUT_TRIGGER, "shortcut.trigger"),
];

/// 任何后端方法被调用都会累加计数；占位路径必须让它保持 0。
#[derive(Default)]
struct CountingBackend {
    touches: AtomicUsize,
}

impl CountingBackend {
    fn touches(&self) -> usize {
        self.touches.load(Ordering::SeqCst)
    }

    fn touch(&self) {
        self.touches.fetch_add(1, Ordering::SeqCst);
    }
}

#[async_trait]
impl MimeOps for CountingBackend {
    async fn default_app(&self, mime: &str) -> Result<DefaultAppResolution> {
        self.touch();
        Ok(DefaultAppResolution {
            target: mime.to_string(),
            desktop_id: Some("firefox.desktop".into()),
            source: "counting".into(),
        })
    }

    async fn default_browser(&self) -> Result<DefaultAppResolution> {
        self.touch();
        Ok(DefaultAppResolution {
            target: "web-browser".into(),
            desktop_id: None,
            source: "counting".into(),
        })
    }
}

#[async_trait]
impl PeripheralsOps for CountingBackend {
    async fn touchpad_status(&self) -> Result<TouchpadStatus> {
        self.touch();
        Ok(TouchpadStatus {
            source: "counting".into(),
            enabled: Some(true),
            natural_scroll: Some(true),
            tap_to_click: Some(true),
            devices: vec![TouchpadDevice {
                sysname: None,
                name: "counting".into(),
                enabled: true,
                natural_scroll: None,
                tap_to_click: None,
            }],
        })
    }

    async fn keyboard_layouts(&self) -> Result<KeyboardLayouts> {
        self.touch();
        Ok(KeyboardLayouts {
            source: "counting".into(),
            active_index: Some(0),
            layouts: vec![KeyboardLayout {
                index: 0,
                layout: "us".into(),
                variant: None,
                display_name: None,
            }],
        })
    }
}

#[async_trait]
impl BluetoothOps for CountingBackend {
    async fn list_devices(&self) -> Result<Vec<BtDevice>> {
        self.touch();
        Ok(Vec::new())
    }
}

#[async_trait]
impl SoftwareOps for CountingBackend {
    async fn list_flatpaks(&self) -> Result<Vec<FlatpakApp>> {
        self.touch();
        Ok(Vec::new())
    }
}

#[async_trait]
impl ShortcutOps for CountingBackend {
    async fn bind(&self, _combo: &KeyCombo, _action: &str) -> Result<ShortcutBinding> {
        self.touch();
        Ok(ShortcutBinding {
            backend: "counting".into(),
            component: None,
            combo: String::new(),
            action: String::new(),
        })
    }
}

#[tokio::test]
async fn every_placeholder_method_reports_exact_receipt_without_backend_touch() {
    let mut fixture = Fixture::start().await;
    let backend = Arc::new(CountingBackend::default());
    {
        let daemon = fixture.daemon_mut();
        daemon.mime = Some(Arc::clone(&backend) as Arc<dyn MimeOps>);
        daemon.peripherals = Some(Arc::clone(&backend) as Arc<dyn PeripheralsOps>);
        daemon.bluetooth = Some(Arc::clone(&backend) as Arc<dyn BluetoothOps>);
        daemon.software = Some(Arc::clone(&backend) as Arc<dyn SoftwareOps>);
        daemon.shortcut = Some(Arc::clone(&backend) as Arc<dyn ShortcutOps>);
    }

    // 占位表恰好 14 条：条目删除/新增都会让本断言显形。
    assert_eq!(PLACEHOLDERS.len(), 14);

    for (method_name, service) in PLACEHOLDERS {
        let response = fixture
            .call(method_name, Some(json!({ "key": "k", "value": "v" })))
            .await;
        assert!(
            response.error.is_none(),
            "{method_name} 占位不得返回错误（退出码必须为 0），got: {:?}",
            response.error
        );
        assert_eq!(
            response.result.expect("placeholder result"),
            json!({ "status": "not_implemented", "service": service }),
            "{method_name} 的占位回执必须精确"
        );
    }

    assert_eq!(
        backend.touches(),
        0,
        "14 个占位方法都不得触达 MIME/外设/蓝牙/软件/快捷键后端"
    );

    // 未登记方法不是占位：仍以 MethodNotFound 显形。
    let error = fixture
        .call_err("system.not_a_method", Some(json!({ "key": "k" })))
        .await;
    assert_eq!(
        error.code,
        RpcErrorCode::MethodNotFound as i32,
        "未知方法必须返回 1008，got: {error:?}"
    );
}
