//! Right-click menus of the sidebar tree, one per kind of row: a repository
//! (its row is also its main checkout's), a linked worktree, a
//! conversation. What comparable tools offer there, fitted to SuperGIT:
//! start something new first, then open and copy, then the lasting changes,
//! the destructive one last. Rows of a project that is not open get only
//! what needs no open project (open, reveal, copy).

use std::rc::Rc;

use super::*;
use gpui_kit::component::menu::{ContextMenuExt as _, PopupMenu};

/// A worktree row the menu acts on.
pub(super) struct WorktreeTarget {
    pub path: PathBuf,
    pub branch: Option<String>,
    /// Its project is the open one: everything applies.
    pub active: bool,
    pub kept: bool,
    /// Rebase's label and whether it can run now (`rebase_menu_item`).
    pub rebase: Option<(String, bool)>,
}

/// A conversation row the menu acts on.
pub(super) struct ConversationTarget {
    pub root: PathBuf,
    pub worktree: PathBuf,
    pub local_id: String,
    pub provider: Provider,
    pub session_id: Option<String>,
    /// Where its session started (`--resume` runs there).
    pub cwd: PathBuf,
    pub transcript: Option<PathBuf>,
    pub can_fork: bool,
    /// What it is called now: Rename starts from it.
    pub title: String,
    pub unread: bool,
    pub working: bool,
    /// Its project is the open one: everything applies.
    pub active: bool,
}

type Run = dyn Fn(&mut SuperGitApp, &mut Window, &mut Context<SuperGitApp>);

/// An item that runs on the app.
fn item(
    owner: &WeakEntity<SuperGitApp>,
    label: impl Into<SharedString>,
    run: impl Fn(&mut SuperGitApp, &mut Window, &mut Context<SuperGitApp>) + 'static,
) -> PopupMenuItem {
    let owner = owner.clone();
    let run: Rc<Run> = Rc::new(run);
    PopupMenuItem::new(label).on_click(move |_, window, cx| {
        let run = run.clone();
        let _ = owner.update(cx, |this, cx| run(this, window, cx));
    })
}

/// The destructive item, in the danger color.
fn danger_item(
    owner: &WeakEntity<SuperGitApp>,
    label: &'static str,
    run: impl Fn(&mut SuperGitApp, &mut Window, &mut Context<SuperGitApp>) + 'static,
) -> PopupMenuItem {
    let owner = owner.clone();
    PopupMenuItem::element(move |_, cx| div().text_color(cx.theme().danger).child(label)).on_click(
        move |_, window, cx| {
            let _ = owner.update(cx, |this, cx| run(this, window, cx));
        },
    )
}

/// "Copy …": the text to the clipboard, and a word that it is there.
fn copy_item(
    owner: &WeakEntity<SuperGitApp>,
    label: &'static str,
    text: Option<String>,
    done: &'static str,
) -> PopupMenuItem {
    let enabled = text.is_some();
    item(owner, label, move |this, _, cx| {
        if let Some(text) = &text {
            cx.write_to_clipboard(ClipboardItem::new_string(text.clone()));
            this.flash(done, false, cx);
        }
    })
    .disabled(!enabled)
}

/// Open in Editor, Reveal in Finder and, in the open project, the terminal
/// panel's shell for `path`.
fn open_items(
    menu: PopupMenu,
    owner: &WeakEntity<SuperGitApp>,
    path: &Path,
    terminal: bool,
) -> PopupMenu {
    let (editor, reveal, shell) = (path.to_path_buf(), path.to_path_buf(), path.to_path_buf());
    menu.item(item(owner, "Open in Editor", move |this, _, cx| {
        let result = launch::open_in_editor(&editor);
        this.report(result, "Opened in editor", cx);
    }))
    .item(item(
        owner,
        format!("Reveal in {}", launch::FILE_MANAGER),
        move |this, _, cx| {
            let result = launch::reveal_in_finder(&reveal);
            this.report(result, &format!("Opened in {}", launch::FILE_MANAGER), cx);
        },
    ))
    .when(terminal, |menu| {
        menu.item(item(owner, "Open Terminal", move |this, window, cx| {
            this.open_terminal_in(shell.clone(), window, cx)
        }))
    })
}

/// "New Claude Conversation", "New Codex Conversation" in `path`.
fn new_conversation_items(
    mut menu: PopupMenu,
    owner: &WeakEntity<SuperGitApp>,
    path: &Path,
) -> PopupMenu {
    for provider in Provider::ALL {
        let path = path.to_path_buf();
        menu = menu.item(item(
            owner,
            format!("New {} Conversation", provider.label()),
            move |this, window, cx| this.new_chat_in(path.clone(), provider, window, cx),
        ));
    }
    menu
}

