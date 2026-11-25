//! WCAG 2.2 contrast of the theme's colors: the ratio between two colors'
//! relative luminances, (L1 + 0.05) / (L2 + 0.05). Level AA asks 4.5:1 for
//! text (1.4.3) and 3:1 for the parts of controls and states people need to
//! see (1.4.11).

use gpui_kit::{Hsla, Rgba};

/// Text against its background (WCAG 1.4.3, AA).
pub const TEXT: f32 = 4.5;
/// Control boundaries, icons and state marks (WCAG 1.4.11, AA).
pub const NON_TEXT: f32 = 3.0;

fn channel(value: f32) -> f32 {
    if value <= 0.04045 {
        value / 12.92
    } else {
        ((value + 0.055) / 1.055).powf(2.4)
    }
}

/// Relative luminance of `color` drawn over `under` (alpha blended).
fn luminance(color: Hsla, under: Hsla) -> f32 {
    let (top, bottom): (Rgba, Rgba) = (color.into(), under.into());
    let blend = |a: f32, b: f32| a * top.a + b * (1. - top.a);
    0.2126 * channel(blend(top.r, bottom.r))
        + 0.7152 * channel(blend(top.g, bottom.g))
        + 0.0722 * channel(blend(top.b, bottom.b))
}

/// The contrast ratio of `fg` drawn over the opaque `bg` (1 to 21).
pub fn ratio(fg: Hsla, bg: Hsla) -> f32 {
    let bg = Hsla { a: 1., ..bg };
    let (a, b) = (luminance(fg, bg), luminance(bg, bg));
    let (light, dark) = if a > b { (a, b) } else { (b, a) };
    (light + 0.05) / (dark + 0.05)
}

#[cfg(test)]
mod tests {
    use super::*;
    use gpui_kit::component::ThemeColor;

    #[test]
    fn black_on_white_is_twenty_one_to_one() {
        let white = gpui_kit::white();
        let black = gpui_kit::black();
        assert!((ratio(black, white) - 21.).abs() < 0.01);
        assert!((ratio(white, white) - 1.).abs() < 0.01);
    }

    /// Below AA today: the hairline borders, which decorate rather than
    /// mark controls (inputs show their field by their placeholder and
    /// focus ring). Fixing one means taking it off this list.
    const BELOW_AA: [(&str, &str); 4] = [
        ("light", "border/background"),
        ("light", "input/background"),
        ("dark", "border/background"),
        ("dark", "input/background"),
    ];

    /// The pairs the app draws with, in both appearances: those that meet
    /// AA must keep meeting it.
    #[test]
    fn theme_pairs_meet_aa_or_are_listed() {
        let mut low = Vec::new();
        // The palettes as the app shows them (`appearance` patches them).
        let mut light = *ThemeColor::light();
        crate::appearance::readable_status_colors(&mut light, false);
        let mut dark = *ThemeColor::dark();
        crate::appearance::readable_status_colors(&mut dark, true);
        for (name, c) in [("light", light), ("dark", dark)] {
            for (pair, fg, bg, min) in [
                ("foreground/background", c.foreground, c.background, TEXT),
                (
                    "muted_foreground/background",
                    c.muted_foreground,
                    c.background,
                    TEXT,
                ),
                (
                    "muted_foreground/sidebar",
                    c.muted_foreground,
                    c.sidebar,
                    TEXT,
                ),
                ("foreground/list_active", c.foreground, c.list_active, TEXT),
                (
                    "primary_foreground/primary",
                    c.primary_foreground,
                    c.primary,
                    TEXT,
                ),
                ("danger/background", c.danger, c.background, TEXT),
                ("success/background", c.success, c.background, TEXT),
                ("warning/background", c.warning, c.background, TEXT),
                ("link/background", c.link, c.background, TEXT),
                (
                    "danger_foreground/danger",
                    c.danger_foreground,
                    c.danger,
                    TEXT,
                ),
                (
                    "success_foreground/success",
                    c.success_foreground,
                    c.success,
                    TEXT,
                ),
                ("border/background", c.border, c.background, NON_TEXT),
                ("input/background", c.input, c.background, NON_TEXT),
            ] {
                let r = ratio(fg, bg);
                if r < min && !BELOW_AA.contains(&(name, pair)) {
                    low.push(format!("{name} {pair}: {r:.2} < {min}"));
                }
            }
        }
        assert!(low.is_empty(), "below WCAG AA: {low:#?}");
    }
}
