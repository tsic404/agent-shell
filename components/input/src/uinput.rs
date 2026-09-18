//! uinput 直写后端（设计文档 §12.4 的替代路径）。
//!
//! 不依赖 ydotool 二进制/ydotoold：直接以 O_WRONLY 打开 `/dev/uinput`，用 ioctl
//! 注册一个虚拟输入设备，再向该 fd 写 `input_event` 完成键盘/鼠标/滚轮注入。
//! 能力与 ydotool 后端等价（key/type/move/click/scroll），用于 portal 缺
//! `ConnectToEIS` 且未安装 ydotool 时的降级。
//!
//! 权限模型（§20.3）：调用者需对 `/dev/uinput` 有写权限——由 udev 规则
//! `60-agent-shell-uinput.rules` 授予 `uinput` 组（与 ydotool 一致）。可写性在
//! 构造期探测，不可写即跳过候选（dispatcher 保存失败原因，整链空链时透出
//! `BackendUnavailable` 可诊断错误）。

use std::os::fd::{AsRawFd, RawFd};
use std::os::unix::fs::OpenOptionsExt;

use agent_shell_core::component::ComponentHealth;
use agent_shell_core::error::{AgentShellError, Result};
use agent_shell_core::types::{KeyCombo, MouseButton};
use async_trait::async_trait;
use tokio::sync::Mutex;

use super::dispatcher::{InputService, Op};
use crate::keymap::{char_to_evdev, combo_to_press_sequence, evdev_mouse_button};

// ─── linux/uinput.h ioctl 请求码（_IOW/_IO 展开值，x86_64 ABI） ───
/// `UI_SET_EVBIT = _IOW('U', 100, int)`。
const UI_SET_EVBIT: libc::c_ulong = 0x4004_5564;
/// `UI_SET_KEYBIT = _IOW('U', 101, int)`。
const UI_SET_KEYBIT: libc::c_ulong = 0x4004_5565;
/// `UI_SET_RELBIT = _IOW('U', 102, int)`。
const UI_SET_RELBIT: libc::c_ulong = 0x4004_5566;
/// `UI_SET_ABSBIT = _IOW('U', 103, int)`。
const UI_SET_ABSBIT: libc::c_ulong = 0x4004_5567;
/// `UI_SET_PROPBIT = _IOW('U', 110, int)`。
const UI_SET_PROPBIT: libc::c_ulong = 0x4004_556e;
/// `UI_DEV_CREATE = _IO('U', 1)`。
const UI_DEV_CREATE: libc::c_ulong = 0x5501;

// ─── linux/input-event-codes.h ───
const EV_SYN: u16 = 0x00;
const EV_KEY: u16 = 0x01;
const EV_REL: u16 = 0x02;
const EV_ABS: u16 = 0x03;
const SYN_REPORT: u16 = 0x00;
const REL_HWHEEL: u16 = 0x06;
const REL_WHEEL: u16 = 0x08;
const ABS_X: u16 = 0x00;
const ABS_Y: u16 = 0x01;
/// 左 Shift（临时 shift 合成大写/符号键用）。
const KEY_LEFTSHIFT: u16 = 42;
/// 虚拟输入设备 ABS 轴逻辑上限（min=0；合成器把 [0, max] 映射到整屏）。
const ABS_RANGE_MAX: i32 = 0x7fff;
/// `INPUT_PROP_POINTER`：设备带指针/光标，libinput 据此归类为 pointer，
/// 避免 ABS 轴设备被合成器启发式误判为 touch/tablet。
const INPUT_PROP_POINTER: u16 = 0x00;
/// `input_id.bustype`：BUS_VIRTUAL。
const BUS_VIRTUAL: u16 = 0x06;
/// `uinput_user_dev.name` 长度。
const UINPUT_MAX_NAME_SIZE: usize = 80;
/// `ABS_CNT = ABS_MAX(0x3f) + 1`。
const ABS_CNT: usize = 64;
/// 设备名。
const DEVICE_NAME: &[u8] = b"agent-shell virtual input";

