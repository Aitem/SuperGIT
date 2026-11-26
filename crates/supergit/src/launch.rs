//! Opening things outside SuperGIT: editors, Finder, the browser. Shells
//! run in the terminal panel (`app/terminal_panel.rs`).

use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};

use anyhow::{Context as _, Result};

/// What macOS users call Finder: the platform's file manager.
pub const FILE_MANAGER: &str = if cfg!(target_os = "macos") {
    "Finder"
} else {
    "File Manager"
};

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

fn spawn(cmd: &mut Command) -> Result<()> {
    cmd.stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .spawn()
        .with_context(|| format!("failed to launch {cmd:?}"))?;
    Ok(())
}

/// The command that continues a session in a terminal.
pub fn resume_command(provider: sg_agents::Provider, session_id: &str) -> String {
    match provider {
        sg_agents::Provider::Claude => format!("claude --resume {}", shell_quote(session_id)),
        sg_agents::Provider::Codex => format!("codex resume {}", shell_quote(session_id)),
    }
}

/// The same from any shell: first into `cwd`, where the session started
/// (`--resume` looks for it there).
pub fn resume_command_in(provider: sg_agents::Provider, session_id: &str, cwd: &Path) -> String {
    format!(
        "cd {} && {}",
        shell_quote(&cwd.to_string_lossy()),
        resume_command(provider, session_id)
    )
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

/// Show `file` in the file manager: selected in Finder, elsewhere its
/// folder.
pub fn reveal_file(file: &Path) -> Result<()> {
    if cfg!(target_os = "macos") {
        return spawn(Command::new("open").arg("-R").arg(file));
    }
    spawn(opener().arg(file.parent().unwrap_or(file)))
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
