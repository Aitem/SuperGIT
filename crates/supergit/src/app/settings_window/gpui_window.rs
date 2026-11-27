//! The settings window drawn by GPUI, where AppKit's is not (Linux): a
//! toolbar of pages, each a scrolling column of grouped rows (`group`,
//! `row`: name and note left, control right; switches for on/off).

use super::*;
use crate::model::project_name;
use crate::updates;
use gpui_kit::component::menu::DropdownMenu as _;
use gpui_kit::component::switch::Switch;

const WIDTH: f32 = 680.;
const HEIGHT: f32 = 640.;
/// The page's column: wider windows center it.
const COLUMN: f32 = 600.;

impl Page {
    fn icon(self) -> IconName {
        match self {
            Self::General => IconName::Settings,
            Self::Agents => IconName::Bot,
            Self::Notifications => IconName::Bell,
            Self::Diffs => IconName::FileDiff,
            Self::Repository => IconName::FolderGit2,
        }
    }

    fn about(self) -> &'static str {
        match self {
            Self::General => "Appearance and updates",
            Self::Agents => "How conversations open, and the prompt agents start with",
            Self::Notifications => "Sounds and banners when an agent needs you or finishes",
            Self::Diffs => "Diff layout and difftastic",
            Self::Repository => "Worktree cleanup in the open repository",
        }
    }
}

/// The settings window while it is open, and the page it showed last.
#[derive(Default)]
struct Open {
    window: Option<(AnyWindowHandle, WeakEntity<SettingsWindow>)>,
    last: Page,
}

impl Global for Open {}

/// Show the settings window on `page` (else the page shown last), opening
/// it or bringing it forward. `focus`: false for screenshot runs, which must
/// not take the user's keystrokes.
pub(crate) fn open(
    app: WeakEntity<SuperGitApp>,
    page: Option<Page>,
    focus: bool,
    cx: &mut App,
) -> Option<AnyWindowHandle> {
    let open = cx.default_global::<Open>();
    let page = page.unwrap_or(open.last);
    if let Some((handle, view)) = open.window.clone() {
        let shown = handle.update(cx, |_, window, cx| {
            window.activate_window();
            let _ = view.update(cx, |view, cx| view.show(page, cx));
        });
        if shown.is_ok() {
            return Some(handle);
        }
    }
    let bounds = Bounds::centered(None, size(px(WIDTH), px(HEIGHT)), cx);
    let options = WindowOptions {
        focus,
        window_bounds: Some(WindowBounds::Windowed(bounds)),
        window_min_size: Some(size(px(520.), px(360.))),
        app_id: Some("supergit".into()),
        ..TitleBar::window_options()
    };
    let (handle, view) = gpui_kit::open_window(options, cx, |window, cx| {
        window.set_window_title("Settings");
        cx.new(|cx| SettingsWindow::new(app, page, window, cx))
    })
    .ok()?;
    cx.default_global::<Open>().window = Some((handle, view.downgrade()));
    Some(handle)
}

pub(crate) struct SettingsWindow {
    app: WeakEntity<SuperGitApp>,
    page: Page,
    focus: FocusHandle,
    scroll: ScrollHandle,
    prompt: Entity<TextareaState>,
    /// Why the last change could not be made (banners not allowed).
    problem: Option<String>,
    _subscriptions: Vec<Subscription>,
}

