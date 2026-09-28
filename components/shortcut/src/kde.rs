//! KDE 全局快捷键后端（KGlobalAccel「命令快捷键服务」，§21.33）。
//!
//! KGlobalAccel 只绑定**已注册组件**的动作：普通组件由常驻应用经 D-Bus 注册，
//! 而 agent-shell 的 daemon 是瞬态进程（一次 CLI 调用一个进程），因此走
//! `KServiceActionComponent`——由 kglobalacceld 自己读 desktop 文件并执行
//! `Exec`。落地三段（2026-09 Plasma 6.7 实测）：
//!
//! 1. 写 `$XDG_DATA_HOME/kglobalaccel/<component>`（`X-KDE-GlobalAccel-CommandShortcut=true`）
//! 2. `doRegister`：kglobalacceld 的 `getOrCreateComponent` 见 `.desktop` 后缀即
//!    解析该文件，组件在运行期即可用（无需重启会话）
//! 3. `setForeignShortcut(…, ["_launch"], [qt_keycode])`：绑定并持久化到
//!    `kglobalshortcutsrc` 的 `[services][<component>] _launch=…`
//!
//! 绑定后回读 `shortcutKeys` 校验生效——按键被别的快捷键占用时 KGlobalAccel
//! 会静默丢弃，不校验就会向调用方谎报成功。

use crate::combo;
use agent_shell_core::error::{AgentShellError, Result};
use agent_shell_core::services::ShortcutBinding;
use agent_shell_core::types::KeyCombo;
use std::path::{Path, PathBuf};
use zbus::zvariant::OwnedObjectPath;

/// 抽象命令快捷键组件的前缀（KGlobalAccel 组件唯一名 = desktop 文件名）。
const COMPONENT_PREFIX: &str = "agent-shell-";
/// 命令快捷键动作的唯一名（kglobalacceld `KServiceActionComponent` 固定用它）。
const LAUNCH_ACTION: &str = "_launch";
/// desktop 文件 `Name` 字段长度上限（KDE 快捷键 KCM 单行展示）。
const LABEL_MAX_CHARS: usize = 64;

/// KGlobalAccel 会话接口（session bus）。
#[zbus::proxy(
    interface = "org.kde.KGlobalAccel",
    default_service = "org.kde.kglobalaccel",
    default_path = "/kglobalaccel"
)]
trait KGlobalAccel {
    // KGlobalAccel 的方法名是 Qt 侧原样的 camelCase（非 D-Bus 惯例的 PascalCase），
    // zbus 默认转换会发出 `DoRegister` 这类不存在的名字——必须逐个钉住。
    /// 已注册组件列表（只读，用作可用性探测）。
    #[zbus(name = "allComponents")]
    fn all_components(&self) -> zbus::Result<Vec<OwnedObjectPath>>;
    /// 注册动作（组件唯一名以 `.desktop` 结尾时 kglobalacceld 解析对应文件）。
    #[zbus(name = "doRegister")]
    fn do_register(&self, action_id: &[&str]) -> zbus::Result<()>;
    /// 绑定动作的快捷键（`ai` = Qt keycode 数组）。
    #[zbus(name = "setForeignShortcut")]
    fn set_foreign_shortcut(&self, action_id: &[&str], keys: &[i32]) -> zbus::Result<()>;
    /// 读回动作已绑定的快捷键；`shortcutKeys` 返回 `a(ai)`（QSet<QKeySequence>，
    /// 每个 QKeySequence 是含至多 4 个键码的 struct，不是裸 `ai`）。
    #[zbus(name = "shortcutKeys")]
    fn shortcut_keys(&self, action_id: &[&str]) -> zbus::Result<Vec<(Vec<i32>,)>>;
}

/// kglobalaccel 是否在会话总线上可用。
pub(crate) async fn ready(conn: &zbus::Connection) -> std::result::Result<(), String> {
    let proxy = KGlobalAccelProxy::new(conn)
        .await
        .map_err(|e| probe_reason(&e))?;
    proxy.all_components().await.map(|_| ()).map_err(|e| {
        if is_service_gone(&e) {
            "org.kde.kglobalaccel not running".to_string()
        } else {
            format!("org.kde.kglobalaccel: {e}")
        }
    })
}

