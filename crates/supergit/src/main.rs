mod agent_context;
mod app;
mod appearance;
mod attention;
mod avatars;
mod chat;
mod difft_view;
mod dump;
mod file_tree;
mod graph;
mod icons;
mod launch;
mod lifecycle;
mod model;
mod notifications;
mod recent;
#[cfg(feature = "screenshot")]
mod screenshot;
mod settings;
mod shell_env;
mod submodules;
mod switcher;
mod syntax;
mod workspace;

use std::path::PathBuf;

use anyhow::{Context as _, Result};
use futures::StreamExt as _;
use gpui_kit::component::TitleBar;
use gpui_kit::*;

actions!(supergit, [Quit]);

const USAGE: &str = "\
SuperGIT — Git GUI for agent-driven development

USAGE:
    supergit [PATH]          open the repository containing PATH (default: cwd,
                             else the last opened repository)
    supergit --dump [PATH]   print worktrees, agent sessions and PRs as text

ENV:
    SUPERGIT_TERMINAL   ghostty | iterm | terminal (default: first installed)
    SUPERGIT_EDITOR     editor CLI for \"Editor\" (default: VS Code)
";

fn main() -> Result<()> {
    // Before anything else (and before any thread): a Dock/Finder launch has
    // no PATH to Homebrew, `gh` or `claude` until we take the shell's.
    shell_env::import_if_needed();

    let args: Vec<String> = std::env::args().skip(1).collect();
    if args.iter().any(|a| a == "-h" || a == "--help") {
        std::io::Write::write_all(&mut std::io::stdout(), USAGE.as_bytes())?;
        return Ok(());
    }
    let dump = args.iter().any(|a| a == "--dump");
    // macOS may pass `-psn_…` to bundled apps; only plain arguments are paths.
    let explicit = args.iter().find(|a| !a.starts_with('-'));

    let (repo, launch_dir) = match explicit {
        Some(p) => {
            let dir = std::fs::canonicalize(p).with_context(|| format!("no such path: {p}"))?;
            (Some(sg_git::Repo::discover(&dir)?), dir)
        }
        None => {
            let cwd = std::env::current_dir().unwrap_or_else(|_| PathBuf::from("/"));
            match sg_git::Repo::discover(&cwd) {
                Ok(repo) => (Some(repo), cwd),
                // Started from the Dock (cwd is `/`): reopen the last repository.
                Err(_) => match recent::load()
                    .into_iter()
                    .find_map(|p| sg_git::Repo::discover(&p).ok())
                {
                    Some(repo) => {
                        let dir = repo.root.clone();
                        (Some(repo), dir)
                    }
                    None => (None, cwd),
                },
            }
        }
    };

    if dump {
        let repo = repo.context("not inside a git repository")?;
        return dump::run(&repo);
    }
    if let Some(repo) = &repo {
        recent::remember(&repo.root);
    }

    let app = gpui_kit::application().with_assets(icons::AppAssets);
    // Folders dropped on the Dock icon or `open -a SuperGIT <dir>`.
    let (open_tx, mut open_rx) = futures::channel::mpsc::unbounded::<PathBuf>();
    app.on_open_urls(move |urls| {
        for path in urls.iter().filter_map(|u| launch::file_url_to_path(u)) {
            let _ = open_tx.unbounded_send(path);
        }
    });

    app.run(move |cx| {
        gpui_kit::init(cx);
        appearance::init(cx);
        let mut notifications = notifications::init(cx);

        cx.bind_keys([
            KeyBinding::new("cmd-q", Quit, None),
            KeyBinding::new("cmd-r", app::Refresh, None),
            KeyBinding::new("cmd-o", app::OpenRepository, None),
            KeyBinding::new("cmd-shift-o", app::CloseRepository, None),
            KeyBinding::new("cmd-l", app::ToggleClaude, None),
            KeyBinding::new("cmd-b", app::ToggleSidebar, None),
            KeyBinding::new("cmd-n", app::NewClaudeChat, None),
            KeyBinding::new("cmd-p", app::OpenSwitcher, None),
            KeyBinding::new("cmd-,", app::OpenSettings, None),
            KeyBinding::new("cmd-shift-a", app::AskAgent, None),
            KeyBinding::new("up", app::SwitcherPrevious, Some("Switcher")),
            KeyBinding::new("down", app::SwitcherNext, Some("Switcher")),
            KeyBinding::new("escape", app::CloseSwitcher, Some("Switcher")),
        ]);
        cx.on_action(|_: &Quit, cx| cx.quit());
        set_app_menus(cx);
        cx.on_window_closed(|cx, _| {
            if cx.windows().is_empty() {
                cx.quit();
            }
        })
        .detach();

        let mut bounds = Bounds::centered(None, size(px(1440.), px(900.)), cx);
        if let Some(saved) = workspace::load().window {
            let display = cx
                .displays()
                .into_iter()
                .find(|display| display.bounds().contains(&point(px(saved.x), px(saved.y))))
                .or_else(|| cx.primary_display());
            if let Some(display) = display {
                let available = display.bounds();
                let restored = saved.clamped(workspace::WindowRect {
                    x: available.origin.x.as_f32(),
                    y: available.origin.y.as_f32(),
                    width: available.size.width.as_f32(),
                    height: available.size.height.as_f32(),
                });
                bounds = Bounds::new(
                    point(px(restored.x), px(restored.y)),
                    size(px(restored.width), px(restored.height)),
                );
            }
        }
        // Screenshot runs must not steal focus (or keystrokes) from the user.
        let headless = std::env::var_os("SUPERGIT_SCREENSHOT").is_some();
        let options = WindowOptions {
            focus: !headless,
            window_bounds: Some(WindowBounds::Windowed(bounds)),
            window_min_size: Some(size(
                px(1100_f32.min(bounds.size.width.as_f32())),
                px(600_f32.min(bounds.size.height.as_f32())),
            )),
            ..TitleBar::window_options()
        };
        let (_handle, view) = gpui_kit::open_window(options, cx, |window, cx| {
            cx.new(|cx| app::SuperGitApp::new(repo, launch_dir, window, cx))
        })
        .expect("failed to open the main window");
        #[cfg(feature = "screenshot")]
        screenshot::schedule(_handle, cx);

        // Global navigation must also work when no child control owns focus.
        let switcher_view = view.downgrade();
        cx.on_action(move |_: &app::OpenSwitcher, cx| {
            let _ = _handle.update(cx, |_, window, cx| {
                switcher_view
                    .update(cx, |app, cx| {
                        app.open_switcher(&app::OpenSwitcher, window, cx)
                    })
                    .ok();
            });
        });

        let notification_view = view.downgrade();
        cx.spawn(async move |cx| {
            while let Some(target) = notifications.next().await {
                let _ = cx.update_window(_handle, |_, window, cx| {
                    notification_view
                        .update(cx, |app, cx| app.open_notification(target, window, cx))
                        .ok();
                });
            }
        })
        .detach();
        let view = view.downgrade();
        cx.spawn(async move |cx| {
            while let Some(path) = open_rx.next().await {
                if view.update(cx, |app, cx| app.open_path(path, cx)).is_err() {
                    break;
                }
            }
        })
        .detach();
        if !headless {
            cx.activate(true);
        }
    });
    Ok(())
}