impl SettingsWindow {
    fn new(
        app: WeakEntity<SuperGitApp>,
        page: Page,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> Self {
        let prompt = cx.new(|cx| {
            TextareaState::new(window, cx)
                .auto_grow(6, 18)
                .placeholder("No initial prompt: agents get their own instructions only")
                .default_value(crate::settings::load().initial_prompt().to_string())
        });
        let mut subscriptions = vec![
            // The preview follows the text; the template is saved on leaving.
            cx.subscribe(&prompt, |this, _, event: &InputEvent, cx| match event {
                InputEvent::Change => cx.notify(),
                InputEvent::Blur => this.save_prompt(cx),
                InputEvent::PressEnter { .. } | InputEvent::Focus => {}
            }),
            cx.on_release(|this, cx| this.save_prompt(cx)),
        ];
        // What the pages show (the agent view, diffs, the repository) is the
        // app's: follow it.
        if let Some(app) = app.upgrade() {
            subscriptions.push(cx.observe(&app, |_, _, cx| cx.notify()));
        }
        let focus = cx.focus_handle();
        focus.focus(window, cx);
        Self {
            app,
            page,
            focus,
            scroll: ScrollHandle::new(),
            prompt,
            problem: None,
            _subscriptions: subscriptions,
        }
    }

    fn show(&mut self, page: Page, cx: &mut Context<Self>) {
        cx.default_global::<Open>().last = page;
        if self.page != page {
            self.page = page;
            self.problem = None;
            self.scroll.set_offset(point(px(0.), px(0.)));
            cx.notify();
        }
    }

    fn save_prompt(&self, cx: &App) {
        super::save_prompt(&self.prompt.read(cx).value());
    }

    fn set_prompt(&mut self, text: &str, window: &mut Window, cx: &mut Context<Self>) {
        let text = text.to_string();
        self.prompt
            .update(cx, |input, cx| input.set_value(text, window, cx));
        self.save_prompt(cx);
        cx.notify();
    }

    /// A banner on or off: turning one on asks the system first.
    fn set_banner(&mut self, needs_you: bool, on: bool, cx: &mut Context<Self>) {
        self.problem = super::set_banner(needs_you, on, cx)
            .err()
            .map(|error| error.to_string());
        cx.notify();
    }

    /// Run `f` on the app, if its window is still open.
    fn with_app(
        app: &WeakEntity<SuperGitApp>,
        cx: &mut App,
        f: impl FnOnce(&mut SuperGitApp, &mut Context<SuperGitApp>),
    ) {
        let _ = app.update(cx, f);
    }

    // ---- pages -------------------------------------------------------------

    fn general(&self, cx: &mut Context<Self>) -> Vec<AnyElement> {
        use crate::settings::Appearance;
        let theme = cx.theme().clone();
        let now = crate::appearance::current(cx);
        let choices = [Appearance::System, Appearance::Light, Appearance::Dark];
        let appearance = ButtonGroup::new("appearance")
            .outline()
            .small()
            .children(choices.map(|choice| {
                Button::new(SharedString::from(format!("appearance-{}", choice.label())))
                    .label(choice.label())
                    .selected(now == choice)
                    .tooltip(match choice {
                        Appearance::System => "Follow the system's light or dark mode",
                        Appearance::Light => "Always light",
                        Appearance::Dark => "Always dark",
                    })
            }))
            .on_click(move |clicked: &Vec<usize>, _, cx| {
                if let Some(&ix) = clicked.first() {
                    crate::appearance::set(choices[ix], cx);
                }
            });
        let app = self.app.clone();
        let check = Button::new("check-updates")
            .label("Check Now")
            .small()
            .outline()
            .tooltip("Look for a newer release on GitHub; the status bar says what it found")
            .on_click(move |_, window, cx| {
                let _ = app.update(cx, |app, cx| {
                    app.check_for_updates(&CheckForUpdates, window, cx)
                });
            });
        vec![
            group(
                "Appearance",
                [row("Theme", None, appearance, &theme)],
                None,
                &theme,
            ),
            group(
                "Updates",
                [row(
                    "Version",
                    Some(format!("SuperGIT {}", updates::running_version())),
                    check,
                    &theme,
                )],
                Some("SuperGIT also checks at startup and then every 12 hours."),
                &theme,
            ),
        ]
    }

    fn agents(&self, app: &Entity<SuperGitApp>, cx: &mut Context<Self>) -> Vec<AnyElement> {
        use crate::settings::AgentView;
        let theme = cx.theme().clone();
        let current = app.read(cx).agent_view;
        let weak = self.app.clone();
        let views = [AgentView::Chat, AgentView::Terminal];
        let view_toggle = ButtonGroup::new("agent-view")
            .outline()
            .small()
            .children(views.map(|view| {
                Button::new(SharedString::from(format!("agent-view-{}", view.label())))
                    .label(view.label())
                    .selected(current == view)
                    .tooltip(match view {
                        AgentView::Chat => "Open conversations in SuperGIT's chat",
                        AgentView::Terminal => {
                            "Open conversations as the agent's own terminal UI (claude, codex)"
                        }
                    })
            }))
            .on_click(move |clicked: &Vec<usize>, _, cx| {
                if let Some(&ix) = clicked.first() {
                    Self::with_app(&weak, cx, |app, cx| app.set_agent_view(views[ix], cx));
                }
            });
        vec![
            group(
                "Conversations",
                [row(
                    "Open conversations as",
                    Some(
                        "Terminal runs the agent's own TUI (claude, codex) in SuperGIT's \
                         colors; Chat is SuperGIT's own view."
                            .into(),
                    ),
                    view_toggle,
                    &theme,
                )],
                None,
                &theme,
            ),
            self.initial_prompt(app, &theme, cx),
        ]
    }

    fn initial_prompt(
        &self,
        app: &Entity<SuperGitApp>,
        theme: &gpui_kit::component::Theme,
        cx: &mut Context<Self>,
    ) -> AnyElement {
        let text = self.prompt.read(cx).value();
        // What the selected worktree's agents would get.
        let app = app.read(cx);
        let preview = app
            .active_root()
            .zip(app.selected_info())
            .map(|(root, info)| {
                let place = SuperGitApp::place(root, info);
                (info.name(), crate::initial_prompt::render(&text, &place))
            });
        let buttons = h_flex()
            .gap_1()
            .child(
                Button::new("initial-prompt-default")
                    .label("Default")
                    .small()
                    .ghost()
                    .disabled(text == crate::initial_prompt::DEFAULT)
                    .tooltip("Put back SuperGIT's prompt")
                    .on_click(cx.listener(|this, _, window, cx| {
                        this.set_prompt(crate::initial_prompt::DEFAULT, window, cx)
                    })),
            )
            .child(
                Button::new("initial-prompt-none")
                    .label("None")
                    .small()
                    .ghost()
                    .disabled(text.is_empty())
                    .tooltip("Start agents with their own instructions only")
                    .on_click(cx.listener(|this, _, window, cx| this.set_prompt("", window, cx))),
            );
        let body = v_flex()
            .p_3()
            .gap_2()
            .child(
                h_flex()
                    .gap_4()
                    .justify_between()
                    .child(div().flex_1().min_w_0().child(hint(
                        "Added to the system prompt of every agent SuperGIT starts: \
                         Claude's appended system prompt, Codex's developer instructions \
                         (new threads).",
                        theme,
                    )))
                    .child(div().flex_none().child(buttons)),
            )
            .child(Textarea::new(&self.prompt))
            .child(hint(
                "Placeholders: {worktree}, {branch}, {repo}, {repo_name}. \
                 {#linked}…{/linked} shows only in a linked worktree, {#main}…{/main} \
                 only in the main checkout, {#folder}…{/folder} only in a folder \
                 without Git.",
                theme,
            ))
            .when_some(preview, |body, (name, prompt)| {
                body.child(
                    div()
                        .pt_1()
                        .text_xs()
                        .font_weight(FontWeight::MEDIUM)
                        .child(format!("Preview for {name}")),
                )
                .child(
                    div()
                        .p_2()
                        .rounded_md()
                        .border_1()
                        .border_color(theme.border)
                        .text_xs()
                        .text_color(theme.muted_foreground)
                        .child(if prompt.is_empty() {
                            "Nothing is added.".to_string()
                        } else {
                            prompt
                        }),
                )
            });
        group("Initial prompt", [body.into_any_element()], None, theme)
    }

    fn notifications(&self, cx: &mut Context<Self>) -> Vec<AnyElement> {
        let theme = cx.theme().clone();
        let settings = crate::settings::load();
        let mut page: Vec<AnyElement> = [true, false]
            .map(|needs_you| {
                let banner_on = if needs_you {
                    settings.notify_approvals
                } else {
                    settings.notify_completions
                };
                let view = cx.entity().downgrade();
                let banner = Switch::new(if needs_you {
                    "banner-approvals"
                } else {
                    "banner-completions"
                })
                .checked(banner_on)
                .tooltip(match (needs_you, banner_on) {
                    (true, true) => "Stop showing banners when an agent needs you",
                    (true, false) => "Show a banner when an agent needs you",
                    (false, true) => "Stop showing banners when an agent finishes",
                    (false, false) => "Show a banner when an agent finishes",
                })
                .on_click(move |on, _, cx| {
                    let _ = view.update(cx, |this, cx| this.set_banner(needs_you, *on, cx));
                });
                group(
                    if needs_you {
                        "Approvals and questions"
                    } else {
                        "Completions and failures"
                    },
                    [
                        row(
                            "Sound",
                            None,
                            self.sound_picker(needs_you, settings.sound(needs_you), cx),
                            &theme,
                        ),
                        row(
                            "Banner",
                            Some("The system's notification".into()),
                            banner,
                            &theme,
                        ),
                    ],
                    Some(if needs_you {
                        "When an agent needs approval or an answer."
                    } else {
                        "When an agent finishes its turn or fails."
                    }),
                    &theme,
                )
            })
            .into();
        page.push(hint(
            if cfg!(target_os = "macos") {
                "For conversations SuperGIT runs, chat or terminal; the one on screen stays \
                 quiet. Sounds play even in a Focus. If macOS blocks banners, enable SuperGIT \
                 in System Settings → Notifications."
            } else {
                "For conversations SuperGIT runs, chat or terminal; the one on screen stays \
                 quiet. Sounds come from your sound theme (canberra-gtk-play, else pw-play or \
                 paplay). Banners go to your desktop's notification server; Do Not Disturb \
                 hides them."
            },
            &theme,
        ));
        page
    }

    /// The sound's menu: None, then the platform's sounds; a pick plays at
    /// once.
    fn sound_picker(
        &self,
        needs_you: bool,
        current: Option<&str>,
        cx: &mut Context<Self>,
    ) -> impl IntoElement + use<> {
        let theme = cx.theme().clone();
        let view = cx.entity().downgrade();
        let current = current.map(str::to_owned);
        let value = current
            .as_deref()
            .map_or_else(|| "None".to_string(), crate::sound::label);
        // A pop-up button, as macOS draws one: the value and ⌃⌄.
        Button::new(if needs_you {
            "sound-approval"
        } else {
            "sound-completion"
        })
        .small()
        .outline()
        .child(
            h_flex().gap_1().items_center().child(value).child(
                Icon::new(IconName::ChevronsUpDown)
                    .xsmall()
                    .text_color(theme.muted_foreground),
            ),
        )
        .tooltip(if needs_you {
            "Pick the sound for an agent that needs approval or an answer"
        } else {
            "Pick the sound for an agent that finishes or fails"
        })
        .dropdown_menu(move |mut menu, _, _| {
            menu = menu.scrollable(true).max_h(px(440.));
            let pick = |name: Option<&'static str>| {
                let view = view.clone();
                move |_: &ClickEvent, _: &mut Window, cx: &mut App| {
                    super::set_sound(needs_you, name);
                    let _ = view.update(cx, |_, cx| cx.notify());
                }
            };
            menu = menu
                .item(
                    PopupMenuItem::new("None")
                        .checked(current.is_none())
                        .on_click(pick(None)),
                )
                .separator();
            for &name in crate::sound::SOUNDS {
                let label = if name == crate::sound::default(needs_you) {
                    format!("{} (default)", crate::sound::label(name))
                } else {
                    crate::sound::label(name)
                };
                menu = menu.item(
                    PopupMenuItem::new(label)
                        .checked(current.as_deref() == Some(name))
                        .on_click(pick(Some(name))),
                );
            }
            menu
        })
    }

