mod agent_context;
mod app;
mod appearance;
mod attention;
mod avatars;
mod chat;
#[cfg(test)]
mod contrast;
mod difft_view;
mod dump;
mod file_tree;
mod graph;
mod icons;
mod initial_prompt;
mod launch;
mod lifecycle;
mod model;
mod notifications;
mod recent;
#[cfg(feature = "screenshot")]
mod screenshot;
mod selectable;
mod settings;
mod shell_env;
mod single_instance;
mod sound;
mod submodules;
mod switcher;
mod syntax;
mod terminal;
mod updates;
mod workspace;

use std::path::PathBuf;

use anyhow::{Context as _, Result};
use futures::StreamExt as _;
use gpui_kit::component::TitleBar;
use gpui_kit::component::menu::AppMenuBar;
use gpui_kit::*;

actions!(supergit, [Quit, Hide, HideOthers, ShowAll, Minimize]);

/// How long the pointer rests on something before its tooltip shows: the
/// components' default (`TooltipDefaults`), and set on every element
/// tooltip, whose own default is shorter.
pub const TOOLTIP_DELAY: std::time::Duration = std::time::Duration::from_secs(1);

/// A keystroke with the primary modifier, as a `'static` keymap string:
/// `shortcut!("shift-b")` is `cmd-shift-b` on macOS, `ctrl-shift-b` elsewhere.
/// (`KeyBinding::new` cannot resolve gpui's `secondary-`.)
macro_rules! shortcut {
    ($keys:literal) => {
        if cfg!(target_os = "macos") {
            concat!("cmd-", $keys)
        } else {
            concat!("ctrl-", $keys)
        }
    };
}

/// The primary shortcut modifier as `shortcut!` spells it, for tooltips.
pub(crate) const MOD: &str = if cfg!(target_os = "macos") {
    "cmd"
} else {
    "ctrl"
};

const USAGE: &str = "\
SuperGIT — Git GUI for agent-driven development

USAGE:
    supergit [PATH]          open the repository containing PATH, else PATH as
                             a folder project without Git (default: the
                             repository at cwd, else the last opened project)
    supergit --dump [PATH]   print worktrees, agent sessions and PRs as text

ENV:
    SUPERGIT_EDITOR     editor CLI for \"Editor\" (default: VS Code)
    SUPERGIT_NEW_INSTANCE  Linux: start a separate instance instead of handing
                        the folder to the running one
";

