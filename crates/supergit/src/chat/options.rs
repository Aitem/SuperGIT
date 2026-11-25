use super::*;
use gpui_kit::component::menu::{DropdownMenu as _, PopupMenuItem};

#[derive(Default)]
pub(super) struct AgentOptions {
    pub(super) model: Option<String>,
    pub(super) effort: Option<String>,
    catalog: Option<Result<Vec<sg_codex::Model>, String>>,
    catalog_task: Option<Task<()>>,
    /// Codex remembers overrides after a turn. Restore its original defaults
    /// when a choice is cleared, without disturbing a newly opened thread.
    apply_model: bool,
    apply_effort: bool,
}

impl AgentOptions {
    fn codex_models(&self) -> &[sg_codex::Model] {
        self.catalog
            .as_ref()
            .and_then(|catalog| catalog.as_ref().ok())
            .map_or(&[], Vec::as_slice)
    }

    fn codex_model(&self, reported: Option<&str>) -> Option<&sg_codex::Model> {
        let selected = self.model.as_deref().or(reported);
        self.codex_models()
            .iter()
            .find(|model| Some(model.id.as_str()) == selected)
            .or_else(|| self.codex_models().iter().find(|model| model.is_default))
    }

    pub(super) fn codex_turn(&self, reported: Option<&str>) -> sg_codex::TurnOptions {
        let default = self.codex_models().iter().find(|model| model.is_default);
        let model = self.model.clone().or_else(|| {
            self.apply_model
                .then(|| default.map(|model| model.id.clone()))
                .flatten()
        });
        let effort = self.effort.clone().or_else(|| {
            let model = if self.model.is_none() && self.apply_model {
                default
            } else {
                self.codex_model(reported)
            };
            model
                .filter(|_| self.apply_effort)
                .and_then(|model| model.default_effort.clone())
        });
        sg_codex::TurnOptions { model, effort }
    }
}

impl ChatSession {
    pub(super) fn load_models(&mut self, cx: &mut Context<Self>) {
        if self.provider() != Provider::Codex
            || self.options.catalog.is_some()
            || self.options.catalog_task.is_some()
        {
            return;
        }
        let cwd = self.cwd.clone();
        self.options.catalog_task = Some(cx.spawn(async move |this, cx| {
            let result =
                cx.background_executor()
                    .spawn(async move {
                        sg_codex::list_models(&cwd).map_err(|error| format!("{error:#}"))
                    })
                    .await;
            let _ = this.update(cx, |this, cx| {
                this.options.catalog = Some(result);
                this.options.catalog_task = None;
                cx.notify();
            });
        }));
    }

    fn change_option(&mut self, model: bool, value: Option<String>, cx: &mut Context<Self>) {
        if self.convo.status.is_busy() {
            return;
        }
        if model {
            if self.options.model == value {
                return;
            }
            self.options.model = value;
            self.options.effort = None;
            self.options.apply_model = true;
        } else {
            self.options.effort = value;
        }
        self.options.apply_effort = true;
        if self.provider() == Provider::Claude {
            // Claude's effort applies at process startup. Resume the same
            // session with the new flags on the next send.
            let _ = self.take_idle_process(cx);
        }
        cx.notify();
    }

