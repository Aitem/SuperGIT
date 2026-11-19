//! Ghostty's terminal emulation (libghostty-vt) behind a small, plain-data
//! interface: feed PTY output in, get a [`Screen`] of styled runs out, turn
//! keys, pastes and wheel scrolls into bytes for the PTY.
//!
//! libghostty handles are `!Send`: a [`Terminal`] lives on the thread that
//! made it (the app's main thread) and only bytes cross threads.

use std::cell::{Cell, RefCell};
use std::rc::Rc;

use anyhow::{Result, anyhow};
use libghostty_vt::RenderState;
use libghostty_vt::color::{Palette, Rgb as GhosttyRgb};
use libghostty_vt::effect::{
    ConformanceLevel, DeviceAttributeFeature, DeviceAttributes, DeviceType,
    PrimaryDeviceAttributes, ProgramStatusKind, ProgramStatusState, SecondaryDeviceAttributes,
    TertiaryDeviceAttributes,
};
use libghostty_vt::key::{self, Action, Mods};
use libghostty_vt::mode::Mode;
use libghostty_vt::render::{CellIterator, CursorVisualStyle, Dirty, RowIterator};
use libghostty_vt::report::{ColorScheme, SizeReportSize};
use libghostty_vt::screen::CellWide;
use libghostty_vt::style::Underline;
use libghostty_vt::terminal::ScrollViewport;

use crate::keys::{KeyInput, logical_key};
use crate::pty::Size;

/// Lines kept above the screen.
const SCROLLBACK: usize = 10_000;

/// An sRGB color.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Hash)]
pub struct Rgb {
    pub r: u8,
    pub g: u8,
    pub b: u8,
}

impl Rgb {
    pub const fn new(r: u8, g: u8, b: u8) -> Self {
        Self { r, g, b }
    }

    /// `0xRRGGBB`.
    pub fn to_u32(self) -> u32 {
        (u32::from(self.r) << 16) | (u32::from(self.g) << 8) | u32::from(self.b)
    }
}

impl From<GhosttyRgb> for Rgb {
    fn from(c: GhosttyRgb) -> Self {
        Self::new(c.r, c.g, c.b)
    }
}

impl From<Rgb> for GhosttyRgb {
    fn from(c: Rgb) -> Self {
        Self {
            r: c.r,
            g: c.g,
            b: c.b,
        }
    }
}

/// The colors programs see: defaults plus the 16 ANSI colors (the rest of
/// the 256-color palette is generated from them, as Ghostty does).
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Colors {
    pub background: Rgb,
    pub foreground: Rgb,
    pub cursor: Rgb,
    /// Black, red, green, yellow, blue, magenta, cyan, white, then the
    /// bright variants.
    pub ansi: [Rgb; 16],
    /// Answered to "dark or light?" queries (`CSI ? 996 n`).
    pub dark: bool,
}

impl Default for Colors {
    fn default() -> Self {
        let palette = Palette::default();
        let mut ansi = [Rgb::default(); 16];
        for (ix, slot) in ansi.iter_mut().enumerate() {
            *slot = palette.0[ix].into();
        }
        Self {
            background: Rgb::new(0x1e, 0x1e, 0x1e),
            foreground: Rgb::new(0xdd, 0xdd, 0xdd),
            cursor: Rgb::new(0xdd, 0xdd, 0xdd),
            ansi,
            dark: true,
        }
    }
}

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub enum UnderlineStyle {
    #[default]
    None,
    Single,
    Double,
    Curly,
    Dotted,
    Dashed,
}

/// Cells in one row that share a style, drawn as one piece of text.
#[derive(Clone, Debug, PartialEq)]
pub struct Run {
    /// First column.
    pub col: u16,
    /// Columns covered (a wide character covers two).
    pub width: u16,
    pub text: String,
    pub fg: Rgb,
    /// `None`: the terminal background.
    pub bg: Option<Rgb>,
    pub bold: bool,
    pub italic: bool,
    pub faint: bool,
    pub underline: UnderlineStyle,
    pub strikethrough: bool,
    pub selected: bool,
}

