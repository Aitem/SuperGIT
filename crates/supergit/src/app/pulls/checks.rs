use sg_github::{CheckAnnotation, CheckInspection};

use super::*;

pub(super) struct OpenCheck {
    pub(super) id: String,
    details: LoadState<CheckInspection>,
}

impl Pulls {
    pub(super) fn close_check_inspection(&mut self) {
        self.check_inspection = None;
        self.check_inspection_task = None;
    }
}

impl SuperGitApp {
    #[cfg(feature = "screenshot")]
    pub(super) fn debug_check_inspection(&mut self, number: u64, cx: &mut Context<Self>) {
        if self.pulls.shown == Some(number)
            && self.pulls.check_inspection.is_none()
            && let Ok(name) = std::env::var("SUPERGIT_DEBUG_PR_CHECK")
            && let Some(id) = self
                .pulls
                .details
                .get(&number)
                .and_then(|loaded| loaded.details.value())
                .and_then(|details| details.checks.iter().find(|check| check.name == name))
                .and_then(|check| check.run_id.clone())
        {
            self.fetch_check_inspection(id, cx);
        }
    }

    pub(super) fn toggle_check_inspection(&mut self, id: &str, cx: &mut Context<Self>) {
        if self
            .pulls
            .check_inspection
            .as_ref()
            .is_some_and(|open| open.id == id)
        {
            self.pulls.close_check_inspection();
            cx.notify();
        } else {
            self.fetch_check_inspection(id.to_string(), cx);
        }
    }

    fn fetch_check_inspection(&mut self, id: String, cx: &mut Context<Self>) {
        let Some(root) = self.repo.as_ref().map(|repo| repo.root.clone()) else {
            return;
        };
        self.pulls.check_inspection = Some(OpenCheck {
            id: id.clone(),
            details: LoadState::Loading,
        });
        self.pulls.check_inspection_task = Some(cx.spawn(async move |this, cx| {
            let requested = id.clone();
            let result = cx
                .background_executor()
                .spawn(async move {
                    sg_github::check_inspection(&root, &requested)
                        .map_err(|error| error.to_string())
                })
                .await;
            let _ = this.update(cx, |this, cx| {
                if let Some(open) = &mut this.pulls.check_inspection
                    && open.id == id
                {
                    open.details = LoadState::from_result(result);
                    this.pulls.check_inspection_task = None;
                    cx.notify();
                }
            });
        }));
        cx.notify();
    }

    pub(super) fn render_check_inspection(
        &self,
        open: &OpenCheck,
        check: &Check,
        theme: &gpui_kit::component::Theme,
        cx: &mut Context<Self>,
    ) -> AnyElement {
        let content = match &open.details {
            LoadState::Loading => div()
                .text_color(theme.muted_foreground)
                .child("Loading check details…")
                .into_any_element(),
            LoadState::Failed { error, .. } => div()
                .text_color(theme.danger)
                .child(error.clone())
                .into_any_element(),
            LoadState::Ready(details) => render_inspection(details, theme),
        };
        let id = open.id.clone();
        let copy = open
            .details
            .value()
            .map(|details| copy_text(check, details));
        v_flex()
            .my_1()
            .min_w_0()
            .gap_2()
            .p_3()
            .text_sm()
            .rounded_md()
            .border_1()
            .border_color(theme.border)
            .bg(theme.background)
            .child(
                h_flex()
                    .items_center()
                    .gap_2()
                    .child(
                        div()
                            .flex_1()
                            .font_weight(FontWeight::SEMIBOLD)
                            .child("Check details"),
                    )
                    .when_some(copy, |row, copy| {
                        row.child(
                            Button::new("copy-check-details")
                                .label("Copy diagnostics")
                                .xsmall()
                                .ghost()
                                .on_click(cx.listener(move |this, _, _, cx| {
                                    cx.write_to_clipboard(ClipboardItem::new_string(copy.clone()));
                                    this.flash("Copied check diagnostics", false, cx);
                                })),
                        )
                    })
                    .child(
                        Button::new("reload-check-details")
                            .label("Refresh")
                            .xsmall()
                            .ghost()
                            .disabled(open.details.is_loading())
                            .on_click(cx.listener(move |this, _, _, cx| {
                                this.fetch_check_inspection(id.clone(), cx);
                            })),
                    ),
            )
            .child(content)
            .into_any_element()
    }
}

