//! Light / dark / system appearance. The choice is an app-wide global, saved
//! to settings, and in `System` mode the theme follows macOS live.

use gpui_kit::component::{Theme, ThemeMode};
use gpui_kit::*;

use crate::settings::{self, Appearance};

actions!(
    supergit,
    [UseSystemAppearance, UseLightAppearance, UseDarkAppearance]
);

struct AppearanceSetting(Appearance);

impl Global for AppearanceSetting {}

/// Load the saved appearance (`SUPERGIT_APPEARANCE` overrides it for this run
/// without saving), apply it and register the menu actions.
pub fn init(cx: &mut App) {
    let appearance = std::env::var("SUPERGIT_APPEARANCE")
        .ok()
        .and_then(|s| Appearance::parse(&s))
        .unwrap_or_else(|| settings::load().appearance);
    cx.set_global(AppearanceSetting(appearance));
    apply(appearance, None, cx);

    cx.on_action(|_: &UseSystemAppearance, cx| set(Appearance::System, cx));
    cx.on_action(|_: &UseLightAppearance, cx| set(Appearance::Light, cx));
    cx.on_action(|_: &UseDarkAppearance, cx| set(Appearance::Dark, cx));
}

pub fn current(cx: &App) -> Appearance {
    cx.try_global::<AppearanceSetting>()
        .map(|s| s.0)
        .unwrap_or_default()
}

/// Switch, persist, and refresh the menu checkmarks.
pub fn set(appearance: Appearance, cx: &mut App) {
    cx.set_global(AppearanceSetting(appearance));
    let mut s = settings::load();
    s.appearance = appearance;
    settings::save(&s);
    apply(appearance, None, cx);
    crate::set_app_menus(cx);
}

/// Called when macOS switches between light and dark.
pub fn system_appearance_changed(window: &mut Window, cx: &mut App) {
    if current(cx) == Appearance::System {
        apply(Appearance::System, Some(window), cx);
    }
}

fn apply(appearance: Appearance, window: Option<&mut Window>, cx: &mut App) {
    match appearance {
        Appearance::System => Theme::sync_system_appearance(window, cx),
        Appearance::Light => Theme::change(ThemeMode::Light, None, cx),
        Appearance::Dark => Theme::change(ThemeMode::Dark, None, cx),
    }
}

/// Menu entries with a checkmark on the current choice.
pub fn menu_items(cx: &App) -> Vec<MenuItem> {
    let now = current(cx);
    vec![
        MenuItem::action("System", UseSystemAppearance).checked(now == Appearance::System),
        MenuItem::action("Light", UseLightAppearance).checked(now == Appearance::Light),
        MenuItem::action("Dark", UseDarkAppearance).checked(now == Appearance::Dark),
    ]
}
