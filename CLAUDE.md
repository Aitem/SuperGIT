# SuperGIT — notes for agents

Native Git GUI for agent-driven development (worktrees + Claude Code and
Codex sessions + commits + PRs). Rust 2024, GPUI through `gpui-kit`. Personal tool
for the author and colleagues: macOS and Linux, GitHub only (via `gh`).

Platform differences stay behind small seams; keep both working:
- `notifications.rs` → `notifications/macos.rs` (UserNotifications, objc2;
  a macOS-only dependency) or `notifications/freedesktop.rs` (notify-rust
  over D-Bus, click = default action).
- Shortcuts: `shortcut!("…")` in main.rs (cmd/ctrl) for bindings, `MOD` in
  tooltips — never a literal `cmd-`. gpui's `secondary-` does not parse in
  `KeyBinding::new` (panics at startup).
- Menus: `cx.set_menus` on macOS; elsewhere `set_app_menus` copies them into
  gpui-component's `AppMenuBar`, drawn in the title bar (`menu_bar`).
- Window chrome: on macOS the side panels reach the window's top edge
  (`FULL_HEIGHT_PANELS`) and the window buttons sit in the first row: the
  sidebar's header, or beside its rail when it is collapsed
  (`sync_window_buttons`). A row they sit in starts `WINDOW_BUTTONS_INSET`
  further right and moves the window through `window_drag_area`. Elsewhere,
  and on macOS without a sidebar, a `TitleBar` row spans the window. Keep
  gpui-component's `TitleBar` out of columns: it is never narrower than its
  content and spills over the neighbours.
- `launch.rs`: AppleScript/`open` on macOS; Linux terminals by binary,
  `xdg-open`, editor CLIs on `PATH`. `launch::FILE_MANAGER` labels Finder.
- Paths: `settings::support_dir()` (`~/.config/supergit` on Linux) and
  `settings::xdg_dir`; Linux single instance via `single_instance.rs`
  (Unix socket; macOS uses LaunchServices + `on_open_urls`).
- Screenshot builds: `render_to_image` exists only on macOS.

## Layout

```
crates/
  sg_git/      git core: worktrees, status, log, diffs (git CLI, porcelain -z);
               difftastic.rs runs the optional `difft` and aligns its JSON;
               submodule.rs inlines changed submodules' own diffs
  sg_github/   pull requests through the `gh` CLI
  sg_agents/   provider-neutral agent layer: `Provider`, chat model
               (chat.rs: Conversation/Item/PermissionRequest), `AgentProcess`
               trait (process.rs); Claude Code session discovery (lib.rs)
  sg_claude/   native Claude chat: `claude` stream-json process + protocol
               (process.rs), event → chat items fold (conversation.rs),
               tool summaries/diffs (tools.rs)
  sg_terminal/ embedded terminal core: PTY (rustix) + Ghostty's emulation
               (libghostty-vt) reduced to plain `Screen` runs; key/paste
               encoding, program status (OSC 7501), render holds (2026)
  sg_codex/    native Codex chat: CLI discovery + version check (install.rs),
               `codex app-server` driver (process.rs), fold (conversation.rs),
               item summaries/diffs (tools.rs), rollout discovery (sessions.rs)
  supergit/    the app (GPUI): main.rs, app.rs (view), model.rs (background
               loading), launch.rs (terminals/editor/browser), dump.rs,
               screenshot.rs (feature `screenshot`), difft_view.rs (the
               difftastic diff view), syntax.rs (syntect highlight names),
               submodules.rs (submodule rows and diff cards), terminal.rs
               (GPUI terminal view; terminal/theme.rs: app colors for it and
               for Claude), chat/terminal_mode.rs (a conversation as a TUI)
docs/ROADMAP.md
```

## Native Claude (sg_claude + supergit/src/chat.rs)