/// linux/input.h `struct input_event`（x86_64 ABI：time 16 字节 + 8 字节 type/code/value）。
#[repr(C)]
struct InputEvent {
    time: libc::timeval,
    type_: u16,
    code: u16,
    value: i32,
}

/// linux/input.h `struct input_id`。
#[repr(C)]
struct InputId {
    bustype: u16,
    vendor: u16,
    product: u16,
    version: u16,
}

/// linux/uinput.h `struct uinput_user_dev`。
#[repr(C)]
struct UinputUserDev {
    name: [u8; UINPUT_MAX_NAME_SIZE],
    id: InputId,
    ff_effects_max: u32,
    absmax: [i32; ABS_CNT],
    absmin: [i32; ABS_CNT],
    absfuzz: [i32; ABS_CNT],
    absflat: [i32; ABS_CNT],
}

/// 设备广告的键码范围：keymap 可能产出的全部键码 + 鼠标按键 BTN_*。
///
/// 与 `keymap` 产出保持一致——漏掉任一键码，对应键的注入会被内核静默丢弃
/// （错误极难在真机之外复现）。
const KEY_CODE_RANGES: &[std::ops::RangeInclusive<u32>] = &[
    1..=15,        // Esc、1..0、BackSpace、Tab、-、=
    26..=57,       // [ ] \ Enter Ctrl、a..z、Alt、Space、; ' ` Shift
    59..=68,       // F1..F10
    87..=88,       // F11..F12
    102..=111,     // Home..Delete
    125..=125,     // Super
    139..=139,     // Menu
    0x110..=0x116, // BTN_LEFT..BTN_BACK（含 BTN_SIDE/EXTRA）
];

/// uinput 直写注入后端。
pub struct UinputInput {
    /// 已打开的 `/dev/uinput` 写句柄（构造时探测可写性）。
    file: std::fs::File,
    /// 虚拟设备是否已创建（UI_DEV_CREATE 已调用）。
    created: Mutex<bool>,
    /// 串行化注入：单一 uinput fd 上多事件序列必须原子提交，否则并发调用会
    /// 交错 press/release 帧（type_text 字符间 sleep 是 await 点，最易触发）。
    /// 整 op（含 sleep 段）持 guard，故用 tokio Mutex（对齐 XTest 的 inject_lock）。
    inject_lock: Mutex<()>,
}

impl UinputInput {
    /// 构造候选实例。探测 `/dev/uinput` 可写（open O_WRONLY|O_NONBLOCK）；
    /// 不可写/权限不足返回 `Err`，由 dispatcher 跳过并记录失败原因。
    pub fn new() -> Result<Self> {
        let file = open_uinput().map_err(|e| {
            // 按 errno 区分诊断：节点缺失（ENOENT/ENODEV）与权限不足（EACCES/EPERM）
            // 是两种不同的环境故障，混用「not writable」会误导排障。
            let reason = match e.raw_os_error() {
                Some(libc::ENOENT) | Some(libc::ENODEV) => "/dev/uinput not present".to_string(),
                Some(libc::EACCES) | Some(libc::EPERM) => {
                    format!("/dev/uinput not writable: {e}")
                }
                _ => format!("/dev/uinput open failed: {e}"),
            };
            AgentShellError::BackendUnavailable(reason)
        })?;
        Ok(Self {
            file,
            created: Mutex::new(false),
            inject_lock: Mutex::new(()),
        })
    }

    /// 首次注入时注册虚拟设备（ioctl 位设置 + UI_DEV_CREATE），跨调用复用。
    async fn ensure_created(&self) -> Result<()> {
        let mut created = self.created.lock().await;
        if *created {
            return Ok(());
        }
        register_device(&self.file).map_err(|e| {
            AgentShellError::BackendUnavailable(format!("uinput device setup failed: {e}"))
        })?;
        *created = true;
        Ok(())
    }
}

/// 打开 `/dev/uinput`（O_WRONLY | O_NONBLOCK）。
fn open_uinput() -> std::io::Result<std::fs::File> {
    std::fs::OpenOptions::new()
        .write(true)
        .custom_flags(libc::O_NONBLOCK)
        .open("/dev/uinput")
}

