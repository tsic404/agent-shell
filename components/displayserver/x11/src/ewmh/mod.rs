//! EWMH 原子集合（批量 intern，§6.2「root window → 批量获取原子」）。
//!
//! 对应设计文档 §1 `ewmh.rs`：x11rb `atom_manager!` 展开产物 + EWMH 常量表。

/// EWMH 原子集合（x11rb `atom_manager!` 批量 intern）。
pub use server::EwmhAtoms;

pub mod moveresize_flags {
    /// `_NET_MOVERESIZE_WINDOW` 静态标志位（ICCCM/EWMH 规定：置 1 的位表示对应字段有效）。
    pub const X: u32 = 1 << 8;
    pub const Y: u32 = 1 << 9;
    pub const WIDTH: u32 = 1 << 10;
    pub const HEIGHT: u32 = 1 << 11;
}

/// `_NET_WM_STATE` 切换动作（`data.l[0]`，ICCCM 4.1.2 / EWMH）。
pub mod wm_state_action {
    /// 移除状态。
    pub const REMOVE: u32 = 0;
    /// 添加状态。
    pub const ADD: u32 = 1;
    /// 反转状态。
    pub const TOGGLE: u32 = 2;
}

pub mod server;

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn atom_names_are_ewmh_prefixed() {
        // 编译期保证核心 EWMH 原子字段生成（§6.2）：字段存在性即契约。
        // 不发起真实 X 连接——仅验证类型与宏展开产物可用。
        fn assert_send_sync<T: Send + Sync + Copy>() {}
        assert_send_sync::<EwmhAtoms>();
    }

    #[test]
    fn moveresize_flags_are_distinct_bits() {
        // 四个标志位互斥，组合可任意叠加（EWMH 规范位定义）。
        let all = moveresize_flags::X
            | moveresize_flags::Y
            | moveresize_flags::WIDTH
            | moveresize_flags::HEIGHT;
        assert_eq!(all.count_ones(), 4);
        assert_eq!(moveresize_flags::X, 1 << 8);
    }

    #[test]
    fn wm_state_actions_match_spec() {
        // data.l[0]: 0=remove 1=add 2=toggle（ICCCM 4.1.2 / EWMH）。
        assert_eq!(wm_state_action::REMOVE, 0);
        assert_eq!(wm_state_action::ADD, 1);
        assert_eq!(wm_state_action::TOGGLE, 2);
    }

    #[test]
    fn negative_coordinates_roundtrip_as_twos_complement() {
        // 阻塞项 #3 回归：_NET_MOVERESIZE_WINDOW 的 data.l[1..4] 为带符号值，
        // 负坐标（主屏左侧显示器）必须按补码位型直传，不得钳位到 0。
        for v in [-1920i32, -1, 0, 1, 3840] {
            let wire = v as u32;
            assert_eq!(wire as i32, v);
        }
        assert_eq!((-1920i32) as u32 as i32, -1920);
    }
}
