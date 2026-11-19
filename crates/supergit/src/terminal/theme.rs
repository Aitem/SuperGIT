//! The embedded terminal in the app's colors, and Claude Code's TUI in the
//! same colors.
//!
//! The terminal's default colors and 16-color palette come from the GPUI
//! theme (and follow it live). Claude Code draws in its own truecolor
//! palette, so SuperGIT also writes two Claude themes built from the app's
//! colors, shipped as a tiny plugin it passes with `--plugin-dir`, and picks
//! one per launch with `--settings <file>` (`{"theme": "custom:supergit:dark"}`).
//! Nothing in `~/.claude` changes. Claude loads plugin themes a moment after
//! start: until then (the welcome banner) it uses its built-in dark theme.

use std::path::PathBuf;

use gpui_kit::component::{ActiveTheme as _, ThemeColor};
use gpui_kit::*;
use serde_json::{Map, Value, json};
use sg_terminal::{Colors, Rgb};

/// The plugin's name: its themes are `supergit:light` and `supergit:dark`.
const PLUGIN: &str = "supergit";

fn rgb(color: Hsla) -> Rgb {
    let c = Rgba::from(color);
    let channel = |v: f32| (v.clamp(0., 1.) * 255.).round() as u8;
    Rgb::new(channel(c.r), channel(c.g), channel(c.b))
}

/// `over` drawn at `alpha` on top of `base`, as one opaque color.
fn blend(base: Rgb, over: Hsla, alpha: f32) -> Rgb {
    let over = rgb(over);
    let mix = |b: u8, o: u8| (f32::from(b) * (1. - alpha) + f32::from(o) * alpha).round() as u8;
    Rgb::new(
        mix(base.r, over.r),
        mix(base.g, over.g),
        mix(base.b, over.b),
    )
}

fn hex(c: Rgb) -> String {
    format!("#{:06x}", c.to_u32())
}

/// What programs in the terminal see, from the app's current theme.
pub fn terminal_colors(cx: &App) -> Colors {
    let theme = cx.theme();
    colors_for(&theme.colors, theme.is_dark())
}

fn colors_for(c: &ThemeColor, dark: bool) -> Colors {
    let background = rgb(c.background);
    let foreground = rgb(c.foreground);
    // Neutrals keep their usual roles: black is dark and white is light in
    // either appearance, so programs that pick "white on black" stay legible.
    let (black, bright_black, white, bright_white) = if dark {
        (
            blend(background, c.muted, 1.),
            rgb(c.muted_foreground),
            blend(foreground, c.muted_foreground, 0.35),
            foreground,
        )
    } else {
        (
            foreground,
            rgb(c.muted_foreground),
            blend(background, c.border, 1.),
            blend(background, c.muted, 1.),
        )
    };
    let (red, green, yellow, blue, magenta, cyan) =
        (c.red, c.green, c.yellow, c.blue, c.magenta, c.cyan);
    let (bright_red, bright_green, bright_yellow, bright_blue, bright_magenta, bright_cyan) =
        if dark {
            (
                c.red_light,
                c.green_light,
                c.yellow_light,
                c.blue_light,
                c.magenta_light,
                c.cyan_light,
            )
        } else {
            // The light theme's `*_light` tints are pale backgrounds; bright text
            // on white needs the full colors.
            (red, green, yellow, blue, magenta, cyan)
        };
    Colors {
        background,
        foreground,
        cursor: rgb(c.caret),
        ansi: [
            black,
            rgb(red),
            rgb(green),
            rgb(yellow),
            rgb(blue),
            rgb(magenta),
            rgb(cyan),
            white,
            bright_black,
            rgb(bright_red),
            rgb(bright_green),
            rgb(bright_yellow),
            rgb(bright_blue),
            rgb(bright_magenta),
            rgb(bright_cyan),
            bright_white,
        ],
        dark,
    }
}

