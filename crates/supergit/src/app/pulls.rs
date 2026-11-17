//! The Pull requests tab: the selected worktree's pull requests (or every
//! one), searchable, beside one PR's description, conversation, review
//! threads, checks and merge box. Merging always asks first.
//!
//! A PR belongs to a worktree when its head is the worktree's branch (or
//! the branch it pushes to) in the same repository, or when it was linked
//! by hand; a hand-made unlink overrides both.

mod checks;

use std::time::Instant;

use gpui_kit::component::menu::DropdownMenu as _;
use gpui_kit::component::text::TextView;
use sg_git::difftastic::Source;
use sg_github::{
    Check, CheckStatus, Comment, MergeMethod, MergeStatus, PrCommit, PrDetails, Review,
    ReviewState, ReviewThread,
};

use super::*;

mod publish;

/// Open PRs change while you look at them (checks, reviews, pushes).
const OPEN_DETAILS_REFRESH: Duration = Duration::from_secs(20);
const CLOSED_DETAILS_REFRESH: Duration = Duration::from_secs(300);
const WORKTREE_REFRESH: Duration = Duration::from_secs(60);
/// A failed diff is retried this often while its tab is open.
const PATCH_RETRY: Duration = Duration::from_secs(60);
const SEARCH_DELAY: Duration = Duration::from_millis(400);
const BRANCH_LIMIT: usize = 50;
const SEARCH_LIMIT: usize = 50;

mod review_actions;
mod review_context;

/// What a worktree's own PR list was fetched for.
#[derive(Clone, PartialEq, Eq)]
struct FetchKey {
    heads: Vec<String>,
    linked: Vec<u64>,
}

struct Fetched {
    key: FetchKey,
    at: Instant,
    prs: LoadState<Vec<PullRequest>>,
}

struct Loaded {
    at: Instant,
    details: LoadState<PrDetails>,
}

/// A PR's patch, taken at `head`: a push makes it stale.
pub(super) struct Patch {
    head: String,
    at: Instant,
    /// GitHub refused (too large, offline): computed from local commits.
    local: bool,
    /// Where the changes start, when the commits are local (difftastic
    /// reads both sides from them).
    merge_base: Option<String>,
    patch: LoadState<String>,
    /// The submodules the patch moves, inlined from local checkouts.
    submodules: Vec<sg_git::SubmoduleChange>,
}

/// The details column's views, as on GitHub.
#[derive(Clone, Copy, PartialEq, Eq)]
enum DetailsTab {
    Conversation,
    Commits,
    Files,
}

struct MergeDialog {
    number: u64,
    method: MergeMethod,
    running: bool,
    error: Option<String>,
    previous_focus: Option<FocusHandle>,
}

pub(super) struct Pulls {
    search: Entity<InputState>,
    layout: Entity<ResizableState>,
    /// Worktrees whose tab shows every PR (`true`) or only their own, as
    /// chosen by hand. Unchosen: their own, or all when they have none.
    show_all: HashMap<PathBuf, bool>,
    /// The PR each worktree shows.
    selected: HashMap<PathBuf, u64>,
    /// PRs fetched for a worktree: every one from its branches, plus links.
    worktree: HashMap<PathBuf, Fetched>,
    worktree_task: Option<(PathBuf, Task<()>)>,
    /// GitHub search for a query: older PRs the repository list lacks.
    search_results: Option<(String, LoadState<Vec<PullRequest>>)>,
    search_task: Option<Task<()>>,
    details: HashMap<u64, Loaded>,
    details_task: Option<(u64, Task<()>)>,
    commits: HashMap<(u64, String), LoadState<Vec<PrCommit>>>,
    commits_task: Option<((u64, String), Task<()>)>,
    /// Review threads shown the other way round from their default
    /// (resolved ones start collapsed).
    toggled_threads: HashSet<String>,
    thread_actions: review_actions::ThreadActions,
    show_passed_checks: bool,
    check_inspection: Option<checks::OpenCheck>,
    check_inspection_task: Option<Task<()>>,
    /// The details column; back to the top when another PR is shown.
    details_scroll: ScrollHandle,
    shown: Option<u64>,
    tab: DetailsTab,
    /// The Files tab: patches by PR, the diff view and its file list.
    patches: HashMap<u64, Patch>,
    patch_task: Option<(u64, Task<()>)>,
    pub(super) diff: Entity<DiffState>,
    /// The same diff through difftastic, while it is on.
    pub(super) difft: Entity<DifftView>,
    /// The `(number, head)` whose patch `diff` holds.
    diff_shown: Option<(u64, String)>,
    pub(super) files_filter: Entity<InputState>,
    pub(super) files_collapsed: HashSet<String>,
    files_layout: Entity<ResizableState>,
    /// A review thread's file and line to show once the diff is ready.
    pending_jump: Option<(u64, String, Option<u64>)>,
    merge: Option<MergeDialog>,
    publish: Option<publish::PublishDialog>,
    publish_task: Option<Task<()>>,
    #[cfg(feature = "screenshot")]
    debug_publish_opened: bool,
    merge_task: Option<Task<()>>,
    merge_handle: FocusHandle,
    _subscriptions: [Subscription; 2],
}

impl Pulls {
    pub(super) fn new(
        window: &mut Window,
        layout: DiffLayout,
        cx: &mut Context<SuperGitApp>,
    ) -> Self {
        let search = cx.new(|cx| {
            InputState::new(window, cx).placeholder("Search: #number, title, branch, @author")
        });
        let subscription = cx.subscribe_in(
            &search,
            window,
            |this: &mut SuperGitApp, _, event: &InputEvent, _, cx| {
                if matches!(event, InputEvent::Change) {
                    this.pr_query_changed(cx);
                }
            },
        );
        let files_filter = cx.new(|cx| InputState::new(window, cx).placeholder("Filter files"));
        let filter_subscription = cx.subscribe(&files_filter, |_, _, event: &InputEvent, cx| {
            if matches!(event, InputEvent::Change) {
                cx.notify();
            }
        });
        Self {
            search,
            layout: cx.new(|_| ResizableState::default()),
            show_all: HashMap::new(),
            selected: HashMap::new(),
            worktree: HashMap::new(),
            worktree_task: None,
            search_results: None,
            search_task: None,
            details: HashMap::new(),
            details_task: None,
            commits: HashMap::new(),
            commits_task: None,
            toggled_threads: HashSet::new(),
            thread_actions: review_actions::ThreadActions::default(),
            show_passed_checks: false,
            check_inspection: None,
            check_inspection_task: None,
            details_scroll: ScrollHandle::new(),
            shown: None,
            // Screenshot builds: `SUPERGIT_DEBUG_PR_FILES=1` starts on Files
            // (a background window only renders for the screenshot itself).
            tab: if cfg!(feature = "screenshot")
                && std::env::var_os("SUPERGIT_DEBUG_PR_COMMITS").is_some()
            {
                DetailsTab::Commits
            } else if cfg!(feature = "screenshot")
                && std::env::var_os("SUPERGIT_DEBUG_PR_FILES").is_some()
            {
                DetailsTab::Files
            } else {
                DetailsTab::Conversation
            },
            patches: HashMap::new(),
            patch_task: None,
            diff: new_diff_state(layout, cx),
            difft: cx.new(|_| DifftView::new(diff_mode(layout))),
            diff_shown: None,
            files_filter,
            files_collapsed: HashSet::new(),
            files_layout: cx.new(|_| ResizableState::default()),
            pending_jump: None,
            merge: None,
            publish: None,
            publish_task: None,
            #[cfg(feature = "screenshot")]
            debug_publish_opened: false,
            merge_task: None,
            merge_handle: cx.focus_handle(),
            _subscriptions: [subscription, filter_subscription],
        }
    }

    /// Another repository: PR numbers mean other PRs now. A running merge
    /// keeps going and still reports its result.
    pub(super) fn reset(&mut self, cx: &mut App) {
        self.layout = cx.new(|_| ResizableState::default());
        self.show_all.clear();
        self.selected.clear();
        self.worktree.clear();
        self.worktree_task = None;
        self.search_results = None;
        self.search_task = None;
        self.details.clear();
        self.details_task = None;
        self.commits.clear();
        self.commits_task = None;
        self.toggled_threads.clear();
        self.close_check_inspection();
        self.merge = None;
        self.publish = None;
        self.patches.clear();
        self.patch_task = None;
        self.diff_shown = None;
        self.diff
            .update(cx, |diff, cx| diff.set_files(Vec::new(), cx));
        self.difft.update(cx, |view, cx| view.clear(cx));
        self.files_collapsed.clear();
        self.files_layout = cx.new(|_| ResizableState::default());
        self.pending_jump = None;
    }

    pub(super) fn dialog_open(&self) -> bool {
        self.merge.is_some() || self.publish.is_some()
    }
}

/// Branch names whose PRs come from this worktree: its branch and the one
/// it pushes to (`origin/x` → `x`).
fn worktree_heads(info: &WorktreeInfo) -> Vec<String> {
    let mut heads: Vec<String> = info.wt.branch.iter().cloned().collect();
    if let Ok(status) = &info.status
        && let Some((_, name)) = status.upstream.as_deref().and_then(|u| u.split_once('/'))
        && !name.is_empty()
        && !heads.iter().any(|h| h == name)
    {
        heads.push(name.to_string());
    }
    heads
}

/// Search as you type: `#12`/`12` matches numbers, other words must all
/// appear in the title, branches or `@author`.
fn pr_matches(pr: &PullRequest, query: &str) -> bool {
    let query = query.trim();
    if query.is_empty() {
        return true;
    }
    if let Some(number) = pr_number_query(query) {
        return pr.number.to_string().starts_with(&number.to_string());
    }
    let haystack = format!(
        "{} {} {} @{}",
        pr.title, pr.head_branch, pr.base_branch, pr.author
    )
    .to_lowercase();
    query
        .to_lowercase()
        .split_whitespace()
        .all(|word| haystack.contains(word))
}

/// Worth asking GitHub about: a number, or at least two characters.
fn searchable(query: &str) -> bool {
    pr_number_query(query).is_some() || query.trim().chars().count() >= 2
}

fn pr_number_query(query: &str) -> Option<u64> {
    let digits = query.trim().trim_start_matches('#');
    if digits.is_empty() || !digits.chars().all(|c| c.is_ascii_digit()) {
        return None;
    }
    digits.parse().ok()
}

/// Add `more` to `prs`, replacing older copies of the same PR.
fn merge_prs(prs: &mut Vec<PullRequest>, more: impl IntoIterator<Item = PullRequest>) {
    for pr in more {
        match prs.iter_mut().find(|p| p.number == pr.number) {
            Some(existing) => *existing = pr,
            None => prs.push(pr),
        }
    }
}

/// Open first, then newest.
fn sort_prs(prs: &mut [PullRequest]) {
    prs.sort_by_key(|pr| (!pr.is_open(), std::cmp::Reverse(pr.number)));
}

/// `2h ago`, `just now`, or nothing.
fn ago(time: Option<SystemTime>) -> String {
    match time.map(time_ago) {
        Some(ago) if ago == "now" => "just now".into(),
        Some(ago) if !ago.is_empty() => format!("{ago} ago"),
        _ => String::new(),
    }
}

/// GitHub bodies carry template comments (`<!-- … -->`) the page hides.
fn visible_markdown(body: &str) -> String {
    let mut out = String::with_capacity(body.len());
    let mut rest = body;
    while let Some(start) = rest.find("<!--") {
        out.push_str(&rest[..start]);
        match rest[start..].find("-->") {
            Some(end) => rest = &rest[start + end + 3..],
            None => {
                rest = "";
            }
        }
    }
    out.push_str(rest);
    out.trim().to_string()
}

fn duration(check: &Check) -> Option<String> {
    let took = check.completed_at?.duration_since(check.started_at?).ok()?;
    let secs = took.as_secs();
    Some(if secs < 60 {
        format!("{secs}s")
    } else if secs < 3_600 {
        format!("{}m {}s", secs / 60, secs % 60)
    } else {
        format!("{}h {}m", secs / 3_600, secs % 3_600 / 60)
    })
}

