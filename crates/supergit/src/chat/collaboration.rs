use super::*;

impl ChatSession {
    fn set_codex_mode(&mut self, wanted: CollaborationMode, cx: &mut Context<Self>) {
        if self.convo.status.is_busy() || self.history_loading() {
            return;
        }
        if let Backend::Codex { collaboration, .. } = &mut self.backend
            && wanted != collaboration.unwrap_or_default()
        {
            *collaboration = Some(wanted);
            cx.notify();
        }
    }

    pub(super) fn render_codex_collaboration(&self, cx: &mut Context<Self>) -> Option<AnyElement> {
        let Backend::Codex { collaboration, .. } = &self.backend else {
            return None;
        };
        let current = collaboration.unwrap_or_default();
        let disabled = self.convo.status.is_busy() || self.history_loading();
        let muted = cx.theme().muted_foreground;
        Some(
            h_flex()
                .gap_1()
                .items_center()
                .child(div().text_xs().text_color(muted).child("Mode"))
                .children(CollaborationMode::ALL.map(|mode| {
                    let (label, help) = match mode {
                        CollaborationMode::Default => (
                            "Default",
                            "Work on your request with the selected permissions",
                        ),
                        CollaborationMode::Plan => {
                            ("Plan", "Explore the worktree and propose a plan with Codex")
                        }
                    };
                    Button::new(("codex-collaboration", mode as usize))
                        .label(label)
                        .xsmall()
                        .when(mode == current, |button| button.secondary())
                        .when(mode != current, |button| button.ghost())
                        .disabled(disabled)
                        .tooltip(help)
                        .on_click(cx.listener(move |this, _, _, cx| this.set_codex_mode(mode, cx)))
                }))
                .into_any_element(),
        )
    }
}
