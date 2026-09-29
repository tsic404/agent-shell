//! KDE 后端：KWin `InputDevice` D-Bus 通道（触控板）+ `kxkbrc`（键盘布局）。
//!
//! 触控板的运行时状态由 KWin 挂在 D-Bus 上（`InputDeviceManager.ListTouch`
//! 枚举 sysname，逐设备读属性）；键盘布局则只在 `$XDG_CONFIG_HOME/kxkbrc` 的
//! `[Layout]` 段持久化，KWin 不通过 D-Bus 暴露布局列表，故从配置文件解析。

use std::path::Path;

use agent_shell_core::error::{AgentShellError, Result};
use agent_shell_core::services::{KeyboardLayout, KeyboardLayouts, TouchpadDevice, TouchpadStatus};
use zbus::proxy;

use crate::{KWIN_INPUT_DEVICE_PREFIX, SOURCE_KDE_KXKBRC, SOURCE_KDE_TOUCHPAD};

/// KWin 输入设备管理器：枚举触控板 sysname。
#[proxy(
    interface = "org.kde.KWin.InputDeviceManager",
    default_service = "org.kde.KWin",
    default_path = "/org/kde/KWin/InputDevice"
)]
trait InputDeviceManager {
    /// 返回触控板设备的 sysname 列表（空列表是合法答案）。
    fn list_touch(&self) -> zbus::Result<Vec<String>>;
}

/// 单个 KWin 输入设备：属性即当前生效值。
#[proxy(
    interface = "org.kde.KWin.InputDevice",
    default_service = "org.kde.KWin",
    assume_defaults = true
)]
trait InputDevice {
    /// 设备是否启用。
    #[zbus(property, name = "enabled")]
    fn enabled(&self) -> zbus::Result<bool>;
    /// 自然滚动是否开启。
    #[zbus(property, name = "naturalScroll")]
    fn natural_scroll(&self) -> zbus::Result<bool>;
    /// 点击手势是否开启。
    #[zbus(property, name = "tapToClick")]
    fn tap_to_click(&self) -> zbus::Result<bool>;
    /// 设备名。
    #[zbus(property, name = "name")]
    fn name(&self) -> zbus::Result<String>;
}

/// 读 KWin 触控板状态：逐设备明细 + 首个设备投影到顶层。
pub(crate) async fn touchpad(conn: Option<&zbus::Connection>) -> Result<TouchpadStatus> {
    let Some(conn) = conn else {
        return Err(AgentShellError::BackendUnavailable("no session bus".into()));
    };
    let manager = InputDeviceManagerProxy::builder(conn)
        .cache_properties(zbus::proxy::CacheProperties::No)
        .build()
        .await
        .map_err(|e| AgentShellError::DBus(format!("InputDeviceManager: {e}")))?;
    let sysnames = manager
        .list_touch()
        .await
        .map_err(|e| AgentShellError::DBus(format!("ListTouch: {e}")))?;
    let mut devices = Vec::with_capacity(sysnames.len());
    for sysname in sysnames {
        devices.push(device(conn, &sysname).await?);
    }
    let first = devices.first();
    Ok(TouchpadStatus {
        source: SOURCE_KDE_TOUCHPAD.into(),
        enabled: first.map(|d| d.enabled),
        natural_scroll: first.and_then(|d| d.natural_scroll),
        tap_to_click: first.and_then(|d| d.tap_to_click),
        devices,
    })
}

/// 读单个设备的四个属性；任一项失败即整段失败，避免半套数据误导上层。
async fn device(conn: &zbus::Connection, sysname: &str) -> Result<TouchpadDevice> {
    let path = format!("{KWIN_INPUT_DEVICE_PREFIX}/{sysname}");
    let proxy = InputDeviceProxy::builder(conn)
        .path(path.as_str())
        .map_err(|e| AgentShellError::DBus(format!("InputDevice path {path}: {e}")))?
        .cache_properties(zbus::proxy::CacheProperties::No)
        .build()
        .await
        .map_err(|e| AgentShellError::DBus(format!("InputDevice {path}: {e}")))?;
    Ok(TouchpadDevice {
        sysname: Some(sysname.to_string()),
        name: proxy
            .name()
            .await
            .map_err(|e| property_error(&path, "name", e))?,
        enabled: proxy
            .enabled()
            .await
            .map_err(|e| property_error(&path, "enabled", e))?,
        natural_scroll: Some(
            proxy
                .natural_scroll()
                .await
                .map_err(|e| property_error(&path, "naturalScroll", e))?,
        ),
        tap_to_click: Some(
            proxy
                .tap_to_click()
                .await
                .map_err(|e| property_error(&path, "tapToClick", e))?,
        ),
    })
}

