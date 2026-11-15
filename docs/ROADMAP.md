# Roadmap

v0.1 (done): worktree sidebar with status, PR and Claude activity; Changes,
History, Claude sessions and Pull requests tabs; create/remove worktrees;
new/resume Claude in an external terminal; offscreen screenshots.

Ordered by how much each removes from the daily "terminal + VS Code +
GitKraken" juggling.

## 1. Embedded terminal

Terminal tabs per worktree inside SuperGIT, running `claude` (and shells).
`alacritty_terminal` for PTY + VT emulation, rendered with GPUI (the approach
Zed's terminal uses). Sessions keep running when switching worktrees. Replaces
the external-terminal launch in `launch.rs`.

## 2. Exact agent status + notifications

Today status is a heuristic over transcript tails and mtimes. Add an opt-in
Claude Code hooks integration (`UserPromptSubmit`, `Notification`, `Stop`,
`SessionStart/End`) that posts JSON to a SuperGIT unix socket → precise
`working / needs permission / done`, macOS notifications and a Dock badge.
Installing hooks touches `~/.claude/settings.json`: always ask first.

## 3. Commit graph

Lane graph across all branches and worktrees (GitKraken-style), gix-based
traversal for big repos, filters (branch, author, "by Claude"), search.

## 4. Pull request details

Checks with failing job logs, review comments, diff of the PR. Actions:
create PR from a worktree (`gh pr create --fill`), "send review comments /
failing check to the agent in this worktree", merge.

## 5. Commit & sync

Stage/unstage files and hunks, commit (optionally AI-written message), fetch,
pull, push buttons; ahead/behind refresh after fetch.

## 6. Worktree lifecycle

Merged PR → offer cleanup (remove worktree + branch); bulk cleanup; setup hook
after creation (copy `.env`, install deps), worktree from an issue/PR.

## 7. Commit ↔ session linkage

Record which Claude session produced which commit (hook on `git commit`, or
match transcript tool calls to commit times); open the session from a commit
and the commits from a session.

## 8. App polish

Open-repository dialog and recent repos, multiple repositories, `.app` bundle
with icon, settings UI (terminal, editor, theme), FSEvents watcher instead of
polling, command palette.
