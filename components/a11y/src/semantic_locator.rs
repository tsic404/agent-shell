//! 语义定位引擎（设计文档 §14.3 `a11y::semantic_locator`）。
//!
//! 本期实现 `SemanticTarget::ByAccessibility { role, name, parent_role,
//! parent_name }` 策略：有父约束时先在各窗口内定位父元素、再在其子树内
//! 搜目标；无父约束则全窗口搜索。其它策略返回
//! [`AgentShellError::NotImplemented`]（结构上保留扩展位）。

use crate::atspi_bridge::{AtspiBridge, MAX_TRAVERSE_DEPTH};
use crate::tree::{ElementNode, WindowNode};
use agent_shell_core::error::{AgentShellError, Result};
use agent_shell_core::types::SemanticTarget;
use async_trait::async_trait;
use std::future::Future;

/// 名称匹配谓词：`None` 表示不约束。
type NameFilter = Option<String>;

/// 全树遍历的结果条数上限：`--all` 在超大树上不会撑爆协议载荷，截断而非失败。
const MAX_SEARCH_RESULTS: usize = 10_000;

/// 语义定位的树读取依赖：生产走 [`AtspiBridge`]，测试注入假实现。
#[async_trait]
pub trait TreeSource: Send + Sync {
    /// 枚举全部窗口（语义定位的搜索空间）。
    async fn all_windows(&self) -> Result<Vec<WindowNode>>;
    /// 把窗口提升为可搜索的元素节点视图。
    async fn window_as_element(&self, window: &WindowNode) -> Result<ElementNode>;
    /// 枚举节点的直接子元素。
    async fn children(&self, node: &ElementNode) -> Result<Vec<ElementNode>>;
}

#[async_trait]
impl TreeSource for AtspiBridge {
    async fn all_windows(&self) -> Result<Vec<WindowNode>> {
        AtspiBridge::all_windows(self).await
    }

    async fn window_as_element(&self, window: &WindowNode) -> Result<ElementNode> {
        AtspiBridge::window_as_element(self, window).await
    }

    async fn children(&self, node: &ElementNode) -> Result<Vec<ElementNode>> {
        AtspiBridge::children(self, node).await
    }
}

/// 语义定位引擎。持有树读取依赖（生产为 [`AtspiBridge`]）；无独立状态。
pub struct SemanticLocator<B = AtspiBridge> {
    bridge: B,
}

impl<B: TreeSource> SemanticLocator<B> {
    pub fn new(bridge: B) -> Self {
        Self { bridge }
    }

    pub fn bridge(&self) -> &B {
        &self.bridge
    }