/// D-Bus 属性读取失败归一（带设备路径与属性名，便于链路诊断）。
fn property_error(path: &str, property: &str, e: zbus::Error) -> AgentShellError {
    AgentShellError::DBus(format!("{path} {property}: {e}"))
}

/// 读 `kxkbrc` 的布局列表。
pub(crate) async fn layouts(kxkbrc: &Path) -> Result<KeyboardLayouts> {
    let content = match tokio::fs::read_to_string(kxkbrc).await {
        Ok(content) => content,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => {
            return Err(AgentShellError::BackendUnavailable(format!(
                "{} not found",
                kxkbrc.display()
            )))
        }
        Err(e) => {
            return Err(AgentShellError::Other(
                format!("read {}: {e}", kxkbrc.display()).into(),
            ))
        }
    };
    let layouts = parse_kxkbrc(&content);
    if layouts.is_empty() {
        return Err(AgentShellError::BackendUnavailable(format!(
            "{} has no [Layout] list",
            kxkbrc.display()
        )));
    }
    Ok(KeyboardLayouts {
        source: SOURCE_KDE_KXKBRC.into(),
        // KWin 只在内存里持当前组号，不落盘，故无法给出生效序号。
        active_index: None,
        layouts,
    })
}

/// 解析 `kxkbrc` 的 `[Layout]` 段（其余段忽略）。
///
/// `LayoutList` 是逗号分隔的布局代码；`VariantList` / `DisplayNames` 与之按下标
/// 对齐，可缺失、可空、可更短——定点取值而非过滤，否则短列表会整体左移错位。
fn parse_kxkbrc(content: &str) -> Vec<KeyboardLayout> {
    let mut in_layout = false;
    let mut layout_list: Option<&str> = None;
    let mut variant_list: Option<&str> = None;
    let mut display_names: Option<&str> = None;
    for line in content.lines() {
        let line = line.trim();
        if line.is_empty() || line.starts_with('#') {
            continue;
        }
        if let Some(section) = line.strip_prefix('[').and_then(|l| l.strip_suffix(']')) {
            in_layout = section.trim() == "Layout";
            continue;
        }
        if !in_layout {
            continue;
        }
        let Some((key, value)) = line.split_once('=') else {
            continue;
        };
        match key.trim() {
            "LayoutList" => layout_list = Some(value.trim()),
            "VariantList" => variant_list = Some(value.trim()),
            "DisplayNames" => display_names = Some(value.trim()),
            _ => {}
        }
    }
    let Some(layout_list) = layout_list else {
        return Vec::new();
    };
    let codes = split_csv(layout_list);
    let variants = variant_list.map(split_csv).unwrap_or_default();
    let names = display_names.map(split_csv).unwrap_or_default();
    codes
        .iter()
        .enumerate()
        .filter(|(_, code)| !code.is_empty())
        .map(|(i, code)| KeyboardLayout {
            index: i as u32,
            layout: code.clone(),
            variant: non_empty_at(&variants, i),
            display_name: non_empty_at(&names, i),
        })
        .collect()
}

/// 逗号拆分并 trim（保留空项以维持下标对齐）。
fn split_csv(value: &str) -> Vec<String> {
    value.split(',').map(|s| s.trim().to_string()).collect()
}