fn check_glyph(status: CheckStatus, theme: &gpui_kit::component::Theme) -> (&'static str, Hsla) {
    match status {
        CheckStatus::Failed => ("✗", theme.danger),
        CheckStatus::Pending => ("●", theme.warning),
        CheckStatus::Passed => ("✓", theme.success),
        CheckStatus::Skipped => ("–", theme.muted_foreground),
    }
}

/// The list's checks column: the worst state wins.
fn checks_glyph(
    checks: &sg_github::Checks,
    theme: &gpui_kit::component::Theme,
) -> Option<(&'static str, Hsla)> {
    if checks.failed > 0 {
        Some(check_glyph(CheckStatus::Failed, theme))
    } else if checks.pending > 0 {
        Some(check_glyph(CheckStatus::Pending, theme))
    } else if checks.total() > 0 {
        Some(check_glyph(CheckStatus::Passed, theme))
    } else {
        None
    }
}

fn review_decision(
    decision: &str,
    theme: &gpui_kit::component::Theme,
) -> Option<(&'static str, Hsla)> {
    match decision {
        "APPROVED" => Some(("Approved", theme.success)),
        "CHANGES_REQUESTED" => Some(("Changes requested", theme.danger)),
        "REVIEW_REQUIRED" => Some(("Review required", theme.muted_foreground)),
        _ => None,
    }
}

fn checks_summary(checks: &sg_github::Checks) -> String {
    let mut parts = Vec::new();
    if checks.failed > 0 {
        parts.push(format!("{} failing", checks.failed));
    }
    if checks.pending > 0 {
        parts.push(format!("{} pending", checks.pending));
    }
    if checks.passed > 0 {
        parts.push(format!("{} passed", checks.passed));
    }
    parts.join(", ")
}

fn merge_status_color(status: MergeStatus, theme: &gpui_kit::component::Theme) -> Hsla {
    match status {
        MergeStatus::Clean | MergeStatus::HasHooks => theme.success,
        MergeStatus::Unstable | MergeStatus::Behind | MergeStatus::Blocked => theme.warning,
        MergeStatus::Conflicts => theme.danger,
        MergeStatus::Draft | MergeStatus::Unknown => theme.muted_foreground,
    }
}

/// One entry of the conversation, in time order.
enum Activity<'a> {
    Comment(&'a Comment),
    Review(&'a Review),
    Thread(&'a ReviewThread),
}

impl Activity<'_> {
    fn time(&self) -> Option<SystemTime> {
        match self {
            Self::Comment(c) => c.created_at,
            Self::Review(r) => r.submitted_at,
            Self::Thread(t) => t.created_at(),
        }
    }
}

fn activity(details: &PrDetails) -> Vec<Activity<'_>> {
    let mut items: Vec<Activity<'_>> = details.comments.iter().map(Activity::Comment).collect();
    // A plain "reviewed" with no text is just the envelope of its threads.
    items.extend(
        details
            .reviews
            .iter()
            .filter(|r| !(r.state == ReviewState::Commented && r.body.trim().is_empty()))
            .filter(|r| r.state != ReviewState::Pending)
            .map(Activity::Review),
    );
    items.extend(details.threads.iter().map(Activity::Thread));
    items.sort_by_key(Activity::time);
    items
}

impl SuperGitApp {
    // ---- which PRs ---------------------------------------------------------

    fn worktree_view(&self, path: &Path) -> Option<&workspace::WorktreeView> {
        self.repository_view().worktrees.get(path)
    }

    fn fetch_key(&self, info: &WorktreeInfo) -> FetchKey {
        let mut linked = self
            .worktree_view(info.path())
            .map(|view| view.linked_prs.clone())
            .unwrap_or_default();
        linked.sort_unstable();
        FetchKey {
            heads: worktree_heads(info),
            linked,
        }
    }

    /// Whether `pr` came from (or was linked to) this worktree.
    fn pr_linked(&self, info: &WorktreeInfo, pr: &PullRequest) -> bool {
        let view = self.worktree_view(info.path());
        if view.is_some_and(|v| v.unlinked_prs.contains(&pr.number)) {
            return false;
        }
        self.pr_linked_by_hand(info, pr.number) || self.pr_from_branch(info, pr)
    }

    fn pr_linked_by_hand(&self, info: &WorktreeInfo, number: u64) -> bool {
        self.worktree_view(info.path())
            .is_some_and(|v| v.linked_prs.contains(&number))
    }

    fn pr_from_branch(&self, info: &WorktreeInfo, pr: &PullRequest) -> bool {
        if !sg_github::head_in_same_repository(pr) {
            return false;
        }
        if worktree_heads(info).contains(&pr.head_branch) {
            return true;
        }
        // A detached worktree sitting exactly on a PR's head.
        info.wt.branch.is_none()
            && !pr.head_oid.is_empty()
            && info.wt.head.as_deref() == Some(pr.head_oid.as_str())
    }

    /// This worktree's PRs, open first.
    fn worktree_prs(&self, info: &WorktreeInfo) -> Vec<PullRequest> {
        let mut prs: Vec<PullRequest> = Vec::new();
        if let Some(fetched) = self
            .pulls
            .worktree
            .get(info.path())
            .and_then(|f| f.prs.value())
        {
            merge_prs(&mut prs, fetched.iter().cloned());
        }
        // The repository list refreshes on its own schedule: newer copies.
        merge_prs(&mut prs, self.prs.iter().cloned());
        prs.retain(|pr| self.pr_linked(info, pr));
        sort_prs(&mut prs);
        prs
    }

    /// The worktree's own list is still on its way.
    fn worktree_prs_loading(&self, info: &WorktreeInfo) -> bool {
        let key = self.fetch_key(info);
        let fetched = self
            .pulls
            .worktree
            .get(info.path())
            .is_some_and(|f| !f.prs.is_loading());
        (!key.heads.is_empty() || !key.linked.is_empty()) && !fetched
    }

    fn shows_all_prs(&self, info: &WorktreeInfo) -> bool {
        match self.pulls.show_all.get(info.path()) {
            Some(all) => *all,
            None => {
                !self.worktree_prs_loading(info)
                    && self.prs_loaded
                    && self.worktree_prs(info).is_empty()
            }
        }
    }

    fn pr_query(&self, cx: &App) -> String {
        self.pulls.search.read(cx).value().trim().to_string()
    }

    /// The list as shown: the scope, filtered by the search; in "All",
    /// GitHub's results for the query follow the local matches.
    fn visible_prs(&self, info: &WorktreeInfo, cx: &App) -> Vec<PullRequest> {
        let query = self.pr_query(cx);
        if !self.shows_all_prs(info) {
            let mut prs = self.worktree_prs(info);
            prs.retain(|pr| pr_matches(pr, &query));
            return prs;
        }
        let mut prs: Vec<PullRequest> = self
            .prs
            .iter()
            .filter(|pr| pr_matches(pr, &query))
            .cloned()
            .collect();
        if let Some((searched, results)) = &self.pulls.search_results
            && *searched == query
            && let Some(results) = results.value()
        {
            for pr in results {
                if !prs.iter().any(|p| p.number == pr.number) {
                    prs.push(pr.clone());
                }
            }
        }
        prs
    }

    fn selected_pr_number(&self, path: &Path, visible: &[PullRequest]) -> Option<u64> {
        self.pulls
            .selected
            .get(path)
            .copied()
            .filter(|n| visible.iter().any(|pr| pr.number == *n))
            .or_else(|| visible.first().map(|pr| pr.number))
    }

    // ---- loading -------------------------------------------------------------

    /// Fetch what the tab shows and is missing or stale: the worktree's
    /// own PRs, GitHub search results, the selected PR's details. Cheap
    /// when nothing is due; called on every render of the tab.
    pub(super) fn ensure_pulls(&mut self, cx: &mut Context<Self>) {
        if self.tab != MainTab::PullRequests {
            return;
        }
        let Some(root) = self.active_root().map(Path::to_path_buf) else {
            return;
        };
        let Some(info) = self.selected_info().cloned() else {
            return;
        };
        #[cfg(feature = "screenshot")]
        {
            self.debug_pr_once(&info, cx);
            self.debug_publish_dialog(info.wt.path.clone(), cx);
        }
        let path = info.wt.path.clone();
        let key = self.fetch_key(&info);
        let due = match self.pulls.worktree.get(&path) {
            None => true,
            Some(fetched) => fetched.key != key || fetched.at.elapsed() > WORKTREE_REFRESH,
        };
        let running = self
            .pulls
            .worktree_task
            .as_ref()
            .is_some_and(|(p, _)| *p == path);
        if due && !running && (!key.heads.is_empty() || !key.linked.is_empty()) {
            self.fetch_worktree_prs(root.clone(), path.clone(), key, cx);
        }

        let query = self.pr_query(cx);
        let searched = self
            .pulls
            .search_results
            .as_ref()
            .is_some_and(|(q, _)| *q == query);
        if self.shows_all_prs(&info) && !searched && self.pulls.search_task.is_none() {
            self.run_pr_search(cx);
        }

        let visible = self.visible_prs(&info, cx);
        let selected = self.selected_pr_number(&path, &visible);
        if self.pulls.shown != selected {
            self.pulls.shown = selected;
            self.pulls.close_check_inspection();
            self.pulls.details_scroll.set_offset(point(px(0.), px(0.)));
        }
        if let Some(number) = selected {
            let listed = visible.iter().find(|pr| pr.number == number);
            let open = listed.is_some_and(PullRequest::is_open);
            self.ensure_pr_details(root.clone(), number, open, cx);
            // The details carry the freshest head (a push changes the diff)
            // and the base commit the diff starts from: wait for them,
            // unless they failed.
            let loaded = self.pulls.details.get(&number);
            let target = match loaded.and_then(|l| l.details.value()) {
                Some(details) => Some((details.pr.clone(), details.base_oid.clone())),
                None if loaded.is_some_and(|l| l.details.error().is_some()) => {
                    listed.map(|pr| (pr.clone(), String::new()))
                }
                None => None,
            };
            if let Some((pr, base_oid)) = target {
                match self.pulls.tab {
                    DetailsTab::Files => self.ensure_pr_patch(root, &pr, base_oid, cx),
                    DetailsTab::Commits => self.ensure_pr_commits(root, &pr, cx),
                    DetailsTab::Conversation => {}
                }
            }
            self.show_pr_diff(number, cx);
        }
    }

    fn ensure_pr_commits(&mut self, root: PathBuf, pr: &PullRequest, cx: &mut Context<Self>) {
        let key = (pr.number, pr.head_oid.clone());
        if self.pulls.commits.contains_key(&key)
            || self
                .pulls
                .commits_task
                .as_ref()
                .is_some_and(|(pending, _)| *pending == key)
        {
            return;
        }
        let request = key.clone();
        let task = cx.spawn(async move |this, cx| {
            let result = cx
                .background_executor()
                .spawn(async move {
                    sg_github::pr_commits(&root, request.0, &request.1)
                        .map_err(|error| format!("{error:#}"))
                })
                .await;
            let _ = this.update(cx, |this, cx| {
                this.pulls.commits_task = None;
                this.pulls.commits.retain(|(number, _), _| *number != key.0);
                this.pulls
                    .commits
                    .insert(key, LoadState::from_result(result));
                cx.notify();
            });
        });
        self.pulls.commits_task = Some(((pr.number, pr.head_oid.clone()), task));
    }

