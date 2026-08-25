//! X11 显示服务器实现。
//!
//! 对应设计文档 §6：连接、EWMH 窗口操作、XTest 注入、截图与监视器几何。

use x11rb::atom_manager;
use x11rb::connection::Connection as _;
use x11rb::errors::{ConnectError, ConnectionError, ReplyError};
use x11rb::protocol::randr::ConnectionExt as _RandrExt;
use x11rb::protocol::xproto::{
    AtomEnum, ClientMessageEvent, ConnectionExt, EventMask, ImageFormat,
};
use x11rb::protocol::xtest::ConnectionExt as _XTestExt;
use x11rb::rust_connection::RustConnection;

use agent_shell_core::error::{AgentShellError, Result};
use agent_shell_core::types::Rect;

use super::{moveresize_flags, wm_state_action};

// EWMH 原子集合（批量 intern，§6.2「root window → 批量获取原子」）。
// 宏调用不支持文档注释：字段清单见 ewmh 模块展开说明（x11rb atom_manager!）。
atom_manager! {
    pub EwmhAtoms: EwmhAtomsCookie {
        _NET_CLIENT_LIST,
        _NET_CLIENT_LIST_STACKING,
        _NET_ACTIVE_WINDOW,
        _NET_CURRENT_DESKTOP,
        _NET_NUMBER_OF_DESKTOPS,
        _NET_DESKTOP_NAMES,
        _NET_DESKTOP_GEOMETRY,
        _NET_DESKTOP_VIEWPORT,
        _NET_WORKAREA,
        _NET_SUPPORTED,
        _NET_SUPPORTING_WM_CHECK,
        _NET_CLOSE_WINDOW,
        _NET_MOVERESIZE_WINDOW,
        _NET_WM_NAME,
        _NET_WM_STATE,
        _NET_WM_STATE_MAXIMIZED_VERT,
        _NET_WM_STATE_MAXIMIZED_HORZ,
        _NET_WM_STATE_HIDDEN,
        _NET_WM_STATE_FULLSCREEN,
        _NET_WM_STATE_ABOVE,
        _NET_WM_DESKTOP,
        _NET_WM_WINDOW_TYPE,
        _NET_WM_WINDOW_TYPE_NORMAL,
        _NET_WM_WINDOW_TYPE_DIALOG,
        _NET_WM_WINDOW_TYPE_DOCK,
        _NET_WM_WINDOW_TYPE_DESKTOP,
        _NET_WM_WINDOW_TYPE_MENU,
        _NET_WM_WINDOW_TYPE_TOOLTIP,
        _NET_WM_WINDOW_TYPE_SPLASH,
        _NET_WM_PID,
        UTF8_STRING,
        WM_CLASS,
    }
}

/// X11 显示服务器（协议通道基础，§6）。
#[derive(Debug)]
pub struct X11DisplayServer {
    conn: RustConnection,
    screen_index: usize,
    root: x11rb::protocol::xproto::Window,
    atoms: EwmhAtoms,
    /// ICCCM 协议原子（WM_PROTOCOLS/DELETE_WINDOW 等，§6 icccm 模块）。
    pub(crate) wm_atoms: crate::icccm::WmProtocolAtoms,
    /// XTest 扩展可用性（连接时探测；XWayland 下通常不可用或被禁）。
    xtest_available: bool,
}

impl X11DisplayServer {
    /// 连接 `$DISPLAY` 并初始化 EWMH 原子（§6.2）。
    pub fn connect() -> Result<Self> {
        let (conn, screen_index) = RustConnection::connect(None).map_err(|e| {
            AgentShellError::BackendUnavailable(format!("x11 connect: {}", x11_err(&e)))
        })?;
        Self::init(conn, screen_index)
    }

    /// 连接指定 display（`:0`、`host:0.0` 等；测试与多会话用）。
    pub fn connect_to(dpy_name: Option<&str>) -> Result<Self> {
        let (conn, screen_index) = RustConnection::connect(dpy_name).map_err(|e| {
            AgentShellError::BackendUnavailable(format!("x11 connect: {}", x11_err(&e)))
        })?;
        Self::init(conn, screen_index)
    }

