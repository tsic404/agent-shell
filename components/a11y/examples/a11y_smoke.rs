//! 实机冒烟：GTK 窗口上验证 get_desktop/get_app_tree/active window/
//! locate(ByAccessibility)/click/get_text/set_text 全链路。

use agent_shell_a11y::{AtSpiComponent, ElementNode};
use agent_shell_core::types::SemanticTarget;
use std::process::Stdio;

#[tokio::main]
async fn main() -> Result<(), Box<dyn std::error::Error>> {
    // 启动带 a11y 的 GTK 探针窗口
    let mut child = tokio::process::Command::new("python3")
        .arg("-c")
        .arg(
            r#"
import gi
gi.require_version('Gtk', '3.0')
from gi.repository import Gtk, GLib
w = Gtk.Window(title='agent-shell a11y probe')
v = Gtk.Box()
e = Gtk.Entry()
b = Gtk.Button(label='probe-button')
v.pack_start(e, True, True, 0); v.pack_start(b, True, True, 0)
w.add(v); w.show_all()
GLib.timeout_add_seconds(60, Gtk.main_quit)
Gtk.main()
"#,
        )
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .spawn()?;
    tokio::time::sleep(std::time::Duration::from_secs(4)).await;

    let pid = child.id().unwrap_or(0);
    let result = run_probe(&pid).await;
    let _ = child.kill().await;
    result?;
    println!("SMOKE OK");
    Ok(())
}

async fn run_probe(pid: &u32) -> Result<(), Box<dyn std::error::Error>> {
    let component = AtSpiComponent::connect().await?;
    use agent_shell_core::component::DesktopComponent;
    assert!(component.is_available());

    // 1. get_desktop（list_applications）
    let apps = component.locator().bridge().list_applications().await?;
    println!("apps: {}", apps.len());
    assert!(!apps.is_empty(), "no a11y applications");

    // 2. get_app_tree by PID
    let app = component.locator().bridge().get_app_tree(*pid).await?;
    println!("app tree: name={:?} pid={:?}", app.name, app.pid);
    assert_eq!(app.pid, Some(*pid));

    // 3. active window
    let active = component.locator().bridge().get_active_window().await?;
    println!(
        "active window: {:?} states={:?}",
        active.name, active.states.0
    );

    // 4. locate ByAccessibility（button）
    let targets = component
        .locator()
        .locate(&SemanticTarget::ByAccessibility {
            role: Some("button".into()),
            name: Some("probe-button".into()),
            parent_role: None,
            parent_name: None,
        })
        .await?;
    println!("located buttons: {}", targets.len());
    assert!(!targets.is_empty(), "ByAccessibility locate returned empty");

    // 5. parent 约束收敛：在 box 内找 button
    let constrained = component
        .locator()
        .locate(&SemanticTarget::ByAccessibility {
            role: Some("button".into()),
            name: Some("probe-button".into()),
            parent_role: Some("panel".into()), // GtkBox → AT-SPI panel/filler
            parent_name: None,
        })
        .await?;
    println!("parent-constrained buttons: {}", constrained.len());
    for t in &targets {
        assert_element_wired(t);
    }

    // 6. set_text / get_text 往返一致
    let entries = component
        .locator()
        .locate(&SemanticTarget::ByAccessibility {
            role: Some("text".into()),
            name: None,
            parent_role: None,
            parent_name: None,
        })
        .await?;
    assert!(!entries.is_empty(), "no text entry located");
    let entry = &entries[0];
    component
        .actions()
        .set_text(entry, "round-trip-123")
        .await?;
    let read_back = component.actions().get_text(entry).await?;
    println!("entry text: {read_back:?}");
    assert_eq!(read_back, "round-trip-123", "set_text/get_text mismatch");

    // 7. click（Action 接口路径）
    component.actions().click(&targets[0]).await?;
    println!("click via Action interface: ok");

    Ok(())
}

fn assert_element_wired(el: &ElementNode) {
    assert!(!el.bus_name.is_empty());
    assert!(el.path.starts_with("/org/a11y/atspi/"));
}