    fn ensure_pr_patch(
        &mut self,
        root: PathBuf,
        pr: &PullRequest,
        base_oid: String,
        cx: &mut Context<Self>,
    ) {
        let number = pr.number;
        let due = match self.pulls.patches.get(&number) {
            None => true,
            Some(p) => {
                p.head != pr.head_oid || (p.patch.error().is_some() && p.at.elapsed() > PATCH_RETRY)
            }
        };
        let running = self
            .pulls
            .patch_task
            .as_ref()
            .is_some_and(|(n, _)| *n == number);
        if !due || running {
            return;
        }
        let head = pr.head_oid.clone();
        let base = pr.base_branch.clone();
        let request = (root.clone(), head.clone());
        let task = cx.spawn(async move |this, cx| {
            let (result, merge_base) = cx
                .background_executor()
                .spawn(async move {
                    let (root, head) = request;
                    // The PR's diff is its head against this merge base:
                    // the local repository often has both commits.
                    let merge_base = sg_git::pr_merge_base(&root, &base_oid, &base, &head);
                    let result = match sg_github::pr_diff(&root, number) {
                        Ok(patch) => Ok((sg_git::inline_submodules(&root, &patch, None), false)),
                        Err(remote) => match &merge_base {
                            Ok(from) => sg_git::range_patch(&root, from, &head)
                                .map(|patch| (patch, true))
                                .map_err(|local| format!("{remote:#}\nLocal diff: {local:#}")),
                            Err(local) => Err(format!("{remote:#}\nLocal diff: {local:#}")),
                        },
                    };
                    (result, merge_base.ok())
                })
                .await;
            this.update(cx, |this, cx| {
                if this.active_root() != Some(root.as_path()) {
                    return;
                }
                if this
                    .pulls
                    .patch_task
                    .as_ref()
                    .is_some_and(|(n, _)| *n == number)
                {
                    this.pulls.patch_task = None;
                }
                let previous = this
                    .pulls
                    .patches
                    .remove(&number)
                    .filter(|p| p.head == head);
                let local = match (&result, &previous) {
                    (Ok((_, local)), _) => *local,
                    (Err(_), Some(previous)) => previous.local,
                    (Err(_), None) => false,
                };
                let (mut patch, mut submodules) = previous
                    .map(|p| (p.patch, p.submodules))
                    .unwrap_or_default();
                if let Ok((loaded, _)) = &result {
                    submodules = loaded.submodules.clone();
                }
                patch.update(LoadState::from_result(result.map(|(patch, _)| patch.text)));
                this.pulls.patches.insert(
                    number,
                    Patch {
                        head,
                        at: Instant::now(),
                        local,
                        merge_base,
                        patch,
                        submodules,
                    },
                );
                this.show_pr_diff(number, cx);
                cx.notify();
            })
            .ok();
        });
        self.pulls.patch_task = Some((number, task));
    }

    /// Put PR `number`'s patch into the diff view (once per head), then
    /// scroll to a review thread's line if one was asked for.
    fn show_pr_diff(&mut self, number: u64, cx: &mut Context<Self>) {
        if self.pulls.shown != Some(number) {
            return;
        }
        let Some(entry) = self.pulls.patches.get_mut(&number) else {
            return;
        };
        let key = (number, entry.head.clone());
        if self.pulls.diff_shown.as_ref() != Some(&key) {
            let Some(patch) = entry.patch.value() else {
                return;
            };
            match crate::syntax::parse_patch(patch) {
                Ok(files) => {
                    self.pulls
                        .diff
                        .update(cx, |diff, cx| diff.set_files(files, cx));
                    self.pulls.files_collapsed.clear();
                }
                Err(error) => {
                    entry.patch = LoadState::Failed {
                        error: format!("Cannot display the diff: {error}"),
                        previous: None,
                    };
                    self.pulls
                        .diff
                        .update(cx, |diff, cx| diff.set_files(Vec::new(), cx));
                }
            }
            self.pulls.diff_shown = Some(key);
            self.refresh_difft(FileList::Pull, cx);
        }
        if let Some((_, path, line)) = self.pulls.pending_jump.take_if(|(n, _, _)| *n == number) {
            self.pulls
                .difft
                .update(cx, |view, cx| view.scroll_to_file(&path, cx));
            self.pulls.diff.update(cx, |diff, cx| match line {
                Some(line) => diff.scroll_to_line(
                    DiffLinePosition::new(path, DiffSide::Modified, line as usize),
                    cx,
                ),
                None => diff.scroll_to_file(&path, cx),
            });
        }
    }

    /// What difftastic compares for the shown PR diff: its patch and both
    /// sides as local commits, when they are here.
    pub(super) fn pr_difft_parts(&self) -> Option<(&String, Source, Source)> {
        let (number, head) = self.pulls.diff_shown.as_ref()?;
        let entry = self
            .pulls
            .patches
            .get(number)
            .filter(|entry| entry.head == *head)?;
        let from = entry.merge_base.clone()?;
        Some((
            entry.patch.value()?,
            Source::Revision(from),
            Source::Revision(head.clone()),
        ))
    }

    /// The submodules the shown PR diff moves.
    pub(super) fn pr_submodules(&self) -> &[sg_git::SubmoduleChange] {
        self.pulls
            .diff_shown
            .as_ref()
            .and_then(|(number, head)| {
                self.pulls
                    .patches
                    .get(number)
                    .filter(|entry| entry.head == *head)
            })
            .map_or(&[], |entry| entry.submodules.as_slice())
    }

    /// Difftastic is on but cannot show this PR: its commits are not local.
    pub(super) fn pr_difft_unavailable(&self) -> bool {
        self.difftastic_active() && self.pr_difft_parts().is_none()
    }

    /// The Files tab's diff as agent context: the PR and the head it shows.
    pub(super) fn pr_diff_context(
        &self,
    ) -> anyhow::Result<(&Entity<DiffState>, &LoadState<String>, String)> {
        let (number, head) = self
            .pulls
            .diff_shown
            .as_ref()
            .filter(|_| self.pulls.tab == DetailsTab::Files)
            .ok_or_else(|| anyhow::anyhow!("Open the pull request's Files tab first"))?;
        let entry = self
            .pulls
            .patches
            .get(number)
            .filter(|entry| entry.head == *head)
            .ok_or_else(|| anyhow::anyhow!("Wait for the pull request's diff"))?;
        Ok((
            &self.pulls.diff,
            &entry.patch,
            format!(
                "Pull request #{number} at {}{}",
                &head[..head.len().min(8)],
                if entry.local {
                    " (computed locally)"
                } else {
                    ""
                }
            ),
        ))
    }

    fn fetch_worktree_prs(
        &mut self,
        root: PathBuf,
        path: PathBuf,
        key: FetchKey,
        cx: &mut Context<Self>,
    ) {
        let request = key.clone();
        let known: Vec<u64> = self.prs.iter().map(|pr| pr.number).collect();
        let request_root = root.clone();
        let task_path = path.clone();
        let task = cx.spawn(async move |this, cx| {
            let result = cx
                .background_executor()
                .spawn(async move {
                    let mut prs: Vec<PullRequest> = Vec::new();
                    let mut error = None;
                    for head in &request.heads {
                        match sg_github::list_branch_prs(&request_root, head, BRANCH_LIMIT) {
                            Ok(found) => merge_prs(&mut prs, found),
                            Err(e) => error = Some(format!("{e:#}")),
                        }
                    }
                    for &number in &request.linked {
                        if known.contains(&number) || prs.iter().any(|p| p.number == number) {
                            continue;
                        }
                        match sg_github::view_pr(&request_root, number) {
                            Ok(pr) => prs.push(pr),
                            Err(e) => error = Some(format!("{e:#}")),
                        }
                    }
                    match error {
                        Some(error) if prs.is_empty() => Err(error),
                        _ => Ok(prs),
                    }
                })
                .await;
            this.update(cx, |this, cx| {
                if this.active_root() != Some(root.as_path()) {
                    return;
                }
                if this
                    .pulls
                    .worktree_task
                    .as_ref()
                    .is_some_and(|(p, _)| *p == path)
                {
                    this.pulls.worktree_task = None;
                }
                let mut prs = this
                    .pulls
                    .worktree
                    .remove(&path)
                    .filter(|f| f.key == key)
                    .map(|f| f.prs)
                    .unwrap_or_default();
                prs.update(LoadState::from_result(result));
                this.pulls.worktree.insert(
                    path,
                    Fetched {
                        key,
                        at: Instant::now(),
                        prs,
                    },
                );
                cx.notify();
            })
            .ok();
        });
        self.pulls.worktree_task = Some((task_path, task));
    }

    fn pr_query_changed(&mut self, cx: &mut Context<Self>) {
        // Local matches update at once; GitHub is asked once typing pauses.
        self.pulls.search_task = Some(cx.spawn(async move |this, cx| {
            cx.background_executor().timer(SEARCH_DELAY).await;
            this.update(cx, |this, cx| {
                this.pulls.search_task = None;
                this.run_pr_search(cx);
            })
            .ok();
        }));
        cx.notify();
    }

    /// Ask GitHub for PRs matching the query beyond the loaded list: a
    /// number fetches that PR, words run GitHub's search.
    fn run_pr_search(&mut self, cx: &mut Context<Self>) {
        let query = self.pr_query(cx);
        let Some(root) = self.active_root().map(Path::to_path_buf) else {
            return;
        };
        let number = pr_number_query(&query);
        let all = self
            .selected_info()
            .is_some_and(|info| self.shows_all_prs(info));
        if !all || !searchable(&query) {
            self.pulls.search_results = None;
            self.pulls.search_task = None;
            return;
        }
        if number.is_some_and(|n| self.prs.iter().any(|pr| pr.number == n)) {
            self.pulls.search_results = Some((query, LoadState::Ready(Vec::new())));
            return;
        }
        self.pulls.search_results = Some((query.clone(), LoadState::Loading));
        let request = query.clone();
        self.pulls.search_task = Some(cx.spawn(async move |this, cx| {
            let request_root = root.clone();
            let result = cx
                .background_executor()
                .spawn(async move {
                    match number {
                        // A number that does not exist is just no result.
                        Some(number) => Ok(sg_github::view_pr(&request_root, number)
                            .into_iter()
                            .collect()),
                        None => sg_github::search_prs(&request_root, &request, SEARCH_LIMIT)
                            .map_err(|e| format!("{e:#}")),
                    }
                })
                .await;
            this.update(cx, |this, cx| {
                if this.active_root() != Some(root.as_path()) || this.pr_query(cx) != query {
                    return;
                }
                this.pulls.search_task = None;
                this.pulls.search_results = Some((query, LoadState::from_result(result)));
                cx.notify();
            })
            .ok();
        }));
    }

    fn ensure_pr_details(
        &mut self,
        root: PathBuf,
        number: u64,
        open: bool,
        cx: &mut Context<Self>,
    ) {
        let refresh = if open {
            OPEN_DETAILS_REFRESH
        } else {
            CLOSED_DETAILS_REFRESH
        };
        let fresh = self
            .pulls
            .details
            .get(&number)
            .is_some_and(|loaded| loaded.at.elapsed() < refresh);
        let running = self
            .pulls
            .details_task
            .as_ref()
            .is_some_and(|(n, _)| *n == number);
        if fresh || running {
            return;
        }
        self.load_pr_details(root, number, cx);
    }

    fn load_pr_details(&mut self, root: PathBuf, number: u64, cx: &mut Context<Self>) {
        let task = cx.spawn(async move |this, cx| {
            let request = root.clone();
            let result = cx
                .background_executor()
                .spawn(async move {
                    sg_github::pr_details(&request, number).map_err(|e| format!("{e:#}"))
                })
                .await;
            this.update(cx, |this, cx| {
                if this.active_root() != Some(root.as_path()) {
                    return;
                }
                if this
                    .pulls
                    .details_task
                    .as_ref()
                    .is_some_and(|(n, _)| *n == number)
                {
                    this.pulls.details_task = None;
                }
                if let Ok(details) = &result {
                    this.refresh_listed_pr(&details.pr);
                }
                let loaded = this.pulls.details.entry(number).or_insert(Loaded {
                    at: Instant::now(),
                    details: LoadState::Loading,
                });
                loaded.at = Instant::now();
                loaded.details.update(LoadState::from_result(result));
                #[cfg(feature = "screenshot")]
                this.debug_check_inspection(number, cx);
                // The Files tab waits for the head and base commit: start
                // its diff now rather than at the next frame.
                if this.pulls.tab == DetailsTab::Files
                    && this.pulls.shown == Some(number)
                    && let Some(details) = this
                        .pulls
                        .details
                        .get(&number)
                        .and_then(|l| l.details.value())
                {
                    let (pr, base_oid) = (details.pr.clone(), details.base_oid.clone());
                    this.ensure_pr_patch(root.clone(), &pr, base_oid, cx);
                }
                cx.notify();
            })
            .ok();
        });
        self.pulls.details_task = Some((number, task));
    }

