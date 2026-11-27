//! Settings in a window of their own, opened with ⌘, (SuperGIT ▸
//! Settings…) from any window. There is one: ⌘, again brings it forward,
//! ⌘W or Esc closes it. Pages: General, Agents, Notifications, Diffs,
//! Repository. Values are saved as they change (`settings.json`; a
//! repository's cleanup in its view), the initial prompt when its field
//! loses focus or the window closes.
//!
//! macOS draws it with AppKit (`native`): a preferences window with the
//! pages in its toolbar and the system's controls. Elsewhere GPUI draws the
//! same pages (`gpui_window`).

use super::*;

#[cfg_attr(
    target_os = "macos",
    expect(dead_code, reason = "macOS shows AppKit's window instead (native)")
)]
mod gpui_window;
#[cfg(target_os = "macos")]
mod native;

actions!(settings_window, [CloseSettings]);

/// The window's key context: ⌘W and Esc close it (main.rs).
pub(crate) const CONTEXT: &str = "Settings";

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub(crate) enum Page {
    #[default]
    General,
    Agents,
    Notifications,
    Diffs,
    Repository,
}

impl Page {
    const ALL: [Self; 5] = [
        Self::General,
        Self::Agents,
        Self::Notifications,
        Self::Diffs,
        Self::Repository,
    ];

    fn label(self) -> &'static str {
        match self {
            Self::General => "General",
            Self::Agents => "Agents",
            Self::Notifications => "Notifications",
            Self::Diffs => "Diffs",
            Self::Repository => "Repository",
        }
    }

    /// `SUPERGIT_DEBUG_SETTINGS=<page>` (screenshot builds): its label, any
    /// case; anything else is General.
    #[cfg(feature = "screenshot")]
    pub(crate) fn parse(name: &str) -> Self {
        Self::ALL
            .into_iter()
            .find(|page| page.label().eq_ignore_ascii_case(name))
            .unwrap_or_default()
    }
}

/// Show the settings window on `page` (else the page shown last), opening
/// it or bringing it forward. `focus`: false for screenshot runs, which must
/// not take the user's keystrokes. Returns GPUI's window, if it is one.
pub(crate) fn open(
    app: WeakEntity<SuperGitApp>,
    page: Option<Page>,
    focus: bool,
    cx: &mut App,
) -> Option<AnyWindowHandle> {
    #[cfg(target_os = "macos")]
    {
        native::open(app, page, focus, cx);
        None
    }
    #[cfg(not(target_os = "macos"))]
    gpui_window::open(app, page, focus, cx)
}

/// Screenshot builds on macOS: AppKit's settings window drawn to `path`.
#[cfg(all(feature = "screenshot", target_os = "macos"))]
pub(crate) fn capture(path: &str) -> anyhow::Result<()> {
    native::capture(path)
}

/// The initial prompt's template as edited: the default is stored as none,
/// so a later default reaches it.
fn save_prompt(text: &str) {
    let mut settings = crate::settings::load();
    if settings.initial_prompt() == text {
        return;
    }
    settings.initial_prompt = (text != crate::initial_prompt::DEFAULT).then(|| text.to_string());
    crate::settings::save(&settings);
}

/// The sound for agents that need the user (`needs_you`) or finished; a
/// pick plays at once.
fn set_sound(needs_you: bool, name: Option<&str>) {
    let mut settings = crate::settings::load();
    settings.set_sound(needs_you, name);
    crate::settings::save(&settings);
    if let Some(name) = name {
        crate::sound::preview(name);
    }
}

/// A banner on or off: turning one on asks the system first.
fn set_banner(needs_you: bool, on: bool, cx: &App) -> anyhow::Result<()> {
    if on {
        notifications::request_permission(cx)?;
    }
    let mut settings = crate::settings::load();
    if needs_you {
        settings.notify_approvals = on;
    } else {
        settings.notify_completions = on;
    }
    crate::settings::save(&settings);
    Ok(())
}
