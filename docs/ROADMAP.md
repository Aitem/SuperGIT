# Roadmap

v0.1 (done): worktree sidebar with status, PR and Claude activity; Changes,
History, Claude sessions and Pull requests tabs; create/remove worktrees;
new/resume Claude in an external terminal; offscreen screenshots.

Ordered by how much each removes from the daily "terminal + VS Code +
GitKraken" juggling.

## 1. Claude panel, round two

Done (v0.2): native chat via stream-json, permission cards, inline diffs,
tabs per worktree, resume with history, modes, stop.

Next: `@file` mentions and the current selection/diff as context, images and
paste, slash commands from `initialize` (`commands`), model and effort picker,
TodoWrite rendered as a checklist, subagent (Task) calls folded into a tree,
plan-mode "accept plan" card, per-hunk accept/revert of edits, macOS
notification when a conversation needs approval or finishes, queued messages
while Claude works, copy buttons.

## 1a. Codex alongside Claude

Done: `sg_agents` holds the provider-neutral chat model and
`AgentProcess`; `sg_codex` drives `codex app-server` (version + login check,
thread start/resume, queued turns, approvals, interrupt), folds its events,
reads history via `thread/turns/list` and discovers rollouts. The panel picks
the agent per new conversation (switchable until the first message), labels
tabs, rails, sidebar and project activity by provider, and shows Codex's
policies (Read only / Auto / Full access) instead of Claude's modes. Tested
with recorded events, a fake app server and opt-in live tests.

The chat also shows live plan updates and context usage, answers Codex
questions, and lets you pick the model, reasoning effort, and Default or
Plan mode.

Next: stream command output (`item/commandExecution/outputDelta`).

Original plan:

Make the native coding-agent panel support both Claude Code and Codex, with
an agent picker for each new conversation and independent sessions per
worktree. Keep existing Claude conversations working throughout the change.

1. Extract a UI-independent agent interface for session start/resume, sending
   turns, streaming text/tool events, approval requests, interruption and exit.
   Keep provider-specific transports and transcript formats in their adapters;
   identify sessions by `(provider, session_id)` to avoid collisions.
2. Add an `sg_codex` adapter using the installed Codex CLI's structured
   `app-server` protocol over stdio. Verify the supported CLI/protocol version
   and handshake before starting a conversation; report missing or incompatible
   installations in the panel. Use the user's existing Codex login.
3. Add the Claude/Codex picker, provider labels on tabs and sidebar activity,
   and Codex session discovery/history/resume. Retain the original working
   directory for resume and preserve existing Claude history.
4. Render each provider's approval and sandbox capabilities explicitly: do
   not assume that Claude permission modes map to Codex policies. Support
   stop, process failure and reconnect without losing the visible transcript.
5. Test both adapters with recorded protocol events and fake processes:
   approvals, denied actions, interruption, crashes, resume and simultaneous
   sessions in different worktrees. Keep real-agent tests opt-in.

## 1b. Embedded terminal

Done (first cut): `Chat | Terminal` in the agent panel opens conversations
as the agent's own TUI (`claude`, `codex`) in an embedded terminal:
libghostty-vt 1.0 + GPUI (`sg_terminal`, `supergit/src/terminal.rs`), app
colors for the terminal and a generated Claude theme, exact status from
Claude's OSC 7501 reports. See docs/EMBEDDED_TERMINAL.md.

Next: mouse selection + copy, IME (dead keys, CJK), clickable links and
paths, switching an idle conversation between chat and terminal, plain
shell tabs.

## 2. Exact agent status + notifications

Today status is a heuristic over transcript tails and mtimes. Add an opt-in
Claude Code hooks integration (`UserPromptSubmit`, `Notification`, `Stop`,
`SessionStart/End`) that posts JSON to a SuperGIT unix socket → precise
`working / needs permission / done`, macOS notifications and a Dock badge.
Installing hooks touches `~/.claude/settings.json`: always ask first.

## 3. Commit graph

Done: lane graph for branch history, commit search, and loading older commits
on demand.

Next: an all-branches view, filters by branch or author, and linking commits
to agent sessions.

Git backend: evaluate a hybrid before a full migration. Benchmark `gix` and
`git2`/libgit2 for history, refs and status behind the existing `sg_git` API;
keep worktree mutations, fetch/push and signing/hook-sensitive operations on
the Git CLI initially. Compare results with the CLI for staged + unstaged
changes, renames, conflicts, untracked files, symlinks, detached/unborn HEAD,
linked worktrees and concurrent external Git operations. Measure both latency
and idle CPU on large repos and many worktrees; first avoid recomputing
unchanged history/diffs on every poll and evaluate filesystem notifications.

## 4. Pull request details

Done: PR tab scoped to the worktree (by branch/upstream, manual link/unlink)
or all PRs, search (local + GitHub search), details with description,
comments, reviews, inline review threads, checks, the diff (Files changed:
Unified/Split, difftastic, local fallback for large diffs, agent context),
and merge (method choice, confirmation, auto-merge).

You can also read failed job logs and send them to the worktree's agent,
open a PR from a worktree, and attach a review thread to chat. In the diff,
you can post line comments, reply to and resolve threads, and mark files
as viewed. A PR from a fork can be checked out in its own worktree.

## 5. Commit & sync

Done: stage and unstage files and hunks, mark conflicts as resolved, commit
staged changes, save and apply stashes, and cherry-pick from History. Fetch,
pull by fast-forward, push, or rebase onto a local branch.

Next: optional AI-written commit messages.

## 6. Worktree lifecycle

Done: open a worktree from a GitHub issue, PR, or commit. Clean up merged
worktrees and local branches.

Next: setup hooks after creation (copy `.env`, install dependencies).

## 7. Commit ↔ session linkage

Record which Claude session produced which commit (hook on `git commit`, or
match transcript tool calls to commit times); open the session from a commit
and the commits from a session.

## 8. App polish

Done in v0.1.1: open-repository dialog, recent repos, `.app` bundle with icon,
login-shell environment for Dock launches.

Next: several repositories at once (tabs or windows), settings UI (terminal,
editor, theme), FSEvents watcher instead of polling, command palette, `~` in
displayed paths.