    /// The model and the effort for the next turn: one trigger by Send
    /// ("Opus 5.5 High", as on claude.ai) and one menu with both.
    pub(super) fn render_model_options(&self, cx: &mut Context<Self>) -> impl IntoElement {
        let codex = self.provider() == Provider::Codex;
        let busy = self.convo.status.is_busy();
        let current = self.options.model.clone();
        let mut models = vec![(None, "Default".to_string())];
        if codex {
            models.extend(
                self.options
                    .codex_models()
                    .iter()
                    .map(|model| (Some(model.id.clone()), model.label.clone())),
            );
        } else {
            models.extend(
                [
                    ("sonnet", "Sonnet"),
                    ("opus", "Opus"),
                    ("haiku", "Haiku"),
                    ("fable", "Fable"),
                ]
                .map(|(model, label)| (Some(model.to_string()), label.to_string())),
            );
        }
        let model = if codex && current.is_none() && self.options.apply_model {
            self.options
                .codex_models()
                .iter()
                .find(|model| model.is_default)
        } else {
            self.options.codex_model(self.convo.model.as_deref())
        };
        let active_model = models
            .iter()
            .find(|(model, _)| model.is_some() && model == &current)
            .map(|(_, label)| label.clone())
            .or_else(|| current.clone())
            .or_else(|| model.map(|model| model.label.clone()))
            .or_else(|| self.convo.model.as_deref().map(model_name))
            .unwrap_or_else(|| "Default model".to_string());
        let choices = if codex {
            model.map_or_else(Vec::new, |model| model.efforts.clone())
        } else {
            ["low", "medium", "high", "xhigh", "max"]
                .map(str::to_string)
                .to_vec()
        };
        let mut efforts = vec![(None, "Default".to_string())];
        efforts.extend(choices.into_iter().map(|effort| {
            let label = effort_name(&effort);
            (Some(effort), label)
        }));
        let current_effort = self.options.effort.clone();
        let active_effort = current_effort.as_deref().map(effort_name);
        let owner = cx.entity().downgrade();
        let failure = self
            .options
            .catalog
            .as_ref()
            .and_then(|result| result.as_ref().err())
            .cloned();
        let loading = self.options.catalog_task.is_some();
        let theme = cx.theme().clone();
        let parts = std::iter::once(div().text_color(theme.foreground).child(active_model)).chain(
            active_effort.map(|effort| div().text_color(theme.muted_foreground).child(effort)),
        );
        chevron_trigger("chat-model", busy, &theme, parts)
            .tooltip(if busy {
                "The model can change once this turn ends".to_string()
            } else {
                failure
                    .clone()
                    .unwrap_or_else(|| "Model and effort for the next turn".to_string())
            })
            .dropdown_menu_with_anchor(Anchor::BottomRight, move |mut menu, _, _| {
                menu = menu.label("Model");
                if loading {
                    menu = menu.item(PopupMenuItem::new("Loading Codex models…").disabled(true));
                }
                if failure.is_some() {
                    let owner = owner.clone();
                    menu = menu.item(PopupMenuItem::new("Retry loading Codex models").on_click(
                        move |_, _, cx| {
                            let _ = owner.update(cx, |this, cx| {
                                this.options.catalog = None;
                                this.load_models(cx);
                                cx.notify();
                            });
                        },
                    ));
                }
                for (id, label) in &models {
                    let owner = owner.clone();
                    let value = id.clone();
                    menu = menu.item(
                        PopupMenuItem::new(label.clone())
                            .checked(id == &current)
                            .on_click(move |_, _, cx| {
                                let _ = owner.update(cx, |this, cx| {
                                    this.change_option(true, value.clone(), cx)
                                });
                            }),
                    );
                }
                if efforts.len() > 1 {
                    menu = menu.separator().label("Effort");
                    for (id, label) in &efforts {
                        let owner = owner.clone();
                        let value = id.clone();
                        menu = menu.item(
                            PopupMenuItem::new(label.clone())
                                .checked(id == &current_effort)
                                .on_click(move |_, _, cx| {
                                    let _ = owner.update(cx, |this, cx| {
                                        this.change_option(false, value.clone(), cx)
                                    });
                                }),
                        );
                    }
                }
                menu
            })
    }
}

impl ChatSession {
    /// Claude's permission mode or Codex's access policy: a dropdown by
    /// Send that opens upward (the composer sits at the window's bottom),
    /// each choice with what it means.
    pub(super) fn render_mode_options(&self, cx: &mut Context<Self>) -> AnyElement {
        let theme = cx.theme().clone();
        let owner = cx.entity().downgrade();
        match &self.backend {
            Backend::Claude { mode: current, .. } => {
                let current = *current;
                setting_trigger("chat-mode", "Mode", current.label(), false, &theme)
                    .tooltip(mode_description(current))
                    .dropdown_menu_with_anchor(Anchor::BottomLeft, move |mut menu, _, _| {
                        for mode in PermissionMode::ALL {
                            let owner = owner.clone();
                            menu = menu.item(
                                described_item(mode.label(), mode_description(mode))
                                    .checked(mode == current)
                                    .on_click(move |_, _, cx| {
                                        let _ =
                                            owner.update(cx, |this, cx| this.set_mode(mode, cx));
                                    }),
                            );
                        }
                        menu
                    })
                    .into_any_element()
            }
            Backend::Codex {
                policy: current, ..
            } => {
                let current = *current;
                setting_trigger("chat-policy", "Access", current.label(), false, &theme)
                    .tooltip(format!(
                        "{} · approvals {}, sandbox {} · applies from the next turn",
                        current.description(),
                        current.approval_policy(),
                        current.sandbox_mode()
                    ))
                    .dropdown_menu_with_anchor(Anchor::BottomLeft, move |mut menu, _, _| {
                        for policy in Policy::ALL {
                            let owner = owner.clone();
                            menu = menu.item(
                                described_item(policy.label(), policy.description())
                                    .checked(policy == current)
                                    .on_click(move |_, _, cx| {
                                        let _ = owner
                                            .update(cx, |this, cx| this.set_policy(policy, cx));
                                    }),
                            );
                        }
                        menu
                    })
                    .into_any_element()
            }
        }
    }
}