impl Run {
    fn same_style(&self, other: &Run) -> bool {
        self.fg == other.fg
            && self.bg == other.bg
            && self.bold == other.bold
            && self.italic == other.italic
            && self.faint == other.faint
            && self.underline == other.underline
            && self.strikethrough == other.strikethrough
            && self.selected == other.selected
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum CursorShape {
    Block,
    HollowBlock,
    Bar,
    Underline,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Cursor {
    pub col: u16,
    pub row: u16,
    pub shape: CursorShape,
    /// On a wide character: two cells.
    pub wide: bool,
    pub color: Option<Rgb>,
}

/// Where the viewport is in the scrollback.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct Scroll {
    pub total: u64,
    pub offset: u64,
    pub visible: u64,
}

impl Scroll {
    /// Scrolled away from the live screen.
    pub fn is_scrolled_back(self) -> bool {
        self.offset + self.visible < self.total
    }
}

/// What to draw.
#[derive(Clone, Debug, Default)]
pub struct Screen {
    pub cols: u16,
    pub rows: u16,
    pub background: Rgb,
    pub foreground: Rgb,
    /// One entry per visible row; runs never overlap and skip default cells.
    pub lines: Vec<Vec<Run>>,
    pub cursor: Option<Cursor>,
    pub scroll: Scroll,
}

/// What a program says it is doing (the OSC 7501 program status protocol,
/// which Claude Code speaks): exact agent state without guessing from output.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ProgramState {
    Idle,
    Working,
    /// Finished a task (until the next one starts).
    Done,
    /// Waiting for the user.
    Blocked(BlockedOn),
    Error,
    /// No status any more (the program withdrew it, or the terminal reset).
    Cleared,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum BlockedOn {
    Permission,
    Question,
    Auth,
    Other,
}

/// A desktop notification the program asked for (`OSC 9`, `OSC 777`).
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct Notification {
    pub title: String,
    pub body: String,
}

/// Things the program did that the host acts on, collected while feeding
/// output.
#[derive(Debug, Default)]
pub struct Effects {
    /// Replies to the program's queries: write them to the PTY.
    pub reply: Vec<u8>,
    pub bell: bool,
    /// The title changed (`OSC 0`/`OSC 2`); read it with [`Terminal::title`].
    pub title_changed: bool,
    /// Text the program asked to put on the clipboard (`OSC 52`).
    pub clipboard: Option<String>,
    /// The program's latest own status report (its root record).
    pub status: Option<ProgramState>,
    pub notifications: Vec<Notification>,
}

#[derive(Default)]
struct Shared {
    reply: RefCell<Vec<u8>>,
    bell: Cell<bool>,
    title_changed: Cell<bool>,
    clipboard: RefCell<Option<String>>,
    status: Cell<Option<ProgramState>>,
    notifications: RefCell<Vec<Notification>>,
    /// Synchronized output (mode 2026) is on: the program is mid-frame.
    held: Cell<bool>,
    dark: Cell<bool>,
    size: Cell<Size>,
}

pub struct Terminal {
    vt: libghostty_vt::Terminal<'static, 'static>,
    render: RenderState<'static>,
    row_iter: RowIterator<'static>,
    cell_iter: CellIterator<'static>,
    keys: key::Encoder<'static>,
    key_event: key::Event<'static>,
    shared: Rc<Shared>,
    /// Lines drawn last time; only dirty rows are rebuilt.
    lines: Vec<Vec<Run>>,
    colors: Colors,
}

/// Untrusted text from the program, for showing outside the terminal: no
/// control or bidi-override characters.
fn clean_text(bytes: &[u8]) -> String {
    String::from_utf8_lossy(bytes)
        .chars()
        .filter(|c| {
            !c.is_control() && !matches!(c, '\u{202a}'..='\u{202e}' | '\u{2066}'..='\u{2069}')
        })
        .collect()
}

impl Terminal {
    pub fn new(size: Size, colors: &Colors) -> Result<Self> {
        let mut vt = libghostty_vt::Terminal::new(size.cols.max(1), size.rows.max(1))?;
        vt.set_scrollback_max_lines(Some(SCROLLBACK))?;
        let shared = Rc::new(Shared::default());
        shared.size.set(size);
        let reply = Rc::clone(&shared);
        let bell = Rc::clone(&shared);
        let title = Rc::clone(&shared);
        let clipboard = Rc::clone(&shared);
        let scheme = Rc::clone(&shared);
        let report = Rc::clone(&shared);
        let status = Rc::clone(&shared);
        let notify = Rc::clone(&shared);
        let hold = Rc::clone(&shared);
        vt.on_pty_write(move |_, data| reply.reply.borrow_mut().extend_from_slice(data))?
            .on_bell(move |_| bell.bell.set(true))?
            .on_title_changed(move |_| title.title_changed.set(true))?
            .on_clipboard_write(move |_, write| {
                if let Some(text) = write
                    .contents()
                    .find(|content| content.mime.starts_with(b"text/"))
                {
                    *clipboard.clipboard.borrow_mut() =
                        Some(String::from_utf8_lossy(text.data).into_owned());
                }
                write.reply(Ok(()), false);
            })?
            .on_color_scheme(move |_| {
                Some(if scheme.dark.get() {
                    ColorScheme::Dark
                } else {
                    ColorScheme::Light
                })
            })?
            .on_size(move |_| {
                let size = report.size.get();
                Some(SizeReportSize {
                    rows: size.rows,
                    columns: size.cols,
                    cell_width: u32::from(size.cell_width),
                    cell_height: u32::from(size.cell_height),
                })
            })?
            // Like Ghostty: a VT220-class terminal with ANSI colors.
            .on_device_attributes(|_| {
                Some(DeviceAttributes {
                    primary: PrimaryDeviceAttributes::new(
                        ConformanceLevel::LEVEL_2,
                        &[DeviceAttributeFeature::ANSI_COLOR],
                    ),
                    secondary: SecondaryDeviceAttributes {
                        device_type: DeviceType::VT220,
                        firmware_version: 1,
                        rom_cartridge: 0,
                    },
                    tertiary: TertiaryDeviceAttributes { unit_id: 0 },
                })
            })?
            .on_xtversion(|_| Some(concat!("SuperGIT ", env!("CARGO_PKG_VERSION"))))?
            // Only the root record (empty id): the program itself.
            .on_program_status(move |_, report| {
                if !report.id().is_empty() {
                    return;
                }
                let state = match report.state() {
                    Ok(ProgramStatusState::Idle) => ProgramState::Idle,
                    Ok(ProgramStatusState::Working) => ProgramState::Working,
                    Ok(ProgramStatusState::Done) => ProgramState::Done,
                    Ok(ProgramStatusState::Error) => ProgramState::Error,
                    Ok(ProgramStatusState::Clear) => ProgramState::Cleared,
                    Ok(ProgramStatusState::Blocked) => ProgramState::Blocked(match report.kind() {
                        Ok(ProgramStatusKind::Permission) => BlockedOn::Permission,
                        Ok(ProgramStatusKind::Question) => BlockedOn::Question,
                        Ok(ProgramStatusKind::Auth) => BlockedOn::Auth,
                        _ => BlockedOn::Other,
                    }),
                    _ => return,
                };
                status.status.set(Some(state));
            })?
            .on_render_hold(move |_, held| hold.held.set(held))?
            .on_desktop_notification(move |_, notification| {
                notify.notifications.borrow_mut().push(Notification {
                    title: clean_text(notification.title()),
                    body: clean_text(notification.body()),
                });
            })?;
        let mut terminal = Self {
            vt,
            render: RenderState::new()?,
            row_iter: RowIterator::new()?,
            cell_iter: CellIterator::new()?,
            keys: key::Encoder::new()?,
            key_event: key::Event::new()?,
            shared,
            lines: Vec::new(),
            colors: colors.clone(),
        };
        terminal.apply_colors()?;
        terminal.resize(size)?;
        Ok(terminal)
    }

    /// Process program output.
    pub fn feed(&mut self, bytes: &[u8]) -> Effects {
        self.vt.vt_write(bytes);
        Effects {
            reply: std::mem::take(&mut *self.shared.reply.borrow_mut()),
            bell: self.shared.bell.replace(false),
            title_changed: self.shared.title_changed.replace(false),
            clipboard: self.shared.clipboard.borrow_mut().take(),
            status: self.shared.status.take(),
            notifications: std::mem::take(&mut *self.shared.notifications.borrow_mut()),
        }
    }

    pub fn resize(&mut self, size: Size) -> Result<()> {
        self.shared.size.set(size);
        self.vt.resize(
            size.cols.max(1),
            size.rows.max(1),
            u32::from(size.cell_width),
            u32::from(size.cell_height),
        )?;
        Ok(())
    }

    /// New theme colors, e.g. after the app switched between light and dark.
    pub fn set_colors(&mut self, colors: &Colors) -> Result<()> {
        if *colors == self.colors {
            return Ok(());
        }
        let was_dark = self.colors.dark;
        self.colors = colors.clone();
        self.apply_colors()?;
        // Programs that asked to hear about it (mode 2031) learn the new
        // scheme right away.
        if was_dark != colors.dark && self.vt.mode(Mode::COLOR_SCHEME_REPORT)? {
            let scheme = if colors.dark {
                ColorScheme::Dark
            } else {
                ColorScheme::Light
            };
            let mut buf = [0u8; 32];
            let n = scheme.encode(&mut buf)?;
            self.shared.reply.borrow_mut().extend_from_slice(&buf[..n]);
        }
        Ok(())
    }

    /// The program is in the middle of a synchronized update: keep showing
    /// the previous frame.
    pub fn render_held(&self) -> bool {
        self.shared.held.get()
    }

    /// End a synchronized update the program never finished (the terminal
    /// has no clock; the host times holds out).
    pub fn end_render_hold(&mut self) {
        let _ = self.vt.set_mode(Mode::SYNC_OUTPUT, false);
        self.shared.held.set(false);
    }

    /// Replies queued outside [`Terminal::feed`] (e.g. by `set_colors`).
    pub fn take_reply(&mut self) -> Vec<u8> {
        std::mem::take(&mut *self.shared.reply.borrow_mut())
    }

    fn apply_colors(&mut self) -> Result<()> {
        let colors = &self.colors;
        self.shared.dark.set(colors.dark);
        let mut base = Palette::default();
        for (ix, color) in colors.ansi.iter().enumerate() {
            base.0[ix] = (*color).into();
        }
        let palette = Palette::generate(
            Some(&base),
            None,
            colors.background.into(),
            colors.foreground.into(),
            false,
        );
        self.vt
            .set_default_bg_color(Some(colors.background.into()))?
            .set_default_fg_color(Some(colors.foreground.into()))?
            .set_default_cursor_color(Some(colors.cursor.into()))?
            .set_default_color_palette(Some(palette))?;
        // Every cell may resolve to a new color.
        self.lines.clear();
        Ok(())
    }

    pub fn title(&self) -> String {
        self.vt.title().map(clean_text).unwrap_or_default()
    }

    /// The visible screen. Rows the terminal did not change since the last
    /// call are reused.
    pub fn screen(&mut self) -> Result<Screen> {
        let snapshot = self.render.update(&self.vt)?;
        let cols = snapshot.cols()?;
        let rows = snapshot.rows()?;
        let colors = snapshot.colors()?;
        let background = Rgb::from(colors.background);
        let foreground = Rgb::from(colors.foreground);
        let full = snapshot.dirty()? == Dirty::Full || self.lines.len() != usize::from(rows);
        if full {
            self.lines = vec![Vec::new(); usize::from(rows)];
            // Rebuilding for our own reasons (new colors): visit every row.
            snapshot.set_dirty(Dirty::Full)?;
        }
        let mut rows_iter = self.row_iter.update(&snapshot)?;
        // Only rows that changed (every row after a full invalidation).
        while let Some((y, row)) = rows_iter.next_dirty() {
            let y = usize::from(y);
            if y >= self.lines.len() {
                break;
            }
            {
                let selection = row.selection()?;
                let mut cells = self.cell_iter.update(row)?;
                let mut runs: Vec<Run> = Vec::new();
                let mut text = String::new();
                let mut x: u16 = 0;
                while let Some(cell) = cells.next() {
                    let col = x;
                    x += 1;
                    let wide = match cell.raw_cell()?.wide()? {
                        CellWide::SpacerTail | CellWide::SpacerHead => continue,
                        CellWide::Wide => true,
                        CellWide::Narrow => false,
                    };
                    text.clear();
                    cell.graphemes_utf8(&mut text)?;
                    let selected = selection.is_some_and(|s| col >= s.start_x && col <= s.end_x);
                    let style = if cell.has_styling()? {
                        Some(cell.style()?)
                    } else {
                        None
                    };
                    let mut fg = cell.fg_color()?.map_or(foreground, Rgb::from);
                    let mut bg = cell.bg_color()?.map(Rgb::from);
                    if style.is_some_and(|s| s.inverse) {
                        let new_bg = fg;
                        fg = bg.unwrap_or(background);
                        bg = Some(new_bg);
                    }
                    if style.is_some_and(|s| s.invisible) {
                        text.clear();
                    }
                    if text.is_empty() && bg.is_none() && !selected {
                        continue;
                    }
                    if text.is_empty() {
                        text.push(' ');
                    }
                    let run = Run {
                        col,
                        width: if wide { 2 } else { 1 },
                        text: text.clone(),
                        fg,
                        bg,
                        bold: style.is_some_and(|s| s.bold),
                        italic: style.is_some_and(|s| s.italic),
                        faint: style.is_some_and(|s| s.faint),
                        underline: style.map_or(UnderlineStyle::None, |s| underline(s.underline)),
                        strikethrough: style.is_some_and(|s| s.strikethrough),
                        selected,
                    };
                    push_cell(&mut runs, run);
                }
                self.lines[y] = runs;
            }
        }
        let state = snapshot.cursor()?;
        let cursor = state.viewport.filter(|_| state.visible).map(|at| Cursor {
            col: at.x,
            row: at.y,
            shape: match state.visual_style {
                CursorVisualStyle::Bar => CursorShape::Bar,
                CursorVisualStyle::Underline => CursorShape::Underline,
                CursorVisualStyle::BlockHollow => CursorShape::HollowBlock,
                _ => CursorShape::Block,
            },
            wide: at.at_wide_tail,
            color: colors.cursor.map(Rgb::from),
        });
        snapshot.clean()?;
        let scroll = self
            .vt
            .scrollbar()
            .map(|s| Scroll {
                total: s.total,
                offset: s.offset,
                visible: s.len,
            })
            .unwrap_or_default();
        Ok(Screen {
            cols,
            rows,
            background,
            foreground,
            lines: self.lines.clone(),
            cursor,
            scroll,
        })
    }

    /// Bytes for a key press, in whatever encoding the program asked for
    /// (legacy, or the Kitty keyboard protocol).
    pub fn encode_key(&mut self, input: &KeyInput) -> Result<Vec<u8>> {
        let (logical, unshifted) = logical_key(&input.key);
        let mut mods = Mods::empty();
        mods.set(Mods::SHIFT, input.shift);
        mods.set(Mods::CTRL, input.ctrl);
        mods.set(Mods::SUPER, input.cmd);
        // macOS: Option typing a character (Option+u → ü, layouts that need
        // it) sends that character. Option with anything else is Alt.
        let text = input
            .text
            .as_deref()
            .filter(|t| !t.chars().any(char::is_control));
        let option_typed = input.alt && text.is_some() && input.option_composes;
        mods.set(Mods::ALT, input.alt && !option_typed);
        self.keys.set_options_from_terminal(&self.vt);
        self.key_event
            .set_action(if input.repeat {
                Action::Repeat
            } else {
                Action::Press
            })
            .set_key(logical)
            .set_mods(mods)
            .set_consumed_mods(if input.shift && text.is_some() {
                Mods::SHIFT
            } else {
                Mods::empty()
            })
            .set_composing(false)
            .set_utf8(text.map(str::to_string));
        if let Some(c) = unshifted {
            self.key_event.set_unshifted_codepoint(c);
        }
        let mut out = Vec::new();
        self.keys.encode_to_vec(&self.key_event, &mut out)?;
        Ok(out)
    }

    /// Bytes for pasted text: bracketed when the program asked for it, with
    /// control sequences that could fake input removed.
    pub fn encode_paste(&self, text: &str) -> Result<Vec<u8>> {
        let bracketed = self.vt.mode(Mode::BRACKETED_PASTE)?;
        let mut data = text.as_bytes().to_vec();
        let mut buf = vec![0u8; data.len() + 16];
        let n = libghostty_vt::paste::encode(&mut data, bracketed, &mut buf)
            .map_err(|e| anyhow!("encode paste: {e:?}"))?;
        buf.truncate(n);
        Ok(buf)
    }

    /// Focus gained/lost reports, for programs that asked (mode 1004).
    pub fn encode_focus(&self, focused: bool) -> Vec<u8> {
        if self.vt.mode(Mode::FOCUS_EVENT).unwrap_or(false) {
            if focused { b"\x1b[I" } else { b"\x1b[O" }.to_vec()
        } else {
            Vec::new()
        }
    }

    /// A wheel scroll of `lines` (negative: up, towards history). Programs
    /// on the alternate screen get arrow keys (as in other terminals);
    /// otherwise the view scrolls back.
    pub fn scroll(&mut self, lines: isize) -> Vec<u8> {
        if lines == 0 {
            return Vec::new();
        }
        let alt_screen = self.vt.mode(Mode::ALT_SCREEN_SAVE).unwrap_or(false)
            || self.vt.mode(Mode::ALT_SCREEN).unwrap_or(false);
        if alt_screen && self.vt.mode(Mode::ALT_SCROLL).unwrap_or(true) {
            let app = self.vt.mode(Mode::DECCKM).unwrap_or(false);
            let key: &[u8] = match (lines < 0, app) {
                (true, true) => b"\x1bOA",
                (true, false) => b"\x1b[A",
                (false, true) => b"\x1bOB",
                (false, false) => b"\x1b[B",
            };
            return key.repeat(lines.unsigned_abs());
        }
        self.vt.scroll_viewport(ScrollViewport::Delta(lines));
        Vec::new()
    }

    /// Back to the live screen (typing does this).
    pub fn scroll_to_bottom(&mut self) {
        self.vt.scroll_viewport(ScrollViewport::Bottom);
    }
}

fn underline(u: Underline) -> UnderlineStyle {
    match u {
        Underline::Single => UnderlineStyle::Single,
        Underline::Double => UnderlineStyle::Double,
        Underline::Curly => UnderlineStyle::Curly,
        Underline::Dotted => UnderlineStyle::Dotted,
        Underline::Dashed => UnderlineStyle::Dashed,
        _ => UnderlineStyle::None,
    }
}

/// Append a cell, joining it to the previous run when both are plain ASCII
/// in the same style and adjacent. Anything else (wide, emoji, box drawing)
/// gets its own run so a fallback font's advance can never shift the grid.
fn push_cell(runs: &mut Vec<Run>, cell: Run) {
    let ascii = cell.width == 1 && cell.text.is_ascii();
    if ascii
        && let Some(last) = runs.last_mut()
        && last.text.is_ascii()
        && last.col + last.width == cell.col
        && last.same_style(&cell)
    {
        last.text.push_str(&cell.text);
        last.width += 1;
        return;
    }
    runs.push(cell);
}