    fn init(conn: RustConnection, screen_index: usize) -> Result<Self> {
        let setup = conn.setup();
        let screen = setup.roots.get(screen_index).ok_or_else(|| {
            AgentShellError::BackendUnavailable("x11: screen index out of range".into())
        })?;
        let root = screen.root;

        let atoms = EwmhAtoms::new(&conn).map_err(cerr)?.reply().map_err(rerr)?;

        let wm_atoms = crate::icccm::WmProtocolAtoms::new(&conn)?;

        // XTest 可用性探测：查询扩展版本失败 → 输入注入走降级链。
        let xtest_available = conn
            .xtest_get_version(2, 2)
            .map(|c| c.reply().is_ok())
            .unwrap_or(false);

        Ok(Self {
            conn,
            screen_index,
            root,
            atoms,
            wm_atoms,
            xtest_available,
        })
    }

    /// 默认屏幕的根窗口。
    pub fn root_window(&self) -> x11rb::protocol::xproto::Window {
        self.root
    }

    /// 屏幕索引。
    pub fn screen_index(&self) -> usize {
        self.screen_index
    }

    /// EWMH 原子集合。
    pub fn atoms(&self) -> &EwmhAtoms {
        &self.atoms
    }

    /// XTest 输入注入是否原生可用（§6.3；false 时上层降级 libei/ydotool）。
    pub fn is_xtest_available(&self) -> bool {
        self.xtest_available
    }

    // ───────────────────────── ICCCM 协议操作（§6 icccm 模块） ─────────────────────────

    /// 探测目标窗口支持的 `WM_PROTOCOLS` 列表（ICCCM 4.1.2.7）。
    pub fn get_wm_protocols(
        &self,
        window: x11rb::protocol::xproto::Window,
    ) -> Result<Vec<crate::icccm::WmProtocol>> {
        let reply = self
            .conn
            .get_property(
                false,
                window,
                self.wm_atoms.wm_protocols,
                x11rb::protocol::xproto::Atom::from(AtomEnum::ATOM),
                0,
                u32::MAX,
            )
            .map_err(cerr)?
            .reply()
            .map_err(rerr)?;
        let atoms: Vec<u32> = reply.value32().into_iter().flatten().collect();
        Ok(atoms.iter().map(|&a| self.wm_atoms.classify(a)).collect())
    }

    /// 窗口是否支持 `WM_DELETE_WINDOW`（优雅关闭可用性预判）。
    pub fn supports_delete_window(&self, window: x11rb::protocol::xproto::Window) -> Result<bool> {
        Ok(self
            .get_wm_protocols(window)?
            .iter()
            .any(|p| matches!(p, crate::icccm::WmProtocol::DeleteWindow)))
    }

    /// 发送 `WM_DELETE_WINDOW` ClientMessage（ICCCM 4.2.8.1）。
    ///
    /// 客户端协议路径（区别于 EWMH `_NET_CLOSE_WINDOW`）：窗口未声明支持时
    /// 返回错误，调用方降级 XKillClient / D-Bus 通道。直接发给目标窗口，
    /// 不经 root 重定向。
    pub fn delete_window(&self, window: x11rb::protocol::xproto::Window) -> Result<()> {
        if !self.supports_delete_window(window)? {
            return Err(AgentShellError::Input(format!(
                "icccm: window {window} does not support WM_DELETE_WINDOW"
            )));
        }
        self.send_wm_protocol(window, self.wm_atoms.wm_delete_window)
    }

    /// 发送任意 `WM_PROTOCOLS` ClientMessage（`WM_TAKE_FOCUS` / `WM_PING` 复用）。
    pub fn send_wm_protocol(
        &self,
        window: x11rb::protocol::xproto::Window,
        protocol: x11rb::protocol::xproto::Atom,
    ) -> Result<()> {
        let event = ClientMessageEvent::new(
            32,
            window,
            self.wm_atoms.wm_protocols,
            [protocol, 0, 0, 0, 0],
        );
        self.conn
            .send_event(false, window, EventMask::NO_EVENT, event)
            .map_err(cerr)?;
        self.conn.flush().map_err(cerr)
    }

