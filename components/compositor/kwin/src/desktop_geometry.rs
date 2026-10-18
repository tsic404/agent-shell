//! 桌面逻辑几何探测（`wl_output` + `zxdg_output_v1`）。
//!
//! 输入组件要把 `input click --at X,Y` 的像素坐标换算成 uinput 直写设备的绝对
//! 轴值（`components/input/src/uinput.rs` 的 `pixel_to_abs`），换算系数就是桌面
//! 逻辑几何；这两个协议是**核心协议，不受 KWin 私有接口授权门控**——原生注入
//! 通道未绑定（缺 `X-KDE-Wayland-Interfaces` 条目）时，它是仍可用的几何来源。
//!
//! 坐标系：`zxdg_output_v1` 的 `logical_position`/`logical_size` 即合成器全局
//! 逻辑坐标（窗口几何、`--at` 同一空间）。只有包围盒原点为 `(0,0)` 时才返回结
//! 果——KWin 把设备绝对轴折算到 `workspace()->geometry().size()` 后按全局坐标
//! 使用，原点非零的布局下该折算与全局坐标错位；宁可不给几何（候选声明能力缺口、
//! 错误可归因）也不注入到错位置。
//!
//! 探测是只读的：结果不缓存（显示布局在 daemon 生命周期内可能变化，调用方——
//! 输入链——在每次移动注入前重取，而不是复用较早的结果），但 registry/队列/
//! manager 常态持有（[`Probe`]）：每次重取都重建 registry 会在合成器侧按重取
//! 次数累积对象（`wl_registry` 是 core 单例，只有 `bind` 请求、**没有析构
//! 请求**），而重取期临时绑定的输出对象有析构请求——读数后显式释放。

use std::collections::HashMap;

use agent_shell_core::types::Rect;
use wayland_client::globals::{registry_queue_init, GlobalList, GlobalListContents};
use wayland_client::protocol::wl_output::{self, WlOutput};
use wayland_client::protocol::wl_registry;
use wayland_client::{Connection, Dispatch, EventQueue, Proxy, QueueHandle};
use wayland_protocols::xdg::xdg_output::zv1::client::zxdg_output_manager_v1::ZxdgOutputManagerV1;
use wayland_protocols::xdg::xdg_output::zv1::client::zxdg_output_v1::{self, ZxdgOutputV1};

/// `zxdg_output_manager_v1` 请求版本上限。
///
/// 取 2：v3 起 `logical_position`/`logical_size` 在 xdg-output 侧停发（改用
/// `wl_output` 的 name/description），而本模块正需要这两个事件。
const XDG_OUTPUT_MAX_VERSION: u32 = 2;

/// `wl_output` 请求版本上限（仅作 xdg-output 的句柄参数，不需要 v4 事件）。
const WL_OUTPUT_MAX_VERSION: u32 = 4;

/// 探测期状态：`zxdg_output_v1` 对象 id → 该输出的逻辑几何。
#[derive(Debug, Default)]
struct GeometryState {
    outputs: HashMap<u32, OutputGeometry>,
}

#[derive(Debug, Default, Clone, Copy)]
struct OutputGeometry {
    position: Option<(i32, i32)>,
    /// `logical_size`（协议按 i32 编码，非正值视为无效）。
    size: Option<(i32, i32)>,
}

impl Dispatch<wl_registry::WlRegistry, GlobalListContents> for GeometryState {
    fn event(
        _: &mut Self,
        _: &wl_registry::WlRegistry,
        _: wl_registry::Event,
        _: &GlobalListContents,
        _: &Connection,
        _: &QueueHandle<Self>,
    ) {
    }
}

/// manager 无事件；仅用于绑定 `get_xdg_output`。
impl Dispatch<ZxdgOutputManagerV1, ()> for GeometryState {
    fn event(
        _: &mut Self,
        _: &ZxdgOutputManagerV1,
        _: <ZxdgOutputManagerV1 as Proxy>::Event,
        _: &(),
        _: &Connection,
        _: &QueueHandle<Self>,
    ) {
    }
}

