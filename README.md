# SuperGIT

A native Git GUI for macOS and Linux built around agent-driven development: many
**worktrees**, many **Claude Code sessions**, and the **commits and pull
requests** they produce — all in one window.

Written in Rust on [GPUI](https://github.com/zed-industries/zed/tree/main/crates/gpui)
(Zed's GPU UI framework) via [gpui-kit](https://gpui-kit.com).

## What it does

- **Claude, built in** — a native chat panel on the right (`cmd-l`), like the
  VS Code extension: streaming answers in Markdown, tool calls with inline
  diffs, **permission prompts as cards** (Allow / Allow all edits this session
  / Deny), Stop, permission mode (Ask / Edits / Plan / Auto / Bypass; Auto by
  default). Every worktree has its own conversations (tabs) that keep running
  while you look at other worktrees; `cmd-n` starts a new one. Sessions are
  regular Claude Code sessions — open any past one from the **Claude** tab
  (history included) or continue it in a terminal.

- **Worktrees sidebar** — every worktree of the repo with branch, changed files,
  ahead/behind, its PR (number, state, CI) and live Claude activity
  (`working` / `needs you` / `done`).
- **Changes** — full diff of the worktree against HEAD, untracked files included.
- **History** — commits of the worktree's branch with refs; commits co-authored
  by Claude are marked `✦`; click a commit to see its diff.
- **Claude tab** — every session that ran in this worktree (found from Claude
  Code's own transcripts, nothing to configure): AI title, last prompt,
  branch, age; **Open** it in the panel or resume it in a **Terminal**.
- **Pull requests** — the selected worktree's PRs (its branch or the branch it
  pushes to, plus PRs linked by hand) or all of them, searchable (number,
  title, branch, `@author`; older PRs via GitHub search). A PR shows its
  conversation — description, comments, reviews, inline review threads (jump
  to the line in the diff), every check with links to its logs — and a merge
  box: merge / squash / rebase after a confirmation (pinned to the shown head
  commit) or enable auto-merge. **Files changed** shows its diff like History
  does — file list (filter, tree), Unified/Split, difftastic when the PR's
  commits are local; right-click asks the worktree's agent about a file or
  hunk. Diffs GitHub refuses (too large) are computed from local commits.
  Open in the browser or check one out into a new worktree.
- **Create / remove worktrees** — new branch → sibling directory
  `<repo>-<branch>`; removal asks for confirmation (twice when dirty).
- Refreshes every 3 s (PRs every minute); `cmd-r` refreshes now.

## Install

Requires macOS or Linux, [rustup](https://rustup.rs), `git`, and the
[`gh` CLI](https://cli.github.com) (logged in) for pull requests.

As a Mac app in `/Applications` (Dock, Spotlight, Launchpad):

```sh
scripts/bundle-macos.sh --install   # rebuild + reinstall after every pull/change
```

It is a local, ad-hoc signed build — no Apple developer account needed.
The script quits a running SuperGIT, moves the new bundle into place (the
staging folder is `.noindex`, so Spotlight never shows a second copy).
Launched from the Dock it reopens the last repository; `cmd-o` opens another,
**Recent** (`cmd-shift-o`) lists the previous ones. You can also drop a repo
folder on the Dock icon or run `open -a SuperGIT ~/Work/myrepo`. The app takes
`PATH` from your login shell, so `gh`/`claude` resolve as in a terminal.

On Linux (needs the usual GPUI libraries: Wayland or X11, `libxkbcommon`,
Vulkan drivers, `fontconfig`), for the current user:

```sh
scripts/install-linux.sh            # ~/.local/bin/supergit + launcher + icon
scripts/install-linux.sh --uninstall
```

Shortcuts use `ctrl` where macOS uses `cmd`, menus sit in the window's title
bar, notifications go to the desktop's notification server (click one to open
the conversation), and settings live in `~/.config/supergit`. Running
`supergit <dir>` (or "Open with SuperGIT" in a file manager) while SuperGIT is
open hands the folder to the running window, like `open -a` on macOS.

As a terminal command:

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
| `SUPERGIT_TERMINAL` | where sessions open — macOS: `ghostty`, `iterm`, `terminal`; Linux: `ghostty`, `kitty`, `wezterm`, `alacritty`, `konsole`, `gnome-terminal`, `ptyxis`, `xfce4-terminal`, `foot`, `xterm` (default: first installed; on Linux `$TERMINAL`, then the desktop's own) |
| `SUPERGIT_EDITOR`   | editor CLI for the **Editor** button, e.g. `zed`, `cursor` (default: VS Code; on Linux the first of `code`, `codium`, `zed`, `zeditor`, `cursor`) |
| `SUPERGIT_NEW_INSTANCE` | Linux: run a separate instance instead of handing the folder to the open one |
| `SUPERGIT_TAB`      | tab at startup: `changes`, `history`, `claude`, `prs`          |
| `SUPERGIT_APPEARANCE` | `system`, `light`, `dark` for this run (the saved choice is in View ▸ Appearance) |

## Develop

See [CLAUDE.md](CLAUDE.md) for the layout, commands and conventions, and
[docs/ROADMAP.md](docs/ROADMAP.md) for what comes next.

## License

[MIT](LICENSE)
