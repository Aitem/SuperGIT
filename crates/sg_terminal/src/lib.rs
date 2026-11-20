//! The embedded terminal's core: a child process on a PTY ([`pty`]) and
//! Ghostty's terminal emulation ([`term`], libghostty-vt) reduced to plain
//! data the app draws. UI-agnostic like the other `sg_*` crates; the GPUI
//! side lives in `supergit/src/terminal.rs`.

pub mod config;
pub mod keys;
pub mod pty;
pub mod term;

pub use keys::KeyInput;
pub use pty::{Pty, PtyEvent, Size, Spawn};
pub use term::{
    BlockedOn, Colors, Cursor, CursorShape, Effects, MouseAction, MouseButton, MouseInput,
    Notification, ProgramState, Rgb, Run, Screen, Scroll, Terminal, UnderlineStyle,
};