Same protocol as the Agent SDKs and the VS Code extension:
`claude -p --input-format stream-json --output-format stream-json --verbose
--include-partial-messages --permission-prompt-tool stdio [--resume=<id>]
[--append-system-prompt <initial prompt>]`.
- Host → CLI (stdin JSON lines): `control_request {subtype: initialize}` first,
  user turns `{"type":"user","message":{"role":"user","content":…}}`,
  `interrupt`, `set_permission_mode`, and `control_response` to permission
  prompts: `{"behavior":"allow","updatedInput":…,"updatedPermissions"?}` /
  `{"behavior":"deny","message":…}`.
- CLI → host: `system/init` (session_id, model, cwd), `stream_event` partial
  deltas, `assistant` (one content block per event), `user` (tool_result +
  `tool_use_result` with `structuredPatch` for edits), `control_request
  {subtype: can_use_tool, tool_name, display_name, description, input,
  permission_suggestions, tool_use_id}`, `result` (turn end, cost).
- Without `--permission-prompt-tool stdio` prompts are silently denied.
- Unsupported control requests must get an error `control_response`.
- Permission modes: `default` (alias `manual`), `acceptEdits`, `plan`,
  `auto`, `bypassPermissions`, `dontAsk`. `set_permission_mode` gets a
  `control_response` (error when refused: `auto_mode_unavailable`,
  `bypass_not_launched` — bypass needs a session launched for it) and, on
  success, `system/status {permissionMode}`; the chat follows that echo.
  Chats start in `auto` (`--permission-mode auto` always passed); where auto
  is unavailable the CLI falls back to `default` and `system/init` says so.
- Auto mode: the classifier's refusals arrive as `system/permission_denied`
  (`decision_reason_type: "classifier"`), slow checks as
  `system/permission_check_status` (`checking`/`done`), escalations as
  ordinary `can_use_tool` with `decision_reason` (may carry ANSI escapes).
- `AskUserQuestion` asks permission like any tool; `sg_claude::questions`
  turns it into a question form (`Item::UserInput`, shared with Codex).
  Answer = allow with `updatedInput` = the input plus `answers` {question
  text → label or typed text, an array for `multiSelect`}; Decline = deny.
  The CLI adds no "Other" option: the form always offers free text.
  Malformed input stays an ordinary permission prompt. A question never
  gets a tool row: live the permission request brings the form, in history
  the `tool_use` does (answers from `toolUseResult.answers`).
- What Claude is doing between messages: `system/status {status}` is
  `requesting`, `compacting` (repeated while it lasts) or null;
  `system/thinking_tokens {estimated_tokens}` while it thinks (thinking text
  itself is empty); a `stream_event` `content_block_start` of a `tool_use`
  names the tool before its input has streamed (the row shows from then).
  `compact_boundary` carries `compact_metadata {pre_tokens, post_tokens}`.
- The user's Stop ends the turn with an *error* `result`
  (`error_during_execution`, `terminal_reason: aborted_streaming |
  aborted_tools`): `conversation::was_stopped` → `interrupt_turn`, a notice,
  not a failed turn. Other failures: `result`, `errors[]` (minus
  `[ede_diagnostic]`), then the reason codes in words.
- Permission suggestions: `addRules` names the rule (`ruleContent`, often
  narrower than the tool) and where it is saved (`destination`: `session`,
  `localSettings`, `projectSettings`, `userSettings`); the button says both.
- A background task that ends sends `system/task_notification`; one in the
  foreground does too, just before its tool result (no notice for those).
- `Conversation::from_transcript` replays a session jsonl for history.
  Transcripts hold no `result`: a turn ends at `system/turn_duration` (TUI
  sessions) or after a message with `stop_reason: end_turn`. A message sent
  while Claude worked is only an `attachment {type: queued_command}`; a
  slash command is a `<command-name>` user record, its output
  `system/local_command`; compaction is `system/compact_boundary`
  (`compactMetadata {preTokens, postTokens}`).