/// 绑定：写组件文件 → 运行期注册 → 设置并回读校验快捷键。
pub(crate) async fn bind(
    conn: &zbus::Connection,
    data_home: &Path,
    combo: &KeyCombo,
    action: &str,
) -> Result<ShortcutBinding> {
    let keycode = combo::qt_keycode(combo).map_err(|e| AgentShellError::Other(e.into()))?;
    let component = component_id(action);
    let label = label_for(action);
    write_command_desktop(data_home, &component, &label, action)?;

    let proxy = KGlobalAccelProxy::new(conn)
        .await
        .map_err(|e| AgentShellError::DBus(format!("kglobalaccel proxy: {e}")))?;
    // 空 friendly 名让 kglobalacceld 沿用 desktop 文件里的 Name（同 KDE 快捷键
    // KCM 的「注册哑动作以触发解析」手法）。
    let register_id = [component.as_str(), label.as_str(), "", ""];
    proxy
        .do_register(&register_id)
        .await
        .map_err(|e| AgentShellError::DBus(format!("doRegister({component}): {e}")))?;

    let action_id = [
        component.as_str(),
        LAUNCH_ACTION,
        label.as_str(),
        label.as_str(),
    ];
    proxy
        .set_foreign_shortcut(&action_id, &[keycode])
        .await
        .map_err(|e| AgentShellError::DBus(format!("setForeignShortcut({component}): {e}")))?;

    let bound = proxy
        .shortcut_keys(&action_id)
        .await
        .map_err(|e| AgentShellError::DBus(format!("shortcutKeys({component}): {e}")))?;
    if !bound.iter().any(|(seq,)| seq.first() == Some(&keycode)) {
        return Err(AgentShellError::Other(
            format!(
                "kglobalaccel refused shortcut for {component}: bound keys {bound:?} \
                 (组合键可能已被其它全局快捷键占用)"
            )
            .into(),
        ));
    }

    Ok(ShortcutBinding {
        backend: "kde-kglobalaccel".into(),
        component: Some(component),
        combo: combo::canonical(combo),
        action: action.to_string(),
    })
}

/// 组件唯一名：动作内容哈希，保证同一动作重复绑定复用同一组件（不累积垃圾文件）。
fn component_id(action: &str) -> String {
    let mut hash: u64 = 0xcbf2_9ce4_8422_2325;
    for byte in action.as_bytes() {
        hash ^= u64::from(*byte);
        hash = hash.wrapping_mul(0x0000_0100_0000_01b3);
    }
    format!("{COMPONENT_PREFIX}{hash:016x}.desktop")
}

/// desktop `Name`：单行、可读、限长（KDE 快捷键 KCM 用它作为组件展示名）。
fn label_for(action: &str) -> String {
    let single_line: String = action
        .chars()
        .map(|c| if c.is_control() { ' ' } else { c })
        .collect();
    let trimmed = single_line.trim();
    let mut label: String = trimmed.chars().take(LABEL_MAX_CHARS).collect();
    if trimmed.chars().count() > LABEL_MAX_CHARS {
        label.push('…');
    }
    format!("agent-shell: {label}")
}

/// 写命令快捷键 desktop 文件（原子替换：先写临时文件再 rename）。
fn write_command_desktop(
    data_home: &Path,
    component: &str,
    label: &str,
    action: &str,
) -> Result<PathBuf> {
    let dir = data_home.join("kglobalaccel");
    std::fs::create_dir_all(&dir)
        .map_err(|e| AgentShellError::Other(format!("create {}: {e}", dir.display()).into()))?;
    let path = dir.join(component);
    let exec = desktop_exec(action).map_err(|e| AgentShellError::Other(e.into()))?;
    let contents = format!(
        "[Desktop Entry]\n\
         Type=Application\n\
         Name={label}\n\
         Exec={exec}\n\
         X-KDE-GlobalAccel-CommandShortcut=true\n\
         StartupNotify=false\n"
    );
    let staged = dir.join(format!("{component}.tmp"));
    std::fs::write(&staged, contents)
        .map_err(|e| AgentShellError::Other(format!("write {}: {e}", staged.display()).into()))?;
    std::fs::rename(&staged, &path)
        .map_err(|e| AgentShellError::Other(format!("rename {}: {e}", path.display()).into()))?;
    Ok(path)
}