impl SuperGitApp {
    /// A repository's row, which stands for its main checkout too; a
    /// folder project's (`folder`), which has no worktrees to add.
    pub(super) fn repository_menu(
        &self,
        row: Stateful<Div>,
        root: &Path,
        main: Option<&Path>,
        active: bool,
        folder: bool,
        cx: &mut Context<Self>,
    ) -> AnyElement {
        let owner = cx.entity().downgrade();
        let root = root.to_path_buf();
        let main = main.map(Path::to_path_buf);
        row.context_menu(move |menu, _, _| {
            let checkout = main.clone().unwrap_or_else(|| root.clone());
            let new_root = root.clone();
            let remove_root = root.clone();
            let menu = if folder {
                menu
            } else {
                menu.item(item(&owner, "New Worktree…", move |this, window, cx| {
                    this.open_new_worktree(new_root.clone(), window, cx)
                }))
            };
            let menu = match &main {
                Some(main) if active => new_conversation_items(menu, &owner, main),
                _ => menu,
            };
            open_items(
                menu.separator(),
                &owner,
                &checkout,
                active && main.is_some(),
            )
            .item(copy_item(
                &owner,
                "Copy Path",
                Some(checkout.display().to_string()),
                "Copied the path",
            ))
            .separator()
            .item(item(&owner, "Remove from Projects", move |this, _, cx| {
                this.remove_project(remove_root.clone(), cx)
            }))
        })
        .into_any_element()
    }

    /// A linked worktree's row.
    pub(super) fn worktree_menu(
        &self,
        row: Stateful<Div>,
        target: WorktreeTarget,
        cx: &mut Context<Self>,
    ) -> AnyElement {
        let owner = cx.entity().downgrade();
        row.context_menu(move |menu, _, _| {
            let WorktreeTarget {
                path,
                branch,
                active,
                kept,
                rebase,
                ..
            } = &target;
            let mut menu = menu;
            if *active {
                menu = new_conversation_items(menu, &owner, path).separator();
                if let Some((label, enabled)) = rebase {
                    let rebase_path = path.clone();
                    menu = menu
                        .item(
                            item(&owner, label.clone(), move |this, window, cx| {
                                this.open_rebase_dialog(rebase_path.clone(), window, cx)
                            })
                            .disabled(!enabled),
                        )
                        .separator();
                }
            }
            let menu = open_items(menu, &owner, path, *active)
                .item(copy_item(
                    &owner,
                    "Copy Path",
                    Some(path.display().to_string()),
                    "Copied the path",
                ))
                .item(copy_item(
                    &owner,
                    "Copy Branch Name",
                    branch.clone(),
                    "Copied the branch name",
                ));
            if !*active {
                return menu;
            }
            let (keep_path, delete_path) = (path.clone(), path.clone());
            menu.separator()
                .item(
                    item(&owner, "Keep Active", move |this, _, cx| {
                        this.toggle_keep_path(keep_path.clone(), cx)
                    })
                    .checked(*kept),
                )
                // The cleanup preview says what goes and what would be lost.
                .item(danger_item(
                    &owner,
                    "Delete Worktree…",
                    move |this, window, cx| {
                        this.open_cleanup_preview(vec![delete_path.clone()], window, cx)
                    },
                ))
        })
        .into_any_element()
    }