    /// 读经典 `WM_STATE` 状态码（属性缺失 → Withdrawn，ICCCM 4.1.3.1）。
    pub fn get_wm_state(&self, window: x11rb::protocol::xproto::Window) -> Result<u32> {
        Ok(self
            .property_bytes(window, self.wm_atoms.wm_state, self.wm_atoms.wm_state)?
            .and_then(|raw| raw.first_chunk::<4>().map(|c| u32::from_ne_bytes(*c)))
            .unwrap_or(crate::icccm::wm_state::WITHDRAWN))
    }

    /// 底层 X 连接（ICCCM 等子模块协议操作复用同一连接）。
    pub fn connection(&self) -> &RustConnection {
        &self.conn
    }

    // ───────────────────────── 窗口列表与属性（§6.2） ─────────────────────────

    /// `_NET_CLIENT_LIST`：初始映射顺序的客户窗口列表。
    ///
    /// 属性缺失（非 EWMH WM）返回空表——调用方据此触发 CLI 兜底（T1g）。
    pub fn get_client_list(&self) -> Result<Vec<x11rb::protocol::xproto::Window>> {
        self.get_property_u32(
            self.root,
            self.atoms._NET_CLIENT_LIST,
            AtomEnum::WINDOW.into(),
        )
    }

    /// `_NET_CLIENT_LIST_STACKING`：从底到顶的层叠顺序客户窗口列表。
    pub fn get_client_list_stacking(&self) -> Result<Vec<x11rb::protocol::xproto::Window>> {
        self.get_property_u32(
            self.root,
            self.atoms._NET_CLIENT_LIST_STACKING,
            AtomEnum::WINDOW.into(),
        )
    }

    /// `_NET_ACTIVE_WINDOW`：当前活动窗口（无焦点时为 `None`）。
    pub fn get_active_window(&self) -> Result<Option<x11rb::protocol::xproto::Window>> {
        let ids = self.get_property_u32(
            self.root,
            self.atoms._NET_ACTIVE_WINDOW,
            AtomEnum::WINDOW.into(),
        )?;
        Ok(ids.first().filter(|&&w| w != 0).copied())
    }

    /// `_NET_CURRENT_DESKTOP`：当前工作区索引。
    pub fn get_current_desktop(&self) -> Result<u32> {
        let d = self.get_property_u32(
            self.root,
            self.atoms._NET_CURRENT_DESKTOP,
            AtomEnum::CARDINAL.into(),
        )?;
        d.first()
            .copied()
            .ok_or_else(|| AgentShellError::Other("x11: _NET_CURRENT_DESKTOP empty".into()))
    }

    /// `_NET_NUMBER_OF_DESKTOPS`：虚拟桌面总数。
    pub fn get_number_of_desktops(&self) -> Result<u32> {
        let n = self.get_property_u32(
            self.root,
            self.atoms._NET_NUMBER_OF_DESKTOPS,
            AtomEnum::CARDINAL.into(),
        )?;
        n.first()
            .copied()
            .ok_or_else(|| AgentShellError::Other("x11: _NET_NUMBER_OF_DESKTOPS empty".into()))
    }

    /// `_NET_WORKAREA`：全部工作区联合可用区域 `[x, y, w, h]`。
    pub fn get_work_area(&self) -> Result<Option<Rect>> {
        let v = self.get_property_u32(
            self.root,
            self.atoms._NET_WORKAREA,
            AtomEnum::CARDINAL.into(),
        )?;
        if v.len() >= 4 {
            Ok(Some(Rect {
                x: v[0] as i32,
                y: v[1] as i32,
                width: v[2] as i32,
                height: v[3] as i32,
            }))
        } else {
            Ok(None)
        }
    }

    /// `_NET_WM_NAME` / `WM_NAME`：UTF-8 优先的窗口标题。
    pub fn get_window_name(&self, window: x11rb::protocol::xproto::Window) -> Result<String> {
        if let Some(name) =
            self.get_property_string(window, self.atoms._NET_WM_NAME, self.atoms.UTF8_STRING)?
        {
            return Ok(name);
        }
        self.get_property_string(window, AtomEnum::WM_NAME.into(), AtomEnum::STRING.into())?
            .ok_or_else(|| AgentShellError::WindowNotFound(format!("window {window}: no name")))
    }

