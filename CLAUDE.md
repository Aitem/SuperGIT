# SuperGIT — notes for agents

Native macOS Git GUI for agent-driven development (worktrees + Claude Code
sessions + commits + PRs). Rust 2024, GPUI through `gpui-kit`. Personal tool
for the author and colleagues: macOS only, GitHub only (via `gh`).

## Layout

```
crates/
  sg_git/      git core: worktrees, status, log, diffs (git CLI, porcelain -z)
  sg_github/   pull requests through the `gh` CLI
  sg_agents/   Claude Code sessions from ~/.claude/projects transcripts
  sg_claude/   native Claude chat: `claude` stream-json process + protocol
               (process.rs), event → chat items fold (conversation.rs),
               tool summaries/diffs (tools.rs)
  supergit/    the app (GPUI): main.rs, app.rs (view), model.rs (background
               loading), launch.rs (terminals/editor/browser), dump.rs,
               screenshot.rs (feature `screenshot`)
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
- `Conversation::from_transcript` replays a session jsonl for history.
- Live test against the real CLI (costs cents): `cargo test -p sg_claude -- --ignored`.
- UI checks: `SUPERGIT_DEBUG_CHAT=resume` or `SUPERGIT_DEBUG_CHAT='prompt:…'`
  (screenshot builds only) opens/sends on startup, then screenshot it.

Core crates are UI-agnostic and synchronous; the app runs them on GPUI's
background executor and applies results on the main thread. Keep it that way:
no GPUI types in `sg_*` crates.

## Commands

```sh
cargo build                         # debug app (deps built with opt-level 2)
cargo test --workspace              # unit + integration tests (temp git repos)
cargo clippy --workspace --all-targets && cargo fmt --all
cargo run -- --dump $SUPERGIT_TEST_REPO  # data layer as text, no window
cargo run -- $SUPERGIT_TEST_REPO         # the app
scripts/bundle-macos.sh --install   # SuperGIT.app → /Applications (ad-hoc signed)
scripts/make-icon.sh                # regenerate assets/AppIcon.icns (Swift/AppKit)
```

CI (`.github/workflows/ci.yml`, on PRs and pushes to `main`, `macos-latest`):
`cargo fmt --all --check`, `cargo clippy --workspace --all-targets
--all-features -- -D warnings` (warnings fail CI), build + `cargo test
--workspace`, and `scripts/bundle-macos.sh` (release `.app`, no upload yet).
All builds use `--locked`: commit `Cargo.lock` changes. The `CI ok` job
aggregates the rest — the one check to require for merges.

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
  `claude --resume` must run in the session's `start_cwd`.
- **PRs**: `gh pr list --state all` with check rollups is slow (~10 s for 100);
  fetch open + recent merged in parallel instead.
- Never modify the user's `~/.claude` configuration (hooks etc.) without asking.
- Commits by agents carry a `Co-Authored-By: Claude …` trailer — the History
  view relies on it for the `✦` marker.
