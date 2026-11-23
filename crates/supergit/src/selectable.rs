//! A selectable label with styled parts (drag, double click, `cmd-c`):
//! gpui-base's `SelectableText` lays out plain text only.
//!
//! The text is laid out whole: an ellipsis would change what is laid out
//! and switch selection off, so a container clips it instead.

use std::ops::Range;

use gpui_kit::base::{TextSelectionHandle, TextSelectionRegistration, TextSelectionRun};
use gpui_kit::component::ActiveTheme as _;
use gpui_kit::*;

/// Reading order among the window's selectable text: apart from the
/// numbers other text gets, so a selection here takes in nothing else.
const ORDER: u64 = 1 << 50;

pub struct SelectableLabel {
    id: ElementId,
    handle: TextSelectionHandle,
    text: SharedString,
    styled: StyledText,
}

impl SelectableLabel {
    /// `highlights` style parts of `text` (ordered, not overlapping). The
    /// handle is the label's own: one handle, one label.
    pub fn new(
        id: impl Into<ElementId>,
        handle: TextSelectionHandle,
        text: impl Into<SharedString>,
        highlights: Vec<(Range<usize>, HighlightStyle)>,
    ) -> Self {
        let text = text.into();
        Self {
            id: id.into(),
            handle,
            styled: StyledText::new(text.clone()).with_highlights(highlights),
            text,
        }
    }
}

impl IntoElement for SelectableLabel {
    type Element = Self;

    fn into_element(self) -> Self::Element {
        self
    }
}

impl Element for SelectableLabel {
    type RequestLayoutState = ();
    type PrepaintState = ();

    fn id(&self) -> Option<ElementId> {
        Some(self.id.clone())
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
        let hitbox = window.insert_hitbox(bounds, HitboxBehavior::Normal);
        let registration = TextSelectionRegistration::new(hitbox, bounds)
            .with_document_order(ORDER)
            .with_text_bounds(vec![bounds])
            .with_rendered_element(&self.handle, window, cx);
        self.handle.register(registration, window, cx);
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
        let layout = self.styled.layout().clone();
        let projection = self.handle.update_runs(
            &[
                TextSelectionRun::new(self.text.clone(), layout.clone(), bounds)
                    .with_document_order(ORDER),
            ],
            cx,
        );
        // A selection projected anew (a drag, a new layout) needs another
        // frame, as `SelectableText` does.
        let ranges: Painted = projection.ranges().to_vec();
        if let Some(id) = id {
            let changed = window.with_element_state(id, |painted: Option<Painted>, _| {
                (painted.as_ref() != Some(&ranges), ranges.clone())
            });
            if changed {
                window.refresh();
            }
        }
        let color = cx.theme().selection;
        for range in ranges.into_iter().flatten() {
            paint_selection(&layout, range, color, window);
        }
        self.styled
            .paint(id, inspector_id, bounds, &mut (), &mut (), window, cx);
    }
}

/// The projection painted last frame.
type Painted = Vec<Option<Range<usize>>>;

fn paint_selection(layout: &TextLayout, range: Range<usize>, color: Hsla, window: &mut Window) {
    let (Some(start), Some(end)) = (
        layout.position_for_index(range.start),
        layout.position_for_index(range.end),
    ) else {
        return;
    };
    // One line: the label never wraps.
    let quad = Bounds::from_corners(start, point(end.x, end.y + layout.line_height()));
    window.paint_quad(fill(quad, color));
}