- The chat's look: `chat/look.rs` (text sizes, timeline marks, code blocks,
  durations), `chat/timeline.rs` (which rows show, runs of calls),
  `chat/activity.rs` (the line above the composer saying what the agent is
  doing, with a clock). Bash rows read as their `description`; unfolded
  they show the command and the end of its output.
- Live test against the real CLI (costs cents): `cargo test -p sg_claude -- --ignored`.
- UI checks: `SUPERGIT_DEBUG_CHAT=resume` or `SUPERGIT_DEBUG_CHAT='prompt:…'`
  (screenshot builds only) opens/sends on startup, then screenshot it.

## Embedded terminal (sg_terminal + supergit/src/terminal.rs)

`Settings::agent_view` (`Chat | Terminal` in Settings and on a new
conversation's screen)
decides whether New / Resume open SuperGIT's chat or the agent's own TUI.
Design notes: docs/EMBEDDED_TERMINAL.md.
- Build: needs **Zig 0.17.x** on PATH, nothing else. libghostty-vt's Rust
  bindings come from Uzaaft/libghostty-rs at a pinned git rev (crates.io
  still pins a pre-1.0 Ghostty that needs Zig 0.15). `.cargo/config.toml`
  points `GHOSTTY_SOURCE_DIR` at `crates/sg_terminal/ghostty`, a tiny Zig
  project: its `build.zig.zon` pins the signed libghostty-vt release (Zig
  fetches it once into its global cache, shared by every worktree), and its
  `build.zig` runs Ghostty's own `zig build -Demit-lib-vt` in the fetched
  package (only that build merges the SIMD archives into one
  `libghostty-vt.a`). Bump the release and the bindings' rev together (their
  build.rs `GHOSTTY_COMMIT` is the `vt-X.Y.Z` tag); new hash: `zig fetch
  <url>`. Cargo does not watch those files: after editing them, `cargo clean
  -p libghostty-vt-sys`.
- libghostty handles are `!Send`: the `Terminal` lives on the main thread,
  the PTY reader thread only moves bytes; one repaint per batch, and the last
  frame stays up while a program holds output (mode 2026, 1 s timeout).
