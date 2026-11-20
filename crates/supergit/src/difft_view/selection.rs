//! Text selection in the difftastic view: drag to select, double-click a
//! word, `cmd-c` to copy, as in the built-in diff.
//!
//! The list is virtualized: rows come and go as it scrolls. So the whole
//! list is one participant of the window's selection ([`SelectionLayer`]),
//! every painted code line reports its layout to a shared [`Geometry`], and
//! the selection's ends are positions in one side's text (content keys).
//! Copying reads that text, not what is still on screen.
//!
//! A selection stays on the side it starts on: in Split a drag selects
//! within one column, in Unified it takes the old or the new lines.

use std::cell::RefCell;
use std::ops::Range;
use std::rc::Rc;
use std::sync::Arc;

use gpui_kit::base::{
    ScrollbarHandle as _, TextSelectionContentKey, TextSelectionHandle, TextSelectionRegistration,
    TextSelectionRun,
};
use gpui_kit::*;

use super::{DifftView, Prepared, Side};

/// A position in one side's text of a file: line and byte offset in it.
/// Positions order as the text reads (files in list order).
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord)]
pub(super) struct Pos {
    pub file: usize,
    pub line: usize,
    pub offset: usize,
}

const OFFSET_BITS: u32 = 20;
const LINE_BITS: u32 = 20;
const SIDE_BIT: u64 = 1 << (OFFSET_BITS + LINE_BITS);

/// A content key: file, side, line, offset, ordered as listed. Lines and
/// offsets past 2^20 share the last key (a megabyte line, a million lines).
fn encode(side: Side, pos: Pos) -> u64 {
    let field = |value: usize, bits: u32| (value as u64).min((1 << bits) - 1);
    (field(pos.file, 63 - OFFSET_BITS - LINE_BITS) << (OFFSET_BITS + LINE_BITS + 1))
        | if side == Side::New { SIDE_BIT } else { 0 }
        | (field(pos.line, LINE_BITS) << OFFSET_BITS)
        | field(pos.offset, OFFSET_BITS)
}

fn decode(key: u64) -> (Side, Pos) {
    let side = if key & SIDE_BIT == 0 {
        Side::Old
    } else {
        Side::New
    };
    let pos = Pos {
        file: (key >> (OFFSET_BITS + LINE_BITS + 1)) as usize,
        line: ((key >> OFFSET_BITS) & ((1 << LINE_BITS) - 1)) as usize,
        offset: (key & ((1 << OFFSET_BITS) - 1)) as usize,
    };
    (side, pos)
}

/// A painted code line as the selection sees it.
struct Run {
    side: Side,
    /// The line's start.
    at: Pos,
    text: SharedString,
    layout: TextLayout,
    bounds: Bounds<Pixels>,
}

/// What the current frame painted: the list's bounds and scroll offset, and
/// its code lines.
#[derive(Default)]
pub(super) struct Geometry {
    bounds: Bounds<Pixels>,
    scroll_offset: Point<Pixels>,
    runs: Vec<Run>,
}

impl Geometry {
    /// The position at `point` (the participant's content coordinates): on
    /// `side` when the selection already has one, else on the side of the
    /// line nearest to it.
    fn key_at(&self, point: Point<Pixels>, side: Option<Side>) -> Option<u64> {
        let point = point + self.bounds.origin + self.scroll_offset;
        let side = side.or_else(|| {
            self.runs
                .iter()
                .min_by(|a, b| distance(point, a.bounds).total_cmp(&distance(point, b.bounds)))
                .map(|run| run.side)
        })?;
        // The line whose band holds the point, else the closest one.
        let run = self
            .runs
            .iter()
            .filter(|run| run.side == side)
            .min_by(|a, b| {
                band_distance(point, a.bounds)
                    .cmp(&band_distance(point, b.bounds))
                    .then_with(|| distance(point, a.bounds).total_cmp(&distance(point, b.bounds)))
            })?;
        let offset = run
            .layout
            .index_for_position(point)
            .unwrap_or_else(|closest| closest)
            .min(run.text.len());
        Some(encode(run.side, Pos { offset, ..run.at }))
    }
}

/// Outside the line's rows (half-open: a caret at a row's top belongs to
/// it), then how far from its middle.
fn band_distance(point: Point<Pixels>, bounds: Bounds<Pixels>) -> (bool, u32) {
    let outside = point.y < bounds.top() || point.y >= bounds.bottom();
    let distance: f32 = (point.y - bounds.center().y).abs().into();
    (outside, distance.to_bits())
}

fn distance(point: Point<Pixels>, bounds: Bounds<Pixels>) -> f32 {
    let dx: f32 = (bounds.left() - point.x)
        .max(point.x - bounds.right())
        .max(px(0.))
        .into();
    let dy: f32 = (bounds.top() - point.y)
        .max(point.y - bounds.bottom())
        .max(px(0.))
        .into();
    dx * dx + dy * dy
}