/// `wl_output` 事件不消费：几何一律取自 `zxdg_output_v1`（见模块头坐标系说明）。
impl Dispatch<WlOutput, ()> for GeometryState {
    fn event(
        _: &mut Self,
        _: &WlOutput,
        _: wl_output::Event,
        _: &(),
        _: &Connection,
        _: &QueueHandle<Self>,
    ) {
    }
}

impl Dispatch<ZxdgOutputV1, ()> for GeometryState {
    fn event(
        state: &mut Self,
        output: &ZxdgOutputV1,
        event: zxdg_output_v1::Event,
        _: &(),
        _: &Connection,
        _: &QueueHandle<Self>,
    ) {
        let entry = state.outputs.entry(output.id().protocol_id()).or_default();
        match event {
            zxdg_output_v1::Event::LogicalPosition { x, y } => entry.position = Some((x, y)),
            zxdg_output_v1::Event::LogicalSize { width, height } => {
                entry.size = Some((width, height))
            }
            _ => {}
        }
    }
}

/// 常驻探测态：registry 快照 + 派发队列 + xdg-output manager（一次建立、多次重取）。
///
/// 为什么不能每次重取都 `registry_queue_init`：`wl_registry` 是 core 单例对象，
/// 只有 `bind` 请求而**没有析构请求**，每次重取新建一个 registry 会在合成器侧
/// 按重取次数累积资源，还多付一次 registry 初始化往返。故 registry/队列/manager
/// 常态持有，只把「当前输出布局」按次重取——重取期临时绑定的 `wl_output` /
/// `zxdg_output_v1` 有析构请求，读数后显式释放，服务端对象数不随 `--at` 次数增长。
#[derive(Debug)]
pub struct Probe {
    globals: GlobalList,
    queue: EventQueue<GeometryState>,
    manager: ZxdgOutputManagerV1,
    state: GeometryState,
}

impl Probe {
    /// 建立常驻探测态（本连接的 registry 快照 + 输出管理器）。
    ///
    /// `None` = 本连接给不出可信几何：registry 初始化失败，或合成器未通告
    /// `zxdg_output_manager_v1`（拿不到逻辑几何，换算无从谈起）。调用方可稍后
    /// 重试——非 KWin 会话、旧合成器恒 `None`，uinput 候选据此声明能力缺口。
    pub fn new(connection: &Connection) -> Option<Self> {
        let (globals, queue) = registry_queue_init::<GeometryState>(connection).ok()?;
        let manager = globals
            .bind::<ZxdgOutputManagerV1, _, _>(&queue.handle(), 1..=XDG_OUTPUT_MAX_VERSION, ())
            .ok()?;
        Some(Self {
            globals,
            queue,
            manager,
            state: GeometryState::default(),
        })
    }

    /// 重取桌面逻辑包围盒（输出布局的并集，原点 `(0,0)`）。
    ///
    /// 返回 `None` 的情形都是「无法给出可信几何」：无输出、某输出未公布逻辑几何
    /// （合成器不支持 xdg-output）、尺寸为零、包围盒原点非零。
    pub fn refresh(&mut self) -> Option<Rect> {
        let queue_handle = self.queue.handle();
        // 先派发挂起事件：registry 的 `global`/`global_remove` 让快照反映热插拔
        // 与布局变化（新输出不在快照里，就谈不上「重取当前布局」），顺带把上一次
        // 重取发出的析构请求冲刷出去。
        self.queue.roundtrip(&mut self.state).ok()?;

        // 逐个 wl_output global 绑定（多实例 global，GlobalList::bind 只取首个）；
        // 上一轮的输出对象已析构，本轮的逻辑几何按对象 id 重新收集。
        self.state.outputs.clear();
        let outputs: Vec<(WlOutput, u32)> = self
            .globals
            .contents()
            .clone_list()
            .iter()
            .filter(|g| g.interface == WlOutput::interface().name)
            .map(|g| {
                let version = g.version.min(WL_OUTPUT_MAX_VERSION);
                let output = self.globals.registry().bind::<WlOutput, _, _>(
                    g.name,
                    version,
                    &queue_handle,
                    (),
                );
                (output, version)
            })
            .collect();
        if outputs.is_empty() {
            return None;
        }
        let xdg_outputs: Vec<ZxdgOutputV1> = outputs
            .iter()
            .map(|(output, _)| self.manager.get_xdg_output(output, &queue_handle, ()))
            .collect();

        // 绑定请求随本进程发往合成器，logical_* 事件在本往返内到达（KWin 在
        // xdg-output 资源绑定时就发送，不依赖 wl_output.done）。
        let geometry = match self.queue.roundtrip(&mut self.state) {
            Ok(_) => desktop_box(&self.state),
            Err(_) => None,
        };

        // 显式析构本轮临时对象：不发析构请求时 KWin 侧对象数与 `--at` 次数同阶
        // 增长（`zxdg_output_v1` 有 `destroy`、`wl_output` 自 v3 起有 `release`）。
        for xdg in &xdg_outputs {
            xdg.destroy();
        }
        for (output, version) in &outputs {
            // 低于 v3 的 `wl_output` 没有 release 请求，只能随连接关闭回收。
            if *version >= 3 {
                output.release();
            }
        }
        // 析构请求没有回执，flush 即可发出（下一次重取的往返也会捎带它）。
        let _ = self.queue.flush();
        geometry
    }
}

