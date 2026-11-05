//! 私有 bus 上的 AT-SPI 元素树夹具（无障碍元素动作用例用）。
//!
//! [`crate::FakeA11y`] 替换整个无障碍组件，只能断言「定位 → 动作」的派发；
//! 元素动作的**降级分支**（无 Action 接口 → 几何坐标点击）发生在真实
//! `AtSpiComponent` 内部，必须让它走真实 D-Bus 调用才能被覆盖。本夹具在私有
//! dbus-daemon 上注册最小可用的 AT-SPI 对象（Registry 根、应用、窗口、元素），
//! 由被测 daemon 的桥接层按生产路径读取。

use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::Arc;

use zbus::zvariant::OwnedObjectPath;

use crate::bus::{claim, PrivateBus, ATSPI_REGISTRY_SERVICE, ATSPI_ROOT_PATH};

/// 无障碍节点：`Accessible` 接口的最小实现（子引用 + 名称/角色/状态）。
pub struct MockAccessible {
    name: String,
    role: u32,
    role_name: String,
    states: (u32, u32),
    children: Vec<(String, String)>,
}

impl MockAccessible {
    /// 构造节点：子节点以 `(bus_name, path)` 引用（AT-SPI 引用含总线名）。
    pub fn new(
        name: &str,
        role: u32,
        role_name: &str,
        states: (u32, u32),
        children: Vec<(String, String)>,
    ) -> Self {
        Self {
            name: name.into(),
            role,
            role_name: role_name.into(),
            states,
            children,
        }
    }
}

#[zbus::interface(name = "org.a11y.atspi.Accessible")]
impl MockAccessible {
    #[zbus(property)]
    fn name(&self) -> String {
        self.name.clone()
    }

    #[zbus(property)]
    fn child_count(&self) -> i32 {
        self.children.len() as i32
    }

    fn get_child_at_index(&self, index: i32) -> zbus::fdo::Result<(String, OwnedObjectPath)> {
        let (bus, path) = self
            .children
            .get(index as usize)
            .ok_or_else(|| zbus::fdo::Error::InvalidArgs(format!("no child at index {index}")))?;
        let path = OwnedObjectPath::try_from(path.clone())
            .map_err(|e| zbus::fdo::Error::Failed(format!("bad child path: {e}")))?;
        Ok((bus.clone(), path))
    }

    fn get_role(&self) -> u32 {
        self.role
    }

    fn get_role_name(&self) -> String {
        self.role_name.clone()
    }

    fn get_state(&self) -> Vec<u32> {
        vec![self.states.0, self.states.1]
    }
}

/// `Component` 接口：只实现屏幕坐标包围盒（坐标降级的几何来源）。
pub struct MockComponent {
    extents: (i32, i32, i32, i32),
}

impl MockComponent {
    /// 屏幕坐标包围盒 `(x, y, width, height)`。
    pub fn new(extents: (i32, i32, i32, i32)) -> Self {
        Self { extents }
    }
}

#[zbus::interface(name = "org.a11y.atspi.Component")]
impl MockComponent {
    fn get_extents(&self, _coord_type: u32) -> (i32, i32, i32, i32) {
        self.extents
    }
}

/// `Action` 接口：记录 `DoAction` 调用次数，供「有动作接口的元素不降级」断言；
/// `DoAction(0)` 的返回值可配置，用来覆盖「有动作接口但主动作执行失败」分支。
#[derive(Clone)]
pub struct MockAction {
    calls: Arc<AtomicUsize>,
    result: bool,
}

impl Default for MockAction {
    fn default() -> Self {
        Self {
            calls: Arc::new(AtomicUsize::new(0)),
            result: true,
        }
    }
}

impl MockAction {
    /// 主动作数量恒为 1（GTK "Click" / Qt "press"），`DoAction(0)` 返回成功。
    pub fn new() -> Self {
        Self::default()
    }

    /// 覆盖 `DoAction(0)` 的返回值：`false` = 元素有动作接口但主动作执行失败。
    pub fn with_result(mut self, result: bool) -> Self {
        self.result = result;
        self
    }

