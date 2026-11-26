//! Projects: several repositories in one window, the top level of the
//! sidebar's tree. Only the active project polls Git and GitHub; the others
//! keep their last snapshot (read once in the background when there is none
//! yet), so their worktrees stay listed, switching back is instant, and their
//! native conversations keep running.

use super::*;
use gpui_kit::component::menu::DropdownMenu as _;

/// What a project showed when it was last active, or what a background read
/// found (see [`SuperGitApp::load_project`]).
pub(super) struct ProjectSnapshot {
    worktrees: Vec<WorktreeInfo>,
    prs: Vec<PullRequest>,
    prs_error: Option<String>,
    prs_loaded: bool,
    prs_fetched_at: Option<std::time::Instant>,
    observed_open_prs: HashMap<u64, worktrees::ObservedPr>,
    cleanup_attempted: HashSet<String>,
    creation_base: LoadState<sg_git::BaseInfo>,
}

impl ProjectSnapshot {
    fn loaded(worktrees: Vec<WorktreeInfo>) -> Self {
        Self {
            worktrees,
            prs: Vec::new(),
            prs_error: None,
            prs_loaded: false,
            prs_fetched_at: None,
            observed_open_prs: HashMap::new(),
            cleanup_attempted: HashSet::new(),
            creation_base: LoadState::Loading,
        }
    }
}

/// A background read of an inactive project for the sidebar.
pub(super) enum ProjectLoad {
    Loading { _task: Task<()> },
    Failed(String),
}

/// Native conversation activity across one project's worktrees.
#[derive(Default)]
pub(super) struct Activity {
    pub(super) agent: Option<AgentState>,
    /// The most urgent attention, how many conversations have it, and one
    /// of them to open: `(worktree, conversation id, its agent)`.
    pub(super) attention: Option<(Kind, usize, PathBuf, String, Provider)>,
}

/// The title bar's order: the first needs the user most.
pub(super) const ATTENTION_ORDER: [Kind; 5] = [
    Kind::Approval,
    Kind::Question,
    Kind::Failed,
    Kind::Completed,
    Kind::Running,
];

fn wants_attention(kind: Kind, state: &crate::attention::Attention) -> bool {
    state.kind == kind && (kind.is_live() || state.unread)
}

pub(super) use crate::model::project_name;

/// `root` with the home directory shortened to `~`.
pub(super) fn short_path(root: &Path) -> String {
    match std::env::var_os("HOME").map(PathBuf::from) {
        Some(home) if root.starts_with(&home) => match root.strip_prefix(&home) {
            Ok(rest) if rest.as_os_str().is_empty() => "~".into(),
            Ok(rest) => format!("~/{}", rest.display()),
            Err(_) => root.display().to_string(),
        },
        _ => root.display().to_string(),
    }
}

impl SuperGitApp {
    pub(super) fn active_root(&self) -> Option<&Path> {
        self.repo
            .as_ref()
            .map(|repo| repo.root.as_path())
            .or(self.folder.as_deref())
    }

    /// A project is open: a repository or a folder (else the welcome screen).
    pub(super) fn project_open(&self) -> bool {
        self.repo.is_some() || self.folder.is_some()
    }

    /// The active project, to load in the background.
    pub(super) fn active_project(&self) -> Option<model::Project> {
        match (&self.repo, &self.folder) {
            (Some(repo), _) => Some(model::Project::Repo(repo.clone())),
            (None, Some(root)) => Some(model::Project::Folder(root.clone())),
            (None, None) => None,
        }
    }

    /// `root` is a plain folder, not a Git repository: the active project
    /// knows; another one as it was last opened or read.
    pub(super) fn is_folder_project(&self, root: &Path) -> bool {
        if self.active_root() == Some(root) {
            return self.folder.is_some();
        }
        self.persisted.projects.is_folder(root)
    }

    /// Switching projects in the middle of a worktree operation would apply
    /// its result to the wrong repository.
    pub(super) fn git_operation_running(&self) -> bool {
        self.action_task.is_some()
            || self.cleanup_task.is_some()
            || !self.base_update_paths.is_empty()
    }

    /// Park the active project's data before another one takes over.
    pub(super) fn stash_project(&mut self) {
        let Some(root) = self.active_root().map(Path::to_path_buf) else {
            return;
        };
        if self.worktrees.is_empty() {
            return;
        }
        self.project_cache.insert(
            root,
            ProjectSnapshot {
                worktrees: std::mem::take(&mut self.worktrees),
                prs: std::mem::take(&mut self.prs),
                prs_error: self.prs_error.take(),
                prs_loaded: self.prs_loaded,
                prs_fetched_at: self.prs_fetched_at,
                observed_open_prs: std::mem::take(&mut self.observed_open_prs),
                cleanup_attempted: std::mem::take(&mut self.cleanup_attempted),
                creation_base: std::mem::replace(&mut self.creation_base, LoadState::Loading),
            },
        );
    }