/// The largest char boundary of `text` at or before `offset`.
fn floor_boundary(text: &str, offset: usize) -> usize {
    let mut offset = offset.min(text.len());
    while !text.is_char_boundary(offset) {
        offset -= 1;
    }
    offset
}

/// The current selection, start before end, on one side.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(super) struct Selected {
    pub side: Side,
    pub start: Pos,
    pub end: Pos,
}

impl Selected {
    /// The selection `handle` holds, when both ends are in its text.
    pub fn of(handle: &TextSelectionHandle, cx: &App) -> Option<Self> {
        let snapshot = handle.snapshot(cx)?;
        let this = Some(handle.entity_id());
        if snapshot.anchor().entity_id() != this || snapshot.cursor().entity_id() != this {
            return None;
        }
        let (side, anchor) = decode(snapshot.anchor().content_key()?.value());
        let (cursor_side, cursor) = decode(snapshot.cursor().content_key()?.value());
        (side == cursor_side && anchor != cursor).then(|| Self {
            side,
            start: anchor.min(cursor),
            end: anchor.max(cursor),
        })
    }

    /// The part of `text`, line `line` of `file`, the selection takes in:
    /// empty outside it.
    fn clip(&self, file: usize, line: usize, text: &str) -> Range<usize> {
        let here = (file, line);
        if here < (self.start.file, self.start.line) || here > (self.end.file, self.end.line) {
            return 0..0;
        }
        let start = if here == (self.start.file, self.start.line) {
            floor_boundary(text, self.start.offset)
        } else {
            0
        };
        let end = if here == (self.end.file, self.end.line) {
            floor_boundary(text, self.end.offset)
        } else {
            text.len()
        };
        start..end.max(start)
    }

    /// What to highlight in a painted line: `lines` are the side lines it
    /// shows (Unified shows an unchanged line once for both sides), `text`
    /// what it shows.
    pub fn in_line(
        &self,
        file: usize,
        lines: &[(Side, usize)],
        text: &str,
    ) -> Option<Range<usize>> {
        lines
            .iter()
            .filter(|(side, _)| *side == self.side)
            .map(|&(_, line)| self.clip(file, line, text))
            .find(|range| !range.is_empty())
    }

    /// The selected text: the side's lines between the two ends, including
    /// those folded away, one per line.
    pub fn text(&self, files: &[Arc<Prepared>]) -> String {
        let mut pieces: Vec<&str> = Vec::new();
        for file in self.start.file..=self.end.file {
            let Some(prepared) = files.get(file) else {
                break;
            };
            let lines = match self.side {
                Side::Old => &prepared.file.old,
                Side::New => &prepared.file.new,
            };
            let first = if file == self.start.file {
                self.start.line
            } else {
                0
            };
            for (ix, line) in lines.iter().enumerate().skip(first) {
                if (file, ix) > (self.end.file, self.end.line) {
                    break;
                }
                pieces.push(&line.text[self.clip(file, ix, &line.text)]);
            }
        }
        pieces.join("\n")
    }
}

/// Sets `color` as the background of `range` over `highlights` (ordered,
/// not overlapping, as `StyledText` takes them), keeping their other styles.
pub(super) fn overlay_background(
    highlights: Vec<(Range<usize>, HighlightStyle)>,
    range: Range<usize>,
    color: Hsla,
) -> Vec<(Range<usize>, HighlightStyle)> {
    let selected = |style: HighlightStyle| HighlightStyle {
        background_color: Some(color),
        ..style
    };
    let mut out = Vec::with_capacity(highlights.len() + 3);
    // Everything of `range` before this is in `out`.
    let mut covered = range.start;
    for (styled, style) in highlights {
        if styled.start > covered && covered < range.end {
            out.push((
                covered..styled.start.min(range.end),
                selected(Default::default()),
            ));
        }
        let start = styled.start.max(range.start);
        let end = styled.end.min(range.end);
        if start < end {
            if styled.start < start {
                out.push((styled.start..start, style));
            }
            out.push((start..end, selected(style)));
            if end < styled.end {
                out.push((end..styled.end, style));
            }
        } else {
            out.push((styled.clone(), style));
        }
        covered = covered.max(styled.end);
    }
    if covered < range.end {
        out.push((covered..range.end, selected(Default::default())));
    }
    out
}

/// A painted code line: its highlighted text, reported to the selection as
/// `lines` of `file`.
pub(super) struct CodeText {
    styled: StyledText,
    text: SharedString,
    file: usize,
    lines: Vec<(Side, usize)>,
    geometry: Rc<RefCell<Geometry>>,
}