    /// Details are fresher than the lists: show the same state everywhere
    /// (a merge shows up at once, not at the next list refresh).
    fn refresh_listed_pr(&mut self, fresh: &PullRequest) {
        let lists = self
            .pulls
            .worktree
            .values_mut()
            .filter_map(|f| match &mut f.prs {
                LoadState::Ready(prs) => Some(prs),
                LoadState::Failed { previous, .. } => previous.as_mut(),
                LoadState::Loading => None,
            })
            .chain(
                self.pulls
                    .search_results
                    .iter_mut()
                    .filter_map(|(_, r)| match r {
                        LoadState::Ready(prs) => Some(prs),
                        _ => None,
                    }),
            )
            .chain(std::iter::once(&mut self.prs));
        for list in lists {
            if let Some(pr) = list.iter_mut().find(|pr| pr.number == fresh.number) {
                *pr = fresh.clone();
            }
        }
    }

    // ---- actions ---------------------------------------------------------------

    fn select_pr(&mut self, path: PathBuf, number: u64, cx: &mut Context<Self>) {
        self.pulls.selected.insert(path, number);
        cx.notify();
    }

    fn set_pr_scope(&mut self, path: PathBuf, all: bool, cx: &mut Context<Self>) {
        self.pulls.show_all.insert(path, all);
        if !all {
            self.pulls.search_results = None;
            self.pulls.search_task = None;
        }
        cx.notify();
    }

    /// Link `number` to the worktree at `path`, or undo the link (also one
    /// made by its branch).
    fn set_pr_link(&mut self, path: PathBuf, number: u64, link: bool, cx: &mut Context<Self>) {
        let Some(root) = self.active_root().map(Path::to_path_buf) else {
            return;
        };
        let name = self
            .worktrees
            .iter()
            .find(|info| info.path() == path)
            .map(|info| info.wt.display_name())
            .unwrap_or_default();
        let view = self
            .persisted
            .repositories
            .entry(root)
            .or_default()
            .worktrees
            .entry(path)
            .or_default();
        view.linked_prs.retain(|n| *n != number);
        view.unlinked_prs.retain(|n| *n != number);
        if link {
            view.linked_prs.push(number);
        } else {
            view.unlinked_prs.push(number);
        }
        self.queue_persist(cx);
        self.flash(
            if link {
                format!("Linked #{number} to {name}")
            } else {
                format!("Unlinked #{number} from {name}")
            },
            false,
            cx,
        );
    }

    fn set_details_tab(&mut self, tab: DetailsTab, cx: &mut Context<Self>) {
        self.pulls.tab = tab;
        cx.notify();
    }

    /// From a review thread to its line in the Files tab.
    fn jump_to_thread(
        &mut self,
        number: u64,
        path: String,
        line: Option<u64>,
        cx: &mut Context<Self>,
    ) {
        self.pulls.pending_jump = Some((number, path, line));
        self.pulls.tab = DetailsTab::Files;
        self.show_pr_diff(number, cx);
        cx.notify();
    }

    fn retry_pr_patch(&mut self, number: u64, cx: &mut Context<Self>) {
        self.pulls.patches.remove(&number);
        if self
            .pulls
            .diff_shown
            .as_ref()
            .is_some_and(|(n, _)| *n == number)
        {
            self.pulls.diff_shown = None;
        }
        cx.notify();
    }

    fn toggle_thread(&mut self, id: String, cx: &mut Context<Self>) {
        if !self.pulls.toggled_threads.remove(&id) {
            self.pulls.toggled_threads.insert(id);
        }
        cx.notify();
    }

    fn open_merge_dialog(&mut self, number: u64, window: &mut Window, cx: &mut Context<Self>) {
        let Some(details) = self
            .pulls
            .details
            .get(&number)
            .and_then(|loaded| loaded.details.value())
        else {
            self.flash("The pull request is still loading", true, cx);
            return;
        };
        let method = details.default_method;
        self.close_switcher(&CloseSwitcher, window, cx);
        self.pulls.merge = Some(MergeDialog {
            number,
            method,
            running: false,
            error: None,
            previous_focus: window.focused(cx),
        });
        self.pulls.merge_handle.focus(window, cx);
        cx.notify();
    }

    fn close_merge_dialog(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        if let Some(dialog) = self.pulls.merge.take() {
            dialog
                .previous_focus
                .unwrap_or_else(|| self.focus_handle.clone())
                .focus(window, cx);
            cx.notify();
        }
    }

    fn choose_merge_method(&mut self, method: MergeMethod, cx: &mut Context<Self>) {
        if let Some(dialog) = &mut self.pulls.merge
            && !dialog.running
        {
            dialog.method = method;
            cx.notify();
        }
    }

    /// Merge now, or (`auto`) let GitHub merge once requirements pass.
    /// Pinned to the head the dialog showed: a newer push makes it fail.
    fn confirm_merge(&mut self, auto: bool, window: &mut Window, cx: &mut Context<Self>) {
        let Some(root) = self.active_root().map(Path::to_path_buf) else {
            return;
        };
        let Some(dialog) = &mut self.pulls.merge else {
            return;
        };
        if dialog.running || self.pulls.merge_task.is_some() {
            return;
        }
        let (number, method) = (dialog.number, dialog.method);
        let Some(head) = self
            .pulls
            .details
            .get(&number)
            .and_then(|loaded| loaded.details.value())
            .map(|details| details.pr.head_oid.clone())
        else {
            return;
        };
        dialog.running = true;
        dialog.error = None;
        self.pulls.merge_task = Some(cx.spawn_in(window, async move |this, cx| {
            let request = root.clone();
            let result = cx
                .background_executor()
                .spawn(async move {
                    sg_github::merge_pr(&request, number, method, &head, auto)
                        .map_err(|e| format!("{e:#}"))
                })
                .await;
            this.update_in(cx, |this, window, cx| {
                this.pulls.merge_task = None;
                let here = this.active_root() == Some(root.as_path());
                match result {
                    Ok(()) => {
                        if here {
                            if this
                                .pulls
                                .merge
                                .as_ref()
                                .is_some_and(|d| d.number == number)
                            {
                                this.close_merge_dialog(window, cx);
                            }
                            this.pulls.details.remove(&number);
                            this.pulls.worktree.clear();
                            this.start_pr_polling(cx);
                        }
                        this.flash(
                            if auto {
                                format!("Auto-merge enabled for #{number}")
                            } else {
                                format!("Merged #{number}")
                            },
                            false,
                            cx,
                        );
                    }
                    Err(error) => match &mut this.pulls.merge {
                        Some(dialog) if here && dialog.number == number => {
                            dialog.running = false;
                            dialog.error = Some(error);
                            cx.notify();
                        }
                        _ => this.flash(format!("Merging #{number} failed: {error}"), true, cx),
                    },
                }
            })
            .ok();
        }));
        cx.notify();
    }

    fn disable_auto_merge(&mut self, number: u64, cx: &mut Context<Self>) {
        let Some(root) = self.active_root().map(Path::to_path_buf) else {
            return;
        };
        if self.pulls.merge_task.is_some() {
            return;
        }
        self.flash(format!("Disabling auto-merge for #{number}…"), false, cx);
        self.pulls.merge_task = Some(cx.spawn(async move |this, cx| {
            let request = root.clone();
            let result = cx
                .background_executor()
                .spawn(async move { sg_github::disable_auto_merge(&request, number) })
                .await;
            this.update(cx, |this, cx| {
                this.pulls.merge_task = None;
                if this.active_root() == Some(root.as_path()) {
                    this.pulls.details.remove(&number);
                }
                this.report(result, &format!("Auto-merge disabled for #{number}"), cx);
            })
            .ok();
        }));
    }

    /// Screenshot builds: `SUPERGIT_DEBUG_PR=<number>` selects that PR (in
    /// "All" when it is not the worktree's); `SUPERGIT_DEBUG_PR_MERGE=1`
    /// then opens its merge dialog once the details are loaded,
    /// `SUPERGIT_DEBUG_PR_SCROLL=<px>` scrolls its conversation and
    /// `SUPERGIT_DEBUG_PR_FILES=1` opens its Files tab.
    /// `SUPERGIT_DEBUG_PR_CHECK=<name>` inspects a check in Conversation.
    #[cfg(feature = "screenshot")]
    fn debug_pr_once(&mut self, info: &WorktreeInfo, cx: &mut Context<Self>) {
        let Some(number) = std::env::var("SUPERGIT_DEBUG_PR")
            .ok()
            .and_then(|n| n.parse::<u64>().ok())
        else {
            return;
        };
        if std::env::var_os("SUPERGIT_DEBUG_PR_DONE").is_none() {
            if !self.prs_loaded || self.worktree_prs_loading(info) {
                return;
            }
            // SAFETY: isolated screenshot builds only, on the main thread.
            unsafe { std::env::set_var("SUPERGIT_DEBUG_PR_DONE", "1") };
            let own = self.worktree_prs(info).iter().any(|pr| pr.number == number);
            self.pulls.show_all.insert(info.wt.path.clone(), !own);
            self.pulls.selected.insert(info.wt.path.clone(), number);
            if !own && !self.prs.iter().any(|pr| pr.number == number) {
                // Not in the list: make the search fetch it.
                if let Some(window) = cx.windows().first().copied() {
                    let search = self.pulls.search.clone();
                    cx.defer(move |cx| {
                        let _ = window.update(cx, |_, window, cx| {
                            search.update(cx, |input, cx| {
                                input.set_value(format!("#{number}"), window, cx)
                            });
                        });
                    });
                }
            }
        }
        let loaded = self
            .pulls
            .details
            .get(&number)
            .is_some_and(|l| l.details.value().is_some());
        // `SUPERGIT_DEBUG_PR_SCROLL=<px>` scrolls the details down to the conversation.
        if let Some(y) = std::env::var("SUPERGIT_DEBUG_PR_SCROLL")
            .ok()
            .and_then(|y| y.parse::<f32>().ok())
            && loaded
            && self.pulls.shown == Some(number)
        {
            self.pulls.details_scroll.set_offset(point(px(0.), px(-y)));
        }
        if loaded
            && std::env::var_os("SUPERGIT_DEBUG_PR_MERGE").is_some()
            && std::env::var_os("SUPERGIT_DEBUG_PR_MERGE_DONE").is_none()
        {
            // SAFETY: isolated screenshot builds only, on the main thread.
            unsafe { std::env::set_var("SUPERGIT_DEBUG_PR_MERGE_DONE", "1") };
            let Some(window) = cx.windows().first().copied() else {
                return;
            };
            let owner = cx.entity();
            cx.defer(move |cx| {
                let _ = window.update(cx, |_, window, cx| {
                    owner.update(cx, |this, cx| this.open_merge_dialog(number, window, cx));
                });
            });
        }
    }

    // ---- rendering -------------------------------------------------------------

    pub(super) fn render_pulls(
        &self,
        info: &WorktreeInfo,
        width: f32,
        cx: &mut Context<Self>,
    ) -> AnyElement {
        if let Some(error) = &self.prs_error
            && !self.prs_loaded
        {
            return self.render_data_notice("Pull requests unavailable", Some(error), false, cx);
        }
        let theme = cx.theme().clone();
        let visible = self.visible_prs(info, cx);
        let selected = self.selected_pr_number(info.path(), &visible);
        let list_max = (width - 320.).clamp(200., 640.);
        let list_width = self
            .pulls
            .layout
            .read(cx)
            .sizes()
            .first()
            .map(|w| w.as_f32())
            .unwrap_or(self.repository_view().pulls_width)
            .min(list_max);
        let list = self.render_pr_list(info, &visible, selected, &theme, cx);
        let details = match selected.and_then(|n| visible.iter().find(|pr| pr.number == n)) {
            Some(pr) => self.render_pr_details(info, pr, width - list_width, &theme, cx),
            None => div()
                .size_full()
                .flex()
                .items_center()
                .justify_center()
                .text_color(theme.muted_foreground)
                .child("Select a pull request")
                .into_any_element(),
        };
        h_resizable("pulls-layout")
            .with_state(&self.pulls.layout)
            .on_resize(
                cx.listener(|this, state, _, cx| {
                    this.remember_layout(LayoutPanel::Pulls, state, cx)
                }),
            )
            .child(
                resizable_panel()
                    .size(px(self.repository_view().pulls_width.min(list_max)))
                    .size_range(px(200.)..px(list_max))
                    .fixed()
                    .child(list),
            )
            .child(
                resizable_panel()
                    .size_range(px(320.)..Pixels::MAX)
                    .child(elastic(details)),
            )
            .into_any_element()
    }

