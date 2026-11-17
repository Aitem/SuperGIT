//! The difftastic view: a structural diff (`sg_git::difftastic`) laid out like
//! the built-in one — file headers, folded unchanged lines, Unified or Split —
//! with syntax colors, where only the tokens difftastic found novel are
//! emphasized. Reformatting that changes no syntax shows as no change.

use std::collections::{HashMap, HashSet};
use std::ops::Range;
use std::path::PathBuf;
use std::rc::Rc;
use std::sync::Arc;

use gpui_kit::assets::IconName;
use gpui_kit::component::button::{Button, ButtonVariants as _};
use gpui_kit::component::diff::DiffMode;
use gpui_kit::component::scroll::{ScrollableMask, Scrollbar};
use gpui_kit::component::{ActiveTheme as _, Sizable as _, h_flex, v_flex};
use gpui_kit::prelude::*;
use gpui_kit::*;
use sg_git::difftastic::{self, FilePair, FileRequest, Line, Outcome, Source, StructuralFile};

use crate::syntax::{self, LineHighlights};

/// Unchanged rows shown around each change, as in the built-in diff.
const CONTEXT_ROWS: usize = 3;
/// Files longer than this are shown without syntax colors.
const MAX_HIGHLIGHTED_LINES: usize = 20_000;
/// The content width follows the longest line up to this many characters.
const MAX_MEASURED_CHARS: usize = 1_000;

/// What a comparison is computed for. Loading the same request again keeps
/// the current result.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Request {
    pub worktree: PathBuf,
    pub old: Source,
    pub new: Source,
    pub files: Vec<FileRequest>,
    /// Changes with the patch, so edits in the working tree are picked up.
    pub revision: u64,
}

impl Request {
    /// The same comparison, possibly of newer contents.
    fn same_target(&self, other: &Self) -> bool {
        self.worktree == other.worktree && self.old == other.old && self.new == other.new
    }
}

/// One compared file with its syntax colors, prepared off the main thread.
pub struct Prepared {
    file: StructuralFile,
    fingerprint: u64,
    old_syntax: Vec<LineHighlights>,
    new_syntax: Vec<LineHighlights>,
}

impl Prepared {
    fn new(file: StructuralFile, fingerprint: u64) -> Self {
        let highlight = |lines: &[Line], path: &str| {
            if lines.len() > MAX_HIGHLIGHTED_LINES {
                return Vec::new();
            }
            let texts: Vec<&str> = lines.iter().map(|line| line.text.as_str()).collect();
            syntax::highlight_lines(path, &texts)
        };
        let old_path = file.old_path.clone().unwrap_or_else(|| file.path.clone());
        Self {
            old_syntax: highlight(&file.old, &old_path),
            new_syntax: highlight(&file.new, &file.path),
            file,
            fingerprint,
        }
    }
}

#[derive(Clone, Debug, PartialEq, Eq)]
enum Status {
    Empty,
    Loading,
    Ready,
    Failed(String),
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum LineKind {
    Context,
    Removed,
    Added,
}

#[derive(Clone, Debug)]
enum Item {
    Header(usize),
    Note(SharedString),
    /// Folded unchanged rows of a file; a click reveals them.
    Gap {
        file: usize,
        rows: Range<usize>,
    },
    /// Split: one row, old on the left, new on the right.
    Pair {
        file: usize,
        row: usize,
    },
    /// Unified: one line of either side, or of both when unchanged.
    Line {
        file: usize,
        old: Option<usize>,
        new: Option<usize>,
        kind: LineKind,
    },
}

pub struct DifftView {
    status: Status,
    request: Option<Request>,
    files: Rc<Vec<Arc<Prepared>>>,
    /// Results by input fingerprint: a refresh compares only what changed.
    cache: HashMap<u64, Arc<Prepared>>,
    items: Rc<Vec<Item>>,
    collapsed: HashSet<String>,
    /// Revealed row ranges per path.
    expanded: HashMap<String, Vec<Range<usize>>>,
    mode: DiffMode,
    max_chars: usize,
    max_line_number: usize,
    list: ListState,
    horizontal: ScrollHandle,
    generation: u64,
    task: Option<Task<()>>,
}

impl DifftView {
    pub fn new(mode: DiffMode) -> Self {
        Self {
            status: Status::Empty,
            request: None,
            files: Rc::default(),
            cache: HashMap::new(),
            items: Rc::default(),
            collapsed: HashSet::new(),
            expanded: HashMap::new(),
            mode,
            max_chars: 0,
            max_line_number: 0,
            list: ListState::new(0, ListAlignment::Top, px(600.)),
            horizontal: ScrollHandle::new(),
            generation: 0,
            task: None,
        }
    }

