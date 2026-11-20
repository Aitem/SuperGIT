//! The agent's side of the chat as a timeline: a line down the left with a
//! dot per step, and runs of tool calls folded into one row ("Bash ×5").
//!
//! Each transcript item is one row of the virtualized message list, so the
//! layout is decided per row from its neighbours: which rows take no space,
//! which tool call heads a run, and where the line starts and ends.

use std::collections::HashSet;
use std::ops::Range;

use sg_agents::chat::{InputResolution, Item};

/// How a transcript row is drawn.
#[derive(Clone, Debug, PartialEq, Eq)]
pub(super) enum Row {
    /// Takes no space: thinking, the later calls of a folded run.
    Hidden,
    User,
    /// One step on the agent's timeline. `first`/`last`: the line starts or
    /// ends at this step's dot (a user turn, or nothing, is next to it).
    Step {
        shape: Shape,
        first: bool,
        last: bool,
    },
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub(super) enum Shape {
    Single,
    /// The first call of a run of tool calls spanning `items[run]`.
    Run(Range<usize>),
    /// A later call of an unfolded run.
    InRun,
}

/// Thinking is not a row (the line above the composer says "Thinking…"
/// while it lasts) and does not break a run.
fn quiet(item: &Item) -> bool {
    matches!(item, Item::Thinking { .. })
}

/// Whether a subagent made the call: its calls fold among themselves, not
/// with the agent's own.
fn nested(item: &Item) -> bool {
    matches!(item, Item::Tool(tool) if tool.parent.is_some())
}

/// The run of tool calls around `ix`: at least two calls `groupable`
/// accepts, all the agent's own or all its subagents', with nothing but
/// thinking between them.
pub(super) fn run_at(
    items: &[Item],
    ix: usize,
    groupable: &dyn Fn(&Item) -> bool,
) -> Option<Range<usize>> {
    let anchor = items.get(ix)?;
    if !groupable(anchor) {
        return None;
    }
    let inner = nested(anchor);
    let groupable = |item: &Item| groupable(item) && nested(item) == inner;
    let fits = |i: usize| groupable(&items[i]) || quiet(&items[i]);
    let mut start = ix;
    while start > 0 && fits(start - 1) {
        start -= 1;
    }
    while !groupable(&items[start]) {
        start += 1;
    }
    let mut end = ix + 1;
    while end < items.len() && fits(end) {
        end += 1;
    }
    while !groupable(&items[end - 1]) {
        end -= 1;
    }
    let calls = (start..end).filter(|&i| groupable(&items[i])).count();
    (calls >= 2).then_some(start..end)
}

/// The step row `ix` draws, `None` when it is hidden or a user turn.
fn shape(
    items: &[Item],
    ix: usize,
    groupable: &dyn Fn(&Item) -> bool,
    open: &HashSet<usize>,
) -> Option<Shape> {
    let item = items.get(ix)?;
    if quiet(item) || matches!(item, Item::User { .. }) {
        return None;
    }
    Some(match run_at(items, ix, groupable) {
        Some(run) if run.start == ix => Shape::Run(run),
        Some(run) if open.contains(&run.start) => Shape::InRun,
        Some(_) => return None,
        None => Shape::Single,
    })
}

/// Is the nearest visible row before (`back`) or after `ix` a user turn,
/// or is there none? Folded runs are skipped as a whole.
fn turn_edge(
    items: &[Item],
    ix: usize,
    back: bool,
    groupable: &dyn Fn(&Item) -> bool,
    open: &HashSet<usize>,
) -> bool {
    let mut j = ix;
    loop {
        if back {
            if j == 0 {
                return true;
            }
            j -= 1;
        } else {
            j += 1;
            if j >= items.len() {
                return true;
            }
        }
        if matches!(items[j], Item::User { .. }) {
            return true;
        }
        if shape(items, j, groupable, open).is_some() {
            return false;
        }
        // A hidden call of a folded run: its first call is visible.
        if let Some(run) = run_at(items, j, groupable) {
            if back {
                return false;
            }
            j = run.end - 1;
        }
    }
}

/// How row `ix` is drawn. `groupable`: tool calls that may fold into a run
/// (not edits, which show their diff); `open`: runs (by first index) the
/// user unfolded.
pub(super) fn row(
    items: &[Item],
    ix: usize,
    groupable: &dyn Fn(&Item) -> bool,
    open: &HashSet<usize>,
) -> Row {
    match items.get(ix) {
        Some(Item::User { .. }) => Row::User,
        Some(_) => match shape(items, ix, groupable, open) {
            Some(shape) => Row::Step {
                first: turn_edge(items, ix, true, groupable, open),
                last: turn_edge(items, ix, false, groupable, open),
                shape,
            },
            None => Row::Hidden,
        },
        None => Row::Hidden,
    }
}

/// Is the text at `ix` the agent's last word of its turn (a turn's end, a
/// user turn or — once the agent is idle — nothing follows it)? That one
/// carries the answer's time; the remarks before it do not.
pub(super) fn closes_turn(items: &[Item], ix: usize, busy: bool) -> bool {
    for item in items.iter().skip(ix + 1) {
        match item {
            Item::User { .. } | Item::TurnEnd { .. } => return true,
            Item::Thinking { .. } | Item::Notice { .. } => {}
            Item::Permission(p) if p.decision.is_some() => {}
            Item::UserInput(request) if request.resolution != InputResolution::Pending => {}
            _ => return false,
        }
    }
    !busy
}

/// "Bash ×3, Read ×2": the run's tools by name, in order of first use.
pub(super) fn run_label<'a>(names: impl IntoIterator<Item = &'a str>) -> String {
    let mut counts: Vec<(&str, usize)> = Vec::new();
    for name in names {
        match counts.iter_mut().find(|(counted, _)| *counted == name) {
            Some((_, n)) => *n += 1,
            None => counts.push((name, 1)),
        }
    }
    counts
        .iter()
        .map(|(name, n)| match n {
            1 => (*name).to_string(),
            n => format!("{name} ×{n}"),
        })
        .collect::<Vec<_>>()
        .join(", ")
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::Value;
    use sg_agents::chat::ToolCall;

    fn tool(name: &str) -> Item {
        Item::Tool(ToolCall {
            id: name.into(),
            name: name.into(),
            input: Value::Null,
            result: None,
            parent: None,
            checking: false,
        })
    }

    /// A call a subagent made.
    fn inner(name: &str) -> Item {
        match tool(name) {
            Item::Tool(call) => Item::Tool(ToolCall {
                parent: Some("agent".into()),
                ..call
            }),
            item => item,
        }
    }

    fn end() -> Item {
        Item::TurnEnd {
            duration_ms: None,
            is_error: false,
            message: None,
        }
    }

    fn user() -> Item {
        Item::User {
            text: "go".into(),
            at: None,
        }
    }

    fn text() -> Item {
        Item::Text {
            text: "ok".into(),
            streaming: false,
            at: None,
        }
    }

    fn not_edit(item: &Item) -> bool {
        matches!(item, Item::Tool(t) if t.name != "Edit")
    }

    fn step(shape: Shape, first: bool, last: bool) -> Row {
        Row::Step { shape, first, last }
    }

    #[test]
    fn runs_fold_and_the_line_spans_each_turn() {
        let items = [
            user(),                              // 0
            text(),                              // 1
            tool("Bash"),                        // 2
            Item::Thinking { streaming: false }, // 3
            tool("Bash"),                        // 4
            tool("Read"),                        // 5
            text(),                              // 6
            user(),                              // 7
            tool("Edit"),                        // 8
            Item::TurnEnd {
                duration_ms: None,
                is_error: false,
                message: None,
            }, // 9
        ];
        let folded = HashSet::new();
        let rows: Vec<_> = (0..items.len())
            .map(|ix| row(&items, ix, &not_edit, &folded))
            .collect();
        assert_eq!(rows[0], Row::User);
        assert_eq!(rows[1], step(Shape::Single, true, false));
        assert_eq!(rows[2], step(Shape::Run(2..6), false, false));
        assert_eq!(rows[3..6], [Row::Hidden, Row::Hidden, Row::Hidden]);
        assert_eq!(rows[6], step(Shape::Single, false, true));
        assert_eq!(rows[7], Row::User);
        assert_eq!(rows[8], step(Shape::Single, true, false));
        assert_eq!(rows[9], step(Shape::Single, false, true));

        let open = HashSet::from([2]);
        assert_eq!(
            row(&items, 4, &not_edit, &open),
            step(Shape::InRun, false, false)
        );
        assert_eq!(
            row(&items, 5, &not_edit, &open),
            step(Shape::InRun, false, false)
        );
        assert_eq!(row(&items, 3, &not_edit, &open), Row::Hidden);
    }

    #[test]
    fn a_lone_call_or_an_edit_does_not_fold() {
        let items = [user(), tool("Bash"), text(), tool("Edit"), tool("Edit")];
        assert_eq!(run_at(&items, 1, &not_edit), None);
        assert_eq!(run_at(&items, 3, &not_edit), None);
        assert_eq!(
            row(&items, 1, &not_edit, &HashSet::new()),
            step(Shape::Single, true, false)
        );
    }

    #[test]
    fn labels_count_tools_in_order_of_first_use() {
        assert_eq!(
            run_label(["Bash", "Read", "Bash", "Bash", "Grep"]),
            "Bash ×3, Read, Grep"
        );
    }

    #[test]
    fn thinking_is_never_a_row() {
        let items = [
            user(),
            Item::Thinking { streaming: true },
            tool("Bash"),
            Item::Thinking { streaming: true },
        ];
        let none = HashSet::new();
        assert_eq!(row(&items, 1, &not_edit, &none), Row::Hidden);
        assert_eq!(row(&items, 3, &not_edit, &none), Row::Hidden);
        // The call is the turn's only row: the line starts and ends at it.
        assert_eq!(
            row(&items, 2, &not_edit, &none),
            step(Shape::Single, true, true)
        );
    }

    #[test]
    fn a_subagents_calls_fold_apart_from_the_agents_own() {
        let items = [
            user(),        // 0
            tool("Read"),  // 1
            inner("Bash"), // 2
            inner("Read"), // 3
            inner("Bash"), // 4
            tool("Bash"),  // 5
            tool("Grep"),  // 6
        ];
        assert_eq!(run_at(&items, 1, &not_edit), None);
        assert_eq!(run_at(&items, 3, &not_edit), Some(2..5));
        assert_eq!(run_at(&items, 6, &not_edit), Some(5..7));
        let none = HashSet::new();
        assert_eq!(
            row(&items, 2, &not_edit, &none),
            step(Shape::Run(2..5), false, false)
        );
        assert_eq!(
            row(&items, 5, &not_edit, &none),
            step(Shape::Run(5..7), false, true)
        );
    }

    #[test]
    fn only_the_last_text_of_a_turn_closes_it() {
        let items = [
            user(),                              // 0
            text(),                              // 1: a remark
            tool("Bash"),                        // 2
            text(),                              // 3: the answer
            Item::Thinking { streaming: false }, // 4
            end(),                               // 5
            user(),                              // 6
            text(),                              // 7
            Item::Notice {
                text: "Interrupted".into(),
                is_error: false,
            }, // 8
        ];
        assert!(!closes_turn(&items, 1, false));
        assert!(closes_turn(&items, 3, false));
        // Nothing follows: the answer, unless the agent is still at it.
        assert!(closes_turn(&items, 7, false));
        assert!(!closes_turn(&items, 7, true));
    }
}
