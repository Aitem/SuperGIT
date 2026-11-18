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

    pub(super) fn render_model_options(&self, cx: &mut Context<Self>) -> impl IntoElement {
        let codex = self.provider() == Provider::Codex;
        let busy = self.convo.status.is_busy();
        let current = self.options.model.clone();
        let mut models = vec![(None, "Provider default".to_string())];
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
            .map(|(_, label)| label.as_str())
            .or(current.as_deref())
            .or_else(|| model.map(|model| model.label.as_str()))
            .or(self.convo.model.as_deref())
            .unwrap_or("Default");
        let choices = if codex {
            model.map_or_else(Vec::new, |model| model.efforts.clone())
        } else {
            ["low", "medium", "high", "xhigh", "max"]
                .map(str::to_string)
                .to_vec()
        };
        let mut efforts = vec![(None, "Provider default".to_string())];
        efforts.extend(
            choices
                .into_iter()
                .map(|effort| (Some(effort.clone()), effort)),
        );
        let current_effort = self.options.effort.clone();
        let active_effort = current_effort.as_deref().unwrap_or("Default");
        let owner = cx.entity().downgrade();
        let model_owner = owner.clone();
        let failure = self
            .options
            .catalog
            .as_ref()
            .and_then(|result| result.as_ref().err())
            .cloned();
        let loading = self.options.catalog_task.is_some();
        h_flex()
            .gap_1()
            .min_w_0()
            .child(
                Button::new("chat-model")
                    .small()
                    .ghost()
                    .label(format!("Model · {active_model} ▾"))
                    .tooltip(
                        failure
                            .as_deref()
                            .unwrap_or("Model for the next turn")
                            .to_string(),
                    )
                    .disabled(busy)
                    .dropdown_menu(move |mut menu, _, _| {
                        if loading {
                            menu = menu
                                .item(PopupMenuItem::new("Loading Codex models…").disabled(true));
                        }
                        if failure.is_some() {
                            let owner = model_owner.clone();
                            menu = menu.item(
                                PopupMenuItem::new("Retry loading Codex models").on_click(
                                    move |_, _, cx| {
                                        let _ = owner.update(cx, |this, cx| {
                                            this.options.catalog = None;
                                            this.load_models(cx);
                                            cx.notify();
                                        });
                                    },
                                ),
                            );
                        }
                        for (id, label) in &models {
                            let owner = model_owner.clone();
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
                        menu
                    }),
            )
            .child(
                Button::new("chat-effort")
                    .small()
                    .ghost()
                    .label(format!("Effort · {active_effort} ▾"))
                    .tooltip("Reasoning effort for the next turn")
                    .disabled(busy || efforts.len() < 2)
                    .dropdown_menu(move |mut menu, _, _| {
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
                        menu
                    }),
            )
    }
}