- Launch: `claude --session-id <new uuid>` / `--resume <id>` in the
  session's cwd, plus `--plugin-dir <support>/claude-terminal/plugin
  --settings <support>/claude-terminal/settings.json`: a generated plugin
  with themes `supergit:light|dark` built from the GPUI theme, picked per
  appearance (`custom:supergit:dark`). Nothing in `~/.claude` changes.
  Session/terminal variables of the parent (`CLAUDECODE`, `TMUX`, …) are
  removed; `TERM=xterm-256color`, `TERM_PROGRAM=SuperGIT`.
- Status: Claude Code asks `OSC 7501 ; ?` and then reports working /
  blocked (permission, question) / done; that maps onto `Attention`. Without
  reports (Codex) status comes from output activity. Typed-in text (issue
  drafts, attached context, `SUPERGIT_DEBUG_CHAT=prompt:`) waits until the
  TUI has drawn and gone quiet.
- Keys: GPUI runs bindings before key listeners, so the `Terminal` key
  context binds `NoAction` to Tab/Shift-Tab (and on Linux the app's `ctrl-`
  shortcuts) to release them to the program; `cmd-c`/`cmd-v`/`cmd-a`
  (`ctrl-shift-c/v/a`) are `terminal::Copy`/`Paste`/`SelectAll`. Keys are
  encoded by libghostty (Kitty keyboard protocol: Shift+Enter is `ESC[13;2u`).
- Selection: libghostty's gesture state machine (click, double click word,
  triple click line, Option-drag block, autoscroll past the edges) and its
  formatter for copying (plain, soft wraps joined, trimmed). Drag and release
  listen on the whole window (registered in the canvas paint); typing drops
  the selection. Right click: Copy / Paste / Select All (the same actions,
  `action_context` = the terminal). Programs that track the mouse get
  clicks (right ones too), drags, motion and the wheel instead (libghostty's
  mouse encoder); Shift still selects.
- Font: Ghostty's `font-family` / `font-size` (`sg_terminal::config` reads
  its config files at startup; `terminal::TerminalFont` global), else the
  theme's mono font. Colors stay the app's. `cmd-=`/`cmd-+`, `cmd--`,
  `cmd-0` in the `Terminal` context (and its right-click menu) step the
  size of every terminal by 1 pt (6–72), saved as `terminal_font_size`.
- Terminal panel (`app/terminal_panel.rs`): `cmd-j` / View ▸ Toggle
  Terminal / the status bar's Terminal button / the sidebar's "⋯" menu show
  the user's shell under the review, one per worktree in its folder; shells
  outlive hiding and worktree switches, end on exit, Kill, worktree cleanup
  or project close. `terminal::spawn_in` gives shells and agent TUIs the
  same environment. Screenshot check: `SUPERGIT_DEBUG_TERMINAL_PANEL=1`.
- Tests: `cargo test -p sg_terminal` (emulation, keys, a real PTY child).
  UI check: write `{"agent_view":"terminal"}` to
  `$SUPERGIT_STATE_DIR/settings.json`, then `SUPERGIT_DEBUG_CHAT=new` (or
  `resume`, `prompt:…`) in a screenshot build; `SUPERGIT_DEBUG_TERMINAL=select`
  also drags a selection over the first rows once the TUI is ready, `=menu`
  then right-clicks it (the context menu).

## Native Codex (sg_codex)

`codex app-server` over stdio, JSON-RPC without the `jsonrpc` field, one
process per conversation (protocol of Codex 0.157; `install::MIN_VERSION`).
- CLI lookup: `SUPERGIT_CODEX`, `codex` on PATH, `~/.codex/packages/standalone`,
  `/Applications/Codex.app`; each must answer `--version` (a pnpm shim whose
  Node is gone does not). Regenerate the protocol types to read along:
  `codex app-server generate-ts --out /tmp/codex-ts`.
- Driver thread (process.rs) owns stdin and protocol state: `initialize` →
  `initialized` → `account/read` (signed out → "run `codex login`") →
  `thread/start {cwd, approvalPolicy, sandbox}` or `thread/resume {threadId,
  excludeTurns}` → `turn/start` per queued message (with `approvalPolicy` +
  `sandboxPolicy`: Codex applies policies per turn) → `turn/interrupt
  {threadId, turnId}`. Answers to our requests reach the fold as
  `{"response": method, "result"|"error"}`; notifications and server requests
  verbatim.
- Approvals are server requests: `item/commandExecution/requestApproval`
  (`availableDecisions`: accept / acceptForSession /
  acceptWithExecpolicyAmendment / decline / cancel), `item/fileChange/
  requestApproval` (diff from the started `fileChange` item),
  `item/permissions/requestApproval`. The fold stores the exact answers in
  `PermissionRequest.suggestions[0]` (`allow`/`session`/`deny`); MCP
  elicitations are declined, anything else gets a JSON-RPC error.
- Policies (`Policy`: Read only / Auto / Full access) are Codex's approval +
  sandbox presets — never mapped onto Claude's permission modes.
- History: `read_history` runs a short-lived app server (`thread/read` +
  `thread/turns/list`, no agent starts). Discovery reads rollouts
  (`~/.codex/sessions/**/rollout-*.jsonl`: first-line `session_meta`, tail
  turn boundaries) and thread names from `session_index.jsonl`.
- Tests: recorded events (`src/conversation/tests.rs`), a fake app server
  (`tests/fake_app_server.rs`); real Codex is opt-in:
  `cargo test -p sg_codex --test live -- --ignored --nocapture`.
- UI checks: `SUPERGIT_DEBUG_AGENT=codex` with `SUPERGIT_DEBUG_CHAT=…`, and
  `SUPERGIT_DEBUG_POLICY=read-only|auto|full-access` for `prompt:` runs.

Core crates are UI-agnostic and synchronous; the app runs them on GPUI's
background executor and applies results on the main thread. Keep it that way:
no GPUI types in `sg_*` crates.

**Run independent slow work side by side.** Subprocesses (`git` ~5–50 ms,
`gh` ~0.5–1 s) and file reads that don't need each other's results must not
run one after another:
- `sg_git::parallel::join(a, b)` for two calls (it behaves like calling them
  in sequence, panics included), `sg_git::parallel::map(items, f)` for one
  call per worktree, submodule, PR or login. `map` runs up to
  `parallel::workers()` (2–8) threads, never one per item: dozens of
  worktrees are normal. `sg_github` (no `sg_git` dependency) uses
  `std::thread::scope`, as in `list_prs`.
- In the app, start a view's requests together, not one after another's
  result. For example, a PR's diff, commits and viewed state start with the
  listed head while its details load, and refetch only what a newer head
  changed.
- Prefer one request to several: one GraphQL document with aliased fields,
  not N `gh` calls.
- Some work must stay sequential: checks that guard a mutation, commands
  that write the same repository (`index.lock`, `packed-refs.lock`,
  `config.lock`) or the same file, and cursor pagination.
- Never do I/O or parse large data (transcripts, patches, JSON) on the main
  thread. Use the background executor, or a thread of its own when the
  shared pool may be busy with slow Git and GitHub calls (as for chat
  history).
- Cache what is expensive to recompute and rarely changes, such as avatars
  and email → account lookups. Bound caches that grow with use: per
  worktree, per PR, per session.

## Commands

```sh
cargo build                         # debug app (deps built with opt-level 2)
cargo test --workspace              # unit + integration tests (temp git repos)
cargo lint && cargo fmt --all        # lint = CI's clippy (.cargo/config.toml)
cargo run -- --dump $SUPERGIT_TEST_REPO  # data layer as text, no window
cargo run -- $SUPERGIT_TEST_REPO         # the app
scripts/bundle-macos.sh --install   # SuperGIT.app → /Applications (ad-hoc signed)
scripts/install-linux.sh            # Linux: ~/.local/bin + .desktop launcher + icon
scripts/make-icon.sh                # regenerate assets/AppIcon.icns (Swift/AppKit)
```

CI (`.github/workflows/ci.yml`, on PRs and pushes to `main`, `macos-latest`):
`cargo fmt --all --check`, `cargo clippy --workspace --all-targets
--all-features -- -D warnings` (warnings fail CI), build + `cargo test
--workspace`, and `scripts/bundle-macos.sh` (release `.app`, no upload yet).
All builds use `--locked`: commit `Cargo.lock` changes. The `CI ok` job
aggregates the rest — the one check to require for merges.

### Rust workflow for agents

- **Iterate per crate.** `cargo check -p sg_git` / `cargo test -p sg_git` take
  seconds: core crates don't build GPUI. Check the app crate (`-p supergit`)
  only when you touched it.
- **Hooks** (`.claude/settings.json`, scripts in `.claude/hooks/`): every
  edited `.rs` file is rustfmt-ed — after Edit/Write the file written, after
  any Bash command (scripts, `sed`) every `.rs` file that differs from HEAD
  or is new; while Rust files differ from HEAD, finishing
  a turn or running `git commit` first needs `cargo clippy --workspace
  --all-targets -- -D warnings` to pass (exit 2 returns the errors to you).
  `SUPERGIT_NO_RUST_GATE=1` turns the gate off. CI additionally checks
  `--all-features` (`cargo lint`).
- **Lints** (`[workspace.lints]` + `clippy.toml`) fail CI. Fix the code; when a
  lint is truly wrong, use `#[expect(clippy::…, reason = "…")]` on the
  narrowest item — `#[allow]` itself is a lint error. No `unwrap()` outside
  tests (`?`, or `expect("why this holds")`), every `unsafe` block gets a
  `// SAFETY:` comment, no `println!` outside `--dump`, `foo.rs` + `foo/`
  instead of `foo/mod.rs`. `sg_claude` and `sg_github` forbid `unsafe`.
- **LSP**: the `LSP` tool talks to rust-analyzer — `findReferences` before
  changing a signature, `goToImplementation` for trait impls, `hover` for
  inferred types. Right after a session starts it answers empty while it
  indexes (about a minute). It does not push diagnostics after edits: the
  compiler (`cargo check`) is the source of truth.
- **GPUI API**: `gpui-pre 0.3.12` differs from the Zed GPUI you may remember.
  Read the real source (`goToDefinition`, or
  `~/.cargo/registry/src/*/gpui-pre-0.3.12`, `gpui-kit-0.9.1`) and existing
  views in `app.rs` before guessing a method.
- **Text is UTF-8** (Cyrillic branch names, commit subjects): never cut
  user-visible strings by byte index (`&s[..n]` panics mid-character); use
  `chars()` / `char_indices()`. Byte slicing is fine for SHAs and ASCII
  porcelain fields.

`main` is protected by a repository ruleset ("main: PR + green CI"): no
direct pushes, force-pushes or deletion, no bypass even for admins. Every
change lands through a pull request whose `CI ok` is green (no approval
needed). Work on a branch, push it, `gh pr create --fill`, then
`gh pr merge --auto --merge` — GitHub merges once CI passes and deletes the
remote branch.

Update checks (`updates.rs`, `app/update_check.rs`): the latest release of
`Aitem/SuperGIT` (`sg_github::latest_release`: `gh api`, else `curl` against
the public API) at startup and then at most every 12 hours, restarts
included (`updates.json` in the support dir); a newer tag than
`CARGO_PKG_VERSION` shows a status bar button whose menu installs it, saves
it to Downloads or opens its release page. SuperGIT ▸ Check for Updates…
checks at once. `updates/install.rs` downloads this platform's package with
`curl` (no quarantine flag, so the ad-hoc signed app opens), checks it
against the release's `SHA256SUMS`, unpacks it next to the target (the
`.app` bundle, or the Linux binary; never a build in a Cargo target dir),
runs its `--help`, swaps it in by rename, and "Restart to Update" starts it
once this process has exited. Screenshot runs stay offline unless
`SUPERGIT_DEBUG_UPDATE=<version>` (screenshot builds) pretends to run that
version and checks right away; `SUPERGIT_DEBUG_UPDATE_INSTALL=1` then
installs it (run a copy of the binary inside a stand-in `SuperGIT.app`).

