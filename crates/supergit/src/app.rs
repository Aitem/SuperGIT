//! The main window: worktrees on the left, the selected worktree's changes,
//! history, Claude sessions and pull requests on the right.

use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::time::{Duration, SystemTime};

use gpui_kit::component::button::{Button, ButtonVariants as _};
use gpui_kit::component::diff::{Diff, DiffEvent, DiffFile, DiffLinePosition, DiffSide, DiffState};
use gpui_kit::component::input::{Input, InputEvent, InputState};
use gpui_kit::component::resizable::{ResizableState, h_resizable, resizable_panel};
use gpui_kit::component::tab::{Tab, TabBar};
use gpui_kit::component::{ActiveTheme as _, Sizable as _, TitleBar, h_flex, v_flex};
use gpui_kit::prelude::*;
use gpui_kit::*;
use sg_agents::{AgentState, Session};
use sg_git::{Commit, FileState, Repo, relative_time};
use sg_github::{PrState, PullRequest};

use crate::chat::ChatSession;
use crate::launch::{self, TerminalApp};
use crate::model::{self, Details, LoadState, SharedSessionIndex, WorktreeInfo};
use crate::settings::Appearance;
use crate::workspace::{self, RepositoryView, ReviewPosition, WindowRect};

const POLL_INTERVAL: Duration = Duration::from_secs(3);
/// PRs come from the network: refresh every Nth poll.
const PR_POLL_EVERY: u64 = 20;

actions!(
    supergit,
    [
        Refresh,
        OpenRepository,
        CloseRepository,
        ToggleClaude,
        NewClaudeChat
    ]
);

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum MainTab {
    Changes,
    History,
    Sessions,
    PullRequests,
}

impl MainTab {
    fn name(self) -> &'static str {
        match self {
            Self::Changes => "changes",
            Self::History => "history",
            Self::Sessions => "claude",
            Self::PullRequests => "prs",
        }
    }

    fn from_name(name: &str) -> Self {
        match name {
            "history" => Self::History,
            "claude" | "sessions" => Self::Sessions,
            "prs" | "pr" => Self::PullRequests,
            _ => Self::Changes,
        }
    }
}

#[derive(Clone, Copy)]
enum LayoutPanel {
    Sidebar,
    Chat,
    Changes,
    History,
}

struct ReviewView {
    tab: MainTab,
    details: Details,
    selected_commit: Option<String>,
    commit_patch: LoadState<String>,
    changes_diff: Entity<DiffState>,
    commit_diff: Entity<DiffState>,
}

const TABS: [MainTab; 4] = [
    MainTab::Changes,
    MainTab::History,
    MainTab::Sessions,
    MainTab::PullRequests,
];

pub struct SuperGitApp {
    /// `None` shows the welcome screen (e.g. first launch from the Dock).
    repo: Option<Repo>,
    recent: Vec<PathBuf>,
    launch_dir: PathBuf,
    terminal: TerminalApp,
    index: SharedSessionIndex,

    worktrees: Vec<WorktreeInfo>,
    load_error: Option<String>,
    prs: Vec<PullRequest>,
    prs_error: Option<String>,
    prs_loaded: bool,

    selected: Option<PathBuf>,
    tab: MainTab,
    details: Details,
    changes_diff: Entity<DiffState>,
    sidebar_layout: Entity<ResizableState>,
    chat_layout: Entity<ResizableState>,
    changes_layout: Entity<ResizableState>,
    history_layout: Entity<ResizableState>,
    selected_commit: Option<String>,
    commit_diff: Entity<DiffState>,
    commit_patch: LoadState<String>,
    details_generation: u64,
    commit_generation: u64,

    branch_input: Entity<InputState>,
    confirm_remove: Option<PathBuf>,

    /// Native Claude conversations per worktree (tabs in the right panel).
    /// They keep running while other worktrees are selected.
    chats: HashMap<PathBuf, Vec<Entity<ChatSession>>>,
    active_chat: HashMap<PathBuf, usize>,
    chat_open: bool,
    message: Option<(String, bool)>,

    persisted: workspace::Store,
    review_views: HashMap<PathBuf, ReviewView>,
    persist_task: Option<Task<()>>,
    window_rect: Option<WindowRect>,
    poll_task: Option<Task<()>>,
    details_task: Option<Task<()>>,
    commit_task: Option<Task<()>>,
    action_task: Option<Task<()>>,
    _subscriptions: Vec<Subscription>,
}

