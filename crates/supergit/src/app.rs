//! The main window: worktrees on the left, the selected worktree's changes,
//! history, agent sessions and pull requests on the right.

mod sync;

mod base;
mod edit;
mod issue_worktree;
mod projects;
mod pulls;
mod sidebar;
mod staged_commit;
mod staging;
mod worktrees;

mod clone_repository;

use crate::MOD;
use std::collections::{HashMap, HashSet};
use std::path::{Path, PathBuf};
use std::time::{Duration, SystemTime};

use gpui_kit::assets::IconName;
use gpui_kit::base::{Disableable as _, Selectable as _};
use gpui_kit::component::button::{Button, ButtonVariants as _};
use gpui_kit::component::diff::{
    Diff, DiffAnnotation, DiffEvent, DiffFile, DiffFileStatus, DiffLinePosition, DiffMode,
    DiffSide, DiffState,
};
use gpui_kit::component::input::{Input, InputEvent, InputState};
use gpui_kit::component::menu::{ContextMenuExt as _, PopupMenu, PopupMenuItem};
use gpui_kit::component::resizable::{ResizableState, h_resizable, resizable_panel};
use gpui_kit::component::tab::{Tab, TabBar};
use gpui_kit::component::tooltip::Tooltip;
use gpui_kit::component::{ActiveTheme as _, Icon, Sizable as _, TitleBar, h_flex, v_flex};
use gpui_kit::prelude::*;
use gpui_kit::*;
use sg_agents::{AgentState, Provider, Session};
use sg_git::{
    ChangeScope, Commit, CommitDetails, FileChange, FileState, RefKind, RefLabel, Repo,
    SubmoduleChange, relative_time,
};
use sg_github::{PrState, PullRequest};

use crate::agent_context::{self, Attachment};
use crate::attention::Kind;
use crate::avatars;
use crate::chat::ChatSession;
use crate::difft_view::{self, DifftView};
use crate::file_tree::{self, TreeRow};
use crate::graph;
use crate::launch::{self, TerminalApp};
use crate::model::{self, Details, History, LoadState, SharedSessionIndex, WorktreeInfo};
use crate::notifications;
use crate::settings::{Appearance, DiffLayout};
use crate::switcher::{self, Destination, Entry};
use crate::workspace::{self, RepositoryView, ReviewPosition, WindowRect};

const POLL_INTERVAL: Duration = Duration::from_secs(3);
/// Network requests run independently so they cannot delay local refreshes.
const PR_POLL_INTERVAL: Duration = Duration::from_secs(60);

actions!(
    supergit,
    [
        Refresh,
        OpenRepository,
        CloseRepository,
        ToggleClaude,
        ToggleSidebar,
        NewClaudeChat,
        OpenSwitcher,
        CloseSwitcher,
        SwitcherNext,
        SwitcherPrevious,
        OpenSettings,
        AskAgent,
        AskAgentNewConversation,
        ToggleKeepWorktree,
        CleanUpWorktree,
        UpdateBase
    ]
);

mod stashes;

type ClickHandler = Box<dyn Fn(&ClickEvent, &mut Window, &mut App)>;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum MainTab {
    History,
    Changes,
    Sessions,
    PullRequests,
}

impl MainTab {
    fn from_name(name: &str) -> Self {
        match name {
            "changes" => Self::Changes,
            "claude" | "sessions" => Self::Sessions,
            "prs" | "pr" => Self::PullRequests,
            _ => Self::History,
        }
    }
}

#[derive(Clone, Copy)]
enum LayoutPanel {
    Chat,
    Changes,
    History,
    CommitFiles,
    Pulls,
}

/// The project a sidebar row belongs to: the active one (live data, every
/// action) or another one, shown from its last snapshot.
#[derive(Clone, Copy)]
struct Scope<'a> {
    view: &'a RepositoryView,
    prs: &'a [PullRequest],
    /// The pull requests tell merged branches apart (completed worktrees).
    prs_fresh: bool,
    active: bool,
}

impl<'a> Scope<'a> {
    fn kept(&self, path: &Path) -> bool {
        self.view.worktrees.get(path).is_some_and(|view| view.kept)
    }

    fn pr_for_branch(&self, branch: Option<&str>) -> Option<&'a PullRequest> {
        let branch = branch?;
        // Open pull requests come first, so one wins over an old merged one.
        self.prs.iter().find(|pr| pr.head_branch == branch)
    }
}

/// A file list beside a diff: the Changes tab's, the selected commit's, or
/// the selected pull request's.
#[derive(Clone, Copy, PartialEq, Eq)]
enum FileList {
    Changes,
    Commit,
    Pull,
}

#[derive(Clone, Copy)]
enum ContextScope {
    Auto,
    Hunk,
    File,
}

struct ReviewView {
    details: Details,
    history: History,
    selected_commit: Option<String>,
    commit_patch: LoadState<String>,
    commit_submodules: Vec<SubmoduleChange>,
    changes_diff: Entity<DiffState>,
    commit_diff: Entity<DiffState>,
    changes_difft: Entity<DifftView>,
    commit_difft: Entity<DifftView>,
}

struct Switcher {
    previous_focus: Option<FocusHandle>,
    entries: Vec<Entry>,
    selected: usize,
    scroll: ScrollHandle,
}

const TABS: [MainTab; 4] = [
    MainTab::History,
    MainTab::Changes,
    MainTab::Sessions,
    MainTab::PullRequests,
];

pub struct SuperGitApp {
    worktree_dialog: Option<issue_worktree::WorktreeDialog>,
    worktree_dialog_handle: FocusHandle,
    creation_base: LoadState<sg_git::BaseInfo>,
    creation_base_task: Option<Task<()>>,
    base_update_error: Option<String>,
    sync_dialog: Option<sync::SyncDialog>,
    sync_focus: FocusHandle,
    base_update_paths: std::collections::HashSet<PathBuf>,
    cleanup_task: Option<Task<()>>,
    cleanup_paths: std::collections::HashSet<PathBuf>,
    cleanup_results: Vec<(String, bool)>,
    cleanup_results_open: bool,
    stash_dialog: Option<stashes::StashDialog>,
    stash_handle: FocusHandle,
    observed_open_prs: HashMap<u64, worktrees::ObservedPr>,
    cleanup_attempted: std::collections::HashSet<String>,
    cleanup_preview: Option<worktrees::CleanupPreview>,
    cleanup_handle: FocusHandle,
    rebase_dialog: Option<base::RebaseDialog>,
    rebase_handle: FocusHandle,
    prs_fetched_at: Option<std::time::Instant>,
    focus_handle: FocusHandle,
    settings_open: bool,
    settings_focus: Option<FocusHandle>,
    settings_handle: FocusHandle,
    notified: HashMap<String, u64>,
    pending_notification: Option<notifications::Target>,
    attention_cursor: HashMap<Kind, usize>,
    base_inputs: HashMap<PathBuf, Entity<InputState>>,
    switcher_input: Entity<InputState>,
    switcher: Option<Switcher>,
    /// `None` shows the welcome screen (e.g. first launch from the Dock).
    repo: Option<Repo>,
    recent: Vec<PathBuf>,
    clone_repository: Option<clone_repository::CloneForm>,
    clone_focus: FocusHandle,
    launch_dir: PathBuf,
    terminal: TerminalApp,
    index: SharedSessionIndex,

    worktrees: Vec<WorktreeInfo>,
    load_error: Option<String>,
    prs: Vec<PullRequest>,
    prs_error: Option<String>,
    prs_loaded: bool,
    /// The Pull requests tab: search, scope, details, merging.
    pulls: pulls::Pulls,

    selected: Option<PathBuf>,
    tab: MainTab,
    details: Details,
    history: History,
    changes_diff: Entity<DiffState>,
    /// The sidebar's width (window-wide, not per project).
    sidebar_layout: Entity<ResizableState>,
    chat_layout: Entity<ResizableState>,
    changes_layout: Entity<ResizableState>,
    history_layout: Entity<ResizableState>,
    history_search: Entity<InputState>,
    selected_commit: Option<String>,
    commit_diff: Entity<DiffState>,
    /// The same diffs through difftastic, loaded only while it is on.
    changes_difft: Entity<DifftView>,
    commit_difft: Entity<DifftView>,
    /// Unified or Split, for every diff (Settings).
    diff_layout: DiffLayout,
    /// Show diffs through difftastic (Settings); needs `difft` on PATH.
    difftastic: bool,
    /// `None` until looked up; then the installed version, if any.
    difft_version: Option<Option<String>>,
    commit_patch: LoadState<String>,
    /// The submodules the selected commit moves, inlined in its patch.
    commit_submodules: Vec<SubmoduleChange>,
    /// The selected commit's card (full message, dates); matched by sha.
    commit_details: Option<CommitDetails>,
    details_generation: u64,
    commit_generation: u64,

    branch_input: Entity<InputState>,
    git_editor: edit::GitEditor,
    /// Filters the Changes tab's file list by path.
    staged_commit: Option<staged_commit::StagedCommit>,
    staged_commit_handle: FocusHandle,
    changes_filter: Entity<InputState>,
    /// Folders collapsed in the Changes tab's tree view.
    changes_collapsed: HashSet<String>,
    /// Filters the selected commit's file list in the History tab.
    commit_filter: Entity<InputState>,
    commit_collapsed: HashSet<String>,
    commit_files_layout: Entity<ResizableState>,
    /// Inactive projects' last worktrees and PRs, for the sidebar and
    /// instant switching.
    project_cache: HashMap<PathBuf, projects::ProjectSnapshot>,
    /// Background reads of inactive projects that have no snapshot yet.
    project_loads: HashMap<PathBuf, projects::ProjectLoad>,

    /// Native agent conversations (Claude, Codex) per worktree (tabs in the
    /// right panel).
    /// They keep running while other worktrees, or projects, are selected.
    chats: HashMap<PathBuf, Vec<Entity<ChatSession>>>,
    active_chat: HashMap<PathBuf, usize>,
    /// The repository root each conversation's worktree belongs to.
    chat_projects: HashMap<PathBuf, PathBuf>,
    chat_open: bool,
    message: Option<(String, bool)>,

    persisted: workspace::Store,
    review_views: HashMap<PathBuf, ReviewView>,
    persist_task: Option<Task<()>>,
    window_rect: Option<WindowRect>,
    poll_task: Option<Task<()>>,
    prs_task: Option<Task<()>>,
    details_task: Option<Task<()>>,
    history_task: Option<Task<()>>,
    history_generation: u64,
    commit_task: Option<Task<()>>,
    action_task: Option<Task<()>>,
    /// Commit authors' GitHub avatars, loaded once per repository.
    avatars: avatars::Avatars,
    avatars_root: Option<PathBuf>,
    avatars_task: Option<Task<()>>,
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
        let switcher_input = cx.new(|cx| {
            InputState::new(window, cx).placeholder("Branch, PR number, conversation or project…")
        });
        let switcher_subscription = cx.subscribe_in(
            &switcher_input,
            window,
            |this, _, event: &InputEvent, window, cx| match event {
                InputEvent::Change => this.filter_switcher(cx),
                InputEvent::PressEnter { .. } => this.accept_switcher(window, cx),
                _ => {}
            },
        );
        let subscription = cx.subscribe_in(
            &branch_input,
            window,
            |this, _, event: &InputEvent, window, cx| {
                if matches!(event, InputEvent::PressEnter { .. }) {
                    this.create_worktree(window, cx);
                }
            },
        );
        let changes_filter = cx.new(|cx| InputState::new(window, cx).placeholder("Filter files"));
        let commit_filter = cx.new(|cx| InputState::new(window, cx).placeholder("Filter files"));
        let history_search = cx.new(|cx| InputState::new(window, cx).placeholder("Search commits"));
        let history_search_subscription = cx.subscribe_in(
            &history_search,
            window,
            |this, _, event: &InputEvent, _, cx| match event {
                InputEvent::Change => cx.notify(),
                InputEvent::PressEnter { .. } => this.open_history_match(cx),
                _ => {}
            },
        );
        let filter_subscriptions = [&changes_filter, &commit_filter].map(|input| {
            cx.subscribe(input, |_, _, event: &InputEvent, cx| {
                if matches!(event, InputEvent::Change) {
                    cx.notify();
                }
            })
        });
        // In "System" mode the theme follows macOS light/dark live.
        let appearance_subscription = cx.observe_window_appearance(window, |_, window, cx| {
            crate::appearance::system_appearance_changed(window, cx);
        });
        let settings = crate::settings::load();
        let diff_layout = settings.diff_layout;
        let changes_diff = new_diff_state(diff_layout, cx);
        let commit_diff = new_diff_state(diff_layout, cx);
        let changes_difft = cx.new(|_| DifftView::new(diff_mode(diff_layout)));
        let commit_difft = cx.new(|_| DifftView::new(diff_mode(diff_layout)));
        let pulls = pulls::Pulls::new(window, diff_layout, cx);