    fn diffs(&self, app: &Entity<SuperGitApp>, cx: &mut Context<Self>) -> Vec<AnyElement> {
        let theme = cx.theme().clone();
        let state = app.read(cx);
        let available = state.difft_available();
        let on = state.difftastic_active();
        let current = state.diff_layout;
        let status = match &state.difft_version {
            None => "Looking for difftastic…".to_string(),
            Some(None) => "difft was not found on PATH. Install it with `brew install \
                           difftastic`, then reopen SuperGIT."
                .to_string(),
            Some(Some(version)) => format!(
                "difftastic {version} compares syntax trees: only changed tokens are \
                 highlighted, and reformatting shows as no change."
            ),
        };
        let weak = self.app.clone();
        let layouts = [DiffLayout::Unified, DiffLayout::Split];
        let layout = ButtonGroup::new("diff-layout")
            .outline()
            .small()
            .children(layouts.map(|layout| {
                Button::new(SharedString::from(format!("diff-layout-{layout:?}")))
                    .label(match layout {
                        DiffLayout::Unified => "Unified",
                        DiffLayout::Split => "Split",
                    })
                    .selected(current == layout)
                    .tooltip(layout_tooltip(layout))
            }))
            .on_click(move |clicked: &Vec<usize>, _, cx| {
                if let Some(&ix) = clicked.first() {
                    Self::with_app(&weak, cx, |app, cx| app.set_diff_layout(layouts[ix], cx));
                }
            });
        let weak = self.app.clone();
        let difftastic = Switch::new("difftastic")
            .checked(on)
            .disabled(!available)
            .tooltip(if !available {
                "difftastic is not installed: brew install difftastic"
            } else if on {
                "Go back to line-by-line diffs"
            } else {
                "Highlight only changed syntax and ignore reformatting"
            })
            .on_click(move |on, _, cx| {
                let on = *on;
                Self::with_app(&weak, cx, |app, cx| app.set_difftastic(on, cx));
            });
        vec![group(
            "Diffs",
            [
                row(
                    "Layout",
                    Some("Also in the bar above each diff".into()),
                    layout,
                    &theme,
                ),
                row(
                    "Show diffs with difftastic",
                    Some(status),
                    difftastic,
                    &theme,
                ),
            ],
            None,
            &theme,
        )]
    }

