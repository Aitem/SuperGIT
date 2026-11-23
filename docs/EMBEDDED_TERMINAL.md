# Embedded terminal for agent sessions (libghostty)

Status: first cut implemented (branch `libghostty-based-chat`, 2026-11-19).

The agent panel opens conversations either as SuperGIT's native chat or as
the agent's own TUI (`claude`, `codex`) in an embedded terminal. A
`Chat | Terminal` switch in Settings, repeated under the agent choice on a
new conversation's screen, picks which one New, Resume last and session rows
open (`settings.json`: `agent_view`). The chat stays the default.

Why bother when the chat exists: the TUI has every CLI feature on day one
(slash commands, `/model`, plan approval, `@` mentions, images, new UI). The
chat has only what SuperGIT reimplements.

## Which libghostty

| | libghostty-vt | full libghostty (what Ghostty.app embeds) |
|---|---|---|
| What it is | VT parser, terminal state, scrollback, key/mouse/paste encoders. The host draws. | All of that plus Ghostty's renderer, drawing into a native view. |
| API | 1.0.1, stable C ABI (2026-11-06). MIT. | Not covered by the 1.0 promise. |
| Fits GPUI | Yes: the grid is drawn like any other GPUI view. | No: a native child view sits on top of GPUI's single Metal layer (tooltips and dialogs end up under it) and breaks focus and offscreen screenshots. |

Chosen: libghostty-vt plus a GPUI-drawn grid. Over `alacritty_terminal` it
brings the key encoder (Kitty keyboard protocol: Shift+Enter, Ctrl/Alt
chords), query replies, synchronized output, and the program-status
protocol below.

## How it is built

```
crates/sg_terminal/          UI-agnostic, no GPUI types
  pty.rs     child on a PTY (rustix: openpt, setsid + TIOCSCTTY, TIOCSWINSZ);
             reader, writer and wait threads; hang-up on drop
  term.rs    libghostty-vt Terminal → `Screen` (rows of styled runs, cursor,
             scroll position), rebuilt only for dirty rows; key/paste/focus
             encoding; effects: replies, title, bell, OSC 52, OSC 7501
             program status, render holds (mode 2026)
  keys.rs    GPUI key names → Ghostty logical keys
crates/supergit/src/
  terminal.rs             TerminalView: canvas element, input, focus, wheel
  terminal/theme.rs       app colors → terminal palette, and a Claude theme
  chat/terminal_mode.rs   a ChatSession shown as a TUI: launch, status
```

- **Bindings.** `libghostty-vt` comes from git (Uzaaft/libghostty-rs at a
  pinned rev that targets vt 1.0.1). The crates.io release (0.2.2) still pins
  a pre-1.0 Ghostty that only builds with Zig 0.15.
- **Build.** Zig 0.17.x on `PATH`, nothing else. The bindings' build script
  runs `zig build` in `GHOSTTY_SOURCE_DIR`, and `.cargo/config.toml` points
  that at `crates/sg_terminal/ghostty`, a small Zig project:
  - its `build.zig.zon` pins the signed libghostty-vt source release
    (`release.files.ghostty.org/libghostty-vt/1.0.1/libghostty-vt-1.0.1.tar.gz`,
    4.5 MB) by content hash, so Zig's package manager downloads it once
    into its global cache, shared by every worktree and target dir;
  - its `build.zig` runs Ghostty's own `zig build -Demit-lib-vt` inside the
    fetched package. As a Zig dependency Ghostty only hands out the bare
    archive and leaves its SIMD libraries to Zig's `linkLibrary`. Only the
    top-level build merges them into one self-contained `libghostty-vt.a`,
    which is what Cargo links.

  This replaced an earlier `scripts/fetch-ghostty.sh` that vendored a
  37 MB repo snapshot into each checkout. CI installs Zig with
  `mlugg/setup-zig`. libghostty links statically, so nothing extra goes
  into the `.app`.
- **Threading.** libghostty handles are `!Send`, so the terminal lives on
  the main thread and the PTY reader only moves bytes. Each batch of output
  gets one repaint. While a program holds a synchronized frame, the last
  frame stays up; a hold longer than 1 s is ended by the host.