    pub fn set_mode(&mut self, mode: DiffMode, cx: &mut Context<Self>) {
        if self.mode != mode {
            self.mode = mode;
            self.rebuild(false);
            cx.notify();
        }
    }

    /// Nothing to compare (a clean tree, a commit without changes).
    pub fn clear(&mut self, cx: &mut Context<Self>) {
        self.generation += 1;
        self.task = None;
        self.request = None;
        self.status = Status::Empty;
        self.files = Rc::default();
        self.cache.clear();
        self.rebuild(true);
        cx.notify();
    }

    /// Compares `request` in the background unless it is already shown.
    pub fn load(&mut self, request: Request, cx: &mut Context<Self>) {
        if self.request.as_ref() == Some(&request) && !matches!(self.status, Status::Failed(_)) {
            return;
        }
        let refresh = self
            .request
            .as_ref()
            .is_some_and(|current| current.same_target(&request));
        if !refresh {
            // Another commit or worktree: start from the top, empty.
            self.files = Rc::default();
            self.collapsed.clear();
            self.expanded.clear();
            self.rebuild(true);
        }
        self.request = Some(request.clone());
        self.status = Status::Loading;
        self.generation += 1;
        let generation = self.generation;
        let cache = self.cache.clone();
        self.task = Some(cx.spawn(async move |this, cx| {
            let result = cx
                .background_executor()
                .spawn(async move { prepare(&request, &cache) })
                .await;
            this.update(cx, |this, cx| {
                if this.generation != generation {
                    return;
                }
                this.task = None;
                match result {
                    Ok(files) => this.set_files(files, !refresh),
                    Err(error) => this.status = Status::Failed(format!("{error:#}")),
                }
                cx.notify();
            })
            .ok();
        }));
        cx.notify();
    }

    /// Scrolls to `path`'s header.
    pub fn scroll_to_file(&mut self, path: &str, cx: &mut Context<Self>) {
        let target = self.items.iter().position(
            |item| matches!(item, Item::Header(file) if self.files[*file].file.path == path),
        );
        if let Some(item_ix) = target {
            self.list.scroll_to(ListOffset {
                item_ix,
                offset_in_item: px(0.),
            });
            cx.notify();
        }
    }

    fn set_files(&mut self, files: Vec<Arc<Prepared>>, reset: bool) {
        // Revealed rows belong to the contents they were revealed in.
        let previous: HashMap<&str, u64> = self
            .files
            .iter()
            .map(|prepared| (prepared.file.path.as_str(), prepared.fingerprint))
            .collect();
        self.expanded.retain(|path, _| {
            files.iter().any(|prepared| {
                prepared.file.path == *path
                    && previous.get(path.as_str()) == Some(&prepared.fingerprint)
            })
        });
        self.cache = files
            .iter()
            .filter(|prepared| prepared.fingerprint != 0)
            .map(|prepared| (prepared.fingerprint, prepared.clone()))
            .collect();
        let lines = || {
            files
                .iter()
                .flat_map(|prepared| prepared.file.old.iter().chain(&prepared.file.new))
        };
        self.max_chars = lines()
            .map(|line| line.text.chars().count().min(MAX_MEASURED_CHARS))
            .max()
            .unwrap_or(0);
        self.max_line_number = files
            .iter()
            .map(|prepared| prepared.file.old.len().max(prepared.file.new.len()))
            .max()
            .unwrap_or(0);
        self.files = Rc::new(files);
        self.status = Status::Ready;
        self.rebuild(reset);
    }

