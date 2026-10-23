//! XTest 后端（设计文档 §12.1「X11 原生」行、§6.3）。
//!
//! 仅原生 X11 会话可用（XWayland 下 XTest 被合成器禁用，探测即失败）。
//! 通过 x11rb 直连 `$DISPLAY`，注入走 `xtest_fake_input`；键码为 Xorg
//! 默认 keymap（PC/AT evdev+8）。构造失败（无 DISPLAY / 连接拒绝 /
//! 扩展不可用）由 dispatcher 跳过该候选。

use std::sync::Arc;

use x11rb::connection::Connection as _;

use agent_shell_core::error::{AgentShellError, Result};
use agent_shell_core::types::{Key, KeyCombo, KeyName, MouseButton};
use async_trait::async_trait;
use tokio::sync::Mutex;
use x11rb::protocol::xproto::*;
use x11rb::protocol::xtest::ConnectionExt as _;
use x11rb::rust_connection::RustConnection;

use super::dispatcher::InputService;

/// XTest 注入后端：持有独立 X11 连接。
pub struct XTestInput {
    conn: Arc<RustConnection>,
    root: Window,
    /// 扩展版本协商结果（构造时探测）。
    available: bool,
    /// 串行化注入：fake_input 是异步请求 + flush，多线程交错会打乱
    /// press/release 次序。异步 Mutex 保证一个组合的完整序列原子提交
    /// （guard 持有跨 .await，故用 tokio 锁而非 std/parking_lot）。
    inject_lock: Mutex<()>,
}

impl XTestInput {
    /// 连接 `$DISPLAY` 并探测 XTest 扩展；任一步失败返回 `Err`，
    /// dispatcher 不压入该候选。
    pub fn new() -> Result<Self> {
        Self::connect(None)
    }

    /// 连接指定 display（`None` 取 `$DISPLAY`）并执行同一探测。
    ///
    /// 显式 display 参数让测试不依赖宿主环境变量也能走到连接失败分支。
    fn connect(display: Option<&str>) -> Result<Self> {
        let (conn, screen_index) = RustConnection::connect(display)
            .map_err(|e| AgentShellError::BackendUnavailable(format!("x11 connect: {e}")))?;
        let setup = conn.setup();
        let screen = setup.roots.get(screen_index).ok_or_else(|| {
            AgentShellError::BackendUnavailable("x11: screen index out of range".into())
        })?;
        let root = screen.root;
        let available = conn
            .xtest_get_version(2, 2)
            .map(|c| c.reply().is_ok())
            .unwrap_or(false);
        if !available {
            return Err(AgentShellError::BackendUnavailable(
                "XTest extension unavailable (XWayland or missing extension)".into(),
            ));
        }
        Ok(Self {
            conn: Arc::new(conn),
            root,
            available,
            inject_lock: Mutex::new(()),
        })
    }

    fn require_xtest(&self) -> Result<()> {
        if self.available {
            Ok(())
        } else {
            Err(AgentShellError::BackendUnavailable(
                "XTest extension unavailable".into(),
            ))
        }
    }

    /// 在阻塞线程池上执行同步 x11rb I/O，避免长 `type_text` 饿死 tokio
    /// worker（事件订阅等并发任务）——见 §19 审查项：XTest 是同步协议。
    async fn with_conn<F, T>(&self, f: F) -> Result<T>
    where
        F: FnOnce(&RustConnection) -> Result<T> + Send + 'static,
        T: Send + 'static,
    {
        let conn = Arc::clone(&self.conn);
        tokio::task::spawn_blocking(move || f(&conn))
            .await
            .map_err(|e| AgentShellError::Input(format!("x11 blocking task join: {e}")))?
    }

    async fn fake_key(&self, keycode: u8, is_press: bool) -> Result<()> {
        let type_ = if is_press {
            KEY_PRESS_EVENT
        } else {
            KEY_RELEASE_EVENT
        };
        let root = self.root;
        self.with_conn(move |conn| {
            conn.xtest_fake_input(type_, keycode, x11rb::CURRENT_TIME, root, 0, 0, 0)
                .map_err(xerr)?;
            conn.flush().map_err(xerr)
        })
        .await
    }

    async fn fake_button(&self, button: u8, is_press: bool) -> Result<()> {
        let type_ = if is_press {
            BUTTON_PRESS_EVENT
        } else {
            BUTTON_RELEASE_EVENT
        };
        let root = self.root;
        self.with_conn(move |conn| {
            conn.xtest_fake_input(type_, button, x11rb::CURRENT_TIME, root, 0, 0, 0)
                .map_err(xerr)?;
            conn.flush().map_err(xerr)
        })
        .await
    }

