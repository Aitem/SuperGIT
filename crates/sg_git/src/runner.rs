use std::ffi::OsStr;
use std::path::Path;
use std::process::{Command, Stdio};

use anyhow::Result;

/// A failed git invocation with its stderr, so the UI can show what git said.
#[derive(Debug)]
pub struct GitError {
    pub args: String,
    pub code: Option<i32>,
    pub stderr: String,
}

impl std::fmt::Display for GitError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        let msg = self.stderr.trim();
        if msg.is_empty() {
            write!(f, "git {} failed ({:?})", self.args, self.code)
        } else {
            write!(f, "git {}: {msg}", self.args)
        }
    }
}

impl std::error::Error for GitError {}

fn command<I, S>(cwd: &Path, args: I) -> (Command, String)
where
    I: IntoIterator<Item = S>,
    S: AsRef<OsStr>,
{
    let mut cmd = Command::new("git");
    let mut printable = Vec::new();
    for arg in args {
        printable.push(arg.as_ref().to_string_lossy().into_owned());
        cmd.arg(arg);
    }
    cmd.current_dir(cwd)
        // Never take index.lock for read-only commands like `status`: agents
        // commit in these worktrees concurrently.
        .env("GIT_OPTIONAL_LOCKS", "0")
        // Never block on a credential or editor prompt; there is no TTY.
        .env("GIT_TERMINAL_PROMPT", "0")
        .env("GIT_EDITOR", "true")
        .stdin(Stdio::null());
    (cmd, printable.join(" "))
}

/// Run git in `cwd` and return raw stdout.
pub fn git_bytes<I, S>(cwd: &Path, args: I) -> Result<Vec<u8>>
where
    I: IntoIterator<Item = S>,
    S: AsRef<OsStr>,
{
    git_bytes_in_index(cwd, args, None)
}

pub(crate) fn git_bytes_in_index<I, S>(cwd: &Path, args: I, index: Option<&Path>) -> Result<Vec<u8>>
where
    I: IntoIterator<Item = S>,
    S: AsRef<OsStr>,
{
    let (mut cmd, printable) = command(cwd, args);
    if let Some(index) = index {
        cmd.env("GIT_INDEX_FILE", index);
    }
    let out = cmd.output()?;
    if !out.status.success() {
        return Err(GitError {
            args: printable,
            code: out.status.code(),
            stderr: String::from_utf8_lossy(&out.stderr).into_owned(),
        }
        .into());
    }
    Ok(out.stdout)
}

/// Run git in `cwd` and return stdout as (lossy) UTF-8.
pub fn git<I, S>(cwd: &Path, args: I) -> Result<String>
where
    I: IntoIterator<Item = S>,
    S: AsRef<OsStr>,
{
    git_bytes(cwd, args).map(|b| String::from_utf8_lossy(&b).into_owned())
}
