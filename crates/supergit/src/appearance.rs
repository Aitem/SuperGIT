//! Light / dark / system appearance. The choice is an app-wide global, saved
//! to settings, and in `System` mode the theme follows macOS live.

use gpui_kit::component::{Theme, ThemeColor, ThemeMode};
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
    // A mode change loads the theme's own colors: patch them after it.
    Theme::update(cx, |theme| {
        let dark = theme.mode.is_dark();
        readable_status_colors(&mut theme.colors, dark);
    });
}

/// Status colors readable as text and on filled buttons (WCAG AA, 4.5:1).
/// gpui-kit's light ones are made for fills and fail as text ("+4", "fork
/// point", errors): Tailwind's 700/600 shades instead. Its dark ones are
/// light, so white on a filled danger or success button fails: the
/// button's text takes the background's color.
pub fn readable_status_colors(colors: &mut ThemeColor, dark: bool) {
    if dark {
        colors.danger_foreground = colors.background;
        colors.success_foreground = colors.background;
    } else {
        colors.success = rgb(0x15803d).into();
        colors.danger = rgb(0xdc2626).into();
        colors.warning = rgb(0xb45309).into();
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
