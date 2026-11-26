//! Settings ▸ Notifications: for agents that need the user (approvals,
//! questions) and agents that finished (done, failed), a sound and a
//! banner (the system's notification). The conversation on screen stays
//! quiet (`watch_chat`).

use super::*;
use gpui_kit::component::menu::DropdownMenu as _;

impl SuperGitApp {
    pub(super) fn render_notification_settings(&self, cx: &mut Context<Self>) -> impl IntoElement {
        let settings = crate::settings::load();
        let theme = cx.theme().clone();
        v_flex()
            .gap_2()
            .child(
                div()
                    .font_weight(FontWeight::SEMIBOLD)
                    .child("Notifications"),
            )
            .children([true, false].map(|needs_you| {
                h_flex()
                    .justify_between()
                    .gap_2()
                    .child(div().min_w_0().truncate().child(if needs_you {
                        "Approvals and questions"
                    } else {
                        "Completion and failures"
                    }))
                    .child(
                        h_flex()
                            .flex_none()
                            .gap_1()
                            .child(self.sound_picker(needs_you, settings.sound(needs_you), cx))
                            .child(self.banner_toggle(
                                needs_you,
                                if needs_you {
                                    settings.notify_approvals
                                } else {
                                    settings.notify_completions
                                },
                                cx,
                            )),
                    )
            }))
            .child(div().text_sm().text_color(theme.muted_foreground).child(
                if cfg!(target_os = "macos") {
                    "For conversations SuperGIT runs, chat or terminal; the one on \
                         screen stays quiet. Sounds play even in a Focus. If macOS blocks \
                         banners, enable SuperGIT in System Settings → Notifications."
                } else {
                    "For conversations SuperGIT runs, chat or terminal; the one on \
                         screen stays quiet. Sounds come from your sound theme \
                         (canberra-gtk-play, else pw-play or paplay). Banners go to your \
                         desktop's notification server; Do Not Disturb hides them."
                },
            ))
    }

    /// The sound's dropdown: None, then the platform's sounds; a pick
    /// plays at once.
    fn sound_picker(
        &self,
        needs_you: bool,
        current: Option<&str>,
        cx: &mut Context<Self>,
    ) -> impl IntoElement {
        let theme = cx.theme().clone();
        let owner = cx.entity().downgrade();
        let current = current.map(str::to_owned);
        let value = current
            .as_deref()
            .map_or_else(|| "None".to_string(), crate::sound::label);
        crate::chat::setting_trigger(
            if needs_you {
                "sound-approval"
            } else {
                "sound-completion"
            },
            "Sound",
            value,
            false,
            &theme,
        )
        .tooltip(if needs_you {
            "Pick the sound for an agent that needs approval or an answer"
        } else {
            "Pick the sound for an agent that finishes or fails"
        })
        .dropdown_menu(move |mut menu, _, _| {
            menu = menu.scrollable(true).max_h(px(440.));
            let pick = |name: Option<&'static str>| {
                let owner = owner.clone();
                move |_: &ClickEvent, _: &mut Window, cx: &mut App| {
                    let _ = owner.update(cx, |this, cx| this.set_sound(needs_you, name, cx));
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

    /// "Banner On/Off": the system notification. Its value sits in a fixed
    /// slot, so switching it moves nothing.
    fn banner_toggle(&self, needs_you: bool, on: bool, cx: &mut Context<Self>) -> Button {
        let theme = cx.theme();
        Button::new(if needs_you {
            "notify-approval"
        } else {
            "notify-completion"
        })
        .small()
        .ghost()
        .child(
            h_flex()
                .gap_1()
                .child(div().text_color(theme.muted_foreground).child("Banner"))
                .child(div().w(px(24.)).text_color(theme.foreground).child(if on {
                    "On"
                } else {
                    "Off"
                })),
        )
        .tooltip(match (needs_you, on) {
            (true, true) => "Stop showing banners when an agent needs approval or an answer",
            (true, false) => "Show a banner when an agent needs approval or an answer",
            (false, true) => "Stop showing banners when an agent finishes or fails",
            (false, false) => "Show a banner when an agent finishes or fails",
        })
        .on_click(cx.listener(move |this, _, _, cx| this.toggle_notification(needs_you, cx)))
    }

    fn set_sound(&mut self, needs_you: bool, name: Option<&str>, cx: &mut Context<Self>) {
        let mut settings = crate::settings::load();
        settings.set_sound(needs_you, name);
        crate::settings::save(&settings);
        if let Some(name) = name {
            crate::sound::preview(name);
        }
        cx.notify();
    }
}