    fn repository(&self, app: &Entity<SuperGitApp>, cx: &mut Context<Self>) -> Vec<AnyElement> {
        let theme = cx.theme().clone();
        let state = app.read(cx);
        let Some(root) = state.repo.as_ref().map(|repo| repo.root.clone()) else {
            return vec![
                div()
                    .py_8()
                    .text_center()
                    .text_sm()
                    .text_color(theme.muted_foreground)
                    .child("Open a repository to set up its worktree cleanup.")
                    .into_any_element(),
            ];
        };
        let view = state.repository_view();
        let auto_cleanup = view.auto_cleanup;
        let protected_branches = view.protected_branches.clone();
        let mut branches = protected_branches.clone();
        branches.extend(
            state
                .worktrees
                .iter()
                .filter_map(|info| info.wt.branch.clone()),
        );
        branches.sort();
        branches.dedup();
        let weak = self.app.clone();
        let auto = Switch::new("auto-cleanup")
            .checked(auto_cleanup)
            .tooltip(if auto_cleanup {
                "Stop removing worktrees on their own after their PR merges"
            } else {
                "Remove a worktree and its local branch once its PR merges"
            })
            .on_click(move |_, _, cx| {
                Self::with_app(&weak, cx, |app, cx| app.toggle_auto_cleanup(cx));
            });
        let protection = branches.into_iter().enumerate().map(|(ix, branch)| {
            let protected = protected_branches.contains(&branch);
            let weak = self.app.clone();
            let toggle = branch.clone();
            row(
                branch,
                None,
                Switch::new(("protected-branch", ix))
                    .checked(protected)
                    .tooltip(if protected {
                        "Let cleanup delete this branch again"
                    } else {
                        "Keep cleanup from ever deleting this branch"
                    })
                    .on_click(move |_, _, cx| {
                        Self::with_app(&weak, cx, |app, cx| {
                            app.toggle_cleanup_protection(&toggle, cx)
                        });
                    }),
                &theme,
            )
        });
        let protection: Vec<AnyElement> = protection.collect();
        let pending = app.update(cx, |app, cx| app.render_pending_cleanup(cx));
        let mut page = vec![group(
            format!("Worktree cleanup · {}", project_name(&root)),
            [row(
                "Clean up after a PR merges",
                Some(
                    "Only PRs seen open while SuperGIT ran qualify. Selected, dirty, busy, \
                     kept, protected or unread work stays. Squash and rebase merges need a \
                     manual review."
                        .into(),
                ),
                auto,
                &theme,
            )],
            None,
            &theme,
        )];
        if !protection.is_empty() {
            page.push(group(
                "Protected local branches",
                protection,
                Some(
                    "Cleanup never deletes these. The main worktree and the default base are \
                     always protected.",
                ),
                &theme,
            ));
        }
        page.push(pending);
        page
    }

