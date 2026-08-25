//! 语义定位引擎（设计文档 §14.3 `a11y::semantic_locator`）。
//!
//! 本期实现 `SemanticTarget::ByAccessibility { role, name, parent_role,
//! parent_name }` 策略：有父约束时先在各窗口内定位父元素、再在其子树内
//! 搜目标；无父约束则全窗口搜索。其它策略返回
//! [`AgentShellError::NotImplemented`]（结构上保留扩展位）。

use crate::atspi_bridge::{AtspiBridge, MAX_TRAVERSE_DEPTH};
use crate::tree::ElementNode;
use agent_shell_core::error::{AgentShellError, Result};
use agent_shell_core::types::SemanticTarget;
use std::future::Future;

/// 名称匹配谓词：`None` 表示不约束。
type NameFilter = Option<String>;
/// 语义定位引擎。持有桥接引用；无独立状态。
pub struct SemanticLocator {
    bridge: AtspiBridge,
}

impl SemanticLocator {
    pub fn new(bridge: AtspiBridge) -> Self {
        Self { bridge }
    }

    pub fn bridge(&self) -> &AtspiBridge {
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
                for window in self.bridge.all_windows().await? {
                    let window_el = self.bridge.window_as_element(&window).await?;
                    match (parent_role, parent_name) {
                        (Some(prole), Some(pname)) => {
                            // 有父约束：先找父，再在父子树内搜目标
                            let parents =
                                self.find_elements(&window_el, Some(prole), Some(pname.clone()));
                            for parent in parents.await {
                                results.extend(
                                    self.find_elements(&parent, role.as_deref(), name.clone())
                                        .await,
                                );
                            }
                        }
                        _ => {
                            // 无父约束：全窗口搜索
                            results.extend(
                                self.find_elements(&window_el, role.as_deref(), name.clone())
                                    .await,
                            );
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

    /// 在子树内按 (role, name) 过滤搜索（DFS + 深度保护）。
    ///
    /// `role`/`name` 均可选；两者同时为 `None` 时返回空（避免全树枚举）。
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
            if role.is_none() && name.is_none() {
                return out;
            }
            self.search(&root, role.clone(), name.clone(), &mut out, 0)
                .await;
            out
        }
    }

    /// 递归 DFS。命中即收集（多结果）；深度超限截断。
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
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::tree::{AtspiRole, AtspiState};

    fn element(role_name: &str, name: &str) -> ElementNode {
        ElementNode {
            bus_name: ":1.0".into(),
            path: "/org/a11y/atspi/accessible/1".into(),
            name: name.into(),
            role: AtspiRole {
                code: 43,
                name: role_name.into(),
            },
            states: AtspiState(0),
        }
    }

    #[test]
    fn role_matching_normalizes_separators() {
        let role = element("push button", "").role;
        assert!(role.matches_name("push button"));
        assert!(role.matches_name("Push Button"));
        assert!(role.matches_name("push_button"));
        assert!(role.matches_name("pushbutton"));
        assert!(!role.matches_name("text"));
    }
}
