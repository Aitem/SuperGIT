//! Opening things outside SuperGIT: terminals running Claude, editors, Finder,
//! the browser. Until the embedded terminal lands, Claude sessions run in the
//! user's terminal app, started in the right worktree.

use std::path::Path;
use std::process::{Command, Stdio};

use anyhow::{Context as _, Result, bail};

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum TerminalApp {
    Ghostty,
    ITerm,
    Terminal,
}

impl TerminalApp {
    pub fn name(self) -> &'static str {
        match self {
            Self::Ghostty => "Ghostty",
            Self::ITerm => "iTerm",
            Self::Terminal => "Terminal",
        }
    }

    /// `SUPERGIT_TERMINAL=ghostty|iterm|terminal`, else the first installed.
    pub fn detect() -> Self {
        match std::env::var("SUPERGIT_TERMINAL")
            .unwrap_or_default()
            .to_ascii_lowercase()
            .as_str()
        {
            "ghostty" => return Self::Ghostty,
            "iterm" | "iterm2" => return Self::ITerm,
            "terminal" => return Self::Terminal,
            _ => {}
        }
        if app_installed("Ghostty") {
            Self::Ghostty
        } else if app_installed("iTerm") {
            Self::ITerm
        } else {
            Self::Terminal
        }
    }
}

fn app_installed(name: &str) -> bool {
    Path::new(&format!("/Applications/{name}.app")).exists()
}

fn shell_quote(s: &str) -> String {
    format!("'{}'", s.replace('\'', r"'\''"))
}

fn applescript_string(s: &str) -> String {
    format!("\"{}\"", s.replace('\\', r"\\").replace('"', "\\\""))
}

fn spawn(cmd: &mut Command) -> Result<()> {
    cmd.stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .spawn()
        .with_context(|| format!("failed to launch {cmd:?}"))?;
    Ok(())
}

/// Open a terminal window in `dir`, optionally running `command` (the shell
/// stays open afterwards).
pub fn open_terminal(app: TerminalApp, dir: &Path, command: Option<&str>) -> Result<()> {
    let dir_s = dir.to_string_lossy();
    let line = match command {
        Some(c) => format!("cd {} && {c}", shell_quote(&dir_s)),
        None => format!("cd {}", shell_quote(&dir_s)),
    };
    match app {
        TerminalApp::Ghostty => {
            let shell = std::env::var("SHELL").unwrap_or_else(|_| "/bin/zsh".into());
            let mut cmd = Command::new("open");
            cmd.args(["-na", "Ghostty", "--args"])
                .arg(format!("--working-directory={dir_s}"));
            if let Some(c) = command {
                cmd.arg("-e")
                    .arg(&shell)
                    .arg("-lic")
                    .arg(format!("{c}; exec {shell} -l"));
            }
            spawn(&mut cmd)
        }
        TerminalApp::ITerm => osascript(&format!(
            "tell application \"iTerm\"\n\
               activate\n\
               set w to (create window with default profile)\n\
               tell current session of w to write text {}\n\
             end tell",
            applescript_string(&line)
        )),
        TerminalApp::Terminal => osascript(&format!(
            "tell application \"Terminal\"\n\
               activate\n\
               do script {}\n\
             end tell",
            applescript_string(&line)
        )),
    }
}

fn osascript(script: &str) -> Result<()> {
    let out = Command::new("osascript")
        .arg("-e")
        .arg(script)
        .stdin(Stdio::null())
        .output()?;
    if !out.status.success() {
        bail!("osascript: {}", String::from_utf8_lossy(&out.stderr).trim());
    }
    Ok(())
}

/// New Claude Code session in `dir`.
pub fn claude_new(app: TerminalApp, dir: &Path) -> Result<()> {
    open_terminal(app, dir, Some("claude"))
}

/// Resume an existing Claude Code session (must run in the session's cwd).
pub fn claude_resume(app: TerminalApp, dir: &Path, session_id: &str) -> Result<()> {
    open_terminal(
        app,
        dir,
        Some(&format!("claude --resume {}", shell_quote(session_id))),
    )
}

/// `SUPERGIT_EDITOR` (a CLI like `code`, `zed`, `cursor`), else VS Code.
pub fn open_in_editor(dir: &Path) -> Result<()> {
    if let Ok(editor) = std::env::var("SUPERGIT_EDITOR") {
        return spawn(Command::new(editor).arg(dir));
    }
    spawn(
        Command::new("open")
            .args(["-a", "Visual Studio Code"])
            .arg(dir),
    )
}

pub fn reveal_in_finder(dir: &Path) -> Result<()> {
    spawn(Command::new("open").arg(dir))
}

pub fn open_url(url: &str) -> Result<()> {
    spawn(Command::new("open").arg(url))
}