/// desktop `Exec` 字段：包一层 `/bin/sh -c` 以获得与 Hyprland `exec` 一致的
/// shell 语义（管道/重定向可用）。
///
/// `%` 是 desktop 规范的字段码前缀（`%f`/`%u`…），必须写成 `%%` 才不会被
/// KService 展开或吞掉；单引号按 POSIX shell 规则转义。换行会提前结束
/// `Exec=` 行、把后续文本变成 desktop 文件的新键（KService / kglobalacceld
/// 解析该文件），故拒绝而非转义。
fn desktop_exec(action: &str) -> std::result::Result<String, String> {
    if action.chars().any(char::is_control) {
        return Err("action contains control characters (newline injection)".into());
    }
    let escaped = action.replace('%', "%%").replace('\'', r"'\''");
    Ok(format!("/bin/sh -c '{escaped}'"))
}

/// 服务名不存在的 D-Bus 错误（探测失败原因归一到「没在运行」）。
fn is_service_gone(e: &zbus::Error) -> bool {
    matches!(
        e,
        zbus::Error::MethodError(name, _, _)
            if matches!(
                name.as_str(),
                "org.freedesktop.DBus.Error.ServiceUnknown"
                    | "org.freedesktop.DBus.Error.NameHasNoOwner"
            )
    ) || matches!(e, zbus::Error::Failure(_))
}

/// 建代理失败时的探测原因（连接缺失等）。
fn probe_reason(e: &zbus::Error) -> String {
    format!("kglobalaccel unreachable: {e}")
}

#[cfg(test)]
mod tests {
    use super::*;
    use agent_shell_core::types::{Key, ModifierMask};

    fn meta_t() -> KeyCombo {
        KeyCombo {
            keys: vec![Key::Char('t')],
            modifiers: ModifierMask {
                meta: true,
                ..ModifierMask::NONE
            },
        }
    }

    #[derive(Clone)]
    struct FakeAccel {
        components: std::sync::Arc<std::sync::Mutex<Vec<String>>>,
        keys: std::sync::Arc<std::sync::Mutex<Vec<Vec<i32>>>>,
        /// 模拟「按键被占用」：记录调用但拒绝落键（KGlobalAccel 静默丢弃）。
        accept_keys: std::sync::Arc<std::sync::atomic::AtomicBool>,
    }

    impl FakeAccel {
        fn new() -> Self {
            Self {
                components: std::sync::Arc::new(std::sync::Mutex::new(Vec::new())),
                keys: std::sync::Arc::new(std::sync::Mutex::new(Vec::new())),
                accept_keys: std::sync::Arc::new(std::sync::atomic::AtomicBool::new(true)),
            }
        }
    }

    /// 假 KGlobalAccel：方法名照抄真实服务（camelCase）——名字对不上时测试
    /// 必须失败，否则无法拦住「客户端按 D-Bus 惯例发 PascalCase」这类接线错误。
    #[zbus::interface(name = "org.kde.KGlobalAccel")]
    impl FakeAccel {
        #[zbus(name = "allComponents")]
        fn all_components(&self) -> Vec<OwnedObjectPath> {
            vec![OwnedObjectPath::try_from("/component/kwin").expect("static path")]
        }

        #[zbus(name = "doRegister")]
        fn do_register(&self, action_id: Vec<String>) {
            self.components
                .lock()
                .expect("lock")
                .push(action_id.join("|"));
        }

        #[zbus(name = "setForeignShortcut")]
        fn set_foreign_shortcut(&self, action_id: Vec<String>, keys: Vec<i32>) {
            self.components
                .lock()
                .expect("lock")
                .push(format!("{}|{keys:?}", action_id.join("|")));
            if self.accept_keys.load(std::sync::atomic::Ordering::Relaxed) {
                *self.keys.lock().expect("lock") = keys.into_iter().map(|k| vec![k]).collect();
            }
        }

        /// 回读形态照抄真实服务：`a(ai)`（struct 包一层键码数组）。
        #[zbus(name = "shortcutKeys")]
        fn shortcut_keys(&self, _action_id: Vec<String>) -> Vec<(Vec<i32>,)> {
            self.keys
                .lock()
                .expect("lock")
                .iter()
                .map(|keys| (keys.clone(),))
                .collect()
        }
    }