/// 各输出逻辑几何 → 桌面包围盒（纯逻辑，脱离协议往返供测试覆盖）。
fn desktop_box(state: &GeometryState) -> Option<Rect> {
    let mut min = (i32::MAX, i32::MAX);
    let mut max = (i32::MIN, i32::MIN);
    for geometry in state.outputs.values() {
        let (x, y) = geometry.position?;
        let (width, height) = geometry.size?;
        if width <= 0 || height <= 0 {
            return None;
        }
        min = (min.0.min(x), min.1.min(y));
        max = (max.0.max(x + width), max.1.max(y + height));
    }
    if min.0 == i32::MAX {
        return None;
    }
    if min != (0, 0) {
        return None;
    }
    Some(Rect {
        x: 0,
        y: 0,
        width: max.0,
        height: max.1,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    /// 测试输入：`(对象 id, logical_position, logical_size)`。
    type OutputEntry = (u32, Option<(i32, i32)>, Option<(i32, i32)>);

    fn state(entries: &[OutputEntry]) -> GeometryState {
        let mut state = GeometryState::default();
        for (id, position, size) in entries {
            state.outputs.insert(
                *id,
                OutputGeometry {
                    position: *position,
                    size: *size,
                },
            );
        }
        state
    }

    #[test]
    fn single_output_yields_its_logical_size() {
        let s = state(&[(1, Some((0, 0)), Some((1920, 1080)))]);
        assert_eq!(
            desktop_box(&s),
            Some(Rect {
                x: 0,
                y: 0,
                width: 1920,
                height: 1080
            })
        );
    }

    #[test]
    fn neighbouring_outputs_unite_into_desktop_box() {
        // 双屏左右拼接：桌面包围盒是并集（绝对坐标必须覆盖两块屏）。
        let s = state(&[
            (1, Some((0, 0)), Some((1920, 1080))),
            (2, Some((1920, 0)), Some((2560, 1440))),
        ]);
        assert_eq!(
            desktop_box(&s),
            Some(Rect {
                x: 0,
                y: 0,
                width: 4480,
                height: 1440
            })
        );
    }

    #[test]
    fn missing_or_degenerate_geometry_is_not_usable() {
        // 缺逻辑几何、零尺寸、无输出：都无法给出可信换算，必须返回 None 而不是
        // 猜一个尺寸（猜错即「看起来成功、实际点错位置」）。
        assert_eq!(desktop_box(&state(&[])), None);
        assert_eq!(desktop_box(&state(&[(1, None, Some((1920, 1080)))])), None);
        assert_eq!(desktop_box(&state(&[(1, Some((0, 0)), None)])), None);
        assert_eq!(
            desktop_box(&state(&[(1, Some((0, 0)), Some((0, 1080)))])),
            None
        );
    }

    #[test]
    fn non_zero_origin_layout_is_rejected() {
        // 负原点布局（输出排在主屏左侧）：KWin 折算按 size 而非全局坐标，
        // 与 --at 的全局坐标不一致——拒绝，由候选声明能力缺口并归因。
        let s = state(&[(1, Some((-1920, 0)), Some((1920, 1080)))]);
        assert_eq!(desktop_box(&s), None);
    }
}
