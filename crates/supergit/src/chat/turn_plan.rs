use gpui_kit::assets::IconName;
use gpui_kit::component::{Icon, Sizable as _, Theme, h_flex, v_flex};
use gpui_kit::prelude::*;
use gpui_kit::*;
use sg_agents::chat::{PlanStepStatus, TurnPlan};

pub(super) fn render(plan: &TurnPlan, theme: &Theme) -> AnyElement {
    if plan.steps.is_empty() {
        return div().into_any_element();
    }
    let finished_steps = plan
        .steps
        .iter()
        .filter(|step| step.status == PlanStepStatus::Completed)
        .count();
    v_flex()
        .gap_2()
        .rounded_lg()
        .border_1()
        .border_color(theme.border)
        .p_3()
        .child(
            h_flex()
                .gap_2()
                .child(
                    div()
                        .flex_1()
                        .text_sm()
                        .font_weight(FontWeight::SEMIBOLD)
                        .child("Turn plan"),
                )
                .child(
                    div()
                        .text_xs()
                        .text_color(theme.muted_foreground)
                        .child(format!("{finished_steps}/{} complete", plan.steps.len())),
                ),
        )
        .when_some(plan.explanation.as_ref(), |card, explanation| {
            card.child(
                div()
                    .text_sm()
                    .text_color(theme.muted_foreground)
                    .child(explanation.clone()),
            )
        })
        .children(plan.steps.iter().map(|step| {
            let (mark, status, color) = match step.status {
                PlanStepStatus::Completed => (IconName::CircleCheck, "Done", theme.success),
                PlanStepStatus::InProgress if !plan.finished => {
                    (IconName::LoaderCircle, "In progress", theme.blue)
                }
                PlanStepStatus::InProgress => {
                    (IconName::Circle, "Unfinished", theme.muted_foreground)
                }
                PlanStepStatus::Pending => (IconName::Circle, "Pending", theme.muted_foreground),
            };
            h_flex()
                .items_start()
                .gap_2()
                // Level with the first line of the step's text.
                .child(
                    div()
                        .flex_none()
                        .pt(px(3.))
                        .child(Icon::new(mark).small().text_color(color)),
                )
                .child(div().flex_1().min_w_0().text_sm().child(step.text.clone()))
                .child(div().flex_none().text_xs().text_color(color).child(status))
        }))
        .into_any_element()
}
