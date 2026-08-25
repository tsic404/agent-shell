//! 元素操作封装（设计文档 §14.4 `a11y::action`）。
//!
//! click 优先 AT-SPI Action 接口（`DoAction(0)`，实测 GTK/Qt 的动作名
//! 均为 "Click"/"press"）；元素无 Action 能力时降级为中心坐标点击，
//! 经 [`PointerInput`] trait 注入——与 TSI-2315 输入子系统之间以 trait
//! 依赖解耦（设计 §14.4「以 trait 依赖而非硬编码实例」）。

use crate::atspi_bridge::AtspiBridge;
use crate::tree::{AtspiState, ElementNode};
use agent_shell_core::error::{AgentShellError, Result};
use agent_shell_core::types::MouseButton;
use async_trait::async_trait;

/// 指针注入抽象（TSI-2315 InputDispatcher 实现此 trait 后接入）。
#[async_trait]
pub trait PointerInput: Send + Sync {
    /// 移动指针到屏幕绝对坐标。
    async fn mouse_move(&self, x: i32, y: i32) -> Result<()>;
    /// 在当前位置点击指定按键。
    async fn mouse_click(&self, button: MouseButton) -> Result<()>;
}

/// 无输入后端的占位实现：坐标降级路径显式报错而非静默丢弃。
///
/// 输入子系统（TSI-2315）装配完成后由真实 dispatcher 替换。
pub struct NoPointerInput;

#[async_trait]
impl PointerInput for NoPointerInput {
    async fn mouse_move(&self, _x: i32, _y: i32) -> Result<()> {
        Err(AgentShellError::NotImplemented(
            "pointer input backend not assembled (TSI-2315)".into(),
        ))
    }

    async fn mouse_click(&self, _button: MouseButton) -> Result<()> {
        Err(AgentShellError::NotImplemented(
            "pointer input backend not assembled (TSI-2315)".into(),
        ))
    }
}

/// 元素操作封装。
pub struct ElementActions {
    bridge: AtspiBridge,
    input: std::sync::Arc<dyn PointerInput>,
}

impl ElementActions {
    pub fn new(bridge: AtspiBridge) -> Self {
        Self {
            bridge,
            input: std::sync::Arc::new(NoPointerInput),
        }
    }

    pub fn with_input(mut self, input: std::sync::Arc<dyn PointerInput>) -> Self {
        self.input = input;
        self
    }

    pub fn bridge(&self) -> &AtspiBridge {
        &self.bridge
    }

    // ───────────────────── 接口能力探测 ─────────────────────

    /// 元素是否实现 AT-SPI Action 接口。
    pub async fn has_action_interface(&self, element: &ElementNode) -> bool {
        self.bridge
            .has_interface_public(&element.bus_name, &element.path, "org.a11y.atspi.Action")
            .await
    }

    /// 元素是否实现 Text 接口。
    pub async fn has_text_interface(&self, element: &ElementNode) -> bool {
        self.bridge
            .has_interface_public(&element.bus_name, &element.path, "org.a11y.atspi.Text")
            .await
    }

    /// 元素是否实现 EditableText 接口且处于可编辑状态。
    pub async fn is_editable(&self, element: &ElementNode) -> bool {
        if !element.states.contains(AtspiState::EDITABLE) {
            return false;
        }
        self.bridge
            .has_interface_public(
                &element.bus_name,
                &element.path,
                "org.a11y.atspi.EditableText",
            )
            .await
    }

    // ───────────────────── 设计 §14.4 四操作 ─────────────────────

    /// 点击元素：优先 AT-SPI Action 接口，降级中心坐标点击。
    pub async fn click(&self, element: &ElementNode) -> Result<()> {
        if self.has_action_interface(element).await {
            let p = self
                .bridge
                .proxy_for(
                    element.bus_name.as_str(),
                    element.path.as_str(),
                    "org.a11y.atspi.Action",
                )
                .await?;
            let n_actions: i32 = p
                .get_property("NActions")
                .await
                .map_err(|e| AgentShellError::DBus(format!("Action.NActions: {e}")))?;
            if n_actions > 0 {
                let ok: bool = AtspiBridge::call_checked(
                    &p,
                    "DoAction",
                    &(0,), // index 0 = 主动作（GTK "Click" / Qt "press"）
                    "Action.DoAction",
                )
                .await?;
                if ok {
                    return Ok(());
                }
                tracing::warn!(
                    path = %element.path,
                    "DoAction returned false; falling back to coordinate click"
                );
            }
        }
        // 降级：中心坐标 → 移动 + 左键点击
        let rect = self
            .bridge
            .get_extents(&element.bus_name, &element.path)
            .await?;
        let (cx, cy) = ElementNode::center_of(rect);
        self.input.mouse_move(cx, cy).await?;
        self.input.mouse_click(MouseButton::Left).await
    }