    /// `WM_CLASS`：应用类名（instance\0class\0），取 class 部分。
    pub fn get_wm_class(&self, window: x11rb::protocol::xproto::Window) -> Result<Option<String>> {
        let bytes = self.property_bytes(window, self.atoms.WM_CLASS, AtomEnum::STRING.into())?;
        Ok(bytes.and_then(|raw| {
            raw.split(|&b| b == 0)
                .find(|s| !s.is_empty())
                .map(String::from_utf8_lossy)
                .map(Into::into)
        }))
    }

    /// `_NET_WM_PID`：创建窗口的客户进程 PID。
    pub fn get_window_pid(&self, window: x11rb::protocol::xproto::Window) -> Result<Option<u32>> {
        let pids =
            self.get_property_u32(window, self.atoms._NET_WM_PID, AtomEnum::CARDINAL.into())?;
        Ok(pids.first().copied())
    }

    /// `_NET_WM_DESKTOP`：窗口所在工作区（u32::MAX 表示所有工作区）。
    pub fn get_window_desktop(
        &self,
        window: x11rb::protocol::xproto::Window,
    ) -> Result<Option<u32>> {
        Ok(self
            .get_property_u32(
                window,
                self.atoms._NET_WM_DESKTOP,
                AtomEnum::CARDINAL.into(),
            )?
            .first()
            .copied())
    }

    /// `_NET_WM_STATE`：窗口当前状态原子列表。
    pub fn get_window_states(&self, window: x11rb::protocol::xproto::Window) -> Result<Vec<u32>> {
        self.get_property_u32(window, self.atoms._NET_WM_STATE, AtomEnum::ATOM.into())
    }

    /// 窗口几何（`GetGeometry`，内容坐标相对父窗口）。
    pub fn get_window_geometry(&self, window: x11rb::protocol::xproto::Window) -> Result<Rect> {
        let reply = self
            .conn
            .get_geometry(window)
            .map_err(cerr)?
            .reply()
            .map_err(rerr)?;
        Ok(Rect {
            x: reply.x as i32,
            y: reply.y as i32,
            width: reply.width as i32,
            height: reply.height as i32,
        })
    }

    // ───────────────────────── 窗口操作（§6.2） ─────────────────────────

    /// `_NET_ACTIVE_WINDOW` ClientMessage：请求聚焦窗口。
    pub fn activate_window(&self, window: x11rb::protocol::xproto::Window) -> Result<()> {
        let event = ClientMessageEvent::new(
            32,
            window,
            self.atoms._NET_ACTIVE_WINDOW,
            [0u32, 0, 0, 0, 0],
        );
        self.send_root_event(
            event,
            EventMask::SUBSTRUCTURE_REDIRECT | EventMask::SUBSTRUCTURE_NOTIFY,
        )
    }

    /// `_NET_CLOSE_WINDOW` ClientMessage：礼貌关闭窗口。
    pub fn close_window(&self, window: x11rb::protocol::xproto::Window) -> Result<()> {
        let event =
            ClientMessageEvent::new(32, window, self.atoms._NET_CLOSE_WINDOW, [0u32, 0, 0, 0, 0]);
        self.send_root_event(
            event,
            EventMask::SUBSTRUCTURE_REDIRECT | EventMask::SUBSTRUCTURE_NOTIFY,
        )
    }