impl CodeText {
    /// `styled` lays out `text`.
    pub fn new(
        styled: StyledText,
        text: SharedString,
        file: usize,
        lines: Vec<(Side, usize)>,
        geometry: Rc<RefCell<Geometry>>,
    ) -> Self {
        Self {
            styled,
            text,
            file,
            lines,
            geometry,
        }
    }
}

impl IntoElement for CodeText {
    type Element = Self;

    fn into_element(self) -> Self::Element {
        self
    }
}

impl Element for CodeText {
    type RequestLayoutState = ();
    type PrepaintState = ();

    fn id(&self) -> Option<ElementId> {
        None
    }

    fn source_location(&self) -> Option<&'static std::panic::Location<'static>> {
        None
    }

    fn request_layout(
        &mut self,
        id: Option<&GlobalElementId>,
        inspector_id: Option<&InspectorElementId>,
        window: &mut Window,
        cx: &mut App,
    ) -> (LayoutId, ()) {
        self.styled.request_layout(id, inspector_id, window, cx)
    }

    fn prepaint(
        &mut self,
        id: Option<&GlobalElementId>,
        inspector_id: Option<&InspectorElementId>,
        bounds: Bounds<Pixels>,
        _: &mut (),
        window: &mut Window,
        cx: &mut App,
    ) {
        self.styled
            .prepaint(id, inspector_id, bounds, &mut (), window, cx);
        let layout = self.styled.layout().clone();
        let mut geometry = self.geometry.borrow_mut();
        for &(side, line) in &self.lines {
            geometry.runs.push(Run {
                side,
                at: Pos {
                    file: self.file,
                    line,
                    offset: 0,
                },
                text: self.text.clone(),
                layout: layout.clone(),
                bounds,
            });
        }
    }

    fn paint(
        &mut self,
        id: Option<&GlobalElementId>,
        inspector_id: Option<&InspectorElementId>,
        bounds: Bounds<Pixels>,
        _: &mut (),
        _: &mut (),
        window: &mut Window,
        cx: &mut App,
    ) {
        self.styled
            .paint(id, inspector_id, bounds, &mut (), &mut (), window, cx);
    }
}

/// The selection's participant: the list, with every code line it painted.
pub(super) struct SelectionLayer {
    child: AnyElement,
    list: ListState,
    horizontal: ScrollHandle,
    handle: TextSelectionHandle,
    geometry: Rc<RefCell<Geometry>>,
}

impl SelectionLayer {
    pub fn new(
        child: impl IntoElement,
        list: ListState,
        horizontal: ScrollHandle,
        handle: TextSelectionHandle,
        geometry: Rc<RefCell<Geometry>>,
    ) -> Self {
        Self {
            child: child.into_any_element(),
            list,
            horizontal,
            handle,
            geometry,
        }
    }
}

impl IntoElement for SelectionLayer {
    type Element = Self;

    fn into_element(self) -> Self::Element {
        self
    }
}

impl Element for SelectionLayer {
    type RequestLayoutState = ();
    type PrepaintState = ();

    fn id(&self) -> Option<ElementId> {
        None
    }

    fn source_location(&self) -> Option<&'static std::panic::Location<'static>> {
        None
    }

    fn request_layout(
        &mut self,
        _: Option<&GlobalElementId>,
        _: Option<&InspectorElementId>,
        window: &mut Window,
        cx: &mut App,
    ) -> (LayoutId, ()) {
        (self.child.request_layout(window, cx), ())
    }

    fn prepaint(
        &mut self,
        _: Option<&GlobalElementId>,
        _: Option<&InspectorElementId>,
        bounds: Bounds<Pixels>,
        _: &mut (),
        window: &mut Window,
        cx: &mut App,
    ) {
        {
            let mut geometry = self.geometry.borrow_mut();
            geometry.runs.clear();
            geometry.bounds = bounds;
        }
        // Behind the rows: their buttons still take clicks.
        let hitbox = window.insert_hitbox(bounds, HitboxBehavior::Normal);
        self.child.prepaint(window, cx);
        let mut offset = self.list.offset();
        offset.x = self.horizontal.offset().x;
        let text_bounds = {
            let mut geometry = self.geometry.borrow_mut();
            geometry.scroll_offset = offset;
            geometry.runs.iter().map(|run| run.bounds).collect()
        };
        self.handle.register(
            TextSelectionRegistration::new(hitbox, bounds)
                .with_scroll_offset(offset)
                .with_text_bounds(text_bounds)
                .with_rendered_element(&self.handle, window, cx),
            window,
            cx,
        );
    }

