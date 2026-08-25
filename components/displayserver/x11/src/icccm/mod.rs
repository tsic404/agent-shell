//! ICCCM 协议操作基础模块（设计文档 §6 / x11-display-server.md §6）。
//!
//! EWMH 覆盖「窗口管理器协议」（`_NET_*`）；ICCCM 是其下层的**客户端与
//! 窗口管理器之间的基础约定**（`WM_*`）。本模块提供各 X11 会话合成器
//! （KWin/Mutter/DDE/X11Compositor）共用的 ICCCM 原语，全部操作挂在
//! [`X11DisplayServer`](crate::X11DisplayServer) 上：
//!
//! - `WM_PROTOCOLS` 协商：探测目标窗口支持的 protocol（DELETE/TAKE_FOCUS…）
//! - `WM_DELETE_WINDOW`：优雅关闭（EWMH `_NET_CLOSE_WINDOW` 之外的客户端协议路径）
//! - `WM_STATE` / `WM_CHANGE_STATE`：经典（非 EWMH）最小化状态读写
//!
//! 原子在连接建立时批量 intern（[`WmProtocolAtoms`]，与 EWMH 同策略 §6.2）；
//! 其他合成器不直接持有本模块——统一走 `X11DisplayServer` 公开方法。
use agent_shell_core::error::{AgentShellError, Result};

use x11rb::protocol::xproto::ConnectionExt as _;

/// `WM_PROTOCOLS` 中本模块识别的 protocol。
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum WmProtocol {
    /// `WM_DELETE_WINDOW`——请求窗口自行关闭。
    DeleteWindow,
    /// `WM_TAKE_FOCUS`——焦点交给窗口（配合 `_NET_ACTIVE_WINDOW` 使用）。
    TakeFocus,
    /// `WM_PING`——窗口存活检测（响应超时 = 应用挂起）。
    Ping,
    /// 未识别的 protocol（保留原始原子值，调用方可与 `WmProtocolAtoms`
    /// 字段比对做精确归类）。
    Other(x11rb::protocol::xproto::Atom),
}

/// `WM_STATE` 经典状态码（ICCCM 4.1.3.1）。
pub mod wm_state {
    /// WithdrawnState：窗口未映射且未被管理。
    pub const WITHDRAWN: u32 = 0;
    /// NormalState：窗口正常映射。
    pub const NORMAL: u32 = 1;
    /// IconicState：窗口图标化（经典最小化）。
    pub const ICONIC: u32 = 3;
}

/// `WM_NORMAL_HINTS` flags 位（ICCCM 4.1.2.3）——resize 约束感知用。
pub mod size_hints_flags {
    /// USPosition | PPosition：位置由客户端/用户指定。
    pub const POSITION: u32 = (1 << 2) | (1 << 0);
    /// USSize | PSize：尺寸由客户端/用户指定。
    pub const SIZE: u32 = (1 << 3) | (1 << 1);
    /// PMinSize：min_width/min_height 有效。
    pub const MIN_SIZE: u32 = 1 << 4;
    /// PMaxSize：max_width/max_height 有效。
    pub const MAX_SIZE: u32 = 1 << 5;
    /// PResizeInc：width_inc/height_inc 有效。
    pub const RESIZE_INC: u32 = 1 << 6;
    /// PAspect：min/max aspect 有效。
    pub const ASPECT: u32 = 1 << 7;
    /// PBaseSize：base_width/base_height 有效。
    pub const BASE_SIZE: u32 = 1 << 8;
    /// PWinGravity：win_gravity 有效。
    pub const WIN_GRAVITY: u32 = 1 << 9;
}

