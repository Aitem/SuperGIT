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
  supergit/    the app (GPUI): main.rs, app.rs (view), model.rs (background
               loading), launch.rs (terminals/editor/browser), dump.rs,
               screenshot.rs (feature `screenshot`)
docs/ROADMAP.md
```

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

Then read the PNG. Use this to verify every UI change. `$SUPERGIT_TEST_REPO` is a
real repository with several worktrees, many sessions and PRs — good test data
(read-only: never commit, push or create worktrees there without asking).

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
