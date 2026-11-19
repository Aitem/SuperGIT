//! A real PTY child and Ghostty's emulation together.
use futures::StreamExt as _;
use sg_terminal::{Colors, KeyInput, Pty, PtyEvent, Rgb, Size, Spawn, Terminal};

const SIZE: Size = Size {
    cols: 40,
    rows: 6,
    cell_width: 8,
    cell_height: 16,
};

fn line_text(screen: &sg_terminal::Screen, row: usize) -> String {
    let mut text = String::new();
    for run in &screen.lines[row] {
        while text.chars().count() < usize::from(run.col) {
            text.push(' ');
        }
        text.push_str(&run.text);
    }
    text
}

#[test]
fn styled_output_becomes_runs() {
    let colors = Colors::default();
    let mut term = Terminal::new(SIZE, &colors).unwrap();
    term.feed(b"\x1b[1;31mhello\x1b[0m world\r\n\x1b[7minv\x1b[0m");
    let screen = term.screen().unwrap();
    assert_eq!(line_text(&screen, 0), "hello world");
    let hello = &screen.lines[0][0];
    assert_eq!(hello.text, "hello");
    assert!(hello.bold);
    assert_eq!(hello.fg, colors.ansi[1]);
    // Inverse: the default colors swap.
    let inv = &screen.lines[1][0];
    assert_eq!(inv.text, "inv");
    assert_eq!(inv.bg, Some(colors.foreground));
    assert_eq!(inv.fg, colors.background);
}

#[test]
fn wide_characters_take_two_columns() {
    let mut term = Terminal::new(SIZE, &Colors::default()).unwrap();
    term.feed("a日b".as_bytes());
    let screen = term.screen().unwrap();
    let runs = &screen.lines[0];
    assert_eq!(runs.len(), 3);
    assert_eq!(
        (runs[1].col, runs[1].width, runs[1].text.as_str()),
        (1, 2, "日")
    );
    assert_eq!((runs[2].col, runs[2].text.as_str()), (3, "b"));
}

#[test]
fn queries_get_replies_and_theme_colors_apply() {
    let colors = Colors {
        background: Rgb::new(250, 250, 250),
        foreground: Rgb::new(20, 20, 20),
        dark: false,
        ..Colors::default()
    };
    let mut term = Terminal::new(SIZE, &colors).unwrap();
    // Primary device attributes, and "dark or light?".
    let effects = term.feed(b"\x1b[c\x1b[?996n");
    let reply = String::from_utf8(effects.reply).unwrap();
    assert!(reply.starts_with("\x1b[?62;22c"), "{reply:?}");
    assert!(reply.ends_with("\x1b[?997;2n"), "{reply:?}");
    let screen = term.screen().unwrap();
    assert_eq!(screen.background, colors.background);
}

#[test]
fn keys_follow_the_programs_keyboard_mode() {
    let mut term = Terminal::new(SIZE, &Colors::default()).unwrap();
    let enter = KeyInput {
        key: "enter".into(),
        ..Default::default()
    };
    let shift_enter = KeyInput {
        shift: true,
        ..enter.clone()
    };
    assert_eq!(term.encode_key(&enter).unwrap(), b"\r");
    // The Kitty keyboard protocol (what Claude Code asks for) tells
    // Shift+Enter apart.
    term.feed(b"\x1b[>1u");
    assert_eq!(term.encode_key(&enter).unwrap(), b"\r");
    assert_eq!(term.encode_key(&shift_enter).unwrap(), b"\x1b[13;2u");
    let ctrl_c = KeyInput {
        key: "c".into(),
        ctrl: true,
        ..Default::default()
    };
    assert_eq!(term.encode_key(&ctrl_c).unwrap(), b"\x1b[99;5u");
    let typed = KeyInput {
        key: "a".into(),
        text: Some("ф".into()),
        ..Default::default()
    };
    assert_eq!(term.encode_key(&typed).unwrap(), "ф".as_bytes());
}