    fn toolbar(&self, theme: &gpui_kit::component::Theme, cx: &mut Context<Self>) -> Div {
        h_flex()
            .flex_none()
            .justify_center()
            .gap_1()
            .px_3()
            .pb_2()
            .border_b_1()
            .border_color(theme.border)
            .children(Page::ALL.map(|page| {
                let selected = page == self.page;
                v_flex()
                    .id(page.label())
                    .w(px(88.))
                    .py_1p5()
                    .gap_1()
                    .items_center()
                    .rounded_md()
                    .cursor_pointer()
                    .text_color(if selected {
                        theme.foreground
                    } else {
                        theme.muted_foreground
                    })
                    .when(selected, |tab| tab.bg(theme.secondary))
                    .hover(|tab| {
                        tab.bg(if selected {
                            theme.secondary
                        } else {
                            theme.list_hover
                        })
                    })
                    .tooltip(move |window, cx| Tooltip::new(page.about()).build(window, cx))
                    .tooltip_show_delay(crate::TOOLTIP_DELAY)
                    .on_click(cx.listener(move |this, _, _, cx| this.show(page, cx)))
                    .child(Icon::new(page.icon()).with_size(gpui_kit::component::Size::Large))
                    .child(div().text_xs().child(page.label()))
            }))
    }
}