    /// Show the active project's last snapshot right away (worktree list,
    /// selection, cached diffs); the restarted polling then refreshes it.
    pub(super) fn restore_project(&mut self, cx: &mut Context<Self>) {
        let Some(root) = self.active_root().map(Path::to_path_buf) else {
            return;
        };
        let Some(snapshot) = self.project_cache.remove(&root) else {
            return;
        };
        self.prs = snapshot.prs;
        self.prs_error = snapshot.prs_error;
        self.prs_loaded = snapshot.prs_loaded;
        self.prs_fetched_at = snapshot.prs_fetched_at;
        self.observed_open_prs = snapshot.observed_open_prs;
        self.cleanup_attempted = snapshot.cleanup_attempted;
        self.creation_base = snapshot.creation_base;
        self.apply_worktrees(Ok(snapshot.worktrees), cx);
    }

    /// An inactive project's last snapshot, as the sidebar's rows see it.
    pub(super) fn snapshot_scope(&self, root: &Path) -> Option<(Scope<'_>, &[WorktreeInfo])> {
        let snapshot = self.project_cache.get(root)?;
        Some((
            Scope {
                view: self.view_of(root),
                prs: &snapshot.prs,
                prs_fresh: snapshot.prs_error.is_none() && snapshot.prs_fetched_at.is_some(),
                active: false,
            },
            &snapshot.worktrees,
        ))
    }

    /// Why an inactive project without a snapshot lists no worktrees.
    pub(super) fn project_load_error(&self, root: &Path) -> Option<&str> {
        match self.project_loads.get(root)? {
            ProjectLoad::Failed(error) => Some(error),
            ProjectLoad::Loading { .. } => None,
        }
    }

    /// Read the unfolded inactive projects that have no snapshot yet.
    pub(super) fn load_inactive_projects(&mut self, cx: &mut Context<Self>) {
        let roots: Vec<PathBuf> = self
            .persisted
            .projects
            .roots
            .iter()
            .filter(|root| {
                self.active_root() != Some(root.as_path())
                    && !self.persisted.projects.is_folded(root)
                    && !self.project_cache.contains_key(*root)
                    && !self.project_loads.contains_key(*root)
            })
            .cloned()
            .collect();
        for root in roots {
            self.load_project(root, cx);
        }
    }

    /// Read an inactive project's worktrees, then its pull requests, in the
    /// background so the sidebar can list them: once when it has no snapshot
    /// yet and again each time it is unfolded. It does not poll.
    pub(super) fn load_project(&mut self, root: PathBuf, cx: &mut Context<Self>) {
        if self.active_root() == Some(root.as_path())
            || matches!(
                self.project_loads.get(&root),
                Some(ProjectLoad::Loading { .. })
            )
        {
            return;
        }
        let index = self.index.clone();
        let request = root.clone();
        // A folder has no pull requests: no `gh` call for it.
        let folder_hint = self.persisted.projects.is_folder(&root);
        let task = cx.spawn(async move |this, cx| {
            let worktrees = cx.background_executor().spawn({
                let root = request.clone();
                async move {
                    let project = model::Project::open(&root)?;
                    let folder = matches!(project, model::Project::Folder(_));
                    project.load(&index).map(|worktrees| (worktrees, folder))
                }
            });
            // Next to the worktrees.
            let prs = cx.background_executor().spawn({
                let root = request.clone();
                async move {
                    if folder_hint {
                        Err("Pull requests load once the project is open".to_string())
                    } else if sg_github::gh_available() {
                        sg_github::list_prs(&root, 100, 30).map_err(|e| e.to_string())
                    } else {
                        Err("gh CLI not found — install it for pull requests".to_string())
                    }
                }
            });
            let worktrees = worktrees.await;
            let folder = worktrees.as_ref().is_ok_and(|(_, folder)| *folder);
            let listed = this
                .update(cx, |this, cx| {
                    this.apply_project_worktrees(
                        &request,
                        worktrees.map(|(worktrees, _)| worktrees),
                        folder,
                        cx,
                    )
                })
                .unwrap_or(false);
            if !listed || folder {
                if folder {
                    this.update(cx, |this, _| this.project_loads.remove(&request))
                        .ok();
                }
                return;
            }
            let prs = prs.await;
            this.update(cx, |this, cx| this.apply_project_prs(&request, prs, cx))
                .ok();
        });
        self.project_loads
            .insert(root, ProjectLoad::Loading { _task: task });
    }