#[test]
fn bracketed_paste() {
    let mut term = Terminal::new(SIZE, &Colors::default()).unwrap();
    assert_eq!(term.encode_paste("x").unwrap(), b"x");
    term.feed(b"\x1b[?2004h");
    assert_eq!(term.encode_paste("x").unwrap(), b"\x1b[200~x\x1b[201~");
}

#[test]
fn a_child_on_a_pty() {
    let dir = std::env::temp_dir();
    let (pty, mut events) = Pty::spawn(&Spawn {
        program: "/bin/sh".into(),
        args: vec![
            "-c".into(),
            // A terminal for stdin, and the size we set.
            "test -t 0 && stty size && printf 'done'".into(),
        ],
        cwd: dir,
        env: vec![("TERM".into(), "xterm-256color".into())],
        env_remove: Vec::new(),
        size: SIZE,
    })
    .unwrap();
    let mut term = Terminal::new(SIZE, &Colors::default()).unwrap();
    let code = futures::executor::block_on(async {
        let mut code = None;
        while let Some(event) = events.next().await {
            match event {
                PtyEvent::Output(bytes) => {
                    term.feed(&bytes);
                }
                PtyEvent::Exited(c) => {
                    code = c;
                    break;
                }
            }
        }
        code
    });
    // Output may trail the exit notice: drain what is left.
    while let Ok(PtyEvent::Output(bytes)) = events.try_recv() {
        term.feed(&bytes);
    }
    drop(pty);
    assert_eq!(code, Some(0));
    let screen = term.screen().unwrap();
    assert_eq!(line_text(&screen, 0), "6 40");
    assert_eq!(line_text(&screen, 1), "done");
}

#[test]
fn program_status_and_notifications() {
    use sg_terminal::{BlockedOn, ProgramState};
    let mut term = Terminal::new(SIZE, &Colors::default()).unwrap();
    // Claude Code asks whether the terminal speaks OSC 7501 first.
    let effects = term.feed(b"\x1b]7501;?\x07");
    assert!(
        String::from_utf8_lossy(&effects.reply).contains("7501;?:states="),
        "{:?}",
        String::from_utf8_lossy(&effects.reply)
    );
    let effects = term.feed(b"\x1b]7501;state=working\x1b\\");
    assert_eq!(effects.status, Some(ProgramState::Working));
    let effects = term.feed(b"\x1b]7501;state=blocked:kind=permission\x1b\\");
    assert_eq!(
        effects.status,
        Some(ProgramState::Blocked(BlockedOn::Permission))
    );
    // Records other than the program's own are not its state.
    let effects = term.feed(b"\x1b]7501;state=done:id=task\x1b\\");
    assert_eq!(effects.status, None);
    let effects = term.feed(b"\x1b]777;notify;Claude Code;Needs your approval\x07");
    assert_eq!(effects.notifications.len(), 1);
    assert_eq!(effects.notifications[0].title, "Claude Code");
    assert_eq!(effects.notifications[0].body, "Needs your approval");
}

#[test]
fn synchronized_output_holds_the_frame() {
    let mut term = Terminal::new(SIZE, &Colors::default()).unwrap();
    term.feed(b"\x1b[?2026h");
    assert!(term.render_held());
    term.feed(b"\x1b[?2026l");
    assert!(!term.render_held());
    term.feed(b"\x1b[?2026h");
    term.end_render_hold();
    assert!(!term.render_held());
}

/// Cells of 10×20 px after 6 px of padding, as the view lays them out.
const SURFACE: sg_terminal::term::Surface = sg_terminal::term::Surface {
    padding: 6.,
    cell_width: 10.,
    cell_height: 20.,
    height: 6. + 20. * 6.,
};

/// A point in a cell, in view pixels: `at` near its left edge (where a
/// press starts a selection), `past` beyond its middle (where a drag takes
/// the cell in, as with native text).
fn at(col: u16, row: u16) -> (f32, f32) {
    point(col, row, 0.2)
}