    /// A conversation's row.
    pub(super) fn conversation_menu(
        &self,
        row: Stateful<Div>,
        target: ConversationTarget,
        cx: &mut Context<Self>,
    ) -> AnyElement {
        let owner = cx.entity().downgrade();
        let target = Rc::new(target);
        row.context_menu(move |menu, _, _| {
            let t = target.clone();
            let rename = t.clone();
            // At once: the menu gives the focus back on closing only when
            // its item left it where it was, and the field takes it.
            let mut menu = menu.item(item(&owner, "Rename…", move |this, window, cx| {
                this.start_rename(
                    rename.root.clone(),
                    rename.worktree.clone(),
                    rename.local_id.clone(),
                    rename.title.clone(),
                    window,
                    cx,
                )
            }));
            if t.active {
                let fork = t.clone();
                menu = menu.item(
                    item(&owner, "Fork Conversation", move |this, window, cx| {
                        this.fork_conversation(&fork.worktree, &fork.local_id, window, cx)
                    })
                    .disabled(!t.can_fork),
                );
            }
            if t.unread {
                let read = t.clone();
                menu = menu.item(item(&owner, "Mark as Read", move |this, _, cx| {
                    this.mark_conversation_read(&read.root, &read.worktree, &read.local_id, cx)
                }));
            }
            let resume = t
                .session_id
                .as_deref()
                .map(|id| launch::resume_command_in(t.provider, id, &t.cwd));
            let transcript = t.transcript.clone();
            let menu = menu
                .separator()
                .item(copy_item(
                    &owner,
                    "Copy Session ID",
                    t.session_id.clone(),
                    "Copied the session ID",
                ))
                .item(copy_item(
                    &owner,
                    "Copy Resume Command",
                    resume,
                    "Copied the command that continues it in a terminal",
                ))
                .item(
                    item(
                        &owner,
                        format!("Reveal Transcript in {}", launch::FILE_MANAGER),
                        move |this, _, cx| {
                            if let Some(file) = &transcript {
                                let result = launch::reveal_file(file);
                                this.report(
                                    result,
                                    &format!("Opened in {}", launch::FILE_MANAGER),
                                    cx,
                                );
                            }
                        },
                    )
                    .disabled(t.transcript.is_none()),
                );
            if !t.active {
                return menu;
            }
            // Closing stops its agent; its transcript stays where the agent
            // keeps it.
            let close = t.clone();
            menu.separator().item(item(
                &owner,
                if t.working {
                    "Stop and Close Conversation"
                } else {
                    "Close Conversation"
                },
                move |this, _, cx| {
                    this.close_conversation(&close.root, &close.worktree, &close.local_id, cx)
                },
            ))
        })
        .into_any_element()
    }

