//! The text box's height. It grows with the text (2–12 lines) until the
//! composer's top edge is dragged; from then on it keeps the dragged height,
//! in every chat and across launches (`settings.json`), until a double click
//! on that edge gives the growing back.

use std::cell::Cell;
use std::rc::Rc;
use std::time::Duration;

use gpui_kit::base::{ElementExt as _, InteractiveElementExt as _};
use gpui_kit::component::resizable::{HandleEdge, resize_handle};

use super::*;

/// Lines of a growing text box.
const GROW_LINES: (usize, usize) = (2, 12);
/// The tallest a dragged text box gets, as a share of the window's height.
const MAX_SHARE: f32 = 0.6;

struct ComposerHeight {
    height: Option<Pixels>,
    /// Saves the latest height once a drag settles; held only to be dropped
    /// (cancelled) by every newer one.
    _save: Option<Task<()>>,
}

impl Global for ComposerHeight {}

/// Load the saved height; call once at startup.
pub fn init(cx: &mut App) {
    let height = crate::settings::load().composer_height.map(px);
    cx.set_global(ComposerHeight {
        height,
        _save: None,
    });
}

fn current(cx: &App) -> Option<Pixels> {
    cx.try_global::<ComposerHeight>()
        .and_then(|state| state.height)
}

fn set(height: Option<Pixels>, cx: &mut App) {
    if height == current(cx) {
        return;
    }
    let save = cx.spawn(async move |cx| {
        let executor = cx.background_executor().clone();
        executor.timer(Duration::from_millis(400)).await;
        executor
            .spawn(async move {
                let mut settings = crate::settings::load();
                settings.composer_height = height.map(|height| height.as_f32());
                crate::settings::save(&settings);
            })
            .await;
    });
    cx.set_global(ComposerHeight {
        height,
        _save: Some(save),
    });
}

/// A line of the text box: gpui-component's input sets 1.25rem.
fn line_height(window: &Window) -> Pixels {
    rems(1.25).to_pixels(window.rem_size())
}

/// What a drag on the composer's top edge carries (and draws: nothing).
#[derive(Clone)]
pub(super) struct ComposerResize;

impl Render for ComposerResize {
    fn render(&mut self, _: &mut Window, _: &mut Context<Self>) -> impl IntoElement {
        Empty
    }
}

/// A chat's side of the shared height.
#[derive(Default)]
pub(super) struct ComposerSize {
    /// The height its text box got in the last frame.
    measured: Rc<Cell<Pixels>>,
    /// The height its text box is set up for (`None`: growing).
    applied: Option<Pixels>,
}

impl ChatSession {
    /// Follow the shared height: a dragged box shows as many lines as fit
    /// and scrolls the rest.
    pub(super) fn sync_composer_height(&mut self, window: &Window, cx: &mut Context<Self>) {
        let height = current(cx);
        if height == self.composer.applied {
            return;
        }
        self.composer.applied = height;
        let (min, max) = match height {
            Some(height) => (1, ((height / line_height(window)).floor() as usize).max(1)),
            None => GROW_LINES,
        };
        self.input
            .update(cx, |input, cx| input.set_auto_grow(min, max, cx));
    }

    /// The text box, at the dragged height if there is one.
    pub(super) fn render_text_box(&self, cx: &App) -> impl IntoElement {
        let measured = self.composer.measured.clone();
        div()
            .on_prepaint(move |bounds, _, _| measured.set(bounds.size.height))
            .child(
                Textarea::new(&self.input)
                    .appearance(false)
                    .when_some(current(cx), |textarea, height| textarea.h(height)),
            )
    }

    /// The composer's top edge: drag it to resize the text box, double click
    /// to let the box grow with the text again. Its line is the composer's
    /// top border.
    pub(super) fn render_composer_edge(&self, cx: &mut Context<Self>) -> impl IntoElement {
        resize_handle::<ComposerResize, ComposerResize>("composer-edge", Axis::Vertical)
            .inside(HandleEdge::Leading)
            .on_drag(ComposerResize, |drag, _, _, cx| cx.new(|_| drag.clone()))
            .on_double_click(cx.listener(|_, _, _, cx| {
                set(None, cx);
                cx.notify();
            }))
    }

    /// A drag of the top edge, heard by the whole composer (`bounds`): the
    /// edge follows the pointer.
    pub(super) fn drag_composer_edge(
        &mut self,
        event: &DragMoveEvent<ComposerResize>,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let line = line_height(window);
        let max = (window.viewport_size().height * MAX_SHARE).max(line * 2.);
        let height = self.composer.measured.get() + (event.bounds.top() - event.event.position.y);
        set(Some(height.max(line * 2.).min(max)), cx);
        cx.notify();
    }
}
