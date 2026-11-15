//! The main window: worktrees on the left, the selected worktree's changes,
//! history, Claude sessions and pull requests on the right.

use std::path::PathBuf;
use std::time::{Duration, SystemTime};

use gpui_kit::component::button::{Button, ButtonVariants as _};
use gpui_kit::component::diff::{Diff, DiffFile, DiffState};
use gpui_kit::component::input::{Input, InputEvent, InputState};
use gpui_kit::component::tab::{Tab, TabBar};
use gpui_kit::component::{ActiveTheme as _, Sizable as _, TitleBar, h_flex, v_flex};
use gpui_kit::prelude::*;
use gpui_kit::*;
use sg_agents::{AgentState, Session};
use sg_git::{Commit, FileState, Repo, relative_time};
use sg_github::{PrState, PullRequest};

use crate::launch::{self, TerminalApp};
use crate::model::{self, Details, SharedSessionIndex, WorktreeInfo};

const POLL_INTERVAL: Duration = Duration::from_secs(3);
/// PRs come from the network: refresh every Nth poll.
const PR_POLL_EVERY: u64 = 20;

actions!(supergit, [Refresh]);

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum MainTab {
    Changes,
    History,
    Sessions,
    PullRequests,
}

const TABS: [MainTab; 4] = [
    MainTab::Changes,
    MainTab::History,
    MainTab::Sessions,
    MainTab::PullRequests,
];

pub struct SuperGitApp {
    repo: Repo,
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
    selected_commit: Option<String>,
    commit_diff: Entity<DiffState>,

    branch_input: Entity<InputState>,
    confirm_remove: Option<PathBuf>,
    message: Option<(String, bool)>,

    poll_task: Option<Task<()>>,
    details_task: Option<Task<()>>,
    commit_task: Option<Task<()>>,
    action_task: Option<Task<()>>,
    _subscriptions: Vec<Subscription>,
}

impl SuperGitApp {
    pub fn new(
        repo: Repo,
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
        let changes_diff = cx.new(|cx| DiffState::new(Vec::<DiffFile>::new(), cx));
        let commit_diff = cx.new(|cx| DiffState::new(Vec::<DiffFile>::new(), cx));

        let mut this = Self {
            repo,
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
            selected_commit: None,
            commit_diff,
            branch_input,
            confirm_remove: None,
            message: None,
            poll_task: None,
            details_task: None,
            commit_task: None,
            action_task: None,
            _subscriptions: vec![subscription],
        };
        this.start_polling(cx);
        this
    }

    // ---- data flow -------------------------------------------------------