/// Keys inside the embedded terminal. Copy and paste are its own actions;
/// keys that outer bindings would take (Tab moves focus, and on Linux the
/// app's `ctrl-` shortcuts and `ctrl-c`) are released to the program:
/// `NoAction` in the deeper context outranks them. `ctrl-j` stays the app's
/// on Linux, as in VS Code: it hides the terminal panel from inside it.
fn bind_terminal_keys(cx: &mut App) {
    let context = Some(terminal::KEY_CONTEXT);
    cx.bind_keys([
        KeyBinding::new("tab", NoAction {}, context),
        KeyBinding::new("shift-tab", NoAction {}, context),
        // Text size, as in every terminal: `=` is `+` without Shift.
        KeyBinding::new(shortcut!("="), terminal::IncreaseFontSize, context),
        KeyBinding::new(shortcut!("+"), terminal::IncreaseFontSize, context),
        KeyBinding::new(shortcut!("-"), terminal::DecreaseFontSize, context),
        KeyBinding::new(shortcut!("0"), terminal::ResetFontSize, context),
    ]);
    if cfg!(target_os = "macos") {
        cx.bind_keys([
            KeyBinding::new("cmd-c", terminal::Copy, context),
            KeyBinding::new("cmd-v", terminal::Paste, context),
            KeyBinding::new("cmd-a", terminal::SelectAll, context),
        ]);
    } else {
        // Ctrl+Shift+C/V copy and paste; plain Ctrl+letters reach the TUI.
        cx.bind_keys([
            KeyBinding::new("ctrl-shift-c", terminal::Copy, context),
            KeyBinding::new("ctrl-shift-v", terminal::Paste, context),
            KeyBinding::new("ctrl-shift-a", terminal::SelectAll, context),
        ]);
        cx.bind_keys(
            [
                "ctrl-c", "ctrl-v", "ctrl-r", "ctrl-o", "ctrl-l", "ctrl-b", "ctrl-n", "ctrl-p",
            ]
            .map(|keys| KeyBinding::new(keys, NoAction {}, context)),
        );
    }
}

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

    let (project, launch_dir) = match explicit {
        // A repository, or any folder as a folder project.
        Some(p) => {
            let dir = std::fs::canonicalize(p).with_context(|| format!("no such path: {p}"))?;
            (Some(model::Project::open(&dir)?), dir)
        }
        None => {
            let cwd = std::env::current_dir().unwrap_or_else(|_| PathBuf::from("/"));
            // Only a repository: the folder a terminal happens to be in (or
            // the Dock's `/`) is no project anyone asked for.
            match sg_git::Repo::discover(&cwd) {
                Ok(repo) => (Some(model::Project::Repo(repo)), cwd),
                // Started from the Dock (cwd is `/`): reopen the last project.
                Err(_) => match recent::load()
                    .into_iter()
                    .find_map(|p| model::Project::open(&p).ok())
                {
                    Some(project) => {
                        let dir = project.root().to_path_buf();
                        (Some(project), dir)
                    }
                    None => (None, cwd),
                },
            }
        }
    };

    if dump {
        return match project {
            Some(model::Project::Repo(repo)) => dump::run(&repo),
            Some(model::Project::Folder(root)) => {
                anyhow::bail!("{} is not a Git repository", root.display())
            }
            None => anyhow::bail!("not inside a git repository"),
        };
    }
    // Screenshot runs are throwaway windows: never hand off to, or act as,
    // the user's instance.
    let headless = std::env::var_os("SUPERGIT_SCREENSHOT").is_some();
    let listener = if headless {
        None
    } else {
        // A launcher click (no repository here) only raises the window.
        match single_instance::claim(project.is_some().then_some(launch_dir.as_path())) {
            single_instance::Role::Forwarded => return Ok(()),
            single_instance::Role::Primary(listener) => listener,
        }
    };
    if let Some(project) = &project {
        recent::remember(project.root());
    }

    let app = gpui_kit::application().with_assets(icons::AppAssets);
    // Folders dropped on the Dock icon or `open -a SuperGIT <dir>`; on Linux
    // later launches (`supergit <dir>`, "Open with"). `None` only raises.
    let (open_tx, mut open_rx) = futures::channel::mpsc::unbounded::<Option<PathBuf>>();
    if let Some(listener) = listener {
        let open_tx = open_tx.clone();
        single_instance::serve(listener, move |folder| {
            let _ = open_tx.unbounded_send(folder);
        });
    }
    app.on_open_urls(move |urls| {
        for path in urls.iter().filter_map(|u| launch::file_url_to_path(u)) {
            let _ = open_tx.unbounded_send(Some(path));
        }
    });

    app.run(move |cx| {
        gpui_kit::init(cx);
        gpui_kit::base::TooltipDefaults::new()
            .with_show_delay(TOOLTIP_DELAY)
            .install(cx);
        appearance::init(cx);
        chat::init(cx);
        terminal::init(cx);
        let mut notifications = notifications::init(cx);

        cx.bind_keys([
            KeyBinding::new(shortcut!("q"), Quit, None),
            KeyBinding::new(shortcut!("r"), app::Refresh, None),
            KeyBinding::new(shortcut!("o"), app::OpenRepository, None),
            KeyBinding::new(shortcut!("shift-o"), app::CloseRepository, None),
            KeyBinding::new(shortcut!("l"), app::ToggleClaude, None),
            KeyBinding::new(shortcut!("b"), app::ToggleSidebar, None),
            KeyBinding::new(shortcut!("j"), app::ToggleTerminal, None),
            KeyBinding::new(shortcut!("shift-h"), app::ToggleHistory, None),
            KeyBinding::new(shortcut!("n"), app::NewClaudeChat, None),
            KeyBinding::new(shortcut!("p"), app::OpenSwitcher, None),
            KeyBinding::new(shortcut!(","), app::OpenSettings, None),
            KeyBinding::new(shortcut!("shift-a"), app::AskAgent, None),
            KeyBinding::new("up", app::SwitcherPrevious, Some("Switcher")),
            KeyBinding::new("down", app::SwitcherNext, Some("Switcher")),
            KeyBinding::new("escape", app::CloseSwitcher, Some("Switcher")),
            KeyBinding::new(
                shortcut!("w"),
                app::settings_window::CloseSettings,
                Some(app::settings_window::CONTEXT),
            ),
            KeyBinding::new(
                "escape",
                app::settings_window::CloseSettings,
                Some(app::settings_window::CONTEXT),
            ),
        ]);
        if cfg!(target_os = "macos") {
            cx.bind_keys([
                KeyBinding::new("cmd-h", Hide, None),
                KeyBinding::new("alt-cmd-h", HideOthers, None),
                KeyBinding::new("cmd-m", Minimize, None),
            ]);
        }
        bind_terminal_keys(cx);
        cx.on_action(|_: &Quit, cx| cx.quit());
        // The standard macOS window and app commands (⌘H, ⌥⌘H, ⌘M).
        cx.on_action(|_: &Hide, cx| cx.hide());
        cx.on_action(|_: &HideOthers, cx| cx.hide_other_apps());
        cx.on_action(|_: &ShowAll, cx| cx.unhide_other_apps());
        cx.on_action(|_: &Minimize, cx| {
            if let Some(window) = cx.active_window() {
                let _ = window.update(cx, |_, window, _| window.minimize_window());
            }
        });
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
        let options = WindowOptions {
            focus: !headless,
            window_bounds: Some(WindowBounds::Windowed(bounds)),
            window_min_size: Some(size(
                px(1100_f32.min(bounds.size.width.as_f32())),
                px(600_f32.min(bounds.size.height.as_f32())),
            )),
            // Wayland app id / X11 class: matches `supergit.desktop`
            // (scripts/install-linux.sh) for the taskbar icon.
            app_id: Some("supergit".into()),
            // macOS: the window buttons centred in the sidebar's header,
            // which is as tall as the rails are wide (48px; the default
            // suits a 34px bar).
            titlebar: Some(TitlebarOptions {
                traffic_light_position: Some(app::window_buttons_home()),
                ..TitleBar::title_bar_options()
            }),
            ..TitleBar::window_options()
        };
        let (_handle, view) = gpui_kit::open_window(options, cx, |window, cx| {
            window.set_window_title("SuperGIT");
            cx.new(|cx| app::SuperGitApp::new(project, launch_dir, window, cx))
        })
        .expect("failed to open the main window");
        // ⌘, and SuperGIT ▸ Settings… from any window: the settings have a
        // window of their own.
        let settings_app = view.downgrade();
        cx.on_action(move |_: &app::OpenSettings, cx| {
            app::settings_window::open(settings_app.clone(), None, true, cx);
        });
        // `SUPERGIT_DEBUG_SETTINGS=<page>` shoots the settings window instead.
        #[cfg(feature = "screenshot")]
        match std::env::var("SUPERGIT_DEBUG_SETTINGS") {
            Ok(page) => {
                let page = app::settings_window::Page::parse(&page);
                match app::settings_window::open(view.downgrade(), Some(page), false, cx) {
                    Some(window) => screenshot::schedule(window, cx),
                    // AppKit's window is not GPUI's: it draws itself.
                    #[cfg(target_os = "macos")]
                    None => screenshot::schedule_settings(cx),
                    #[cfg(not(target_os = "macos"))]
                    None => screenshot::schedule(_handle, cx),
                }
            }
            Err(_) => screenshot::schedule(_handle, cx),
        }

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
                    // macOS brings the app forward on click; elsewhere we must.
                    window.activate_window();
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
                let _ = cx.update_window(_handle, |_, window, _| window.activate_window());
                let Some(path) = path else { continue };
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
    cx.set_menus(
        [
            Menu::new("SuperGIT").items(
                [
                    MenuItem::action("Settings…", app::OpenSettings),
                    MenuItem::action("Check for Updates…", app::CheckForUpdates),
                    MenuItem::separator(),
                ]
                .into_iter()
                // Only macOS has a Services menu.
                .chain(
                    cfg!(target_os = "macos")
                        .then(|| {
                            [
                                MenuItem::os_submenu("Services", SystemMenuType::Services),
                                MenuItem::separator(),
                                MenuItem::action("Hide SuperGIT", Hide),
                                MenuItem::action("Hide Others", HideOthers),
                                MenuItem::action("Show All", ShowAll),
                                MenuItem::separator(),
                            ]
                        })
                        .into_iter()
                        .flatten(),
                )
                .chain([MenuItem::action("Quit SuperGIT", Quit)])
                .collect::<Vec<_>>(),
            ),
            Menu::new("File").items([
                MenuItem::action("Open Folder as Project…", app::OpenRepository),
                MenuItem::action("Recent Projects", app::CloseRepository),
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
                MenuItem::action("Toggle Terminal", app::ToggleTerminal),
                MenuItem::action("Toggle History", app::ToggleHistory),
                MenuItem::action("New Agent Conversation", app::NewClaudeChat),
                MenuItem::separator(),
                MenuItem::submenu(Menu::new("Appearance").items(appearance::menu_items(cx))),
            ]),
            Menu::new("Worktree").items([
                MenuItem::action("Toggle Keep Active", app::ToggleKeepWorktree),
                MenuItem::action("Clean Up Worktree…", app::CleanUpWorktree),
                MenuItem::action("Update Worktree Base", app::UpdateBase),
            ]),
        ]
        .into_iter()
        .chain(
            cfg!(target_os = "macos")
                .then(|| Menu::new("Window").items([MenuItem::action("Minimize", Minimize)])),
        )
        .collect::<Vec<_>>(),
    );
    // Linux and Windows have no global menu bar: the window draws the same
    // menus itself (`menu_bar`).
    if !cfg!(target_os = "macos") {
        let menus = cx.get_menus().unwrap_or_default();
        gpui_kit::base::GlobalState::global_mut(cx).set_app_menus(menus);
        if let Some(bar) = cx.try_global::<MenuBar>().map(|bar| bar.0.clone()) {
            bar.update(cx, |bar, cx| bar.reload(cx));
        }
    }
}

struct MenuBar(Entity<AppMenuBar>);
impl Global for MenuBar {}

/// The in-window menu bar where the platform has no global one.
pub(crate) fn menu_bar(cx: &mut App) -> Option<Entity<AppMenuBar>> {
    if cfg!(target_os = "macos") {
        return None;
    }
    if let Some(bar) = cx.try_global::<MenuBar>() {
        return Some(bar.0.clone());
    }
    let bar = AppMenuBar::new(cx);
    cx.set_global(MenuBar(bar.clone()));
    Some(bar)
}

#[cfg(test)]
mod tests {
    #[test]
    fn shortcuts_parse_on_this_platform() {
        for keys in [
            shortcut!("q"),
            shortcut!("shift-b"),
            shortcut!("="),
            shortcut!("+"),
            shortcut!("-"),
            shortcut!("0"),
        ] {
            gpui_kit::Keystroke::parse(keys).expect(keys);
        }
        assert!(!shortcut!("q").contains("secondary"));
    }
}