/// 注册虚拟设备：事件类型位、键位、相对/绝对位、legacy setup write、UI_DEV_CREATE。
///
/// 顺序不可颠倒：`UI_DEV_CREATE` 是「提交」而非「构造」。内核
/// `uinput_create_device` 对 `state != UIST_SETUP_COMPLETE` 的 udev 直接
/// `-EINVAL`（无 dmesg），而 state 只有先 `write` 完整 `uinput_user_dev`
/// （`uinput_setup_device_legacy`）才置位。
fn register_device(file: &std::fs::File) -> std::io::Result<()> {
    let fd = file.as_raw_fd();
    set_bit(fd, UI_SET_EVBIT, EV_KEY)?;
    set_bit(fd, UI_SET_EVBIT, EV_REL)?;
    set_bit(fd, UI_SET_EVBIT, EV_ABS)?;
    for range in KEY_CODE_RANGES {
        for code in range.clone() {
            set_bit(fd, UI_SET_KEYBIT, code as u16)?;
        }
    }
    set_bit(fd, UI_SET_RELBIT, REL_WHEEL)?;
    set_bit(fd, UI_SET_RELBIT, REL_HWHEEL)?;
    set_bit(fd, UI_SET_ABSBIT, ABS_X)?;
    set_bit(fd, UI_SET_ABSBIT, ABS_Y)?;
    // 显式声明 pointer：ABS 轴 + BTN_LEFT 组合在部分合成器下依赖启发式归类，
    // 标 INPUT_PROP_POINTER 使其确定性地被 libinput 当作指针（带光标）。
    set_bit(fd, UI_SET_PROPBIT, INPUT_PROP_POINTER)?;

    // Legacy uinput 路径：一次性 write 完整 struct uinput_user_dev（内核要求
    // 单次 write 的 count == sizeof(struct)），随后 UI_DEV_CREATE（无参数）。
    write_all_once(file, pod_bytes(&new_user_dev()))?;
    let ret = unsafe { libc::ioctl(fd, UI_DEV_CREATE) };
    if ret < 0 {
        return Err(std::io::Error::last_os_error());
    }
    Ok(())
}

/// 构造 legacy `uinput_user_dev` 设备描述（name/id/ABS 范围）。
fn new_user_dev() -> UinputUserDev {
    let mut dev = UinputUserDev {
        name: [0u8; UINPUT_MAX_NAME_SIZE],
        id: InputId {
            bustype: BUS_VIRTUAL,
            vendor: 0,
            product: 0,
            version: 0,
        },
        ff_effects_max: 0,
        absmax: [0; ABS_CNT],
        absmin: [0; ABS_CNT],
        absfuzz: [0; ABS_CNT],
        absflat: [0; ABS_CNT],
    };
    dev.name[..DEVICE_NAME.len()].copy_from_slice(DEVICE_NAME);
    dev.absmax[ABS_X as usize] = ABS_RANGE_MAX;
    dev.absmax[ABS_Y as usize] = ABS_RANGE_MAX;
    dev
}

/// `UI_SET_*_BIT` ioctl：第三个参数为位索引（int）。
fn set_bit(fd: RawFd, request: libc::c_ulong, code: u16) -> std::io::Result<()> {
    let ret = unsafe { libc::ioctl(fd, request, code as libc::c_int) };
    if ret < 0 {
        Err(std::io::Error::last_os_error())
    } else {
        Ok(())
    }
}

/// repr(C) POD 的字节视图（用于单次 write 到 uinput fd）。
fn pod_bytes<T>(value: &T) -> &[u8] {
    // SAFETY: 调用方仅传 repr(C) 且无内部指针的 POD；切片生命周期绑定入参。
    unsafe { std::slice::from_raw_parts(value as *const T as *const u8, std::mem::size_of::<T>()) }
}