/// The menu bar. Rebuilt when a checkmark changes (appearance).
pub(crate) fn set_app_menus(cx: &mut App) {
    cx.set_menus([
        Menu::new("SuperGIT").items([
            MenuItem::action("Settings…", app::OpenSettings),
            MenuItem::separator(),
            MenuItem::os_submenu("Services", SystemMenuType::Services),
            MenuItem::separator(),
            MenuItem::action("Quit SuperGIT", Quit),
        ]),
        Menu::new("File").items([
            MenuItem::action("Open Repository as Project…", app::OpenRepository),
            MenuItem::action("Recent Repositories", app::CloseRepository),
            MenuItem::separator(),
            MenuItem::action("Refresh", app::Refresh),
        ]),
        Menu::new("View").items([
            MenuItem::action("Ask Agent About Diff Selection", app::AskAgent),
            MenuItem::action(
                "Ask Agent in New Conversation",
                app::AskAgentNewConversation,
            ),
            MenuItem::separator(),
            MenuItem::action(
                "Switch Worktree, Conversation or Project…",
                app::OpenSwitcher,
            ),
            MenuItem::separator(),
            MenuItem::action("Toggle Sidebar", app::ToggleSidebar),
            MenuItem::action("Toggle Agent Panel", app::ToggleClaude),
            MenuItem::action("New Agent Conversation", app::NewClaudeChat),
            MenuItem::separator(),
            MenuItem::submenu(Menu::new("Appearance").items(appearance::menu_items(cx))),
        ]),
        Menu::new("Worktree").items([
            MenuItem::action("Toggle Keep Active", app::ToggleKeepWorktree),
            MenuItem::action("Clean Up Worktree…", app::CleanUpWorktree),
            MenuItem::action("Update Worktree Base", app::UpdateBase),
        ]),
    ]);
}
