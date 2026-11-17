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
cargo lint && cargo fmt --all        # lint = CI's clippy (.cargo/config.toml)
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
(conversation, review threads) and `SUPERGIT_DEBUG_PR_MERGE=1` opens its merge
dialog. For rich PR data use a shallow clone of a public repository (e.g.
`git clone --depth 1 https://github.com/cli/cli /tmp/x`) — never merge there.

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