- **Drawing.** Background quads per style run, then text. ASCII runs are
  shaped as one line snapped to the cell grid. Every other character gets
  its own cell, so fallback-font advances can't shift the grid. Block
  elements (U+2580–U+259F, Claude's logo) are drawn as rectangles. The
  cursor is a block, bar or underline, hollow when unfocused.
- **Keys.** GPUI dispatches bindings before key listeners, so the
  `Terminal` key context binds `NoAction` to Tab and Shift+Tab (and on Linux
  the app's `ctrl-` shortcuts and `ctrl-c`), handing them to the program.
  `cmd-c`/`cmd-v`/`cmd-a` (`ctrl-shift-c/v/a` on Linux) copy, paste and
  select all. Paste is bracketed and sanitized by libghostty.
- **Selection.** libghostty's selection gestures: drag, double click for a
  word, triple click for a line, Option-drag for a block, autoscroll when
  dragging past the top or bottom. The selection follows the content as
  output arrives, typing drops it, and copy uses libghostty's formatter
  (plain text, soft-wrapped lines joined, trailing blanks trimmed). Right
  click opens Copy, Paste and Select All, with their shortcuts.
- **Mouse reporting.** Programs that turn on mouse tracking (1000/1002/1003,
  in X10, SGR, urxvt or SGR-pixel coordinates) get clicks, drags, motion
  and the wheel, encoded by libghostty; right clicks go to the program
  then, not to the menu. Shift selects text anyway. Claude
  Code's inline UI doesn't track the mouse, so selection works without it.

## Launching the agent

| | new | resume (in the session's start cwd) |
|---|---|---|
| Claude | `claude --session-id <uuid>` | `claude --resume <id>` (keeps the id) |
| Codex | `codex` | `codex resume <id>` |

The environment gets `TERM=xterm-256color`, `COLORTERM=truecolor` and
`TERM_PROGRAM=SuperGIT`. Variables describing SuperGIT's own surroundings
are removed (`CLAUDECODE`, Claude Code session markers, `TMUX`,
`GHOSTTY_*`, `KITTY_*`, …). Without that, a SuperGIT started from inside
Claude Code makes the child think it is a sub-session ("transcript saving is
off").

- New (`+`, MOD-n) shows the agent and view choice and waits for Start, in
  either view (a chat shows its composer only then);
  "Start {agent} here", asking about a selection and issue drafts start at
  once. A conversation with history resumes its TUI once it is on screen
  (without taking the focus); off screen, nothing starts. A Claude session
  that Claude Code's registry of live sessions (`~/.claude/sessions/<pid>.json`,
  the pid checked with signal 0) shows open in another process keeps Resume
  and says why: two processes would write one transcript.
- Opening a session that is already open focuses its tab, whichever mode
  that tab uses.
- When the agent exits, a bar offers Resume (or Start again).

## Colors

- **Terminal.** The default background and foreground, the cursor and the
  16 ANSI colors come from the GPUI theme. The other 240 palette entries are
  generated from them as Ghostty does. They follow appearance changes live,
  and programs that subscribed (mode 2031) are told the new scheme.
- **Claude.** SuperGIT writes a small plugin to
  `<support dir>/claude-terminal/plugin` with two themes, `supergit:light`
  and `supergit:dark`. Their overrides are built from the app's colors:
  text, muted text, borders, blue/magenta/cyan accents, success/danger/
  warning, and diff colors made the same way as the app's diff view (12%
  tint per line, 30% per changed word). SuperGIT then launches `claude
  --plugin-dir … --settings <support>/claude-terminal/settings.json`, and
  that file holds `{"theme": "custom:supergit:<mode>"}`. Nothing in
  `~/.claude` changes.
- **Limits.** Claude loads plugin themes a moment after it starts, so the
  welcome banner is drawn in its built-in dark theme. Plain text uses the
  terminal's default color and stays readable in both appearances. A Claude
  that is already running keeps its theme until it restarts, unless it
  reloads `--settings` files (not verified).

## Font

The terminal (agent TUIs and the terminal panel) uses the font of the
user's Ghostty: `font-family` and `font-size` from Ghostty's config files
(`sg_terminal::config`: `~/.config/ghostty/config`, then on macOS
`~/Library/Application Support/com.mitchellh.ghostty/config`, both also as
`config.ghostty`, with `config-file` includes). The first listed family
that is installed wins. Without a config, or with none of its fonts
installed, it is the theme's monospace font (Menlo 13 on macOS). The files
are read once at startup, off the main thread; Ghostty's colors and theme
are not used (the terminal keeps the app's colors).

`cmd-=` (or `cmd-+`), `cmd--` and `cmd-0` (`ctrl-` on Linux) inside a
terminal, or Bigger Text / Smaller Text / Default Text Size in its
right-click menu, change the text size of every embedded terminal by 1 pt
(6–72 pt). The size picked is kept in `settings.json`
(`terminal_font_size`); `cmd-0` forgets it and goes back to Ghostty's.

## Status and attention

Claude Code asks `OSC 7501 ; ?` (the program-status protocol). libghostty
answers, and Claude then reports `working`, `blocked` (`permission` or
`question`) and `done`. These map onto the tab's attention states:

- running
- needs approval
- needs an answer
- ✓ unread

Background notifications therefore work as they do for chats.

Programs that don't report status (Codex) fall back to output activity. The
echo of a key just typed doesn't count, and 3 s of quiet after working means
done; a bell means needs input.

Text typed into a terminal tab waits until the TUI has drawn its screen and
gone quiet for 0.8 s. Claude reports no state before its first turn, so this
is the only readiness signal. This covers issue drafts, Ask agent context
(pasted, not sent) and `SUPERGIT_DEBUG_CHAT=prompt:` (pasted and sent).

## Terminal panel (`cmd-j`)

As in VS Code: `cmd-j` (`ctrl-j` on Linux), View ▸ Toggle Terminal, the
sidebar's "⋯" menu, or the Terminal button in the status bar (panel icon,
lit while open) show a panel under the review, above the status bar, with
the user's shell (`$SHELL`, a login shell on macOS)
in the selected worktree's folder (`app/terminal_panel.rs`).

- One shell per worktree, started the first time the panel shows that
  worktree. Shells keep running while the panel is hidden or another
  worktree or project is shown. Cleaning up a worktree or closing its
  project ends its shell.
- A shell that exits (`exit`, `ctrl-d`) is gone, and the panel hides if it
  showed it; the next `cmd-j` starts a new one. Kill in the header does the
  same for a stuck shell. SuperGIT no longer opens external terminal apps
  (`SUPERGIT_TERMINAL` is gone).
- The panel spans the review only: the sidebar and the agent panel keep
  their full height. Its height is saved per project (`terminal_height`);
  whether it is open is not.
- Same environment as agent TUIs (`terminal::spawn_in`): `TERM`,
  `COLORTERM`, `TERM_PROGRAM=SuperGIT`, without SuperGIT's own
  surroundings, so `claude` started in it is not a sub-session.
- `ctrl-j` stays the app's inside the terminal on Linux (the shell gets
  Enter for a new line anyway), so the panel can be hidden from inside it.
- A session row's Terminal button (Agents tab) resumes the session in the
  agent panel as the agent's TUI; with the panel switched to Terminal, Open
  does that already and the button is not shown.
- Screenshot check: `SUPERGIT_DEBUG_TERMINAL_PANEL=1` opens the panel once
  the worktrees load.

## Verified

- `cargo test -p sg_terminal`:
  - styled runs and inverse colors
  - wide characters
  - DA1 and color-scheme replies
  - Kitty keyboard encoding (Shift+Enter → `ESC[13;2u`)
  - Cyrillic input
  - bracketed paste
  - OSC 7501 and OSC 777
  - render holds
  - a real child on a PTY (`stty size`)
- Screenshot builds, light and dark: the real Claude Code 2.1.328 TUI in the
  panel. A prompt sent through `SUPERGIT_DEBUG_CHAT=prompt:` reaches Claude,
  and the tab shows "● running".
- `cargo clippy --workspace --all-targets --all-features -D warnings` and
  `cargo test --workspace` pass on macOS.

Not verified yet: CI (the workflow changes have not run), Linux, and Codex's
TUI in the terminal.

## Next

- IME and dead keys (a GPUI `InputHandler`). Plain layouts, Cyrillic
  included, already work through `key_char`.
- Clickable OSC 8 links and file paths that open in the diff view.
- Switching an idle conversation between chat and terminal (same session
  id: stop one driver, resume in the other).
- Several shells per worktree (tabs in the terminal panel).
- Codex: new sessions get no id until its rollout exists, so a restored
  Codex terminal tab starts a fresh session.
- When `libghostty-vt` reaches crates.io with 1.0 support, move off the git
  dependency (bump the crate and `ghostty/build.zig.zon` together). An
  upstream change letting the sys crate fetch through Zig instead of
  `git clone` would make our Zig wrapper unnecessary.

## Sources

- libghostty-vt 1.0 announcement: https://mitchellh.com/writing/libghostty-1-0
- Docs: https://lib.ghostty.org/
- Rust bindings: https://github.com/Uzaaft/libghostty-rs
- GPUI + libghostty-vt demo: https://xuanwo.io/2026/01-gpui-ghostty/
