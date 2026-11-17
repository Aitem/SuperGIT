//! Opening things outside SuperGIT: terminals running Claude, editors, Finder,
//! the browser. Until the embedded terminal lands, Claude sessions run in the
//! user's terminal app, started in the right worktree.

use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};

use anyhow::{Context as _, Result, bail};

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum TerminalApp {
    Ghostty,
    // macOS
    ITerm,
    Terminal,
    // Linux (and other freedesktop systems)
    Kitty,
    WezTerm,
    Alacritty,
    Konsole,
    GnomeTerminal,
    Ptyxis,
    Xfce,
    Foot,
    Xterm,
}

/// What macOS users call Finder: the platform's file manager.
pub const FILE_MANAGER: &str = if cfg!(target_os = "macos") {
    "Finder"
} else {
    "File Manager"
};

impl TerminalApp {
    pub fn name(self) -> &'static str {
        match self {
            Self::Ghostty => "Ghostty",
            Self::ITerm => "iTerm",
            Self::Terminal => "Terminal",
            Self::Kitty => "kitty",
            Self::WezTerm => "WezTerm",
            Self::Alacritty => "Alacritty",
            Self::Konsole => "Konsole",
            Self::GnomeTerminal => "GNOME Terminal",
            Self::Ptyxis => "Ptyxis",
            Self::Xfce => "Xfce Terminal",
            Self::Foot => "foot",
            Self::Xterm => "xterm",
        }
    }

    /// The executable on Linux.
    fn binary(self) -> &'static str {
        match self {
            Self::Ghostty => "ghostty",
            Self::Kitty => "kitty",
            Self::WezTerm => "wezterm",
            Self::Alacritty => "alacritty",
            Self::Konsole => "konsole",
            Self::GnomeTerminal => "gnome-terminal",
            Self::Ptyxis => "ptyxis",
            Self::Xfce => "xfce4-terminal",
            Self::Foot => "foot",
            Self::Xterm | Self::ITerm | Self::Terminal => "xterm",
        }
    }

    fn parse(name: &str) -> Option<Self> {
        Some(match name {
            "ghostty" => Self::Ghostty,
            "iterm" | "iterm2" if cfg!(target_os = "macos") => Self::ITerm,
            "terminal" if cfg!(target_os = "macos") => Self::Terminal,
            "kitty" => Self::Kitty,
            "wezterm" => Self::WezTerm,
            "alacritty" => Self::Alacritty,
            "konsole" => Self::Konsole,
            "gnome-terminal" | "gnome" => Self::GnomeTerminal,
            "ptyxis" => Self::Ptyxis,
            "xfce4-terminal" | "xfce" => Self::Xfce,
            "foot" => Self::Foot,
            "xterm" => Self::Xterm,
            _ => return None,
        })
    }

    /// `SUPERGIT_TERMINAL` (see `--help`), else the first installed.
    pub fn detect() -> Self {
        let wanted = std::env::var("SUPERGIT_TERMINAL")
            .unwrap_or_default()
            .to_ascii_lowercase();
        if let Some(app) = Self::parse(&wanted) {
            return app;
        }
        if cfg!(target_os = "macos") {
            return if app_installed("Ghostty") {
                Self::Ghostty
            } else if app_installed("iTerm") {
                Self::ITerm
            } else {
                Self::Terminal
            };
        }
        // `$TERMINAL` is the closest Linux has to a default terminal setting;
        // then the desktop's own terminal, then any known one.
        let preferred = std::env::var("TERMINAL")
            .ok()
            .and_then(|t| {
                let name = Path::new(&t).file_name()?.to_str()?.to_ascii_lowercase();
                Self::parse(&name)
            })
            .into_iter();
        let desktop = std::env::var("XDG_CURRENT_DESKTOP")
            .unwrap_or_default()
            .to_ascii_uppercase();
        let native: &[Self] = if desktop.contains("KDE") {
            &[Self::Konsole]
        } else if desktop.contains("GNOME") {
            &[Self::Ptyxis, Self::GnomeTerminal]
        } else if desktop.contains("XFCE") {
            &[Self::Xfce]
        } else {
            &[]
        };
        let known = [
            Self::Ghostty,
            Self::Kitty,
            Self::WezTerm,
            Self::Alacritty,
            Self::Foot,
            Self::Konsole,
            Self::Ptyxis,
            Self::GnomeTerminal,
            Self::Xfce,
        ];
        preferred
            .chain(native.iter().copied())
            .chain(known)
            .find(|app| on_path(app.binary()))
            .unwrap_or(Self::Xterm)
    }
}