    /// Whether the read goes on to pull requests.
    fn apply_project_worktrees(
        &mut self,
        root: &Path,
        result: anyhow::Result<Vec<WorktreeInfo>>,
        folder: bool,
        cx: &mut Context<Self>,
    ) -> bool {
        // Opened meanwhile (it polls itself now) or removed.
        if self.active_root() == Some(root)
            || !self.persisted.projects.roots.iter().any(|r| r == root)
        {
            self.project_loads.remove(root);
            return false;
        }
        match result {
            Ok(worktrees) => {
                if self.persisted.projects.is_folder(root) != folder {
                    self.persisted.projects.set_folder(root, folder);
                    self.queue_persist(cx);
                }
                match self.project_cache.get_mut(root) {
                    Some(snapshot) => snapshot.worktrees = worktrees,
                    None => {
                        self.project_cache
                            .insert(root.to_path_buf(), ProjectSnapshot::loaded(worktrees));
                    }
                }
                cx.notify();
                true
            }
            Err(error) => {
                self.project_loads.insert(
                    root.to_path_buf(),
                    ProjectLoad::Failed(model::git_error_message(error)),
                );
                cx.notify();
                false
            }
        }
    }

    fn apply_project_prs(
        &mut self,
        root: &Path,
        result: Result<Vec<PullRequest>, String>,
        cx: &mut Context<Self>,
    ) {
        self.project_loads.remove(root);
        if self.active_root() == Some(root) {
            return;
        }
        let Some(snapshot) = self.project_cache.get_mut(root) else {
            return;
        };
        snapshot.prs_loaded = true;
        match result {
            Ok(prs) => {
                snapshot.prs = prs;
                snapshot.prs_error = None;
                snapshot.prs_fetched_at = Some(std::time::Instant::now());
            }
            Err(error) => snapshot.prs_error = Some(error),
        }
        cx.notify();
    }

    pub(super) fn switch_project(&mut self, root: PathBuf, cx: &mut Context<Self>) {
        if self.active_root() == Some(root.as_path()) {
            return;
        }
        self.open_path(root, cx);
    }

    /// Remove a project from the panel. Its conversations are saved (adding
    /// the project again restores them) and their idle processes stop.
    pub(super) fn remove_project(&mut self, root: PathBuf, cx: &mut Context<Self>) {
        let busy = self.project_chats(&root).any(|(_, chat)| {
            matches!(
                chat.read(cx).status(),
                sg_agents::chat::Status::Working
                    | sg_agents::chat::Status::Starting
                    | sg_agents::chat::Status::NeedsPermission
                    | sg_agents::chat::Status::NeedsInput
            )
        });
        if busy {
            self.flash(
                format!(
                    "An agent is working in {} — stop it first",
                    project_name(&root)
                ),
                true,
                cx,
            );
            return;
        }
        let active = self.active_root() == Some(root.as_path());
        if active && self.git_operation_running() {
            self.flash("Wait for the running Git operation first", true, cx);
            return;
        }
        self.persist_workspace(cx);
        let paths: Vec<PathBuf> = self
            .chat_projects
            .iter()
            .filter(|(_, owner)| **owner == root)
            .map(|(path, _)| path.clone())
            .collect();
        for path in &paths {
            self.chats.remove(path);
            self.active_chat.remove(path);
            self.chat_projects.remove(path);
            self.review_views.remove(path);
        }
        self.drop_project_shells(&root);
        self.project_avatars.remove(&root);
        if let Some(snapshot) = self.project_cache.remove(&root) {
            for info in &snapshot.worktrees {
                self.review_views.remove(info.path());
            }
        }
        self.project_loads.remove(&root);
        self.persisted.projects.set_folded(&root, false);
        let roots = &mut self.persisted.projects.roots;
        let ix = roots.iter().position(|known| *known == root);
        roots.retain(|known| *known != root);
        if active {
            let next = ix.and_then(|ix| roots.get(ix).or(roots.last()).cloned());
            for info in &self.worktrees {
                self.review_views.remove(info.path());
            }
            if !next.is_some_and(|next| self.open_path(next, cx)) {
                self.leave_project(cx);
            }
        }
        self.queue_persist(cx);
        cx.notify();
    }