    fn toggle_file(&mut self, file: usize, cx: &mut Context<Self>) {
        let Some(prepared) = self.files.get(file) else {
            return;
        };
        let path = prepared.file.path.clone();
        if !self.collapsed.remove(&path) {
            self.collapsed.insert(path);
        }
        self.rebuild(false);
        cx.notify();
    }

    fn expand(&mut self, file: usize, rows: Range<usize>, cx: &mut Context<Self>) {
        let Some(prepared) = self.files.get(file) else {
            return;
        };
        self.expanded
            .entry(prepared.file.path.clone())
            .or_default()
            .push(rows);
        self.rebuild(false);
        cx.notify();
    }

    fn rebuild(&mut self, reset: bool) {
        let mut items = Vec::new();
        for (ix, prepared) in self.files.iter().enumerate() {
            items.push(Item::Header(ix));
            let file = &prepared.file;
            if self.collapsed.contains(&file.path) {
                continue;
            }
            let note = match &file.outcome {
                Outcome::Binary => Some("Binary file not shown".to_string()),
                Outcome::TooLarge => Some("File too large for difftastic".to_string()),
                Outcome::Skipped => Some(format!(
                    "Not compared: difftastic shows the first {} files",
                    difftastic::MAX_FILES
                )),
                Outcome::Failed(error) => Some(format!("difftastic failed: {error}")),
                Outcome::Unchanged => Some(
                    "No syntactic changes — only whitespace, formatting or the file mode differ"
                        .to_string(),
                ),
                Outcome::Changed | Outcome::Created | Outcome::Deleted => None,
            };
            if let Some(note) = note {
                items.push(Item::Note(note.into()));
                continue;
            }
            let mut visible = file.hunks(CONTEXT_ROWS);
            if let Some(expanded) = self.expanded.get(&file.path) {
                visible.extend(expanded.iter().cloned());
            }
            let mut previous_end = 0;
            for range in merge(visible, file.rows.len()) {
                if range.start > previous_end {
                    items.push(Item::Gap {
                        file: ix,
                        rows: previous_end..range.start,
                    });
                }
                self.push_rows(&mut items, ix, file, range.clone());
                previous_end = range.end;
            }
            if previous_end < file.rows.len() {
                items.push(Item::Gap {
                    file: ix,
                    rows: previous_end..file.rows.len(),
                });
            }
        }
        let old_count = self.items.len();
        self.items = Rc::new(items);
        if reset {
            self.list.reset(self.items.len());
            self.horizontal.set_offset(point(px(0.), px(0.)));
        } else {
            self.list.splice(0..old_count, self.items.len());
        }
    }

