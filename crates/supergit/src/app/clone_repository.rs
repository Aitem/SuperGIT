use super::*;

pub(super) struct CloneForm {
    source: Entity<InputState>,
    name: Entity<InputState>,
    parent: Option<PathBuf>,
    choosing_folder: bool,
    running: bool,
    error: Option<String>,
    previous_focus: Option<FocusHandle>,
    _subscriptions: [Subscription; 2],
}

impl CloneForm {
    fn name_from_inputs(&self, cx: &App) -> Option<String> {
        let source = self.source.read(cx).value();
        let suggested = sg_git::clone_suggested_name(&source)?;
        let name = self.name.read(cx).value();
        let name = name.trim();
        if name.is_empty() {
            Some(suggested)
        } else {
            sg_git::clone_folder_name(name).then(|| name.to_owned())
        }
    }
}

impl SuperGitApp {
    pub(super) fn open_clone_form(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        if self.git_operation_running() || self.clone_repository.is_some() {
            return;
        }
        let source = cx.new(|cx| {
            InputState::new(window, cx).placeholder("https://github.com/owner/repository.git")
        });
        let name = cx.new(|cx| InputState::new(window, cx).placeholder("Use repository name"));
        let subscriptions = [&source, &name].map(|input| {
            cx.subscribe(input, |_, _, event: &InputEvent, cx| {
                if matches!(event, InputEvent::Change) {
                    cx.notify();
                }
            })
        });
        self.clone_repository = Some(CloneForm {
            source: source.clone(),
            name,
            parent: self
                .active_root()
                .and_then(Path::parent)
                .map(Path::to_path_buf),
            choosing_folder: false,
            running: false,
            error: None,
            previous_focus: window.focused(cx),
            _subscriptions: subscriptions,
        });
        source.update(cx, |input, cx| input.focus(window, cx));
        cx.notify();
    }

    fn choose_clone_parent(&mut self, cx: &mut Context<Self>) {
        let Some(form) = &mut self.clone_repository else {
            return;
        };
        if form.running || form.choosing_folder {
            return;
        }
        form.choosing_folder = true;
        let source = form.source.clone();
        let paths = cx.prompt_for_paths(PathPromptOptions {
            files: false,
            directories: true,
            multiple: false,
            prompt: Some("Choose Parent Directory".into()),
        });
        cx.spawn(async move |this, cx| {
            let directory = paths
                .await
                .ok()
                .and_then(Result::ok)
                .flatten()
                .and_then(|paths| paths.into_iter().next());
            this.update(cx, |this, cx| {
                if let Some(form) = this
                    .clone_repository
                    .as_mut()
                    .filter(|form| form.source == source)
                {
                    form.choosing_folder = false;
                    if let Some(directory) = directory {
                        form.parent = Some(directory);
                        form.error = None;
                    }
                    cx.notify();
                }
            })
            .ok();
        })
        .detach();
        cx.notify();
    }

    fn close_clone_form(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        if self
            .clone_repository
            .as_ref()
            .is_some_and(|form| form.running)
        {
            return;
        }
        if let Some(previous) = self
            .clone_repository
            .take()
            .and_then(|form| form.previous_focus)
        {
            previous.focus(window, cx);
        }
        cx.notify();
    }

    fn submit_clone(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        if self.git_operation_running() {
            return;
        }
        let Some(form) = &mut self.clone_repository else {
            return;
        };
        if form.choosing_folder || form.running {
            return;
        }
        let Some((parent, name)) = form.parent.clone().zip(form.name_from_inputs(cx)) else {
            return;
        };
        let source = form.source.read(cx).value().trim().to_string();
        form.running = true;
        form.error = None;
        self.action_task = Some(cx.spawn_in(window, async move |this, cx| {
            let result = cx
                .background_executor()
                .spawn(async move { sg_git::clone_repository(&source, &parent, &name) })
                .await;
            this.update_in(cx, |this, window, cx| {
                this.action_task = None;
                if let Some(form) = &mut this.clone_repository {
                    form.running = false;
                }
                match result {
                    Ok(repo) => {
                        this.close_clone_form(window, cx);
                        if this.open_path(repo, cx) {
                            this.flash("Repository cloned", false, cx);
                        }
                    }
                    Err(error) => {
                        if let Some(form) = &mut this.clone_repository {
                            form.error = Some(format!("{error:#}"));
                        }
                    }
                }
                cx.notify();
            })
            .ok();
        }));
        cx.notify();
    }