    /// (Re)start the refresh loop. Restarting refreshes immediately, PRs included.
    fn start_polling(&mut self, cx: &mut Context<Self>) {
        self.poll_task = Some(cx.spawn(async move |this, cx| {
            let mut tick: u64 = 0;
            loop {
                let Ok((repo, index)) =
                    this.read_with(cx, |this, _| (this.repo.clone(), this.index.clone()))
                else {
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
            Err(e) => self.load_error = Some(e.to_string()),
        }
        let valid = self
            .selected
            .as_ref()
            .is_some_and(|p| self.worktrees.iter().any(|w| &w.wt.path == p));
        if !valid {
            // Prefer the worktree SuperGIT was launched from.
            self.selected = self
                .worktrees
                .iter()
                .filter(|w| self.launch_dir.starts_with(&w.wt.path))
                .max_by_key(|w| w.wt.path.as_os_str().len())
                .or(self.worktrees.first())
                .map(|w| w.wt.path.clone());
        }
        self.reload_details(cx);
        cx.notify();
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
        let Some(info) = self.selected_info() else {
            return;
        };
        let path = info.wt.path.clone();
        let status = info.status.clone().ok();
        self.details_task = Some(cx.spawn(async move |this, cx| {
            let details = cx
                .background_executor()
                .spawn(async move { model::load_details(&path, status.as_ref()) })
                .await;
            this.update(cx, |this, cx| this.apply_details(details, cx))
                .ok();
        }));
    }

    fn apply_details(&mut self, details: Details, cx: &mut Context<Self>) {
        if self.selected.as_ref() != Some(&details.path) {
            return;
        }
        if details.patch != self.details.patch || details.path != self.details.path {
            let files = DiffFile::parse(&details.patch).unwrap_or_default();
            self.changes_diff
                .update(cx, |state, cx| state.set_files(files, cx));
        }
        if let Some(sha) = &self.selected_commit
            && !details.commits.iter().any(|c| &c.sha == sha)
        {
            self.selected_commit = None;
            self.commit_diff
                .update(cx, |state, cx| state.set_files(Vec::new(), cx));
        }
        let first_commit = details.commits.first().map(|c| c.sha.clone());
        self.details = details;
        // Show the latest commit right away instead of an empty pane.
        if self.selected_commit.is_none()
            && let Some(sha) = first_commit
        {
            self.select_commit(sha, cx);
        }
        cx.notify();
    }

    fn select_worktree(&mut self, path: PathBuf, cx: &mut Context<Self>) {
        if self.selected.as_ref() == Some(&path) {
            return;
        }
        self.selected = Some(path);
        self.confirm_remove = None;
        self.selected_commit = None;
        self.details = Details::default();
        self.changes_diff
            .update(cx, |state, cx| state.set_files(Vec::new(), cx));
        self.commit_diff
            .update(cx, |state, cx| state.set_files(Vec::new(), cx));
        self.reload_details(cx);
        cx.notify();
    }

    fn select_commit(&mut self, sha: String, cx: &mut Context<Self>) {
        let Some(path) = self.selected.clone() else {
            return;
        };
        self.selected_commit = Some(sha.clone());
        self.commit_task = Some(cx.spawn(async move |this, cx| {
            let patch = cx
                .background_executor()
                .spawn(async move { sg_git::commit_patch(&path, &sha) })
                .await;
            this.update(cx, |this, cx| {
                let files = match patch {
                    Ok(p) => DiffFile::parse(&p).unwrap_or_default(),
                    Err(e) => {
                        this.flash(format!("{e}"), true, cx);
                        Vec::new()
                    }
                };
                this.commit_diff
                    .update(cx, |state, cx| state.set_files(files, cx));
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
        let path = model::default_worktree_path(&self.repo, &branch);
        let root = self.repo.root.clone();
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
        let branch = pr.head_branch.clone();
        let path = model::default_worktree_path(&self.repo, &branch);
        let root = self.repo.root.clone();
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
        let root = self.repo.root.clone();
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
        let theme = cx.theme();
        let working = self
            .worktrees
            .iter()
            .filter(|w| w.agent_state() == Some(AgentState::Working))
            .count();
        let waiting = self
            .worktrees
            .iter()
            .filter(|w| {
                matches!(
                    w.agent_state(),
                    Some(AgentState::NeedsAttention | AgentState::Finished)
                )
            })
            .count();
        TitleBar::new().child(
            h_flex()
                .w_full()
                .pr_2()
                .justify_between()
                .child(
                    h_flex()
                        .gap_2()
                        .child(div().font_weight(FontWeight::BOLD).child("SuperGIT"))
                        .child(
                            div()
                                .text_color(theme.muted_foreground)
                                .child(format!("· {}", self.repo.name())),
                        ),
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
            .w(px(320.))
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
        let agent = info.agent_state();

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
            Err(e) => meta.push(
                div()
                    .text_color(theme.danger)
                    .child(e.clone())
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
        let changes = info.changed_files();
        let sessions = info.sessions.len();
        let open_prs = self
            .prs
            .iter()
            .filter(|p| matches!(p.state, PrState::Open | PrState::Draft))
            .count();

        let content = match self.tab {
            MainTab::Changes => self.render_changes(&info, &theme, cx).into_any_element(),
            MainTab::History => self.render_history(&theme, cx).into_any_element(),
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

        let p1 = path.clone();
        let p3 = path.clone();
        let p4 = path.clone();
        let last_session = info.sessions.first().map(|s| {
            (
                s.id.clone(),
                s.start_cwd.clone().unwrap_or_else(|| path.clone()),
            )
        });

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
                            .label("New Claude")
                            .small()
                            .primary()
                            .tooltip(format!("Start `claude` here in {}", terminal.name()))
                            .on_click(cx.listener(move |this, _, _, cx| {
                                let r = launch::claude_new(terminal, &p1);
                                this.report(r, "Claude started in a new terminal", cx);
                            })),
                    )
                    .when_some(last_session, |el, (id, dir)| {
                        el.child(
                            Button::new("claude-resume")
                                .label("Resume last")
                                .small()
                                .tooltip("claude --resume <most recent session>")
                                .on_click(cx.listener(move |this, _, _, cx| {
                                    let r = launch::claude_resume(terminal, &dir, &id);
                                    this.report(r, "Resumed the last session", cx);
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

    fn render_changes(
        &self,
        info: &WorktreeInfo,
        theme: &gpui_kit::component::Theme,
        cx: &mut Context<Self>,
    ) -> impl IntoElement {
        let files = info
            .status
            .as_ref()
            .map(|s| s.files.clone())
            .unwrap_or_default();
        if files.is_empty() {
            return div()
                .size_full()
                .flex()
                .items_center()
                .justify_center()
                .text_color(theme.muted_foreground)
                .child("Working tree clean")
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
        h_flex()
            .size_full()
            .child(
                v_flex()
                    .id("change-list")
                    .w(px(300.))
                    .flex_none()
                    .h_full()
                    .overflow_y_scroll()
                    .border_r_1()
                    .border_color(theme.border)
                    .children(rows),
            )
            .child(
                div()
                    .flex_1()
                    .min_w_0()
                    .h_full()
                    .child(Diff::new(&self.changes_diff).line_number(true)),
            )
            .into_any_element()
    }

    fn render_history(
        &self,
        theme: &gpui_kit::component::Theme,
        cx: &mut Context<Self>,
    ) -> impl IntoElement {
        let rows = self.details.commits.iter().enumerate().map(|(ix, c)| {
            let selected = self.selected_commit.as_deref() == Some(c.sha.as_str());
            let sha = c.sha.clone();
            commit_row(ix, c, selected, theme)
                .on_click(cx.listener(move |this, _, _, cx| this.select_commit(sha.clone(), cx)))
        });
        h_flex()
            .size_full()
            .child(
                v_flex()
                    .id("commit-list")
                    .w(relative(0.45))
                    .flex_none()
                    .h_full()
                    .overflow_y_scroll()
                    .border_r_1()
                    .border_color(theme.border)
                    .children(rows),
            )
            .child(
                div()
                    .flex_1()
                    .min_w_0()
                    .h_full()
                    .when(self.selected_commit.is_none(), |el| {
                        el.flex()
                            .items_center()
                            .justify_center()
                            .text_color(theme.muted_foreground)
                            .child("Select a commit")
                    })
                    .when(self.selected_commit.is_some(), |el| {
                        el.child(Diff::new(&self.commit_diff).line_number(true))
                    }),
            )
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
                        .label("Start Claude here")
                        .primary()
                        .on_click(cx.listener(move |this, _, _, cx| {
                            let r = launch::claude_new(terminal, &path);
                            this.report(r, "Claude started in a new terminal", cx);
                        })),
                )
                .into_any_element();
        }
        let rows = info.sessions.iter().enumerate().map(|(ix, s)| {
            let id = s.id.clone();
            let path = s.start_cwd.clone().unwrap_or_else(|| path.clone());
            session_row(ix, s, theme).child(
                Button::new(("resume", ix))
                    .label("Resume")
                    .small()
                    .ghost()
                    .on_click(cx.listener(move |this, _, _, cx| {
                        let r = launch::claude_resume(terminal, &path, &id);
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
            .child(format!("{}", self.repo.root.display()))
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
            .child(self.render_title_bar(cx))
            .child(
                h_flex()
                    .flex_1()
                    .min_h_0()
                    .child(self.render_sidebar(cx))
                    .child(self.render_main(window, cx)),
            )
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
