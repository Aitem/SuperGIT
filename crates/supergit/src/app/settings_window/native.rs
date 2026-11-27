//! The settings window on macOS, drawn by AppKit (`appkit`). This side is
//! GPUI's: what the window shows (`State`, read from the app and
//! `settings.json`) and what it reports (`Event`, applied to the app).
//! AppKit calls its controls' actions on the main thread but outside GPUI's
//! updates, so events come through a channel to a GPUI task; while the
//! window shows, every change of the app sends it the new state.

use futures::StreamExt as _;
use futures::channel::mpsc::{UnboundedSender, unbounded};

use super::*;
use crate::initial_prompt::Place;
use crate::model::project_name;
use crate::settings::{AgentView, Appearance};

mod appkit;

/// What the window shows.
#[derive(Clone, Debug, PartialEq)]
pub(super) struct State {
    pub appearance: Appearance,
    pub version: String,
    pub agent_view: AgentView,
    /// The saved template (the field keeps what is typed).
    pub initial_prompt: String,
    /// The selected worktree, for the prompt's preview: its name and place.
    pub prompt_place: Option<(String, Place)>,
    /// Approvals and questions first, then completions and failures.
    pub sounds: [Option<String>; 2],
    pub banners: [bool; 2],
    /// Why the last change could not be made (banners not allowed).
    pub problem: Option<String>,
    pub diff_layout: DiffLayout,
    pub difftastic: bool,
    pub difftastic_available: bool,
    pub difftastic_status: String,
    pub repository: Option<Repository>,
}

/// The open repository's cleanup.
#[derive(Clone, Debug, PartialEq)]
pub(super) struct Repository {
    pub name: String,
    pub auto_cleanup: bool,
    /// Local branches, and whether cleanup must keep each.
    pub branches: Vec<(String, bool)>,
}

/// What the window reports.
#[derive(Debug)]
pub(super) enum Event {
    Appearance(Appearance),
    CheckForUpdates,
    AgentView(AgentView),
    InitialPrompt(String),
    Sound {
        needs_you: bool,
        name: Option<String>,
    },
    Banner {
        needs_you: bool,
        on: bool,
    },
    DiffLayout(DiffLayout),
    Difftastic(bool),
    AutoCleanup(bool),
    Protect {
        branch: String,
        protected: bool,
    },
    Closed,
}

/// GPUI's side of the window.
struct Bridge {
    app: WeakEntity<SuperGitApp>,
    events: UnboundedSender<Event>,
    /// Follows the app while the window shows.
    observing: Option<Subscription>,
    problem: Option<String>,
}

impl Global for Bridge {}

pub(super) fn open(app: WeakEntity<SuperGitApp>, page: Option<Page>, focus: bool, cx: &mut App) {
    let Some(entity) = app.upgrade() else {
        return;
    };
    if !cx.has_global::<Bridge>() {
        let (events, mut received) = unbounded();
        cx.spawn(async move |cx| {
            while let Some(event) = received.next().await {
                cx.update(|cx| handle(event, cx));
            }
        })
        .detach();
        cx.set_global(Bridge {
            app: app.clone(),
            events,
            observing: None,
            problem: None,
        });
    }
    let observing = cx.observe(&entity, |_, cx| push(cx));
    let bridge = cx.global_mut::<Bridge>();
    bridge.app = app;
    bridge.problem = None;
    bridge.observing = Some(observing);
    let events = bridge.events.clone();
    if let Some(state) = snapshot(cx) {
        appkit::show(&state, page, focus, events);
    }
    #[cfg(feature = "screenshot")]
    if std::env::var_os("SUPERGIT_DEBUG_SETTINGS_CLICK").is_some() {
        cx.spawn(async |cx| {
            cx.background_executor()
                .timer(std::time::Duration::from_secs(2))
                .await;
            cx.update(|_| appkit::debug_click());
        })
        .detach();
    }
}

