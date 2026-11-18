use gpui_kit::component::tooltip::Tooltip;

use super::*;

fn compact_tokens(tokens: u64) -> String {
    match tokens {
        0..1000 => tokens.to_string(),
        1000..1_000_000 => format!("{:.1}k", tokens as f64 / 1000.),
        _ => format!("{:.1}M", tokens as f64 / 1_000_000.),
    }
}

impl ChatSession {
    pub(super) fn render_codex_usage(&self, cx: &App) -> Option<AnyElement> {
        let Backend::Codex { fold, .. } = &self.backend else {
            return None;
        };
        let usage = fold.token_usage()?;
        let total = &usage.total;
        let used = usage.last.total_tokens;
        let window = usage.model_context_window.filter(|window| *window > 0);
        let theme = cx.theme();
        let tooltip = format!(
            "{}\nThread total {} tokens · {} input · {} output · {} cached input",
            window.map_or_else(
                || format!("Latest context {used} tokens"),
                |size| format!("Latest context {used} of {size} reported tokens")
            ),
            total.total_tokens,
            total.input_tokens,
            total.output_tokens,
            total.cached_input_tokens,
        );
        let label = window.map_or_else(
            || format!("{} thread tokens", compact_tokens(total.total_tokens)),
            |size| {
                format!(
                    "{} / {} context",
                    compact_tokens(used),
                    compact_tokens(size)
                )
            },
        );
        Some(
            h_flex()
                .id("codex-token-usage")
                .flex_none()
                .gap_1()
                .items_center()
                .when_some(window, |el, size| {
                    let fraction = (used as f64 / size as f64).min(1.0);
                    let color = if fraction >= 0.9 {
                        theme.warning
                    } else {
                        theme.blue
                    };
                    el.child(
                        div()
                            .w(px(24.))
                            .h(px(3.))
                            .rounded_full()
                            .bg(theme.border)
                            .child(
                                div()
                                    .w(px((24. * fraction) as f32))
                                    .h_full()
                                    .rounded_full()
                                    .bg(color),
                            ),
                    )
                })
                .child(label)
                .tooltip(move |window, cx| Tooltip::new(tooltip.clone()).build(window, cx))
                .into_any_element(),
        )
    }
}