    /// 私有 session bus（并行测试隔离；dbus-daemon 为测试前置）。
    struct TestBus {
        addr: String,
        _child: std::process::Child,
    }

    impl TestBus {
        async fn start() -> Self {
            use std::process::Stdio;
            let mut child = std::process::Command::new("dbus-daemon")
                .args(["--session", "--nofork", "--print-address=1"])
                .stdout(Stdio::piped())
                .stderr(Stdio::null())
                .spawn()
                .expect("dbus-daemon must be installed for shortcut tests");
            let stdout = child.stdout.take().expect("piped stdout");
            let addr = read_address_line(stdout);
            assert!(
                addr.starts_with("unix:"),
                "dbus-daemon printed unexpected address: {addr:?}"
            );
            Self {
                addr,
                _child: child,
            }
        }

        async fn connect(&self) -> zbus::Connection {
            zbus::connection::Builder::address(self.addr.as_str())
                .expect("dbus-daemon address must parse")
                .build()
                .await
                .expect("connect to private session bus")
        }
    }

    impl Drop for TestBus {
        fn drop(&mut self) {
            // SIGTERM 让 dbus-daemon 走正常退出路径清理 socket。
            // SAFETY: `_child.id()` 是存活的子进程 PID，发信号无内存安全风险。
            unsafe { libc::kill(self._child.id() as i32, libc::SIGTERM) };
            let _ = self._child.wait();
        }
    }

    fn read_address_line(stdout: std::process::ChildStdout) -> String {
        use std::io::Read as _;
        let mut reader = std::io::BufReader::new(stdout);
        let mut bytes = Vec::new();
        loop {
            let mut buf = [0u8; 1];
            match reader.read_exact(&mut buf) {
                Ok(()) => {}
                Err(e) if e.kind() == std::io::ErrorKind::UnexpectedEof => break,
                Err(e) => panic!("read dbus-daemon address: {e}"),
            }
            bytes.push(buf[0]);
            if buf[0] == b'\n' {
                break;
            }
        }
        let line = String::from_utf8(bytes).expect("dbus-daemon address must be UTF-8");
        assert!(!line.is_empty(), "dbus-daemon printed no address line");
        line.trim_end_matches('\n').to_string()
    }

    async fn spawn_fake_accel(bus: &TestBus) -> (zbus::Connection, FakeAccel) {
        let conn = bus.connect().await;
        let fake = FakeAccel::new();
        conn.object_server()
            .at("/kglobalaccel", fake.clone())
            .await
            .expect("register fake kglobalaccel");
        use zbus::names::WellKnownName;
        let name =
            WellKnownName::try_from("org.kde.kglobalaccel".to_string()).expect("valid bus name");
        conn.request_name(name).await.expect("claim kglobalaccel");
        // 连接必须由调用方持有：drop 即释放总线名，后续调用会退化为 ServiceUnknown。
        (conn, fake)
    }

    #[test]
    fn component_id_is_stable_and_action_specific() {
        let a = component_id("notify-send hello");
        assert_eq!(a, component_id("notify-send hello"));
        assert_ne!(a, component_id("notify-send hello "));
        assert!(a.starts_with("agent-shell-") && a.ends_with(".desktop"));
        assert_eq!(a.len(), "agent-shell-".len() + 16 + ".desktop".len());
    }

    #[test]
    fn label_is_single_line_and_bounded() {
        assert_eq!(label_for("notify-send hi"), "agent-shell: notify-send hi");
        assert_eq!(
            label_for("echo one\ntwo\tthree"),
            "agent-shell: echo one two three"
        );
        let long = label_for(&"x".repeat(200));
        assert_eq!(
            long.chars().count(),
            "agent-shell: ".chars().count() + 64 + 1
        );
        assert!(long.ends_with('…'));
    }

    #[test]
    fn desktop_exec_wraps_shell_and_escapes_field_codes() {
        assert_eq!(
            desktop_exec("notify-send hi").expect("plain command"),
            "/bin/sh -c 'notify-send hi'"
        );
        // 百分号是 desktop 字段码前缀，必须转义为 %%。
        assert_eq!(
            desktop_exec("printf '%s' hi").expect("field code"),
            r"/bin/sh -c 'printf '\''%%s'\'' hi'"
        );
        // 单引号按 shell 规则转义，避免命令被截断。
        assert_eq!(
            desktop_exec("sh -c 'echo hi'").expect("quote"),
            r"/bin/sh -c 'sh -c '\''echo hi'\'''"
        );
    }