    /// 已收到的 `DoAction` 次数。
    pub fn calls(&self) -> usize {
        self.calls.load(Ordering::Relaxed)
    }
}

#[zbus::interface(name = "org.a11y.atspi.Action")]
impl MockAction {
    #[zbus(property)]
    fn n_actions(&self) -> i32 {
        1
    }

    fn do_action(&self, index: i32) -> bool {
        if index == 0 {
            self.calls.fetch_add(1, Ordering::Relaxed);
            return self.result;
        }
        false
    }
}

/// `Application` 接口标记：桥接层用它把根的子节点识别为应用而非 Registry 自身。
#[derive(Default)]
pub struct MockApplication;

#[zbus::interface(name = "org.a11y.atspi.Application")]
impl MockApplication {
    #[zbus(property)]
    fn atspi_version(&self) -> String {
        "2.1".into()
    }
}

/// 元素树节点描述（应用连接内的一个对象）。
pub struct NodeSpec {
    /// 对象路径（`a11y query` 结果的 path 字段）。
    pub path: &'static str,
    /// 无障碍名称（`a11y query --name` 的匹配面）。
    pub name: &'static str,
    /// AT-SPI 角色编码（23 = FRAME，43 = push button）。
    pub role: u32,
    /// 角色名（语义定位按名称匹配）。
    pub role_name: &'static str,
    /// 子节点路径（同一应用连接内）。
    pub children: &'static [&'static str],
    /// `Component.GetExtents` 的屏幕包围盒；`None` = 不实现 Component 接口。
    pub extents: Option<(i32, i32, i32, i32)>,
    /// `Action` 接口：`Some(返回值)` = 实现该接口且 `DoAction(0)` 返回该值
    /// （`false` 覆盖「有动作接口但主动作执行失败」），`None` = 不实现。
    pub action: Option<bool>,
    /// 是否是应用根节点（暴露 `org.a11y.atspi.Application`）。
    pub is_application: bool,
}

/// 应用连接：在一条新连接上注册整棵元素树。
///
/// 返回 `(连接, 总线名)`——总线名是 `GetChildAtIndex` 引用的目标，用例据此
/// 把查询结果与动作请求串起来（同一标识空间）。
pub async fn serve_application(
    bus: &PrivateBus,
    nodes: &[NodeSpec],
) -> (zbus::Connection, String, Vec<MockAction>) {
    let conn = bus.connect().await;
    let bus_name = conn
        .unique_name()
        .expect("fixture connection has a unique name")
        .to_string();
    let mut actions = Vec::new();
    for node in nodes {
        let children = node
            .children
            .iter()
            .map(|path| (bus_name.clone(), (*path).to_string()))
            .collect();
        let accessible =
            MockAccessible::new(node.name, node.role, node.role_name, (0, 0), children);
        conn.object_server()
            .at(node.path, accessible)
            .await
            .expect("register Accessible interface");
        if let Some(extents) = node.extents {
            conn.object_server()
                .at(node.path, MockComponent::new(extents))
                .await
                .expect("register Component interface");
        }
        if let Some(result) = node.action {
            let action = MockAction::new().with_result(result);
            conn.object_server()
                .at(node.path, action.clone())
                .await
                .expect("register Action interface");
            actions.push(action);
        }
        if node.is_application {
            conn.object_server()
                .at(node.path, MockApplication)
                .await
                .expect("register Application interface");
        }
    }
    (conn, bus_name, actions)
}

/// Registry 根：桌面根对象的唯一子节点是应用根。
pub async fn serve_registry(bus: &PrivateBus, app_bus: &str, app_path: &str) -> zbus::Connection {
    let root = MockAccessible::new(
        "",
        0,
        "",
        (0, 0),
        vec![(app_bus.to_string(), app_path.to_string())],
    );
    let conn = bus.serve(ATSPI_ROOT_PATH, root).await;
    claim(&conn, ATSPI_REGISTRY_SERVICE).await;
    conn
}