    /// Select `worktree` of the open project with its conversations ready;
    /// false when it can't be (a Git operation holds it).
    fn select_for_menu(
        &mut self,
        worktree: &Path,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> bool {
        let path = worktree.to_path_buf();
        self.select_worktree(path.clone(), cx);
        if self.selected.as_ref() != Some(&path) {
            return false;
        }
        self.ensure_chats(&path, window, cx);
        true
    }

    /// A conversation with `provider` in `worktree`, started at once.
    fn new_chat_in(
        &mut self,
        worktree: PathBuf,
        provider: Provider,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        self.tree_agent = None;
        if self.select_for_menu(&worktree, window, cx) {
            self.new_chat_with(provider, window, cx);
        }
    }

    /// The shell of `worktree` in the terminal panel.
    fn open_terminal_in(&mut self, worktree: PathBuf, window: &mut Window, cx: &mut Context<Self>) {
        self.tree_agent = None;
        if self.select_for_menu(&worktree, window, cx) {
            self.show_terminal(window, cx);
        }
    }

    /// A new conversation that continues a copy of this one, opened beside
    /// it; the original stays as it is.
    fn fork_conversation(
        &mut self,
        worktree: &Path,
        local_id: &str,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        if !self.select_for_menu(worktree, window, cx) {
            return;
        }
        let seed = self.chats.get(worktree).and_then(|chats| {
            let chat = chats
                .iter()
                .find(|chat| chat.read(cx).local_id() == local_id)?;
            Some(chat.read(cx).fork_seed(cx))
        });
        let seed = match seed {
            Some(Some(seed)) => seed,
            Some(None) => {
                self.flash("Nothing to fork yet: send a message first", true, cx);
                return;
            }
            None => {
                self.flash("This conversation is no longer available", true, cx);
                return;
            }
        };
        let path = worktree.to_path_buf();
        let chat = cx.new(|cx| ChatSession::restore(path.clone(), &seed, window, cx));
        let terminal = chat.read(cx).is_terminal();
        self.tree_agent = Some(chat.read(cx).local_id().to_string());
        self.add_chat(path, chat.clone(), window, cx);
        // A TUI starts at once, as a resumed one does.
        if terminal {
            chat.update(cx, |chat, cx| chat.start_terminal(window, cx));
        }
    }

    /// Stop a conversation's agent and take it off the tree and the
    /// worktree's column. Its transcript stays where the agent keeps it.
    fn close_conversation(
        &mut self,
        root: &Path,
        worktree: &Path,
        local_id: &str,
        cx: &mut Context<Self>,
    ) {
        if let Some(chats) = self.chats.get_mut(worktree) {
            if let Some(ix) = chats
                .iter()
                .position(|chat| chat.read(cx).local_id() == local_id)
            {
                // Dropping the session kills its agent process. An empty
                // list gets a fresh conversation when the worktree shows.
                chats.remove(ix);
                let active = self.active_chat.get(worktree).copied().unwrap_or(0);
                let next = if active > ix { active - 1 } else { active };
                self.active_chat.insert(
                    worktree.to_path_buf(),
                    next.min(chats.len().saturating_sub(1)),
                );
            }
        } else if let Some(saved) = self
            .persisted
            .repositories
            .get_mut(root)
            .and_then(|view| view.conversations.get_mut(worktree))
        {
            saved.retain(|conversation| conversation.local_id != local_id);
        }
        if self.tree_agent.as_deref() == Some(local_id) {
            self.tree_agent = None;
        }
        self.queue_persist(cx);
        cx.notify();
    }

    fn mark_conversation_read(
        &mut self,
        root: &Path,
        worktree: &Path,
        local_id: &str,
        cx: &mut Context<Self>,
    ) {
        if let Some(chats) = self.chats.get(worktree) {
            if let Some(chat) = chats
                .iter()
                .find(|chat| chat.read(cx).local_id() == local_id)
                .cloned()
            {
                chat.update(cx, |chat, cx| chat.mark_seen(cx));
            }
        } else if let Some(conversation) = self
            .persisted
            .repositories
            .get_mut(root)
            .and_then(|view| view.conversations.get_mut(worktree))
            .and_then(|saved| saved.iter_mut().find(|c| c.local_id == local_id))
        {
            conversation.attention.unread = false;
        }
        self.queue_persist(cx);
        cx.notify();
    }

    /// Screenshot builds only: `SUPERGIT_DEBUG_RIGHT_CLICK=x,y` right-clicks
    /// that window point (= PNG pixel) five seconds after startup, for
    /// checking context menus, and `…=x,y,mx,my` then clicks the menu item
    /// at `mx,my`; `SUPERGIT_DEBUG_DOUBLE_CLICK=x,y` double-clicks the point
    /// (renaming a conversation).
    #[cfg(feature = "screenshot")]
    pub(super) fn debug_right_click_once(&mut self, cx: &mut Context<Self>) {
        if std::env::var_os("SUPERGIT_DEBUG_CLICK_DONE").is_some() {
            return;
        }
        let (spec, button, clicks) = match (
            std::env::var("SUPERGIT_DEBUG_RIGHT_CLICK"),
            std::env::var("SUPERGIT_DEBUG_DOUBLE_CLICK"),
        ) {
            (Ok(spec), _) => (spec, MouseButton::Right, 1),
            (_, Ok(spec)) => (spec, MouseButton::Left, 2),
            _ => return,
        };
        let coords: Vec<f32> = spec
            .split(',')
            .filter_map(|value| value.trim().parse().ok())
            .collect();
        let (at, item) = match coords[..] {
            [x, y] => (point(px(x), px(y)), None),
            [x, y, mx, my] => (point(px(x), px(y)), Some(point(px(mx), px(my)))),
            _ => return,
        };
        // SAFETY: isolated screenshot builds only, on the main thread.
        unsafe { std::env::set_var("SUPERGIT_DEBUG_CLICK_DONE", "1") };
        let Some(window) = cx.windows().first().copied() else {
            return;
        };
        cx.spawn(async move |_, cx| {
            cx.background_executor().timer(Duration::from_secs(5)).await;
            let press = |steps: &mut Vec<PlatformInput>, at, button, clicks| {
                steps.push(PlatformInput::MouseMove(MouseMoveEvent {
                    position: at,
                    pressed_button: None,
                    modifiers: Modifiers::default(),
                }));
                for click_count in 1..=clicks {
                    steps.push(PlatformInput::MouseDown(MouseDownEvent {
                        button,
                        position: at,
                        modifiers: Modifiers::default(),
                        click_count,
                        first_mouse: false,
                    }));
                    steps.push(PlatformInput::MouseUp(MouseUpEvent {
                        button,
                        position: at,
                        modifiers: Modifiers::default(),
                        click_count,
                    }));
                }
            };
            let mut steps = Vec::new();
            press(&mut steps, at, button, clicks);
            if let Some(item) = item {
                press(&mut steps, item, MouseButton::Left, 1);
            }
            for step in steps {
                // An unfocused window draws no frames of its own.
                let _ = window.update(cx, |_, window, cx| {
                    window.draw(cx).clear(cx);
                    window.dispatch_event(step, cx);
                });
                cx.background_executor()
                    .timer(Duration::from_millis(60))
                    .await;
            }
            // A few more frames, so what the last click did (focus) settles.
            for _ in 0..3 {
                let _ = window.update(cx, |_, window, cx| window.draw(cx).clear(cx));
                cx.background_executor()
                    .timer(Duration::from_millis(60))
                    .await;
            }
        })
        .detach();
    }
}