#[cfg(feature = "screenshot")]
pub(super) fn capture(path: &str) -> anyhow::Result<()> {
    appkit::capture(path)
}

/// The window's state, from the app.
fn snapshot(cx: &App) -> Option<State> {
    let bridge = cx.try_global::<Bridge>()?;
    let entity = bridge.app.upgrade()?;
    let app = entity.read(cx);
    let settings = crate::settings::load();
    let prompt_place = app
        .active_root()
        .zip(app.selected_info())
        .map(|(root, info)| (info.name().to_string(), SuperGitApp::place(root, info)));
    let repository = app.repo.as_ref().map(|repo| {
        let view = app.repository_view();
        let mut names = view.protected_branches.clone();
        names.extend(
            app.worktrees
                .iter()
                .filter_map(|info| info.wt.branch.clone()),
        );
        names.sort();
        names.dedup();
        Repository {
            name: project_name(&repo.root),
            auto_cleanup: view.auto_cleanup,
            branches: names
                .into_iter()
                .map(|name| {
                    let protected = view.protected_branches.contains(&name);
                    (name, protected)
                })
                .collect(),
        }
    });
    Some(State {
        appearance: crate::appearance::current(cx),
        version: crate::updates::running_version(),
        agent_view: app.agent_view,
        initial_prompt: settings.initial_prompt().to_string(),
        prompt_place,
        sounds: [true, false].map(|needs_you| settings.sound(needs_you).map(str::to_string)),
        banners: [settings.notify_approvals, settings.notify_completions],
        problem: bridge.problem.clone(),
        diff_layout: app.diff_layout,
        difftastic: app.difftastic_active(),
        difftastic_available: app.difft_available(),
        difftastic_status: match &app.difft_version {
            None => "Looking for difftastic…".to_string(),
            Some(None) => "difft was not found on PATH. Install it with `brew install \
                           difftastic`, then reopen SuperGIT."
                .to_string(),
            Some(Some(version)) => format!(
                "difftastic {version} compares syntax trees: only changed tokens are \
                 highlighted, and reformatting shows as no change."
            ),
        },
        repository,
    })
}

/// Send the window the app's state, while it shows.
fn push(cx: &mut App) {
    if let Some(state) = snapshot(cx) {
        appkit::update(&state);
    }
}

fn handle(event: Event, cx: &mut App) {
    let Some(app) = cx
        .try_global::<Bridge>()
        .and_then(|bridge| bridge.app.upgrade())
    else {
        return;
    };
    match event {
        Event::Appearance(appearance) => crate::appearance::set(appearance, cx),
        Event::CheckForUpdates => app.update(cx, |app, cx| app.check_updates_now(cx)),
        Event::AgentView(view) => app.update(cx, |app, cx| app.set_agent_view(view, cx)),
        Event::InitialPrompt(text) => save_prompt(&text),
        Event::Sound { needs_you, name } => set_sound(needs_you, name.as_deref()),
        Event::Banner { needs_you, on } => {
            let problem = set_banner(needs_you, on, cx)
                .err()
                .map(|error| error.to_string());
            cx.global_mut::<Bridge>().problem = problem;
        }
        Event::DiffLayout(layout) => app.update(cx, |app, cx| app.set_diff_layout(layout, cx)),
        Event::Difftastic(on) => app.update(cx, |app, cx| app.set_difftastic(on, cx)),
        Event::AutoCleanup(on) => app.update(cx, |app, cx| {
            if app.repository_view().auto_cleanup != on {
                app.toggle_auto_cleanup(cx);
            }
        }),
        Event::Protect { branch, protected } => app.update(cx, |app, cx| {
            if app.repository_view().protected_branches.contains(&branch) != protected {
                app.toggle_cleanup_protection(&branch, cx);
            }
        }),
        Event::Closed => {
            let bridge = cx.global_mut::<Bridge>();
            bridge.observing = None;
            bridge.problem = None;
            return;
        }
    }
    push(cx);
}
