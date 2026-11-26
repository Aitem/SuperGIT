//! The chat's visual vocabulary: the sizes, marks and small blocks its rows
//! are built from, so that rows agree with each other.
//!
//! - Text: prose (the user's and the agent's) is `text_sm`; the agent's
//!   steps (tool calls, runs) are a size under it ([`STEP_TEXT`]); times,
//!   notices and code are `text_xs`.
//! - Color: prose in the text color, everything else muted. Blue, green,
//!   red and yellow only mark a step's state on the timeline.
//! - Shape: cards have `rounded_lg`, the blocks inside them and under tool
//!   rows `rounded_md`.

use std::time::{Duration, SystemTime};

use gpui_kit::assets::IconName;
use gpui_kit::component::tooltip::Tooltip;
use gpui_kit::component::{Icon, Sizable as _, Theme, h_flex};
use gpui_kit::prelude::*;
use gpui_kit::*;
use sg_agents::chat::{InputResolution, Item, ToolCall};

/// Width of the timeline column left of the agent's steps.
pub(super) const GUTTER: f32 = 18.;
/// The conversation's widest column: its rows, the activity line and the
/// composer; a wider panel centers it. Wide enough for a diff of 100-column
/// code (rustfmt's limit) beside its line numbers, so edits read without
/// scrolling sideways. Prose then runs past the 80 characters WCAG 2.2
/// gives readable text (1.4.8), a trade made for the diffs. In rems, so it
/// follows the text size.
pub(super) const COLUMN_MAX: Rems = rems(60.);
/// Text size of the agent's steps: a size under its prose.
pub(super) const STEP_TEXT: f32 = 13.;
/// Max lines of tool output shown when a tool row is expanded.
pub(super) const OUTPUT_LINES: usize = 40;
/// Max characters of them: one line can be a megabyte of minified JSON.
const OUTPUT_CHARS: usize = 16_000;
/// Lines of a long prompt shown until it is unfolded.
pub(super) const PROMPT_LINES: usize = 12;

/// `content` in the conversation's column: the panel's width up to
/// [`COLUMN_MAX`], centered past it.
pub(super) fn column(content: impl IntoElement) -> Div {
    h_flex()
        .flex_none()
        .w_full()
        .justify_center()
        .child(div().flex_1().min_w_0().max_w(COLUMN_MAX).child(content))
}

/// What marks a step on the timeline.
#[derive(Clone, Copy)]
pub(super) enum Mark {
    Dot { color: Hsla, size: f32 },
    Icon { name: IconName, color: Hsla },
}

impl Mark {
    /// The same mark for a step inside an unfolded run: a smaller dot.
    pub(super) fn in_run(self) -> Self {
        match self {
            Self::Dot { color, .. } => Self::Dot { color, size: 5. },
            icon => icon,
        }
    }
}

/// A step's mark: tool calls and prompts by state, a turn's end by how it
/// ended, messages and notices a quiet dot.
pub(super) fn step_mark(item: &Item, busy: bool, theme: &Theme) -> Mark {
    let state = |color| Mark::Dot { color, size: 7. };
    match item {
        Item::Tool(tool) => state(tool_color(tool, busy, theme)),
        Item::Permission(p) if p.decision.is_none() => state(theme.warning),
        Item::UserInput(request) if request.resolution == InputResolution::Pending => {
            state(theme.warning)
        }
        Item::Plan(_) => state(theme.blue),
        Item::Notice { is_error: true, .. } => state(theme.danger),
        Item::TurnEnd { is_error: true, .. } => Mark::Icon {
            name: IconName::CircleX,
            color: theme.danger,
        },
        Item::TurnEnd { .. } => Mark::Icon {
            name: IconName::Check,
            color: theme.success,
        },
        _ => Mark::Dot {
            color: quiet(theme),
            size: 5.,
        },
    }
}