fn past(col: u16, row: u16) -> (f32, f32) {
    point(col, row, 0.8)
}

fn point(col: u16, row: u16, x: f32) -> (f32, f32) {
    (
        SURFACE.padding + (f32::from(col) + x) * SURFACE.cell_width,
        SURFACE.padding + (f32::from(row) + 0.5) * SURFACE.cell_height,
    )
}

#[test]
fn dragging_selects_and_copies_text() {
    let mut term = Terminal::new(SIZE, &Colors::default()).unwrap();
    term.feed(b"hello world\r\nsecond line");
    assert_eq!(term.selection_text().unwrap(), None);
    let (x, y) = at(0, 0);
    term.select_press(x, y, &SURFACE).unwrap();
    let (x, y) = past(4, 0);
    assert_eq!(term.select_drag(x, y, false, &SURFACE).unwrap(), None);
    term.select_release(x, y, &SURFACE).unwrap();
    assert_eq!(term.selection_text().unwrap().as_deref(), Some("hello"));
    // The screen marks the selected cells.
    let screen = term.screen().unwrap();
    let selected: String = screen.lines[0]
        .iter()
        .filter(|run| run.selected)
        .map(|run| run.text.as_str())
        .collect();
    assert_eq!(selected, "hello");
    // Across lines.
    let (x, y) = at(6, 0);
    term.select_press(x, y, &SURFACE).unwrap();
    let (x, y) = past(5, 1);
    term.select_drag(x, y, false, &SURFACE).unwrap();
    assert_eq!(
        term.selection_text().unwrap().as_deref(),
        Some("world\nsecond")
    );
    term.clear_selection();
    assert!(!term.has_selection());
}

#[test]
fn double_click_selects_a_word_and_triple_click_the_line() {
    let mut term = Terminal::new(SIZE, &Colors::default()).unwrap();
    term.feed(b"hello world");
    let (x, y) = at(7, 0);
    term.select_press(x, y, &SURFACE).unwrap();
    term.select_release(x, y, &SURFACE).unwrap();
    term.select_press(x, y, &SURFACE).unwrap();
    assert_eq!(term.selection_text().unwrap().as_deref(), Some("world"));
    term.select_release(x, y, &SURFACE).unwrap();
    term.select_press(x, y, &SURFACE).unwrap();
    assert_eq!(
        term.selection_text().unwrap().as_deref(),
        Some("hello world")
    );
}

#[test]
fn select_all_and_dragging_past_the_edge() {
    let mut term = Terminal::new(SIZE, &Colors::default()).unwrap();
    term.feed(b"one\r\ntwo");
    term.select_all().unwrap();
    let all = term.selection_text().unwrap().unwrap();
    assert!(all.contains("one") && all.contains("two"), "{all:?}");
    // Below the view: asks to scroll down.
    let (x, _) = at(1, 0);
    term.select_press(x, at(0, 0).1, &SURFACE).unwrap();
    let below = SURFACE.height + 30.;
    assert_eq!(
        term.select_drag(x, below, false, &SURFACE).unwrap(),
        Some(sg_terminal::term::SelectScroll::Down)
    );
}

#[test]
fn the_selection_stays_on_its_text_as_output_scrolls() {
    let mut term = Terminal::new(SIZE, &Colors::default()).unwrap();
    term.feed(b"keep me\r\n");
    let (x, y) = at(0, 0);
    term.select_press(x, y, &SURFACE).unwrap();
    let (x, y) = past(3, 0);
    term.select_drag(x, y, false, &SURFACE).unwrap();
    // Ten more lines push "keep me" into the scrollback.
    for n in 0..10 {
        term.feed(format!("line {n}\r\n").as_bytes());
    }
    assert_eq!(term.selection_text().unwrap().as_deref(), Some("keep"));
}