    /// 键码序列注入：修饰键 → 实体键按下，整体逆序释放；临时 shift 补齐。
    async fn send_sequence(&self, combo: &KeyCombo) -> Result<()> {
        let _guard = self.inject_lock.lock().await;
        let mut seq: Vec<(u8, bool)> = Vec::new(); // (keycode, is_temp_shift)

        if combo.modifiers.ctrl {
            seq.push((37, false)); // Left Control = evdev 29 + 8
        }
        if combo.modifiers.alt {
            seq.push((64, false)); // Left Alt = evdev 56 + 8
        }
        if combo.modifiers.shift {
            seq.push((50, false)); // Left Shift = evdev 42 + 8
        }
        if combo.modifiers.meta {
            seq.push((133, false)); // Left Super = evdev 125 + 8
        }
        for key in &combo.keys {
            match key {
                Key::Named(n) => {
                    let code = named_keycode(*n).ok_or_else(|| {
                        AgentShellError::Input(format!("no keycode mapped for {n:?}"))
                    })?;
                    seq.push((code, false));
                }
                Key::Char(c) => {
                    let (code, shift) = char_keycode(*c).ok_or_else(|| {
                        AgentShellError::Input(format!("cannot inject character {c:?}"))
                    })?;
                    if shift && !combo.modifiers.shift {
                        seq.push((50, true)); // 临时 shift：释放时最先弹起
                    }
                    seq.push((code, false));
                }
            }
        }

        for (code, _) in &seq {
            self.fake_key(*code, true).await?;
        }
        // 逆序释放；临时 shift 在其实体键之后弹起（逆序自然满足）。
        for (code, _) in seq.iter().rev() {
            self.fake_key(*code, false).await?;
        }
        Ok(())
    }

    /// 单字符键入：press/release 一对，含临时 shift。
    async fn type_char(&self, c: char) -> Result<()> {
        let _guard = self.inject_lock.lock().await;
        let (code, shift) = char_keycode(c)
            .ok_or_else(|| AgentShellError::Input(format!("cannot type character {c:?}")))?;
        if shift {
            self.fake_key(50, true).await?;
        }
        self.fake_key(code, true).await?;
        self.fake_key(code, false).await?;
        if shift {
            self.fake_key(50, false).await?;
        }
        Ok(())
    }

    /// 鼠标按键编号（X11 协议按钮序号）。
    fn mouse_button(b: MouseButton) -> u8 {
        match b {
            MouseButton::Left => 1,
            MouseButton::Middle => 2,
            MouseButton::Right => 3,
            MouseButton::Back => 8,
            MouseButton::Forward => 9,
        }
    }

    async fn click_at(&self, button: u8) -> Result<()> {
        let _guard = self.inject_lock.lock().await;
        self.fake_button(button, true).await?;
        self.fake_button(button, false).await
    }
}

/// 命名键 → Xorg 默认 keymap 键码（PC/AT evdev+8）。
fn named_keycode(name: KeyName) -> Option<u8> {
    use KeyName::*;
    Some(match name {
        Return => 36,
        Escape => 9,
        BackSpace => 22,
        Tab => 23,
        Space => 65,
        Left => 113,
        Right => 114,
        Up => 111,
        Down => 116,
        Home => 110,
        End => 115,
        PageUp => 112,
        PageDown => 117,
        Insert => 118,
        Delete => 119,
        Menu => 135,
        F1 => 67,
        F2 => 68,
        F3 => 69,
        F4 => 70,
        F5 => 71,
        F6 => 72,
        F7 => 73,
        F8 => 74,
        F9 => 75,
        F10 => 76,
        F11 => 95,
        F12 => 96,
    })
}