impl SuperGitApp {
    pub fn new(
        repo: Option<Repo>,
        launch_dir: PathBuf,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> Self {
        let branch_input =
            cx.new(|cx| InputState::new(window, cx).placeholder("new branch → worktree"));
        let subscription = cx.subscribe_in(
            &branch_input,
            window,
            |this, _, event: &InputEvent, window, cx| {
                if matches!(event, InputEvent::PressEnter { .. }) {
                    this.create_worktree(window, cx);
                }
            },
        );
        // In "System" mode the theme follows macOS light/dark live.
        let appearance_subscription = cx.observe_window_appearance(window, |_, window, cx| {
            crate::appearance::system_appearance_changed(window, cx);
        });
        let changes_diff = cx.new(|cx| DiffState::new(Vec::<DiffFile>::new(), cx));
        let commit_diff = cx.new(|cx| DiffState::new(Vec::<DiffFile>::new(), cx));

        let persisted = workspace::load();
        let chat_open = repo
            .as_ref()
            .and_then(|repo| persisted.repositories.get(&repo.root))
            .map(|view| view.chat_open)
            .unwrap_or(true);
        let quit_subscription = cx.on_app_quit(|this, cx| {
            this.persist_workspace(cx);
            async {}
        });
        let release_subscription = cx.on_release(|this, cx| this.persist_workspace(cx));
        let bounds_subscription = cx.observe_window_bounds(window, |this, window, cx| {
            let bounds = window.window_bounds().get_bounds();
            this.window_rect = Some(WindowRect {
                x: bounds.origin.x.as_f32(),
                y: bounds.origin.y.as_f32(),
                width: bounds.size.width.as_f32(),
                height: bounds.size.height.as_f32(),
            });
            this.queue_persist(cx);
        });
        let mut this = Self {
            repo,
            recent: crate::recent::load(),
            launch_dir,
            terminal: TerminalApp::detect(),
            index: Default::default(),
            worktrees: Vec::new(),
            load_error: None,
            prs: Vec::new(),
            prs_error: None,
            prs_loaded: false,
            selected: None,
            tab: initial_tab(),
            details: Details::default(),
            changes_diff,
            sidebar_layout: cx.new(|_| ResizableState::default()),
            chat_layout: cx.new(|_| ResizableState::default()),
            changes_layout: cx.new(|_| ResizableState::default()),
            history_layout: cx.new(|_| ResizableState::default()),
            selected_commit: None,
            commit_diff,
            commit_patch: LoadState::Loading,
            details_generation: 0,
            commit_generation: 0,
            branch_input,
            confirm_remove: None,
            chats: HashMap::new(),
            active_chat: HashMap::new(),
            chat_open,
            message: None,
            persisted,
            review_views: HashMap::new(),
            persist_task: None,
            window_rect: None,
            poll_task: None,
            details_task: None,
            commit_task: None,
            action_task: None,
            _subscriptions: vec![
                subscription,
                appearance_subscription,
                quit_subscription,
                release_subscription,
                bounds_subscription,
            ],
        };
        this.start_polling(cx);
        this
    }

    fn pane_widths(&self, window: &Window, cx: &App) -> (f32, f32, f32) {
        let total = window.viewport_size().width.as_f32();
        let show_chat = self.chat_open && self.selected.is_some();
        let sidebar_max = (total - if show_chat { 680. } else { 360. }).clamp(220., 520.);
        let saved = self.repository_view();
        let sidebar = self
            .sidebar_layout
            .read(cx)
            .sizes()
            .first()
            .map(|v| v.as_f32())
            .unwrap_or(saved.sidebar_width)
            .clamp(220., sidebar_max);
        let chat_max = (total - sidebar - 360.).clamp(320., 960.);
        let chat = if show_chat {
            self.chat_layout
                .read(cx)
                .sizes()
                .get(1)
                .map(|v| v.as_f32())
                .unwrap_or(saved.chat_width)
                .clamp(320., chat_max)
        } else {
            0.
        };
        (sidebar_max, chat_max, (total - sidebar - chat).max(360.))
    }

    fn repository_view(&self) -> &RepositoryView {
        static EMPTY: std::sync::OnceLock<RepositoryView> = std::sync::OnceLock::new();
        self.repo
            .as_ref()
            .and_then(|repo| self.persisted.repositories.get(&repo.root))
            .unwrap_or_else(|| EMPTY.get_or_init(RepositoryView::default))
    }

    fn queue_persist(&mut self, cx: &mut Context<Self>) {
        self.persist_task = Some(cx.spawn(async move |this, cx| {
            cx.background_executor()
                .timer(Duration::from_millis(500))
                .await;
            let _ = this.update(cx, |this, cx| this.persist_workspace(cx));
        }));
    }

    fn capture_saved_review(&mut self, cx: &App) {
        let (Some(repo), Some(path)) = (&self.repo, &self.selected) else {
            return;
        };
        let position = |state: &Entity<DiffState>| {
            state.read(cx).selected_lines().map(|range| ReviewPosition {
                path: range.path().to_string(),
                line: range.start(),
                original: range.side() == DiffSide::Original,
            })
        };
        let changes = position(&self.changes_diff);
        let commit = position(&self.commit_diff);
        let view = self
            .persisted
            .repositories
            .entry(repo.root.clone())
            .or_default()
            .worktrees
            .entry(path.clone())
            .or_default();
        view.tab = self.tab.name().into();
        view.commit = self.selected_commit.clone();
        if changes.is_some() {
            view.changes_position = changes;
        }
        if commit.is_some() {
            view.commit_position = commit;
        }
    }

    fn persist_workspace(&mut self, cx: &App) {
        self.capture_saved_review(cx);
        let Some(root) = self.repo.as_ref().map(|repo| repo.root.clone()) else {
            if let Some(window) = self.window_rect {
                workspace::save_window(window);
            }
            return;
        };
        let view = self.persisted.repositories.entry(root.clone()).or_default();
        if !self.worktrees.is_empty() {
            view.selected = self.selected.clone();
            view.chat_open = self.chat_open;
        }
        for worktree in &self.worktrees {
            let path = worktree.path();
            if let Some(chats) = self.chats.get(path) {
                let mut snapshots: Vec<_> = chats
                    .iter()
                    .map(|chat| chat.read(cx).snapshot(cx))
                    .collect();
                if let Some(saved) = view.conversations.get(path) {
                    snapshots.extend(
                        saved
                            .iter()
                            .filter(|conversation| {
                                !conversation.provider.is_empty()
                                    && conversation.provider != "claude"
                            })
                            .cloned(),
                    );
                }
                view.conversations.insert(path.to_path_buf(), snapshots);
                view.active_conversations.insert(
                    path.to_path_buf(),
                    self.active_chat.get(path).copied().unwrap_or(0),
                );
            }
        }
        workspace::save_repository(&root, view, self.window_rect);
    }

    fn cache_review(&mut self, cx: &App) {
        self.capture_saved_review(cx);
        if let Some(path) = &self.selected {
            self.review_views.insert(
                path.clone(),
                ReviewView {
                    tab: self.tab,
                    details: self.details.clone(),
                    selected_commit: self.selected_commit.clone(),
                    commit_patch: self.commit_patch.clone(),
                    changes_diff: self.changes_diff.clone(),
                    commit_diff: self.commit_diff.clone(),
                },
            );
        }
    }

    fn restore_review(&mut self, cx: &mut Context<Self>) {
        let Some(path) = self.selected.clone() else {
            return;
        };
        if let Some(cached) = self.review_views.remove(&path) {
            self.tab = cached.tab;
            self.details = cached.details;
            self.selected_commit = cached.selected_commit;
            self.commit_patch = cached.commit_patch;
            self.changes_diff = cached.changes_diff;
            self.commit_diff = cached.commit_diff;
            return;
        }
        let saved = self
            .repository_view()
            .worktrees
            .get(&path)
            .cloned()
            .unwrap_or_default();
        self.tab = if std::env::var_os("SUPERGIT_TAB").is_some() {
            initial_tab()
        } else {
            MainTab::from_name(&saved.tab)
        };
        self.selected_commit = saved.commit;
        for (commit, entity) in [
            (false, self.changes_diff.clone()),
            (true, self.commit_diff.clone()),
        ] {
            let subscription = cx.subscribe(&entity, move |this, source, event: &DiffEvent, cx| {
                let active = if commit {
                    &this.commit_diff
                } else {
                    &this.changes_diff
                };
                if *active != source {
                    return;
                }
                if let DiffEvent::SelectionChanged(Some(range)) = event {
                    this.remember_position(
                        ReviewPosition {
                            path: range.path().to_string(),
                            line: range.start(),
                            original: range.side() == DiffSide::Original,
                        },
                        commit,
                        cx,
                    );
                }
            });
            self._subscriptions.push(subscription);
        }
    }

    fn remember_position(
        &mut self,
        position: ReviewPosition,
        commit: bool,
        cx: &mut Context<Self>,
    ) {
        let (Some(repo), Some(path)) = (&self.repo, &self.selected) else {
            return;
        };
        let view = self
            .persisted
            .repositories
            .entry(repo.root.clone())
            .or_default()
            .worktrees
            .entry(path.clone())
            .or_default();
        if commit {
            view.commit_position = Some(position);
        } else {
            view.changes_position = Some(position);
        }
        self.queue_persist(cx);
    }

    fn restore_position(&mut self, commit: bool, cx: &mut Context<Self>) {
        let saved = self
            .selected
            .as_ref()
            .and_then(|path| self.repository_view().worktrees.get(path));
        let position = saved.and_then(|view| {
            if commit {
                view.commit_position.clone()
            } else {
                view.changes_position.clone()
            }
        });
        if let Some(position) = position {
            let state = if commit {
                &self.commit_diff
            } else {
                &self.changes_diff
            };
            state.update(cx, |state, cx| {
                state.scroll_to_line(
                    DiffLinePosition::new(
                        position.path,
                        if position.original {
                            DiffSide::Original
                        } else {
                            DiffSide::Modified
                        },
                        position.line,
                    ),
                    cx,
                )
            });
        }
    }

    fn remember_layout(
        &mut self,
        panel: LayoutPanel,
        state: &Entity<ResizableState>,
        cx: &mut Context<Self>,
    ) {
        let ix = if matches!(panel, LayoutPanel::Chat) {
            1
        } else {
            0
        };
        let Some(width) = state.read(cx).sizes().get(ix).map(|width| width.as_f32()) else {
            return;
        };
        let Some(repo) = &self.repo else { return };
        let view = self
            .persisted
            .repositories
            .entry(repo.root.clone())
            .or_default();
        match panel {
            LayoutPanel::Sidebar => view.sidebar_width = width,
            LayoutPanel::Chat => view.chat_width = width,
            LayoutPanel::Changes => view.changes_width = width,
            LayoutPanel::History => view.history_width = width,
        }
        view.sanitize();
        self.queue_persist(cx);
    }

    // ---- data flow -------------------------------------------------------

    /// (Re)start the refresh loop. Restarting refreshes immediately, PRs included.
    fn start_polling(&mut self, cx: &mut Context<Self>) {
        if self.repo.is_none() {
            self.poll_task = None;
            return;
        }
        self.poll_task = Some(cx.spawn(async move |this, cx| {
            let mut tick: u64 = 0;
            loop {
                let Ok(Some((repo, index))) = this.read_with(cx, |this, _| {
                    this.repo.clone().map(|r| (r, this.index.clone()))
                }) else {
                    return;
                };
                let root = repo.root.clone();
                let result = cx
                    .background_executor()
                    .spawn(async move { model::load_worktrees(&repo, &index) })
                    .await;
                if this
                    .update(cx, |this, cx| this.apply_worktrees(result, cx))
                    .is_err()
                {
                    return;
                }
                if tick.is_multiple_of(PR_POLL_EVERY) {
                    let prs = cx
                        .background_executor()
                        .spawn(async move {
                            if sg_github::gh_available() {
                                sg_github::list_prs(&root, 100, 30).map_err(|e| e.to_string())
                            } else {
                                Err("gh CLI not found — install it for pull requests".into())
                            }
                        })
                        .await;
                    if this.update(cx, |this, cx| this.apply_prs(prs, cx)).is_err() {
                        return;
                    }
                }
                tick += 1;
                cx.background_executor().timer(POLL_INTERVAL).await;
            }
        }));
    }

    fn apply_worktrees(
        &mut self,
        result: anyhow::Result<Vec<WorktreeInfo>>,
        cx: &mut Context<Self>,
    ) {
        match result {
            Ok(list) => {
                self.load_error = None;
                self.worktrees = list;
            }
            Err(e) => {
                self.load_error = Some(model::git_error_message(e));
                cx.notify();
                return;
            }
        }
        let previous = self.selected.clone();
        let valid = self
            .selected
            .as_ref()
            .is_some_and(|p| self.worktrees.iter().any(|w| &w.wt.path == p));
        if !valid {
            self.cache_review(cx);
            let preferred = self.repo.as_ref().and_then(|repo| {
                (self.launch_dir == repo.root)
                    .then(|| {
                        self.persisted
                            .repositories
                            .get(&repo.root)
                            .and_then(|view| view.selected.clone())
                    })
                    .flatten()
            });
            self.selected = preferred
                .filter(|path| {
                    self.worktrees.iter().any(|info| {
                        info.path() == path && info.path().is_dir() && info.wt.prunable.is_none()
                    })
                })
                .or_else(|| {
                    self.worktrees
                        .iter()
                        .filter(|w| self.launch_dir.starts_with(&w.wt.path))
                        .max_by_key(|w| w.wt.path.as_os_str().len())
                        .or(self.worktrees.first())
                        .map(|w| w.wt.path.clone())
                });
        }
        if self.selected != previous {
            self.reset_details(cx);
            self.restore_review(cx);
            self.queue_persist(cx);
        }
        self.reload_details(cx);
        #[cfg(feature = "screenshot")]
        self.debug_chat_once(cx);
        cx.notify();
    }

    /// Screenshot builds only: `SUPERGIT_DEBUG_CHAT=resume` opens the latest
    /// session in the panel, `SUPERGIT_DEBUG_CHAT=prompt:<text>` sends a turn.
    #[cfg(feature = "screenshot")]
    fn debug_chat_once(&mut self, cx: &mut Context<Self>) {
        let Ok(spec) = std::env::var("SUPERGIT_DEBUG_CHAT") else {
            return;
        };
        if self.worktrees.is_empty() || std::env::var_os("SUPERGIT_DEBUG_CHAT_DONE").is_some() {
            return;
        }
        // SAFETY: screenshot/debug builds only; set once on the main thread.
        unsafe { std::env::set_var("SUPERGIT_DEBUG_CHAT_DONE", "1") };
        let Some(window) = cx.windows().first().copied() else {
            return;
        };
        let this = cx.entity();
        cx.defer(move |cx| {
            let _ = window.update(cx, |_, window, cx| {
                this.update(cx, |app, cx| {
                    if spec == "resume" {
                        if let Some(s) = app
                            .selected_info()
                            .and_then(|i| i.sessions.first().cloned())
                        {
                            app.open_session_chat(s, window, cx);
                        }
                    } else if let Some(prompt) = spec.strip_prefix("prompt:") {
                        app.new_chat(window, cx);
                        if let Some(chat) = app.active_chat_entity() {
                            chat.update(cx, |chat, cx| chat.send_text(prompt, cx));
                        }
                    }
                })
            });
        });
    }

    fn apply_prs(&mut self, result: Result<Vec<PullRequest>, String>, cx: &mut Context<Self>) {
        self.prs_loaded = true;
        match result {
            Ok(prs) => {
                self.prs = prs;
                self.prs_error = None;
            }
            Err(e) => self.prs_error = Some(e),
        }
        cx.notify();
    }

    fn selected_info(&self) -> Option<&WorktreeInfo> {
        let path = self.selected.as_ref()?;
        self.worktrees.iter().find(|w| &w.wt.path == path)
    }

    fn pr_for_branch(&self, branch: Option<&str>) -> Option<&PullRequest> {
        let branch = branch?;
        // `prs` lists open ones first, so an open PR wins over an old merged one.
        self.prs.iter().find(|pr| pr.head_branch == branch)
    }

    fn reload_details(&mut self, cx: &mut Context<Self>) {
        // Let a slow read finish instead of canceling it on every poll.
        if self.details_task.is_some() {
            return;
        }
        let Some(info) = self.selected_info() else {
            return;
        };
        let path = info.wt.path.clone();
        let status = info.status.clone();
        self.details_generation += 1;
        let generation = self.details_generation;
        self.details_task = Some(cx.spawn(async move |this, cx| {
            let details = cx
                .background_executor()
                .spawn(async move { model::load_details(&path, status) })
                .await;
            this.update(cx, |this, cx| {
                if generation == this.details_generation {
                    this.details_task = None;
                    this.apply_details(details, cx);
                }
            })
            .ok();
        }));
    }

    fn apply_details(&mut self, mut details: Details, cx: &mut Context<Self>) {
        if self.selected.as_ref() != Some(&details.path) {
            return;
        }
        if let LoadState::Ready(patch) = &details.patch
            && (self.details.patch.value() != Some(patch) || details.path != self.details.path)
        {
            match DiffFile::parse(patch) {
                Ok(files) => {
                    self.changes_diff
                        .update(cx, |state, cx| state.set_files(files, cx));
                    self.restore_position(false, cx);
                }
                Err(error) => {
                    details.patch =
                        LoadState::from_result(Err(format!("Cannot display diff: {error}")));
                }
            }
        }
        // A failed history refresh must not clear the current review selection.
        if let LoadState::Ready(commits) = &details.commits
            && let Some(sha) = &self.selected_commit
            && !commits.iter().any(|c| &c.sha == sha)
        {
            self.selected_commit = None;
            self.commit_task = None;
            self.commit_generation += 1;
            self.commit_patch = LoadState::Loading;
            self.commit_diff
                .update(cx, |state, cx| state.set_files(Vec::new(), cx));
        }
        self.details.update(details);
        if self.commit_task.is_none()
            && self.commit_patch.is_loading()
            && let Some(sha) = self.selected_commit.clone().or_else(|| {
                self.details
                    .commits
                    .value()
                    .and_then(|commits| commits.first())
                    .map(|c| c.sha.clone())
            })
        {
            self.select_commit(sha, cx);
        }
        cx.notify();
    }

    fn reset_details(&mut self, cx: &mut Context<Self>) {
        self.details_generation += 1;
        self.commit_generation += 1;
        self.details_task = None;
        self.commit_task = None;
        self.details = Details::default();
        self.selected_commit = None;
        self.commit_patch = LoadState::Loading;
        self.changes_diff = cx.new(|cx| DiffState::new(Vec::<DiffFile>::new(), cx));
        self.commit_diff = cx.new(|cx| DiffState::new(Vec::<DiffFile>::new(), cx));
    }

    fn select_worktree(&mut self, path: PathBuf, cx: &mut Context<Self>) {
        if self.selected.as_ref() == Some(&path) {
            return;
        }
        self.cache_review(cx);
        self.selected = Some(path);
        self.confirm_remove = None;
        self.reset_details(cx);
        self.restore_review(cx);
        self.reload_details(cx);
        self.queue_persist(cx);
        cx.notify();
    }

    fn select_commit(&mut self, sha: String, cx: &mut Context<Self>) {
        let Some(path) = self.selected.clone() else {
            return;
        };
        if self.selected_commit.as_ref() != Some(&sha) {
            self.capture_saved_review(cx);
            if let (Some(repo), Some(path)) = (&self.repo, &self.selected) {
                self.persisted
                    .repositories
                    .entry(repo.root.clone())
                    .or_default()
                    .worktrees
                    .entry(path.clone())
                    .or_default()
                    .commit_position = None;
            }
            self.commit_patch = LoadState::Loading;
            self.commit_diff
                .update(cx, |state, cx| state.set_files(Vec::new(), cx));
        }
        self.selected_commit = Some(sha.clone());
        self.commit_generation += 1;
        let generation = self.commit_generation;
        self.commit_task = Some(cx.spawn(async move |this, cx| {
            let request_path = path.clone();
            let request_sha = sha.clone();
            let patch = cx
                .background_executor()
                .spawn(async move {
                    sg_git::commit_patch(&request_path, &request_sha)
                        .map_err(model::git_error_message)
                })
                .await;
            this.update(cx, |this, cx| {
                if generation != this.commit_generation
                    || this.selected.as_ref() != Some(&path)
                    || this.selected_commit.as_ref() != Some(&sha)
                {
                    return;
                }
                let patch = patch.and_then(|patch| {
                    let files = DiffFile::parse(&patch)
                        .map_err(|e| format!("Cannot display commit diff: {e}"))?;
                    this.commit_diff
                        .update(cx, |state, cx| state.set_files(files, cx));
                    Ok(patch)
                });
                this.commit_patch.update(LoadState::from_result(patch));
                this.commit_task = None;
                this.restore_position(true, cx);
                this.queue_persist(cx);
                cx.notify();
            })
            .ok();
        }));
        cx.notify();
    }

    fn flash(&mut self, message: impl Into<String>, is_error: bool, cx: &mut Context<Self>) {
        self.message = Some((message.into(), is_error));
        cx.notify();
    }

    fn report(&mut self, result: anyhow::Result<()>, ok: &str, cx: &mut Context<Self>) {
        match result {
            Ok(()) => self.flash(ok, false, cx),
            Err(e) => self.flash(format!("{e:#}"), true, cx),
        }
    }

    /// Open the repository containing `path` (from the dialog, a recent entry,
    /// a folder dropped on the Dock icon or `open -a SuperGIT <dir>`).
    pub fn open_path(&mut self, path: PathBuf, cx: &mut Context<Self>) {
        let repo = match Repo::discover(&path) {
            Ok(repo) => repo,
            Err(e) => {
                self.flash(format!("{e:#}"), true, cx);
                return;
            }
        };
        self.persist_workspace(cx);
        self.cache_review(cx);
        crate::recent::remember(&repo.root);
        self.recent = crate::recent::load();
        let same = self.repo.as_ref().is_some_and(|r| r.root == repo.root);
        self.launch_dir = path;
        if !same {
            self.repo = Some(repo);
            self.reset_view(cx);
            self.sidebar_layout = cx.new(|_| ResizableState::default());
            self.chat_layout = cx.new(|_| ResizableState::default());
            self.changes_layout = cx.new(|_| ResizableState::default());
            self.history_layout = cx.new(|_| ResizableState::default());
            self.chat_open = self.repository_view().chat_open;
        } else {
            self.selected = None; // re-pick the worktree containing `path`
        }
        self.message = None;
        self.start_polling(cx);
        cx.notify();
    }

    fn reset_view(&mut self, cx: &mut Context<Self>) {
        self.worktrees.clear();
        self.load_error = None;
        self.prs.clear();
        self.prs_error = None;
        self.prs_loaded = false;
        self.selected = None;
        self.confirm_remove = None;
        self.reset_details(cx);
    }

    pub fn open_repository(&mut self, _: &OpenRepository, _: &mut Window, cx: &mut Context<Self>) {
        let paths = cx.prompt_for_paths(PathPromptOptions {
            files: false,
            directories: true,
            multiple: false,
            prompt: Some("Open Repository".into()),
        });
        cx.spawn(async move |this, cx| {
            if let Ok(Ok(Some(paths))) = paths.await
                && let Some(path) = paths.into_iter().next()
            {
                this.update(cx, |this, cx| this.open_path(path, cx)).ok();
            }
        })
        .detach();
    }

    /// Back to the welcome screen with the recent repositories.
    pub fn close_repository(
        &mut self,
        _: &CloseRepository,
        _: &mut Window,
        cx: &mut Context<Self>,
    ) {
        self.persist_workspace(cx);
        self.cache_review(cx);
        self.repo = None;
        self.poll_task = None;
        self.reset_view(cx);
        self.recent = crate::recent::load();
        cx.notify();
    }

    // ---- Claude panel ------------------------------------------------------

    fn add_chat(
        &mut self,
        path: PathBuf,
        chat: Entity<ChatSession>,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        // Re-render (sidebar dots, title counts) whenever a conversation changes.
        cx.observe(&chat, |this, _, cx| {
            this.queue_persist(cx);
            cx.notify();
        })
        .detach();
        let list = self.chats.entry(path.clone()).or_default();
        list.push(chat.clone());
        self.active_chat.insert(path, list.len() - 1);
        self.chat_open = true;
        self.queue_persist(cx);
        let input = chat.read(cx).input();
        input.update(cx, |state, cx| state.focus(window, cx));
        cx.notify();
    }

    /// A new conversation for the selected worktree.
    pub fn new_chat(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let Some(path) = self.selected.clone() else {
            return;
        };
        // Reuse an untouched empty conversation instead of piling up tabs.
        if let Some(ix) = self.chats.get(&path).and_then(|list| {
            list.iter().position(|c| {
                let c = c.read(cx);
                c.is_empty() && c.session_id().is_none()
            })
        }) {
            self.active_chat.insert(path.clone(), ix);
            self.chat_open = true;
            let input = self.chats[&path][ix].read(cx).input();
            input.update(cx, |state, cx| state.focus(window, cx));
            cx.notify();
            return;
        }
        let chat = cx.new(|cx| ChatSession::new(path.clone(), window, cx));
        self.add_chat(path, chat, window, cx);
    }

    pub fn new_chat_action(
        &mut self,
        _: &NewClaudeChat,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        self.new_chat(window, cx);
    }

    /// Open an existing Claude Code session in the panel (history + resume).
    fn open_session_chat(&mut self, session: Session, window: &mut Window, cx: &mut Context<Self>) {
        let Some(path) = self.selected.clone() else {
            return;
        };
        if let Some(ix) = self.chats.get(&path).and_then(|list| {
            list.iter()
                .position(|c| c.read(cx).session_id() == Some(session.id.as_str()))
        }) {
            self.active_chat.insert(path, ix);
            self.chat_open = true;
            cx.notify();
            return;
        }
        let chat = cx.new(|cx| ChatSession::resume(path.clone(), &session, window, cx));
        self.add_chat(path, chat, window, cx);
    }

    fn close_chat(&mut self, ix: usize, cx: &mut Context<Self>) {
        let Some(path) = self.selected.clone() else {
            return;
        };
        if let Some(list) = self.chats.get_mut(&path)
            && ix < list.len()
        {
            // Dropping the session kills its `claude` process.
            list.remove(ix);
            let active = self.active_chat.get(&path).copied().unwrap_or(0);
            let next = if active > ix { active - 1 } else { active };
            self.active_chat
                .insert(path, next.min(list.len().saturating_sub(1)));
        }
        self.queue_persist(cx);
        cx.notify();
    }

    pub fn toggle_claude(&mut self, _: &ToggleClaude, window: &mut Window, cx: &mut Context<Self>) {
        self.chat_open = !self.chat_open;
        self.queue_persist(cx);
        if self.chat_open
            && let Some(chat) = self.active_chat_entity()
        {
            let input = chat.read(cx).input();
            input.update(cx, |state, cx| state.focus(window, cx));
        }
        cx.notify();
    }

    fn active_chat_entity(&self) -> Option<Entity<ChatSession>> {
        let path = self.selected.as_ref()?;
        let list = self.chats.get(path)?;
        let ix = self.active_chat.get(path).copied().unwrap_or(0);
        list.get(ix).or(list.last()).cloned()
    }

    /// State of the native conversations in a worktree, if any is active.
    fn live_agent_state(&self, path: &Path, cx: &App) -> Option<AgentState> {
        self.chats
            .get(path)?
            .iter()
            .filter_map(|c| match c.read(cx).status() {
                sg_claude::Status::NeedsPermission => Some(AgentState::NeedsAttention),
                sg_claude::Status::Working | sg_claude::Status::Starting => {
                    Some(AgentState::Working)
                }
                _ => None,
            })
            .min()
    }

    /// Transcript-based and live state combined (most urgent wins).
    fn agent_state_for(&self, info: &WorktreeInfo, cx: &App) -> Option<AgentState> {
        [info.agent_state(), self.live_agent_state(info.path(), cx)]
            .into_iter()
            .flatten()
            .min()
    }

    pub fn refresh(&mut self, _: &Refresh, _: &mut Window, cx: &mut Context<Self>) {
        self.flash("Refreshing…", false, cx);
        self.start_polling(cx);
    }

    // ---- actions ---------------------------------------------------------

    /// Run a blocking git operation off the main thread, then refresh.
    fn run_git_action(
        &mut self,
        label: String,
        select_after: Option<PathBuf>,
        op: impl FnOnce() -> anyhow::Result<()> + Send + 'static,
        cx: &mut Context<Self>,
    ) {
        self.flash(format!("{label}…"), false, cx);
        self.action_task = Some(cx.spawn(async move |this, cx| {
            let result = cx.background_executor().spawn(async move { op() }).await;
            this.update(cx, |this, cx| {
                match result {
                    Ok(()) => {
                        this.flash(format!("{label} — done"), false, cx);
                        if let Some(path) = select_after {
                            this.selected = Some(path);
                        }
                    }
                    Err(e) => this.flash(format!("{label} failed: {e:#}"), true, cx),
                }
                this.start_polling(cx);
            })
            .ok();
        }));
    }

    fn create_worktree(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let branch = self.branch_input.read(cx).value().trim().to_string();
        if branch.is_empty() {
            self.flash("Type a branch name first", true, cx);
            return;
        }
        if branch.contains(char::is_whitespace) {
            self.flash("Branch names cannot contain spaces", true, cx);
            return;
        }
        let Some(repo) = self.repo.clone() else {
            return;
        };
        let path = model::default_worktree_path(&repo, &branch);
        let root = repo.root.clone();
        let target = path.clone();
        self.branch_input
            .update(cx, |state, cx| state.set_value("", window, cx));
        self.run_git_action(
            format!("Creating worktree {branch}"),
            Some(path),
            move || {
                if let Some(parent) = target.parent() {
                    std::fs::create_dir_all(parent)?;
                }
                // Existing branch: check it out; otherwise create it from HEAD.
                let exists = sg_git::git(
                    &root,
                    [
                        "rev-parse",
                        "--verify",
                        "--quiet",
                        &format!("refs/heads/{branch}"),
                    ],
                )
                .is_ok();
                if exists {
                    sg_git::add_worktree(&root, &target, None, Some(&branch))
                } else {
                    sg_git::add_worktree(&root, &target, Some(&branch), None)
                }
            },
            cx,
        );
    }

    fn checkout_pr(&mut self, pr: &PullRequest, cx: &mut Context<Self>) {
        if let Some(existing) = self
            .worktrees
            .iter()
            .find(|w| w.wt.branch.as_deref() == Some(pr.head_branch.as_str()))
        {
            let path = existing.wt.path.clone();
            self.select_worktree(path, cx);
            return;
        }
        let Some(repo) = self.repo.clone() else {
            return;
        };
        let branch = pr.head_branch.clone();
        let path = model::default_worktree_path(&repo, &branch);
        let root = repo.root.clone();
        let target = path.clone();
        self.run_git_action(
            format!("Checking out #{} into a worktree", pr.number),
            Some(path),
            move || {
                if let Some(parent) = target.parent() {
                    std::fs::create_dir_all(parent)?;
                }
                let local = sg_git::git(
                    &root,
                    [
                        "rev-parse",
                        "--verify",
                        "--quiet",
                        &format!("refs/heads/{branch}"),
                    ],
                )
                .is_ok();
                if !local {
                    // Updates origin/<branch>; `worktree add` then creates a tracking branch.
                    sg_git::git(&root, ["fetch", "origin", &branch])?;
                }
                sg_git::add_worktree(&root, &target, None, Some(&branch))
            },
            cx,
        );
    }

    fn remove_selected(&mut self, cx: &mut Context<Self>) {
        let Some(info) = self.selected_info() else {
            return;
        };
        if info.wt.is_main {
            self.flash("The main worktree cannot be removed", true, cx);
            return;
        }
        let path = info.wt.path.clone();
        if self.confirm_remove.as_ref() != Some(&path) {
            self.confirm_remove = Some(path);
            cx.notify();
            return;
        }
        let force = info.changed_files() > 0;
        let Some(root) = self.repo.as_ref().map(|r| r.root.clone()) else {
            return;
        };
        let name = info.wt.display_name();
        self.confirm_remove = None;
        self.selected = None;
        self.run_git_action(
            format!("Removing worktree {name}"),
            None,
            move || sg_git::remove_worktree(&root, &path, force),
            cx,
        );
    }

    // ---- rendering -------------------------------------------------------

    fn render_title_bar(&self, cx: &mut Context<Self>) -> impl IntoElement {
        let working = self
            .worktrees
            .iter()
            .filter(|w| self.agent_state_for(w, cx) == Some(AgentState::Working))
            .count();
        let waiting = self
            .worktrees
            .iter()
            .filter(|w| {
                matches!(
                    self.agent_state_for(w, cx),
                    Some(AgentState::NeedsAttention | AgentState::Finished)
                )
            })
            .count();
        let theme = cx.theme();
        TitleBar::new().child(
            h_flex()
                .w_full()
                .pr_2()
                .justify_between()
                .child(
                    h_flex()
                        .gap_2()
                        .child(div().font_weight(FontWeight::BOLD).child("SuperGIT"))
                        .when_some(self.repo.as_ref().map(Repo::name), |el, name| {
                            el.child(
                                div()
                                    .text_color(theme.muted_foreground)
                                    .child(format!("· {name}")),
                            )
                        }),
                )
                .child(
                    h_flex()
                        .gap_3()
                        .text_xs()
                        .when(working > 0, |el| {
                            el.child(
                                div()
                                    .text_color(theme.blue)
                                    .child(format!("● {working} working")),
                            )
                        })
                        .when(waiting > 0, |el| {
                            el.child(
                                div()
                                    .text_color(theme.warning)
                                    .child(format!("● {waiting} waiting for you")),
                            )
                        })
                        .child(
                            Button::new("open-repo")
                                .label("Open…")
                                .ghost()
                                .small()
                                .tooltip("Open another repository (cmd-o)")
                                .on_click(cx.listener(|this, _, window, cx| {
                                    this.open_repository(&OpenRepository, window, cx)
                                })),
                        )
                        .when(self.repo.is_some(), |el| {
                            el.child(
                                Button::new("close-repo")
                                    .label("Recent")
                                    .ghost()
                                    .small()
                                    .tooltip("Switch to a recent repository (cmd-shift-o)")
                                    .on_click(cx.listener(|this, _, window, cx| {
                                        this.close_repository(&CloseRepository, window, cx)
                                    })),
                            )
                        })
                        .child({
                            let current = crate::appearance::current(cx);
                            Button::new("appearance")
                                .label(format!(
                                    "{} {}",
                                    match current {
                                        Appearance::System => "◐",
                                        Appearance::Light => "☀",
                                        Appearance::Dark => "☾",
                                    },
                                    current.label()
                                ))
                                .ghost()
                                .small()
                                .tooltip(
                                    "Appearance: System → Light → Dark (also View ▸ Appearance)",
                                )
                                .on_click(cx.listener(move |_, _, _, cx| {
                                    crate::appearance::set(current.next(), cx);
                                    cx.notify();
                                }))
                        })
                        .child(
                            Button::new("refresh")
                                .label("Refresh")
                                .ghost()
                                .small()
                                .tooltip("Refresh now (cmd-r)")
                                .on_click(cx.listener(|this, _, window, cx| {
                                    this.refresh(&Refresh, window, cx)
                                })),
                        ),
                ),
        )
    }

    fn render_sidebar(&self, cx: &mut Context<Self>) -> impl IntoElement {
        let theme = cx.theme().clone();
        let rows = self
            .worktrees
            .iter()
            .enumerate()
            .map(|(ix, info)| self.render_worktree_row(ix, info, &theme, cx))
            .collect::<Vec<_>>();

        v_flex()
            .w_full()
            .min_w_0()
            .h_full()
            .flex_none()
            .bg(theme.sidebar)
            .border_r_1()
            .border_color(theme.sidebar_border)
            .child(
                h_flex()
                    .px_3()
                    .py_2()
                    .justify_between()
                    .text_xs()
                    .text_color(theme.muted_foreground)
                    .child(format!("WORKTREES · {}", self.worktrees.len()))
                    .when_some(self.load_error.clone(), |el, e| {
                        el.child(div().text_color(theme.danger).truncate().child(e))
                    }),
            )
            .child(
                v_flex()
                    .id("worktrees")
                    .flex_1()
                    .overflow_y_scroll()
                    .children(rows),
            )
            .child(
                v_flex()
                    .p_2()
                    .gap_2()
                    .border_t_1()
                    .border_color(theme.sidebar_border)
                    .child(Input::new(&self.branch_input).small())
                    .child(
                        Button::new("create-worktree")
                            .label("Create worktree")
                            .small()
                            .primary()
                            .on_click(
                                cx.listener(|this, _, window, cx| this.create_worktree(window, cx)),
                            ),
                    ),
            )
    }

    fn render_worktree_row(
        &self,
        ix: usize,
        info: &WorktreeInfo,
        theme: &gpui_kit::component::Theme,
        cx: &mut Context<Self>,
    ) -> Stateful<Div> {
        let selected = self.selected.as_deref() == Some(info.path());
        let path = info.wt.path.clone();
        let branch = info.wt.branch.clone();
        let pr = self.pr_for_branch(branch.as_deref());
        let agent = self.agent_state_for(info, cx);

        let mut meta: Vec<AnyElement> = Vec::new();
        match &info.status {
            Ok(st) => {
                if st.is_clean() {
                    meta.push(div().child("clean").into_any_element());
                } else {
                    meta.push(
                        div()
                            .text_color(theme.warning)
                            .child(format!("{} changed", st.files.len()))
                            .into_any_element(),
                    );
                }
                if st.ahead > 0 {
                    meta.push(div().child(format!("↑{}", st.ahead)).into_any_element());
                }
                if st.behind > 0 {
                    meta.push(div().child(format!("↓{}", st.behind)).into_any_element());
                }
            }
            Err(_) => meta.push(
                div()
                    .text_color(theme.danger)
                    .child("Status unavailable")
                    .into_any_element(),
            ),
        }

        v_flex()
            .id(("worktree", ix))
            .px_3()
            .py_2()
            .gap_0p5()
            .cursor_pointer()
            .border_l_2()
            .border_color(if selected {
                theme.primary
            } else {
                transparent_black()
            })
            .when(selected, |el| el.bg(theme.sidebar_accent))
            .hover(|el| el.bg(theme.sidebar_accent))
            .on_click(cx.listener(move |this, _, _, cx| this.select_worktree(path.clone(), cx)))
            .child(
                h_flex()
                    .gap_2()
                    .child(agent_dot(agent, theme))
                    .child(
                        div()
                            .flex_1()
                            .min_w_0()
                            .truncate()
                            .font_weight(FontWeight::SEMIBOLD)
                            .child(info.wt.display_name()),
                    )
                    .when(info.wt.is_main, |el| {
                        el.child(
                            div()
                                .text_xs()
                                .text_color(theme.muted_foreground)
                                .child("main"),
                        )
                    })
                    .when_some(pr, |el, pr| el.child(pr_badge(pr, theme))),
            )
            .child(
                h_flex()
                    .pl_4()
                    .gap_2()
                    .text_xs()
                    .text_color(theme.muted_foreground)
                    .children(meta)
                    .when_some(agent, |el, a| {
                        el.child(
                            div()
                                .text_color(agent_color(a, theme))
                                .child(format!("claude {}", a.label())),
                        )
                    }),
            )
            .child(
                div()
                    .pl_4()
                    .text_xs()
                    .text_color(theme.muted_foreground)
                    .truncate()
                    .child(info.wt.dir_name()),
            )
    }

    fn render_main(&self, window: &mut Window, cx: &mut Context<Self>) -> AnyElement {
        let theme = cx.theme().clone();
        let Some(info) = self.selected_info().cloned() else {
            if let Some(error) = &self.load_error {
                return self.render_data_notice("Worktrees unavailable", Some(error), false, cx);
            }
            return div()
                .flex_1()
                .flex()
                .items_center()
                .justify_center()
                .text_color(theme.muted_foreground)
                .child(if self.worktrees.is_empty() {
                    "Loading worktrees…"
                } else {
                    "Select a worktree"
                })
                .into_any_element();
        };

        let tab_ix = TABS.iter().position(|t| *t == self.tab).unwrap_or(0);
        let changes = info
            .status
            .as_ref()
            .map(|status| status.files.len().to_string())
            .unwrap_or_else(|_| "—".into());
        let sessions = info.sessions.len();
        let open_prs = self
            .prs
            .iter()
            .filter(|p| matches!(p.state, PrState::Open | PrState::Draft))
            .count();

        let (_, _, main_width) = self.pane_widths(window, cx);
        let content = match self.tab {
            MainTab::Changes => self
                .render_changes(&info, main_width, &theme, cx)
                .into_any_element(),
            MainTab::History => self
                .render_history(main_width, &theme, cx)
                .into_any_element(),
            MainTab::Sessions => self.render_sessions(&info, &theme, cx).into_any_element(),
            MainTab::PullRequests => self.render_prs(&info, &theme, cx).into_any_element(),
        };
        let _ = window;

        v_flex()
            .flex_1()
            .min_w_0()
            .h_full()
            .child(self.render_header(&info, &theme, cx))
            .child(
                TabBar::new("main-tabs")
                    .underline()
                    .px_2()
                    .selected_index(tab_ix)
                    .on_click(cx.listener(|this, ix: &usize, _, cx| {
                        this.tab = TABS[*ix];
                        this.queue_persist(cx);
                        cx.notify();
                    }))
                    .child(Tab::new().label(format!("Changes · {changes}")))
                    .child(Tab::new().label("History"))
                    .child(Tab::new().label(format!("Claude · {sessions}")))
                    .child(Tab::new().label(format!("Pull requests · {open_prs}"))),
            )
            .child(div().flex_1().min_h_0().child(content))
            .into_any_element()
    }

    fn render_header(
        &self,
        info: &WorktreeInfo,
        theme: &gpui_kit::component::Theme,
        cx: &mut Context<Self>,
    ) -> impl IntoElement {
        let path = info.wt.path.clone();
        let status_line = match &info.status {
            Ok(st) => {
                let mut parts = Vec::new();
                if let Some(up) = &st.upstream {
                    parts.push(format!("→ {up}"));
                    parts.push(format!("↑{} ↓{}", st.ahead, st.behind));
                } else if st.branch.is_some() {
                    parts.push("no upstream".into());
                }
                if st.staged_count() > 0 {
                    parts.push(format!("{} staged", st.staged_count()));
                }
                if st.conflicted_count() > 0 {
                    parts.push(format!("{} conflicts", st.conflicted_count()));
                }
                parts.join("  ·  ")
            }
            Err(e) => e.clone(),
        };
        let confirming = self.confirm_remove.as_ref() == Some(&path);
        let dirty = info.changed_files() > 0;
        let terminal = self.terminal;

        let p3 = path.clone();
        let p4 = path.clone();
        let last_session = info.sessions.first().cloned();

        h_flex()
            .px_4()
            .py_3()
            .gap_4()
            .justify_between()
            .border_b_1()
            .border_color(theme.border)
            .child(
                v_flex()
                    .min_w_0()
                    .gap_1()
                    .child(
                        div()
                            .text_lg()
                            .font_weight(FontWeight::BOLD)
                            .truncate()
                            .child(info.wt.display_name()),
                    )
                    .child(
                        div()
                            .text_xs()
                            .text_color(theme.muted_foreground)
                            .truncate()
                            .child(format!("{}   {status_line}", path.display())),
                    ),
            )
            .child(
                h_flex()
                    .gap_1()
                    .flex_none()
                    .child(
                        Button::new("claude-new")
                            .label("✦ New Claude")
                            .small()
                            .primary()
                            .tooltip("New Claude conversation in this worktree (cmd-n)")
                            .on_click(cx.listener(|this, _, window, cx| this.new_chat(window, cx))),
                    )
                    .when_some(last_session, |el, session| {
                        el.child(
                            Button::new("claude-resume")
                                .label("Resume last")
                                .small()
                                .tooltip(format!(
                                    "Continue “{}” in the Claude panel",
                                    session.title
                                ))
                                .on_click(cx.listener(move |this, _, window, cx| {
                                    this.open_session_chat(session.clone(), window, cx)
                                })),
                        )
                    })
                    .child(
                        Button::new("terminal")
                            .label("Terminal")
                            .small()
                            .ghost()
                            .on_click(cx.listener(move |this, _, _, cx| {
                                let r = launch::open_terminal(terminal, &p3, None);
                                this.report(r, "Terminal opened", cx);
                            })),
                    )
                    .child(
                        Button::new("editor")
                            .label("Editor")
                            .small()
                            .ghost()
                            .tooltip("Open in VS Code (or $SUPERGIT_EDITOR)")
                            .on_click(cx.listener(move |this, _, _, cx| {
                                let r = launch::open_in_editor(&p4);
                                this.report(r, "Opened in editor", cx);
                            })),
                    )
                    .child(
                        Button::new("finder")
                            .label("Finder")
                            .small()
                            .ghost()
                            .on_click(cx.listener({
                                let path = path.clone();
                                move |this, _, _, cx| {
                                    let r = launch::reveal_in_finder(&path);
                                    this.report(r, "Opened in Finder", cx);
                                }
                            })),
                    )
                    .when(!info.wt.is_main, |el| {
                        el.child(
                            Button::new("remove")
                                .label(match (confirming, dirty) {
                                    (false, _) => "Remove",
                                    (true, false) => "Confirm remove",
                                    (true, true) => "Discard changes & remove",
                                })
                                .small()
                                .when(confirming, |b| b.danger())
                                .when(!confirming, |b| b.ghost())
                                .on_click(cx.listener(|this, _, _, cx| this.remove_selected(cx))),
                        )
                    }),
            )
    }

    /// Inline feedback keeps errors actionable without a modal or a focus change.
    fn render_data_notice(
        &self,
        title: &str,
        detail: Option<&str>,
        retry_commit: bool,
        cx: &mut Context<Self>,
    ) -> AnyElement {
        let theme = cx.theme();
        v_flex()
            .w_full()
            .min_w_0()
            .p_3()
            .gap_2()
            .text_sm()
            .border_b_1()
            .border_color(theme.border)
            .child(
                div()
                    .font_weight(FontWeight::SEMIBOLD)
                    .child(title.to_string()),
            )
            .when_some(detail.map(str::to_string), |el, detail| {
                el.child(div().text_color(theme.muted_foreground).child(detail))
                    .child(
                        Button::new(if retry_commit {
                            "retry-commit"
                        } else {
                            "retry-git-data"
                        })
                        .label("Retry")
                        .small()
                        .ghost()
                        .on_click(cx.listener(move |this, _, _, cx| {
                            if retry_commit {
                                if let Some(sha) = this.selected_commit.clone() {
                                    this.select_commit(sha, cx);
                                }
                            } else {
                                this.start_polling(cx);
                            }
                        })),
                    )
            })
            .into_any_element()
    }

    fn render_changes(
        &self,
        info: &WorktreeInfo,
        width: f32,
        theme: &gpui_kit::component::Theme,
        cx: &mut Context<Self>,
    ) -> impl IntoElement {
        let error = self
            .load_error
            .as_deref()
            .or_else(|| info.status.as_ref().err().map(String::as_str))
            .or_else(|| self.details.status.error())
            .or_else(|| self.details.patch.error());
        let Some(status) = self.details.status.value() else {
            return if let Some(error) = error {
                self.render_data_notice("Changes unavailable", Some(error), false, cx)
            } else {
                self.render_data_notice("Loading changes…", None, false, cx)
            };
        };
        let notice = error.map(|error| {
            self.render_data_notice(
                "Changes may be out of date — showing the last successful data",
                Some(error),
                false,
                cx,
            )
        });
        let files = status.files.clone();
        if files.is_empty() {
            return v_flex()
                .size_full()
                .children(notice)
                .child(
                    div()
                        .flex_1()
                        .flex()
                        .items_center()
                        .justify_center()
                        .text_color(theme.muted_foreground)
                        .child(if error.is_some() {
                            "Last successful status: no changes"
                        } else {
                            "Working tree clean"
                        }),
                )
                .into_any_element();
        }
        let rows = files.into_iter().enumerate().map(|(ix, f)| {
            let state = f.state();
            let path = f.path.clone();
            h_flex()
                .id(("change", ix))
                .px_3()
                .py_1()
                .gap_2()
                .text_sm()
                .cursor_pointer()
                .hover(|el| el.bg(theme.list_hover))
                .on_click(cx.listener(move |this, _, _, cx| {
                    this.remember_position(
                        ReviewPosition {
                            path: path.clone(),
                            line: 1,
                            original: false,
                        },
                        false,
                        cx,
                    );
                    this.changes_diff
                        .update(cx, |state, cx| state.scroll_to_file(&path, cx));
                }))
                .child(
                    div()
                        .w(px(14.))
                        .flex_none()
                        .font_family(theme.mono_font_family.clone())
                        .text_color(file_state_color(state, theme))
                        .child(state.letter()),
                )
                .child(div().min_w_0().truncate().child(f.path.clone()))
                .when(f.staged.is_some(), |el| {
                    el.child(div().text_xs().text_color(theme.success).child("staged"))
                })
        });
        let content = h_resizable("changes-layout")
            .with_state(&self.changes_layout)
            .on_resize(cx.listener(|this, state, _, cx| {
                this.remember_layout(LayoutPanel::Changes, state, cx)
            }))
            .child(
                resizable_panel()
                    .size(px(self
                        .repository_view()
                        .changes_width
                        .min((width - 180.).clamp(140., 600.))))
                    .size_range(px(140.)..px((width - 180.).clamp(140., 600.)))
                    .child(
                        v_flex()
                            .id("change-list")
                            .w_full()
                            .min_w_0()
                            .h_full()
                            .overflow_y_scroll()
                            .border_r_1()
                            .border_color(theme.border)
                            .children(rows),
                    ),
            )
            .child(
                resizable_panel().size_range(px(180.)..Pixels::MAX).child(
                    div()
                        .flex_1()
                        .min_w_0()
                        .h_full()
                        .child(Diff::new(&self.changes_diff).line_number(true)),
                ),
            )
            .into_any_element();
        v_flex()
            .size_full()
            .children(notice)
            .child(div().flex_1().min_h_0().child(content))
            .into_any_element()
    }

    fn render_history(
        &self,
        width: f32,
        theme: &gpui_kit::component::Theme,
        cx: &mut Context<Self>,
    ) -> impl IntoElement {
        let error = self
            .load_error
            .as_deref()
            .or_else(|| self.details.commits.error());
        let Some(commits) = self.details.commits.value() else {
            return if let Some(error) = error {
                self.render_data_notice("History unavailable", Some(error), false, cx)
            } else {
                self.render_data_notice("Loading history…", None, false, cx)
            };
        };
        let notice = error.map(|error| {
            self.render_data_notice(
                "History may be out of date — showing the last successful data",
                Some(error),
                false,
                cx,
            )
        });
        if commits.is_empty() {
            return v_flex()
                .size_full()
                .children(notice)
                .child(
                    div()
                        .flex_1()
                        .flex()
                        .items_center()
                        .justify_center()
                        .text_color(theme.muted_foreground)
                        .child(if error.is_some() {
                            "Last successful history: no commits"
                        } else {
                            "No commits yet"
                        }),
                )
                .into_any_element();
        }
        let rows = commits.iter().enumerate().map(|(ix, c)| {
            let selected = self.selected_commit.as_deref() == Some(c.sha.as_str());
            let sha = c.sha.clone();
            commit_row(ix, c, selected, theme)
                .on_click(cx.listener(move |this, _, _, cx| this.select_commit(sha.clone(), cx)))
        });
        let content = h_resizable("history-layout")
            .with_state(&self.history_layout)
            .on_resize(cx.listener(|this, state, _, cx| {
                this.remember_layout(LayoutPanel::History, state, cx)
            }))
            .child(
                resizable_panel()
                    .size(px(self
                        .repository_view()
                        .history_width
                        .min((width - 180.).clamp(160., 800.))))
                    .size_range(px(160.)..px((width - 180.).clamp(160., 800.)))
                    .child(
                        v_flex()
                            .id("commit-list")
                            .w_full()
                            .min_w_0()
                            .h_full()
                            .overflow_y_scroll()
                            .border_r_1()
                            .border_color(theme.border)
                            .children(rows),
                    ),
            )
            .child(
                resizable_panel()
                    .size_range(px(180.)..Pixels::MAX)
                    .child(self.render_commit_patch(cx)),
            );
        v_flex()
            .size_full()
            .children(notice)
            .child(div().flex_1().min_h_0().child(content))
            .into_any_element()
    }

    fn render_commit_patch(&self, cx: &mut Context<Self>) -> AnyElement {
        if self.selected_commit.is_none() {
            return self.render_data_notice("Select a commit", None, false, cx);
        }
        if self.commit_patch.is_loading() {
            return self.render_data_notice("Loading commit diff…", None, false, cx);
        }
        let notice = self.commit_patch.error().map(|error| {
            self.render_data_notice(
                if self.commit_patch.value().is_some() {
                    "Commit diff may be out of date — showing the last successful data"
                } else {
                    "Commit diff unavailable"
                },
                Some(error),
                true,
                cx,
            )
        });
        v_flex()
            .size_full()
            .children(notice)
            .when_some(self.commit_patch.value(), |el, patch| {
                el.child(div().flex_1().min_h_0().child(if patch.is_empty() {
                    self.render_data_notice("No changes in this commit", None, false, cx)
                } else {
                    div()
                        .size_full()
                        .child(Diff::new(&self.commit_diff).line_number(true))
                        .into_any_element()
                }))
            })
            .into_any_element()
    }

    fn render_sessions(
        &self,
        info: &WorktreeInfo,
        theme: &gpui_kit::component::Theme,
        cx: &mut Context<Self>,
    ) -> impl IntoElement {
        let terminal = self.terminal;
        let path = info.wt.path.clone();
        if info.sessions.is_empty() {
            return v_flex()
                .size_full()
                .items_center()
                .justify_center()
                .gap_2()
                .text_color(theme.muted_foreground)
                .child("No Claude sessions in this worktree yet")
                .child(
                    Button::new("claude-new-empty")
                        .label("✦ Start Claude here")
                        .primary()
                        .on_click(cx.listener(|this, _, window, cx| this.new_chat(window, cx))),
                )
                .into_any_element();
        }
        let rows = info.sessions.iter().enumerate().map(|(ix, s)| {
            let id = s.id.clone();
            let dir = s.start_cwd.clone().unwrap_or_else(|| path.clone());
            let session = s.clone();
            session_row(ix, s, theme)
                .child(
                    Button::new(("open-session", ix))
                        .label("Open")
                        .small()
                        .tooltip("Continue in the Claude panel")
                        .on_click(cx.listener(move |this, _, window, cx| {
                            this.open_session_chat(session.clone(), window, cx)
                        })),
                )
                .child(
                    Button::new(("resume-terminal", ix))
                        .label("Terminal")
                        .small()
                        .ghost()
                        .tooltip(format!("claude --resume in {}", terminal.name()))
                        .on_click(cx.listener(move |this, _, _, cx| {
                            let r = launch::claude_resume(terminal, &dir, &id);
                            this.report(r, "Session resumed in a new terminal", cx);
                        })),
                )
        });
        v_flex()
            .id("sessions")
            .size_full()
            .overflow_y_scroll()
            .children(rows)
            .into_any_element()
    }

    fn render_prs(
        &self,
        info: &WorktreeInfo,
        theme: &gpui_kit::component::Theme,
        cx: &mut Context<Self>,
    ) -> impl IntoElement {
        if let Some(e) = &self.prs_error {
            return div()
                .p_4()
                .text_color(theme.danger)
                .child(e.clone())
                .into_any_element();
        }
        let current = info.wt.branch.clone();
        let mut prs: Vec<&PullRequest> = self.prs.iter().collect();
        // This worktree's PR first.
        prs.sort_by_key(|p| Some(p.head_branch.as_str()) != current.as_deref());
        let rows = prs.into_iter().enumerate().map(|(ix, pr)| {
            let url = pr.url.clone();
            let pr_owned = pr.clone();
            let mine = Some(pr.head_branch.as_str()) == current.as_deref();
            let has_worktree = self
                .worktrees
                .iter()
                .any(|w| w.wt.branch.as_deref() == Some(pr.head_branch.as_str()));
            pr_row(ix, pr, mine, theme)
                .child(
                    Button::new(("pr-open", ix))
                        .label("Open")
                        .small()
                        .ghost()
                        .on_click(cx.listener(move |this, _, _, cx| {
                            let r = launch::open_url(&url);
                            this.report(r, "Opened in browser", cx);
                        })),
                )
                .when(
                    matches!(pr.state, PrState::Open | PrState::Draft) && !mine,
                    |el| {
                        el.child(
                            Button::new(("pr-wt", ix))
                                .label(if has_worktree {
                                    "Go to worktree"
                                } else {
                                    "Worktree"
                                })
                                .small()
                                .ghost()
                                .on_click(cx.listener(move |this, _, _, cx| {
                                    this.checkout_pr(&pr_owned, cx)
                                })),
                        )
                    },
                )
        });
        v_flex()
            .id("prs")
            .size_full()
            .overflow_y_scroll()
            .when(self.prs.is_empty(), |el| {
                el.child(
                    div()
                        .p_4()
                        .text_color(theme.muted_foreground)
                        .child(if self.prs_loaded {
                            "No open pull requests"
                        } else {
                            "Loading pull requests…"
                        }),
                )
            })
            .children(rows)
            .into_any_element()
    }

    fn render_claude_panel(&mut self, window: &mut Window, cx: &mut Context<Self>) -> AnyElement {
        let path = self.selected.clone().unwrap_or_default();
        // Every worktree gets a ready-to-type conversation; the `claude`
        // process only starts with the first message.
        if !self.chats.contains_key(&path) {
            let saved = self.repository_view().clone();
            let conversations = saved.conversations.get(&path).cloned().unwrap_or_default();
            let mut restored = Vec::new();
            for conversation in conversations
                .iter()
                .filter(|c| c.provider == "claude" || c.provider.is_empty())
            {
                let chat =
                    cx.new(|cx| ChatSession::restore(path.clone(), conversation, window, cx));
                cx.observe(&chat, |this, _, cx| {
                    this.queue_persist(cx);
                    cx.notify();
                })
                .detach();
                restored.push(chat);
            }
            if !restored.is_empty() {
                self.active_chat.insert(
                    path.clone(),
                    saved
                        .active_conversations
                        .get(&path)
                        .copied()
                        .unwrap_or(0)
                        .min(restored.len() - 1),
                );
                self.chats.insert(path.clone(), restored);
            }
        }
        if self.chats.get(&path).is_none_or(Vec::is_empty) {
            let chat = cx.new(|cx| ChatSession::new(path.clone(), window, cx));
            cx.observe(&chat, |this, _, cx| {
                this.queue_persist(cx);
                cx.notify();
            })
            .detach();
            self.chats.insert(path.clone(), vec![chat]);
            self.active_chat.insert(path.clone(), 0);
        }
        let theme = cx.theme().clone();
        let chats = self.chats.get(&path).cloned().unwrap_or_default();
        let active = self
            .active_chat
            .get(&path)
            .copied()
            .unwrap_or(0)
            .min(chats.len().saturating_sub(1));
        let tabs = chats.iter().enumerate().map(|(ix, chat)| {
            let c = chat.read(cx);
            let title: String = c.title().chars().take(22).collect();
            let dot = match c.status() {
                sg_claude::Status::Working | sg_claude::Status::Starting => Some(theme.blue),
                sg_claude::Status::NeedsPermission => Some(theme.warning),
                _ => None,
            };
            let path = path.clone();
            h_flex()
                .id(("chat-tab", ix))
                .flex_none()
                .gap_1()
                .px_2()
                .py_0p5()
                .rounded_md()
                .text_xs()
                .cursor_pointer()
                .when(ix == active, |el| el.bg(theme.list_active))
                .hover(|el| el.bg(theme.list_hover))
                .on_click(cx.listener(move |this, _, _, cx| {
                    this.active_chat.insert(path.clone(), ix);
                    this.queue_persist(cx);
                    cx.notify();
                }))
                .when_some(dot, |el, color| {
                    el.child(div().size(px(6.)).rounded_full().bg(color))
                })
                .child(title)
                .when(chats.len() > 1, |el| {
                    el.child(
                        div()
                            .id(("chat-close", ix))
                            .px_0p5()
                            .text_color(theme.muted_foreground)
                            .hover(|el| el.text_color(theme.foreground))
                            .on_click(cx.listener(move |this, _, _, cx| {
                                cx.stop_propagation();
                                this.close_chat(ix, cx)
                            }))
                            .child("×"),
                    )
                })
        });

        v_flex()
            .w_full()
            .min_w_0()
            .flex_none()
            .h_full()
            .border_l_1()
            .border_color(theme.border)
            .child(
                h_flex()
                    .px_2()
                    .py_1()
                    .gap_1()
                    .border_b_1()
                    .border_color(theme.border)
                    .child(
                        div()
                            .flex_none()
                            .px_1()
                            .text_sm()
                            .font_weight(FontWeight::BOLD)
                            .child("✦ Claude"),
                    )
                    .child(
                        h_flex()
                            .id("chat-tabs")
                            .flex_1()
                            .min_w_0()
                            .gap_1()
                            .overflow_x_scroll()
                            .children(tabs),
                    )
                    .child(
                        Button::new("chat-new")
                            .label("+")
                            .xsmall()
                            .ghost()
                            .tooltip("New conversation (cmd-n)")
                            .on_click(cx.listener(|this, _, window, cx| this.new_chat(window, cx))),
                    )
                    .child(
                        Button::new("chat-hide")
                            .label("⇥")
                            .xsmall()
                            .ghost()
                            .tooltip("Hide the Claude panel (cmd-l)")
                            .on_click(cx.listener(|this, _, window, cx| {
                                this.toggle_claude(&ToggleClaude, window, cx)
                            })),
                    ),
            )
            .child(
                div()
                    .flex_1()
                    .min_h_0()
                    .children(chats.get(active).cloned()),
            )
            .into_any_element()
    }

    fn render_welcome(&self, cx: &mut Context<Self>) -> impl IntoElement {
        let theme = cx.theme().clone();
        let rows = self.recent.iter().enumerate().map(|(ix, root)| {
            let path = root.clone();
            let name = root
                .file_name()
                .map(|n| n.to_string_lossy().into_owned())
                .unwrap_or_default();
            v_flex()
                .id(("recent", ix))
                .px_3()
                .py_2()
                .rounded_md()
                .cursor_pointer()
                .hover(|el| el.bg(theme.list_hover))
                .on_click(cx.listener(move |this, _, _, cx| this.open_path(path.clone(), cx)))
                .child(div().font_weight(FontWeight::SEMIBOLD).child(name))
                .child(
                    div()
                        .text_xs()
                        .text_color(theme.muted_foreground)
                        .child(root.display().to_string()),
                )
        });
        div().flex_1().min_h_0().flex().justify_center().child(
            v_flex()
                .id("welcome")
                .w(px(520.))
                .pt(px(96.))
                .gap_4()
                .overflow_y_scroll()
                .child(
                    div()
                        .text_2xl()
                        .font_weight(FontWeight::BOLD)
                        .child("SuperGIT"),
                )
                .child(
                    div().text_color(theme.muted_foreground).child(
                        "Worktrees, Claude sessions, commits and pull requests in one place.",
                    ),
                )
                .child(
                    h_flex().child(
                        Button::new("welcome-open")
                            .label("Open Repository…")
                            .primary()
                            .on_click(cx.listener(|this, _, window, cx| {
                                this.open_repository(&OpenRepository, window, cx)
                            })),
                    ),
                )
                .when(!self.recent.is_empty(), |el| {
                    el.child(
                        div()
                            .pt_4()
                            .text_xs()
                            .text_color(theme.muted_foreground)
                            .child("RECENT"),
                    )
                    .children(rows)
                }),
        )
    }

    fn render_status_bar(&self, cx: &mut Context<Self>) -> impl IntoElement {
        let theme = cx.theme();
        h_flex()
            .h(px(26.))
            .px_3()
            .gap_3()
            .text_xs()
            .border_t_1()
            .border_color(theme.border)
            .bg(theme.status_bar)
            .text_color(theme.muted_foreground)
            .when_some(self.repo.as_ref(), |el, repo| {
                el.child(repo.root.display().to_string())
            })
            .child(div().flex_1())
            .when_some(self.message.clone(), |el, (msg, is_error)| {
                el.child(
                    div()
                        .truncate()
                        .text_color(if is_error {
                            theme.danger
                        } else {
                            theme.muted_foreground
                        })
                        .child(msg),
                )
            })
    }
}

impl Render for SuperGitApp {
    fn render(&mut self, window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let theme = cx.theme().clone();
        v_flex()
            .size_full()
            .bg(theme.background)
            .text_color(theme.foreground)
            .on_action(cx.listener(Self::refresh))
            .on_action(cx.listener(Self::open_repository))
            .on_action(cx.listener(Self::close_repository))
            .on_action(cx.listener(Self::toggle_claude))
            .on_action(cx.listener(Self::new_chat_action))
            .child(self.render_title_bar(cx))
            .child(if self.repo.is_some() {
                let show_chat = self.chat_open && self.selected.is_some();
                let (sidebar_max, chat_max, _) = self.pane_widths(window, cx);
                let main = self.render_main(window, cx);
                let workspace = if show_chat {
                    let chat = self.render_claude_panel(window, cx);
                    h_resizable("chat-layout")
                        .with_state(&self.chat_layout)
                        .on_resize(cx.listener(|this, state, _, cx| {
                            this.remember_layout(LayoutPanel::Chat, state, cx)
                        }))
                        .child(
                            resizable_panel()
                                .size_range(px(360.)..Pixels::MAX)
                                .child(main),
                        )
                        .child(
                            resizable_panel()
                                .size(px(self.repository_view().chat_width.min(chat_max)))
                                .size_range(px(320.)..px(chat_max))
                                .child(chat),
                        )
                        .into_any_element()
                } else {
                    main
                };
                div()
                    .flex_1()
                    .min_h_0()
                    .child(
                        h_resizable("sidebar-layout")
                            .with_state(&self.sidebar_layout)
                            .on_resize(cx.listener(|this, state, _, cx| {
                                this.remember_layout(LayoutPanel::Sidebar, state, cx)
                            }))
                            .child(
                                resizable_panel()
                                    .size(px(self.repository_view().sidebar_width.min(sidebar_max)))
                                    .size_range(px(220.)..px(sidebar_max))
                                    .child(self.render_sidebar(cx)),
                            )
                            .child(
                                resizable_panel()
                                    .size_range(
                                        px(if show_chat { 680. } else { 360. })..Pixels::MAX,
                                    )
                                    .child(workspace),
                            ),
                    )
                    .into_any_element()
            } else {
                self.render_welcome(cx).into_any_element()
            })
            .child(self.render_status_bar(cx))
    }
}

/// `SUPERGIT_TAB=changes|history|claude|prs` picks the tab shown at startup.
fn initial_tab() -> MainTab {
    match std::env::var("SUPERGIT_TAB").unwrap_or_default().as_str() {
        "history" => MainTab::History,
        "claude" | "sessions" => MainTab::Sessions,
        "prs" | "pr" => MainTab::PullRequests,
        _ => MainTab::Changes,
    }
}

// ---- row builders (pure presentation) -------------------------------------

fn agent_color(state: AgentState, theme: &gpui_kit::component::Theme) -> Hsla {
    match state {
        AgentState::Working => theme.blue,
        AgentState::NeedsAttention => theme.warning,
        AgentState::Finished => theme.success,
        AgentState::Idle => theme.muted_foreground,
    }
}

fn agent_dot(state: Option<AgentState>, theme: &gpui_kit::component::Theme) -> impl IntoElement {
    div()
        .size(px(8.))
        .flex_none()
        .rounded_full()
        .bg(match state {
            Some(s) => agent_color(s, theme),
            None => theme.border,
        })
}

fn file_state_color(state: FileState, theme: &gpui_kit::component::Theme) -> Hsla {
    match state {
        FileState::Added | FileState::Untracked => theme.success,
        FileState::Deleted => theme.danger,
        FileState::Conflicted => theme.red,
        FileState::Renamed | FileState::Copied => theme.blue,
        FileState::Modified | FileState::TypeChanged => theme.warning,
    }
}

fn pr_color(state: PrState, theme: &gpui_kit::component::Theme) -> Hsla {
    match state {
        PrState::Open => theme.success,
        PrState::Draft => theme.muted_foreground,
        PrState::Merged => theme.magenta,
        PrState::Closed => theme.danger,
    }
}

fn pr_badge(pr: &PullRequest, theme: &gpui_kit::component::Theme) -> impl IntoElement {
    let checks = if pr.checks.failed > 0 {
        Some(("✗", theme.danger))
    } else if pr.checks.pending > 0 {
        Some(("●", theme.warning))
    } else if pr.checks.total() > 0 {
        Some(("✓", theme.success))
    } else {
        None
    };
    h_flex()
        .gap_1()
        .text_xs()
        .text_color(pr_color(pr.state, theme))
        .child(format!("#{}", pr.number))
        .when_some(checks, |el, (sym, color)| {
            el.child(div().text_color(color).child(sym))
        })
}

fn time_ago(t: SystemTime) -> String {
    t.duration_since(SystemTime::UNIX_EPOCH)
        .map(|d| relative_time(d.as_secs() as i64))
        .unwrap_or_default()
}

fn commit_row(
    ix: usize,
    c: &Commit,
    selected: bool,
    theme: &gpui_kit::component::Theme,
) -> Stateful<Div> {
    h_flex()
        .id(("commit", ix))
        .px_3()
        .py_1()
        .gap_2()
        .text_sm()
        .cursor_pointer()
        .when(selected, |el| el.bg(theme.list_active))
        .hover(|el| el.bg(theme.list_hover))
        .child(
            div()
                .flex_none()
                .font_family(theme.mono_font_family.clone())
                .text_xs()
                .text_color(theme.muted_foreground)
                .child(c.short_sha().to_string()),
        )
        .when(c.by_agent, |el| {
            el.child(
                div()
                    .flex_none()
                    .text_xs()
                    .text_color(theme.blue)
                    .child("✦"),
            )
        })
        .child(div().flex_1().min_w_0().truncate().child(c.subject.clone()))
        .children(c.refs.iter().take(3).map(|r| {
            div()
                .flex_none()
                .px_1()
                .rounded_sm()
                .text_xs()
                .bg(theme.secondary)
                .text_color(theme.secondary_foreground)
                .child(r.replace("HEAD -> ", "➜ "))
        }))
        .child(
            div()
                .flex_none()
                .w(px(90.))
                .text_xs()
                .text_color(theme.muted_foreground)
                .truncate()
                .child(c.author.clone()),
        )
        .child(
            div()
                .flex_none()
                .w(px(32.))
                .text_xs()
                .text_color(theme.muted_foreground)
                .child(relative_time(c.time)),
        )
}

fn session_row(ix: usize, s: &Session, theme: &gpui_kit::component::Theme) -> Stateful<Div> {
    h_flex()
        .id(("session", ix))
        .px_4()
        .py_2()
        .gap_3()
        .border_b_1()
        .border_color(theme.border)
        .child(
            div()
                .w(px(76.))
                .flex_none()
                .text_xs()
                .text_color(agent_color(s.state, theme))
                .child(format!("● {}", s.state.label())),
        )
        .child(
            v_flex()
                .flex_1()
                .min_w_0()
                .gap_0p5()
                .child(div().truncate().child(s.title.clone()))
                .when_some(s.last_prompt.clone().filter(|p| *p != s.title), |el, p| {
                    el.child(
                        div()
                            .text_xs()
                            .text_color(theme.muted_foreground)
                            .truncate()
                            .child(format!("› {p}")),
                    )
                }),
        )
        .when_some(s.git_branch.clone(), |el, b| {
            el.child(
                div()
                    .flex_none()
                    .text_xs()
                    .text_color(theme.muted_foreground)
                    .child(b),
            )
        })
        .child(
            div()
                .flex_none()
                .w(px(32.))
                .text_xs()
                .text_color(theme.muted_foreground)
                .child(time_ago(s.modified)),
        )
}

fn pr_row(
    ix: usize,
    pr: &PullRequest,
    mine: bool,
    theme: &gpui_kit::component::Theme,
) -> Stateful<Div> {
    let review = match pr.review_decision.as_str() {
        "APPROVED" => Some(("approved", theme.success)),
        "CHANGES_REQUESTED" => Some(("changes requested", theme.danger)),
        "REVIEW_REQUIRED" => Some(("review required", theme.muted_foreground)),
        _ => None,
    };
    h_flex()
        .id(("pr", ix))
        .px_4()
        .py_2()
        .gap_3()
        .border_b_1()
        .border_color(theme.border)
        .when(mine, |el| el.bg(theme.list_active))
        .child(
            div()
                .w(px(56.))
                .flex_none()
                .text_xs()
                .text_color(pr_color(pr.state, theme))
                .child(pr.state.label()),
        )
        .child(
            v_flex()
                .flex_1()
                .min_w_0()
                .gap_0p5()
                .child(
                    div()
                        .truncate()
                        .child(format!("#{} {}", pr.number, pr.title)),
                )
                .child(
                    h_flex()
                        .gap_2()
                        .text_xs()
                        .text_color(theme.muted_foreground)
                        .child(format!("{} → {}", pr.head_branch, pr.base_branch))
                        .child(format!("@{}", pr.author))
                        .child(
                            div()
                                .text_color(theme.success)
                                .child(format!("+{}", pr.additions)),
                        )
                        .child(
                            div()
                                .text_color(theme.danger)
                                .child(format!("−{}", pr.deletions)),
                        )
                        .when(pr.checks.total() > 0, |el| {
                            el.child(format!(
                                "checks ✓{} ✗{} ●{}",
                                pr.checks.passed, pr.checks.failed, pr.checks.pending
                            ))
                        })
                        .when_some(review, |el, (label, color)| {
                            el.child(div().text_color(color).child(label))
                        }),
                ),
        )
}