        let mut persisted = workspace::load();
        if let Some(repo) = &repo {
            persisted.projects.add(&repo.root);
        }
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
            worktree_dialog: None,
            worktree_dialog_handle: cx.focus_handle(),
            creation_base: LoadState::Loading,
            creation_base_task: None,
            base_update_error: None,
            sync_dialog: None,
            sync_focus: cx.focus_handle(),
            base_update_paths: Default::default(),
            cleanup_task: None,
            cleanup_paths: Default::default(),
            cleanup_results: Vec::new(),
            cleanup_results_open: false,
            stash_dialog: None,
            stash_handle: cx.focus_handle(),
            observed_open_prs: HashMap::new(),
            cleanup_attempted: Default::default(),
            cleanup_preview: None,
            cleanup_handle: cx.focus_handle(),
            rebase_dialog: None,
            rebase_handle: cx.focus_handle(),
            prs_fetched_at: None,
            focus_handle: cx.focus_handle(),
            settings_open: cfg!(feature = "screenshot")
                && std::env::var_os("SUPERGIT_DEBUG_SETTINGS").is_some(),
            settings_focus: None,
            settings_handle: cx.focus_handle(),
            notified: HashMap::new(),
            pending_notification: None,
            attention_cursor: HashMap::new(),
            base_inputs: HashMap::new(),
            switcher_input,
            switcher: None,
            repo,
            recent: crate::recent::load(),
            clone_repository: None,
            clone_focus: cx.focus_handle(),
            launch_dir,
            terminal: TerminalApp::detect(),
            index: Default::default(),
            worktrees: Vec::new(),
            load_error: None,
            prs: Vec::new(),
            prs_error: None,
            prs_loaded: false,
            pulls,
            selected: None,
            tab: initial_tab(),
            details: Details::default(),
            history: History::default(),
            changes_diff,
            sidebar_layout: cx.new(|_| ResizableState::default()),
            chat_layout: cx.new(|_| ResizableState::default()),
            changes_layout: cx.new(|_| ResizableState::default()),
            history_layout: cx.new(|_| ResizableState::default()),
            history_search,
            selected_commit: None,
            commit_diff,
            changes_difft,
            commit_difft,
            diff_layout,
            difftastic: settings.difftastic,
            difft_version: None,
            commit_patch: LoadState::Loading,
            commit_submodules: Vec::new(),
            commit_details: None,
            details_generation: 0,
            commit_generation: 0,
            branch_input,
            git_editor: Default::default(),
            staged_commit: None,
            staged_commit_handle: cx.focus_handle(),
            changes_filter,
            changes_collapsed: HashSet::new(),
            commit_filter,
            commit_collapsed: HashSet::new(),
            commit_files_layout: cx.new(|_| ResizableState::default()),
            project_cache: HashMap::new(),
            project_loads: HashMap::new(),
            chats: HashMap::new(),
            active_chat: HashMap::new(),
            chat_projects: HashMap::new(),
            chat_open,
            message: None,
            persisted,
            review_views: HashMap::new(),
            persist_task: None,
            window_rect: None,
            poll_task: None,
            prs_task: None,
            details_task: None,
            history_task: None,
            history_generation: 0,
            commit_task: None,
            action_task: None,
            avatars: avatars::Avatars::new(),
            avatars_root: None,
            avatars_task: None,
            _subscriptions: vec![
                subscription,
                switcher_subscription,
                appearance_subscription,
                quit_subscription,
                release_subscription,
                bounds_subscription,
                history_search_subscription,
            ],
        };
        this._subscriptions.extend(filter_subscriptions);
        this.detect_difftastic(cx);
        this.focus_handle.focus(window, cx);
        this.start_polling(cx);
        this.load_inactive_projects(cx);
        this
    }

    /// The agent panel's widest size and the main area's width, beside the
    /// sidebar.
    fn pane_widths(&self, window: &Window, cx: &App) -> (f32, f32) {
        let total = window.viewport_size().width.as_f32() - self.sidebar_width(cx);
        let show_chat = self.chat_open && self.selected.is_some();
        let saved = self.repository_view();
        let chat_max = (total - 360.).clamp(320., 960.);
        let chat = if show_chat {
            self.chat_layout
                .read(cx)
                .sizes()
                .get(1)
                .map(|v| v.as_f32())
                .unwrap_or(saved.chat_width)
                .clamp(320., chat_max)
        } else if self.selected.is_some() {
            RAIL_WIDTH
        } else {
            0.
        };
        (chat_max, (total - chat).max(360.))
    }

    fn repository_view(&self) -> &RepositoryView {
        match &self.repo {
            Some(repo) => self.view_of(&repo.root),
            None => empty_view(),
        }
    }

    /// A project's saved view (the defaults until it has one).
    fn view_of(&self, root: &Path) -> &RepositoryView {
        self.persisted
            .repositories
            .get(root)
            .unwrap_or_else(|| empty_view())
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
        let current = self.repo.as_ref().map(|repo| repo.root.clone());
        if let Some(root) = &current
            && !self.worktrees.is_empty()
        {
            let view = self.persisted.repositories.entry(root.clone()).or_default();
            view.selected = self.selected.clone();
            view.chat_open = self.chat_open;
        }
        // Conversations of every project, including those running in the
        // background while another project is shown.
        let mut touched: Vec<PathBuf> = current.into_iter().collect();
        for (path, chats) in &self.chats {
            let Some(root) = self.chat_projects.get(path) else {
                continue;
            };
            let view = self.persisted.repositories.entry(root.clone()).or_default();
            let mut snapshots: Vec<_> = chats
                .iter()
                .map(|chat| chat.read(cx).snapshot(cx))
                .collect();
            if let Some(saved) = view.conversations.get(path) {
                snapshots.extend(
                    saved
                        .iter()
                        .filter(|conversation| !native_provider(&conversation.provider))
                        .cloned(),
                );
            }
            view.conversations.insert(path.clone(), snapshots);
            view.active_conversations.insert(
                path.clone(),
                self.active_chat.get(path).copied().unwrap_or(0),
            );
            if !touched.contains(root) {
                touched.push(root.clone());
            }
        }
        let views: Vec<_> = touched
            .iter()
            .filter_map(|root| {
                self.persisted
                    .repositories
                    .get(root)
                    .map(|view| (root.as_path(), view))
            })
            .collect();
        workspace::save(&views, &self.persisted.projects, self.window_rect);
    }

    fn cache_review(&mut self, cx: &App) {
        self.capture_saved_review(cx);
        if let Some(path) = &self.selected {
            self.review_views.insert(
                path.clone(),
                ReviewView {
                    details: self.details.clone(),
                    history: self.history.clone(),
                    selected_commit: self.selected_commit.clone(),
                    commit_patch: self.commit_patch.clone(),
                    commit_submodules: self.commit_submodules.clone(),
                    changes_diff: self.changes_diff.clone(),
                    commit_diff: self.commit_diff.clone(),
                    changes_difft: self.changes_difft.clone(),
                    commit_difft: self.commit_difft.clone(),
                },
            );
        }
    }

    fn restore_review(&mut self, cx: &mut Context<Self>) {
        let Some(path) = self.selected.clone() else {
            return;
        };
        // Every worktree opens on History (or `SUPERGIT_TAB`); the other
        // tabs are one click away and are not remembered per worktree.
        self.tab = initial_tab();
        if let Some(cached) = self.review_views.remove(&path) {
            self.details = cached.details;
            self.history = cached.history;
            self.selected_commit = cached.selected_commit;
            self.commit_patch = cached.commit_patch;
            self.commit_submodules = cached.commit_submodules;
            self.changes_diff = cached.changes_diff;
            self.commit_diff = cached.commit_diff;
            self.changes_difft = cached.changes_difft;
            self.commit_difft = cached.commit_difft;
            return;
        }
        let saved = self
            .repository_view()
            .worktrees
            .get(&path)
            .cloned()
            .unwrap_or_default();
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
                if let DiffEvent::SelectionChanged(range) = event {
                    if !commit {
                        cx.notify();
                    }
                    let Some(range) = range else {
                        return;
                    };
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

    /// Flat paths or a folder tree in the Changes tab (remembered per repository).
    fn set_changes_tree(&mut self, tree: bool, cx: &mut Context<Self>) {
        let Some(repo) = &self.repo else { return };
        self.persisted
            .repositories
            .entry(repo.root.clone())
            .or_default()
            .changes_tree = tree;
        self.queue_persist(cx);
        cx.notify();
    }

    /// Click handler for a file in `list`: scroll its diff to the file.
    fn open_file(list: FileList, path: String, cx: &mut Context<Self>) -> ClickHandler {
        Box::new(cx.listener(move |this, _: &ClickEvent, _, cx| {
            let (diff, difft) = match list {
                FileList::Pull => (this.pulls.diff.clone(), this.pulls.difft.clone()),
                FileList::Changes | FileList::Commit => {
                    let commit = list == FileList::Commit;
                    this.remember_position(
                        ReviewPosition {
                            path: path.clone(),
                            line: 1,
                            original: false,
                        },
                        commit,
                        cx,
                    );
                    if commit {
                        (this.commit_diff.clone(), this.commit_difft.clone())
                    } else {
                        (this.changes_diff.clone(), this.changes_difft.clone())
                    }
                }
            };
            diff.update(cx, |state, cx| state.scroll_to_file(&path, cx));
            difft.update(cx, |view, cx| view.scroll_to_file(&path, cx));
        }))
    }

    /// Right-click on a file row: ask an agent about that file.
    fn file_menu(row: Stateful<Div>, path: String, cx: &mut Context<Self>) -> AnyElement {
        let owner = cx.entity().downgrade();
        row.context_menu(move |menu, _, cx| {
            let Some(owner) = owner.upgrade() else {
                return menu;
            };
            owner.update(cx, |this, cx| this.context_menu(menu, Some(&path), cx))
        })
        .into_any_element()
    }

    fn toggle_dir(&mut self, list: FileList, path: String, cx: &mut Context<Self>) {
        let collapsed = match list {
            FileList::Changes => &mut self.changes_collapsed,
            FileList::Commit => &mut self.commit_collapsed,
            FileList::Pull => &mut self.pulls.files_collapsed,
        };
        if !collapsed.remove(&path) {
            collapsed.insert(path);
        }
        cx.notify();
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
            LayoutPanel::Chat => view.chat_width = width,
            LayoutPanel::Changes => view.changes_width = width,
            LayoutPanel::History => view.history_width = width,
            LayoutPanel::CommitFiles => view.commit_files_width = width,
            LayoutPanel::Pulls => view.pulls_width = width,
        }
        view.sanitize();
        self.queue_persist(cx);
    }

    fn navigation_entries(&self, cx: &App) -> Vec<Entry> {
        let mut entries = Vec::new();
        for info in self
            .worktrees
            .iter()
            .filter(|info| info.status.is_ok() && !info.wt.is_bare && info.wt.prunable.is_none())
        {
            let path = info.wt.path.clone();
            let branch = info.wt.branch.as_deref().unwrap_or("Detached HEAD");
            let pr = self
                .pr_for_branch(info.wt.branch.as_deref())
                .map(|pr| format!(" #{} {}", pr.number, pr.title))
                .unwrap_or_default();
            let detail = path.display().to_string();
            let search = format!("{branch}{pr} {detail}");
            entries.push(Entry {
                key: format!("worktree:{}", path.display()),
                path: path.clone(),
                title: format!("{branch}{pr}"),
                detail: format!("Worktree · {detail}"),
                search: search.clone(),
                destination: Destination::Worktree,
            });
            let mut opened_sessions = std::collections::HashSet::new();
            if let Some(chats) = self.chats.get(&path) {
                for chat in chats {
                    let chat = chat.read(cx);
                    let provider = chat.provider();
                    if let Some(id) = chat.session_id() {
                        opened_sessions.insert((provider, id.to_string()));
                    }
                    entries.push(Entry {
                        key: format!("chat:{}:{}", path.display(), chat.local_id()),
                        path: path.clone(),
                        title: chat.title(),
                        detail: format!("{} · {branch} · {}", provider.label(), path.display()),
                        search: format!("{} {search} {}", chat.title(), provider.as_str()),
                        destination: Destination::Conversation(chat.local_id().into()),
                    });
                }
            } else if let Some(chats) = self.repository_view().conversations.get(&path) {
                for chat in chats.iter() {
                    let Some(provider) = Provider::parse(&chat.provider) else {
                        continue;
                    };
                    if let Some(id) = &chat.session_id {
                        opened_sessions.insert((provider, id.clone()));
                    }
                    entries.push(Entry {
                        key: format!("chat:{}:{}", path.display(), chat.local_id),
                        path: path.clone(),
                        title: chat.title.clone(),
                        detail: format!("{} · {branch} · {}", provider.label(), path.display()),
                        search: format!("{} {search} {}", chat.title, provider.as_str()),
                        destination: Destination::Conversation(chat.local_id.clone()),
                    });
                }
            }
            for session in info
                .sessions
                .iter()
                .filter(|s| !opened_sessions.contains(&(s.provider, s.id.clone())))
            {
                let provider = session.provider;
                entries.push(Entry {
                    key: format!("session:{}:{}", provider.as_str(), session.id),
                    path: path.clone(),
                    title: session.title.clone(),
                    detail: format!("{} · {branch} · {}", provider.label(), path.display()),
                    search: format!("{} {search} {}", session.title, provider.as_str()),
                    destination: Destination::Transcript(session.clone()),
                });
            }
        }
        for root in self
            .persisted
            .projects
            .roots
            .iter()
            .filter(|root| self.active_root() != Some(root.as_path()))
        {
            let name = root
                .file_name()
                .map(|name| name.to_string_lossy().into_owned())
                .unwrap_or_default();
            entries.push(Entry {
                key: format!("project:{}", root.display()),
                path: root.clone(),
                title: name.clone(),
                detail: format!("Project · {}", root.display()),
                search: format!("{name} {} project", root.display()),
                destination: Destination::Project,
            });
        }
        entries
    }

    fn remember_target(&mut self, key: String, cx: &mut Context<Self>) {
        if let Some(repo) = &self.repo {
            let recent = &mut self
                .persisted
                .repositories
                .entry(repo.root.clone())
                .or_default()
                .recent_targets;
            recent.retain(|previous| previous != &key);
            recent.insert(0, key);
            recent.truncate(50);
            self.queue_persist(cx);
        }
    }

    pub(crate) fn open_switcher(
        &mut self,
        _: &OpenSwitcher,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        if self.cleanup_preview.is_some() {
            self.close_cleanup_preview(window, cx);
        }
        self.close_rebase_dialog(window, cx);
        if self.settings_open {
            self.close_settings(window, cx);
        }
        if self.switcher.is_some() {
            return;
        }
        self.switcher = Some(Switcher {
            previous_focus: window.focused(cx),
            entries: Vec::new(),
            selected: 0,
            scroll: ScrollHandle::new(),
        });
        self.switcher_input.update(cx, |input, cx| {
            input.set_value("", window, cx);
            input.focus(window, cx);
        });
        self.filter_switcher(cx);
    }

    fn filter_switcher(&mut self, cx: &mut Context<Self>) {
        let entries = switcher::ranked(
            &self.navigation_entries(cx),
            &self.switcher_input.read(cx).value(),
            &self.repository_view().recent_targets,
        );
        if let Some(switcher) = &mut self.switcher {
            switcher.entries = entries;
            switcher.selected = 0;
            switcher.scroll.scroll_to_item(0);
            cx.notify();
        }
    }

    fn close_switcher(&mut self, _: &CloseSwitcher, window: &mut Window, cx: &mut Context<Self>) {
        if let Some(switcher) = self.switcher.take() {
            if let Some(focus) = switcher.previous_focus {
                focus.focus(window, cx);
            } else {
                self.focus_handle.focus(window, cx);
            }
            cx.notify();
        }
    }

    fn move_switcher(&mut self, delta: isize, cx: &mut Context<Self>) {
        if let Some(switcher) = &mut self.switcher {
            if switcher.entries.is_empty() {
                return;
            }
            switcher.selected = (switcher.selected as isize + delta)
                .rem_euclid(switcher.entries.len() as isize)
                as usize;
            switcher.scroll.scroll_to_item(switcher.selected);
            cx.notify();
        }
    }

    fn switcher_next(&mut self, _: &SwitcherNext, _: &mut Window, cx: &mut Context<Self>) {
        self.move_switcher(1, cx);
    }

    fn switcher_previous(&mut self, _: &SwitcherPrevious, _: &mut Window, cx: &mut Context<Self>) {
        self.move_switcher(-1, cx);
    }

    fn accept_switcher(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let entry = self
            .switcher
            .as_ref()
            .and_then(|switcher| switcher.entries.get(switcher.selected))
            .cloned();
        let Some(entry) = entry else {
            return;
        };
        self.close_switcher(&CloseSwitcher, window, cx);
        self.open_destination(entry, window, cx);
    }

    fn open_destination(&mut self, entry: Entry, window: &mut Window, cx: &mut Context<Self>) {
        if matches!(entry.destination, Destination::Project) {
            self.remember_target(entry.key, cx);
            self.switch_project(entry.path, cx);
            return;
        }
        if self.cleanup_paths.contains(&entry.path) || self.base_update_paths.contains(&entry.path)
        {
            self.flash("A Git operation is in progress for this worktree", true, cx);
            return;
        }
        self.cleanup_results_open = false;
        if self.cleanup_preview.is_some() {
            self.close_cleanup_preview(window, cx);
        }
        self.close_rebase_dialog(window, cx);
        // A worktree can disappear between the search and Return.
        if !entry.path.is_dir() || !self.worktrees.iter().any(|info| info.path() == entry.path) {
            self.flash(
                "This worktree is no longer available. Refresh and try again.",
                true,
                cx,
            );
            return;
        }
        self.select_worktree(entry.path.clone(), cx);
        match entry.destination {
            Destination::Worktree | Destination::Project => {}
            Destination::Conversation(id) => {
                self.ensure_chats(&entry.path, window, cx);
                if let Some(ix) = self
                    .chats
                    .get(&entry.path)
                    .and_then(|chats| chats.iter().position(|chat| chat.read(cx).local_id() == id))
                {
                    self.active_chat.insert(entry.path, ix);
                    self.chat_open = true;
                    if let Some(chat) = self.active_chat_entity() {
                        chat.update(cx, |chat, cx| chat.focus_attention(window, cx));
                    }
                } else {
                    self.flash("This conversation is no longer available", true, cx);
                }
            }
            Destination::Transcript(session) => self.open_session_chat(session, window, cx),
        }
        self.remember_target(entry.key, cx);
        cx.notify();
    }

    fn render_switcher(&self, cx: &mut Context<Self>) -> AnyElement {
        let Some(switcher) = &self.switcher else {
            return div().into_any_element();
        };
        let theme = cx.theme().clone();
        let rows = switcher.entries.iter().enumerate().map(|(ix, entry)| {
            v_flex()
                .id(("switcher-entry", ix))
                .px_3()
                .py_2()
                .gap_1()
                .cursor_pointer()
                .bg(if ix == switcher.selected {
                    theme.list_active
                } else {
                    theme.background
                })
                .hover(|el| el.bg(theme.list_hover))
                .on_click(cx.listener(move |this, _, window, cx| {
                    if let Some(switcher) = &mut this.switcher {
                        switcher.selected = ix;
                    }
                    this.accept_switcher(window, cx);
                }))
                .child(div().truncate().child(entry.title.clone()))
                .child(
                    div()
                        .text_xs()
                        .truncate()
                        .text_color(theme.muted_foreground)
                        .child(entry.detail.clone()),
                )
        });
        div()
            .absolute()
            .inset_0()
            .flex()
            .justify_center()
            .items_start()
            .pt(px(64.))
            .bg(gpui_kit::black().opacity(0.35))
            .on_mouse_down(
                MouseButton::Left,
                cx.listener(|this, _, window, cx| {
                    this.close_switcher(&CloseSwitcher, window, cx);
                }),
            )
            .child(
                v_flex()
                    .key_context("Switcher")
                    .w(px(620.))
                    .max_w_full()
                    .h_auto()
                    .bg(theme.background)
                    .border_1()
                    .border_color(theme.border)
                    .rounded_lg()
                    .shadow_lg()
                    .on_mouse_down(MouseButton::Left, |_, _, cx| cx.stop_propagation())
                    .on_action(cx.listener(Self::close_switcher))
                    .on_action(cx.listener(Self::switcher_next))
                    .on_action(cx.listener(Self::switcher_previous))
                    .capture_key_down(cx.listener(|this, event: &KeyDownEvent, window, cx| {
                        match event.keystroke.key.as_str() {
                            "up" => this.move_switcher(-1, cx),
                            "down" => this.move_switcher(1, cx),
                            "escape" => this.close_switcher(&CloseSwitcher, window, cx),
                            "enter" => this.accept_switcher(window, cx),
                            _ => return,
                        }
                        cx.stop_propagation();
                    }))
                    .child(div().p_2().child(Input::new(&self.switcher_input)))
                    .child(
                        v_flex()
                            .id("switcher-results")
                            .max_h(px(400.))
                            .overflow_y_scroll()
                            .track_scroll(&switcher.scroll)
                            .children(rows)
                            .when(switcher.entries.is_empty(), |el| {
                                el.child(
                                    div()
                                        .p_4()
                                        .text_color(theme.muted_foreground)
                                        .child("No matching worktrees or conversations"),
                                )
                            }),
                    )
                    .child(
                        div()
                            .px_3()
                            .py_2()
                            .text_xs()
                            .text_color(theme.muted_foreground)
                            .child("↑ ↓ Navigate    Return Open    Esc Cancel"),
                    ),
            )
            .into_any_element()
    }

    fn open_settings(&mut self, _: &OpenSettings, window: &mut Window, cx: &mut Context<Self>) {
        self.cleanup_results_open = false;
        if self.cleanup_preview.is_some() {
            self.close_cleanup_preview(window, cx);
        }
        self.close_rebase_dialog(window, cx);
        self.settings_focus = window.focused(cx);
        self.settings_open = true;
        self.settings_handle.focus(window, cx);
        cx.notify();
    }

    fn close_settings(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        self.settings_open = false;
        if let Some(focus) = self.settings_focus.take() {
            focus.focus(window, cx);
        } else {
            window.blur(cx);
        }
        cx.notify();
    }

    fn toggle_notification(&mut self, approvals: bool, cx: &mut Context<Self>) {
        let mut settings = crate::settings::load();
        let enabled = if approvals {
            &mut settings.notify_approvals
        } else {
            &mut settings.notify_completions
        };
        if !*enabled && let Err(error) = notifications::request_permission(cx) {
            self.flash(error.to_string(), true, cx);
            return;
        }
        *enabled = !*enabled;
        crate::settings::save(&settings);
        cx.notify();
    }

    fn render_settings(&self, cx: &mut Context<Self>) -> AnyElement {
        let settings = crate::settings::load();
        let theme = cx.theme().clone();
        div().absolute().inset_0().flex().justify_center().items_start().pt(px(80.))
            .bg(gpui_kit::black().opacity(0.35))
            .on_mouse_down(MouseButton::Left,cx.listener(|this,_,window,cx| this.close_settings(window,cx)))
            .child(v_flex().id("settings-content").max_h(px(650.)).overflow_y_scroll().w(px(560.)).track_focus(&self.settings_handle).max_w_full().p_4().gap_3().rounded_lg().shadow_lg()
                .border_1().border_color(theme.border).bg(theme.background)
                .on_mouse_down(MouseButton::Left,|_,_,cx| cx.stop_propagation())
                .capture_key_down(cx.listener(|this,event: &KeyDownEvent,window,cx| {
                    if event.keystroke.key == "escape" { this.close_settings(window,cx); cx.stop_propagation(); }
                }))
                .child(h_flex().justify_between().child(div().text_lg().font_weight(FontWeight::BOLD).child("Settings"))
                    .child(Button::new("close-settings").label("Done").small().ghost()
                        .on_click(cx.listener(|this,_,window,cx| this.close_settings(window,cx)))))
                .child(div().font_weight(FontWeight::SEMIBOLD).child("Background notifications"))
                .children([(true,"Approvals and questions",settings.notify_approvals),(false,"Completion and failures",settings.notify_completions)]
                    .into_iter().map(|(approval,label,enabled)| h_flex().justify_between().child(label)
                        .child(Button::new(if approval {"notify-approval"} else {"notify-completion"})
                            .label(if enabled {"On"} else {"Off"}).small().ghost()
                            .on_click(cx.listener(move |this,_,_,cx| this.toggle_notification(approval,cx))))))
                .child(div().text_sm().text_color(theme.muted_foreground).child(
                    if cfg!(target_os = "macos") {
                        "Only exact events from native conversations are notified. The visible conversation stays quiet. If macOS blocks alerts, enable SuperGIT in System Settings → Notifications."
                    } else {
                        "Only exact events from native conversations are notified. The visible conversation stays quiet. Alerts go to your desktop's notification server; Do Not Disturb hides them."
                    }))
                .child(self.render_diff_settings(cx))
                .child(self.render_cleanup_settings(cx)))
            .into_any_element()
    }

    // ---- data flow -------------------------------------------------------

    /// (Re)start the refresh loop. Restarting refreshes immediately, PRs included.
    fn start_polling(&mut self, cx: &mut Context<Self>) {
        self.start_base_polling(cx);
        if self.repo.is_none() {
            self.poll_task = None;
            self.prs_task = None;
            return;
        }
        self.load_avatars(cx);
        self.poll_task = Some(cx.spawn(async move |this, cx| {
            loop {
                let Ok(Some((repo, index))) = this.read_with(cx, |this, _| {
                    this.repo.clone().map(|r| (r, this.index.clone()))
                }) else {
                    return;
                };
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
                cx.background_executor().timer(POLL_INTERVAL).await;
            }
        }));
        self.start_pr_polling(cx);
    }

    /// (Re)start the pull request refresh loop; it fetches right away.
    fn start_pr_polling(&mut self, cx: &mut Context<Self>) {
        self.prs_task = Some(cx.spawn(async move |this, cx| {
            loop {
                let Ok(Some(root)) =
                    this.read_with(cx, |this, _| this.repo.as_ref().map(|r| r.root.clone()))
                else {
                    return;
                };
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
                cx.background_executor().timer(PR_POLL_INTERVAL).await;
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
                        .filter(|w| {
                            !w.wt.is_bare
                                && w.wt.prunable.is_none()
                                && w.status.is_ok()
                                && !self.cleanup_paths.contains(w.path())
                        })
                        .filter(|w| self.launch_dir.starts_with(&w.wt.path))
                        .max_by_key(|w| w.wt.path.as_os_str().len())
                        .or_else(|| {
                            self.worktrees.iter().find(|w| {
                                !w.wt.is_bare
                                    && w.wt.prunable.is_none()
                                    && w.status.is_ok()
                                    && !self.cleanup_paths.contains(w.path())
                            })
                        })
                        .map(|w| w.wt.path.clone())
                });
        }
        if self.selected != previous {
            self.reset_details(cx);
            self.restore_review(cx);
            self.queue_persist(cx);
        }
        self.reload_details(cx);
        self.reload_history(cx);
        if let Some(target) = self.pending_notification.take() {
            let this = cx.entity();
            if let Some(window) = cx.windows().first().copied() {
                cx.defer(move |cx| {
                    let _ = window.update(cx, |_, window, cx| {
                        this.update(cx, |this, cx| this.open_notification(target, window, cx));
                    });
                });
            }
        }
        #[cfg(feature = "screenshot")]
        self.debug_chat_once(cx);
        #[cfg(feature = "screenshot")]
        self.debug_rebase_once(cx);
        #[cfg(feature = "screenshot")]
        self.debug_commit_once(cx);
        cx.notify();
    }

    /// Screenshot builds only: `SUPERGIT_DEBUG_COMMIT=<sha prefix>` selects
    /// that History commit two seconds after startup, as a click would.
    #[cfg(feature = "screenshot")]
    fn debug_commit_once(&mut self, cx: &mut Context<Self>) {
        let Ok(prefix) = std::env::var("SUPERGIT_DEBUG_COMMIT") else {
            return;
        };
        let Some(sha) = self
            .history
            .commits
            .value()
            .and_then(|commits| commits.iter().find(|c| c.sha.starts_with(&prefix)))
            .map(|c| c.sha.clone())
        else {
            return;
        };
        if std::env::var_os("SUPERGIT_DEBUG_COMMIT_DONE").is_some() {
            return;
        }
        // SAFETY: isolated screenshot builds only, on the main thread.
        unsafe { std::env::set_var("SUPERGIT_DEBUG_COMMIT_DONE", "1") };
        cx.spawn(async move |this, cx| {
            cx.background_executor().timer(Duration::from_secs(2)).await;
            let _ = this.update(cx, |this, cx| this.select_commit(sha, cx));
        })
        .detach();
    }

    /// Screenshot builds only: `SUPERGIT_DEBUG_CHAT=resume` opens the latest
    /// session in the panel, `SUPERGIT_DEBUG_CHAT=prompt:<text>` sends a turn.
    #[cfg(feature = "screenshot")]
    fn debug_chat_once(&mut self, cx: &mut Context<Self>) {
        if let Ok(query) = std::env::var("SUPERGIT_DEBUG_SWITCHER")
            && std::env::var_os("SUPERGIT_DEBUG_SWITCHER_DONE").is_none()
        {
            // SAFETY: isolated screenshot builds only, on the main thread.
            unsafe { std::env::set_var("SUPERGIT_DEBUG_SWITCHER_DONE", "1") };
            if let Some(window) = cx.windows().first().copied() {
                let this = cx.entity();
                cx.defer(move |cx| {
                    let _ = window.update(cx, |_, window, cx| {
                        this.update(cx, |this, cx| {
                            this.open_switcher(&OpenSwitcher, window, cx);
                            this.switcher_input
                                .update(cx, |input, cx| input.set_value(query, window, cx));
                            this.filter_switcher(cx);
                        });
                    });
                });
            }
        }
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
                    // `SUPERGIT_DEBUG_AGENT=codex|claude` picks the agent.
                    let agent = std::env::var("SUPERGIT_DEBUG_AGENT")
                        .ok()
                        .and_then(|a| Provider::parse(&a));
                    if spec == "resume" {
                        if let Some(s) = app.selected_info().and_then(|i| {
                            i.sessions
                                .iter()
                                .find(|s| agent.is_none_or(|a| s.provider == a))
                                .cloned()
                        }) {
                            app.open_session_chat(s, window, cx);
                        }
                    } else if let Some(prompt) = spec.strip_prefix("prompt:") {
                        app.new_chat_with(agent.unwrap_or_else(default_provider), window, cx);
                        // `SUPERGIT_DEBUG_POLICY=read-only|auto|full-access` (Codex).
                        let policy = std::env::var("SUPERGIT_DEBUG_POLICY")
                            .ok()
                            .and_then(|p| sg_codex::Policy::parse(&p));
                        if let Some(chat) = app.active_chat_entity() {
                            chat.update(cx, |chat, cx| {
                                if let Some(policy) = policy {
                                    chat.set_policy(policy, cx);
                                }
                                chat.send_text(prompt, cx)
                            });
                        }
                    }
                })
            });
        });
    }

    fn apply_prs(&mut self, result: Result<Vec<PullRequest>, String>, cx: &mut Context<Self>) {
        let was_loaded = self.prs_loaded;
        self.prs_loaded = true;
        match result {
            Ok(prs) => {
                self.prs_fetched_at = Some(std::time::Instant::now());
                self.prs = prs;
                self.prs_error = None;
                self.observe_cleanup_prs(cx);
            }
            Err(e) => self.prs_error = Some(e),
        }
        if !was_loaded
            && self
                .selected_info()
                .is_some_and(|info| self.is_completed(info, cx))
        {
            self.set_completed_collapsed(false, cx);
        }
        #[cfg(feature = "screenshot")]
        if std::env::var_os("SUPERGIT_DEBUG_CLEANUP").is_some()
            && std::env::var_os("SUPERGIT_DEBUG_CLEANUP_DONE").is_none()
        {
            let paths: Vec<_> = self
                .worktrees
                .iter()
                .filter(|info| self.is_completed(info, cx))
                .map(|info| info.wt.path.clone())
                .collect();
            if !paths.is_empty() {
                // SAFETY: isolated screenshot builds only, on the main thread.
                unsafe { std::env::set_var("SUPERGIT_DEBUG_CLEANUP_DONE", "1") };
                let owner = cx.entity();
                if let Some(window) = cx.windows().first().copied() {
                    cx.defer(move |cx| {
                        let _ = window.update(cx, |_, window, cx| {
                            owner.update(cx, |this, cx| {
                                this.open_cleanup_preview(paths, window, cx)
                            });
                        });
                    });
                }
            }
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
        let base = self.comparison_base();
        let scope = self.change_scope();
        self.details_generation += 1;
        let generation = self.details_generation;
        self.details_task = Some(cx.spawn(async move |this, cx| {
            let details = cx
                .background_executor()
                .spawn(async move { model::load_details(&path, status, base, scope) })
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
        if self.selected.as_ref() != Some(&details.path)
            || self.comparison_base() != details.base
            || self.change_scope() != details.scope
        {
            return;
        }
        let mut patch_changed = false;
        if let LoadState::Ready(patch) = &details.patch
            && (self.details.patch.value() != Some(patch) || details.path != self.details.path)
        {
            match crate::syntax::parse_patch(patch) {
                Ok(files) => {
                    self.changes_diff
                        .update(cx, |state, cx| state.set_files(files, cx));
                    self.restore_position(false, cx);
                    patch_changed = true;
                }
                Err(error) => {
                    details.patch =
                        LoadState::from_result(Err(format!("Cannot display diff: {error}")));
                }
            }
        }
        self.details.update(details);
        if patch_changed {
            self.refresh_difft(FileList::Changes, cx);
        }
        #[cfg(feature = "screenshot")]
        if std::env::var_os("SUPERGIT_DEBUG_CONTEXT").is_some()
            && std::env::var_os("SUPERGIT_DEBUG_CONTEXT_DONE").is_none()
            && self.details.patch.value().is_some()
        {
            // SAFETY: isolated screenshot builds only, on the main thread.
            unsafe { std::env::set_var("SUPERGIT_DEBUG_CONTEXT_DONE", "1") };
            if let Some(window) = cx.windows().first().copied() {
                let owner = cx.entity();
                cx.defer(move |cx| {
                    let _ = window.update(cx, |_, window, cx| {
                        owner.update(cx, |this, cx| this.ask_agent(&AskAgent, window, cx));
                    });
                });
            }
        }
        cx.notify();
    }

    fn reload_history(&mut self, cx: &mut Context<Self>) {
        self.load_history(self.history.limit(), cx);
    }

    fn load_history(&mut self, limit: usize, cx: &mut Context<Self>) {
        if self.tab != MainTab::History || self.history_task.is_some() {
            return;
        }
        let Some(path) = self.selected.clone() else {
            return;
        };
        let source = self.repo.as_ref().and_then(|repo| {
            let status = self.selected_info()?.status.as_ref().ok()?;
            Some(model::HistorySource::new(&repo.common_dir, status))
        });
        let previous = self.history.clone();
        self.history_generation += 1;
        let generation = self.history_generation;
        self.history_task = Some(cx.spawn(async move |this, cx| {
            let selected = path.clone();
            let history = cx
                .background_executor()
                .spawn(async move { previous.load(&path, source.as_ref(), limit) })
                .await;
            let _ = this.update(cx, |this, cx| {
                if generation == this.history_generation
                    && this.selected.as_ref() == Some(&selected)
                {
                    this.history_task = None;
                    this.apply_history(history, cx);
                }
            });
        }));
    }

    fn apply_history(&mut self, history: History, cx: &mut Context<Self>) {
        // A failed history refresh must not clear the current review selection.
        if let LoadState::Ready(commits) = &history.commits
            && let Some(sha) = &self.selected_commit
            && !commits.iter().any(|c| &c.sha == sha)
        {
            self.selected_commit = None;
            self.commit_task = None;
            self.commit_generation += 1;
            self.commit_patch = LoadState::Loading;
            self.commit_submodules.clear();
            self.commit_diff
                .update(cx, |state, cx| state.set_files(Vec::new(), cx));
            self.commit_difft.update(cx, |view, cx| view.clear(cx));
        }
        self.history.update(history);
        if self.tab == MainTab::History
            && self.commit_task.is_none()
            && self.commit_patch.is_loading()
            && let Some(sha) = self.selected_commit.clone().or_else(|| {
                self.history
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

    fn open_history_match(&mut self, cx: &mut Context<Self>) {
        let terms = model::history::search_terms(&self.history_search.read(cx).value());
        let Some(sha) = self.history.commits.value().and_then(|commits| {
            commits
                .iter()
                .find(|commit| model::history::matches_commit(commit, &terms))
                .map(|commit| commit.sha.clone())
        }) else {
            return;
        };
        self.select_commit(sha, cx);
    }

    fn reset_details(&mut self, cx: &mut Context<Self>) {
        self.reset_edit_selection();
        self.details_generation += 1;
        self.commit_generation += 1;
        self.history_generation += 1;
        self.details_task = None;
        self.history_task = None;
        self.commit_task = None;
        self.details = Details::default();
        self.history = History::default();
        self.selected_commit = None;
        self.commit_patch = LoadState::Loading;
        self.commit_submodules.clear();
        self.changes_diff = new_diff_state(self.diff_layout, cx);
        self.commit_diff = new_diff_state(self.diff_layout, cx);
        let mode = diff_mode(self.diff_layout);
        self.changes_difft = cx.new(|_| DifftView::new(mode));
        self.commit_difft = cx.new(|_| DifftView::new(mode));
    }

    fn select_worktree(&mut self, path: PathBuf, cx: &mut Context<Self>) {
        if self.selected.as_ref() != Some(&path) {
            self.reset_edit_selection();
        }
        if self.cleanup_paths.contains(&path) || self.base_update_paths.contains(&path) {
            self.flash("A Git operation is in progress for this worktree", true, cx);
            return;
        }
        if self
            .worktrees
            .iter()
            .find(|info| info.path() == path)
            .is_some_and(|info| self.is_completed(info, cx))
        {
            self.set_completed_collapsed(false, cx);
        }
        self.remember_target(format!("worktree:{}", path.display()), cx);
        if self.selected.as_ref() == Some(&path) {
            return;
        }
        self.cache_review(cx);
        self.selected = Some(path);
        self.reset_details(cx);
        self.restore_review(cx);
        // A restored review may predate turning difftastic on.
        self.refresh_difft(FileList::Changes, cx);
        self.refresh_difft(FileList::Commit, cx);
        self.reload_details(cx);
        self.reload_history(cx);
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
            self.commit_submodules.clear();
            self.commit_diff
                .update(cx, |state, cx| state.set_files(Vec::new(), cx));
            self.commit_difft.update(cx, |view, cx| view.clear(cx));
        }
        self.selected_commit = Some(sha.clone());
        self.commit_generation += 1;
        let generation = self.commit_generation;
        self.commit_task = Some(cx.spawn(async move |this, cx| {
            let request_path = path.clone();
            let request_sha = sha.clone();
            let (patch, details) = cx
                .background_executor()
                .spawn(async move {
                    let patch = sg_git::commit_patch(&request_path, &request_sha)
                        .map_err(model::git_error_message);
                    // The card is extra: without it the diff still shows.
                    let details = sg_git::commit_details(&request_path, &request_sha).ok();
                    (patch, details)
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
                    let files = crate::syntax::parse_patch(&patch.text)
                        .map_err(|e| format!("Cannot display commit diff: {e}"))?;
                    this.commit_diff
                        .update(cx, |state, cx| state.set_files(files, cx));
                    this.commit_submodules = patch.submodules;
                    Ok(patch.text)
                });
                this.commit_patch.update(LoadState::from_result(patch));
                this.commit_details = details;
                this.commit_task = None;
                this.refresh_difft(FileList::Commit, cx);
                this.restore_position(true, cx);
                this.queue_persist(cx);
                cx.notify();
            })
            .ok();
        }));
        cx.notify();
    }

    fn flash(&mut self, message: impl Into<String>, is_error: bool, cx: &mut Context<Self>) {
        let message = message
            .into()
            .split_whitespace()
            .collect::<Vec<_>>()
            .join(" ");
        self.message = Some((message, is_error));
        cx.notify();
    }

    fn report(&mut self, result: anyhow::Result<()>, ok: &str, cx: &mut Context<Self>) {
        match result {
            Ok(()) => self.flash(ok, false, cx),
            Err(e) => self.flash(format!("{e:#}"), true, cx),
        }
    }

    /// Open the repository containing `path` (from the dialog, a recent entry,
    /// the sidebar, a folder dropped on the Dock icon or `open -a
    /// SuperGIT <dir>`) and add it to the sidebar's projects. The previous
    /// project keeps its conversations running. Returns whether it opened.
    pub fn open_path(&mut self, path: PathBuf, cx: &mut Context<Self>) -> bool {
        let repo = match Repo::discover(&path) {
            Ok(repo) => repo,
            Err(e) => {
                self.flash(format!("{e:#}"), true, cx);
                return false;
            }
        };
        let same = self.repo.as_ref().is_some_and(|r| r.root == repo.root);
        if !same && self.git_operation_running() {
            self.flash(
                "Wait for the running Git operation before switching projects",
                true,
                cx,
            );
            return false;
        }
        self.persist_workspace(cx);
        self.cache_review(cx);
        crate::recent::remember(&repo.root);
        self.recent = crate::recent::load();
        self.persisted.projects.add(&repo.root);
        self.launch_dir = path;
        if !same {
            self.stash_project();
            self.repo = Some(repo);
            self.reset_view(cx);
            self.chat_layout = cx.new(|_| ResizableState::default());
            self.changes_layout = cx.new(|_| ResizableState::default());
            self.history_layout = cx.new(|_| ResizableState::default());
            self.commit_files_layout = cx.new(|_| ResizableState::default());
            self.chat_open = self.repository_view().chat_open;
            self.restore_project(cx);
        } else {
            self.selected = None; // re-pick the worktree containing `path`
        }
        self.message = None;
        self.start_polling(cx);
        self.queue_persist(cx);
        cx.notify();
        true
    }

    /// GitHub avatars for the commit list, once per repository.
    fn load_avatars(&mut self, cx: &mut Context<Self>) {
        let Some(root) = self.repo.as_ref().map(|repo| repo.root.clone()) else {
            return;
        };
        if self.avatars_root.as_ref() == Some(&root) {
            return;
        }
        self.avatars_root = Some(root.clone());
        self.avatars.clear();
        self.avatars_task = Some(cx.spawn(async move |this, cx| {
            let request = root.clone();
            let avatars = cx
                .background_executor()
                .spawn(async move { avatars::load(&request) })
                .await;
            this.update(cx, |this, cx| {
                if this.avatars_root.as_ref() == Some(&root) {
                    this.avatars = avatars;
                    this.avatars_task = None;
                    cx.notify();
                }
            })
            .ok();
        }));
    }

    fn reset_view(&mut self, cx: &mut Context<Self>) {
        self.worktree_dialog = None;
        self.creation_base = LoadState::Loading;
        self.base_update_error = None;
        self.sync_dialog = None;
        self.observed_open_prs.clear();
        self.cleanup_attempted.clear();
        self.cleanup_results.clear();
        self.cleanup_results_open = false;
        self.cleanup_preview = None;
        self.rebase_dialog = None;
        self.stash_dialog = None;
        self.prs_fetched_at = None;
        self.worktrees.clear();
        self.load_error = None;
        self.prs.clear();
        self.prs_error = None;
        self.prs_loaded = false;
        self.pulls.reset(cx);
        self.staged_commit = None;
        self.selected = None;
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
        self.leave_project(cx);
    }

    fn leave_project(&mut self, cx: &mut Context<Self>) {
        if self.git_operation_running() {
            self.flash("Wait for the running Git operation first", true, cx);
            return;
        }
        self.persist_workspace(cx);
        self.cache_review(cx);
        self.stash_project();
        self.repo = None;
        self.poll_task = None;
        self.prs_task = None;
        self.reset_view(cx);
        self.recent = crate::recent::load();
        cx.notify();
    }

    // ---- agent panel -------------------------------------------------------

    fn watch_chat(&mut self, path: PathBuf, chat: &Entity<ChatSession>, cx: &mut Context<Self>) {
        let Some(repository) = self.repo.as_ref().map(|repo| repo.root.clone()) else {
            return;
        };
        self.chat_projects.insert(path.clone(), repository.clone());
        let key = format!(
            "{}:{}:{}",
            repository.display(),
            path.display(),
            chat.read(cx).local_id()
        );
        self.notified
            .insert(key.clone(), chat.read(cx).attention().revision);
        cx.observe(chat, move |this, chat, cx| {
            let state = chat.read(cx).attention().clone();
            let local_id = chat.read(cx).local_id().to_string();
            let provider = chat.read(cx).provider();
            let revision_changed =
                this.notified.insert(key.clone(), state.revision) != Some(state.revision);
            let focused = this
                .repo
                .as_ref()
                .is_some_and(|repo| repo.root == repository)
                && this.selected.as_ref() == Some(&path)
                && this.chat_open
                && !this.settings_open
                && !this.cleanup_results_open
                && this.switcher.is_none()
                && this.cleanup_preview.is_none()
                && this.rebase_dialog.is_none()
                && this
                    .active_chat_entity()
                    .is_some_and(|active| active == chat)
                && cx.windows().first().is_some_and(|window| {
                    window
                        .update(cx, |_, window, _| window.is_window_active())
                        .unwrap_or(false)
                });
            if focused && chat.read(cx).follows_tail(cx) {
                chat.update(cx, |chat, cx| chat.mark_seen(cx));
            }
            if revision_changed {
                let settings = crate::settings::load();
                if state.should_notify(
                    focused,
                    settings.notify_approvals,
                    settings.notify_completions,
                ) {
                    notifications::deliver(
                        &notifications::Target {
                            repository: repository.clone(),
                            worktree: path.clone(),
                            provider: provider.as_str().into(),
                            conversation: local_id,
                        },
                        state.revision,
                        &match state.kind {
                            Kind::Approval => format!("{} needs your approval", provider.label()),
                            Kind::Question => format!("{} has a question", provider.label()),
                            Kind::Failed => format!("{} task failed", provider.label()),
                            _ => format!("{} task completed", provider.label()),
                        },
                        &chat.read(cx).title(),
                        cx,
                    );
                }
            }
            this.queue_persist(cx);
            cx.notify();
        })
        .detach();
    }

    fn attention_entries(&self, kind: Kind, cx: &App) -> Vec<Entry> {
        self.navigation_entries(cx)
            .into_iter()
            .filter(|entry| {
                let Destination::Conversation(id) = &entry.destination else {
                    return false;
                };
                let state = self
                    .chats
                    .get(&entry.path)
                    .and_then(|chats| chats.iter().find(|chat| chat.read(cx).local_id() == id))
                    .map(|chat| chat.read(cx).attention())
                    .or_else(|| {
                        self.repository_view()
                            .conversations
                            .get(&entry.path)
                            .and_then(|chats| chats.iter().find(|chat| chat.local_id == *id))
                            .map(|chat| &chat.attention)
                    });
                state.is_some_and(|state| state.kind == kind && (kind.is_live() || state.unread))
            })
            .collect()
    }

    fn attention_count(&self, kind: Kind, cx: &App) -> usize {
        self.worktrees
            .iter()
            .map(|info| {
                if let Some(chats) = self.chats.get(info.path()) {
                    chats
                        .iter()
                        .filter(|chat| {
                            let state = chat.read(cx).attention();
                            state.kind == kind && (kind.is_live() || state.unread)
                        })
                        .count()
                } else {
                    self.repository_view()
                        .conversations
                        .get(info.path())
                        .into_iter()
                        .flatten()
                        .filter(|chat| native_provider(&chat.provider))
                        .filter(|chat| {
                            chat.attention.kind == kind && (kind.is_live() || chat.attention.unread)
                        })
                        .count()
                }
            })
            .sum()
    }

    fn open_attention(&mut self, kind: Kind, window: &mut Window, cx: &mut Context<Self>) {
        let entries = self.attention_entries(kind, cx);
        if entries.is_empty() {
            return;
        }
        let cursor = self.attention_cursor.entry(kind).or_default();
        let entry = entries[*cursor % entries.len()].clone();
        *cursor += 1;
        self.open_destination(entry, window, cx);
    }

    pub fn open_notification(
        &mut self,
        target: notifications::Target,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        if !native_provider(&target.provider) {
            self.flash(
                "This provider does not support notification navigation yet",
                true,
                cx,
            );
            return;
        }
        if self
            .repo
            .as_ref()
            .is_none_or(|repo| repo.root != target.repository)
        {
            // A cached project restores its worktrees at once and takes the
            // pending destination right away; otherwise the first poll does.
            let repository = target.repository.clone();
            self.pending_notification = Some(target);
            if !self.open_path(repository, cx) {
                self.pending_notification = None;
            }
            return;
        }
        if self.worktrees.is_empty() {
            self.pending_notification = Some(target);
            return;
        }
        self.open_destination(
            Entry {
                key: format!("chat:{}:{}", target.worktree.display(), target.conversation),
                path: target.worktree,
                title: String::new(),
                detail: String::new(),
                search: String::new(),
                destination: Destination::Conversation(target.conversation),
            },
            window,
            cx,
        );
        window.activate_window();
        cx.activate(true);
    }

    fn add_chat(
        &mut self,
        path: PathBuf,
        chat: Entity<ChatSession>,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        chat.update(cx, |chat, cx| {
            chat.set_git_busy(self.base_update_paths.contains(&path), cx)
        });
        // Re-render (sidebar dots, title counts) whenever a conversation changes.
        self.watch_chat(path.clone(), &chat, cx);
        let list = self.chats.entry(path.clone()).or_default();
        list.push(chat.clone());
        self.active_chat.insert(path, list.len() - 1);
        self.chat_open = true;
        self.queue_persist(cx);
        let input = chat.read(cx).input();
        input.update(cx, |state, cx| state.focus(window, cx));
        cx.notify();
    }

    /// A new conversation for the selected worktree, with the agent picked
    /// last (switchable until the first message).
    pub fn new_chat(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        self.new_chat_with(default_provider(), window, cx);
    }

    pub fn new_chat_with(
        &mut self,
        provider: Provider,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let Some(path) = self.selected.clone() else {
            return;
        };
        if self.cleanup_paths.contains(&path) || self.base_update_paths.contains(&path) {
            return;
        }
        // Reuse an untouched empty conversation instead of piling up tabs.
        if let Some(ix) = self.chats.get(&path).and_then(|list| {
            list.iter().position(|c| {
                let c = c.read(cx);
                c.is_fresh() && !c.has_draft(cx)
            })
        }) {
            self.active_chat.insert(path.clone(), ix);
            self.chat_open = true;
            let chat = self.chats[&path][ix].clone();
            chat.update(cx, |chat, cx| chat.set_provider(provider, window, cx));
            let input = chat.read(cx).input();
            input.update(cx, |state, cx| state.focus(window, cx));
            cx.notify();
            return;
        }
        let chat = cx.new(|cx| ChatSession::new(path.clone(), provider, window, cx));
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

    /// Open an existing agent session in the panel (history + resume).
    fn open_session_chat(&mut self, session: Session, window: &mut Window, cx: &mut Context<Self>) {
        let Some(path) = self.selected.clone() else {
            return;
        };
        if let Some(ix) = self.chats.get(&path).and_then(|list| {
            list.iter().position(|c| {
                let c = c.read(cx);
                c.provider() == session.provider && c.session_id() == Some(session.id.as_str())
            })
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
            // Dropping the session kills its agent process.
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
                sg_agents::chat::Status::NeedsPermission | sg_agents::chat::Status::NeedsInput => {
                    Some(AgentState::NeedsAttention)
                }
                sg_agents::chat::Status::Working | sg_agents::chat::Status::Starting => {
                    Some(AgentState::Working)
                }
                _ => None,
            })
            .min()
    }

    /// Estimates apply only to transcripts not owned by the native UI.
    fn external_agent_state(&self, info: &WorktreeInfo, cx: &App) -> Option<AgentState> {
        self.external_agent(info, cx).map(|(state, _)| state)
    }

    /// The most urgent estimated state of sessions running outside SuperGIT,
    /// with the agent behind it.
    fn external_agent(&self, info: &WorktreeInfo, cx: &App) -> Option<(AgentState, Provider)> {
        let ids: std::collections::HashSet<(Provider, String)> = self
            .chats
            .get(info.path())
            .into_iter()
            .flatten()
            .filter_map(|chat| {
                let chat = chat.read(cx);
                Some((chat.provider(), chat.session_id()?.to_string()))
            })
            .chain(
                self.repository_view()
                    .conversations
                    .get(info.path())
                    .into_iter()
                    .flatten()
                    .filter_map(|chat| {
                        Some((Provider::parse(&chat.provider)?, chat.session_id.clone()?))
                    }),
            )
            .collect();
        info.sessions
            .iter()
            .filter(|session| !ids.contains(&(session.provider, session.id.clone())))
            .filter(|session| session.state.is_active())
            .map(|session| (session.state, session.provider))
            .min()
    }

    /// The most urgent attention among a worktree's conversations in a
    /// project (`view` is its saved view), with the agent and local id of the
    /// first conversation that has it.
    fn native_attention_in(
        &self,
        view: &RepositoryView,
        path: &Path,
        cx: &App,
    ) -> Option<(Kind, Provider, String)> {
        let states: Vec<(&crate::attention::Attention, Provider, &str)> =
            if let Some(chats) = self.chats.get(path) {
                chats
                    .iter()
                    .map(|chat| {
                        let chat = chat.read(cx);
                        (chat.attention(), chat.provider(), chat.local_id())
                    })
                    .collect()
            } else {
                view.conversations
                    .get(path)
                    .into_iter()
                    .flatten()
                    .filter_map(|chat| {
                        Some((
                            &chat.attention,
                            Provider::parse(&chat.provider)?,
                            chat.local_id.as_str(),
                        ))
                    })
                    .collect()
            };
        projects::ATTENTION_ORDER.into_iter().find_map(|kind| {
            states.iter().find_map(|(state, provider, id)| {
                (state.kind == kind && (kind.is_live() || state.unread))
                    .then(|| (kind, *provider, (*id).to_string()))
            })
        })
    }

    /// Exact native activity and explicitly labeled external estimates.
    fn agent_state_for(&self, info: &WorktreeInfo, cx: &App) -> Option<AgentState> {
        [
            self.external_agent_state(info, cx),
            self.live_agent_state(info.path(), cx),
        ]
        .into_iter()
        .flatten()
        .min()
    }

    pub fn refresh(&mut self, _: &Refresh, _: &mut Window, cx: &mut Context<Self>) {
        self.flash("Refreshing…", false, cx);
        self.history.invalidate();
        for review in self.review_views.values_mut() {
            review.history.invalidate();
        }
        self.history_generation += 1;
        self.history_task = None;
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
        if self.action_task.is_some() || self.cleanup_task.is_some() {
            self.flash("Another Git operation is in progress", true, cx);
            return;
        }
        let root = self.repo.as_ref().map(|repo| repo.root.clone());
        self.flash(format!("{label}…"), false, cx);
        self.action_task = Some(cx.spawn(async move |this, cx| {
            let result = cx.background_executor().spawn(async move { op() }).await;
            this.update(cx, |this, cx| {
                this.action_task = None;
                match result {
                    Ok(()) => {
                        this.flash(format!("{label} — done"), false, cx);
                        if let Some(path) = select_after
                            && this.repo.as_ref().map(|repo| &repo.root) == root.as_ref()
                        {
                            this.select_worktree(path, cx);
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
        let base = self
            .creation_base
            .value()
            .filter(|_| self.creation_base.error().is_none())
            .map(|info| info.creation_ref().map_err(|error| error.to_string()))
            .unwrap_or_else(|| {
                Err("Base is unavailable. Refresh or choose a local branch first".into())
            });
        self.branch_input
            .update(cx, |state, cx| state.set_value("", window, cx));
        self.run_git_action(
            format!("Creating worktree {branch}"),
            Some(path),
            move || {
                if let Some(parent) = target.parent() {
                    std::fs::create_dir_all(parent)?;
                }
                // Existing branches keep their own history; new ones use the visible base.
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
                    let base = base.map_err(anyhow::Error::msg)?;
                    sg_git::add_worktree(&root, &target, Some(&branch), Some(&base))
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

    // ---- rendering -------------------------------------------------------

    fn render_title_bar(&self, cx: &mut Context<Self>) -> impl IntoElement {
        let menu_bar = crate::menu_bar(cx);
        let indicators = projects::ATTENTION_ORDER.into_iter().filter_map(|kind| {
            let count = self.attention_count(kind, cx);
            (count > 0).then(|| {
                Button::new(match kind {
                    Kind::Approval => "attention-approval",
                    Kind::Question => "attention-question",
                    Kind::Failed => "attention-failed",
                    Kind::Completed => "attention-completed",
                    _ => "attention-running",
                })
                .label(format!("{count} {}", kind.label()))
                .text_color(attention_color(kind, cx.theme()))
                .xsmall()
                .ghost()
                .tooltip(format!(
                    "{} · Open the exact conversation; repeat to cycle",
                    if kind == Kind::Completed {
                        "Completed and unread"
                    } else {
                        kind.label()
                    }
                ))
                .on_click(
                    cx.listener(move |this, _, window, cx| this.open_attention(kind, window, cx)),
                )
            })
        });
        let theme = cx.theme();
        TitleBar::new().child(
            h_flex()
                .w_full()
                .pr_2()
                .justify_between()
                .child(
                    h_flex()
                        .gap_2()
                        .when_some(menu_bar, |el, bar| el.child(div().h_full().child(bar)))
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
                        .child(
                            Button::new("open-switcher")
                                .label("Switch…")
                                .small()
                                .ghost()
                                .tooltip(format!(
                                    "Switch worktree, conversation or project ({MOD}-p)"
                                ))
                                .on_click(cx.listener(|this, _, window, cx| {
                                    this.open_switcher(&OpenSwitcher, window, cx)
                                })),
                        )
                        .children(indicators)
                        .child(
                            Button::new("open-repo")
                                .label("Open…")
                                .ghost()
                                .small()
                                .tooltip(format!("Open a repository as a project ({MOD}-o)"))
                                .on_click(cx.listener(|this, _, window, cx| {
                                    this.open_repository(&OpenRepository, window, cx)
                                })),
                        )
                        .when(self.repo.is_some(), |el| {
                            el.child(
                                Button::new("clone-repo")
                                    .label("Clone…")
                                    .ghost()
                                    .small()
                                    .on_click(cx.listener(|this, _, window, cx| {
                                        this.open_clone_form(window, cx)
                                    })),
                            )
                        })
                        .when(self.repo.is_some(), |el| {
                            el.child(
                                Button::new("close-repo")
                                    .label("Recent")
                                    .ghost()
                                    .small()
                                    .tooltip(format!(
                                        "Switch to a recent repository ({MOD}-shift-o)"
                                    ))
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
                                .tooltip(format!("Refresh now ({MOD}-r)"))
                                .on_click(cx.listener(|this, _, window, cx| {
                                    this.refresh(&Refresh, window, cx)
                                })),
                        ),
                ),
        )
    }

    fn render_main(&mut self, window: &mut Window, cx: &mut Context<Self>) -> AnyElement {
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
        let changes = if self.comparison_base().is_some() {
            self.details
                .files
                .value()
                .map(|files| files.len().to_string())
                .unwrap_or_else(|| "—".into())
        } else {
            info.status
                .as_ref()
                .map(|status| status.files.len().to_string())
                .unwrap_or_else(|_| "—".into())
        };
        let sessions = info.sessions.len();
        // Commits since the branch forked from main; none on main itself.
        let history = match &self.history.fork {
            Some(fork) if self.tab == MainTab::History => {
                format!("History · {}", fork.commits)
            }
            _ => "History".to_string(),
        };
        let open_prs = self
            .prs
            .iter()
            .filter(|p| matches!(p.state, PrState::Open | PrState::Draft))
            .count();

        let (_, main_width) = self.pane_widths(window, cx);
        let content = match self.tab {
            MainTab::Changes => {
                #[cfg(feature = "screenshot")]
                self.debug_staging_once(cx);
                let switch = self.render_staging_switch(cx).into_any_element();
                let body = if self.git_editor.staging {
                    self.render_index_editor(&info, window, cx)
                } else {
                    v_flex()
                        .size_full()
                        .child(self.render_comparison(window, cx))
                        .child(
                            div()
                                .flex_1()
                                .min_h_0()
                                .child(self.render_changes(&info, main_width, &theme, cx)),
                        )
                        .into_any_element()
                };
                v_flex()
                    .size_full()
                    .child(switch)
                    .child(div().flex_1().min_h_0().child(body))
                    .into_any_element()
            }
            MainTab::History => self
                .render_history(main_width, &theme, cx)
                .into_any_element(),
            MainTab::Sessions => self.render_sessions(&info, &theme, cx).into_any_element(),
            MainTab::PullRequests => {
                self.ensure_pulls(cx);
                self.render_pulls(&info, main_width, cx)
            }
        };
        let _ = window;

        v_flex()
            .flex_1()
            .min_w_0()
            .h_full()
            .child(self.render_header(&info, &theme, cx))
            .child(
                h_flex()
                    .px_3()
                    .py_1()
                    .border_b_1()
                    .border_color(theme.border)
                    .child(self.render_sync(&info, cx)),
            )
            .child(
                TabBar::new("main-tabs")
                    .underline()
                    .px_2()
                    .selected_index(tab_ix)
                    .on_click(cx.listener(|this, ix: &usize, _, cx| {
                        this.tab = TABS[*ix];
                        this.reload_history(cx);
                        this.queue_persist(cx);
                        cx.notify();
                    }))
                    .child(Tab::new().label(history))
                    .child(Tab::new().label(format!("Changes · {changes}")))
                    .child(Tab::new().label(format!("Agents · {sessions}")))
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
                        h_flex()
                            .min_w_0()
                            .gap_1()
                            .child(
                                div()
                                    .min_w_0()
                                    .text_lg()
                                    .font_weight(FontWeight::BOLD)
                                    .truncate()
                                    .child(info.wt.display_name()),
                            )
                            .when_some(info.wt.branch.clone(), |row, branch| {
                                row.child(
                                    Button::new("copy-branch-name")
                                        .label("Copy")
                                        .small()
                                        .ghost()
                                        .tooltip(format!("Copy branch name: {branch}"))
                                        .on_click(cx.listener(move |this, _, _, cx| {
                                            cx.write_to_clipboard(ClipboardItem::new_string(
                                                branch.clone(),
                                            ));
                                            this.flash("Copied branch name", false, cx);
                                        })),
                                )
                            }),
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
                            .label({
                                let provider = default_provider();
                                format!("{} New {}", provider.glyph(), provider.label())
                            })
                            .small()
                            .primary()
                            .tooltip(format!("New conversation in this worktree ({MOD}-n); pick the agent before the first message"))
                            .on_click(cx.listener(|this, _, window, cx| this.new_chat(window, cx))),
                    )
                    .when_some(last_session, |el, session| {
                        el.child(
                            Button::new("claude-resume")
                                .label("Resume last")
                                .small()
                                .tooltip(format!(
                                    "Continue {} “{}” in the agent panel",
                                    session.provider.label(),
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
                            .label(launch::FILE_MANAGER)
                            .small()
                            .ghost()
                            .on_click(cx.listener({
                                let path = path.clone();
                                move |this, _, _, cx| {
                                    let r = launch::reveal_in_finder(&path);
                                    this.report(r, &format!("Opened in {}", launch::FILE_MANAGER), cx);
                                }
                            })),
                    )
                    .child(Button::new("sync-worktree").label("Sync…").small().ghost()
                        .disabled(self.git_operation_running())
                        .on_click(cx.listener(|this, _, window, cx| this.open_sync_dialog(window, cx))))
                    .when(!info.wt.is_main, |el| {
                        el.child(
                            Button::new("clean-up-worktree")
                                .label("Clean up…")
                                .small()
                                .ghost()
                                .tooltip("Remove this worktree and safely delete its local branch")
                                .on_click(cx.listener(|this, _, window, cx| {
                                    this.clean_up_worktree(&CleanUpWorktree, window, cx)
                                })),
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

    fn context_snapshot(
        &self,
        explicit_file: Option<&str>,
        scope: ContextScope,
        cx: &App,
    ) -> anyhow::Result<Attachment> {
        let path = self
            .selected
            .clone()
            .ok_or_else(|| anyhow::anyhow!("Select a worktree first"))?;
        let (state, patch, comparison, anchor) = match self.tab {
            MainTab::Changes => (
                &self.changes_diff,
                &self.details.patch,
                self.details
                    .base
                    .as_ref()
                    .map(|base| {
                        format!(
                            "Branch changes vs merge base of {base} ({})",
                            self.details
                                .merge_base
                                .as_deref()
                                .map(|oid| &oid[..oid.len().min(8)])
                                .unwrap_or("unavailable")
                        )
                    })
                    .unwrap_or_else(|| scope_description(self.details.scope).into()),
                self.repository_view()
                    .worktrees
                    .get(&path)
                    .and_then(|view| view.changes_position.as_ref()),
            ),
            MainTab::History => (
                &self.commit_diff,
                &self.commit_patch,
                format!(
                    "Commit {}",
                    self.selected_commit.as_deref().unwrap_or("unavailable")
                ),
                self.repository_view()
                    .worktrees
                    .get(&path)
                    .and_then(|view| view.commit_position.as_ref()),
            ),
            MainTab::PullRequests => {
                let (state, patch, comparison) = self.pr_diff_context()?;
                (state, patch, comparison, None)
            }
            MainTab::Sessions => {
                anyhow::bail!("Open Changes, History or a pull request's files first")
            }
        };
        let patch_text = patch.value().ok_or_else(|| {
            anyhow::anyhow!("Wait for an available diff before attaching context")
        })?;
        let state = state.read(cx);
        let selected = state
            .selected_lines()
            .filter(|range| explicit_file.is_none_or(|file| range.path().as_ref() == file));
        let relative = explicit_file
            .map(str::to_string)
            .or_else(|| selected.as_ref().map(|range| range.path().to_string()))
            .or_else(|| anchor.map(|anchor| anchor.path.clone()))
            .or_else(|| (state.files().len() == 1).then(|| state.files()[0].path().to_string()))
            .ok_or_else(|| anyhow::anyhow!("Select lines or a file in the diff first"))?;
        let file = state
            .files()
            .iter()
            .find(|file| file.path().as_ref() == relative)
            .ok_or_else(|| {
                anyhow::anyhow!("The selected file is no longer in this diff. Select it again.")
            })?;
        let raw = agent_context::file_patch(patch_text, &relative)
            .ok_or_else(|| anyhow::anyhow!("The selected diff snapshot is no longer available"))?;
        let anchor = anchor.filter(|anchor| anchor.path == relative);
        let original = selected
            .as_ref()
            .map(|range| range.side() == DiffSide::Original)
            .or_else(|| anchor.map(|anchor| anchor.original))
            .unwrap_or(file.modified_path().is_none());
        let line = selected
            .as_ref()
            .map(|range| range.start())
            .or_else(|| anchor.map(|anchor| anchor.line))
            .unwrap_or(1);
        let (selection, body) = if matches!(scope, ContextScope::Hunk) {
            (
                format!(
                    "Hunk containing {} line {line}",
                    if original { "original" } else { "modified" }
                ),
                agent_context::hunk_patch(raw, original, line)
                    .ok_or_else(|| anyhow::anyhow!("Select a source line inside the hunk first"))?,
            )
        } else if !matches!(scope, ContextScope::File)
            && explicit_file.is_none()
            && let Some(range) = selected
        {
            (
                format!(
                    "{} {} → {} {}",
                    if range.side() == DiffSide::Original {
                        "Original"
                    } else {
                        "Modified"
                    },
                    range.start(),
                    if range.end_side() == DiffSide::Original {
                        "original"
                    } else {
                        "modified"
                    },
                    range.end()
                ),
                state.selected_text(cx),
            )
        } else {
            ("File diff".into(), raw.to_string())
        };
        Ok(Attachment {
            kind: agent_context::AttachmentKind::Diff,
            worktree: path,
            path: relative,
            comparison: format!(
                "{comparison}{}",
                if patch.error().is_some()
                    || (self.tab == MainTab::Changes
                        && (self.load_error.is_some() || self.details.status.error().is_some()))
                {
                    " · stale, last successful snapshot"
                } else {
                    ""
                }
            ),
            selection,
            body,
            truncated: file.is_truncated(),
            unavailable: file.is_binary(),
        }
        .bounded())
    }

    fn attach_context(
        &mut self,
        item: Attachment,
        new: bool,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        if self.selected.as_ref() != Some(&item.worktree) {
            self.flash(
                "This context belongs to another worktree. Switch back before attaching it.",
                true,
                cx,
            );
            return;
        }
        self.ensure_chats(&item.worktree, window, cx);
        if new {
            self.new_chat(window, cx);
        }
        if let Some(chat) = self.active_chat_entity() {
            match chat.update(cx, |chat, cx| chat.attach(item, window, cx)) {
                Ok(()) => {
                    self.chat_open = true;
                    self.queue_persist(cx);
                    cx.notify();
                }
                Err(error) => self.flash(error.to_string(), true, cx),
            }
        }
    }

    fn ask_agent(&mut self, _: &AskAgent, window: &mut Window, cx: &mut Context<Self>) {
        match self.context_snapshot(None, ContextScope::Auto, cx) {
            Ok(item) => self.attach_context(item, false, window, cx),
            Err(error) => self.flash(error.to_string(), true, cx),
        }
    }

    fn ask_agent_new(
        &mut self,
        _: &AskAgentNewConversation,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        match self.context_snapshot(None, ContextScope::Auto, cx) {
            Ok(item) => self.attach_context(item, true, window, cx),
            Err(error) => self.flash(error.to_string(), true, cx),
        }
    }

    fn context_menu(
        &self,
        mut menu: PopupMenu,
        explicit_file: Option<&str>,
        cx: &mut Context<Self>,
    ) -> PopupMenu {
        let choices = if explicit_file.is_some() {
            vec![
                (ContextScope::File, "Ask agent about file", false),
                (ContextScope::File, "Ask agent in new conversation", true),
            ]
        } else {
            vec![
                (ContextScope::Auto, "Ask agent", false),
                (ContextScope::Hunk, "Ask agent about hunk", false),
                (ContextScope::File, "Ask agent about file", false),
                (ContextScope::Auto, "Ask agent in new conversation", true),
            ]
        };
        for (scope, label, new) in choices {
            let snapshot = self.context_snapshot(explicit_file, scope, cx);
            let owner = cx.entity().downgrade();
            menu = menu.item(
                PopupMenuItem::new(label)
                    .disabled(snapshot.is_err())
                    .on_click(move |_, window, cx| {
                        if let Ok(item) = &snapshot {
                            let _ = owner.update(cx, |this, cx| {
                                this.attach_context(item.clone(), new, window, cx)
                            });
                        }
                    }),
            );
        }
        menu
    }

    /// The diff beside a file list (Changes, the selected commit or pull
    /// request): a toolbar with the layout and difftastic toggles above the
    /// diff itself.
    fn render_review_diff(&self, list: FileList, cx: &mut Context<Self>) -> AnyElement {
        let owner = cx.entity().downgrade();
        // A PR whose commits are not local keeps the line diff.
        let difftastic =
            self.difftastic_active() && !(list == FileList::Pull && self.pr_difft_unavailable());
        let content = if difftastic {
            match list {
                FileList::Changes => self.changes_difft.clone().into_any_element(),
                FileList::Commit => self.commit_difft.clone().into_any_element(),
                FileList::Pull => self.pulls.difft.clone().into_any_element(),
            }
        } else {
            let state = match list {
                FileList::Changes => &self.changes_diff,
                FileList::Commit => &self.commit_diff,
                FileList::Pull => &self.pulls.diff,
            };
            // Split wraps long lines so each side keeps half the width, as
            // on GitHub; Unified scrolls sideways instead.
            let diff = Diff::new(state)
                .line_number(true)
                .soft_wrap(self.diff_layout == DiffLayout::Split);
            with_submodule_cards(diff, self.submodules(list)).into_any_element()
        };
        v_flex()
            .size_full()
            .min_w_0()
            .child(self.render_diff_toolbar(list, cx))
            .child(
                div()
                    .flex_1()
                    .min_h_0()
                    .min_w_0()
                    .overflow_hidden()
                    .child(content)
                    .context_menu(move |menu, _, cx| {
                        let Some(owner) = owner.upgrade() else {
                            return menu;
                        };
                        owner.update(cx, |this, cx| this.context_menu(menu, None, cx))
                    }),
            )
            .into_any_element()
    }

    /// Unified/Split and the difftastic switch (also in Settings).
    fn render_diff_toolbar(&self, list: FileList, cx: &mut Context<Self>) -> impl IntoElement {
        let theme = cx.theme();
        let key = list as usize;
        let layouts = [
            (
                DiffLayout::Unified,
                IconName::Rows2,
                "Unified",
                "diff-unified",
            ),
            (DiffLayout::Split, IconName::Columns2, "Split", "diff-split"),
        ]
        .map(|(layout, icon, label, id)| {
            Button::new((id, key))
                .icon(icon)
                .label(label)
                .xsmall()
                .ghost()
                .selected(self.diff_layout == layout)
                .on_click(cx.listener(move |this, _, _, cx| this.set_diff_layout(layout, cx)))
        });
        let available = self.difft_available();
        let tooltip = match &self.difft_version {
            None => "Looking for difftastic…".to_string(),
            Some(None) => "difftastic is not installed: brew install difftastic".to_string(),
            Some(Some(version)) => format!(
                "Structural diff by difftastic {version}: only changed syntax is highlighted, reformatting is ignored"
            ),
        };
        h_flex()
            .flex_none()
            .w_full()
            .min_w_0()
            .gap_1()
            .px_2()
            .py_1()
            .border_b_1()
            .border_color(theme.border)
            .children(layouts)
            .child(div().flex_1())
            .when_some(self.hunk_action_button(list, cx), |toolbar, action| {
                toolbar.child(action)
            })
            .child(
                Button::new(("diff-difftastic", key))
                    .icon(IconName::FileDiff)
                    .label("Difftastic")
                    .xsmall()
                    .ghost()
                    .selected(self.difftastic_active())
                    .disabled(!available)
                    .tooltip(tooltip)
                    .on_click(cx.listener(|this, _, _, cx| {
                        let on = !this.difftastic;
                        this.set_difftastic(on, cx);
                    })),
            )
    }

    fn difft_available(&self) -> bool {
        matches!(self.difft_version, Some(Some(_)))
    }

    fn difftastic_active(&self) -> bool {
        self.difftastic && self.difft_available()
    }

    /// Looks for `difft` once, off the main thread.
    fn detect_difftastic(&mut self, cx: &mut Context<Self>) {
        cx.spawn(async move |this, cx| {
            let version = cx
                .background_executor()
                .spawn(async { sg_git::difftastic::version() })
                .await;
            this.update(cx, |this, cx| {
                this.difft_version = Some(version);
                this.refresh_difft(FileList::Changes, cx);
                this.refresh_difft(FileList::Commit, cx);
                this.refresh_difft(FileList::Pull, cx);
                cx.notify();
            })
            .ok();
        })
        .detach();
    }

    /// Compares `list`'s files with difftastic when it is on; a no-op when
    /// the shown result is already for this patch.
    fn refresh_difft(&mut self, list: FileList, cx: &mut Context<Self>) {
        if !self.difftastic_active() {
            return;
        }
        let Some(worktree) = self.selected.clone() else {
            return;
        };
        let (state, view, patch) = match list {
            FileList::Changes => (
                &self.changes_diff,
                self.changes_difft.clone(),
                self.details.patch.value(),
            ),
            FileList::Commit => (
                &self.commit_diff,
                self.commit_difft.clone(),
                self.commit_patch.value(),
            ),
            FileList::Pull => (
                &self.pulls.diff,
                self.pulls.difft.clone(),
                self.pr_difft_parts().map(|(patch, _, _)| patch),
            ),
        };
        let submodules = self.submodules(list).to_vec();
        let files: Vec<_> = state
            .read(cx)
            .files()
            .iter()
            .map(|file| file_request(file, &submodules))
            .collect();
        let sources = match list {
            FileList::Changes if self.details.scope == ChangeScope::Staged => Some((
                sg_git::difftastic::Source::Revision("HEAD".into()),
                sg_git::difftastic::Source::Index,
            )),
            FileList::Changes if self.details.scope == ChangeScope::Unstaged => Some((
                sg_git::difftastic::Source::Index,
                sg_git::difftastic::Source::WorkTree,
            )),
            FileList::Changes => Some((
                match (&self.details.base, &self.details.merge_base) {
                    (Some(_), Some(merge_base)) => {
                        sg_git::difftastic::Source::Revision(merge_base.clone())
                    }
                    _ => sg_git::difftastic::Source::Revision("HEAD".into()),
                },
                sg_git::difftastic::Source::WorkTree,
            )),
            // The first parent, as `git show --first-parent`; a root
            // commit's missing parent reads as empty files.
            FileList::Commit => self.selected_commit.clone().map(|sha| {
                (
                    sg_git::difftastic::Source::Revision(format!("{sha}^")),
                    sg_git::difftastic::Source::Revision(sha),
                )
            }),
            // The PR's merge base and head, when both are local.
            FileList::Pull => self.pr_difft_parts().map(|(_, old, new)| (old, new)),
        };
        let (Some((old, new)), Some(patch), false) = (sources, patch, files.is_empty()) else {
            view.update(cx, |view, cx| view.clear(cx));
            return;
        };
        let revision = {
            use std::hash::{Hash as _, Hasher as _};
            let mut hasher = std::hash::DefaultHasher::new();
            patch.hash(&mut hasher);
            hasher.finish()
        };
        let request = difft_view::Request {
            worktree,
            old,
            new,
            files,
            submodules,
            revision,
        };
        view.update(cx, |view, cx| view.load(request, cx));
    }

    /// The submodules `list`'s patch changes.
    fn submodules(&self, list: FileList) -> &[SubmoduleChange] {
        match list {
            FileList::Changes => &self.details.submodules,
            FileList::Commit => &self.commit_submodules,
            FileList::Pull => self.pr_submodules(),
        }
    }

    fn set_diff_layout(&mut self, layout: DiffLayout, cx: &mut Context<Self>) {
        self.diff_layout = layout;
        let mut settings = crate::settings::load();
        settings.diff_layout = layout;
        crate::settings::save(&settings);
        let mode = diff_mode(layout);
        let cached = self.review_views.values();
        let states: Vec<_> = [
            self.changes_diff.clone(),
            self.commit_diff.clone(),
            self.pulls.diff.clone(),
        ]
        .into_iter()
        .chain(
            cached
                .clone()
                .flat_map(|view| [view.changes_diff.clone(), view.commit_diff.clone()]),
        )
        .collect();
        let views: Vec<_> = [
            self.changes_difft.clone(),
            self.commit_difft.clone(),
            self.pulls.difft.clone(),
        ]
        .into_iter()
        .chain(cached.flat_map(|view| [view.changes_difft.clone(), view.commit_difft.clone()]))
        .collect();
        for state in states {
            state.update(cx, |state, cx| state.set_mode(mode, cx));
        }
        for view in views {
            view.update(cx, |view, cx| view.set_mode(mode, cx));
        }
        cx.notify();
    }

    fn set_difftastic(&mut self, on: bool, cx: &mut Context<Self>) {
        self.difftastic = on;
        let mut settings = crate::settings::load();
        settings.difftastic = on;
        crate::settings::save(&settings);
        self.refresh_difft(FileList::Changes, cx);
        self.refresh_difft(FileList::Commit, cx);
        self.refresh_difft(FileList::Pull, cx);
        cx.notify();
    }

    /// The Settings section for diffs.
    fn render_diff_settings(&self, cx: &mut Context<Self>) -> impl IntoElement {
        let theme = cx.theme().clone();
        let available = self.difft_available();
        let status = match &self.difft_version {
            None => "Looking for difftastic…".to_string(),
            Some(None) => "difft was not found on PATH. Install it with `brew install difftastic`, then reopen SuperGIT.".to_string(),
            Some(Some(version)) => format!(
                "difftastic {version}: compares syntax trees, so only changed tokens are highlighted and reformatting shows as no change."
            ),
        };
        v_flex()
            .gap_2()
            .child(div().font_weight(FontWeight::SEMIBOLD).child("Diffs"))
            .child(
                h_flex().justify_between().child("Layout").child(
                    h_flex().gap_1().children(
                        [
                            (DiffLayout::Unified, "Unified"),
                            (DiffLayout::Split, "Split"),
                        ]
                        .map(|(layout, label)| {
                            Button::new(SharedString::from(format!("settings-layout-{label}")))
                                .label(label)
                                .small()
                                .ghost()
                                .selected(self.diff_layout == layout)
                                .on_click(cx.listener(move |this, _, _, cx| {
                                    this.set_diff_layout(layout, cx)
                                }))
                        }),
                    ),
                ),
            )
            .child(
                h_flex()
                    .justify_between()
                    .child("Show diffs with difftastic")
                    .child(
                        Button::new("settings-difftastic")
                            .label(if self.difftastic_active() {
                                "On"
                            } else {
                                "Off"
                            })
                            .small()
                            .ghost()
                            .disabled(!available)
                            .on_click(cx.listener(|this, _, _, cx| {
                                let on = !this.difftastic;
                                this.set_difftastic(on, cx);
                            })),
                    ),
            )
            .child(
                div()
                    .text_sm()
                    .text_color(theme.muted_foreground)
                    .child(status),
            )
    }

    fn comparison_base(&self) -> Option<String> {
        let view = self
            .selected
            .as_ref()
            .and_then(|path| self.repository_view().worktrees.get(path))?;
        view.branch_changes.then(|| view.base.clone())
    }

    fn change_scope(&self) -> ChangeScope {
        self.selected
            .as_ref()
            .and_then(|path| self.repository_view().worktrees.get(path))
            .filter(|view| !view.branch_changes)
            .map_or(ChangeScope::All, |view| view.change_scope)
    }

    fn change_comparison(
        &mut self,
        branch: bool,
        base: Option<String>,
        scope: ChangeScope,
        cx: &mut Context<Self>,
    ) {
        let (Some(root), Some(path)) = (
            self.repo.as_ref().map(|repo| repo.root.clone()),
            self.selected.clone(),
        ) else {
            return;
        };
        self.capture_saved_review(cx);
        let suggestion = self
            .selected_info()
            .and_then(|info| self.pr_for_branch(info.wt.branch.as_deref()))
            .map(|pr| pr.base_branch.clone())
            .or_else(|| self.details.suggested_base.clone());
        let view = self
            .persisted
            .repositories
            .entry(root)
            .or_default()
            .worktrees
            .entry(path)
            .or_default();
        view.branch_changes = branch;
        if !branch {
            view.change_scope = scope;
        }
        view.changes_position = None;
        if let Some(base) = base {
            view.base = base;
        } else if view.base.is_empty() {
            view.base = suggestion.unwrap_or_default();
        }
        self.details_generation += 1;
        self.details_task = None;
        self.details.base = self.comparison_base();
        self.details.scope = self.change_scope();
        self.details.patch = LoadState::Loading;
        self.details.files = LoadState::Loading;
        self.details.merge_base = None;
        self.changes_diff
            .update(cx, |diff, cx| diff.set_files(Vec::new(), cx));
        self.changes_difft.update(cx, |view, cx| view.clear(cx));
        self.reload_details(cx);
        self.queue_persist(cx);
        cx.notify();
    }

    fn render_comparison(&mut self, window: &mut Window, cx: &mut Context<Self>) -> AnyElement {
        let Some(path) = self.selected.clone() else {
            return div().into_any_element();
        };
        let base = self.comparison_base();
        let theme = cx.theme().clone();
        let mut bar = h_flex()
            .flex_wrap()
            .gap_1()
            .px_2()
            .py_1()
            .children(
                [
                    (ChangeScope::All, "compare-uncommitted", "Uncommitted"),
                    (ChangeScope::Staged, "compare-staged", "Staged"),
                    (ChangeScope::Unstaged, "compare-unstaged", "Unstaged"),
                ]
                .map(|(scope, id, label)| {
                    Button::new(id)
                        .label(label)
                        .xsmall()
                        .ghost()
                        .selected(base.is_none() && self.change_scope() == scope)
                        .on_click(cx.listener(move |this, _, _, cx| {
                            this.change_comparison(false, None, scope, cx)
                        }))
                }),
            )
            .child(
                Button::new("compare-branch")
                    .label("Branch changes")
                    .xsmall()
                    .ghost()
                    .selected(base.is_some())
                    .on_click(cx.listener(|this, _, _, cx| {
                        this.change_comparison(true, None, ChangeScope::All, cx)
                    })),
            )
            .child(
                Button::new("worktree-stashes")
                    .label("Stashes…")
                    .xsmall()
                    .ghost()
                    .disabled(self.git_operation_running())
                    .on_click(cx.listener(|this, _, window, cx| this.open_stashes(window, cx))),
            );
        if let Some(base) = base {
            if !self.base_inputs.contains_key(&path) {
                let input = cx.new(|cx| {
                    let mut input = InputState::new(window, cx).placeholder("Base branch");
                    input.set_value(base, window, cx);
                    input
                });
                cx.subscribe(&input, |this, input, event, cx| {
                    if matches!(event, InputEvent::PressEnter { .. }) {
                        let base = input.read(cx).value().trim().to_string();
                        this.change_comparison(true, Some(base), ChangeScope::All, cx);
                    }
                })
                .detach();
                self.base_inputs.insert(path.clone(), input);
            }
            let input = self.base_inputs[&path].clone();
            bar = bar
                .child(
                    div()
                        .text_xs()
                        .text_color(theme.muted_foreground)
                        .child("Base"),
                )
                .child(
                    div()
                        .flex_1()
                        .min_w(px(80.))
                        .child(Input::new(&input).small()),
                )
                .child(
                    Button::new("apply-base")
                        .label("Compare")
                        .xsmall()
                        .ghost()
                        .on_click(cx.listener(move |this, _, _, cx| {
                            let base = input.read(cx).value().trim().to_string();
                            this.change_comparison(true, Some(base), ChangeScope::All, cx);
                        })),
                );
        }
        bar = bar.child(
            Button::new("ask-agent-diff")
                .label("Ask agent")
                .xsmall()
                .ghost()
                .tooltip(format!("Attach selected lines or a file ({MOD}-shift-a)"))
                .on_click(cx.listener(|this, _, window, cx| this.ask_agent(&AskAgent, window, cx))),
        );
        if let Some(commit) = self.commit_staged_button(cx) {
            bar = bar.child(commit);
        }
        v_flex().w_full().border_b_1().border_color(theme.border).child(bar)
            .child(div().px_3().pb_1().text_xs().text_color(theme.muted_foreground)
                .child(if let Some(base) = &self.details.base {
                    format!("Working tree vs merge base of {base}{} · includes committed and uncommitted work",
                        self.details.merge_base.as_ref().map(|oid| format!(" ({})", &oid[..oid.len().min(8)])).unwrap_or_default())
                } else { scope_description(self.change_scope()).into() }))
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
            .or_else(|| self.details.files.error())
            .or_else(|| self.details.patch.error());
        let Some(files) = self.details.files.value() else {
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
        let files = files.clone();
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
                            if self.comparison_base().is_some() {
                                "No branch changes against this merge base"
                            } else {
                                match self.change_scope() {
                                    ChangeScope::All => "Working tree clean",
                                    ChangeScope::Staged => "No staged changes",
                                    ChangeScope::Unstaged => "No unstaged changes",
                                }
                            }
                        }),
                )
                .into_any_element();
        }
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
                    .fixed()
                    .child(self.render_file_list(FileList::Changes, &files, theme, cx)),
            )
            .child(
                resizable_panel()
                    .size_range(px(180.)..Pixels::MAX)
                    .child(elastic(self.render_review_diff(FileList::Changes, cx))),
            )
            .into_any_element();
        v_flex()
            .size_full()
            .relative()
            .children(notice)
            .child(div().flex_1().min_h_0().child(content))
            .into_any_element()
    }

    /// The file list beside a diff: a filter, the flat/tree toggle, and rows
    /// that scroll the diff to their file. Shared by Changes and History.
    fn render_file_list(
        &self,
        list: FileList,
        files: &[FileChange],
        theme: &gpui_kit::component::Theme,
        cx: &mut Context<Self>,
    ) -> AnyElement {
        let (id, filter, collapsed) = match list {
            FileList::Changes => ("change-list", &self.changes_filter, &self.changes_collapsed),
            FileList::Commit => ("commit-files", &self.commit_filter, &self.commit_collapsed),
            FileList::Pull => (
                "pr-files",
                &self.pulls.files_filter,
                &self.pulls.files_collapsed,
            ),
        };
        let filter = filter.clone();
        // Screenshot builds: `SUPERGIT_CHANGES_TREE=1` shows the tree.
        let tree = self.repository_view().changes_tree
            || (cfg!(feature = "screenshot")
                && std::env::var_os("SUPERGIT_CHANGES_TREE").is_some());
        let query = filter.read(cx).value().to_string();
        let submodules = self.submodules(list);
        let submodule = |path: &str| submodules.iter().find(|s| s.path == path);
        let visible: Vec<usize> = (0..files.len())
            .filter(|&ix| file_tree::matches(&files[ix].path, &query))
            .collect();
        let rows: Vec<AnyElement> = if tree {
            // While filtering, every folder holding a match is open.
            let no_collapsed = HashSet::new();
            let collapsed = if query.trim().is_empty() {
                collapsed
            } else {
                &no_collapsed
            };
            file_tree::rows(
                visible.iter().map(|&ix| (ix, files[ix].path.as_str())),
                collapsed,
            )
            .into_iter()
            .enumerate()
            .map(|(row, entry)| match entry {
                // A submodule with changed files inside: a click shows its
                // entry, the chevron folds its files.
                TreeRow::Dir {
                    path,
                    name,
                    depth,
                    collapsed,
                    files: count,
                    entry: Some(ix),
                } => Self::file_menu(
                    submodule_dir_row(
                        row,
                        &files[ix],
                        submodule(&files[ix].path),
                        name,
                        depth,
                        (collapsed, count),
                        theme,
                        cx.listener(move |this, _, _, cx| {
                            cx.stop_propagation();
                            this.toggle_dir(list, path.clone(), cx)
                        }),
                    )
                    .on_click(Self::open_file(list, files[ix].path.clone(), cx))
                    .children(self.staging_buttons(list, &files[ix], cx)),
                    files[ix].path.clone(),
                    cx,
                ),
                TreeRow::Dir {
                    path,
                    name,
                    depth,
                    collapsed,
                    files,
                    entry: None,
                } => tree_dir_row(row, name, depth, collapsed, files, theme)
                    .on_click(
                        cx.listener(move |this, _, _, cx| this.toggle_dir(list, path.clone(), cx)),
                    )
                    .into_any_element(),
                TreeRow::File { ix, name, depth } => Self::file_menu(
                    change_row(
                        ix,
                        &files[ix],
                        submodule(&files[ix].path),
                        name,
                        tree_indent(depth),
                        theme,
                    )
                    .on_click(Self::open_file(list, files[ix].path.clone(), cx))
                    .children(self.staging_buttons(list, &files[ix], cx)),
                    files[ix].path.clone(),
                    cx,
                ),
            })
            .collect()
        } else {
            visible
                .iter()
                .map(|&ix| {
                    let file = &files[ix];
                    Self::file_menu(
                        change_row(
                            ix,
                            file,
                            submodule(&file.path),
                            file.path.clone(),
                            px(12.),
                            theme,
                        )
                        .on_click(Self::open_file(list, file.path.clone(), cx))
                        .children(self.staging_buttons(list, file, cx)),
                        file.path.clone(),
                        cx,
                    )
                })
                .collect()
        };
        let header = h_flex()
            .flex_none()
            .gap_1()
            .px_2()
            .py_1()
            .border_b_1()
            .border_color(theme.border)
            .child(
                div().flex_1().min_w_0().child(
                    Input::new(&filter).small().cleanable(true).prefix(
                        Icon::new(IconName::Search)
                            .small()
                            .text_color(theme.muted_foreground),
                    ),
                ),
            )
            .children(
                [
                    (false, IconName::List, "Flat list"),
                    (true, IconName::ListTree, "Folder tree"),
                ]
                .map(|(as_tree, icon, tooltip)| {
                    Button::new(SharedString::from(format!(
                        "{id}-{}",
                        if as_tree { "tree" } else { "flat" }
                    )))
                    .icon(icon)
                    .small()
                    .ghost()
                    .selected(tree == as_tree)
                    .toggled(tree == as_tree)
                    .tooltip(tooltip)
                    .on_click(cx.listener(move |this, _, _, cx| this.set_changes_tree(as_tree, cx)))
                }),
            );
        v_flex()
            .size_full()
            .min_w_0()
            .border_r_1()
            .border_color(theme.border)
            .child(header)
            .child(
                v_flex()
                    .id(id)
                    .flex_1()
                    .min_h_0()
                    .overflow_y_scroll()
                    .when(rows.is_empty(), |el| {
                        el.child(
                            div()
                                .p_3()
                                .text_sm()
                                .text_color(theme.muted_foreground)
                                .child("No files match"),
                        )
                    })
                    .children(rows),
            )
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
            .or_else(|| self.history.commits.error());
        let Some(commits) = self.history.commits.value() else {
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
        let commits_max = (width - 180.).clamp(160., 800.);
        let commits_width = self
            .history_layout
            .read(cx)
            .sizes()
            .first()
            .map(|w| w.as_f32())
            .unwrap_or(self.repository_view().history_width)
            .min(commits_max);
        let terms = model::history::search_terms(&self.history_search.read(cx).value());
        let searching = !terms.is_empty();
        let matches = commits
            .iter()
            .filter(|commit| model::history::matches_commit(commit, &terms))
            .collect::<Vec<_>>();
        let (graph, lanes) = if searching {
            (Vec::new(), 0)
        } else {
            graph::layout(
                commits
                    .iter()
                    .map(|c| (c.sha.as_str(), c.parents.as_slice())),
            )
        };
        let lanes = lanes.clamp(1, GRAPH_MAX_LANES);
        let base = self.history_base();
        // Branch and tag badges get at most 40% of the commit list.
        let badge_room = (commits_width * 0.4).max(60.);
        let fork = self.selected_fork();
        let fork_point = fork
            .and_then(|fork| fork.parent.as_ref())
            .map(|parent| parent.merge_base.clone());
        let summary = fork.map(|fork| self.render_fork_summary(fork, theme, cx));
        let matching_count = matches.len();
        let mut graph = graph.into_iter();
        let rows = matches.into_iter().enumerate().map(|(ix, c)| {
            let selected = self.selected_commit.as_deref() == Some(c.sha.as_str());
            let sha = c.sha.clone();
            let avatar = self.avatars.get(&c.email.to_ascii_lowercase()).cloned();
            let badges = ref_badges(("commit-refs", ix), &c.refs, base.as_deref(), 3, theme);
            let fork_point = fork_point.as_deref() == Some(c.sha.as_str());
            commit_row(
                ix,
                c,
                graph.next().map(|row| (row, lanes)),
                avatar,
                badges,
                badge_room,
                fork_point,
                selected,
                theme,
            )
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
                    .fixed()
                    .child(
                        v_flex()
                            .size_full()
                            .min_w_0()
                            .border_r_1()
                            .border_color(theme.border)
                            .child(
                                v_flex()
                                    .flex_none()
                                    .p_2()
                                    .gap_1()
                                    .border_b_1()
                                    .border_color(theme.border)
                                    .child(
                                        Input::new(&self.history_search)
                                            .small()
                                            .cleanable(true)
                                            .prefix(
                                                Icon::new(IconName::Search)
                                                    .small()
                                                    .text_color(theme.muted_foreground),
                                            ),
                                    )
                                    .when(searching, |el| {
                                        el.child(
                                            div()
                                                .text_xs()
                                                .text_color(theme.muted_foreground)
                                                .child(format!(
                                                    "{matching_count} of {} loaded commits",
                                                    commits.len()
                                                )),
                                        )
                                    }),
                            )
                            .child(
                                v_flex()
                                    .id("commit-list")
                                    .flex_1()
                                    .min_h_0()
                                    .overflow_y_scroll()
                                    .when(matching_count == 0, |el| {
                                        el.child(
                                            div()
                                                .p_3()
                                                .text_sm()
                                                .text_color(theme.muted_foreground)
                                                .child("No loaded commits match"),
                                        )
                                    })
                                    .children(rows),
                            )
                            .when(self.history.has_more, |el| {
                                el.child(
                                    div()
                                        .flex_none()
                                        .p_2()
                                        .border_t_1()
                                        .border_color(theme.border)
                                        .child(
                                            Button::new("load-older-commits")
                                                .small()
                                                .outline()
                                                .label("Load older commits")
                                                .disabled(self.history_task.is_some())
                                                .on_click(cx.listener(|this, _, _, cx| {
                                                    this.load_history(
                                                        this.history.next_limit(),
                                                        cx,
                                                    );
                                                    cx.notify();
                                                })),
                                        ),
                                )
                            }),
                    ),
            )
            .child(
                resizable_panel()
                    .size_range(px(180.)..Pixels::MAX)
                    .child(elastic(self.render_commit_patch(width - commits_width, cx))),
            );
        v_flex()
            .size_full()
            .children(notice)
            .children(summary)
            .child(self.render_squash(commits, cx))
            .child(div().flex_1().min_h_0().child(content))
            .into_any_element()
    }

    /// The selected worktree's fork data, once it is loaded for it.
    fn selected_fork(&self) -> Option<&sg_git::Fork> {
        self.history.fork.as_ref()
    }

    /// The branch new work forks from (`origin/main`), for ordering badges.
    fn history_base(&self) -> Option<String> {
        self.selected_fork()
            .map(|fork| fork.base.clone())
            .or_else(|| self.details.suggested_base.clone())
    }

    /// Above the graph: how far the branch is from the base, and where it
    /// forked (click the sha to show that commit).
    fn render_fork_summary(
        &self,
        fork: &sg_git::Fork,
        theme: &gpui_kit::component::Theme,
        cx: &mut Context<Self>,
    ) -> AnyElement {
        let branch = self
            .selected_info()
            .and_then(|info| info.wt.branch.clone())
            .unwrap_or_else(|| "HEAD".into());
        let muted = theme.muted_foreground;
        h_flex()
            .flex_none()
            .px_3()
            .py_1()
            .gap_2()
            .items_center()
            .text_xs()
            .border_b_1()
            .border_color(theme.border)
            .child(
                div()
                    .px_1()
                    .rounded_sm()
                    .bg(theme.primary)
                    .text_color(theme.primary_foreground)
                    .child(format!("➜ {branch}")),
            )
            .child(div().child(format!("{} ahead", fork.commits)))
            .child(div().text_color(muted).child("·"))
            .child(
                div()
                    .when(fork.behind > 0, |el| el.text_color(theme.warning))
                    .child(format!("{} behind {}", fork.behind, fork.base)),
            )
            .when_some(fork.parent.clone(), |el, parent| {
                let sha = parent.merge_base.clone();
                el.child(div().text_color(muted).child("·"))
                    .child(
                        div()
                            .text_color(muted)
                            .child(format!("forked from {} at", parent.name)),
                    )
                    .child(
                        Button::new("fork-point")
                            .label(sha[..sha.len().min(8)].to_string())
                            .xsmall()
                            .ghost()
                            .tooltip(format!(
                                "Where {branch} left {}: the nearest branch sharing its history. Click to show this commit",
                                parent.name
                            ))
                            .on_click(cx.listener(move |this, _, _, cx| {
                                this.select_commit(sha.clone(), cx)
                            })),
                    )
            })
            .into_any_element()
    }

    /// The selected commit: its files (filter, flat or tree) beside its diff,
    /// laid out like the Changes tab. `width` is the room right of the
    /// commit list.
    fn render_commit_patch(&self, width: f32, cx: &mut Context<Self>) -> AnyElement {
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
        let card = self
            .commit_details
            .as_ref()
            .filter(|d| self.selected_commit.as_deref() == Some(d.commit.sha.as_str()))
            .map(|details| self.render_commit_card(details, cx));
        v_flex()
            .size_full()
            .children(notice)
            .children(card)
            .when_some(self.commit_patch.value(), |el, patch| {
                el.child(div().flex_1().min_h_0().child(if patch.is_empty() {
                    self.render_data_notice("No changes in this commit", None, false, cx)
                } else {
                    self.render_commit_files(width, cx)
                }))
            })
            .into_any_element()
    }

    /// Above a commit's files and diff: its message, author, date, sha
    /// (copyable), parents (clickable), refs and change totals.
    fn render_commit_card(&self, details: &CommitDetails, cx: &mut Context<Self>) -> AnyElement {
        let theme = cx.theme().clone();
        let muted = theme.muted_foreground;
        let commit = &details.commit;
        let (files, additions, deletions) = {
            let diff = self.commit_diff.read(cx);
            let files = diff.files();
            (
                files.len(),
                files.iter().map(DiffFile::additions).sum::<usize>(),
                files.iter().map(DiffFile::deletions).sum::<usize>(),
            )
        };
        let sha = commit.sha.clone();
        let checkout = sha.clone();
        let parents = commit.parents.iter().enumerate().map(|(ix, parent)| {
            let parent = parent.clone();
            Button::new(("commit-parent", ix))
                .label(parent[..parent.len().min(8)].to_string())
                .xsmall()
                .ghost()
                .tooltip("Show this parent commit")
                .on_click(cx.listener(move |this, _, _, cx| this.select_commit(parent.clone(), cx)))
        });
        let refs = ref_badges(
            ("commit-card-refs", 0),
            &commit.refs,
            self.history_base().as_deref(),
            usize::MAX,
            &theme,
        );
        v_flex()
            .flex_none()
            .px_4()
            .py_3()
            .gap_2()
            .border_b_1()
            .border_color(theme.border)
            .child(
                div()
                    .font_weight(FontWeight::SEMIBOLD)
                    .child(commit.subject.clone()),
            )
            .when(!details.body.is_empty(), |el| {
                el.child(
                    div()
                        .id("commit-body")
                        .max_h(px(160.))
                        .overflow_y_scroll()
                        .text_sm()
                        .text_color(muted)
                        .child(reflow(&details.body)),
                )
            })
            .child(
                h_flex()
                    .flex_wrap()
                    .gap_2()
                    .text_xs()
                    .text_color(muted)
                    .when(commit.by_agent, |el| {
                        el.child(div().text_color(theme.blue).child("✦ with Claude"))
                    })
                    .child(
                        div()
                            .text_color(theme.foreground)
                            .font_weight(FontWeight::MEDIUM)
                            .child(commit.author.clone()),
                    )
                    .child(commit.email.clone())
                    .child({
                        let ago = relative_time(commit.time);
                        let ago = if ago == "now" {
                            ago
                        } else {
                            format!("{ago} ago")
                        };
                        format!("{} · {ago}", details.date)
                    })
                    .when(details.committer != commit.author, |el| {
                        el.child(format!("committed by {}", details.committer))
                    }),
            )
            .child(
                h_flex()
                    .flex_wrap()
                    .items_center()
                    .gap_1()
                    .text_xs()
                    .text_color(muted)
                    .child(
                        div()
                            .font_family(theme.mono_font_family.clone())
                            .text_color(theme.foreground)
                            .child(commit.short_sha().to_string()),
                    )
                    .child(
                        Button::new("copy-commit-sha")
                            .label("Copy SHA")
                            .xsmall()
                            .ghost()
                            .tooltip(format!("Copy {sha}"))
                            .on_click(cx.listener(move |this, _, _, cx| {
                                cx.write_to_clipboard(ClipboardItem::new_string(sha.clone()));
                                this.flash("Copied commit SHA", false, cx);
                            })),
                    )
                    .child(
                        Button::new("worktree-from-commit")
                            .label("New worktree…")
                            .xsmall()
                            .ghost()
                            .tooltip("Create a branch and worktree at this commit")
                            .disabled(self.git_operation_running())
                            .on_click(cx.listener(move |this, _, window, cx| {
                                this.open_commit_worktree(&checkout, window, cx)
                            })),
                    )
                    .when(!commit.parents.is_empty(), |el| {
                        el.child(div().pl_2().child(if commit.parents.len() > 1 {
                            "merge of"
                        } else {
                            "parent"
                        }))
                        .children(parents)
                    })
                    .children(refs)
                    .child(div().flex_1())
                    .child(format!("{files} file{}", if files == 1 { "" } else { "s" }))
                    .child(
                        div()
                            .text_color(theme.success)
                            .child(format!("+{additions}")),
                    )
                    .child(
                        div()
                            .text_color(theme.danger)
                            .child(format!("−{deletions}")),
                    ),
            )
            .into_any_element()
    }

    fn render_commit_files(&self, width: f32, cx: &mut Context<Self>) -> AnyElement {
        let theme = cx.theme().clone();
        let files: Vec<FileChange> = self
            .commit_diff
            .read(cx)
            .files()
            .iter()
            .map(commit_file)
            .collect();
        let list_max = (width - 180.).clamp(140., 600.);
        h_resizable("commit-files-layout")
            .with_state(&self.commit_files_layout)
            .on_resize(cx.listener(|this, state, _, cx| {
                this.remember_layout(LayoutPanel::CommitFiles, state, cx)
            }))
            .child(
                resizable_panel()
                    .size(px(self.repository_view().commit_files_width.min(list_max)))
                    .size_range(px(140.)..px(list_max))
                    .fixed()
                    .child(self.render_file_list(FileList::Commit, &files, &theme, cx)),
            )
            .child(
                resizable_panel()
                    .size_range(px(180.)..Pixels::MAX)
                    .child(elastic(self.render_review_diff(FileList::Commit, cx))),
            )
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
                .child("No agent sessions in this worktree yet")
                .child(
                    h_flex()
                        .gap_2()
                        .children(Provider::ALL.into_iter().map(|provider| {
                            Button::new(("agent-new-empty", provider as usize))
                                .label(format!(
                                    "{} Start {} here",
                                    provider.glyph(),
                                    provider.label()
                                ))
                                .when(provider == default_provider(), |b| b.primary())
                                .on_click(cx.listener(move |this, _, window, cx| {
                                    this.new_chat_with(provider, window, cx)
                                }))
                        })),
                )
                .into_any_element();
        }
        let rows = info.sessions.iter().enumerate().map(|(ix, s)| {
            let id = s.id.clone();
            let provider = s.provider;
            let dir = s.start_cwd.clone().unwrap_or_else(|| path.clone());
            let session = s.clone();
            session_row(ix, s, theme)
                .child(
                    Button::new(("open-session", ix))
                        .label("Open")
                        .small()
                        .tooltip(format!("Continue in the {} panel", provider.label()))
                        .on_click(cx.listener(move |this, _, window, cx| {
                            this.open_session_chat(session.clone(), window, cx)
                        })),
                )
                .child(
                    Button::new(("resume-terminal", ix))
                        .label("Terminal")
                        .small()
                        .ghost()
                        .tooltip(format!(
                            "{} in {}",
                            launch::resume_command(provider, &id),
                            terminal.name()
                        ))
                        .on_click(cx.listener(move |this, _, _, cx| {
                            let r = launch::agent_resume(terminal, provider, &dir, &id);
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

    fn ensure_chats(&mut self, path: &PathBuf, window: &mut Window, cx: &mut Context<Self>) {
        // Every worktree gets a ready-to-type conversation; the agent process
        // only starts with the first message.
        if !self.chats.contains_key(path) {
            let saved = self.repository_view().clone();
            let conversations = saved.conversations.get(path).cloned().unwrap_or_default();
            let mut restored = Vec::new();
            for conversation in conversations
                .iter()
                .filter(|c| native_provider(&c.provider))
            {
                let chat =
                    cx.new(|cx| ChatSession::restore(path.clone(), conversation, window, cx));
                self.watch_chat(path.clone(), &chat, cx);
                restored.push(chat);
            }
            if !restored.is_empty() {
                self.active_chat.insert(
                    path.clone(),
                    saved
                        .active_conversations
                        .get(path)
                        .copied()
                        .unwrap_or(0)
                        .min(restored.len() - 1),
                );
                self.chats.insert(path.clone(), restored);
            }
        }
        if self.chats.get(path).is_none_or(Vec::is_empty) {
            let provider = default_provider();
            let chat = cx.new(|cx| ChatSession::new(path.clone(), provider, window, cx));
            self.watch_chat(path.clone(), &chat, cx);
            self.chats.insert(path.clone(), vec![chat]);
            self.active_chat.insert(path.clone(), 0);
        }
        let busy = self.base_update_paths.contains(path);
        for chat in self.chats.get(path).into_iter().flatten() {
            if busy {
                chat.update(cx, |chat, cx| chat.set_git_busy(true, cx));
            }
        }
    }

    /// Open the agent panel on the selected worktree's conversation `ix`.
    fn open_chat_at(&mut self, ix: usize, window: &mut Window, cx: &mut Context<Self>) {
        let Some(path) = self.selected.clone() else {
            return;
        };
        self.active_chat.insert(path, ix);
        self.chat_open = true;
        if let Some(chat) = self.active_chat_entity() {
            let input = chat.read(cx).input();
            input.update(cx, |state, cx| state.focus(window, cx));
        }
        self.queue_persist(cx);
        cx.notify();
    }

    /// The collapsed agent panel: a button to expand it, one glyph per
    /// conversation of the selected worktree (✦ Claude, ◆ Codex) colored by
    /// its state (title in the tooltip; clicking opens it), and a
    /// new-conversation button.
    fn render_chat_rail(&mut self, window: &mut Window, cx: &mut Context<Self>) -> AnyElement {
        let path = self.selected.clone().unwrap_or_default();
        self.ensure_chats(&path, window, cx);
        let theme = cx.theme().clone();
        let chats = self.chats.get(&path).cloned().unwrap_or_default();
        let active = self.active_chat.get(&path).copied().unwrap_or(0);
        let cells = chats.iter().enumerate().map(|(ix, chat)| {
            let c = chat.read(cx);
            let kind = c.attention().kind;
            let glyph = c.provider().glyph();
            let tooltip = if kind == Kind::Idle {
                format!("{} · {}", c.provider().label(), c.title())
            } else {
                format!(
                    "{} · {} · {}",
                    c.provider().label(),
                    c.title(),
                    kind.label()
                )
            };
            div()
                .id(("rail-chat", ix))
                .w_full()
                .py_1p5()
                .flex()
                .justify_center()
                .cursor_pointer()
                .border_r_2()
                .border_color(if ix == active {
                    theme.primary
                } else {
                    transparent_black()
                })
                .hover(|el| el.bg(theme.list_hover))
                .tooltip(move |window, cx| Tooltip::new(tooltip.clone()).build(window, cx))
                .on_click(cx.listener(move |this, _, window, cx| this.open_chat_at(ix, window, cx)))
                .child(
                    div()
                        .text_sm()
                        .text_color(attention_color(kind, &theme))
                        .child(glyph),
                )
        });
        v_flex()
            .w(px(RAIL_WIDTH))
            .flex_none()
            .h_full()
            .border_l_1()
            .border_color(theme.border)
            .child(
                h_flex().justify_center().py_1().child(
                    Button::new("expand-chat")
                        .icon(IconName::PanelRightOpen)
                        .xsmall()
                        .ghost()
                        .tooltip(format!("Expand the agent panel ({MOD}-l)"))
                        .on_click(cx.listener(|this, _, window, cx| {
                            this.toggle_claude(&ToggleClaude, window, cx)
                        })),
                ),
            )
            .child(
                v_flex()
                    .id("chat-rail")
                    .flex_1()
                    .min_h_0()
                    .overflow_y_scroll()
                    .children(cells)
                    .child(
                        h_flex().justify_center().py_1().child(
                            Button::new("rail-chat-new")
                                .label("+")
                                .xsmall()
                                .ghost()
                                .tooltip(format!("New conversation ({MOD}-n)"))
                                .on_click(
                                    cx.listener(|this, _, window, cx| this.new_chat(window, cx)),
                                ),
                        ),
                    ),
            )
            .into_any_element()
    }

    fn render_claude_panel(&mut self, window: &mut Window, cx: &mut Context<Self>) -> AnyElement {
        let path = self.selected.clone().unwrap_or_default();
        self.ensure_chats(&path, window, cx);
        let theme = cx.theme().clone();
        let chats = self.chats.get(&path).cloned().unwrap_or_default();
        let active = self
            .active_chat
            .get(&path)
            .copied()
            .unwrap_or(0)
            .min(chats.len().saturating_sub(1));
        if window.is_window_active()
            && !self.settings_open
            && !self.cleanup_results_open
            && self.switcher.is_none()
            && self.cleanup_preview.is_none()
            && self.rebase_dialog.is_none()
            && let Some(chat) = chats.get(active)
            && chat.read(cx).follows_tail(cx)
        {
            chat.update(cx, |chat, cx| chat.mark_seen(cx));
        }
        let tabs = chats.iter().enumerate().map(|(ix, chat)| {
            let c = chat.read(cx);
            let title = format!(
                "{} {}",
                c.provider().glyph(),
                c.title().chars().take(22).collect::<String>()
            );
            let dot = match c.status() {
                sg_agents::chat::Status::Working | sg_agents::chat::Status::Starting => {
                    Some(theme.blue)
                }
                sg_agents::chat::Status::NeedsPermission | sg_agents::chat::Status::NeedsInput => {
                    Some(theme.warning)
                }
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
                .when(c.attention().unread || c.attention().kind.is_live(), |el| {
                    el.child(div().text_xs().child(c.attention().kind.label()))
                })
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
                            .child(
                                chats
                                    .get(active)
                                    .map(|chat| {
                                        let provider = chat.read(cx).provider();
                                        format!("{} {}", provider.glyph(), provider.label())
                                    })
                                    .unwrap_or_else(|| "Agents".into()),
                            ),
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
                            .tooltip(format!("New conversation ({MOD}-n)"))
                            .on_click(cx.listener(|this, _, window, cx| this.new_chat(window, cx))),
                    )
                    .child(
                        Button::new("chat-hide")
                            .icon(IconName::PanelRightClose)
                            .xsmall()
                            .ghost()
                            .tooltip(format!("Collapse the agent panel ({MOD}-l)"))
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

    /// Everything between the title bar and the status bar: the sidebar
    /// (projects and their worktrees), then the active project's review and
    /// agents.
    fn render_body(&mut self, window: &mut Window, cx: &mut Context<Self>) -> AnyElement {
        let project = self.render_project(window, cx);
        if !self.projects_visible() {
            return project;
        }
        if self.persisted.projects.collapsed {
            return h_flex()
                .flex_1()
                .min_h_0()
                .child(self.render_sidebar_rail(cx))
                .child(
                    div()
                        .flex_1()
                        .min_w_0()
                        .h_full()
                        .flex()
                        .child(elastic(project)),
                )
                .into_any_element();
        }
        // What the active project needs at its narrowest.
        let rest = if self.repo.is_none() {
            480.
        } else if self.chat_open && self.selected.is_some() {
            680.
        } else {
            360.
        };
        let max = (window.viewport_size().width.as_f32() - rest).clamp(SIDEBAR_MIN, SIDEBAR_MAX);
        div()
            .flex_1()
            .min_h_0()
            .child(
                h_resizable("sidebar-layout")
                    .with_state(&self.sidebar_layout)
                    .on_resize(
                        cx.listener(|this, state, _, cx| this.remember_sidebar_width(state, cx)),
                    )
                    .child(
                        resizable_panel()
                            .size(px(self.persisted.projects.sidebar_width.min(max)))
                            .size_range(px(SIDEBAR_MIN)..px(max))
                            .fixed()
                            .child(self.render_sidebar(cx)),
                    )
                    .child(
                        resizable_panel()
                            .size_range(px(rest)..Pixels::MAX)
                            .child(elastic(project)),
                    ),
            )
            .into_any_element()
    }

    /// The active project's review and agents, or the welcome screen.
    fn render_project(&mut self, window: &mut Window, cx: &mut Context<Self>) -> AnyElement {
        if self.repo.is_none() {
            return self.render_welcome(cx).into_any_element();
        }
        let main = self.render_main(window, cx);
        if self.chat_open && self.selected.is_some() {
            let (chat_max, _) = self.pane_widths(window, cx);
            let chat = self.render_claude_panel(window, cx);
            div()
                .flex_1()
                .min_h_0()
                .size_full()
                .child(
                    h_resizable("chat-layout")
                        .with_state(&self.chat_layout)
                        .on_resize(cx.listener(|this, state, _, cx| {
                            this.remember_layout(LayoutPanel::Chat, state, cx)
                        }))
                        .child(
                            resizable_panel()
                                .size_range(px(360.)..Pixels::MAX)
                                .child(elastic(main)),
                        )
                        .child(
                            resizable_panel()
                                .size(px(self.repository_view().chat_width.min(chat_max)))
                                .size_range(px(320.)..px(chat_max))
                                .fixed()
                                .child(chat),
                        ),
                )
                .into_any_element()
        } else if self.selected.is_some() {
            let rail = self.render_chat_rail(window, cx);
            // Elastic like the panels above: wide content (a long commit
            // message, a diff line) must not push the rail off-screen.
            h_flex()
                .flex_1()
                .min_h_0()
                .size_full()
                .child(div().flex_1().min_w_0().h_full().child(elastic(main)))
                .child(rail)
                .into_any_element()
        } else {
            main
        }
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
                .on_click(cx.listener(move |this, _, _, cx| {
                    this.open_path(path.clone(), cx);
                }))
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
                .child(div().text_color(theme.muted_foreground).child(
                    "Worktrees, Claude and Codex sessions, commits and pull requests in one place.",
                ))
                .child(
                    h_flex()
                        .gap_2()
                        .child(
                            Button::new("welcome-open")
                                .label("Open Repository…")
                                .primary()
                                .on_click(cx.listener(|this, _, window, cx| {
                                    this.open_repository(&OpenRepository, window, cx)
                                })),
                        )
                        .child(
                            Button::new("welcome-clone")
                                .label("Clone Repository…")
                                .outline()
                                .on_click(cx.listener(|this, _, window, cx| {
                                    this.open_clone_form(window, cx)
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
            .when(!self.cleanup_results.is_empty(), |bar| {
                bar.child(
                    Button::new("cleanup-details")
                        .label("Cleanup details")
                        .xsmall()
                        .ghost()
                        .on_click(cx.listener(|this, _, window, cx| {
                            this.cleanup_results_open = true;
                            this.cleanup_handle.focus(window, cx);
                            cx.notify();
                        })),
                )
            })
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
            .track_focus(&self.focus_handle)
            .relative()
            .bg(theme.background)
            .text_color(theme.foreground)
            .on_action(cx.listener(Self::refresh))
            .on_action(cx.listener(Self::open_repository))
            .on_action(cx.listener(Self::close_repository))
            .on_action(cx.listener(Self::toggle_claude))
            .on_action(cx.listener(Self::toggle_sidebar))
            .on_action(cx.listener(Self::new_chat_action))
            .on_action(cx.listener(Self::update_creation_base))
            .on_action(cx.listener(Self::open_switcher))
            .on_action(cx.listener(Self::open_settings))
            .on_action(cx.listener(Self::ask_agent))
            .on_action(cx.listener(Self::ask_agent_new))
            .on_action(cx.listener(Self::toggle_keep_worktree))
            .on_action(cx.listener(Self::clean_up_worktree))
            .child(self.render_title_bar(cx))
            .child(self.render_body(window, cx))
            .child(self.render_status_bar(cx))
            .when(self.switcher.is_some(), |el| {
                el.child(self.render_switcher(cx))
            })
            .when(self.stash_dialog.is_some(), |el| {
                el.child(self.render_stashes(cx))
            })
            .when(self.settings_open, |el| el.child(self.render_settings(cx)))
            .when(self.clone_repository.is_some(), |el| {
                el.child(self.render_clone_form(cx))
            })
            .when(self.cleanup_preview.is_some(), |el| {
                el.child(self.render_cleanup_preview(cx))
            })
            .when(self.rebase_dialog.is_some(), |el| {
                el.child(self.render_rebase_dialog(cx))
            })
            .when(self.sync_dialog.is_some(), |el| {
                el.child(self.render_sync_dialog(cx))
            })
            .when(self.pulls.dialog_open(), |el| {
                el.child(self.render_pr_dialog(cx))
            })
            .when(self.worktree_dialog.is_some(), |el| {
                el.child(self.render_worktree_dialog(cx))
            })
            .when(self.cleanup_results_open, |el| {
                el.child(self.render_cleanup_results(cx))
            })
            .when(self.staged_commit.is_some(), |el| {
                el.child(self.render_staged_commit(cx))
            })
    }
}

fn scope_description(scope: ChangeScope) -> &'static str {
    match scope {
        ChangeScope::All => "Working tree vs HEAD · staged, unstaged and untracked changes",
        ChangeScope::Staged => "Index vs HEAD · contents prepared for commit",
        ChangeScope::Unstaged => "Working tree vs index · unstaged and untracked changes",
    }
}

/// The content of a panel that should give way: it takes the panel's size
/// and clips what does not fit (long diff lines scroll inside), so wide
/// content never widens the panel and pushes the chat or sidebar away. A
/// column, so the content is exactly as wide as the panel and `flex_1` /
/// `h_full` children fill its height.
/// An empty diff in the user's layout.
fn new_diff_state(layout: DiffLayout, cx: &mut App) -> Entity<DiffState> {
    cx.new(|cx| DiffState::new(Vec::<DiffFile>::new(), cx).with_mode(diff_mode(layout)))
}

fn diff_mode(layout: DiffLayout) -> DiffMode {
    match layout {
        DiffLayout::Unified => DiffMode::Unified,
        DiffLayout::Split => DiffMode::Split,
    }
}

/// Submodule entries in `diff` get a `submodule` tag after their path and a
/// card under their header: the recorded commits, the commits in between
/// and the state of the checkout.
fn with_submodule_cards(diff: Diff, submodules: &[SubmoduleChange]) -> Diff {
    if submodules.is_empty() {
        return diff;
    }
    let submodules = std::rc::Rc::new(submodules.to_vec());
    let cards = submodules.clone();
    let tags = submodules.clone();
    diff.annotations(submodules.iter().map(|submodule| {
        DiffAnnotation::file(
            crate::submodules::card_id(submodule),
            submodule.path.clone(),
        )
    }))
    .render_annotation(move |annotation, _, cx| {
        cards
            .iter()
            .find(|submodule| submodule.path == annotation.path().as_ref())
            .map(|submodule| crate::submodules::card(submodule, cx.theme()))
            .unwrap_or_else(div)
    })
    .render_header_title_suffix(move |file, _, cx| {
        if tags
            .iter()
            .any(|submodule| submodule.path == file.path().as_ref())
        {
            crate::submodules::tag(cx.theme())
        } else {
            div()
        }
    })
}

/// A file of a parsed patch, as difftastic should compare it.
fn file_request(
    file: &DiffFile,
    submodules: &[SubmoduleChange],
) -> sg_git::difftastic::FileRequest {
    let old_path = match (file.original_path(), file.modified_path()) {
        (Some(old), Some(new)) if old != new => Some(old.to_string()),
        _ => None,
    };
    sg_git::difftastic::FileRequest {
        path: file.path().to_string(),
        old_path,
        added: file.status() == DiffFileStatus::Added,
        deleted: file.status() == DiffFileStatus::Deleted,
        submodule: submodules.iter().any(|s| s.path == file.path().as_ref()),
    }
}

fn elastic(content: impl IntoElement) -> Div {
    v_flex()
        .size_full()
        .min_w_0()
        .min_h_0()
        .overflow_hidden()
        .child(content)
}

/// `SUPERGIT_TAB=history|changes|claude|prs` picks the tab shown at startup.
fn initial_tab() -> MainTab {
    MainTab::from_name(&std::env::var("SUPERGIT_TAB").unwrap_or_default())
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

fn attention_color(kind: Kind, theme: &gpui_kit::component::Theme) -> Hsla {
    match kind {
        Kind::Approval | Kind::Question => theme.warning,
        Kind::Failed => theme.danger,
        Kind::Completed => theme.success,
        Kind::Running => theme.blue,
        Kind::Idle => theme.muted_foreground,
    }
}

/// Saved conversations of a provider this build drives natively (an empty
/// provider is Claude: state written before providers existed).
pub(crate) fn native_provider(provider: &str) -> bool {
    Provider::parse(provider).is_some()
}

/// The agent new conversations start with: the one picked last.
pub(crate) fn default_provider() -> Provider {
    Provider::parse(&crate::settings::load().agent).unwrap_or_default()
}

/// Width of a collapsed side panel (sidebar, agent panel).
const RAIL_WIDTH: f32 = 48.;
/// The expanded sidebar's width range.
const SIDEBAR_MIN: f32 = 220.;
const SIDEBAR_MAX: f32 = 520.;

/// Two letters for a worktree in the collapsed list: the first letters of
/// the first two words (`claude-auto-mode` → `CA`), else the name's first
/// two letters (`main` → `Ma`).
fn initials(name: &str) -> String {
    let words: Vec<&str> = name
        .split(['-', '_', '/', '.', ' '])
        .filter(|w| !w.is_empty())
        .collect();
    let mut out = String::new();
    if words.len() >= 2 {
        for word in &words[..2] {
            out.extend(word.chars().next().into_iter().flat_map(char::to_uppercase));
        }
    } else {
        let mut chars = name.chars();
        out.extend(chars.next().into_iter().flat_map(char::to_uppercase));
        out.extend(chars.next());
    }
    out
}

/// The saved view of a project that has none yet.
fn empty_view() -> &'static RepositoryView {
    static EMPTY: std::sync::OnceLock<RepositoryView> = std::sync::OnceLock::new();
    EMPTY.get_or_init(RepositoryView::default)
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

/// Commit messages are hard-wrapped at ~72 columns; rejoin each paragraph
/// so it wraps to the pane instead. Blank lines, list items (`- `, `* `,
/// `1. `) and trailers (`Key: value`) keep their own lines.
fn reflow(body: &str) -> String {
    fn list_item(line: &str) -> bool {
        let line = line.trim_start();
        ["- ", "* ", "• "].iter().any(|p| line.starts_with(p))
            || line.split_once(['.', ')']).is_some_and(|(n, rest)| {
                !n.is_empty() && n.chars().all(|c| c.is_ascii_digit()) && rest.starts_with(' ')
            })
    }
    fn trailer(line: &str) -> bool {
        line.split_once(": ").is_some_and(|(key, _)| {
            !key.is_empty()
                && key.len() <= 30
                && key.chars().all(|c| c.is_ascii_alphanumeric() || c == '-')
        })
    }
    let mut out = String::new();
    let mut joinable = false;
    for line in body.lines().map(str::trim_end) {
        if joinable && !line.is_empty() && !list_item(line) && !trailer(line) {
            out.push(' ');
            out.push_str(line.trim_start());
        } else {
            if !out.is_empty() {
                out.push('\n');
            }
            out.push_str(line);
        }
        joinable = !line.is_empty() && !trailer(line);
    }
    out
}

/// A file of a commit's diff as a list row: committed, so never "staged".
fn commit_file(file: &DiffFile) -> FileChange {
    let state = match file.status() {
        DiffFileStatus::Added => FileState::Added,
        DiffFileStatus::Deleted => FileState::Deleted,
        DiffFileStatus::Renamed => FileState::Renamed,
        DiffFileStatus::Copied => FileState::Copied,
        DiffFileStatus::Conflicted => FileState::Conflicted,
        _ => FileState::Modified,
    };
    FileChange {
        path: file.path().to_string(),
        orig_path: file
            .original_path()
            .filter(|orig| *orig != file.path())
            .map(|orig| orig.to_string()),
        staged: None,
        unstaged: Some(state),
        submodule: None,
    }
}

/// Indentation of a file-list tree row at `depth`.
fn tree_indent(depth: usize) -> Pixels {
    px(12. + depth as f32 * 14.)
}

/// A changed file in the Changes tab's list, labelled `label` (the path, or
/// the file name inside the tree). A submodule shows its icon and what
/// changed in it.
fn change_row(
    ix: usize,
    file: &FileChange,
    submodule: Option<&SubmoduleChange>,
    label: String,
    indent: Pixels,
    theme: &gpui_kit::component::Theme,
) -> Stateful<Div> {
    let state = file.state();
    let is_submodule = submodule.is_some() || file.submodule.is_some();
    h_flex()
        .id(("change", ix))
        .pl(indent)
        .pr_3()
        .py_1()
        .gap_2()
        .text_sm()
        .cursor_pointer()
        .hover(|el| el.bg(theme.list_hover))
        .child(
            div()
                .w(px(14.))
                .flex_none()
                .font_family(theme.mono_font_family.clone())
                .text_color(file_state_color(state, theme))
                .child(state.letter()),
        )
        .when(is_submodule, |el| {
            el.child(
                Icon::new(IconName::FolderGit2)
                    .small()
                    .text_color(theme.muted_foreground),
            )
        })
        .child(submodule_label(label, submodule, theme))
        .when(file.staged.is_some(), |el| {
            el.child(div().text_xs().text_color(theme.success).child("staged"))
        })
}

/// A file-list label; a submodule's has what changed in it underneath.
fn submodule_label(
    label: String,
    submodule: Option<&SubmoduleChange>,
    theme: &gpui_kit::component::Theme,
) -> Div {
    let summary = submodule
        .map(crate::submodules::summary)
        .filter(|summary| !summary.is_empty());
    v_flex()
        .flex_1()
        .min_w_0()
        .child(div().truncate().child(label))
        .when_some(summary, |el, summary| {
            el.child(
                div()
                    .truncate()
                    .text_xs()
                    .text_color(theme.muted_foreground)
                    .child(summary),
            )
        })
}

/// A submodule in the tree whose changed files are listed inside it: a
/// folder row (`toggle` on its chevron) that is also the submodule's entry.
#[expect(
    clippy::too_many_arguments,
    reason = "one row builder, mirroring change_row and tree_dir_row"
)]
fn submodule_dir_row(
    row: usize,
    file: &FileChange,
    submodule: Option<&SubmoduleChange>,
    name: String,
    depth: usize,
    (collapsed, files): (bool, usize),
    theme: &gpui_kit::component::Theme,
    toggle: impl Fn(&ClickEvent, &mut Window, &mut App) + 'static,
) -> Stateful<Div> {
    let muted = theme.muted_foreground;
    let state = file.state();
    h_flex()
        .id(("change-dir", row))
        .pl(tree_indent(depth))
        .pr_3()
        .py_1()
        .gap_2()
        .text_sm()
        .cursor_pointer()
        .hover(|el| el.bg(theme.list_hover))
        .child(
            div()
                .id(("change-dir-toggle", row))
                .w(px(14.))
                .flex_none()
                .on_click(toggle)
                .child(
                    Icon::new(if collapsed {
                        IconName::ChevronRight
                    } else {
                        IconName::ChevronDown
                    })
                    .small()
                    .text_color(muted),
                ),
        )
        .child(
            Icon::new(IconName::FolderGit2)
                .small()
                .text_color(file_state_color(state, theme)),
        )
        .child(submodule_label(name, submodule, theme))
        .when(collapsed, |el| {
            el.child(div().text_xs().text_color(muted).child(files.to_string()))
        })
        .when(file.staged.is_some(), |el| {
            el.child(div().text_xs().text_color(theme.success).child("staged"))
        })
}

/// A folder in the Changes tab's tree; collapsed ones show how many changed
/// files they hold.
fn tree_dir_row(
    row: usize,
    name: String,
    depth: usize,
    collapsed: bool,
    files: usize,
    theme: &gpui_kit::component::Theme,
) -> Stateful<Div> {
    let muted = theme.muted_foreground;
    h_flex()
        .id(("change-dir", row))
        .pl(tree_indent(depth))
        .pr_3()
        .py_1()
        .gap_2()
        .text_sm()
        .cursor_pointer()
        .hover(|el| el.bg(theme.list_hover))
        .child(
            div().w(px(14.)).flex_none().child(
                Icon::new(if collapsed {
                    IconName::ChevronRight
                } else {
                    IconName::ChevronDown
                })
                .small()
                .text_color(muted),
            ),
        )
        .child(
            Icon::new(if collapsed {
                IconName::Folder
            } else {
                IconName::FolderOpen
            })
            .small()
            .text_color(muted),
        )
        .child(div().min_w_0().truncate().child(name))
        .when(collapsed, |el| {
            el.child(div().text_xs().text_color(muted).child(files.to_string()))
        })
}

/// Commit rows have one height so the graph's lines meet across rows.
const COMMIT_ROW_HEIGHT: f32 = 28.;
const GRAPH_LANE_WIDTH: f32 = 12.;
/// Lanes beyond this are not drawn; HEAD's history rarely needs them.
const GRAPH_MAX_LANES: usize = 10;

/// The branch graph cell of a commit row: lines through and out of the
/// commit, and its dot (hollow for a merge).
fn commit_graph(
    row: graph::GraphRow,
    lanes: usize,
    merge: bool,
    theme: &gpui_kit::component::Theme,
) -> impl IntoElement {
    let palette = [
        theme.blue,
        theme.magenta,
        theme.green,
        theme.yellow,
        theme.red,
    ];
    let background = theme.background;
    canvas(
        |_, _, _| (),
        move |bounds, _, window, _| {
            let x = |lane: usize| {
                bounds.left() + px(GRAPH_LANE_WIDTH * lane as f32 + GRAPH_LANE_WIDTH / 2.)
            };
            let (top, mid, bottom) = (bounds.top(), bounds.center().y, bounds.bottom());
            for edge in &row.edges {
                if edge.from >= lanes || edge.to >= lanes {
                    continue;
                }
                let (start, end) = match edge.kind {
                    graph::EdgeKind::Pass => {
                        (point(x(edge.from), top), point(x(edge.from), bottom))
                    }
                    graph::EdgeKind::In => (point(x(edge.from), top), point(x(edge.to), mid)),
                    graph::EdgeKind::Out => (point(x(edge.from), mid), point(x(edge.to), bottom)),
                };
                let mut path = PathBuilder::stroke(px(1.5));
                path.move_to(start);
                if start.x == end.x {
                    path.line_to(end);
                } else {
                    let half = (start.y + end.y) / 2.;
                    path.cubic_bezier_to(end, point(start.x, half), point(end.x, half));
                }
                if let Ok(path) = path.build() {
                    window.paint_path(path, palette[edge.color % palette.len()]);
                }
            }
            if row.lane < lanes {
                let color = palette[row.color % palette.len()];
                let radius = px(4.);
                let center = point(x(row.lane), mid);
                window.paint_quad(quad(
                    Bounds::new(
                        point(center.x - radius, center.y - radius),
                        size(radius * 2., radius * 2.),
                    ),
                    radius,
                    if merge { background } else { color },
                    px(1.5),
                    color,
                    BorderStyle::Solid,
                ));
            }
        },
    )
    .flex_none()
    .w(px(GRAPH_LANE_WIDTH * lanes as f32))
    .h_full()
}

/// The commit author's GitHub avatar, or their initials on a color picked
/// from the email when GitHub does not know them; name and email on hover.
fn author_avatar(
    ix: usize,
    c: &Commit,
    avatar: Option<PathBuf>,
    theme: &gpui_kit::component::Theme,
) -> Stateful<Div> {
    let tooltip = format!("{} <{}>", c.author, c.email);
    let palette = [
        theme.blue,
        theme.magenta,
        theme.green,
        theme.yellow,
        theme.red,
    ];
    let hash = c
        .email
        .to_ascii_lowercase()
        .bytes()
        .fold(0usize, |h, b| h.wrapping_mul(31).wrapping_add(b as usize));
    div()
        .id(("commit-author", ix))
        .flex_none()
        .size(px(18.))
        .rounded_full()
        .overflow_hidden()
        .tooltip(move |window, cx| Tooltip::new(tooltip.clone()).build(window, cx))
        .map(|el| match avatar {
            Some(path) => el.child(img(path).size_full().rounded_full()),
            None => el
                .flex()
                .items_center()
                .justify_center()
                .bg(palette[hash % palette.len()])
                .text_color(white())
                .text_size(px(8.))
                .font_weight(FontWeight::SEMIBOLD)
                .child(initials(&c.author)),
        })
}

/// A commit's branches and tags as badges: the checked-out branch first,
/// then the base, tags, local and remote branches. A local branch and its
/// remote copy on the same commit share one badge (`main · origin`). Past
/// `max` badges the rest collapse into `+N`, listed on hover.
fn ref_badges(
    id: (&'static str, usize),
    refs: &[RefLabel],
    base: Option<&str>,
    max: usize,
    theme: &gpui_kit::component::Theme,
) -> Vec<AnyElement> {
    let base_name = base.map(|base| base.split_once('/').map_or(base, |(_, b)| b));
    let mut merged = vec![false; refs.len()];
    let mut groups: Vec<(&RefLabel, Vec<&str>)> = Vec::new();
    for (ix, label) in refs.iter().enumerate() {
        if merged[ix] {
            continue;
        }
        let mut remotes = Vec::new();
        if label.kind == RefKind::Branch {
            for (other, remote) in refs.iter().enumerate() {
                if !merged[other]
                    && remote.kind == RefKind::Remote
                    && remote.branch_name() == label.name
                {
                    merged[other] = true;
                    remotes.push(remote.name.split_once('/').map_or("", |(r, _)| r));
                }
            }
        }
        groups.push((label, remotes));
    }
    // A remote copy listed before its local branch was merged afterwards.
    groups.retain(|(label, _)| {
        label.kind != RefKind::Remote
            || !refs
                .iter()
                .any(|l| l.kind == RefKind::Branch && l.name == label.branch_name())
    });
    let rank = |label: &RefLabel| match label {
        l if l.current => 0,
        l if Some(l.branch_name()) == base_name && l.kind != RefKind::Tag => 1,
        l if l.kind == RefKind::Tag => 2,
        l if l.kind == RefKind::Branch => 3,
        _ => 4,
    };
    groups.sort_by_key(|(label, _)| rank(label));
    let overflow: Vec<String> = groups
        .iter()
        .skip(max)
        .map(|(label, _)| label.name.clone())
        .collect();
    let mut badges: Vec<AnyElement> = groups
        .into_iter()
        .take(max)
        .map(|(label, remotes)| {
            let badge = h_flex()
                .flex_none()
                .gap_0p5()
                .px_1()
                .rounded_sm()
                .text_xs()
                .border_1();
            let badge = match label.kind {
                _ if label.current => badge
                    .bg(theme.primary)
                    .border_color(theme.primary)
                    .text_color(theme.primary_foreground)
                    .child(format!("➜ {}", label.name)),
                RefKind::Tag => badge
                    .bg(theme.yellow.opacity(0.15))
                    .border_color(theme.yellow.opacity(0.5))
                    .text_color(theme.foreground)
                    .child(Icon::new(IconName::Tag).xsmall())
                    .child(label.name.clone()),
                RefKind::Remote => badge
                    .border_color(theme.border)
                    .text_color(theme.muted_foreground)
                    .child(label.name.clone()),
                RefKind::Branch | RefKind::Head => badge
                    .bg(theme.blue.opacity(0.12))
                    .border_color(theme.blue.opacity(0.5))
                    .text_color(theme.foreground)
                    .child(label.name.clone()),
            };
            badge
                .when(!remotes.is_empty(), |el| {
                    el.child(
                        div()
                            .opacity(0.7)
                            .child(format!("· {}", remotes.join(", "))),
                    )
                })
                .into_any_element()
        })
        .collect();
    if !overflow.is_empty() {
        let tooltip = overflow.join("\n");
        badges.push(
            div()
                .id(id)
                .flex_none()
                .px_1()
                .text_xs()
                .text_color(theme.muted_foreground)
                .tooltip(move |window, cx| Tooltip::new(tooltip.clone()).build(window, cx))
                .child(format!("+{}", overflow.len()))
                .into_any_element(),
        );
    }
    badges
}

#[expect(
    clippy::too_many_arguments,
    reason = "a History row draws one commit from independent view state"
)]
fn commit_row(
    ix: usize,
    c: &Commit,
    graph: Option<(graph::GraphRow, usize)>,
    avatar: Option<PathBuf>,
    badges: Vec<AnyElement>,
    badge_room: f32,
    fork_point: bool,
    selected: bool,
    theme: &gpui_kit::component::Theme,
) -> Stateful<Div> {
    h_flex()
        .id(("commit", ix))
        .h(px(COMMIT_ROW_HEIGHT))
        .pl_2()
        .pr_3()
        .gap_2()
        .text_sm()
        .cursor_pointer()
        .when(selected, |el| el.bg(theme.list_active))
        .hover(|el| el.bg(theme.list_hover))
        .when_some(graph, |el, (graph, lanes)| {
            el.child(commit_graph(graph, lanes, c.is_merge(), theme))
        })
        .child(author_avatar(ix, c, avatar, theme))
        .when(c.by_agent, |el| {
            el.child(
                div()
                    .flex_none()
                    .text_xs()
                    .text_color(theme.blue)
                    .child("✦"),
            )
        })
        // Badges take up to `badge_room` and clip at the right; the subject
        // truncates in the rest.
        .child(div().flex_1().min_w_0().truncate().child(c.subject.clone()))
        .child(
            h_flex()
                .flex_none()
                .max_w(px(badge_room))
                .overflow_hidden()
                .gap_1()
                .when(fork_point, |el| {
                    el.child(
                        div()
                            .flex_none()
                            .px_1()
                            .rounded_sm()
                            .border_1()
                            .border_color(theme.warning)
                            .text_xs()
                            .text_color(theme.warning)
                            .child("fork point"),
                    )
                })
                .children(badges),
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
                .w(px(88.))
                .flex_none()
                .text_xs()
                .text_color(agent_color(s.state, theme))
                .child(format!("{} {}", s.provider.glyph(), s.state.label())),
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

#[cfg(test)]
mod tests {
    // Not `super::*`: GPUI's glob import brings its own `test` attribute.
    use super::{DiffFile, FileState, commit_file, initials, reflow};

    #[test]
    fn rail_initials() {
        assert_eq!(initials("main"), "Ma");
        assert_eq!(initials("FaceLifting"), "Fa");
        assert_eq!(initials("Claude-Auto-mode"), "CA");
        assert_eq!(initials("feat/keyboard-switcher"), "FK");
        assert_eq!(initials("x"), "X");
        assert_eq!(initials(""), "");
    }

    #[test]
    fn commit_bodies_reflow_paragraphs_but_keep_lists_and_trailers() {
        let body = "\
Selecting a commit now lays out like the
Changes tab: commits | files | diff.

- The file list is the same component
  as in Changes.
- Built from the parsed diff.
1. First step
Note: a trailer-like line

Co-Authored-By: Claude <noreply@anthropic.com>
Signed-off-by: Someone <a@b.c>";
        assert_eq!(
            reflow(body),
            "\
Selecting a commit now lays out like the Changes tab: commits | files | diff.

- The file list is the same component as in Changes.
- Built from the parsed diff.
1. First step
Note: a trailer-like line

Co-Authored-By: Claude <noreply@anthropic.com>
Signed-off-by: Someone <a@b.c>"
        );
        assert_eq!(reflow(""), "");
    }

    #[test]
    fn commit_files_carry_their_change_kind() {
        let patch = "\
diff --git a/new.rs b/new.rs
new file mode 100644
--- /dev/null
+++ b/new.rs
@@ -0,0 +1 @@
+fn main() {}
diff --git a/gone.rs b/gone.rs
deleted file mode 100644
--- a/gone.rs
+++ /dev/null
@@ -1 +0,0 @@
-fn old() {}
diff --git a/src/old_name.rs b/src/new_name.rs
similarity index 100%
rename from src/old_name.rs
rename to src/new_name.rs
diff --git a/src/lib.rs b/src/lib.rs
--- a/src/lib.rs
+++ b/src/lib.rs
@@ -1 +1 @@
-pub fn a() {}
+pub fn b() {}
";
        let files: Vec<_> = DiffFile::parse(patch)
            .unwrap()
            .iter()
            .map(commit_file)
            .collect();
        let summary: Vec<_> = files
            .iter()
            .map(|f| (f.path.as_str(), f.state(), f.orig_path.as_deref()))
            .collect();
        assert_eq!(
            summary,
            vec![
                ("new.rs", FileState::Added, None),
                ("gone.rs", FileState::Deleted, None),
                (
                    "src/new_name.rs",
                    FileState::Renamed,
                    Some("src/old_name.rs")
                ),
                ("src/lib.rs", FileState::Modified, None),
            ]
        );
        // Committed files are never shown as "staged".
        assert!(files.iter().all(|f| f.staged.is_none()));
    }
}