    fn paint(
        &mut self,
        _: Option<&GlobalElementId>,
        _: Option<&InspectorElementId>,
        _: Bounds<Pixels>,
        _: &mut (),
        _: &mut (),
        window: &mut Window,
        cx: &mut App,
    ) {
        let runs: Vec<TextSelectionRun> = self
            .geometry
            .borrow()
            .runs
            .iter()
            .map(|run| {
                TextSelectionRun::new(run.text.clone(), run.layout.clone(), run.bounds)
                    .with_document_order(encode(run.side, run.at))
            })
            .collect();
        self.handle.update_runs(&runs, cx);
        self.child.paint(window, cx);
    }
}

/// The view's selection participant: its ends resolve to positions in the
/// text, on the side of the anchor once there is one, and it copies
/// [`DifftView::selected_text`].
pub(super) fn handle(
    view: WeakEntity<DifftView>,
    geometry: &Rc<RefCell<Geometry>>,
    cx: &mut App,
) -> TextSelectionHandle {
    let handle = TextSelectionHandle::new("", cx);
    let geometry = Rc::downgrade(geometry);
    handle.resolve_content_key_with(
        {
            let view = view.clone();
            move |point, cx| {
                let geometry = geometry.upgrade()?;
                let view = view.upgrade()?;
                let selection = &view.read(cx).selection;
                let side = selection
                    .snapshot(cx)
                    .filter(|snapshot| snapshot.anchor().entity_id() == Some(selection.entity_id()))
                    .and_then(|snapshot| snapshot.anchor().content_key())
                    .map(|key| decode(key.value()).0);
                geometry
                    .borrow()
                    .key_at(point, side)
                    .map(TextSelectionContentKey::new)
            }
        },
        cx,
    );
    handle.copy_with(
        move |cx| {
            view.upgrade()
                .map(|view| view.read(cx).selected_text(cx))
                .unwrap_or_default()
        },
        cx,
    );
    handle
}

#[cfg(test)]
mod tests {
    // Not `super::*`: gpui's `test` attribute would shadow the built-in one.
    use super::{Pos, Selected, Side, decode, encode, overlay_background};
    use gpui_kit::HighlightStyle;

    #[test]
    fn keys_order_as_the_text_reads_and_round_trip() {
        let at = |file, line, offset| Pos { file, line, offset };
        let keys = [
            encode(Side::Old, at(0, 0, 5)),
            encode(Side::Old, at(0, 1, 0)),
            encode(Side::New, at(0, 0, 0)),
            encode(Side::Old, at(1, 0, 0)),
        ];
        assert!(keys.is_sorted());
        assert_eq!(decode(keys[2]), (Side::New, at(0, 0, 0)));
        assert_eq!(
            decode(encode(Side::Old, at(3, 42, 7))),
            (Side::Old, at(3, 42, 7))
        );
    }

    #[test]
    fn a_selection_clips_its_first_and_last_lines() {
        let selected = Selected {
            side: Side::New,
            start: Pos {
                file: 0,
                line: 1,
                offset: 4,
            },
            end: Pos {
                file: 0,
                line: 3,
                offset: 2,
            },
        };
        let new = |line| [(Side::New, line)];
        assert_eq!(selected.in_line(0, &new(0), "abcdef"), None);
        assert_eq!(selected.in_line(0, &new(1), "abcdef"), Some(4..6));
        assert_eq!(selected.in_line(0, &new(2), "xyz"), Some(0..3));
        assert_eq!(selected.in_line(0, &new(3), "abcdef"), Some(0..2));
        assert_eq!(selected.in_line(0, &[(Side::Old, 2)], "xyz"), None);
        // Not past the text, nor inside a character.
        assert_eq!(selected.in_line(0, &new(1), "abc"), None);
        assert_eq!(selected.in_line(0, &new(3), "дом"), Some(0..2));
        assert_eq!(selected.in_line(0, &new(1), "ключ"), Some(4..8));
    }

    #[test]
    fn the_selection_background_overrides_only_the_background() {
        let red = HighlightStyle {
            color: Some(gpui_kit::red()),
            ..Default::default()
        };
        let selection = gpui_kit::blue();
        let out = overlay_background(vec![(2..4, red), (6..8, red)], 3..7, selection);
        let ranges: Vec<_> = out.iter().map(|(range, _)| range.clone()).collect();
        assert_eq!(ranges, [2..3, 3..4, 4..6, 6..7, 7..8]);
        assert_eq!(out[1].1.color, red.color);
        assert_eq!(out[1].1.background_color, Some(selection));
        assert_eq!(out[2].1.background_color, Some(selection));
        assert_eq!(out[0].1.background_color, None);
        assert_eq!(out[4].1.background_color, None);
        // Unstyled text gets a range of its own.
        let plain = overlay_background(Vec::new(), 1..3, selection);
        assert_eq!(plain.len(), 1);
        assert_eq!(plain[0].0, 1..3);
        assert_eq!(plain[0].1.background_color, Some(selection));
    }
}