    /// 按语义描述定位元素（设计 §14.3 `locate()`）。
    pub async fn locate(&self, target: &SemanticTarget) -> Result<Vec<ElementNode>> {
        match target {
            SemanticTarget::ByAccessibility {
                role,
                name,
                parent_role,
                parent_name,
            } => {
                let mut results = Vec::new();
                'windows: for window in self.bridge.all_windows().await? {
                    let window_el = self.bridge.window_as_element(&window).await?;
                    match (parent_role, parent_name) {
                        (Some(prole), Some(pname)) => {
                            // 有父约束：先找父，再在父子树内搜目标
                            let parents =
                                self.find_elements(&window_el, Some(prole), Some(pname.clone()));
                            for parent in parents.await {
                                let found = self
                                    .find_elements(&parent, role.as_deref(), name.clone())
                                    .await;
                                results.extend(
                                    found
                                        .into_iter()
                                        .take(MAX_SEARCH_RESULTS.saturating_sub(results.len())),
                                );
                                if results.len() >= MAX_SEARCH_RESULTS {
                                    break 'windows;
                                }
                            }
                        }
                        _ => {
                            // 无父约束：全窗口搜索
                            let found = self
                                .find_elements(&window_el, role.as_deref(), name.clone())
                                .await;
                            results.extend(
                                found
                                    .into_iter()
                                    .take(MAX_SEARCH_RESULTS.saturating_sub(results.len())),
                            );
                            if results.len() >= MAX_SEARCH_RESULTS {
                                break;
                            }
                        }
                    }
                }
                Ok(results)
            }
            other => Err(AgentShellError::NotImplemented(format!(
                "locator strategy: {other:?}"
            ))),
        }
    }

    /// 在子树内按 (role, name) 过滤搜索（DFS + 深度/条数保护）。
    ///
    /// `role`/`name` 均可选；两者同时为 `None` 时遍历全树（通配查询，
    /// 由 [`MAX_SEARCH_RESULTS`] 与 [`MAX_TRAVERSE_DEPTH`] 约束）。
    pub fn find_elements(
        &self,
        root: &ElementNode,
        role: Option<&str>,
        name: NameFilter,
    ) -> impl Future<Output = Vec<ElementNode>> + Send + '_ {
        let root = root.clone();
        let role = role.map(str::to_string);
        async move {
            let mut out = Vec::new();
            self.search(&root, role.clone(), name.clone(), &mut out, 0)
                .await;
            out
        }
    }

    /// 递归 DFS。命中即收集（多结果）；深度/条数超限截断。
    async fn search(
        &self,
        node: &ElementNode,
        role: Option<String>,
        name: Option<String>,
        out: &mut Vec<ElementNode>,
        depth: u8,
    ) {
        if depth > MAX_TRAVERSE_DEPTH {
            tracing::warn!(
                path = %node.path,
                depth,
                "a11y tree traversal depth limit reached; truncating"
            );
            return;
        }
        if out.len() >= MAX_SEARCH_RESULTS {
            return;
        }
        // name 为精确匹配（审查定案）：语义目标给出的是控件可访问名
        // 全文；子串容忍会误命中前缀重叠的相邻控件（如 "保存" 命中
        // "保存并关闭"）。
        let matches = role.as_deref().is_none_or(|r| node.role.matches_name(r))
            && name.as_deref().is_none_or(|n| node.name == n);
        if matches {
            out.push(node.clone());
        }
        let children = match self.bridge.children(node).await {
            Ok(c) => c,
            Err(e) => {
                tracing::debug!(path = %node.path, error = %e, "children fetch failed");
                return;
            }
        };
        for child in &children {
            Box::pin(self.search(child, role.clone(), name.clone(), out, depth + 1)).await;
            if out.len() >= MAX_SEARCH_RESULTS {
                break;
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::tree::{AtspiRole, AtspiState, WindowNode};
    use async_trait::async_trait;

    fn element_at(path: &str, role_name: &str, name: &str) -> ElementNode {
        ElementNode {
            bus_name: ":1.0".into(),
            path: path.into(),
            name: name.into(),
            role: AtspiRole {
                code: 43,
                name: role_name.into(),
            },
            states: AtspiState(0),
        }
    }

    /// 假树源：以 path 为键的静态子树，供遍历测试注入。
    struct FakeTree {
        children: std::collections::HashMap<String, Vec<ElementNode>>,
    }

    #[async_trait]
    impl TreeSource for FakeTree {
        async fn all_windows(&self) -> Result<Vec<WindowNode>> {
            Ok(vec![])
        }

        async fn window_as_element(&self, _window: &WindowNode) -> Result<ElementNode> {
            Err(AgentShellError::NotImplemented("fake tree".into()))
        }

        async fn children(&self, node: &ElementNode) -> Result<Vec<ElementNode>> {
            Ok(self.children.get(&node.path).cloned().unwrap_or_default())
        }
    }

    /// 多窗口假树源：验证跨窗口累加的全局条数上限。
    struct FakeWindows {
        windows: Vec<WindowNode>,
        roots: std::collections::HashMap<String, ElementNode>,
        children: std::collections::HashMap<String, Vec<ElementNode>>,
    }

    #[async_trait]
    impl TreeSource for FakeWindows {
        async fn all_windows(&self) -> Result<Vec<WindowNode>> {
            Ok(self.windows.clone())
        }

        async fn window_as_element(&self, window: &WindowNode) -> Result<ElementNode> {
            self.roots
                .get(&window.path)
                .cloned()
                .ok_or_else(|| AgentShellError::WindowNotFound(window.path.clone()))
        }

        async fn children(&self, node: &ElementNode) -> Result<Vec<ElementNode>> {
            Ok(self.children.get(&node.path).cloned().unwrap_or_default())
        }
    }

    fn fake_locator() -> SemanticLocator<FakeTree> {
        let mut children = std::collections::HashMap::new();
        children.insert(
            "/root".to_string(),
            vec![
                element_at("/root/btn1", "push button", "OK"),
                element_at("/root/btn2", "push button", "Cancel"),
                element_at("/root/panel", "panel", "pane"),
            ],
        );
        children.insert(
            "/root/panel".to_string(),
            vec![element_at("/root/panel/text", "text", "hello")],
        );
        SemanticLocator::new(FakeTree { children })
    }

    #[test]
    fn role_matching_normalizes_separators() {
        let role = element_at("/x", "push button", "").role;
        assert!(role.matches_name("push button"));
        assert!(role.matches_name("Push Button"));
        assert!(role.matches_name("push_button"));
        assert!(role.matches_name("pushbutton"));
        assert!(!role.matches_name("text"));
    }

    #[tokio::test]
    async fn find_elements_without_filter_enumerates_full_tree() {
        // TSI-2525：--all 依赖 (None, None) 真正遍历全树，而非短路返回空。
        let locator = fake_locator();
        let root = element_at("/root", "frame", "win");
        let found = locator.find_elements(&root, None, None).await;
        let paths: Vec<&str> = found.iter().map(|n| n.path.as_str()).collect();
        assert_eq!(
            paths,
            vec![
                "/root",
                "/root/btn1",
                "/root/btn2",
                "/root/panel",
                "/root/panel/text"
            ],
            "full-tree enumeration must return root and all descendants"
        );
    }

    #[tokio::test]
    async fn find_elements_with_role_filter_keeps_filtering() {
        // all=true 不再覆盖 role/name：daemon 原样透传，role 过滤仍生效。
        let locator = fake_locator();
        let root = element_at("/root", "frame", "win");
        let found = locator
            .find_elements(&root, Some("push button"), None)
            .await;
        let paths: Vec<&str> = found.iter().map(|n| n.path.as_str()).collect();
        assert_eq!(
            paths,
            vec!["/root/btn1", "/root/btn2"],
            "role filter must still apply: {found:?}"
        );
    }

    #[tokio::test]
    async fn locate_caps_results_globally_across_windows() {
        // TSI-2525：MAX_SEARCH_RESULTS 是整次查询硬上限，跨窗口累加不可 N×。
        let mut windows = Vec::new();
        let mut roots = std::collections::HashMap::new();
        let mut children = std::collections::HashMap::new();
        for w in 0..2 {
            let wpath = format!("/win{w}");
            windows.push(WindowNode {
                bus_name: ":1.0".into(),
                path: wpath.clone(),
                name: format!("win{w}"),
                states: AtspiState(0),
            });
            roots.insert(wpath.clone(), element_at(&wpath, "frame", "win"));
            let leaves: Vec<ElementNode> = (0..5999)
                .map(|i| element_at(&format!("{wpath}/c{i}"), "push button", "leaf"))
                .collect();
            children.insert(wpath, leaves);
        }
        let locator = SemanticLocator::new(FakeWindows {
            windows,
            roots,
            children,
        });
        let found = locator
            .locate(&SemanticTarget::ByAccessibility {
                role: None,
                name: None,
                parent_role: None,
                parent_name: None,
            })
            .await
            .expect("locate");
        assert_eq!(
            found.len(),
            MAX_SEARCH_RESULTS,
            "global cap must bound cross-window accumulation"
        );
    }
}