/// 单字符 → (键码, 是否需要 shift)，US QWERTY 常见字符。
fn char_keycode(c: char) -> Option<(u8, bool)> {
    // Shift 符号先折回其底键（US QWERTY），再查键码。
    let base = match c {
        '!' => '1',
        '@' => '2',
        '#' => '3',
        '$' => '4',
        '%' => '5',
        '^' => '6',
        '&' => '7',
        '*' => '8',
        '(' => '9',
        ')' => '0',
        '_' => '-',
        '+' => '=',
        '{' => '[',
        '}' => ']',
        '|' => '\\',
        ':' => ';',
        '"' => '\'',
        '<' => ',',
        '>' => '.',
        '?' => '/',
        '~' => '`',
        other => other,
    };
    let folded = match c {
        '!' | '@' | '#' | '$' | '%' | '^' | '&' | '*' | '(' | ')' | '_' | '+' | '{' | '}' | '|'
        | ':' | '"' | '<' | '>' | '?' | '~' => base,
        other => other,
    };
    let lower = folded.to_ascii_lowercase();
    let code: u8 = match lower {
        'a'..='z' => 38 + (lower as u8 - b'a'), // a=38..z=48
        '1'..='9' => 10 + (lower as u8 - b'1'), // 1=10..9=18
        '0' => 19,
        ' ' => 65,
        '.' => 60,
        ',' => 59,
        '/' => 61,
        ';' => 47,
        '\'' => 48,
        '[' => 34,
        ']' => 35,
        '\\' => 51,
        '`' => 49,
        '-' => 20,
        '=' => 21,
        _ => return None,
    };
    let shifted = c.is_ascii_uppercase()
        || matches!(
            c,
            '!' | '@'
                | '#'
                | '$'
                | '%'
                | '^'
                | '&'
                | '*'
                | '('
                | ')'
                | '_'
                | '+'
                | '{'
                | '}'
                | '|'
                | ':'
                | '"'
                | '<'
                | '>'
                | '?'
                | '~'
        );
    Some((code, shifted))
}

fn xerr(e: x11rb::errors::ConnectionError) -> AgentShellError {
    AgentShellError::Input(format!("x11 request: {e}"))
}