    /// Live conversations that belong to `root`'s worktrees.
    fn project_chats<'a>(
        &'a self,
        root: &'a Path,
    ) -> impl Iterator<Item = (&'a PathBuf, &'a Entity<ChatSession>)> + 'a {
        self.chats
            .iter()
            .filter(move |(path, _)| self.chat_projects.get(*path).is_some_and(|r| r == root))
            .flat_map(|(path, chats)| chats.iter().map(move |chat| (path, chat)))
    }

    /// Exact activity of the project's native conversations: live ones, and
    /// saved ones (unread results) for worktrees not opened this run.
    pub(super) fn project_activity(&self, root: &Path, cx: &App) -> Activity {
        let mut states: Vec<(PathBuf, String, crate::attention::Attention, Provider)> = self
            .project_chats(root)
            .map(|(path, chat)| {
                let chat = chat.read(cx);
                (
                    path.clone(),
                    chat.local_id().to_string(),
                    chat.attention().clone(),
                    chat.provider(),
                )
            })
            .collect();
        if let Some(view) = self.persisted.repositories.get(root) {
            for (path, saved) in &view.conversations {
                if self.chats.contains_key(path) {
                    continue;
                }
                states.extend(saved.iter().filter_map(|chat| {
                    Some((
                        path.clone(),
                        chat.local_id.clone(),
                        chat.attention.clone(),
                        Provider::parse(&chat.provider)?,
                    ))
                }));
            }
        }
        let agent =
            self.project_chats(root)
                .filter_map(|(_, chat)| match chat.read(cx).status() {
                    sg_agents::chat::Status::NeedsPermission
                    | sg_agents::chat::Status::NeedsInput => Some(AgentState::NeedsAttention),
                    sg_agents::chat::Status::Working | sg_agents::chat::Status::Starting => {
                        Some(AgentState::Working)
                    }
                    _ => None,
                })
                .min();
        let attention = ATTENTION_ORDER.into_iter().find_map(|kind| {
            let mut matching = states
                .iter()
                .filter(|(_, _, state, _)| wants_attention(kind, state));
            let (path, id, _, provider) = matching.next()?;
            Some((
                kind,
                matching.count() + 1,
                path.clone(),
                id.clone(),
                *provider,
            ))
        });
        Activity { agent, attention }
    }

    /// Open the conversation behind a project's attention badge.
    pub(super) fn open_project_attention(
        &mut self,
        root: PathBuf,
        worktree: PathBuf,
        conversation: String,
        provider: Provider,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        self.open_notification(
            notifications::Target {
                repository: root,
                worktree,
                provider: provider.as_str().into(),
                conversation,
            },
            window,
            cx,
        );
    }

    /// The sidebar is shown once there is a project to list.
    pub(super) fn projects_visible(&self) -> bool {
        self.project_open() || !self.persisted.projects.roots.is_empty()
    }

    /// "+": recent repositories not in the sidebar yet, and the folder dialog.
    pub(super) fn add_project_button(
        &self,
        button: Button,
        cx: &mut Context<Self>,
    ) -> impl IntoElement {
        let owner = cx.entity().downgrade();
        let recent: Vec<PathBuf> = self
            .recent
            .iter()
            .filter(|root| !self.persisted.projects.roots.contains(root))
            .cloned()
            .collect();
        button.dropdown_menu(move |mut menu, _, _| {
            for root in &recent {
                let owner = owner.clone();
                let target = root.clone();
                menu = menu.item(
                    PopupMenuItem::new(format!("{}  {}", project_name(root), short_path(root)))
                        .on_click(move |_, _, cx| {
                            let _ = owner.update(cx, |this, cx| this.open_path(target.clone(), cx));
                        }),
                );
            }
            if !recent.is_empty() {
                menu = menu.separator();
            }
            let owner = owner.clone();
            menu.item(
                PopupMenuItem::new("Open Folder…").on_click(move |_, window, cx| {
                    let _ = owner.update(cx, |this, cx| {
                        this.open_repository(&OpenRepository, window, cx)
                    });
                }),
            )
        })
    }

    pub(super) fn project_menu(
        &self,
        row: Stateful<Div>,
        root: PathBuf,
        cx: &mut Context<Self>,
    ) -> AnyElement {
        let owner = cx.entity().downgrade();
        row.context_menu(move |menu, _, _| {
            let reveal = root.clone();
            let remove_owner = owner.clone();
            let remove = root.clone();
            menu.item(
                PopupMenuItem::new(format!("Reveal in {}", launch::FILE_MANAGER)).on_click(
                    move |_, _, _| {
                        let _ = launch::reveal_in_finder(&reveal);
                    },
                ),
            )
            .item(
                PopupMenuItem::new("Remove from Projects").on_click(move |_, _, cx| {
                    let _ =
                        remove_owner.update(cx, |this, cx| this.remove_project(remove.clone(), cx));
                }),
            )
        })
        .into_any_element()
    }
}

#[cfg(test)]
mod tests {
    // Not `super::*`: GPUI's glob import brings its own `test` attribute.
    use super::{project_name, short_path};
    use std::path::{Path, PathBuf};

    #[test]
    fn project_paths_are_shortened_under_home() {
        let Some(home) = std::env::var_os("HOME").map(PathBuf::from) else {
            return;
        };
        assert_eq!(short_path(&home), "~");
        assert_eq!(short_path(&home.join("src/repo")), "~/src/repo");
        assert_eq!(short_path(Path::new("/opt/repo")), "/opt/repo");
        assert_eq!(project_name(Path::new("/opt/repo")), "repo");
    }
}