    /// `_NET_MOVERESIZE_WINDOW`：一次设定窗口位置与尺寸。
    ///
    /// 未提供的坐标以 0 占位且对应标志位为 0——WM 保持该字段不变。
    /// 坐标语义为带符号值（EWMH data.l[1..4]）：负坐标合法（主屏左侧的
    /// 显示器），按补码位型直传，服务端按符号解释——不做钳位。
    pub fn move_resize_window(
        &self,
        window: x11rb::protocol::xproto::Window,
        x: Option<i32>,
        y: Option<i32>,
        w: Option<i32>,
        h: Option<i32>,
    ) -> Result<()> {
        let mut flags = 0u32;
        flags |= usize_flag(x.as_ref(), moveresize_flags::X);
        flags |= usize_flag(y.as_ref(), moveresize_flags::Y);
        flags |= usize_flag(w.as_ref(), moveresize_flags::WIDTH);
        flags |= usize_flag(h.as_ref(), moveresize_flags::HEIGHT);

        let data: [u32; 5] = [
            flags,
            x.unwrap_or(0) as u32,
            y.unwrap_or(0) as u32,
            w.unwrap_or(0) as u32,
            h.unwrap_or(0) as u32,
        ];
        let event = ClientMessageEvent::new(32, window, self.atoms._NET_MOVERESIZE_WINDOW, data);
        self.send_root_event(
            event,
            EventMask::SUBSTRUCTURE_REDIRECT | EventMask::SUBSTRUCTURE_NOTIFY,
        )
    }

    /// `_NET_WM_STATE` ADD：最小化（EWMH 无 unminimize 消息；还原由 activate 完成）。
    pub fn minimize_window(&self, window: x11rb::protocol::xproto::Window) -> Result<()> {
        self.wm_state_request(
            window,
            wm_state_action::ADD,
            self.atoms._NET_WM_STATE_HIDDEN,
            None,
        )
    }

    /// 还原最小化：清除 HIDDEN + 最大化状态后激活。
    pub fn unminimize_window(&self, window: x11rb::protocol::xproto::Window) -> Result<()> {
        self.toggle_wm_state(
            window,
            wm_state_action::REMOVE,
            self.atoms._NET_WM_STATE_HIDDEN,
        )?;
        self.activate_window(window)
    }

    /// 双向最大化（VERT+HORZ 同时设置）。
    pub fn maximize_window(
        &self,
        window: x11rb::protocol::xproto::Window,
        maximize: bool,
    ) -> Result<()> {
        let action = if maximize {
            wm_state_action::ADD
        } else {
            wm_state_action::REMOVE
        };
        self.toggle_wm_state(window, action, self.atoms._NET_WM_STATE_MAXIMIZED_VERT)?;
        self.toggle_wm_state(window, action, self.atoms._NET_WM_STATE_MAXIMIZED_HORZ)
    }

    /// 全屏切换。
    pub fn set_fullscreen(
        &self,
        window: x11rb::protocol::xproto::Window,
        fullscreen: bool,
    ) -> Result<()> {
        let action = if fullscreen {
            wm_state_action::ADD
        } else {
            wm_state_action::REMOVE
        };
        self.toggle_wm_state(window, action, self.atoms._NET_WM_STATE_FULLSCREEN)
    }

    /// keep-above 切换。
    pub fn set_keep_above(
        &self,
        window: x11rb::protocol::xproto::Window,
        above: bool,
    ) -> Result<()> {
        let action = if above {
            wm_state_action::ADD
        } else {
            wm_state_action::REMOVE
        };
        self.toggle_wm_state(window, action, self.atoms._NET_WM_STATE_ABOVE)
    }

    /// `_NET_WM_STATE` 通用切换（两个状态原子中第二个传 `None` 忽略）。
    pub fn toggle_wm_state(
        &self,
        window: x11rb::protocol::xproto::Window,
        action: u32,
        state_atom: u32,
    ) -> Result<()> {
        self.wm_state_request(window, action, state_atom, None)
    }

    fn wm_state_request(
        &self,
        window: x11rb::protocol::xproto::Window,
        action: u32,
        state_atom: u32,
        second_atom: Option<u32>,
    ) -> Result<()> {
        let event = ClientMessageEvent::new(
            32,
            window,
            self.atoms._NET_WM_STATE,
            [action, state_atom, second_atom.unwrap_or(0), 0, 0],
        );
        self.send_root_event(
            event,
            EventMask::SUBSTRUCTURE_REDIRECT | EventMask::SUBSTRUCTURE_NOTIFY,
        )
    }

    /// `_NET_CURRENT_DESKTOP` ClientMessage：切换工作区。
    pub fn set_current_desktop(&self, desktop: u32) -> Result<()> {
        let event = ClientMessageEvent::new(
            32,
            self.root,
            self.atoms._NET_CURRENT_DESKTOP,
            [desktop, x11rb::CURRENT_TIME, 0, 0, 0],
        );
        self.send_root_event(
            event,
            EventMask::SUBSTRUCTURE_REDIRECT | EventMask::SUBSTRUCTURE_NOTIFY,
        )
    }