/// `WM_*` 协议原子集合（连接时批量 intern，§6.2 策略）。
#[derive(Debug)]
pub struct WmProtocolAtoms {
    /// `WM_PROTOCOLS`：ClientMessage 消息类型原子。
    pub wm_protocols: u32,
    /// `WM_DELETE_WINDOW`：优雅关闭 protocol。
    pub wm_delete_window: u32,
    /// `WM_TAKE_FOCUS`：焦点协议。
    pub wm_take_focus: u32,
    /// `WM_PING`：存活检测 protocol。
    pub wm_ping: u32,
    /// `WM_STATE`：经典最小化状态属性。
    pub wm_state: u32,
    /// `WM_CHANGE_STATE`：客户端请求图标化/还原的消息类型。
    pub wm_change_state: u32,
}

impl WmProtocolAtoms {
    /// 批量 intern 全部 ICCCM 原子（一次 roundtrip；§6.2 批量获取策略）。
    pub fn new(conn: &x11rb::rust_connection::RustConnection) -> Result<Self> {
        const NAMES: [&str; 6] = [
            "WM_PROTOCOLS",
            "WM_DELETE_WINDOW",
            "WM_TAKE_FOCUS",
            "WM_PING",
            "WM_STATE",
            "WM_CHANGE_STATE",
        ];
        let cookies: Vec<_> = NAMES
            .iter()
            .map(|n| conn.intern_atom(false, n.as_bytes()))
            .collect();
        let mut vals = Vec::with_capacity(NAMES.len());
        for c in cookies {
            vals.push(
                c.map_err(|e| AgentShellError::DBus(format!("x11 request: {e}")))?
                    .reply()
                    .map_err(|e| AgentShellError::DBus(format!("x11 reply: {e}")))?
                    .atom,
            );
        }
        Ok(Self {
            wm_protocols: vals[0],
            wm_delete_window: vals[1],
            wm_take_focus: vals[2],
            wm_ping: vals[3],
            wm_state: vals[4],
            wm_change_state: vals[5],
        })
    }

    /// 把原始 protocol 原子归类为 [`WmProtocol`]。
    pub fn classify(&self, atom: u32) -> WmProtocol {
        match atom {
            a if a == self.wm_delete_window => WmProtocol::DeleteWindow,
            a if a == self.wm_take_focus => WmProtocol::TakeFocus,
            a if a == self.wm_ping => WmProtocol::Ping,
            other => WmProtocol::Other(other),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// 无 X 连接的纯函数测试：classify 四路分支全覆盖。
    #[test]
    fn classify_covers_all_protocol_branches() {
        // 原子值不需要真实 intern——classify 只做字段比对。
        let atoms = WmProtocolAtoms {
            wm_protocols: 391,
            wm_delete_window: 392,
            wm_take_focus: 338,
            wm_ping: 641,
            wm_state: 3,
            wm_change_state: 40,
        };
        assert_eq!(atoms.classify(392), WmProtocol::DeleteWindow);
        assert_eq!(atoms.classify(338), WmProtocol::TakeFocus);
        assert_eq!(atoms.classify(641), WmProtocol::Ping);
        assert_eq!(atoms.classify(391), WmProtocol::Other(391));
        assert_eq!(atoms.classify(12345), WmProtocol::Other(12345));
    }

    #[test]
    fn wm_state_codes_match_spec() {
        // ICCCM 4.1.3.1 状态码。
        assert_eq!(wm_state::WITHDRAWN, 0);
        assert_eq!(wm_state::NORMAL, 1);
        assert_eq!(wm_state::ICONIC, 3);
    }

    #[test]
    fn size_hints_flags_are_distinct_bits() {
        // ICCCM 4.1.2.3：各 flag 单 bit，可任意组合。
        let all = size_hints_flags::POSITION
            | size_hints_flags::SIZE
            | size_hints_flags::MIN_SIZE
            | size_hints_flags::MAX_SIZE
            | size_hints_flags::RESIZE_INC
            | size_hints_flags::ASPECT
            | size_hints_flags::BASE_SIZE
            | size_hints_flags::WIN_GRAVITY;
        assert_eq!(all.count_ones(), 10); // POSITION/SIZE 各占 2 位
    }
}
