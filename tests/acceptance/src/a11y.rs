//! 无障碍夹具：确定性元素树、语义定位与动作记录。
//!
//! 真实 AT-SPI 树需要桌面应用注册到 a11y bus，CI 无法复现；本夹具替换
//! `Daemon.a11y`，请求仍走 `a11y.query` / `a11y.action` 的生产校验与投影。

use std::sync::{Arc, Mutex};

use agent_shell_a11y::{A11yOps, AtspiRole, AtspiState, ElementNode};
use agent_shell_core::error::{AgentShellError, Result};
use agent_shell_core::types::SemanticTarget;
use async_trait::async_trait;

/// 元素动作记录：`(bus, path)` 的共享日志。
type ClickLog = Arc<Mutex<Vec<(Option<String>, String)>>>;

/// 可克隆的 a11y 夹具句柄。
#[derive(Clone, Default)]
pub struct FakeA11y {
    nodes: Arc<Mutex<Vec<ElementNode>>>,
    clicks: ClickLog,
    available: bool,
}

impl FakeA11y {
    /// 以给定元素构造可用组件。
    pub fn new(nodes: Vec<ElementNode>) -> Self {
        Self {
            nodes: Arc::new(Mutex::new(nodes)),
            clicks: Arc::new(Mutex::new(Vec::new())),
            available: true,
        }
    }

    /// 模拟无障碍总线不可达。
    pub fn unavailable() -> Self {
        Self {
            available: false,
            ..Self::default()
        }
    }

    /// 已触发的元素动作（`(bus, path)`）。
    pub fn clicks(&self) -> Vec<(Option<String>, String)> {
        self.clicks.lock().expect("a11y click log").clone()
    }

    /// 当前元素集。
    pub fn nodes(&self) -> Vec<ElementNode> {
        self.nodes.lock().expect("a11y nodes").clone()
    }
}

/// 元素构造：角色名与编码同时给出，状态集合按位。
pub fn element(
    bus_name: &str,
    path: &str,
    name: &str,
    role: &str,
    role_code: u32,
    states: u64,
) -> ElementNode {
    ElementNode {
        bus_name: bus_name.into(),
        path: path.into(),
        name: name.into(),
        role: AtspiRole {
            code: role_code,
            name: role.into(),
        },
        states: AtspiState(states),
    }
}

#[async_trait]
impl A11yOps for FakeA11y {
    async fn locate(&self, target: &SemanticTarget) -> Result<Vec<ElementNode>> {
        if !self.available {
            return Err(AgentShellError::BackendUnavailable(
                "AT-SPI bus unreachable (fixture)".into(),
            ));
        }
        let SemanticTarget::ByAccessibility { role, name, .. } = target else {
            return Err(AgentShellError::NotImplemented(format!(
                "fixture locator: unsupported target {target:?}"
            )));
        };
        let nodes = self.nodes.lock().expect("a11y nodes");
        Ok(nodes
            .iter()
            .filter(|node| {
                let role_matches = role
                    .as_deref()
                    .is_none_or(|wanted| node.role.matches_name(wanted));
                let name_matches = name.as_deref().is_none_or(|wanted| node.name == wanted);
                role_matches && name_matches
            })
            .cloned()
            .collect())
    }

    async fn locate_by_path(&self, bus: Option<&str>, path: &str) -> Result<ElementNode> {
        if !self.available {
            return Err(AgentShellError::BackendUnavailable(
                "AT-SPI bus unreachable (fixture)".into(),
            ));
        }
        let nodes = self.nodes.lock().expect("a11y nodes");
        let matches: Vec<&ElementNode> = nodes
            .iter()
            .filter(|node| node.path == path && bus.is_none_or(|b| node.bus_name == b))
            .collect();
        match matches.as_slice() {
            [node] => Ok((*node).clone()),
            [] => Err(AgentShellError::WindowNotFound(format!(
                "a11y path {path} not found"
            ))),
            many => Err(AgentShellError::WindowNotFound(format!(
                "a11y path {path} is ambiguous across bus names {:?}; pass --bus to disambiguate",
                many.iter().map(|n| &n.bus_name).collect::<Vec<_>>()
            ))),
        }
    }

    async fn click(&self, element: &ElementNode) -> Result<()> {
        if !self.available {
            return Err(AgentShellError::BackendUnavailable(
                "AT-SPI bus unreachable (fixture)".into(),
            ));
        }
        self.clicks
            .lock()
            .expect("a11y click log")
            .push((Some(element.bus_name.clone()), element.path.clone()));
        Ok(())
    }
}
