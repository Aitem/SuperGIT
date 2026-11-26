//! Renaming a conversation in place in the sidebar tree, as VS Code renames
//! a file: its title becomes a field with the text selected; Return or a
//! click elsewhere keeps the name, Escape leaves it as it was. An empty name
//! gives the conversation the agent's own title back. The name is
//! SuperGIT's (`SavedConversation::name`): the agent's transcript is left
//! alone.

use super::*;

/// The conversation whose title is a field now.
pub(super) struct Rename {
    root: PathBuf,
    worktree: PathBuf,
    local_id: String,
    input: Entity<InputState>,
    _subscription: Subscription,
}

impl Rename {
    pub(super) fn is(&self, worktree: &Path, local_id: &str) -> bool {
        self.worktree == worktree && self.local_id == local_id
    }
}

impl SuperGitApp {
    /// Turn the conversation's title in the tree into a field holding
    /// `current`, all of it selected.
    pub(super) fn start_rename(
        &mut self,
        root: PathBuf,
        worktree: PathBuf,
        local_id: String,
        current: String,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let input = cx.new(|cx| InputState::new(window, cx).default_value(current));
        input.update(cx, |input, cx| {
            input.select_all(window, cx);
            input.focus(window, cx);
        });
        let subscription =
            cx.subscribe_in(
                &input,
                window,
                |this, _, event: &InputEvent, _, cx| match event {
                    // A click elsewhere keeps the name, as Return does.
                    InputEvent::PressEnter { .. } | InputEvent::Blur => {
                        this.finish_rename(true, cx)
                    }
                    InputEvent::Change | InputEvent::Focus => {}
                },
            );
        self.renaming = Some(Rename {
            root,
            worktree,
            local_id,
            input,
            _subscription: subscription,
        });
        cx.notify();
    }

    /// End the rename: `keep` the typed name (Return, a click elsewhere) or
    /// not (Escape).
    pub(super) fn finish_rename(&mut self, keep: bool, cx: &mut Context<Self>) {
        let Some(rename) = self.renaming.take() else {
            return;
        };
        if keep {
            let name = rename.input.read(cx).value().trim().to_string();
            self.rename_conversation(&rename.root, &rename.worktree, &rename.local_id, name, cx);
        }
        cx.notify();
    }

    /// The field in place of a conversation's title, if it is the one being
    /// renamed. Shifted left by most of its padding and border, so the text
    /// stays about where the title was and the focus ring clears the mark.
    pub(super) fn rename_field(
        &self,
        worktree: &Path,
        local_id: &str,
        cx: &mut Context<Self>,
    ) -> Option<AnyElement> {
        let rename = self
            .renaming
            .as_ref()
            .filter(|r| r.is(worktree, local_id))?;
        Some(
            div()
                .flex_1()
                .min_w_0()
                .ml(px(-3.))
                // The row's click would open the conversation and take the
                // focus away.
                .on_mouse_down(MouseButton::Left, |_, _, cx| cx.stop_propagation())
                // A click elsewhere keeps the name, as in VS Code (a click
                // on something that takes no focus would not blur it).
                .on_mouse_down_out(cx.listener(|this, _, _, cx| this.finish_rename(true, cx)))
                .capture_key_down(cx.listener(|this, event: &KeyDownEvent, _, cx| {
                    if event.keystroke.key == "escape" {
                        this.finish_rename(false, cx);
                        cx.stop_propagation();
                    }
                }))
                .child(Input::new(&rename.input).xsmall())
                .into_any_element(),
        )
    }

    /// Name a conversation, live or saved; a blank name clears it.
    fn rename_conversation(
        &mut self,
        root: &Path,
        worktree: &Path,
        local_id: &str,
        name: String,
        cx: &mut Context<Self>,
    ) {
        let name = Some(name).filter(|name| !name.is_empty());
        let live = self.chats.get(worktree).and_then(|chats| {
            chats
                .iter()
                .find(|chat| chat.read(cx).local_id() == local_id)
                .cloned()
        });
        if let Some(chat) = live {
            chat.update(cx, |chat, cx| chat.set_name(name, cx));
        } else if let Some(saved) = self
            .persisted
            .repositories
            .get_mut(root)
            .and_then(|view| view.conversations.get_mut(worktree))
            .and_then(|saved| saved.iter_mut().find(|c| c.local_id == local_id))
        {
            saved.name = name;
        }
        self.queue_persist(cx);
    }
}
