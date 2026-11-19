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
  sg_codex/    native Codex chat: CLI discovery + version check (install.rs),
               `codex app-server` driver (process.rs), fold (conversation.rs),
               item summaries/diffs (tools.rs), rollout discovery (sessions.rs)
  supergit/    the app (GPUI): main.rs, app.rs (view), model.rs (background
               loading), launch.rs (terminals/editor/browser), dump.rs,
               screenshot.rs (feature `screenshot`), difft_view.rs (the
               difftastic diff view), syntax.rs (syntect highlight names),
               submodules.rs (submodule rows and diff cards)
docs/ROADMAP.md
```

## Native Claude (sg_claude + supergit/src/chat.rs)

Same protocol as the Agent SDKs and the VS Code extension:
`claude -p --input-format stream-json --output-format stream-json --verbose
--include-partial-messages --permission-prompt-tool stdio [--resume=<id>]`.
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
  Malformed input stays an ordinary permission prompt.
- `Conversation::from_transcript` replays a session jsonl for history.
- Live test against the real CLI (costs cents): `cargo test -p sg_claude -- --ignored`.
- UI checks: `SUPERGIT_DEBUG_CHAT=resume` or `SUPERGIT_DEBUG_CHAT='prompt:…'`
  (screenshot builds only) opens/sends on startup, then screenshot it.

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
  edited `.rs` file is rustfmt-ed; while Rust files differ from HEAD, finishing
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
owned by the agent provider. Restored conversations never start automatically.

`$SUPERGIT_TEST_REPO` is not set by the project: point it (in your shell or a
local, untracked `CLAUDE.local.md`) at a real repository with several
worktrees, Claude sessions and PRs for realistic data. Treat it as read-only:
never commit, push or create worktrees there without asking.

## Conventions and gotchas

- **Toolchain**: rustup only (`rust-toolchain.toml` pins 1.100). Homebrew Rust
  was removed because it shadowed rustup.
- **GPUI version**: `gpui-kit 0.9.1` pins `gpui-pre 0.3.12`. Import GPUI through
  `gpui_kit::*` plus `gpui_kit::prelude::*` (for `when`/`when_some`);
  components via `gpui_kit::component::…`. Do not enable `gpui-kit/test-support`
  (pulls `gpui-fast`, version conflict) — the `screenshot` feature enables
  `gpui-pre-platform/test-support` directly instead.
- Edition 2024: `impl IntoElement` return types capture `cx`'s lifetime; for
  rows built inside iterator closures return a concrete `Stateful<Div>`.
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
  view relies on it for the `✦` marker.
- **Diffs**: gpui-kit's `Diff` with its `syntect` feature (syntax colors;
  `syntax::parse_patch` names languages its detector misses, e.g. Clojure).
  Unified/Split and "show with difftastic" live in `settings.json`
  (`diff_layout`, `difftastic`) and the toolbar above each diff. The
  difftastic view needs `difft` on PATH (`DFT_UNSTABLE=yes --display json`);
  JSON gives novel token ranges only, so `sg_git::difftastic` reads both
  sides itself and lines them up. Screenshot checks: write
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
  `cargo test -p sg_git --test submodules`.