    fn render_pr_list(
        &self,
        info: &WorktreeInfo,
        visible: &[PullRequest],
        selected: Option<u64>,
        theme: &gpui_kit::component::Theme,
        cx: &mut Context<Self>,
    ) -> AnyElement {
        let path = info.wt.path.clone();
        let all = self.shows_all_prs(info);
        let own = self.worktree_prs(info).len();
        let query = self.pr_query(cx);
        let searching = all
            && ((self.pulls.search_task.is_some() && searchable(&query))
                || self
                    .pulls
                    .search_results
                    .as_ref()
                    .is_some_and(|(q, r)| *q == query && r.is_loading()));
        let search_error = if all {
            self.pulls
                .search_results
                .as_ref()
                .filter(|(q, _)| *q == query)
                .and_then(|(_, r)| r.error())
                .map(|e| format!("GitHub search failed: {e}"))
        } else {
            self.pulls
                .worktree
                .get(info.path())
                .and_then(|f| f.prs.error())
                .map(|e| {
                    format!(
                        "Pull requests from {} unavailable: {e}",
                        info.wt.display_name()
                    )
                })
        };
        let name = info.wt.display_name();
        let rows: Vec<AnyElement> = visible
            .iter()
            .enumerate()
            .map(|(ix, pr)| {
                let number = pr.number;
                let path = path.clone();
                pr_list_row(ix, pr, selected == Some(number), all, theme)
                    .on_click(
                        cx.listener(move |this, _, _, cx| this.select_pr(path.clone(), number, cx)),
                    )
                    .into_any_element()
            })
            .collect();
        let own_path = path.clone();
        let all_path = path.clone();
        let header = v_flex()
            .flex_none()
            .gap_1()
            .px_2()
            .py_1()
            .border_b_1()
            .border_color(theme.border)
            .child(
                Input::new(&self.pulls.search)
                    .small()
                    .cleanable(true)
                    .prefix(
                        Icon::new(IconName::Search)
                            .small()
                            .text_color(theme.muted_foreground),
                    ),
            )
            .child(self.render_publish_button(info, cx))
            .child(
                h_flex()
                    .gap_1()
                    .items_center()
                    .child(
                        Button::new("prs-scope-own")
                            .label(format!("This worktree · {own}"))
                            .xsmall()
                            .ghost()
                            .selected(!all)
                            .tooltip(format!(
                                "Pull requests from {name}: its branch, and those linked by hand"
                            ))
                            .on_click(cx.listener(move |this, _, _, cx| {
                                this.set_pr_scope(own_path.clone(), false, cx)
                            })),
                    )
                    .child(
                        Button::new("prs-scope-all")
                            .label(format!("All · {}", self.prs.len()))
                            .xsmall()
                            .ghost()
                            .selected(all)
                            .tooltip("Open and recently merged pull requests; searching also asks GitHub for older ones")
                            .on_click(cx.listener(move |this, _, _, cx| {
                                this.set_pr_scope(all_path.clone(), true, cx)
                            })),
                    )
                    .child(div().flex_1())
                    .when(searching, |el| {
                        el.child(
                            div()
                                .text_xs()
                                .text_color(theme.muted_foreground)
                                .child("Searching GitHub…"),
                        )
                    }),
            );
        let empty = if !rows.is_empty() {
            None
        } else if !query.is_empty() {
            Some(
                div()
                    .p_3()
                    .text_sm()
                    .text_color(theme.muted_foreground)
                    .child(if searching {
                        "Searching…".to_string()
                    } else {
                        format!("No pull requests match “{query}”")
                    })
                    .into_any_element(),
            )
        } else if !all && (self.worktree_prs_loading(info) || !self.prs_loaded) {
            Some(
                div()
                    .p_3()
                    .text_sm()
                    .text_color(theme.muted_foreground)
                    .child("Loading pull requests…")
                    .into_any_element(),
            )
        } else if !all {
            let path = path.clone();
            Some(
                v_flex()
                    .p_3()
                    .gap_2()
                    .items_start()
                    .text_sm()
                    .text_color(theme.muted_foreground)
                    .child(format!("No pull requests from {name} yet."))
                    .child(
                        Button::new("prs-show-all")
                            .label("Show all pull requests")
                            .small()
                            .outline()
                            .on_click(cx.listener(move |this, _, _, cx| {
                                this.set_pr_scope(path.clone(), true, cx)
                            })),
                    )
                    .into_any_element(),
            )
        } else {
            Some(
                div()
                    .p_3()
                    .text_sm()
                    .text_color(theme.muted_foreground)
                    .child(if self.prs_loaded {
                        "No open or recently merged pull requests"
                    } else {
                        "Loading pull requests…"
                    })
                    .into_any_element(),
            )
        };
        v_flex()
            .size_full()
            .min_w_0()
            .border_r_1()
            .border_color(theme.border)
            .child(header)
            .when_some(self.prs_error.clone(), |el, error| {
                el.child(
                    div()
                        .px_3()
                        .py_1()
                        .text_xs()
                        .text_color(theme.danger)
                        .child(format!("List may be out of date: {error}")),
                )
            })
            .when_some(search_error, |el, error| {
                el.child(
                    div()
                        .px_3()
                        .py_1()
                        .text_xs()
                        .text_color(theme.danger)
                        .child(error),
                )
            })
            .child(
                v_flex()
                    .id("pr-list")
                    .flex_1()
                    .min_h_0()
                    .overflow_y_scroll()
                    .children(empty)
                    .children(rows),
            )
            .into_any_element()
    }

    fn render_pr_details(
        &self,
        info: &WorktreeInfo,
        listed: &PullRequest,
        width: f32,
        theme: &gpui_kit::component::Theme,
        cx: &mut Context<Self>,
    ) -> AnyElement {
        let loaded = self.pulls.details.get(&listed.number);
        let details = loaded.and_then(|l| l.details.value());
        let pr = details.map_or(listed, |d| &d.pr);
        let error = loaded.and_then(|l| l.details.error()).map(|error| {
            self.render_data_notice(
                if details.is_some() {
                    "Details may be out of date — showing the last successful data"
                } else {
                    "Details unavailable"
                },
                Some(error),
                false,
                cx,
            )
        });
        let tab = self.pulls.tab;
        let conversation = details.map(|d| activity(d).len() as u64);
        let files = details.map(|d| d.changed_files).or_else(|| {
            self.pulls
                .diff_shown
                .as_ref()
                .filter(|(n, _)| *n == pr.number)
                .map(|_| self.pulls.diff.read(cx).files().len() as u64)
        });
        let count = |label: &str, n: Option<u64>| match n {
            Some(n) => format!("{label} · {n}"),
            None => label.to_string(),
        };
        let tabs = TabBar::new("pr-tabs")
            .underline()
            .px_2()
            .selected_index(match tab {
                DetailsTab::Conversation => 0,
                DetailsTab::Commits => 1,
                DetailsTab::Files => 2,
            })
            .on_click(cx.listener(|this, ix: &usize, _, cx| {
                this.set_details_tab(
                    [
                        DetailsTab::Conversation,
                        DetailsTab::Commits,
                        DetailsTab::Files,
                    ][*ix],
                    cx,
                )
            }))
            .child(Tab::new().label(count("Conversation", conversation)))
            .child(Tab::new().label(count("Commits", details.map(|d| d.commits))))
            .child(Tab::new().label(count("Files changed", files)));
        let body = match tab {
            DetailsTab::Conversation => div()
                .relative()
                .flex_1()
                .min_h_0()
                .child(
                    v_flex()
                        .id("pr-conversation")
                        // Out of the flow: long code lines in a body must not
                        // widen the panel (their min-content would).
                        .absolute()
                        .inset_0()
                        .overflow_y_scroll()
                        .track_scroll(&self.pulls.details_scroll)
                        .when(
                            details.is_none() && loaded.is_none_or(|l| l.details.is_loading()),
                            |el| {
                                el.child(
                                    div()
                                        .px_4()
                                        .py_3()
                                        .text_sm()
                                        .text_color(theme.muted_foreground)
                                        .child("Loading conversation and checks…"),
                                )
                            },
                        )
                        .when_some(details, |el, details| {
                            el.child(self.render_merge_box(details, theme, cx))
                                .children(self.render_checks(details, theme, cx))
                                .child(render_description(details, theme))
                                .child(self.render_activity(details, theme, cx))
                        }),
                )
                .into_any_element(),
            DetailsTab::Commits => self.render_pr_commits(pr, cx),
            DetailsTab::Files => div()
                .flex_1()
                .min_h_0()
                .child(self.render_pr_files(pr, width, theme, cx))
                .into_any_element(),
        };
        // The whole column is out of the flow too, for the same reason.
        div()
            .relative()
            .size_full()
            .child(
                v_flex()
                    .absolute()
                    .inset_0()
                    .child(self.render_pr_header(info, pr, details, theme, cx))
                    .children(error)
                    .child(tabs)
                    .child(body),
            )
            .into_any_element()
    }

    fn render_pr_commits(&self, pr: &PullRequest, cx: &mut Context<Self>) -> AnyElement {
        let key = (pr.number, pr.head_oid.clone());
        match self.pulls.commits.get(&key) {
            Some(LoadState::Ready(commits)) => {
                let theme = cx.theme().clone();
                v_flex()
                    .id(("pr-commits", pr.number))
                    .flex_1()
                    .min_h_0()
                    .overflow_y_scroll()
                    .when(commits.is_empty(), |list| {
                        list.child(div().p_3().child("No commits"))
                    })
                    .children(commits.iter().map(|commit| {
                        let (sha, url) = (commit.oid.clone(), commit.url.clone());
                        let abbreviated: String = sha.chars().take(8).collect();
                        let date = commit
                            .committed_date
                            .get(..10)
                            .unwrap_or(&commit.committed_date);
                        v_flex()
                            .id(SharedString::from(sha.clone()))
                            .min_w_0()
                            .px_4()
                            .py_3()
                            .gap_1()
                            .border_b_1()
                            .border_color(theme.border)
                            .child(
                                h_flex()
                                    .gap_2()
                                    .justify_between()
                                    .child(
                                        div()
                                            .min_w_0()
                                            .font_weight(FontWeight::SEMIBOLD)
                                            .child(commit.message_headline.clone()),
                                    )
                                    .child(
                                        h_flex()
                                            .gap_1()
                                            .flex_none()
                                            .child(
                                                Button::new("copy-pr-commit-sha")
                                                    .label(abbreviated)
                                                    .small()
                                                    .ghost()
                                                    .tooltip("Copy full commit SHA")
                                                    .on_click(cx.listener(
                                                        move |this, _, _, cx| {
                                                            cx.write_to_clipboard(
                                                                ClipboardItem::new_string(
                                                                    sha.clone(),
                                                                ),
                                                            );
                                                            this.flash(
                                                                "Copied commit SHA",
                                                                false,
                                                                cx,
                                                            );
                                                        },
                                                    )),
                                            )
                                            .child(
                                                Button::new("open-pr-commit")
                                                    .label("Open")
                                                    .small()
                                                    .ghost()
                                                    .tooltip("View the commit on GitHub")
                                                    .on_click(move |_, _, cx| cx.open_url(&url)),
                                            ),
                                    ),
                            )
                            .when(!commit.message_body.is_empty(), |row| {
                                row.child(div().text_sm().child(commit.message_body.clone()))
                            })
                            .child(
                                div()
                                    .text_xs()
                                    .text_color(theme.muted_foreground)
                                    .child(format!("{} · {date}", commit.author())),
                            )
                    }))
                    .into_any_element()
            }
            Some(LoadState::Failed { error, .. }) => v_flex()
                .child(self.render_data_notice("Commits unavailable", Some(error), false, cx))
                .child(
                    Button::new("retry-pr-commits")
                        .label("Retry")
                        .small()
                        .ghost()
                        .on_click(cx.listener(move |this, _, _, cx| {
                            this.pulls.commits.remove(&key);
                            this.ensure_pulls(cx);
                            cx.notify();
                        })),
                )
                .into_any_element(),
            _ => self.render_data_notice("Loading commits…", None, false, cx),
        }
    }