App-launch specifics (`main.rs`, `shell_env.rs`, `recent.rs`): a Dock launch
has cwd `/` and launchd's minimal PATH, so `shell_env` imports the login
shell's environment (only when `TERM` is unset) before any thread starts, and
the last repository from `~/Library/Application Support/SuperGIT/recent.json`
is reopened. Folders arrive via `on_open_urls` (Dock drop, `open -a`).
To simulate a Dock launch: `cd / && env -i HOME=$HOME SHELL=$SHELL
PATH=/usr/bin:/bin target/debug/supergit --dump`.

### Seeing the UI without screen access

`screencapture` has no permission here. Build with the `screenshot` feature and
the app renders its own window through Metal to a PNG, then quits:

```sh
cargo build --features screenshot
SUPERGIT_TAB=claude SUPERGIT_SCREENSHOT=/tmp/ui.png SUPERGIT_SCREENSHOT_DELAY=6 \
  ./target/debug/supergit $SUPERGIT_TEST_REPO
```

Then read the PNG. Use this to verify every UI change. Screenshot runs open a
real (unfocused) window — keep them few, the user may be working.
`SUPERGIT_TAB=history|changes|claude|prs` picks the tab;
`SUPERGIT_CHANGES_TREE=1` (screenshot builds) shows the Changes list as a tree;
`SUPERGIT_DEBUG_REBASE=<branch>` (screenshot builds) opens the Rebase dialog;
`SUPERGIT_DEBUG_COMMIT=<sha prefix>` (screenshot builds) selects that History
commit two seconds after startup, like a click;
`SUPERGIT_DEBUG_DRAG=x1,y1,x2,y2` (screenshot builds) drags the mouse between
two window points (= PNG pixels) five seconds after startup, drawing a frame
per step, and prints what `cmd-c` would copy (text selection checks);
`SUPERGIT_DEBUG_PR=<number>` (screenshot builds, with `SUPERGIT_TAB=prs`)
selects that pull request, `SUPERGIT_DEBUG_PR_SCROLL=<px>` scrolls its details
(conversation, review threads), `SUPERGIT_DEBUG_PR_FILES=1` opens its Files
tab (diff) and `SUPERGIT_DEBUG_PR_MERGE=1` opens its merge dialog. A background
screenshot window only renders at startup and for the shot itself: anything
loaded from `render` (the PR tab's `ensure_pulls`) must be due by the first
frame, so these switches take effect before it. For rich PR data use a shallow
clone of a public repository (e.g. `git clone --depth 1
https://github.com/cli/cli /tmp/x`) — never merge there.

Set `SUPERGIT_STATE_DIR=/tmp/supergit-profile` for isolated UI checks. Settings,
recent repositories and workspace restoration then use that directory instead
of the user's normal application support directory. Agent transcripts remain
owned by the agent provider. Restored chats never start their agent; a
terminal conversation with history resumes its TUI once it is on screen,
unless Claude Code's registry of live sessions (`~/.claude/sessions/<pid>.json`)
shows the session open in another process.

`$SUPERGIT_TEST_REPO` is not set by the project: point it (in your shell or a
local, untracked `CLAUDE.local.md`) at a real repository with several
worktrees, Claude sessions and PRs for realistic data. Treat it as read-only:
never commit, push or create worktrees there without asking.

## Conventions and gotchas

- **Toolchain**: rustup only (`rust-toolchain.toml` pins 1.100). Homebrew Rust
  was removed because it shadowed rustup. `mise.toml` pins the same Rust and
  Zig 0.17.1 (libghostty-vt) for mise users; bump every pin together
  (`rust-toolchain.toml`, `mise.toml`, setup-zig in `ci.yml` and `release.yml`).
- **GPUI version**: `gpui-kit 0.9.1` pins `gpui-pre 0.3.12`. Import GPUI through
  `gpui_kit::*` plus `gpui_kit::prelude::*` (for `when`/`when_some`);
  components via `gpui_kit::component::…`. Do not enable `gpui-kit/test-support`
  (pulls `gpui-fast`, version conflict) — the `screenshot` feature enables
  `gpui-pre-platform/test-support` directly instead.
- Edition 2024: `impl IntoElement` return types capture `cx`'s lifetime; for
  rows built inside iterator closures return a concrete `Stateful<Div>`.
- **Buttons**: every button has a tooltip (shown on hover and while it is
  held) that says what it does: one short phrase that does not repeat the
  button's label ("Fetch" → "Download new commits from all remotes", not
  "Fetch: …"). When its action has a keyboard shortcut, the tooltip always
  names it too, written with `MOD` — e.g. `format!("New conversation in
  this worktree ({MOD}-n)")`. Tooltips show after `TOOLTIP_DELAY` (1 s,
  main.rs): components take it from `TooltipDefaults`; an element's own
  `.tooltip(…)` needs `.tooltip_show_delay(crate::TOOLTIP_DELAY)`.
  Button labels carry no trailing "…", even when they open a dialog;
  menu items keep it (the macOS convention).