/// A tool call's color: running, waiting for a safety check, done, failed;
/// quiet when its turn ended without a result (it was stopped).
pub(super) fn tool_color(tool: &ToolCall, busy: bool, theme: &Theme) -> Hsla {
    match &tool.result {
        None if tool.checking => theme.warning,
        None if busy => theme.blue,
        None => quiet(theme),
        Some(result) if result.is_error => theme.danger,
        Some(_) => theme.success,
    }
}

/// The color of marks that say nothing.
fn quiet(theme: &Theme) -> Hsla {
    theme.muted_foreground.opacity(0.5)
}

/// The timeline beside one step: the line through it (starting or ending at
/// its mark at a turn's edges) and the mark, level with the step's first line.
///
/// The line is a whole pixel wide and the dots have odd sizes around its
/// middle, so that every edge falls on a pixel (on 1x displays too).
pub(super) fn gutter(first: bool, last: bool, top: Pixels, mark: Mark, theme: &Theme) -> Div {
    // The middle of the line, and of a step's first line of text.
    let (x, y) = (GUTTER / 2. + 0.5, top + px(10.5));
    // An icon is not a point on the line: the line stops short of it.
    let gap = match mark {
        Mark::Dot { .. } => px(0.),
        Mark::Icon { .. } => px(8.),
    };
    let line = || {
        div()
            .absolute()
            .left(px(x - 0.5))
            .w(px(1.))
            .bg(theme.border)
    };
    div()
        .relative()
        .flex_none()
        .w(px(GUTTER))
        .when(!first, |el| el.child(line().top_0().h(y - gap)))
        .when(!last, |el| el.child(line().top(y + gap).bottom_0()))
        .child(match mark {
            Mark::Dot { color, size } => div()
                .absolute()
                .left(px(x - size / 2.))
                .top(y - px(size / 2.))
                .size(px(size))
                .rounded_full()
                .bg(color),
            Mark::Icon { name, color } => div()
                .absolute()
                .left(px(x - 6.))
                .top(y - px(6.))
                .child(Icon::new(name).xsmall().text_color(color)),
        })
}

/// A block of code or output: monospace on a quiet background.
pub(super) fn code_box(text: impl Into<SharedString>, color: Hsla, theme: &Theme) -> Div {
    div()
        .w_full()
        .min_w_0()
        .px_2p5()
        .py_2()
        .rounded_md()
        .bg(theme.muted)
        .text_xs()
        .line_height(relative(1.5))
        .font_family(theme.mono_font_family.clone())
        .text_color(color)
        .child(text.into())
}

/// At most [`OUTPUT_LINES`] lines of a tool's output: its start, or its end
/// when that is where the news is (a build, a test run).
pub(super) fn excerpt(text: &str, from_end: bool) -> String {
    let text = text.trim_end();
    let total = text.lines().count();
    let hidden = total.saturating_sub(OUTPUT_LINES);
    let lines = |n: usize| if n == 1 { "line" } else { "lines" };
    let shown = if hidden == 0 {
        text.to_string()
    } else if from_end {
        let tail: Vec<&str> = text.lines().skip(hidden).collect();
        format!("… {hidden} earlier {}\n{}", lines(hidden), tail.join("\n"))
    } else {
        let head: Vec<&str> = text.lines().take(OUTPUT_LINES).collect();
        format!("{}\n… {hidden} more {}", head.join("\n"), lines(hidden))
    };
    if shown.chars().count() <= OUTPUT_CHARS {
        return shown;
    }
    if from_end {
        let skip = shown.chars().count() - OUTPUT_CHARS;
        format!("…{}", shown.chars().skip(skip).collect::<String>())
    } else {
        format!("{}…", shown.chars().take(OUTPUT_CHARS).collect::<String>())
    }
}

/// How long a turn took: "4.2s", "38s", "2m 05s", "1h 02m".
pub(super) fn duration_text(duration: Duration) -> String {
    match duration.as_secs() {
        0..10 => format!("{:.1}s", duration.as_secs_f64()),
        secs => elapsed_text(Duration::from_secs(secs)),
    }
}