/// A composer setting as a dropdown's trigger: its name muted, its value
/// in the text color, then a chevron.
fn setting_trigger(
    id: &'static str,
    name: &'static str,
    value: impl Into<SharedString>,
    disabled: bool,
    theme: &Theme,
) -> Button {
    chevron_trigger(
        id,
        disabled,
        theme,
        [
            div().text_color(theme.muted_foreground).child(name),
            div().text_color(theme.foreground).child(value.into()),
        ],
    )
}

/// A dropdown's trigger in the composer: its parts, then a chevron.
fn chevron_trigger(
    id: &'static str,
    disabled: bool,
    theme: &Theme,
    parts: impl IntoIterator<Item = Div>,
) -> Button {
    Button::new(id).small().ghost().disabled(disabled).child(
        h_flex()
            .gap_1()
            .items_center()
            .when(disabled, |row| row.opacity(0.5))
            .children(parts)
            .child(
                gpui_kit::component::Icon::new(gpui_kit::assets::IconName::ChevronDown)
                    .xsmall()
                    .text_color(theme.muted_foreground),
            ),
    )
}

/// A menu choice with what it means under it.
fn described_item(name: &'static str, about: &'static str) -> PopupMenuItem {
    PopupMenuItem::element(move |_, cx| {
        v_flex().py_0p5().child(div().text_sm().child(name)).child(
            div()
                .text_xs()
                .text_color(cx.theme().muted_foreground)
                .child(about),
        )
    })
}

/// What a permission mode lets Claude do.
fn mode_description(mode: PermissionMode) -> &'static str {
    match mode {
        PermissionMode::Default => "Ask before edits and commands",
        PermissionMode::AcceptEdits => "Accept file edits, ask for commands",
        PermissionMode::Plan => "Plan only, no changes",
        PermissionMode::Auto => "Claude decides; a safety check blocks risky actions",
        PermissionMode::BypassPermissions => "Never ask (careful)",
    }
}

/// "claude-opus-5-5" → "Opus 5.5": a Claude model id as people say it.
/// Anything else (Codex ids, aliases) stays as it is.
fn model_name(id: &str) -> String {
    let (base, wide) = match id.strip_suffix("[1m]") {
        Some(base) => (base, " 1M"),
        None => (id, ""),
    };
    let Some(rest) = base.strip_prefix("claude-") else {
        return id.to_string();
    };
    let mut family = None;
    let mut version = Vec::new();
    for part in rest.split('-') {
        if part.chars().all(|c| c.is_ascii_digit()) {
            // Version numbers; snapshot dates (20250929) are left out.
            if part.len() <= 2 {
                version.push(part);
            }
        } else if family.is_none() {
            family = Some(part);
        } else {
            return id.to_string();
        }
    }
    let Some(family) = family else {
        return id.to_string();
    };
    let mut chars = family.chars();
    let name: String = chars
        .next()
        .map(|first| first.to_ascii_uppercase())
        .into_iter()
        .chain(chars)
        .collect();
    if version.is_empty() {
        format!("{name}{wide}")
    } else {
        format!("{name} {}{wide}", version.join("."))
    }
}

/// "xhigh" → "Extra high": an effort level as the menu shows it.
fn effort_name(effort: &str) -> String {
    if effort == "xhigh" {
        return "Extra high".to_string();
    }
    let mut chars = effort.chars();
    chars
        .next()
        .map(|first| first.to_ascii_uppercase())
        .into_iter()
        .chain(chars)
        .collect()
}

#[cfg(test)]
mod tests {
    use super::{effort_name, model_name};

    #[test]
    fn claude_models_read_as_names() {
        assert_eq!(model_name("claude-opus-5-5"), "Opus 5.5");
        assert_eq!(model_name("claude-sonnet-4-5-20250929"), "Sonnet 4.5");
        assert_eq!(model_name("claude-3-5-haiku-20241022"), "Haiku 3.5");
        assert_eq!(model_name("claude-opus-5-5[1m]"), "Opus 5.5 1M");
        assert_eq!(model_name("gpt-5.5-codex"), "gpt-5.5-codex");
        assert_eq!(model_name("opus"), "opus");
    }

    #[test]
    fn efforts_read_as_words() {
        assert_eq!(effort_name("xhigh"), "Extra high");
        assert_eq!(effort_name("medium"), "Medium");
        assert_eq!(effort_name(""), "");
    }
}