    /// The Files tab: the PR's files beside its diff, like a commit in
    /// History. `width` is the details column's.
    fn render_pr_files(
        &self,
        pr: &PullRequest,
        width: f32,
        theme: &gpui_kit::component::Theme,
        cx: &mut Context<Self>,
    ) -> AnyElement {
        let number = pr.number;
        let muted = |text: &str| {
            div()
                .size_full()
                .flex()
                .items_center()
                .justify_center()
                .text_sm()
                .text_color(theme.muted_foreground)
                .child(text.to_string())
                .into_any_element()
        };
        let Some(entry) = self.pulls.patches.get(&number) else {
            return muted("Loading the diff…");
        };
        let shown = self
            .pulls
            .diff_shown
            .as_ref()
            .is_some_and(|(n, head)| *n == number && *head == entry.head);
        let url = pr.url.clone();
        let notice = entry.patch.error().map(|error| {
            v_flex()
                .flex_none()
                .p_3()
                .gap_2()
                .text_sm()
                .border_b_1()
                .border_color(theme.border)
                .child(div().font_weight(FontWeight::SEMIBOLD).child(
                    if entry.patch.value().is_some() {
                        "The diff may be out of date — showing the last successful one"
                    } else {
                        "Diff unavailable"
                    },
                ))
                .child(
                    div()
                        .text_color(theme.muted_foreground)
                        .child(error.to_string()),
                )
                .child(
                    h_flex()
                        .gap_1()
                        .child(
                            Button::new("pr-diff-retry")
                                .label("Retry")
                                .small()
                                .ghost()
                                .on_click(cx.listener(move |this, _, _, cx| {
                                    this.retry_pr_patch(number, cx)
                                })),
                        )
                        .child(
                            Button::new("pr-diff-web")
                                .label("Open files on GitHub")
                                .small()
                                .ghost()
                                .on_click(cx.listener(move |this, _, _, cx| {
                                    let r = launch::open_url(&format!("{url}/files"));
                                    this.report(r, "Opened in browser", cx);
                                })),
                        ),
                )
        });
        let local = entry.local.then(|| {
            div()
                .flex_none()
                .px_3()
                .py_1()
                .text_xs()
                .text_color(theme.warning)
                .border_b_1()
                .border_color(theme.border)
                .child(format!(
                    "GitHub did not return this diff; computed locally: {} against its merge base with {}",
                    &entry.head[..entry.head.len().min(8)],
                    pr.base_branch
                ))
        });
        let no_difft = (shown && self.pr_difft_unavailable()).then(|| {
            div()
                .flex_none()
                .px_3()
                .py_1()
                .text_xs()
                .text_color(theme.muted_foreground)
                .border_b_1()
                .border_color(theme.border)
                .child(format!(
                    "Difftastic needs the PR's commits locally (fetch {}); showing the line diff",
                    pr.head_branch
                ))
        });
        let content = match entry.patch.value() {
            None if entry.patch.is_loading() => muted("Loading the diff…"),
            None => div().into_any_element(),
            Some(patch) if patch.trim().is_empty() => muted("No changes"),
            Some(_) if !shown => muted("Loading the diff…"),
            Some(_) => {
                let files: Vec<FileChange> = self
                    .pulls
                    .diff
                    .read(cx)
                    .files()
                    .iter()
                    .map(commit_file)
                    .collect();
                let list_max = (width - 180.).clamp(140., 600.);
                h_resizable("pr-files-layout")
                    .with_state(&self.pulls.files_layout)
                    .on_resize(cx.listener(|this, state, _, cx| {
                        this.remember_layout(LayoutPanel::CommitFiles, state, cx)
                    }))
                    .child(
                        resizable_panel()
                            .size(px(self.repository_view().commit_files_width.min(list_max)))
                            .size_range(px(140.)..px(list_max))
                            .fixed()
                            .child(self.render_file_list(FileList::Pull, &files, theme, cx)),
                    )
                    .child(
                        resizable_panel()
                            .size_range(px(180.)..Pixels::MAX)
                            .child(elastic(self.render_review_diff(FileList::Pull, cx))),
                    )
                    .into_any_element()
            }
        };
        v_flex()
            .size_full()
            .border_t_1()
            .border_color(theme.border)
            .children(notice)
            .children(local)
            .children(no_difft)
            .child(div().flex_1().min_h_0().child(content))
            .into_any_element()
    }
    fn render_pr_header(
        &self,
        info: &WorktreeInfo,
        pr: &PullRequest,
        details: Option<&PrDetails>,
        theme: &gpui_kit::component::Theme,
        cx: &mut Context<Self>,
    ) -> AnyElement {
        let muted = theme.muted_foreground;
        let color = pr_color(pr.state, theme);
        let url = pr.url.clone();
        let copy_url = pr.url.clone();
        let number = pr.number;
        let path = info.wt.path.clone();
        let name = info.wt.display_name();
        let linked = self.pr_linked(info, pr);
        let by_hand = self.pr_linked_by_hand(info, number);
        let from_branch = self.pr_from_branch(info, pr);
        let worktree = self
            .worktrees
            .iter()
            .find(|w| w.wt.branch.as_deref() == Some(pr.head_branch.as_str()))
            .map(|w| w.wt.path.clone());
        let on_this_worktree = worktree.as_deref() == Some(info.path());
        let checkout = (!on_this_worktree
            && (worktree.is_some() || (pr.is_open() && sg_github::head_in_same_repository(pr))))
        .then(|| pr.clone());
        let link_tooltip = if linked && from_branch && !by_hand {
            format!(
                "Its head is {}'s branch. Unlink to hide it from {name}",
                pr.head_branch
            )
        } else if linked {
            format!("Linked to {name} by hand")
        } else {
            format!("Show it among {name}'s pull requests")
        };
        let meta = h_flex()
            .flex_wrap()
            .gap_x_2()
            .gap_y_0p5()
            .text_xs()
            .text_color(muted)
            .child(
                div()
                    .text_color(theme.foreground)
                    .font_weight(FontWeight::MEDIUM)
                    .child(format!("@{}", pr.author)),
            )
            .child(format!("opened {}", ago(pr.created_at)))
            .child(
                div()
                    .font_family(theme.mono_font_family.clone())
                    .child(format!("{} → {}", pr.head_branch, pr.base_branch)),
            )
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
            .when_some(details, |el, d| {
                el.child(format!(
                    "{} commit{} · {} file{}",
                    d.commits,
                    if d.commits == 1 { "" } else { "s" },
                    d.changed_files,
                    if d.changed_files == 1 { "" } else { "s" }
                ))
            })
            .when_some(pr.updated(), |el, updated| {
                el.child(format!("updated {}", ago(Some(updated))))
            });
        v_flex()
            .flex_none()
            .px_4()
            .py_3()
            .gap_2()
            .border_b_1()
            .border_color(theme.border)
            .child(
                h_flex()
                    .gap_2()
                    .items_start()
                    .child(
                        div()
                            .flex_none()
                            .mt_0p5()
                            .px_1p5()
                            .rounded_sm()
                            .text_xs()
                            .bg(color.opacity(0.15))
                            .text_color(color)
                            .child(pr.state.label()),
                    )
                    .child(
                        div()
                            .flex_1()
                            .min_w_0()
                            .text_lg()
                            .font_weight(FontWeight::SEMIBOLD)
                            .child(format!("{} #{}", pr.title, pr.number)),
                    ),
            )
            .child(meta)
            .child(
                h_flex()
                    .flex_wrap()
                    .gap_1()
                    .child(
                        Button::new("pr-open-browser")
                            .label("Open on GitHub")
                            .xsmall()
                            .outline()
                            .on_click(cx.listener(move |this, _, _, cx| {
                                let r = launch::open_url(&url);
                                this.report(r, "Opened in browser", cx);
                            })),
                    )
                    .child(
                        Button::new("pr-copy-url")
                            .label("Copy URL")
                            .xsmall()
                            .ghost()
                            .on_click(cx.listener(move |this, _, _, cx| {
                                cx.write_to_clipboard(ClipboardItem::new_string(copy_url.clone()));
                                this.flash("Copied pull request URL", false, cx);
                            })),
                    )
                    .when_some(checkout, |el, pr| {
                        let exists = worktree.is_some();
                        el.child(
                            Button::new("pr-worktree")
                                .label(if exists {
                                    "Go to worktree"
                                } else {
                                    "Check out as worktree"
                                })
                                .xsmall()
                                .ghost()
                                .tooltip(if exists {
                                    format!("Select the worktree on {}", pr.head_branch)
                                } else {
                                    format!("Create a worktree on {}", pr.head_branch)
                                })
                                .on_click(
                                    cx.listener(move |this, _, _, cx| this.checkout_pr(&pr, cx)),
                                ),
                        )
                    })
                    .child(
                        Button::new("pr-link")
                            .label(if linked {
                                format!("Unlink from {name}")
                            } else {
                                format!("Link to {name}")
                            })
                            .xsmall()
                            .ghost()
                            .tooltip(link_tooltip)
                            .on_click(cx.listener(move |this, _, _, cx| {
                                this.set_pr_link(path.clone(), number, !linked, cx)
                            })),
                    ),
            )
            .into_any_element()
    }

    fn render_merge_box(
        &self,
        details: &PrDetails,
        theme: &gpui_kit::component::Theme,
        cx: &mut Context<Self>,
    ) -> AnyElement {
        let pr = &details.pr;
        let section = v_flex()
            .flex_none()
            .px_4()
            .py_3()
            .gap_1p5()
            .text_sm()
            .border_b_1()
            .border_color(theme.border);
        match pr.state {
            PrState::Merged => {
                return section
                    .child(div().text_color(theme.magenta).child(format!(
                            "Merged{} into {} {}",
                            details
                                .merged_by
                                .as_ref()
                                .map(|by| format!(" by @{by}"))
                                .unwrap_or_default(),
                            pr.base_branch,
                            ago(pr.merged_at)
                        )))
                    .into_any_element();
            }
            PrState::Closed => {
                return section
                    .child(
                        div()
                            .text_color(theme.danger)
                            .child(format!("Closed without merging {}", ago(details.closed_at))),
                    )
                    .into_any_element();
            }
            PrState::Open | PrState::Draft => {}
        }
        let status = details.merge_status;
        let number = pr.number;
        let busy = self.pulls.merge_task.is_some();
        let review = review_decision(&pr.review_decision, theme);
        let blocked = matches!(status, MergeStatus::Conflicts | MergeStatus::Draft);
        section
            .child(
                h_flex()
                    .gap_2()
                    .child(
                        div()
                            .text_color(merge_status_color(status, theme))
                            .child(if status.can_merge() { "✓" } else { "●" }),
                    )
                    .child(
                        div()
                            .font_weight(FontWeight::MEDIUM)
                            .child(status.describe()),
                    ),
            )
            .when_some(review, |el, (label, color)| {
                el.child(div().pl_5().text_xs().text_color(color).child(label))
            })
            .when(pr.checks.total() > 0, |el| {
                let color = checks_glyph(&pr.checks, theme)
                    .map(|(_, color)| color)
                    .unwrap_or(theme.muted_foreground);
                el.child(
                    div()
                        .pl_5()
                        .text_xs()
                        .text_color(color)
                        .child(format!("Checks: {}", checks_summary(&pr.checks))),
                )
            })
            .when_some(details.auto_merge.clone(), |el, auto| {
                el.child(
                    h_flex()
                        .pl_5()
                        .gap_2()
                        .text_xs()
                        .text_color(theme.blue)
                        .child(format!(
                            "Auto-merge{} enabled by @{}: GitHub merges once requirements pass",
                            auto.method
                                .map(|m| format!(" ({})", m.short().to_lowercase()))
                                .unwrap_or_default(),
                            auto.enabled_by
                        ))
                        .child(
                            Button::new("pr-disable-auto")
                                .label("Disable")
                                .xsmall()
                                .ghost()
                                .disabled(busy)
                                .on_click(cx.listener(move |this, _, _, cx| {
                                    this.disable_auto_merge(number, cx)
                                })),
                        ),
                )
            })
            .child(
                h_flex().pt_1().gap_2().child(
                    Button::new("pr-merge")
                        .label(if busy { "Merging…" } else { "Merge…" })
                        .small()
                        .when(status.can_merge(), |b| b.primary())
                        .when(!status.can_merge(), |b| b.outline())
                        .disabled(busy || blocked)
                        .tooltip(if blocked {
                            status.describe().to_string()
                        } else {
                            format!("Choose how to merge #{number} into {}", pr.base_branch)
                        })
                        .on_click(cx.listener(move |this, _, window, cx| {
                            this.open_merge_dialog(number, window, cx)
                        })),
                ),
            )
            .into_any_element()
    }