#[async_trait]
impl InputService for XTestInput {
    fn name(&self) -> &'static str {
        "xtest"
    }

    async fn is_available(&self) -> bool {
        self.available
    }

    async fn send_key(&self, combo: &KeyCombo) -> Result<()> {
        self.require_xtest()?;
        self.send_sequence(combo).await
    }

    async fn type_text(&self, text: &str, delay_ms: u32) -> Result<()> {
        self.require_xtest()?;
        for c in text.chars() {
            self.type_char(c).await?;
            if delay_ms > 0 {
                tokio::time::sleep(std::time::Duration::from_millis(delay_ms as u64)).await;
            }
        }
        self.with_conn(|conn| conn.flush().map_err(xerr)).await
    }

    async fn mouse_move(&self, x: i32, y: i32) -> Result<()> {
        self.require_xtest()?;
        let _guard = self.inject_lock.lock().await;
        let (x, y) = (i16::try_from(x), i16::try_from(y));
        let (x, y) = match (x, y) {
            (Ok(x), Ok(y)) => (x, y),
            _ => {
                return Err(AgentShellError::Input(format!(
                    "mouse_move coordinates out of i16 range: ({x:?}, {y:?})"
                )))
            }
        };
        let root = self.root;
        self.with_conn(move |conn| {
            conn.xtest_fake_input(MOTION_NOTIFY_EVENT, 0, x11rb::CURRENT_TIME, root, x, y, 0)
                .map_err(xerr)?;
            conn.flush().map_err(xerr)
        })
        .await
    }

    async fn mouse_click(&self, button: MouseButton) -> Result<()> {
        self.require_xtest()?;
        self.click_at(Self::mouse_button(button)).await
    }

    async fn mouse_scroll(&self, dx: i32, dy: i32) -> Result<()> {
        self.require_xtest()?;
        // dy>0 向下（button 5）、dy<0 向上（4）；dx 同理走 7/6。
        let steps_v = dy.unsigned_abs();
        let steps_h = dx.unsigned_abs();
        let v_btn: u8 = if dy > 0 { 5 } else { 4 };
        let h_btn: u8 = if dx > 0 { 7 } else { 6 };
        // V/H 轴独立循环：max 合并会让短轴多点击 |Δsteps| 格。
        for _ in 0..steps_v {
            self.click_at(v_btn).await?;
        }
        for _ in 0..steps_h {
            self.click_at(h_btn).await?;
        }
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use x11rb::connection::RequestConnection as _;

    #[test]
    fn keycodes_match_pc_at_evdev_plus_8() {
        assert_eq!(named_keycode(KeyName::Return), Some(36));
        assert_eq!(named_keycode(KeyName::Escape), Some(9));
        assert_eq!(char_keycode('a'), Some((38, false)));
        assert_eq!(char_keycode('A'), Some((38, true)));
        assert_eq!(char_keycode('1'), Some((10, false)));
        assert_eq!(char_keycode('!'), Some((10, true)));
        assert_eq!(char_keycode('\n'), None); // 文本路径不处理换行 → 明确报错
    }

    #[test]
    fn mouse_button_numbers_match_x_protocol() {
        assert_eq!(XTestInput::mouse_button(MouseButton::Left), 1);
        assert_eq!(XTestInput::mouse_button(MouseButton::Middle), 2);
        assert_eq!(XTestInput::mouse_button(MouseButton::Right), 3);
        assert_eq!(XTestInput::mouse_button(MouseButton::Back), 8);
        assert_eq!(XTestInput::mouse_button(MouseButton::Forward), 9);
    }

    /// 不可达显示：构造必须返回可归因的 `BackendUnavailable`，不得 panic。
    ///
    /// 显式 display 参数让负例与宿主 `$DISPLAY` 无关——headless 与桌面会话
    /// 得到同一结论，不是「环境缺失时零断言通过」。
    #[test]
    fn construction_fails_cleanly_on_unreachable_display() {
        let Err(err) = XTestInput::connect(Some(":999")) else {
            panic!("unreachable display must not construct");
        };
        assert!(
            matches!(err, AgentShellError::BackendUnavailable(_)),
            "got: {err:?}"
        );
    }

    /// CI 的 Xvfb 步骤以本变量声明「`$DISPLAY` 必为带 XTEST 扩展的 X server」。
    const LIVE_X11_ENV: &str = "AGENT_SHELL_LIVE_X11";

    /// CI 的不广告 XTEST 步骤以本变量声明「`$DISPLAY` 可连接但不广告 XTEST」。
    const NO_XTEST_ENV: &str = "AGENT_SHELL_X11_NO_XTEST";

    /// 构造门控必须与服务端 XTEST 扩展广告一致（§12.1 X11 行、C-054）。
    ///
    /// 无 `DISPLAY`：断言可归因的构造失败（默认 CI 的 headless 面）。
    /// 有 `DISPLAY`：独立连接读扩展表作对照，断言构造成功 ⇔ 服务端广告
    /// XTEST。Xvfb 面（置位 `AGENT_SHELL_LIVE_X11`）要求广告且构造成功；
    /// 不广告 XTEST 面（置位 `AGENT_SHELL_X11_NO_XTEST`）要求不广告且构造以
    /// `BackendUnavailable` 失败——跳过探测（可用恒真）只在该面暴露。
    #[tokio::test]
    async fn construction_gate_matches_server_xtest_advertisement() {
        let live_required = std::env::var_os(LIVE_X11_ENV).is_some();
        let no_xtest_required = std::env::var_os(NO_XTEST_ENV).is_some();
        if std::env::var_os("DISPLAY").is_none() {
            let Err(err) = XTestInput::new() else {
                panic!("connect(None) must fail without DISPLAY");
            };
            assert!(
                matches!(err, AgentShellError::BackendUnavailable(_)),
                "got: {err:?}"
            );
            assert!(
                !live_required,
                "{LIVE_X11_ENV} 已置位但 DISPLAY 未设置：Xvfb 夹具未生效"
            );
            assert!(
                !no_xtest_required,
                "{NO_XTEST_ENV} 已置位但 DISPLAY 未设置：不广告 XTEST 的夹具未生效"
            );
            return;
        }

        let conn = match RustConnection::connect(None) {
            Ok((conn, _)) => conn,
            Err(e) => {
                // `$DISPLAY` 指向不可达套接字：构造同样必须失败且可归因。
                let Err(err) = XTestInput::new() else {
                    panic!("unreachable DISPLAY must not construct");
                };
                assert!(
                    matches!(err, AgentShellError::BackendUnavailable(_)),
                    "got: {err:?}"
                );
                assert!(
                    !live_required,
                    "{LIVE_X11_ENV} 已置位但 DISPLAY 不可连接：{e:?}"
                );
                assert!(
                    !no_xtest_required,
                    "{NO_XTEST_ENV} 已置位但 DISPLAY 不可连接：{e:?}"
                );
                return;
            }
        };
        let advertised = conn
            .extension_information(x11rb::protocol::xtest::X11_EXTENSION_NAME)
            .expect("查询扩展表需要可用连接")
            .is_some();
        if live_required {
            assert!(advertised, "Xvfb 夹具必须广告 XTEST 扩展");
        }
        if no_xtest_required {
            assert!(
                !advertised,
                "{NO_XTEST_ENV} 已置位但服务端仍广告 XTEST：不广告 XTEST 的夹具未生效"
            );
        }
        match XTestInput::new() {
            Ok(xt) => {
                assert!(advertised, "构造成功但服务端未广告 XTEST 扩展");
                assert!(xt.is_available().await, "构造成功后 is_available 必须为真");
            }
            Err(AgentShellError::BackendUnavailable(_)) => {
                assert!(!advertised, "服务端广告 XTEST 时构造不得报告不可用");
            }
            Err(other) => panic!("构造失败必须可归因（BackendUnavailable），got: {other:?}"),
        }
    }
}
