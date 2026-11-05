//! 元素操作封装（设计文档 §14.4 `a11y::action`）。
//!
//! click 优先 AT-SPI Action 接口（`DoAction(0)`，实测 GTK/Qt 的动作名
//! 均为 "Click"/"press"）；元素无 Action 能力时降级为中心坐标点击，
//! 经 [`PointerInput`] trait 注入——与输入子系统之间以 trait
//! 依赖解耦（设计 §14.4「以 trait 依赖而非硬编码实例」）。
//!
//! 指针通道由调用方（daemon）按会话装配结果传入：组件自身不持有输入后端，
//! 避免「本会话没有输入链」与「元素没有几何」这两类失败被同一个占位实现
//! 冒充（前者是后端不可用，后者是元素无法定位）。

use crate::atspi_bridge::AtspiBridge;
use crate::tree::{AtspiState, ElementNode};
use agent_shell_core::error::{AgentShellError, Result};
use agent_shell_core::types::{MouseButton, Rect};
use async_trait::async_trait;

/// 指针注入抽象（输入子系统实现此 trait 后接入）。
#[async_trait]
pub trait PointerInput: Send + Sync {
    /// 移动指针到屏幕绝对坐标。
    async fn mouse_move(&self, x: i32, y: i32) -> Result<()>;
    /// 在当前位置点击指定按键。
    async fn mouse_click(&self, button: MouseButton) -> Result<()>;
}

/// 坐标降级的触发原因。两种原因都会走到坐标点击，但失败时的可观察文案
/// 必须分开：把「有动作接口但主动作执行失败」说成「没有动作接口」是与
/// 本类缺陷同一方向的错误归因（C-103）。
#[derive(Clone, Copy, Debug)]
enum FallbackCause {
    /// 元素没有 AT-SPI Action 接口，或 `NActions` 为 0。
    NoActionInterface,
    /// 元素有 Action 接口，但 `DoAction(0)` 返回 `false`。
    ActionRefused,
}

impl FallbackCause {
    /// 失败文案里的原因短语（与「没有可用几何」「指针后端不可用」并列）。
    fn describe(self) -> &'static str {
        match self {
            Self::NoActionInterface => "no AT-SPI action interface",
            Self::ActionRefused => "primary AT-SPI action returned false",
        }
    }
}

/// 元素操作封装。
pub struct ElementActions {
    bridge: AtspiBridge,
}

impl ElementActions {
    pub fn new(bridge: AtspiBridge) -> Self {
        Self { bridge }
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
    ///
    /// 降级要求元素有可用几何：宽高非正的包围盒（未映射/已移出屏幕）会让
    /// 中心点落到 (0,0) 一类无意义坐标，注入后表现为「点到了别处」的静默
    /// 成功——C-074 明确禁止，故此处以「元素没有可用几何」拒绝，与指针通道
    /// 缺失（后端不可用）分属两种原因。
    pub async fn click(&self, element: &ElementNode, pointer: &dyn PointerInput) -> Result<()> {
        let mut cause = FallbackCause::NoActionInterface;
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
                cause = FallbackCause::ActionRefused;
            }
        }
        // 降级：中心坐标 → 移动 + 左键点击
        let rect = self
            .bridge
            .get_extents(&element.bus_name, &element.path)
            .await
            .map_err(|e| match e {
                // 总线不可达是会话级失败，不能被说成「这个元素没有几何」
                AgentShellError::BackendUnavailable(msg) => {
                    AgentShellError::BackendUnavailable(msg)
                }
                other => AgentShellError::NotSupported(format!(
                    "element {} is not clickable by AT-SPI action ({}) and its geometry is \
                     unreadable: {other}",
                    element.path,
                    cause.describe()
                )),
            })?;
        let (cx, cy) = click_point(rect, &element.path, cause)?;
        pointer.mouse_move(cx, cy).await?;
        pointer.mouse_click(MouseButton::Left).await
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

    /// 获取元素文本：`GetText(0, CharacterCount)` 取全文——CharacterCount
    /// 语义等价全文；Qt atspi 对负值区间按字面处理实测不可靠，故不用
    /// -1 作终点。
    pub async fn get_text(&self, element: &ElementNode) -> Result<String> {
        let text = self
            .bridge
            .proxy_for(
                element.bus_name.as_str(),
                element.path.as_str(),
                "org.a11y.atspi.Text",
            )
            .await?;
        // 终点偏移用 CharacterCount 而非 -1：Qt atspi 按字面区间处理负值会出错
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

/// 坐标降级的目标点：空包围盒（宽或高非正）不可点击。
///
/// 空包围盒的元素（未映射、滚动出可视区、toolkit 不实现 Component）在
/// 屏幕上没有可命中的像素；按中心注入会落到 (0,0) 或负坐标，表现为点了
/// 别处的静默成功。降级原因随 `cause` 分别成句——「没有动作接口」与
/// 「主动作执行失败」不是同一种元素能力缺失。
fn click_point(rect: Rect, path: &str, cause: FallbackCause) -> Result<(i32, i32)> {
    if rect.width <= 0 || rect.height <= 0 {
        return Err(AgentShellError::NotSupported(format!(
            "element {path} has no usable geometry ({}x{} at {},{}); it is not clickable by \
             AT-SPI action ({}), so there is nothing to click",
            rect.width,
            rect.height,
            rect.x,
            rect.y,
            cause.describe()
        )));
    }
    Ok(ElementNode::center_of(rect))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn click_point_uses_rect_center() {
        let rect = Rect {
            x: 100,
            y: 50,
            width: 40,
            height: 20,
        };
        assert_eq!(
            click_point(rect, "/x", FallbackCause::NoActionInterface).unwrap(),
            (120, 60)
        );
    }

    /// 空包围盒必须报「元素没有几何」，而不是把 (0,0) 当作可点击目标；
    /// 两种降级原因分别成句，不再一律说成「没有动作接口」。
    #[test]
    fn click_point_rejects_empty_geometry() {
        for cause in [
            FallbackCause::NoActionInterface,
            FallbackCause::ActionRefused,
        ] {
            for (width, height) in [(0, 20), (40, 0), (0, 0), (-1, 20)] {
                let rect = Rect {
                    x: 0,
                    y: 0,
                    width,
                    height,
                };
                let err = click_point(rect, "/org/example/ghost", cause).unwrap_err();
                assert!(
                    matches!(err, AgentShellError::NotSupported(_)),
                    "empty geometry must be a not-supported error, got {err:?}"
                );
                let message = err.to_string();
                assert!(
                    message.contains("/org/example/ghost")
                        && message.contains("no usable geometry"),
                    "message must name the element and the cause: {message}"
                );
                assert!(
                    message.contains(cause.describe()),
                    "message must carry the actual fallback cause ({:?}): {message}",
                    cause
                );
                assert!(
                    !message.contains("pointer input backend"),
                    "element geometry failure must not be blamed on the pointer backend: {message}"
                );
            }
        }
        // 「主动作执行失败」不得被说成「没有动作接口」（同一缺陷的反向归因）。
        let err = click_point(
            Rect {
                x: 0,
                y: 0,
                width: 0,
                height: 0,
            },
            "/org/example/refuse",
            FallbackCause::ActionRefused,
        )
        .unwrap_err();
        assert!(
            !err.to_string().contains("no AT-SPI action interface"),
            "a refused primary action must not be reported as a missing action interface: {err}"
        );
    }
}