    #[test]
    fn desktop_exec_rejects_control_characters() {
        // 换行会让 `Exec=` 提前结束、后续文本成为新的 desktop 键行。
        assert!(desktop_exec("touch a\nHidden=true").is_err());
        assert!(desktop_exec("touch a\rmonitor=x").is_err());
        assert!(desktop_exec("touch a\u{7}b").is_err());
    }

    #[test]
    fn write_command_desktop_produces_kglobalaccel_command_shortcut() {
        let tmp = tempfile::tempdir().expect("tempdir");
        let path = write_command_desktop(
            tmp.path(),
            "agent-shell-deadbeef00000000.desktop",
            "agent-shell: notify-send hi",
            "notify-send hi",
        )
        .expect("write desktop file");
        let body = std::fs::read_to_string(&path).expect("read back");
        assert!(body.contains("Type=Application"));
        assert!(body.contains("X-KDE-GlobalAccel-CommandShortcut=true"));
        assert!(body.contains("Name=agent-shell: notify-send hi"));
        assert!(body.contains("Exec=/bin/sh -c 'notify-send hi'"));
        assert_eq!(
            path,
            tmp.path()
                .join("kglobalaccel")
                .join("agent-shell-deadbeef00000000.desktop")
        );
        // 含换行的动作不得产出任何文件（入口已拦下，此处兜底）。
        assert!(write_command_desktop(
            tmp.path(),
            "agent-shell-newline.desktop",
            "agent-shell: bad",
            "touch a\nHidden=true",
        )
        .is_err());
        assert!(!tmp
            .path()
            .join("kglobalaccel")
            .join("agent-shell-newline.desktop")
            .exists());
    }

    #[tokio::test]
    async fn bind_registers_component_and_verifies_binding() {
        let bus = TestBus::start().await;
        let (_server, fake) = spawn_fake_accel(&bus).await;
        let tmp = tempfile::tempdir().expect("tempdir");
        let conn = bus.connect().await;

        let binding = bind(&conn, tmp.path(), &meta_t(), "notify-send hi")
            .await
            .expect("bind");
        assert_eq!(binding.backend, "kde-kglobalaccel");
        assert_eq!(binding.combo, "meta+t");
        let component = binding.component.expect("component id");
        assert_eq!(component, component_id("notify-send hi"));

        let calls = fake.components.lock().expect("lock").clone();
        assert_eq!(
            calls.len(),
            2,
            "expected doRegister + setForeignShortcut: {calls:?}"
        );
        assert_eq!(
            calls[0],
            format!("{component}|agent-shell: notify-send hi||")
        );
        assert_eq!(
            calls[1],
            format!("{component}|_launch|agent-shell: notify-send hi|agent-shell: notify-send hi|[268435540]")
        );
        assert!(tmp.path().join("kglobalaccel").join(&component).exists());
    }

    #[tokio::test]
    async fn bind_reports_when_kglobalaccel_refuses_the_key() {
        let bus = TestBus::start().await;
        let (_server, fake) = spawn_fake_accel(&bus).await;
        // 按键被别的全局快捷键占用时 KGlobalAccel 静默丢弃：回读为空必须报错。
        fake.accept_keys
            .store(false, std::sync::atomic::Ordering::Relaxed);
        let tmp = tempfile::tempdir().expect("tempdir");
        let conn = bus.connect().await;

        let err = bind(&conn, tmp.path(), &meta_t(), "notify-send hi")
            .await
            .expect_err("empty read-back must fail");
        assert!(
            err.to_string().contains("refused shortcut"),
            "unexpected error: {err}"
        );
    }

    #[tokio::test]
    async fn ready_reports_missing_kglobalaccel() {
        let bus = TestBus::start().await;
        let conn = bus.connect().await;
        let reason = ready(&conn).await.expect_err("no kglobalaccel on this bus");
        assert!(
            reason.contains("kglobalaccel"),
            "unexpected reason: {reason}"
        );
    }
}