    fn render_checks(
        &self,
        details: &PrDetails,
        theme: &gpui_kit::component::Theme,
        cx: &mut Context<Self>,
    ) -> Option<AnyElement> {
        if details.checks.is_empty() {
            return None;
        }
        let show_passed = self.pulls.show_passed_checks;
        let quiet = details
            .checks
            .iter()
            .filter(|c| matches!(c.status, CheckStatus::Passed | CheckStatus::Skipped))
            .count();
        let rows = details
            .checks
            .iter()
            .enumerate()
            .filter(|(_, c)| {
                show_passed || matches!(c.status, CheckStatus::Failed | CheckStatus::Pending)
            })
            .map(|(ix, check)| {
                let (glyph, color) = check_glyph(check.status, theme);
                let url = check.url.clone();
                let open = self
                    .pulls
                    .check_inspection
                    .as_ref()
                    .filter(|open| Some(&open.id) == check.run_id.as_ref());
                let row = h_flex()
                    .id(("pr-check", ix))
                    .gap_2()
                    .py_0p5()
                    .items_center()
                    .text_sm()
                    .child(div().flex_none().w_4().text_color(color).child(glyph))
                    .child(
                        h_flex()
                            .flex_1()
                            .min_w_0()
                            .gap_2()
                            .child(div().min_w_0().truncate().child(check.name.clone()))
                            .when_some(check.workflow.clone(), |el, workflow| {
                                el.child(
                                    div()
                                        .flex_none()
                                        .text_xs()
                                        .text_color(theme.muted_foreground)
                                        .child(workflow),
                                )
                            })
                            .when(check.required, |el| {
                                el.child(
                                    div()
                                        .flex_none()
                                        .px_1()
                                        .rounded_sm()
                                        .text_xs()
                                        .bg(theme.muted)
                                        .text_color(theme.muted_foreground)
                                        .child("required"),
                                )
                            }),
                    )
                    .child(
                        div()
                            .flex_none()
                            .text_xs()
                            .text_color(theme.muted_foreground)
                            .child(match duration(check) {
                                Some(took) => format!("{} · {took}", check.detail),
                                None => check.detail.clone(),
                            }),
                    )
                    .when_some(check.run_id.clone(), |el, id| {
                        el.child(
                            Button::new(("pr-check-inspect", ix))
                                .label(if open.is_some() {
                                    "Hide details"
                                } else {
                                    "Inspect"
                                })
                                .xsmall()
                                .ghost()
                                .on_click(cx.listener(move |this, _, _, cx| {
                                    this.toggle_check_inspection(&id, cx);
                                })),
                        )
                    })
                    .when_some(url, |el, url| {
                        el.child(
                            Button::new(("pr-check-open", ix))
                                .label("GitHub")
                                .xsmall()
                                .ghost()
                                .tooltip(url.clone())
                                .on_click(cx.listener(move |this, _, _, cx| {
                                    let r = launch::open_url(&url);
                                    this.report(r, "Opened in browser", cx);
                                })),
                        )
                    });
                v_flex()
                    .flex_none()
                    .child(row)
                    .when_some(open, |el, open| {
                        el.child(self.render_check_inspection(open, check, theme, cx))
                    })
                    .into_any_element()
            })
            .collect::<Vec<_>>();
        Some(
            v_flex()
                .flex_none()
                .px_4()
                .py_3()
                .gap_1()
                .border_b_1()
                .border_color(theme.border)
                .child(
                    h_flex()
                        .gap_2()
                        .items_center()
                        .child(div().font_weight(FontWeight::SEMIBOLD).child("Checks"))
                        .child(
                            div()
                                .text_xs()
                                .text_color(theme.muted_foreground)
                                .child(checks_summary(&details.pr.checks)),
                        )
                        .child(div().flex_1())
                        .when(quiet > 0, |el| {
                            el.child(
                                Button::new("pr-toggle-passed")
                                    .label(if show_passed {
                                        "Hide passed".to_string()
                                    } else {
                                        format!("Show {quiet} passed")
                                    })
                                    .xsmall()
                                    .ghost()
                                    .on_click(cx.listener(|this, _, _, cx| {
                                        this.pulls.show_passed_checks =
                                            !this.pulls.show_passed_checks;
                                        cx.notify();
                                    })),
                            )
                        }),
                )
                .children(rows)
                .into_any_element(),
        )
    }

    fn render_activity(
        &self,
        details: &PrDetails,
        theme: &gpui_kit::component::Theme,
        cx: &mut Context<Self>,
    ) -> AnyElement {
        let number = details.pr.number;
        let items = activity(details);
        let count = items.len();
        let older = details
            .comments_total
            .saturating_sub(details.comments.len() as u64);
        let entries: Vec<AnyElement> = items
            .into_iter()
            .enumerate()
            .map(|(ix, item)| match item {
                Activity::Comment(comment) => comment_card(
                    ("pr-comment", ix),
                    number,
                    ix,
                    comment,
                    "commented",
                    None,
                    theme,
                    cx,
                ),
                Activity::Review(review) => {
                    let color = match review.state {
                        ReviewState::Approved => Some(theme.success),
                        ReviewState::ChangesRequested => Some(theme.danger),
                        _ => None,
                    };
                    let comment = Comment {
                        author: review.author.clone(),
                        body: review.body.clone(),
                        created_at: review.submitted_at,
                        url: review.url.clone(),
                    };
                    comment_card(
                        ("pr-review", ix),
                        number,
                        ix,
                        &comment,
                        review.state.label(),
                        color,
                        theme,
                        cx,
                    )
                }
                Activity::Thread(thread) => self.render_thread(&details.pr, ix, thread, theme, cx),
            })
            .collect();
        v_flex()
            .flex_none()
            .px_4()
            .py_3()
            .gap_3()
            .child(
                h_flex()
                    .gap_2()
                    .child(
                        div()
                            .font_weight(FontWeight::SEMIBOLD)
                            .child("Conversation"),
                    )
                    .child(div().text_xs().text_color(theme.muted_foreground).child(
                        if older > 0 {
                            format!("{count} · {older} older comments on GitHub")
                        } else {
                            count.to_string()
                        },
                    )),
            )
            .when(entries.is_empty(), |el| {
                el.child(
                    div()
                        .text_sm()
                        .text_color(theme.muted_foreground)
                        .child("No comments or reviews yet"),
                )
            })
            .children(entries)
            .into_any_element()
    }

    fn render_thread(
        &self,
        pr: &PullRequest,
        ix: usize,
        thread: &ReviewThread,
        theme: &gpui_kit::component::Theme,
        cx: &mut Context<Self>,
    ) -> AnyElement {
        let number = pr.number;
        let expanded = thread.resolved == self.pulls.toggled_threads.contains(&thread.id);
        let id = thread.id.clone();
        let location = match thread.line {
            Some(line) => format!("{}:{line}", thread.path),
            None => thread.path.clone(),
        };
        let tag = |label: &'static str, color: Hsla| {
            div()
                .flex_none()
                .px_1()
                .rounded_sm()
                .text_xs()
                .bg(color.opacity(0.15))
                .text_color(color)
                .child(label)
        };
        let comments = thread.comments.len();
        v_flex()
            .flex_none()
            .rounded_md()
            .border_1()
            .border_color(theme.border)
            .overflow_hidden()
            .child(
                h_flex()
                    .id(("pr-thread", ix))
                    .px_3()
                    .py_1p5()
                    .gap_2()
                    .items_center()
                    .bg(theme.muted)
                    .cursor_pointer()
                    .on_click(cx.listener(move |this, _, _, cx| this.toggle_thread(id.clone(), cx)))
                    .child(
                        Icon::new(if expanded {
                            IconName::ChevronDown
                        } else {
                            IconName::ChevronRight
                        })
                        .small()
                        .text_color(theme.muted_foreground),
                    )
                    .child(
                        div()
                            .min_w_0()
                            .truncate()
                            .text_xs()
                            .font_family(theme.mono_font_family.clone())
                            .child(location),
                    )
                    .when(thread.resolved, |el| {
                        el.child(tag("resolved", theme.success))
                    })
                    .when(thread.outdated, |el| {
                        el.child(tag("outdated", theme.warning))
                    })
                    .child(div().flex_1())
                    .child({
                        let path = thread.path.clone();
                        // An outdated line may be gone: show the file then.
                        let line = thread.line.filter(|_| !thread.outdated);
                        Button::new(("pr-thread-diff", ix))
                            .label("View in diff")
                            .xsmall()
                            .ghost()
                            .on_click(cx.listener(move |this, _, _, cx| {
                                cx.stop_propagation();
                                this.jump_to_thread(number, path.clone(), line, cx)
                            }))
                    })
                    .when(!expanded, |el| {
                        el.child(
                            div()
                                .flex_none()
                                .text_xs()
                                .text_color(theme.muted_foreground)
                                .child(format!(
                                    "{comments} comment{}",
                                    if comments == 1 { "" } else { "s" }
                                )),
                        )
                    }),
            )
            .when(expanded, |el| {
                el.child(diff_excerpt(&thread.diff_hunk, theme)).child(
                    v_flex()
                        .px_3()
                        .py_2()
                        .gap_3()
                        .children(thread.comments.iter().enumerate().map(|(cix, comment)| {
                            comment_body(
                                SharedString::from(format!("pr-{number}-t{ix}-{cix}")),
                                comment,
                                "",
                                None,
                                theme,
                            )
                        }))
                        .child(self.render_thread_actions(pr, ix, thread, cx)),
                )
            })
            .into_any_element()
    }

    pub(super) fn render_pr_dialog(&self, cx: &mut Context<Self>) -> AnyElement {
        if let Some(dialog) = &self.pulls.publish {
            return self.render_publish_dialog(dialog, cx);
        }
        let Some(dialog) = &self.pulls.merge else {
            return div().into_any_element();
        };
        let theme = cx.theme().clone();
        let Some(details) = self
            .pulls
            .details
            .get(&dialog.number)
            .and_then(|loaded| loaded.details.value())
        else {
            return div().into_any_element();
        };
        let pr = &details.pr;
        let status = details.merge_status;
        let method = dialog.method;
        let methods = details.merge_methods.clone();
        let owner = cx.entity();
        let running = dialog.running;
        let can_merge = status.can_merge() || status == MergeStatus::Unknown;
        let can_auto = details.auto_merge_allowed
            && details.auto_merge.is_none()
            && matches!(status, MergeStatus::Blocked | MergeStatus::Unknown);
        let note = |text: String, color: Hsla| div().text_sm().text_color(color).child(text);
        let short = &pr.head_oid[..pr.head_oid.len().min(8)];
        let mut lines: Vec<AnyElement> = vec![
            note(
                status.describe().to_string(),
                merge_status_color(status, &theme),
            )
            .into_any_element(),
        ];
        if let Some((label, color)) = review_decision(&pr.review_decision, &theme) {
            lines.push(note(label.to_string(), color).into_any_element());
        }
        if pr.checks.total() > 0 {
            let color = checks_glyph(&pr.checks, &theme)
                .map(|(_, c)| c)
                .unwrap_or(theme.muted_foreground);
            lines.push(
                note(format!("Checks: {}", checks_summary(&pr.checks)), color).into_any_element(),
            );
        }
        lines.push(
            note(
                format!(
                    "Merges {} as of {short}: GitHub refuses if the branch moved since. The branch is kept — a worktree may still use it.",
                    pr.head_branch
                ),
                theme.muted_foreground,
            )
            .into_any_element(),
        );
        if can_auto {
            lines.push(
                note(
                    "Auto-merge waits for the required reviews and checks, then merges with this method."
                        .into(),
                    theme.muted_foreground,
                )
                .into_any_element(),
            );
        }
        if let Some(error) = &dialog.error {
            lines.push(note(error.clone(), theme.danger).into_any_element());
        }
        div()
            .absolute()
            .inset_0()
            .flex()
            .justify_center()
            .items_start()
            .pt(px(80.))
            .bg(gpui_kit::black().opacity(0.35))
            .on_mouse_down(
                MouseButton::Left,
                cx.listener(|this, _, window, cx| this.close_merge_dialog(window, cx)),
            )
            .child(
                v_flex()
                    .track_focus(&self.pulls.merge_handle)
                    .w(px(540.))
                    .max_w_full()
                    .p_4()
                    .gap_3()
                    .rounded_lg()
                    .shadow_lg()
                    .border_1()
                    .border_color(theme.border)
                    .bg(theme.background)
                    .on_mouse_down(MouseButton::Left, |_, _, cx| cx.stop_propagation())
                    .capture_key_down(cx.listener(|this, event: &KeyDownEvent, window, cx| {
                        if event.keystroke.key == "escape" {
                            this.close_merge_dialog(window, cx);
                            cx.stop_propagation();
                        }
                    }))
                    .child(
                        div()
                            .text_lg()
                            .font_weight(FontWeight::BOLD)
                            .child(format!("Merge #{} into {}", pr.number, pr.base_branch)),
                    )
                    .child(div().text_sm().child(pr.title.clone()))
                    .child(
                        h_flex()
                            .gap_2()
                            .items_center()
                            .child(div().text_sm().child("Method"))
                            .child(
                                Button::new("merge-method")
                                    .small()
                                    .outline()
                                    .label(format!("{} ▾", method.label()))
                                    .disabled(running || methods.len() < 2)
                                    .dropdown_menu(move |mut menu, _, _| {
                                        for &choice in &methods {
                                            let owner = owner.clone();
                                            menu = menu.item(
                                                PopupMenuItem::new(choice.label())
                                                    .checked(choice == method)
                                                    .on_click(move |_, _, cx| {
                                                        owner.update(cx, |this, cx| {
                                                            this.choose_merge_method(choice, cx)
                                                        });
                                                    }),
                                            );
                                        }
                                        menu
                                    }),
                            ),
                    )
                    .child(v_flex().gap_1().children(lines))
                    .child(
                        h_flex()
                            .justify_end()
                            .gap_2()
                            .child(
                                Button::new("merge-cancel")
                                    .label("Cancel")
                                    .small()
                                    .ghost()
                                    .on_click(cx.listener(|this, _, window, cx| {
                                        this.close_merge_dialog(window, cx)
                                    })),
                            )
                            .when(can_auto, |el| {
                                el.child(
                                    Button::new("merge-auto")
                                        .label("Enable auto-merge")
                                        .small()
                                        .outline()
                                        .disabled(running)
                                        .on_click(cx.listener(|this, _, window, cx| {
                                            this.confirm_merge(true, window, cx)
                                        })),
                                )
                            })
                            .child(
                                Button::new("merge-confirm")
                                    .label(if running {
                                        "Merging…".to_string()
                                    } else {
                                        method.label().to_string()
                                    })
                                    .small()
                                    .primary()
                                    .disabled(running || !can_merge)
                                    .on_click(cx.listener(|this, _, window, cx| {
                                        this.confirm_merge(false, window, cx)
                                    })),
                            ),
                    ),
            )
            .into_any_element()
    }
}