/// 单次 `write` 全量字节到 uinput fd；短写视为错误。
///
/// 刻意不用 `Write::write_all`：legacy setup 要求**一次** write 提交完整
/// `uinput_user_dev`，内核按 `count != sizeof(struct)` 拒绝分片。
fn write_all_once(file: &std::fs::File, bytes: &[u8]) -> std::io::Result<()> {
    // SAFETY: bytes 是有效切片，len 为其真实字节长度；uinput 是字符设备，
    // write 语义与文件一致。
    let written = unsafe {
        libc::write(
            file.as_raw_fd(),
            bytes.as_ptr() as *const libc::c_void,
            bytes.len(),
        )
    };
    if written < 0 {
        return Err(std::io::Error::last_os_error());
    }
    if written as usize != bytes.len() {
        return Err(std::io::Error::other(format!(
            "short write to uinput: {written}/{} bytes",
            bytes.len()
        )));
    }
    Ok(())
}

/// 写一条 input_event（不含 SYN）。
fn write_event(file: &std::fs::File, type_: u16, code: u16, value: i32) -> std::io::Result<()> {
    let ev = InputEvent {
        time: libc::timeval {
            tv_sec: 0,
            tv_usec: 0,
        },
        type_,
        code,
        value,
    };
    write_all_once(file, pod_bytes(&ev))
}

/// 帧尾 SYN_REPORT：告知合成器上一帧事件已完整，可立即消费。
fn sync(file: &std::fs::File) -> std::io::Result<()> {
    write_event(file, EV_SYN, SYN_REPORT, 0)
}

#[async_trait]
impl InputService for UinputInput {
    fn name(&self) -> &'static str {
        "uinput"
    }

    async fn is_available(&self) -> bool {
        // 构造期已验证 /dev/uinput 可写；设备创建延迟到首次注入。
        true
    }

    async fn health(&self) -> ComponentHealth {
        match *self.created.lock().await {
            true => ComponentHealth::Healthy,
            false => ComponentHealth::Degraded("uinput device not yet created (lazy)".into()),
        }
    }

    async fn ensure_ready(&self, _op: Op<'_>) -> Result<()> {
        // 设备注册失败发生在任何注入之前——dispatcher 据此安全降级重放。
        self.ensure_created().await
    }

    async fn send_key(&self, combo: &KeyCombo) -> Result<()> {
        let _guard = self.inject_lock.lock().await;
        self.ensure_created().await?;
        // combo_to_press_sequence 的 bool 是 is_temp_shift 标记，非 down 状态；
        // press 阶段统一置 1，逆序释放置 0（临时 shift 在其实体键后弹起）。
        let order = combo_to_press_sequence(combo).map_err(AgentShellError::Input)?;
        for (code, _) in &order {
            write_event(&self.file, EV_KEY, *code as u16, 1).map_err(input_err)?;
            sync(&self.file).map_err(input_err)?;
        }
        for (code, _) in order.iter().rev() {
            write_event(&self.file, EV_KEY, *code as u16, 0).map_err(input_err)?;
            sync(&self.file).map_err(input_err)?;
        }
        Ok(())
    }

    async fn type_text(&self, text: &str, delay_ms: u32) -> Result<()> {
        // 整 op 持锁（含字符间 sleep）：并发调用不得交错字符序列。
        let _guard = self.inject_lock.lock().await;
        self.ensure_created().await?;
        for c in text.chars() {
            let (code, shift) = char_to_evdev(c)
                .ok_or_else(|| AgentShellError::Input(format!("cannot type character {c:?}")))?;
            if shift {
                write_event(&self.file, EV_KEY, KEY_LEFTSHIFT, 1).map_err(input_err)?;
                sync(&self.file).map_err(input_err)?;
            }
            write_event(&self.file, EV_KEY, code as u16, 1).map_err(input_err)?;
            sync(&self.file).map_err(input_err)?;
            write_event(&self.file, EV_KEY, code as u16, 0).map_err(input_err)?;
            sync(&self.file).map_err(input_err)?;
            if shift {
                write_event(&self.file, EV_KEY, KEY_LEFTSHIFT, 0).map_err(input_err)?;
                sync(&self.file).map_err(input_err)?;
            }
            if delay_ms > 0 {
                tokio::time::sleep(std::time::Duration::from_millis(u64::from(delay_ms))).await;
            }
        }
        Ok(())
    }

    async fn mouse_move(&self, x: i32, y: i32) -> Result<()> {
        let _guard = self.inject_lock.lock().await;
        self.ensure_created().await?;
        write_event(&self.file, EV_ABS, ABS_X, x.clamp(0, ABS_RANGE_MAX)).map_err(input_err)?;
        write_event(&self.file, EV_ABS, ABS_Y, y.clamp(0, ABS_RANGE_MAX)).map_err(input_err)?;
        sync(&self.file).map_err(input_err)?;
        Ok(())
    }

    async fn mouse_click(&self, button: MouseButton) -> Result<()> {
        let _guard = self.inject_lock.lock().await;
        self.ensure_created().await?;
        let code = evdev_mouse_button(button) as u16;
        write_event(&self.file, EV_KEY, code, 1).map_err(input_err)?;
        sync(&self.file).map_err(input_err)?;
        write_event(&self.file, EV_KEY, code, 0).map_err(input_err)?;
        sync(&self.file).map_err(input_err)?;
        Ok(())
    }

    async fn mouse_scroll(&self, dx: i32, dy: i32) -> Result<()> {
        let _guard = self.inject_lock.lock().await;
        self.ensure_created().await?;
        if dy != 0 {
            // 契约 dy>0=向下；evdev REL_WHEEL 正值=向上（=X11 button 4），故取反。
            write_event(&self.file, EV_REL, REL_WHEEL, rel_wheel_value(dy)).map_err(input_err)?;
            sync(&self.file).map_err(input_err)?;
        }
        if dx != 0 {
            // REL_HWHEEL 正值=向右，与契约 dx>0=向右一致，原样写入。
            write_event(&self.file, EV_REL, REL_HWHEEL, dx).map_err(input_err)?;
            sync(&self.file).map_err(input_err)?;
        }
        Ok(())
    }
}