    pub(super) fn render_clone_form(&self, cx: &mut Context<Self>) -> AnyElement {
        let Some(form) = &self.clone_repository else {
            return div().into_any_element();
        };
        let theme = cx.theme().clone();
        let target = form
            .parent
            .as_ref()
            .zip(form.name_from_inputs(cx))
            .map(|(parent, name)| parent.join(name));
        let can_start = !form.running
            && !form.choosing_folder
            && target.is_some()
            && !self.git_operation_running();
        let start_tooltip = if form.running {
            "The repository is downloading"
        } else if form.choosing_folder {
            "Finish choosing the parent folder first"
        } else if form.parent.is_none() {
            "Choose a parent folder first"
        } else if target.is_none() {
            "Enter a repository URL or path and a valid folder name"
        } else if self.git_operation_running() {
            "Wait for the running Git operation to finish"
        } else {
            "Download the repository into the folder shown above, then open it"
        };
        div()
            .absolute()
            .inset_0()
            .flex()
            .justify_center()
            .items_start()
            .pt(px(64.))
            .bg(gpui_kit::black().opacity(0.35))
            .on_mouse_down(
                MouseButton::Left,
                cx.listener(|this, _, window, cx| this.close_clone_form(window, cx)),
            )
            .child(
                v_flex()
                    .track_focus(&self.clone_focus)
                    .w(px(560.))
                    .max_w_full()
                    .p_4()
                    .gap_3()
                    .rounded_lg()
                    .shadow_lg()
                    .border_1()
                    .border_color(theme.border)
                    .bg(theme.background)
                    .on_mouse_down(MouseButton::Left, |_, _, cx| cx.stop_propagation())
                    .capture_key_down(cx.listener(|this, event: &KeyDownEvent, window, cx| {
                        if event.keystroke.key == "escape" {
                            this.close_clone_form(window, cx);
                            cx.stop_propagation();
                        }
                    }))
                    .child(
                        div()
                            .text_lg()
                            .font_weight(FontWeight::BOLD)
                            .child("Clone repository"),
                    )
                    .child(
                        v_flex()
                            .gap_1()
                            .child(div().text_sm().child("Repository URL or local path"))
                            .child(Input::new(&form.source).disabled(form.running)),
                    )
                    .child(
                        h_flex()
                            .gap_2()
                            .items_end()
                            .child(
                                v_flex()
                                    .flex_1()
                                    .min_w_0()
                                    .gap_1()
                                    .child(div().text_sm().child("Folder name"))
                                    .child(Input::new(&form.name).disabled(form.running)),
                            )
                            .child(
                                Button::new("choose-clone-parent")
                                    .label("Choose parent folder")
                                    .outline()
                                    .disabled(form.running || form.choosing_folder)
                                    .tooltip(if form.running {
                                        "Wait for the clone to finish"
                                    } else {
                                        "Pick the folder the clone is created in"
                                    })
                                    .on_click(
                                        cx.listener(|this, _, _, cx| this.choose_clone_parent(cx)),
                                    ),
                            ),
                    )
                    .child(
                        div()
                            .text_sm()
                            .text_color(theme.muted_foreground)
                            .child(match target {
                                Some(target) => format!("Clone into {}", target.display()),
                                None => form
                                    .parent
                                    .as_ref()
                                    .map(|parent| format!("Parent folder · {}", parent.display()))
                                    .unwrap_or_else(|| {
                                        "Choose where to place the repository".into()
                                    }),
                            }),
                    )
                    .when_some(form.error.as_ref(), |modal, error| {
                        modal.child(
                            div()
                                .text_sm()
                                .text_color(theme.danger)
                                .child(error.clone()),
                        )
                    })
                    .child(
                        h_flex()
                            .justify_end()
                            .gap_2()
                            .child(
                                Button::new("cancel-clone-repository")
                                    .label("Cancel")
                                    .small()
                                    .ghost()
                                    .disabled(form.running)
                                    .tooltip(if form.running {
                                        "Wait for the clone to finish"
                                    } else {
                                        "Close without cloning (Esc)"
                                    })
                                    .on_click(cx.listener(|this, _, window, cx| {
                                        this.close_clone_form(window, cx)
                                    })),
                            )
                            .child(
                                Button::new("start-clone-repository")
                                    .label(if form.running { "Cloning…" } else { "Clone" })
                                    .small()
                                    .primary()
                                    .disabled(!can_start)
                                    .tooltip(start_tooltip)
                                    .on_click(cx.listener(|this, _, window, cx| {
                                        this.submit_clone(window, cx)
                                    })),
                            ),
                    ),
            )
            .into_any_element()
    }
}