/// A Claude Code theme (`{name, base, overrides}`) from the app's colors.
/// Only keys Claude knows are written; Claude's own orange stays.
fn claude_theme(c: &ThemeColor, dark: bool) -> Value {
    let bg = rgb(c.background);
    let fg = rgb(c.foreground);
    let muted_fg = rgb(c.muted_foreground);
    let border = blend(bg, c.border, 1.);
    let muted_bg = blend(bg, c.muted, 1.);
    let blue = hex(rgb(c.blue));
    let magenta = hex(rgb(c.magenta));
    let cyan = hex(rgb(c.cyan));
    let success = hex(rgb(c.success));
    let danger = hex(rgb(c.danger));
    let warning = hex(rgb(c.warning));
    // As the app's diff view: 12% of success/danger behind changed lines,
    // 30% behind the changed words.
    let added = blend(bg, c.success, 0.12);
    let removed = blend(bg, c.danger, 0.12);
    let mut overrides = Map::new();
    let mut set = |key: &str, value: String| {
        overrides.insert(key.into(), Value::String(value));
    };
    set("text", hex(fg));
    set("inverseText", hex(bg));
    set("inactive", hex(muted_fg));
    set("inactiveShimmer", hex(fg));
    set("subtle", hex(border));
    set("promptBorder", hex(muted_fg));
    set("promptBorderShimmer", hex(fg));
    set("permission", blue.clone());
    set("permissionShimmer", blue.clone());
    set("suggestion", blue.clone());
    set("remember", blue.clone());
    set("ide", blue.clone());
    set("briefLabelYou", blue.clone());
    set("rate_limit_fill", blue.clone());
    set("rate_limit_empty", hex(border));
    set("claudeBlue_FOR_SYSTEM_SPINNER", blue.clone());
    set("claudeBlueShimmer_FOR_SYSTEM_SPINNER", blue);
    set("autoAccept", magenta.clone());
    set("autoAcceptShimmer", magenta.clone());
    set("skill", magenta.clone());
    set("merged", magenta.clone());
    set("effortUltra", magenta.clone());
    set("bashBorder", magenta);
    set("planMode", cyan.clone());
    set("background", cyan);
    set("success", success);
    set("error", danger);
    set("warning", warning.clone());
    set("warningShimmer", warning);
    set("diffAdded", hex(added));
    set("diffRemoved", hex(removed));
    set("diffAddedDimmed", hex(blend(bg, c.success, 0.06)));
    set("diffRemovedDimmed", hex(blend(bg, c.danger, 0.06)));
    set("diffAddedWord", hex(blend(added, c.success, 0.3)));
    set("diffRemovedWord", hex(blend(removed, c.danger, 0.3)));
    set("userMessageBackground", hex(muted_bg));
    set(
        "userMessageBackgroundHover",
        hex(blend(bg, c.list_hover, 1.)),
    );
    set("composerSidebarBackground", hex(muted_bg));
    set("bashMessageBackgroundColor", hex(muted_bg));
    set("memoryBackgroundColor", hex(muted_bg));
    set("selectionBg", hex(blend(bg, c.selection, 1.)));
    let mode = if dark { "dark" } else { "light" };
    json!({
        "name": format!("SuperGIT {}", if dark { "Dark" } else { "Light" }),
        "base": mode,
        "overrides": overrides,
    })
}

/// Where the plugin and the per-launch settings live.
fn plugin_dir() -> Option<PathBuf> {
    Some(crate::settings::support_dir()?.join("claude-terminal"))
}

/// Write (or refresh) the theme plugin and the settings file that selects
/// the theme for the current appearance. Returns the arguments to add to
/// `claude`; empty when the files cannot be written (Claude then uses the
/// user's own theme).
pub fn claude_args(cx: &App) -> Vec<String> {
    match write_claude_theme(cx) {
        Ok((plugin, settings)) => vec![
            "--plugin-dir".into(),
            plugin.display().to_string(),
            "--settings".into(),
            settings.display().to_string(),
        ],
        Err(_) => Vec::new(),
    }
}

/// Point running and future Claude sessions at the theme for the current
/// appearance (Claude reads the settings file again when it changes).
pub fn refresh_claude_theme(cx: &App) {
    let _ = write_claude_theme(cx);
}

