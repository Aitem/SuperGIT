mod app;
mod dump;
mod launch;
mod model;
#[cfg(feature = "screenshot")]
mod screenshot;

use std::path::PathBuf;

use anyhow::{Context as _, Result};
use gpui_kit::component::{Theme, ThemeMode, TitleBar};
use gpui_kit::*;

actions!(supergit, [Quit]);

const USAGE: &str = "\
SuperGIT — Git GUI for agent-driven development

USAGE:
    supergit [PATH]          open the repository containing PATH (default: cwd)
    supergit --dump [PATH]   print worktrees, Claude sessions and PRs as text

ENV:
    SUPERGIT_TERMINAL   ghostty | iterm | terminal (default: first installed)
    SUPERGIT_EDITOR     editor CLI for \"Editor\" (default: VS Code)
";

fn main() -> Result<()> {
    let args: Vec<String> = std::env::args().skip(1).collect();
    if args.iter().any(|a| a == "-h" || a == "--help") {
        print!("{USAGE}");
        return Ok(());
    }
    let dump = args.iter().any(|a| a == "--dump");
    let launch_dir = match args.iter().find(|a| !a.starts_with('-')) {
        Some(p) => {
            std::fs::canonicalize(PathBuf::from(p)).with_context(|| format!("no such path: {p}"))?
        }
        None => std::env::current_dir()?,
    };
    let repo = sg_git::Repo::discover(&launch_dir)?;

    if dump {
        return dump::run(&repo);
    }

    gpui_kit::application().run(move |cx| {
        gpui_kit::init(cx);
        Theme::change(ThemeMode::Dark, None, cx);

        cx.bind_keys([
            KeyBinding::new("cmd-q", Quit, None),
            KeyBinding::new("cmd-r", app::Refresh, None),
        ]);
        cx.on_action(|_: &Quit, cx| cx.quit());
        cx.on_window_closed(|cx, _| {
            if cx.windows().is_empty() {
                cx.quit();
            }
        })
        .detach();

        let bounds = Bounds::centered(None, size(px(1440.), px(900.)), cx);
        let options = WindowOptions {
            window_bounds: Some(WindowBounds::Windowed(bounds)),
            window_min_size: Some(size(px(900.), px(560.))),
            ..TitleBar::window_options()
        };
        let (_handle, _view) = gpui_kit::open_window(options, cx, |window, cx| {
            cx.new(|cx| app::SuperGitApp::new(repo, launch_dir, window, cx))
        })
        .expect("failed to open the main window");
        #[cfg(feature = "screenshot")]
        screenshot::schedule(_handle, cx);
        cx.activate(true);
    });
    Ok(())
}