    /// `_NET_WM_DESKTOP` ClientMessage：把窗口移到工作区（0xFFFF = 所有工作区）。
    pub fn move_window_to_desktop(
        &self,
        window: x11rb::protocol::xproto::Window,
        desktop: u32,
    ) -> Result<()> {
        let event = ClientMessageEvent::new(
            32,
            window,
            self.atoms._NET_WM_DESKTOP,
            [desktop, 0, 0, 0, 0],
        );
        self.send_root_event(
            event,
            EventMask::SUBSTRUCTURE_REDIRECT | EventMask::SUBSTRUCTURE_NOTIFY,
        )
    }

    // ───────────────────────── XTest 输入注入（§6.3） ─────────────────────────

    /// XTest 合成按键事件（keycode 为硬件键码，非 keysym）。
    pub fn fake_key_event(&self, keycode: u8, is_press: bool) -> Result<()> {
        self.require_xtest()?;
        let type_ = if is_press {
            x11rb::protocol::xproto::KEY_PRESS_EVENT
        } else {
            x11rb::protocol::xproto::KEY_RELEASE_EVENT
        };
        self.conn
            .xtest_fake_input(type_, keycode, x11rb::CURRENT_TIME, self.root, 0, 0, 0)
            .map_err(cerr)?;
        self.conn.flush().map_err(cerr)
    }

    /// XTest 合成鼠标按键事件。
    pub fn fake_button_event(&self, button: u8, is_press: bool) -> Result<()> {
        self.require_xtest()?;
        let type_ = if is_press {
            x11rb::protocol::xproto::BUTTON_PRESS_EVENT
        } else {
            x11rb::protocol::xproto::BUTTON_RELEASE_EVENT
        };
        self.conn
            .xtest_fake_input(type_, button, x11rb::CURRENT_TIME, self.root, 0, 0, 0)
            .map_err(cerr)?;
        self.conn.flush().map_err(cerr)
    }

    /// XTest 合成指针绝对移动。
    pub fn fake_motion_event(&self, x: i16, y: i16) -> Result<()> {
        self.require_xtest()?;
        self.conn
            .xtest_fake_input(
                x11rb::protocol::xproto::MOTION_NOTIFY_EVENT,
                0,
                x11rb::CURRENT_TIME,
                self.root,
                x,
                y,
                0,
            )
            .map_err(cerr)?;
        self.conn.flush().map_err(cerr)
    }

    // ───────────────────────── 截图（§6.4） ─────────────────────────

    /// XGetImage 抓取窗口内容（MIT-SHM 零拷贝优化留给 capture 组件 T2b）。
    pub fn capture_window(&self, window: x11rb::protocol::xproto::Window) -> Result<Vec<u8>> {
        let geo = self.get_window_geometry(window)?;
        let reply = self
            .conn
            .get_image(
                ImageFormat::Z_PIXMAP,
                window,
                0,
                0,
                geo.width.max(1) as u16,
                geo.height.max(1) as u16,
                !0,
            )
            .map_err(cerr)?
            .reply()
            .map_err(rerr)?;
        Ok(reply.data)
    }

    // ───────────────────────── 监视器信息 ─────────────────────────

    /// RandR 主输出几何（DisplayLayoutComponent 的 X11 数据源）。
    ///
    /// RandR 不可用时回退屏幕整体尺寸。
    pub fn primary_output_geometry(&self) -> Result<Rect> {
        let setup = self.conn.setup();
        let screen = setup.roots.get(self.screen_index).ok_or_else(|| {
            AgentShellError::BackendUnavailable("x11: screen index out of range".into())
        })?;

        if let Ok(outputs) = self
            .conn
            .randr_get_screen_resources(self.root)
            .map_err(cerr)?
            .reply()
        {
            for crtc_id in outputs.crtcs.iter().copied() {
                if let Ok(info) = self
                    .conn
                    .randr_get_crtc_info(crtc_id, x11rb::CURRENT_TIME)
                    .map_err(cerr)?
                    .reply()
                {
                    if info.width > 0 && info.height > 0 {
                        return Ok(Rect {
                            x: info.x as i32,
                            y: info.y as i32,
                            width: info.width as i32,
                            height: info.height as i32,
                        });
                    }
                }
            }
        }
        Ok(Rect {
            x: 0,
            y: 0,
            width: screen.width_in_pixels as i32,
            height: screen.height_in_pixels as i32,
        })
    }

