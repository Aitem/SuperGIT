use super::*;
use gpui_kit::base::Selectable as _;
use gpui_kit::component::button::ButtonGroup;

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
        // A segmented control in the composer's row, like claude.ai's
        // Chat | Cowork.
        Some(
            ButtonGroup::new("codex-collaboration")
                .outline()
                .small()
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
                        .selected(mode == current)
                        .disabled(disabled)
                        .tooltip(help)
                }))
                .on_click(cx.listener(|this, clicked: &Vec<usize>, _, cx| {
                    if let Some(&mode) = clicked
                        .first()
                        .and_then(|&ix| CollaborationMode::ALL.get(ix))
                    {
                        this.set_codex_mode(mode, cx);
                    }
                }))
                .into_any_element(),
        )
    }
}