fn write_claude_theme(cx: &App) -> anyhow::Result<(PathBuf, PathBuf)> {
    let dir = plugin_dir().ok_or_else(|| anyhow::anyhow!("no support directory"))?;
    let plugin = dir.join("plugin");
    std::fs::create_dir_all(plugin.join(".claude-plugin"))?;
    std::fs::create_dir_all(plugin.join("themes"))?;
    write_if_changed(
        &plugin.join(".claude-plugin/plugin.json"),
        &json!({
            "name": PLUGIN,
            "version": env!("CARGO_PKG_VERSION"),
            "description": "SuperGIT's colors for Claude Code in its embedded terminal",
            "license": "MIT",
        }),
    )?;
    let theme = cx.theme();
    let dark = theme.is_dark();
    // The current appearance from the live theme, the other from defaults.
    let (light_colors, dark_colors) = if dark {
        (ThemeColor::light(), std::sync::Arc::new(theme.colors))
    } else {
        (std::sync::Arc::new(theme.colors), ThemeColor::dark())
    };
    write_if_changed(
        &plugin.join("themes/light.json"),
        &claude_theme(&light_colors, false),
    )?;
    write_if_changed(
        &plugin.join("themes/dark.json"),
        &claude_theme(&dark_colors, true),
    )?;
    let settings = dir.join("settings.json");
    let mode = if dark { "dark" } else { "light" };
    write_if_changed(
        &settings,
        &json!({ "theme": format!("custom:{PLUGIN}:{mode}") }),
    )?;
    Ok((plugin, settings))
}

fn write_if_changed(path: &std::path::Path, value: &Value) -> anyhow::Result<()> {
    let text = serde_json::to_string_pretty(value)?;
    if std::fs::read_to_string(path).ok().as_deref() != Some(text.as_str()) {
        std::fs::write(path, text)?;
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::{Rgb, ThemeColor, claude_theme, colors_for};

    #[test]
    fn claude_themes_use_only_known_keys_and_valid_colors() {
        for (colors, dark) in [(ThemeColor::light(), false), (ThemeColor::dark(), true)] {
            let theme = claude_theme(&colors, dark);
            assert_eq!(theme["base"], if dark { "dark" } else { "light" });
            for (key, value) in theme["overrides"].as_object().unwrap() {
                assert!(KNOWN_KEYS.contains(&key.as_str()), "unknown key {key}");
                let v = value.as_str().unwrap();
                assert!(v.len() == 7 && v.starts_with('#'), "{key}: {v}");
            }
        }
    }

    #[test]
    fn terminal_neutrals_keep_their_roles() {
        for (colors, dark) in [(ThemeColor::light(), false), (ThemeColor::dark(), true)] {
            let c = colors_for(&colors, dark);
            let luma = |c: Rgb| u32::from(c.r) + u32::from(c.g) + u32::from(c.b);
            // Black darker than white, in both appearances.
            assert!(luma(c.ansi[0]) < luma(c.ansi[7]));
            assert!(luma(c.ansi[8]) < luma(c.ansi[15]) || !dark);
            assert_eq!(c.dark, dark);
        }
    }

    /// Claude Code 2.1's theme keys (overrides of unknown keys are ignored).
    const KNOWN_KEYS: &[&str] = &[
        "autoAccept",
        "autoAcceptShimmer",
        "skill",
        "bashBorder",
        "claude",
        "claudeShimmer",
        "claudeBlue_FOR_SYSTEM_SPINNER",
        "claudeBlueShimmer_FOR_SYSTEM_SPINNER",
        "permission",
        "permissionShimmer",
        "planMode",
        "ide",
        "promptBorder",
        "promptBorderShimmer",
        "text",
        "inverseText",
        "inactive",
        "inactiveShimmer",
        "subtle",
        "suggestion",
        "remember",
        "background",
        "success",
        "error",
        "warning",
        "merged",
        "warningShimmer",
        "diffAdded",
        "diffRemoved",
        "diffAddedDimmed",
        "diffRemovedDimmed",
        "diffAddedWord",
        "diffRemovedWord",
        "userMessageBackground",
        "userMessageBackgroundHover",
        "composerSidebarBackground",
        "selectionBg",
        "bashMessageBackgroundColor",
        "memoryBackgroundColor",
        "rate_limit_fill",
        "rate_limit_empty",
        "effortUltra",
        "briefLabelYou",
    ];
}