/// 取对齐列表第 `i` 项；越界或空串 → None。
fn non_empty_at(items: &[String], i: usize) -> Option<String> {
    items.get(i).filter(|s| !s.is_empty()).cloned()
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::testsupport::TestBus;

    /// mock：`org.kde.KWin.InputDeviceManager`（返回固定 sysname 列表）。
    struct FakeManager {
        sysnames: Vec<String>,
    }

    #[zbus::interface(name = "org.kde.KWin.InputDeviceManager")]
    impl FakeManager {
        fn list_touch(&self) -> Vec<String> {
            self.sysnames.clone()
        }
    }

    /// mock：单个 `org.kde.KWin.InputDevice` 属性集。
    struct FakeDevice {
        enabled: bool,
        natural_scroll: bool,
        tap_to_click: bool,
        name: String,
    }

    #[zbus::interface(name = "org.kde.KWin.InputDevice")]
    impl FakeDevice {
        #[zbus(property, name = "enabled")]
        fn enabled(&self) -> bool {
            self.enabled
        }

        #[zbus(property, name = "naturalScroll")]
        fn natural_scroll(&self) -> bool {
            self.natural_scroll
        }

        #[zbus(property, name = "tapToClick")]
        fn tap_to_click(&self) -> bool {
            self.tap_to_click
        }

        #[zbus(property, name = "name")]
        fn name(&self) -> String {
            self.name.clone()
        }
    }

    /// 在私有 bus 上起一个只实现 InputDevice 两接口的 KWin。
    async fn fake_kwin(
        bus: &TestBus,
        sysnames: Vec<String>,
        devices: Vec<(String, FakeDevice)>,
    ) -> zbus::Connection {
        let conn = bus.connect().await;
        let server = conn.object_server();
        server
            .at(KWIN_INPUT_DEVICE_PREFIX, FakeManager { sysnames })
            .await
            .expect("register InputDeviceManager");
        for (sysname, device) in devices {
            server
                .at(format!("{KWIN_INPUT_DEVICE_PREFIX}/{sysname}"), device)
                .await
                .expect("register InputDevice");
        }
        use zbus::names::WellKnownName;
        let name = WellKnownName::try_from("org.kde.KWin".to_string()).expect("valid bus name");
        conn.request_name(name).await.expect("claim org.kde.KWin");
        conn
    }

    #[tokio::test]
    async fn kwin_lists_devices_and_projects_first_to_top_level() {
        let _guard = crate::testsupport::fork_guard().await;
        let bus = TestBus::start();
        let conn = fake_kwin(
            &bus,
            vec!["event3".into(), "event7".into()],
            vec![
                (
                    "event3".into(),
                    FakeDevice {
                        enabled: true,
                        natural_scroll: false,
                        tap_to_click: true,
                        name: "Elan Touchpad".into(),
                    },
                ),
                (
                    "event7".into(),
                    FakeDevice {
                        enabled: false,
                        natural_scroll: true,
                        tap_to_click: false,
                        name: "USB Touchpad".into(),
                    },
                ),
            ],
        )
        .await;

        let status = touchpad(Some(&conn)).await.unwrap();
        assert_eq!(status.source, SOURCE_KDE_TOUCHPAD);
        assert_eq!(status.devices.len(), 2);
        assert_eq!(status.devices[0].sysname.as_deref(), Some("event3"));
        assert_eq!(status.devices[0].name, "Elan Touchpad");
        assert!(status.devices[0].enabled);
        assert_eq!(status.devices[0].natural_scroll, Some(false));
        assert_eq!(status.devices[1].sysname.as_deref(), Some("event7"));
        assert!(!status.devices[1].enabled);
        // 顶层投影复制首个设备，而不是任取/取或。
        assert_eq!(status.enabled, Some(true));
        assert_eq!(status.natural_scroll, Some(false));
        assert_eq!(status.tap_to_click, Some(true));
    }

    #[tokio::test]
    async fn kwin_empty_touch_list_is_a_valid_answer() {
        let _guard = crate::testsupport::fork_guard().await;
        let bus = TestBus::start();
        let conn = fake_kwin(&bus, Vec::new(), Vec::new()).await;
        let status = touchpad(Some(&conn)).await.unwrap();
        assert_eq!(status.source, SOURCE_KDE_TOUCHPAD);
        assert!(status.devices.is_empty());
        assert_eq!(status.enabled, None);
        assert_eq!(status.natural_scroll, None);
        assert_eq!(status.tap_to_click, None);
    }

    #[tokio::test]
    async fn kwin_without_session_bus_is_unavailable() {
        let _guard = crate::testsupport::fork_guard().await;
        let err = touchpad(None).await.unwrap_err();
        assert!(
            matches!(err, AgentShellError::BackendUnavailable(_)),
            "got {err:?}"
        );
    }

    #[test]
    fn kxkbrc_aligns_variants_and_display_names() {
        let content = "[Layout]\nUse=true\nLayoutList=us,ru\n\
                       VariantList=,phonetic\nDisplayNames=English,Русский\n";
        let layouts = parse_kxkbrc(content);
        assert_eq!(layouts.len(), 2);
        assert_eq!(layouts[0].index, 0);
        assert_eq!(layouts[0].layout, "us");
        assert_eq!(layouts[0].variant, None);
        assert_eq!(layouts[0].display_name.as_deref(), Some("English"));
        assert_eq!(layouts[1].index, 1);
        assert_eq!(layouts[1].layout, "ru");
        assert_eq!(layouts[1].variant.as_deref(), Some("phonetic"));
        assert_eq!(layouts[1].display_name.as_deref(), Some("Русский"));
    }

    #[test]
    fn kxkbrc_missing_variant_and_display_name_keys_yield_none() {
        let layouts = parse_kxkbrc("[Layout]\nLayoutList=us,ru\n");
        assert_eq!(layouts.len(), 2);
        assert_eq!(layouts[0].variant, None);
        assert_eq!(layouts[1].variant, None);
        assert_eq!(layouts[0].display_name, None);
        assert_eq!(layouts[1].display_name, None);
    }

    #[test]
    fn kxkbrc_short_variant_list_does_not_shift_indices() {
        // VariantList 比 LayoutList 短：第 2 项无变体，phonetic 绝不能被左移。
        let layouts = parse_kxkbrc("[Layout]\nLayoutList=us,ru,de\nVariantList=phonetic\n");
        assert_eq!(layouts[0].variant.as_deref(), Some("phonetic"));
        assert_eq!(layouts[1].variant, None);
        assert_eq!(layouts[2].variant, None);
    }

    #[test]
    fn kxkbrc_empty_variant_list_entries_map_to_none() {
        let layouts = parse_kxkbrc("[Layout]\nLayoutList=us,ru\nVariantList=,\n");
        assert_eq!(layouts[0].variant, None);
        assert_eq!(layouts[1].variant, None);
    }

    #[test]
    fn kxkbrc_ignores_other_sections_and_requires_layout_list() {
        let content = "[Layout]\nLayoutList=us\n[General]\nLayoutList=de\n";
        let layouts = parse_kxkbrc(content);
        assert_eq!(layouts.len(), 1);
        assert_eq!(layouts[0].layout, "us");
        assert!(parse_kxkbrc("[General]\nLayoutList=de\n").is_empty());
        assert!(parse_kxkbrc("").is_empty());
    }

    #[tokio::test]
    async fn kde_layouts_reads_kxkbrc_file() {
        let dir = tempfile::tempdir().unwrap();
        let file = dir.path().join("kxkbrc");
        std::fs::write(&file, "[Layout]\nLayoutList=us,ru\nVariantList=,phonetic\n").unwrap();
        let result = layouts(&file).await.unwrap();
        assert_eq!(result.source, SOURCE_KDE_KXKBRC);
        assert_eq!(result.active_index, None);
        assert_eq!(result.layouts.len(), 2);
        assert_eq!(result.layouts[1].layout, "ru");
        assert_eq!(result.layouts[1].variant.as_deref(), Some("phonetic"));
    }

    #[tokio::test]
    async fn kde_layouts_missing_file_is_unavailable() {
        let dir = tempfile::tempdir().unwrap();
        let err = layouts(&dir.path().join("absent")).await.unwrap_err();
        assert!(
            matches!(err, AgentShellError::BackendUnavailable(_)),
            "got {err:?}"
        );
    }
}