    fn push_rows(
        &self,
        items: &mut Vec<Item>,
        ix: usize,
        file: &StructuralFile,
        rows: Range<usize>,
    ) {
        if self.mode == DiffMode::Split {
            items.extend(rows.map(|row| Item::Pair { file: ix, row }));
            return;
        }
        let mut row = rows.start;
        while row < rows.end {
            if !file.is_changed_row(&file.rows[row]) {
                let line = file.rows[row];
                items.push(Item::Line {
                    file: ix,
                    old: line.old,
                    new: line.new,
                    kind: LineKind::Context,
                });
                row += 1;
                continue;
            }
            // A run of changed rows: its removed lines, then its added ones.
            let end = (row..rows.end)
                .find(|&r| !file.is_changed_row(&file.rows[r]))
                .unwrap_or(rows.end);
            let run = &file.rows[row..end];
            items.extend(run.iter().filter_map(|r| r.old).map(|old| Item::Line {
                file: ix,
                old: Some(old),
                new: None,
                kind: LineKind::Removed,
            }));
            items.extend(run.iter().filter_map(|r| r.new).map(|new| Item::Line {
                file: ix,
                old: None,
                new: Some(new),
                kind: LineKind::Added,
            }));
            row = end;
        }
    }
}

/// Sorted, merged, clamped to `len`.
fn merge(mut ranges: Vec<Range<usize>>, len: usize) -> Vec<Range<usize>> {
    ranges.sort_by_key(|range| range.start);
    let mut merged: Vec<Range<usize>> = Vec::new();
    for range in ranges {
        let range = range.start.min(len)..range.end.min(len);
        if range.is_empty() {
            continue;
        }
        match merged.last_mut() {
            Some(last) if range.start <= last.end => last.end = last.end.max(range.end),
            _ => merged.push(range),
        }
    }
    merged
}

fn prepare(
    request: &Request,
    cache: &HashMap<u64, Arc<Prepared>>,
) -> anyhow::Result<Vec<Arc<Prepared>>> {
    let pairs = difftastic::read_pairs(
        &request.worktree,
        &request.old,
        &request.new,
        &request.files,
    )?;
    let fingerprints: Vec<u64> = pairs.iter().map(FilePair::fingerprint).collect();
    let missing: Vec<&FilePair> = pairs
        .iter()
        .zip(&fingerprints)
        .filter(|(_, fingerprint)| !cache.contains_key(fingerprint))
        .map(|(pair, _)| pair)
        .collect();
    let mut compared = difftastic::compare_many(&missing).into_iter();
    let mut files = Vec::with_capacity(request.files.len());
    for (pair, fingerprint) in pairs.iter().zip(&fingerprints) {
        let prepared = match cache.get(fingerprint) {
            Some(prepared) => prepared.clone(),
            None => {
                let file = compared.next().unwrap_or_else(|| difftastic::compare(pair));
                Arc::new(Prepared::new(file, *fingerprint))
            }
        };
        files.push(prepared);
    }
    files.extend(
        request
            .files
            .iter()
            .skip(pairs.len())
            .map(|request| Arc::new(Prepared::new(difftastic::skipped(request), 0))),
    );
    Ok(files)
}

/// Everything a row needs to paint, shared by the rows of one frame.
struct Presentation {
    files: Rc<Vec<Arc<Prepared>>>,
    items: Rc<Vec<Item>>,
    view: WeakEntity<DifftView>,
    collapsed: HashSet<String>,
    font_size: Pixels,
    row_height: Pixels,
    gutter_width: Pixels,
    sign_width: Pixels,
    /// Split: long lines wrap within their half.
    wrap: bool,
}

impl Render for DifftView {
    fn render(&mut self, window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let theme = cx.theme().clone();
        let frame = v_flex()
            .id(("difft", cx.entity_id()))
            .size_full()
            .min_w_0()
            .min_h_0()
            .relative()
            .bg(theme.background)
            .text_color(theme.foreground)
            .text_sm()
            .border_1()
            .border_color(theme.border);
        if self.files.is_empty() {
            let message = match &self.status {
                Status::Loading => "Comparing with difftastic…".to_string(),
                Status::Failed(error) => format!("difftastic failed: {error}"),
                Status::Empty | Status::Ready => "No changes".to_string(),
            };
            return frame
                .items_center()
                .justify_center()
                .text_color(theme.muted_foreground)
                .child(message)
                .into_any_element();
        }
        let rem = window.rem_size();
        let font_size = rem * (f32::from(theme.mono_font_size) / 16.);
        let row_height = font_size * 1.6;
        let digits = self.max_line_number.max(1).to_string().len();
        let gutter_width = font_size * 0.7 * digits as f32 + rem;
        let sign_width = rem;
        let font_id = window
            .text_system()
            .resolve_font(&font(theme.mono_font_family.clone()));
        let char_width = window
            .text_system()
            .ch_advance(font_id, font_size)
            .unwrap_or(font_size * 0.6);
        // Split wraps long lines so each side keeps half the width, as on
        // GitHub; Unified keeps lines whole and scrolls sideways.
        let wrap = self.mode == DiffMode::Split;
        let text_width = char_width * self.max_chars as f32 + rem;
        let content_width = rem + gutter_width * 2. + sign_width + text_width;
        let presentation = Rc::new(Presentation {
            files: self.files.clone(),
            items: self.items.clone(),
            view: cx.entity().downgrade(),
            collapsed: self.collapsed.clone(),
            font_size,
            row_height,
            gutter_width,
            sign_width,
            wrap,
        });
        let rows = list(self.list.clone(), move |ix, window, cx| {
            render_item(ix, &presentation, window, cx)
        })
        .h_full();
        let body = div()
            .id("difft-body")
            .relative()
            .flex_1()
            .min_h_0()
            .min_w_0()
            .child(
                div()
                    .id("difft-horizontal")
                    .size_full()
                    .overflow_x_hidden()
                    .track_scroll(&self.horizontal)
                    .child(if wrap {
                        rows.w_full()
                    } else {
                        rows.w(content_width).min_w_full()
                    }),
            )
            .when(!wrap, |el| {
                el.child(ScrollableMask::new(Axis::Horizontal, &self.horizontal).id("difft-wheel"))
            })
            .child(
                div()
                    .absolute()
                    .inset_0()
                    .child(Scrollbar::vertical(&self.list).viewport_from_layout()),
            )
            .when(!wrap, |el| {
                el.child(Scrollbar::horizontal(&self.horizontal))
            });
        frame
            .child(body)
            .when(self.status == Status::Loading, |el| {
                el.child(
                    div()
                        .absolute()
                        .top_1()
                        .right_4()
                        .px_2()
                        .rounded_sm()
                        .bg(theme.muted)
                        .text_xs()
                        .text_color(theme.muted_foreground)
                        .child("difftastic…"),
                )
            })
            .when_some(
                match &self.status {
                    Status::Failed(error) => Some(error.clone()),
                    _ => None,
                },
                |el, error| {
                    el.child(
                        div()
                            .absolute()
                            .bottom_2()
                            .left_2()
                            .right_2()
                            .px_2()
                            .py_1()
                            .rounded_sm()
                            .bg(theme.danger.opacity(0.15))
                            .text_xs()
                            .child(format!(
                                "difftastic failed, showing the last result: {error}"
                            )),
                    )
                },
            )
            .into_any_element()
    }
}

fn render_item(ix: usize, p: &Presentation, _window: &mut Window, cx: &mut App) -> AnyElement {
    let Some(item) = p.items.get(ix) else {
        return div().into_any_element();
    };
    match item {
        Item::Header(file) => render_header(*file, p, cx),
        Item::Note(text) => div()
            .w_full()
            .px_3()
            .py_2()
            .text_sm()
            .text_color(cx.theme().muted_foreground)
            .child(text.clone())
            .into_any_element(),
        Item::Gap { file, rows } => render_gap(ix, *file, rows.clone(), p, cx),
        Item::Pair { file, row } => {
            let prepared = &p.files[*file];
            let row = prepared.file.rows[*row];
            let changed = prepared.file.is_changed_row(&row);
            // Backgrounds belong to the halves, which stretch to the taller
            // of the two wrapped lines.
            let half = |side: Side, ix: Option<usize>| {
                let line = ix.and_then(|ix| match side {
                    Side::Old => prepared.file.old.get(ix),
                    Side::New => prepared.file.new.get(ix),
                });
                let background = match line {
                    Some(line) if line.changed => Some(side_color(side, cx).opacity(0.12)),
                    // GitHub's empty side of a changed row.
                    None if changed => Some(cx.theme().muted.opacity(0.35)),
                    _ => None,
                };
                div()
                    .w_1_2()
                    .min_w_0()
                    .overflow_hidden()
                    .when_some(background, |el, background| el.bg(background))
                    .child(code_line(prepared, side, ix, p, cx))
            };
            h_flex()
                .id(("difft-row", ix))
                .w_full()
                .items_stretch()
                .child(half(Side::Old, row.old))
                .child(
                    half(Side::New, row.new)
                        .border_l_1()
                        .border_color(cx.theme().border),
                )
                .into_any_element()
        }
        Item::Line {
            file,
            old,
            new,
            kind,
        } => unified_line(&p.files[*file], *old, *new, *kind, p, cx),
    }
}

#[derive(Clone, Copy, PartialEq, Eq)]
enum Side {
    Old,
    New,
}

fn render_header(file: usize, p: &Presentation, cx: &App) -> AnyElement {
    let theme = cx.theme();
    let structural = &p.files[file].file;
    let collapsed = p.collapsed.contains(&structural.path);
    let name = match &structural.old_path {
        Some(old) if *old != structural.path => format!("{old} → {}", structural.path),
        _ => structural.path.clone(),
    };
    let status = match structural.outcome {
        Outcome::Created => Some("added"),
        Outcome::Deleted => Some("deleted"),
        _ if structural.old_path.is_some() => Some("renamed"),
        _ => None,
    };
    let view = p.view.clone();
    let counted = matches!(structural.outcome, Outcome::Changed);
    h_flex()
        .id(("difft-header", file))
        .w_full()
        .min_w_0()
        .pl_1()
        .pr_3()
        .py_1()
        .gap_2()
        .when(file > 0, |el| el.border_t_1())
        .border_b_1()
        .border_color(theme.border)
        .bg(theme.muted.opacity(0.2))
        .child(
            Button::new(("difft-collapse", file))
                .ghost()
                .xsmall()
                .icon(if collapsed {
                    IconName::ChevronRight
                } else {
                    IconName::ChevronDown
                })
                .on_click(move |_, _, cx| {
                    let _ = view.update(cx, |view, cx| view.toggle_file(file, cx));
                }),
        )
        .child(
            div()
                .min_w_0()
                .truncate()
                .font_weight(FontWeight::MEDIUM)
                .child(name),
        )
        .when_some(structural.language.clone(), |el, language| {
            el.child(
                div()
                    .flex_shrink_0()
                    .px_1()
                    .rounded_sm()
                    .bg(theme.muted)
                    .text_xs()
                    .text_color(theme.muted_foreground)
                    .child(language),
            )
        })
        .when(counted, |el| {
            el.child(
                h_flex()
                    .gap_1()
                    .text_xs()
                    .child(
                        div()
                            .text_color(theme.success)
                            .child(format!("+{}", structural.additions())),
                    )
                    .child(
                        div()
                            .text_color(theme.danger)
                            .child(format!("−{}", structural.deletions())),
                    ),
            )
        })
        .when_some(status, |el, status| {
            el.child(
                div()
                    .text_xs()
                    .text_color(theme.muted_foreground)
                    .child(status),
            )
        })
        .into_any_element()
}

fn render_gap(
    ix: usize,
    file: usize,
    rows: Range<usize>,
    p: &Presentation,
    cx: &App,
) -> AnyElement {
    let view = p.view.clone();
    let count = rows.len();
    h_flex()
        .w_full()
        .h(p.row_height)
        .px_2()
        .bg(cx.theme().muted.opacity(0.35))
        .child(
            Button::new(("difft-expand", ix))
                .ghost()
                .xsmall()
                .icon(IconName::ChevronsUpDown)
                .text_color(cx.theme().muted_foreground)
                .label(format!(
                    "{count} unchanged line{}",
                    if count == 1 { "" } else { "s" }
                ))
                .on_click(move |_, _, cx| {
                    let _ = view.update(cx, |view, cx| view.expand(file, rows.clone(), cx));
                }),
        )
        .into_any_element()
}

fn side_color(side: Side, cx: &App) -> Hsla {
    match side {
        Side::Old => cx.theme().danger,
        Side::New => cx.theme().success,
    }
}

/// A code row: fixed height and unwrapped, or growing with wrapped text.
fn code_row(p: &Presentation, cx: &App) -> Div {
    h_flex()
        .w_full()
        .px_2()
        .font_family(cx.theme().mono_font_family.clone())
        .text_size(p.font_size)
        .line_height(p.row_height)
        .map(|el| {
            if p.wrap {
                el.min_h(p.row_height).items_start().whitespace_normal()
            } else {
                el.h(p.row_height).whitespace_nowrap()
            }
        })
}

/// The text part of a code row.
fn code_cell(text: StyledText, p: &Presentation) -> Div {
    div()
        .map(|el| {
            if p.wrap {
                el.flex_1().min_w_0()
            } else {
                el.flex_shrink_0()
            }
        })
        .child(text)
}

/// One side's line in Split mode; `ix` is `None` where the side has no line.
fn code_line(
    prepared: &Prepared,
    side: Side,
    ix: Option<usize>,
    p: &Presentation,
    cx: &App,
) -> AnyElement {
    let row = code_row(p, cx);
    let (lines, highlights) = match side {
        Side::Old => (&prepared.file.old, &prepared.old_syntax),
        Side::New => (&prepared.file.new, &prepared.new_syntax),
    };
    let Some((ix, line)) = ix.and_then(|ix| Some((ix, lines.get(ix)?))) else {
        return row.into_any_element();
    };
    let status = side_color(side, cx);
    let sign = match (line.changed, side) {
        (false, _) => " ",
        (true, Side::Old) => "−",
        (true, Side::New) => "+",
    };
    row.child(gutter(Some(ix), line.changed.then_some(status), p, cx))
        .child(
            div()
                .w(p.sign_width)
                .flex_shrink_0()
                .text_color(status)
                .child(sign),
        )
        .child(code_cell(
            code_text(line, highlights.get(ix), status, cx),
            p,
        ))
        .into_any_element()
}

fn unified_line(
    prepared: &Prepared,
    old: Option<usize>,
    new: Option<usize>,
    kind: LineKind,
    p: &Presentation,
    cx: &App,
) -> AnyElement {
    let theme = cx.theme();
    let (status, sign) = match kind {
        LineKind::Context => (theme.muted_foreground, " "),
        LineKind::Removed => (theme.danger, "−"),
        LineKind::Added => (theme.success, "+"),
    };
    // Unchanged lines show the new text; difftastic considers both the same.
    let (line, highlights) = match (kind, new, old) {
        (LineKind::Removed, _, Some(old)) | (LineKind::Context, None, Some(old)) => {
            (prepared.file.old.get(old), prepared.old_syntax.get(old))
        }
        (_, Some(new), _) => (prepared.file.new.get(new), prepared.new_syntax.get(new)),
        _ => (None, None),
    };
    let changed = kind != LineKind::Context;
    code_row(p, cx)
        .when(changed, |el| el.bg(status.opacity(0.12)))
        .child(gutter(old, changed.then_some(status), p, cx))
        .child(gutter(new, changed.then_some(status), p, cx))
        .child(
            div()
                .w(p.sign_width)
                .flex_shrink_0()
                .text_color(status)
                .child(sign),
        )
        .when_some(line, |el, line| {
            el.child(code_cell(code_text(line, highlights, status, cx), p))
        })
        .into_any_element()
}

fn gutter(ix: Option<usize>, tint: Option<Hsla>, p: &Presentation, cx: &App) -> Div {
    div()
        .flex_shrink_0()
        .w(p.gutter_width)
        .h(p.row_height)
        .pr_2()
        .flex()
        .items_center()
        .justify_end()
        .text_color(cx.theme().muted_foreground)
        .when_some(tint, |el, tint| el.bg(tint.opacity(0.08)))
        .when_some(ix, |el, ix| el.child((ix + 1).to_string()))
}

/// The line with syntax colors, and difftastic's novel tokens emphasized.
fn code_text(
    line: &Line,
    highlights: Option<&LineHighlights>,
    status: Hsla,
    cx: &App,
) -> StyledText {
    let theme = &cx.theme().highlight_theme;
    let syntax: Vec<(Range<usize>, HighlightStyle)> = highlights
        .into_iter()
        .flatten()
        .filter_map(|(range, name)| Some((range.clone(), theme.style(name)?)))
        .collect();
    let novel = line.novel.iter().map(|range| {
        (
            range.clone(),
            HighlightStyle {
                background_color: Some(status.opacity(0.3)),
                ..Default::default()
            },
        )
    });
    StyledText::new(SharedString::from(line.text.clone()))
        .with_highlights(combine_highlights(syntax, novel).collect::<Vec<_>>())
}

#[cfg(test)]
mod tests {
    use super::merge;

    #[test]
    fn merges_revealed_rows_into_hunks() {
        assert_eq!(
            merge(vec![5..8, 0..3, 2..4, 20..30], 25),
            vec![0..4, 5..8, 20..25]
        );
        assert!(merge(vec![30..40, 50..60], 25).is_empty());
    }
}