fn render_inspection(details: &CheckInspection, theme: &gpui_kit::component::Theme) -> AnyElement {
    let summary = details
        .summary
        .as_deref()
        .filter(|text| !text.trim().is_empty());
    let failed_steps: Vec<_> = details.failed_steps().collect();
    let annotations: Vec<_> = details.annotations().collect();
    let remaining = details.annotations_remaining();
    v_flex()
        .gap_2()
        .min_w_0()
        .when_some(summary, |body, summary| {
            body.child(div().child(summary.to_string()))
        })
        .when(!failed_steps.is_empty(), |body| {
            body.child(
                div()
                    .text_color(theme.danger)
                    .child(format!("Failed steps · {}", failed_steps.join(" · "))),
            )
        })
        .when(
            summary.is_none() && failed_steps.is_empty() && annotations.is_empty(),
            |body| {
                body.child(
                    div()
                        .text_color(theme.muted_foreground)
                        .child("GitHub returned no diagnostics for this run"),
                )
            },
        )
        .children(
            annotations
                .into_iter()
                .map(|annotation| render_annotation(annotation, theme)),
        )
        .when(remaining > 0, |body| {
            body.child(
                div()
                    .text_xs()
                    .text_color(theme.muted_foreground)
                    .child(format!("{remaining} more annotations on GitHub")),
            )
        })
        .into_any_element()
}

fn render_annotation(
    annotation: &CheckAnnotation,
    theme: &gpui_kit::component::Theme,
) -> AnyElement {
    let (severity, color) = match annotation.annotation_level.as_deref() {
        Some("FAILURE") => ("Failure", theme.danger),
        Some("WARNING") => ("Warning", theme.warning),
        _ => ("Notice", theme.muted_foreground),
    };
    let location = annotation_path(annotation);
    v_flex()
        .gap_1()
        .pt_2()
        .border_t_1()
        .border_color(theme.border)
        .child(
            h_flex()
                .gap_2()
                .min_w_0()
                .text_xs()
                .child(div().flex_none().text_color(color).child(severity))
                .when_some(location, |row, location| {
                    row.child(
                        div()
                            .min_w_0()
                            .truncate()
                            .text_color(theme.muted_foreground)
                            .child(location),
                    )
                }),
        )
        .when_some(
            annotation
                .title
                .as_deref()
                .filter(|title| !title.trim().is_empty()),
            |row, title| {
                row.child(
                    div()
                        .font_weight(FontWeight::SEMIBOLD)
                        .child(title.to_string()),
                )
            },
        )
        .child(div().child(annotation.message.clone()))
        .into_any_element()
}

fn annotation_path(annotation: &CheckAnnotation) -> Option<String> {
    let path = annotation.path.as_ref().filter(|path| !path.is_empty())?;
    Some(match annotation.line() {
        Some(line) => format!("{path}:{line}"),
        None => path.clone(),
    })
}

fn copy_text(check: &Check, details: &CheckInspection) -> String {
    use std::fmt::Write as _;

    let mut text = format!("{} · {}\n", check.name, check.detail);
    if let Some(summary) = &details.summary {
        let _ = writeln!(text, "\n{summary}");
    }
    for step in details.failed_steps() {
        let _ = writeln!(text, "Failed step: {step}");
    }
    for annotation in details.annotations() {
        let _ = writeln!(
            text,
            "\n{} {}",
            annotation.annotation_level.as_deref().unwrap_or("NOTICE"),
            annotation_path(annotation).unwrap_or_default()
        );
        if let Some(title) = &annotation.title {
            let _ = writeln!(text, "{title}");
        }
        let _ = writeln!(text, "{}", annotation.message);
    }
    if let Some(url) = &check.url {
        let _ = writeln!(text, "\n{url}");
    }
    text
}