    /// 聚焦元素：AT-SPI Component.GrabFocus；失败降级 Action.DoAction(0)。
    pub async fn focus(&self, element: &ElementNode) -> Result<()> {
        let component = self
            .bridge
            .proxy_for(
                element.bus_name.as_str(),
                element.path.as_str(),
                "org.a11y.atspi.Component",
            )
            .await?;
        let ok: bool =
            AtspiBridge::call_checked(&component, "GrabFocus", &(), "Component.GrabFocus").await?;
        if ok {
            return Ok(());
        }
        // 降级：有 Action 的元素触发主动作通常等效聚焦
        if self.has_action_interface(element).await {
            let action = self
                .bridge
                .proxy_for(
                    element.bus_name.as_str(),
                    element.path.as_str(),
                    "org.a11y.atspi.Action",
                )
                .await?;
            return AtspiBridge::call_checked(
                &action,
                "DoAction",
                &(0,),
                "Action.DoAction(focus fallback)",
            )
            .await
            .and_then(|ok| {
                if ok {
                    Ok(())
                } else {
                    Err(AgentShellError::DBus("focus fallback failed".into()))
                }
            });
        }
        Err(AgentShellError::DBus(format!(
            "GrabFocus unsupported for {}",
            element.path
        )))
    }

    /// 获取元素文本：`GetText(0, i32::MAX)` 取全文（设计 §14.4 用 -1 表
    /// 全文，但 Qt atspi 实现按字面区间处理负值会出错——实测以
    /// CharacterCount 上界替代）。
    pub async fn get_text(&self, element: &ElementNode) -> Result<String> {
        let text = self
            .bridge
            .proxy_for(
                element.bus_name.as_str(),
                element.path.as_str(),
                "org.a11y.atspi.Text",
            )
            .await?;
        // 终点偏移：设计 §14.4 原文是 -1（AT-SPI 惯例表「取到结尾」），
        // 但 Qt atspi 实现按字面区间处理负值会返回空/出错（实测），
        // 故改用 CharacterCount 作为终点——语义等价（全文），跨实现安全。
        let count: i32 = text
            .get_property("CharacterCount")
            .await
            .map_err(|e| AgentShellError::DBus(format!("Text.CharacterCount: {e}")))?;
        // 实测 GTK atspi GetText 只返回 s（无 start/end offset 附加值）
        let content: String =
            AtspiBridge::call_checked(&text, "GetText", &(0, count), "Text.GetText").await?;
        Ok(content)
    }

    /// 输入文本：EditableText.SetTextContents 整体覆写。
    pub async fn set_text(&self, element: &ElementNode, contents: &str) -> Result<()> {
        if !self.is_editable(element).await {
            return Err(AgentShellError::BackendUnavailable(format!(
                "element {} is not editable (missing EditableText or EDITABLE state)",
                element.path
            )));
        }
        let editable = self
            .bridge
            .proxy_for(
                element.bus_name.as_str(),
                element.path.as_str(),
                "org.a11y.atspi.EditableText",
            )
            .await?;
        let ok: bool = AtspiBridge::call_checked(
            &editable,
            "SetTextContents",
            &(contents,),
            "EditableText.SetTextContents",
        )
        .await?;
        if ok {
            Ok(())
        } else {
            Err(AgentShellError::DBus(
                "SetTextContents returned false".into(),
            ))
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// NoPointerInput 是显式错误而非静默 no-op——坐标降级路径的
    /// 防御性契约。
    #[tokio::test]
    async fn placeholder_input_errors_instead_of_noop() {
        let input: std::sync::Arc<dyn PointerInput> = std::sync::Arc::new(NoPointerInput);
        let move_err = input.mouse_move(10, 10).await.unwrap_err();
        assert!(matches!(move_err, AgentShellError::NotImplemented(_)));
        let click_err = input.mouse_click(MouseButton::Left).await.unwrap_err();
        assert!(matches!(click_err, AgentShellError::NotImplemented(_)));
    }

    #[test]
    fn element_center_uses_rect_api() {
        use crate::tree::ElementNode;
        let rect = agent_shell_core::types::Rect {
            x: 100,
            y: 50,
            width: 40,
            height: 20,
        };
        let (cx, cy) = ElementNode::center_of(rect);
        assert_eq!((cx, cy), (120, 60));
    }
}
