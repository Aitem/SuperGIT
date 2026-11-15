# SuperGIT

A native macOS Git GUI built around agent-driven development: many
**worktrees**, many **Claude Code sessions**, and the **commits and pull
requests** they produce — all in one window.

Written in Rust on [GPUI](https://github.com/zed-industries/zed/tree/main/crates/gpui)
(Zed's GPU UI framework) via [gpui-kit](https://gpui-kit.com).

## What it does (v0.1)

- **Worktrees sidebar** — every worktree of the repo with branch, changed files,
  ahead/behind, its PR (number, state, CI) and live Claude activity
  (`working` / `needs you` / `done`).
- **Changes** — full diff of the worktree against HEAD, untracked files included.
- **History** — commits of the worktree's branch with refs; commits co-authored
  by Claude are marked `✦`; click a commit to see its diff.
- **Claude** — sessions that ran in this worktree (found from Claude Code's own
  transcripts, nothing to configure): AI title, last prompt, branch, age;
  **Resume** any of them, or start a **New Claude** in the worktree.
- **Pull requests** — open PRs plus recently merged ones, with checks and
  review state; open in the browser or check one out into a new worktree.
- **Create / remove worktrees** — new branch → sibling directory
  `<repo>-<branch>`; removal asks for confirmation (twice when dirty).
- Refreshes every 3 s (PRs every minute); `cmd-r` refreshes now.

## Install

Requires macOS, [rustup](https://rustup.rs), `git`, and the
[`gh` CLI](https://cli.github.com) (logged in) for pull requests.

```sh
cargo install --path crates/supergit
```

## Use

```sh
supergit                  # the repository containing the current directory
supergit ~/Work/myrepo    # a specific repository (any of its worktrees works)
supergit --dump [PATH]    # print worktrees, sessions and PRs as text
```

| Variable            | Meaning                                                        |
| ------------------- | -------------------------------------------------------------- |
| `SUPERGIT_TERMINAL` | `ghostty`, `iterm` or `terminal` — where Claude sessions open (default: first installed) |
| `SUPERGIT_EDITOR`   | editor CLI for the **Editor** button, e.g. `zed`, `cursor` (default: VS Code) |
| `SUPERGIT_TAB`      | tab at startup: `changes`, `history`, `claude`, `prs`          |

## Develop

See [CLAUDE.md](CLAUDE.md) for the layout, commands and conventions, and
[docs/ROADMAP.md](docs/ROADMAP.md) for what comes next.