    /// 同步等待服务端处理完已发请求（测试断言用）。
    ///
    /// 卡死保护由调用方用 tokio 超时包裹——x11rb 同步 API 无内建超时，
    /// 此处不提供名不副实的 timeout 变体。
    pub fn sync(&self) -> Result<()> {
        use x11rb::wrapper::ConnectionExt as _;
        self.conn
            .sync()
            .map_err(|e| AgentShellError::DBus(format!("x11 sync: {e}")))
    }

    // ───────────────────────── 内部工具 ─────────────────────────

    fn require_xtest(&self) -> Result<()> {
        if self.xtest_available {
            Ok(())
        } else {
            Err(AgentShellError::Input(
                "x11: XTest extension unavailable (XWayland?); fall back to libei/ydotool".into(),
            ))
        }
    }

    /// 向 root 广播 ClientMessage（EWMH 标准做法）。
    fn send_root_event(&self, event: ClientMessageEvent, mask: EventMask) -> Result<()> {
        self.conn
            .send_event(false, self.root, mask, event)
            .map_err(cerr)?;
        self.conn.flush().map_err(cerr)
    }

    fn property_bytes(
        &self,
        window: x11rb::protocol::xproto::Window,
        property: u32,
        type_: u32,
    ) -> Result<Option<Vec<u8>>> {
        let reply = self
            .conn
            .get_property(false, window, property, type_, 0, u32::MAX)
            .map_err(cerr)?
            .reply()
            .map_err(rerr)?;
        if reply.type_ == 0 && reply.value.is_empty() {
            Ok(None)
        } else {
            Ok(Some(reply.value))
        }
    }

    fn get_property_u32(
        &self,
        window: x11rb::protocol::xproto::Window,
        property: u32,
        type_: u32,
    ) -> Result<Vec<u32>> {
        let Some(raw) = self.property_bytes(window, property, type_)? else {
            return Ok(Vec::new());
        };
        Ok(raw
            .chunks_exact(4)
            .map(|c| u32::from_be_bytes(c.try_into().expect("4-byte chunk")))
            .collect())
    }

    fn get_property_string(
        &self,
        window: x11rb::protocol::xproto::Window,
        property: u32,
        type_: u32,
    ) -> Result<Option<String>> {
        Ok(self.property_bytes(window, property, type_)?.map(|raw| {
            String::from_utf8_lossy(&raw)
                .trim_end_matches('\0')
                .to_owned()
        }))
    }
}

fn usize_flag(value: Option<&i32>, flag: u32) -> u32 {
    if value.is_some() {
        flag
    } else {
        0
    }
}

fn cerr(e: ConnectionError) -> AgentShellError {
    AgentShellError::DBus(format!("x11 request: {e}"))
}

fn rerr(e: ReplyError) -> AgentShellError {
    AgentShellError::DBus(format!("x11 reply: {e}"))
}

fn x11_err(e: &ConnectError) -> String {
    match e {
        ConnectError::DisplayParsingError(_) => "invalid DISPLAY format".into(),
        ConnectError::SetupFailed(msg) => {
            format!("setup failed: {}", String::from_utf8_lossy(&msg.reason))
        }
        ConnectError::SetupAuthenticate(msg) => {
            format!("auth failed: {}", String::from_utf8_lossy(&msg.reason))
        }
        other => format!("{other}"),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn connect_fails_cleanly_without_display() {
        // DISPLAY 指向不存在套接字：必须结构化 BackendUnavailable 而非 panic。
        let err = X11DisplayServer::connect_to(Some(":999")).unwrap_err();
        assert!(
            matches!(err, AgentShellError::BackendUnavailable(_)),
            "got: {err:?}"
        );
    }
}