// ---- presentation ------------------------------------------------------------

fn pr_list_row(
    ix: usize,
    pr: &PullRequest,
    selected: bool,
    show_author: bool,
    theme: &gpui_kit::component::Theme,
) -> Stateful<Div> {
    let color = pr_color(pr.state, theme);
    let checks = checks_glyph(&pr.checks, theme);
    // "Review required" is the norm; only a verdict earns the room.
    let review = match pr.review_decision.as_str() {
        "APPROVED" => Some(("Approved", theme.success)),
        "CHANGES_REQUESTED" => Some(("changes", theme.danger)),
        _ => None,
    };
    v_flex()
        .id(("pr-row", ix))
        .px_3()
        .py_2()
        .gap_0p5()
        .border_b_1()
        .border_color(theme.border)
        .cursor_pointer()
        .when(selected, |el| el.bg(theme.list_active))
        .when(!selected, |el| el.hover(|el| el.bg(theme.list_hover)))
        .child(div().min_w_0().truncate().child(pr.title.clone()))
        .child(
            h_flex()
                .gap_2()
                .min_w_0()
                .text_xs()
                .text_color(theme.muted_foreground)
                .child(div().flex_none().text_color(color).child(format!(
                    "#{} {}",
                    pr.number,
                    pr.state.label()
                )))
                .child(div().min_w_0().truncate().child(if show_author {
                    format!("@{} · {}", pr.author, pr.head_branch)
                } else {
                    pr.head_branch.clone()
                }))
                .child(div().flex_1())
                .when_some(review, |el, (label, color)| {
                    el.child(div().flex_none().text_color(color).child(label))
                })
                .when_some(checks, |el, (glyph, color)| {
                    el.child(div().flex_none().text_color(color).child(glyph))
                })
                .when_some(pr.updated(), |el, updated| {
                    el.child(div().flex_none().child(time_ago(updated)))
                }),
        )
}

fn render_description(details: &PrDetails, theme: &gpui_kit::component::Theme) -> AnyElement {
    let body = visible_markdown(&details.body);
    v_flex()
        .flex_none()
        .px_4()
        .py_3()
        .gap_1()
        .text_sm()
        .border_b_1()
        .border_color(theme.border)
        .child(if body.is_empty() {
            div()
                .text_color(theme.muted_foreground)
                .child("No description")
                .into_any_element()
        } else {
            TextView::markdown(
                SharedString::from(format!("pr-{}-body", details.pr.number)),
                body,
            )
            .selectable(true)
            .into_any_element()
        })
        .into_any_element()
}

/// A comment or review: author, what they did and when, the text.
#[expect(
    clippy::too_many_arguments,
    reason = "a presentation helper; bundling them would only add a struct"
)]
fn comment_card(
    id: (&'static str, usize),
    number: u64,
    ix: usize,
    comment: &Comment,
    action: &str,
    action_color: Option<Hsla>,
    theme: &gpui_kit::component::Theme,
    cx: &mut Context<SuperGitApp>,
) -> AnyElement {
    let url = comment.url.clone();
    v_flex()
        .flex_none()
        .gap_1()
        .px_3()
        .py_2()
        .rounded_md()
        .border_1()
        .border_color(theme.border)
        .child(
            h_flex()
                .gap_2()
                .items_center()
                .child(comment_body(
                    SharedString::from(format!("pr-{number}-{}-{ix}", id.0)),
                    comment,
                    action,
                    action_color,
                    theme,
                ))
                .when(!url.is_empty(), |el| {
                    el.child(
                        Button::new(id)
                            .label("↗")
                            .xsmall()
                            .ghost()
                            .tooltip("Open on GitHub")
                            .on_click(cx.listener(move |this, _, _, cx| {
                                let r = launch::open_url(&url);
                                this.report(r, "Opened in browser", cx);
                            })),
                    )
                }),
        )
        .into_any_element()
}

fn comment_body(
    id: SharedString,
    comment: &Comment,
    action: &str,
    action_color: Option<Hsla>,
    theme: &gpui_kit::component::Theme,
) -> AnyElement {
    let body = visible_markdown(&comment.body);
    v_flex()
        .flex_1()
        .min_w_0()
        .gap_1()
        .child(
            h_flex()
                .gap_1()
                .text_xs()
                .text_color(theme.muted_foreground)
                .child(
                    div()
                        .text_color(theme.foreground)
                        .font_weight(FontWeight::SEMIBOLD)
                        .child(format!("@{}", comment.author)),
                )
                .when(!action.is_empty(), |el| {
                    el.child(
                        div()
                            .text_color(action_color.unwrap_or(theme.muted_foreground))
                            .child(action.to_string()),
                    )
                })
                .child(ago(comment.created_at)),
        )
        .when(!body.is_empty(), |el| {
            el.child(
                div()
                    .text_sm()
                    .child(TextView::markdown(id, body).selectable(true)),
            )
        })
        .into_any_element()
}

/// The last lines of a review comment's diff hunk: the commented line
/// and a little context above it.
fn diff_excerpt(hunk: &str, theme: &gpui_kit::component::Theme) -> AnyElement {
    const LINES: usize = 6;
    let lines: Vec<&str> = hunk
        .lines()
        .filter(|line| !line.starts_with("@@"))
        .collect();
    let start = lines.len().saturating_sub(LINES);
    v_flex()
        .px_3()
        .py_1()
        .border_b_1()
        .border_color(theme.border)
        .text_xs()
        .font_family(theme.mono_font_family.clone())
        .children(lines[start..].iter().map(|line| {
            let (bg, fg) = match line.chars().next() {
                Some('+') => (Some(theme.success.opacity(0.12)), theme.foreground),
                Some('-') => (Some(theme.danger.opacity(0.12)), theme.foreground),
                _ => (None, theme.muted_foreground),
            };
            div()
                .px_1()
                .truncate()
                .text_color(fg)
                .when_some(bg, |el, bg| el.bg(bg))
                .child(if line.is_empty() {
                    " ".to_string()
                } else {
                    line.to_string()
                })
        }))
        .into_any_element()
}

#[cfg(test)]
mod tests {
    use super::{PullRequest, merge_prs, pr_matches, pr_number_query, sort_prs, visible_markdown};
    use sg_github::{Checks, PrState};

    fn pr(number: u64, title: &str, state: PrState) -> PullRequest {
        PullRequest {
            created_at: None,
            merged_at: None,
            number,
            title: title.into(),
            head_branch: "feature/поиск".into(),
            head_oid: String::new(),
            head_repository: None,
            base_branch: "main".into(),
            state,
            url: String::new(),
            author: "Alice".into(),
            review_decision: String::new(),
            checks: Checks::default(),
            additions: 0,
            deletions: 0,
            updated_at: String::new(),
        }
    }

    #[test]
    fn search_matches_numbers_words_branches_and_authors() {
        let p = pr(123, "Каталог: быстрый поиск", PrState::Open);
        assert!(pr_matches(&p, ""));
        assert!(pr_matches(&p, "#12"));
        assert!(pr_matches(&p, "123"));
        assert!(!pr_matches(&p, "#13"));
        assert!(pr_matches(&p, "КАТАЛОГ поиск"));
        assert!(pr_matches(&p, "feature/по"));
        assert!(pr_matches(&p, "@alice"));
        assert!(!pr_matches(&p, "каталог bob"));
        assert_eq!(pr_number_query(" #42 "), Some(42));
        assert_eq!(pr_number_query("#"), None);
        assert_eq!(pr_number_query("4a"), None);
        assert!(super::searchable("#7") && super::searchable("ab"));
        assert!(!super::searchable("a") && !super::searchable("  "));
    }

    #[test]
    fn lists_merge_by_number_and_sort_open_first() {
        let mut prs = vec![pr(1, "old", PrState::Merged), pr(2, "a", PrState::Open)];
        merge_prs(
            &mut prs,
            [pr(1, "newer", PrState::Merged), pr(3, "b", PrState::Draft)],
        );
        assert_eq!(prs.len(), 3);
        assert_eq!(prs[0].title, "newer");
        sort_prs(&mut prs);
        let order: Vec<u64> = prs.iter().map(|p| p.number).collect();
        assert_eq!(order, [3, 2, 1]);
    }

    #[test]
    fn bodies_lose_html_comments() {
        assert_eq!(
            visible_markdown("<!-- template -->\nFixes it\n<!-- a -->ok<!-- open"),
            "Fixes it\nok"
        );
        assert_eq!(visible_markdown("plain"), "plain");
    }
}