/// A running clock in whole seconds: "7s", "2m 05s", "1h 02m".
pub(super) fn elapsed_text(duration: Duration) -> String {
    match duration.as_secs() {
        secs @ 0..60 => format!("{secs}s"),
        secs @ 60..3600 => format!("{}m {:02}s", secs / 60, secs % 60),
        secs => format!("{}h {:02}m", secs / 3600, secs % 3600 / 60),
    }
}

/// 1234 → "1.2k": a token count as people read it.
pub(super) fn count_text(n: u64) -> String {
    match n {
        0..1000 => n.to_string(),
        1000..1_000_000 => format!("{:.1}k", n as f64 / 1000.),
        _ => format!("{:.1}M", n as f64 / 1_000_000.),
    }
}

/// Under a message: when it was sent (the full date on hover). The text
/// itself is selectable for copying.
pub(super) fn time_footer(
    (kind, ix): (&'static str, usize),
    at: Option<SystemTime>,
    theme: &Theme,
) -> impl IntoElement {
    let time = at.map(message_time);
    h_flex()
        .gap_2()
        .text_xs()
        .text_color(theme.muted_foreground)
        .when_some(time, |row, (short, full)| {
            row.child(
                div()
                    .id(SharedString::from(format!("time-{kind}-{ix}")))
                    .opacity(0.8)
                    .tooltip(move |window, cx| Tooltip::new(full.clone()).build(window, cx))
                    .tooltip_show_delay(crate::TOOLTIP_DELAY)
                    .child(short),
            )
        })
}

/// A message's time: short in the chat ("14:05" today, "18 Nov 14:05" this
/// year, "2025-11-18 14:05" before), and in full for the tooltip.
fn message_time(at: SystemTime) -> (String, String) {
    let at = chrono::DateTime::<chrono::Local>::from(at);
    let now = chrono::Local::now();
    let short = if at.date_naive() == now.date_naive() {
        at.format("%H:%M")
    } else if at.format("%Y").to_string() == now.format("%Y").to_string() {
        at.format("%-d %b %H:%M")
    } else {
        at.format("%Y-%m-%d %H:%M")
    };
    (
        short.to_string(),
        at.format("%A, %-d %B %Y, %H:%M:%S").to_string(),
    )
}

#[cfg(test)]
mod tests {
    // Not `super::*`: GPUI's own `test` attribute would shadow `#[test]`.
    use super::{OUTPUT_CHARS, count_text, duration_text, elapsed_text, excerpt};
    use std::time::Duration;

    #[test]
    fn durations_read_like_a_clock() {
        let text = |ms| duration_text(Duration::from_millis(ms));
        assert_eq!(text(4_230), "4.2s");
        assert_eq!(text(38_400), "38s");
        assert_eq!(text(125_000), "2m 05s");
        assert_eq!(text(3_725_400), "1h 02m");
        assert_eq!(elapsed_text(Duration::from_millis(7_900)), "7s");
        assert_eq!(elapsed_text(Duration::from_secs(60)), "1m 00s");
        assert_eq!(count_text(450), "450");
        assert_eq!(count_text(1_234), "1.2k");
        assert_eq!(count_text(2_500_000), "2.5M");
    }

    #[test]
    fn long_output_keeps_the_end_that_matters() {
        let text: String = (1..=45).map(|n| format!("line {n}\n")).collect();
        let head = excerpt(&text, false);
        assert!(head.starts_with("line 1\n") && head.ends_with("line 40\n… 5 more lines"));
        let tail = excerpt(&text, true);
        assert!(tail.starts_with("… 5 earlier lines\nline 6\n") && tail.ends_with("line 45"));
        assert_eq!(excerpt("one\ntwo\n\n", true), "one\ntwo");
        let one_more: String = (1..=41).map(|n| format!("{n}\n")).collect();
        assert!(excerpt(&one_more, true).starts_with("… 1 earlier line\n2\n"));
        // One endless line (minified JSON, Cyrillic included) is cut by
        // characters, from the side that is not shown.
        let endless = "ж".repeat(20_000);
        assert_eq!(excerpt(&endless, false).chars().count(), OUTPUT_CHARS + 1);
        assert!(excerpt(&endless, true).starts_with('…'));
    }
}