fn app_installed(name: &str) -> bool {
    Path::new(&format!("/Applications/{name}.app")).exists()
}

/// Whether `program` is an executable somewhere on `PATH`.
fn on_path(program: &str) -> bool {
    use std::os::unix::fs::PermissionsExt as _;
    std::env::var_os("PATH").is_some_and(|path| {
        std::env::split_paths(&path).any(|dir| {
            std::fs::metadata(dir.join(program))
                .is_ok_and(|m| m.is_file() && m.permissions().mode() & 0o111 != 0)
        })
    })
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
    if !cfg!(target_os = "macos") {
        return open_linux_terminal(app, dir, command);
    }
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
        _ => open_linux_terminal(app, dir, command),
    }
}

/// A new terminal window in `dir`; `command` runs in the user's login shell,
/// which stays open afterwards (like the macOS terminals).
fn open_linux_terminal(app: TerminalApp, dir: &Path, command: Option<&str>) -> Result<()> {
    let dir_s = dir.to_string_lossy();
    let mut cmd = Command::new(app.binary());
    cmd.current_dir(dir);
    // Single-instance terminals (a server process) ignore our cwd.
    match app {
        TerminalApp::Ghostty | TerminalApp::Alacritty | TerminalApp::Xfce => {
            cmd.arg(format!("--working-directory={dir_s}"));
        }
        TerminalApp::GnomeTerminal | TerminalApp::Ptyxis => {
            cmd.arg("--window")
                .arg(format!("--working-directory={dir_s}"));
        }
        TerminalApp::Kitty => {
            cmd.arg("--directory").arg(dir);
        }
        TerminalApp::WezTerm => {
            cmd.args(["start", "--cwd"]).arg(dir);
        }
        TerminalApp::Konsole => {
            cmd.arg("--workdir").arg(dir);
        }
        TerminalApp::Foot => {
            cmd.arg(format!("--working-directory={dir_s}"));
        }
        TerminalApp::Xterm | TerminalApp::ITerm | TerminalApp::Terminal => {}
    }
    if let Some(c) = command {
        let shell = std::env::var("SHELL").unwrap_or_else(|_| "/bin/sh".into());
        // How each terminal takes the program to run.
        match app {
            TerminalApp::Kitty | TerminalApp::Foot => {}
            TerminalApp::WezTerm | TerminalApp::GnomeTerminal | TerminalApp::Ptyxis => {
                cmd.arg("--");
            }
            TerminalApp::Xfce => {
                cmd.arg("-x");
            }
            _ => {
                cmd.arg("-e");
            }
        }
        cmd.arg(&shell)
            .arg("-lic")
            .arg(format!("{c}; exec {shell} -l"));
    }
    spawn(&mut cmd).with_context(|| format!("is {} installed? Set SUPERGIT_TERMINAL", app.name()))
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

/// The command that continues a session in a terminal.
pub fn resume_command(provider: sg_agents::Provider, session_id: &str) -> String {
    match provider {
        sg_agents::Provider::Claude => format!("claude --resume {}", shell_quote(session_id)),
        sg_agents::Provider::Codex => format!("codex resume {}", shell_quote(session_id)),
    }
}

/// Resume an existing agent session in a terminal (run in the session's cwd).
pub fn agent_resume(
    app: TerminalApp,
    provider: sg_agents::Provider,
    dir: &Path,
    session_id: &str,
) -> Result<()> {
    open_terminal(app, dir, Some(&resume_command(provider, session_id)))
}

/// `SUPERGIT_EDITOR` (a CLI like `code`, `zed`, `cursor`), else VS Code
/// (on Linux the first of VS Code, VSCodium, Zed, Cursor on `PATH`).
pub fn open_in_editor(dir: &Path) -> Result<()> {
    if let Ok(editor) = std::env::var("SUPERGIT_EDITOR") {
        return spawn(Command::new(editor).arg(dir));
    }
    if cfg!(target_os = "macos") {
        return spawn(
            Command::new("open")
                .args(["-a", "Visual Studio Code"])
                .arg(dir),
        );
    }
    // Arch packages Zed as `zeditor`.
    let editor = ["code", "codium", "zed", "zeditor", "cursor"]
        .into_iter()
        .find(|e| on_path(e))
        .context("no editor found (VS Code, VSCodium, Zed, Cursor): set SUPERGIT_EDITOR")?;
    spawn(Command::new(editor).arg(dir))
}

/// The platform's "open with the default app": `open` / `xdg-open`.
fn opener() -> Command {
    Command::new(if cfg!(target_os = "macos") {
        "open"
    } else {
        "xdg-open"
    })
}

pub fn reveal_in_finder(dir: &Path) -> Result<()> {
    spawn(opener().arg(dir))
}

pub fn open_url(url: &str) -> Result<()> {
    spawn(opener().arg(url))
}

/// `file:///Users/me/My%20Repo/` → `/Users/me/My Repo/`.
pub fn file_url_to_path(url: &str) -> Option<PathBuf> {
    let rest = url.strip_prefix("file://")?;
    let rest = if rest.starts_with("localhost/") {
        &rest["localhost".len()..]
    } else {
        rest
    };
    if !rest.starts_with('/') {
        return None;
    }
    let bytes = rest.as_bytes();
    let mut out = Vec::with_capacity(bytes.len());
    let mut i = 0;
    while i < bytes.len() {
        if bytes[i] == b'%' {
            let hi = (*bytes.get(i + 1)? as char).to_digit(16)?;
            let lo = (*bytes.get(i + 2)? as char).to_digit(16)?;
            out.push((hi * 16 + lo) as u8);
            i += 3;
            continue;
        }
        out.push(bytes[i]);
        i += 1;
    }
    if out.contains(&0) {
        return None;
    }
    let path = PathBuf::from(String::from_utf8(out).ok()?);
    path.is_absolute().then_some(path)
}

#[cfg(test)]
mod tests {
    use std::path::PathBuf;

    use super::*;

    #[test]
    fn decodes_file_urls() {
        assert_eq!(
            file_url_to_path("file:///Users/me/My%20Repo/"),
            Some(PathBuf::from("/Users/me/My Repo/"))
        );
        assert_eq!(
            file_url_to_path("file://localhost/tmp/%D1%80%D0%B5%D0%BF%D0%BE"),
            Some(PathBuf::from("/tmp/репо"))
        );
        assert_eq!(file_url_to_path("https://example.com"), None);
    }

    #[test]
    fn rejects_malformed_file_urls_without_panicking() {
        for url in [
            "file:///tmp/%💥",
            "file:///tmp/%",
            "file:///tmp/%2",
            "file:///tmp/%GG",
            "file:///tmp/%FF",
            "file:///tmp/%00",
            "file://localhost.evil/tmp/repo",
            "file://server/tmp/repo",
        ] {
            assert_eq!(file_url_to_path(url), None, "{url}");
        }
        assert_eq!(
            file_url_to_path("file:///tmp/репо%25"),
            Some(PathBuf::from("/tmp/репо%"))
        );
    }
}