- **Icons**: gpui-kit embeds only its default icon set; any other
  `IconName` (lucide) draws nothing until it is listed in
  `icons::ExtraIcons` (and in the `icons_used_by_the_app_are_embedded`
  test).
- **Initial prompt** (`initial_prompt.rs`, Settings ▸ Initial prompt): a
  template added to every agent SuperGIT starts — Claude's
  `--append-system-prompt` (chat and TUI, on every start: a resumed session
  does not keep it), Codex's `developerInstructions` on `thread/start` / `-c
  developer_instructions=` in its TUI (new threads only: a thread keeps
  them). Placeholders `{worktree}` `{branch}` `{repo}` `{repo_name}`,
  sections `{#linked}…{/linked}` / `{#main}…{/main}`; the default tells the
  agent it works in a worktree beside the main checkout. A conversation
  finds its place in the `Places` global, which `apply_worktrees` keeps.
  `settings.json` `initial_prompt`: absent = the default, `""` = none.
- **Git calls** go through `sg_git::git()` which sets `GIT_OPTIONAL_LOCKS=0`
  (never fight agents for `index.lock`) and disables prompts. Parse porcelain
  `-z` formats, never human output.
- **Sessions** are attributed to worktrees by the transcript's `cwd`, not by
  the project directory name (`repo/box` and `repo-box` encode the same).
  `claude --resume` must run in the session's `start_cwd`. Session ids are
  unique per provider only: compare `(provider, id)`. Persisted conversations
  carry `provider` (`claude`, `codex`; empty means Claude).
- **PRs**: `gh pr list --state all` with check rollups is slow (~10 s for 100);
  fetch open + recent merged in parallel instead.
- Never modify the user's `~/.claude` configuration (hooks etc.) without asking.
- Commits by agents carry a `Co-Authored-By: Claude …` trailer — the History
  view relies on it to mark them (`sg_git::CommitAgent`; a Codex/OpenAI
  co-author gets OpenAI's mark). Agents are drawn with their official marks
  everywhere (`icons::agent_icon`, SVGs in `assets/agents`), never glyphs.
- **Diffs**: gpui-kit's `Diff` with its `syntect` feature (syntax colors;
  `syntax::parse_patch` names languages its detector misses, e.g. Clojure).
  Unified/Split and "show with difftastic" live in `settings.json`
  (`diff_layout`, `difftastic`) and the toolbar above each diff. The
  difftastic view needs `difft` on PATH (`DFT_UNSTABLE=yes --display json`);
  JSON gives novel token ranges only, so `sg_git::difftastic` reads both
  sides itself and lines them up. Its text selection
  (`difft_view/selection.rs`) works like the built-in diff's: the list is
  one selection participant, the ends are (file, side, line, offset) keys,
  a selection stays on the side it starts on, and the copy is read from the
  data, not from the rows on screen. Screenshot checks: write
  `{"diff_layout":"split","difftastic":true}` to
  `$SUPERGIT_STATE_DIR/settings.json`.
- **Submodules**: every patch is `git diff` with `PATCH_OPTIONS`
  (`--submodule=short`, explicit `a/` `b/` prefixes — user config must not
  change the format), then `sg_git::inline_submodules` finds the
  `Subproject commit` entries and inserts each submodule's own diff after
  its entry (paths under the submodule, untracked files of its checkout
  included), plus a `SubmoduleChange` (commits between the recorded commits,
  why nothing is inlined: not checked out, commits missing). The app keeps
  those per list (`Details.submodules`, `commit_submodules`, PR patches) for
  the file-list rows, the cards under submodule headers and difftastic,
  which reads a submodule's files from its checkout. Tests:
  `cargo test -p sg_git --test submodules`. Cleanup: Git removes a worktree
  with initialized submodules only with `--force`, which deletes their
  repositories (`.git/worktrees/<name>/modules`); `cleanup_worktree` forces
  only after checking every checkout (nested too) has no uncommitted
  changes, no commits no remote has and no stash.