fn input_err(e: std::io::Error) -> AgentShellError {
    AgentShellError::Input(format!("uinput write: {e}"))
}

/// 垂直滚轮值：契约 dy>0=向下，evdev REL_WHEEL 正值=向上（=X11 button 4），故取反。
fn rel_wheel_value(dy: i32) -> i32 {
    -dy
}

/// 键码是否在设备的 KEYBIT 使能集合内（keymap 产出 + 鼠标按键）。
#[cfg(test)]
fn key_bit_enabled(code: u32) -> bool {
    KEY_CODE_RANGES.iter().any(|r| r.contains(&code))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::keymap::{char_to_evdev, named_to_evdev};
    use agent_shell_core::types::KeyName;

    #[test]
    fn input_event_layout_matches_kernel_abi() {
        // 布局错误（如 timeval 长度/对齐不符）会导致真机注入静默失效，
        // 离线无法复现——用字节偏移锚定内核 ABI。
        assert_eq!(std::mem::size_of::<InputEvent>(), 24);
        let ev = InputEvent {
            time: libc::timeval {
                tv_sec: 0,
                tv_usec: 0,
            },
            type_: EV_KEY,
            code: 30,
            value: 1,
        };
        let bytes = pod_bytes(&ev);
        assert_eq!(bytes.len(), 24);
        assert_eq!(&bytes[16..18], &[0x01, 0x00], "type (EV_KEY)");
        assert_eq!(&bytes[18..20], &[0x1e, 0x00], "code (KEY_A)");
        assert_eq!(&bytes[20..24], &[0x01, 0x00, 0x00, 0x00], "value (press)");
    }

    #[test]
    fn user_dev_layout_matches_kernel_abi() {
        // legacy setup 要求单次 write 的 count == sizeof(struct uinput_user_dev)；
        // 布局偏差（如 ABS_CNT 或对齐不符）会让内核直接 -EINVAL，离线不可复现。
        assert_eq!(std::mem::size_of::<UinputUserDev>(), 1116);
        let dev = new_user_dev();
        // 内核 uinput_setup_device_legacy 拒绝空 name。
        assert_ne!(dev.name[0], 0, "device name must be non-empty");
        assert_eq!(&dev.name[..DEVICE_NAME.len()], DEVICE_NAME);
        assert_eq!(
            dev.name[DEVICE_NAME.len()],
            0,
            "name must be NUL-terminated"
        );
        assert_eq!(dev.id.bustype, BUS_VIRTUAL);
        assert_eq!(dev.absmin[ABS_X as usize], 0);
        assert_eq!(dev.absmax[ABS_X as usize], ABS_RANGE_MAX);
        assert_eq!(dev.absmin[ABS_Y as usize], 0);
        assert_eq!(dev.absmax[ABS_Y as usize], ABS_RANGE_MAX);
    }

    #[tokio::test]
    async fn device_creation_succeeds_with_writable_uinput() {
        // 构造路径端到端回归：legacy write(struct uinput_user_dev) → UI_DEV_CREATE。
        // 缺 write 时内核 uinput_create_device 恒 -EINVAL（QA 已复现的真实缺陷）。
        // `/dev/uinput` 不可用（CI/无 uinput 内核）时跳过，不误报失败。
        let input = match UinputInput::new() {
            Ok(i) => i,
            Err(e) => {
                eprintln!("skip: {e}");
                return;
            }
        };
        input
            .ensure_ready(Op::Scroll(0, 0))
            .await
            .expect("uinput device creation (legacy write + UI_DEV_CREATE) must succeed");
        // ioctl 返回 0 不足以证明设备真被注册——内核 input 设备表里应出现设备名。
        let devices = std::fs::read_to_string("/proc/bus/input/devices")
            .expect("/proc/bus/input/devices readable");
        let block = devices
            .split("\n\n")
            .find(|b| b.contains("agent-shell virtual input"))
            .expect("created device must be registered with the kernel");
        // INPUT_PROP_POINTER 必须真的落到设备位图上：请求码错值（如 UI_SET_LEDBIT
        // 0x40045569）同样返回 0，但位图不变——合成器归类随之退化为启发式。
        let prop = block
            .lines()
            .find_map(|l| l.strip_prefix("B: PROP="))
            .expect("PROP bitmask line for created device");
        let prop = u32::from_str_radix(prop.trim(), 16).expect("hex PROP bitmask");
        assert_eq!(prop & 1, 1, "INPUT_PROP_POINTER (bit 0) must be set");
    }

    #[test]
    fn scroll_sign_matches_backend_contract() {
        // 契约（dispatcher trait 文档 + XTest/xdotool/ydotool 三后端）：dy>0=向下、
        // dy<0=向上。evdev REL_WHEEL 正值=向上（=X11 button 4），故取反。
        assert_eq!(rel_wheel_value(2), -2, "向下（dy>0）须映射为负 REL_WHEEL");
        assert_eq!(rel_wheel_value(-3), 3, "向上（dy<0）须映射为正 REL_WHEEL");
    }

    #[test]
    fn key_bit_ranges_cover_all_keymap_outputs() {
        for n in [
            KeyName::Return,
            KeyName::Escape,
            KeyName::BackSpace,
            KeyName::Tab,
            KeyName::Space,
            KeyName::Left,
            KeyName::Right,
            KeyName::Up,
            KeyName::Down,
            KeyName::Home,
            KeyName::End,
            KeyName::PageUp,
            KeyName::PageDown,
            KeyName::Insert,
            KeyName::Delete,
            KeyName::Menu,
            KeyName::F1,
            KeyName::F2,
            KeyName::F3,
            KeyName::F4,
            KeyName::F5,
            KeyName::F6,
            KeyName::F7,
            KeyName::F8,
            KeyName::F9,
            KeyName::F10,
            KeyName::F11,
            KeyName::F12,
        ] {
            let code = named_to_evdev(n).expect("named key mapped");
            assert!(key_bit_enabled(code), "{n:?} -> {code} not in KEYBIT set");
        }
        for c in "abcdefghijklmnopqrstuvwxyzABCDEFGHIJKLMNOPQRSTUVWXYZ0123456789 !@#$%^&*()_+-=[]{}|;:'\",.<>/?`~\\\t\n".chars() {
            if let Some((code, _)) = char_to_evdev(c) {
                assert!(key_bit_enabled(code), "char {c:?} -> {code} not in KEYBIT set");
            }
        }
        for code in [29u32, 56, 42, 125, 0x110, 0x111, 0x112, 0x115, 0x116] {
            assert!(key_bit_enabled(code), "code {code} not in KEYBIT set");
        }
    }
}