impl Render for SettingsWindow {
    fn render(&mut self, _: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let theme = cx.theme().clone();
        let page = match self.app.upgrade() {
            Some(app) => match self.page {
                Page::General => self.general(cx),
                Page::Agents => self.agents(&app, cx),
                Page::Notifications => self.notifications(cx),
                Page::Diffs => self.diffs(&app, cx),
                Page::Repository => self.repository(&app, cx),
            },
            None => vec![hint("SuperGIT's window is closed.", &theme)],
        };
        v_flex()
            .size_full()
            .key_context(CONTEXT)
            .track_focus(&self.focus)
            .on_action(cx.listener(|_, _: &CloseSettings, window, _| window.remove_window()))
            .bg(theme.background)
            .text_color(theme.foreground)
            .child(
                TitleBar::new().child(
                    div()
                        .flex_1()
                        .flex()
                        .justify_center()
                        .text_sm()
                        .font_weight(FontWeight::SEMIBOLD)
                        .child("Settings"),
                ),
            )
            .child(self.toolbar(&theme, cx))
            .child(
                div()
                    .id("settings-page")
                    .flex_1()
                    .min_h_0()
                    .overflow_y_scroll()
                    .track_scroll(&self.scroll)
                    .child(
                        v_flex()
                            .w_full()
                            .max_w(px(COLUMN))
                            .mx_auto()
                            .px_5()
                            .py_5()
                            .gap_5()
                            .children(page)
                            .when_some(self.problem.clone(), |column, problem| {
                                column
                                    .child(div().text_sm().text_color(theme.danger).child(problem))
                            }),
                    ),
            )
    }
}

/// A titled box of rows, lines between them, and a note under it.
fn group(
    title: impl Into<SharedString>,
    rows: impl IntoIterator<Item = AnyElement>,
    note: Option<&'static str>,
    theme: &gpui_kit::component::Theme,
) -> AnyElement {
    v_flex()
        .gap_1p5()
        .child(
            div()
                .px_1()
                .text_sm()
                .font_weight(FontWeight::SEMIBOLD)
                .child(title.into()),
        )
        .child(
            v_flex()
                .rounded_lg()
                .border_1()
                .border_color(theme.border)
                .children(rows.into_iter().enumerate().map(|(ix, row)| {
                    div()
                        .when(ix > 0, |row| row.border_t_1().border_color(theme.border))
                        .child(row)
                })),
        )
        .when_some(note, |group, note| {
            group.child(div().px_1().child(hint(note, theme)))
        })
        .into_any_element()
}

/// A setting: its name (and what it does) on the left, its control on the
/// right.
fn row(
    label: impl Into<SharedString>,
    about: Option<String>,
    control: impl IntoElement,
    theme: &gpui_kit::component::Theme,
) -> AnyElement {
    h_flex()
        .px_3()
        .py_2p5()
        .gap_4()
        .justify_between()
        .child(
            v_flex()
                .flex_1()
                .min_w_0()
                .gap_0p5()
                .child(div().text_sm().truncate().child(label.into()))
                .when_some(about, |text, about| text.child(hint(about, theme))),
        )
        .child(div().flex_none().child(control))
        .into_any_element()
}

fn hint(text: impl Into<SharedString>, theme: &gpui_kit::component::Theme) -> AnyElement {
    div()
        .text_xs()
        .text_color(theme.muted_foreground)
        .child(text.into())
        .into_any_element()
}
