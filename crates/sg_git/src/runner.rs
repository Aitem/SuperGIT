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

pub(crate) fn git_input(cwd: &Path, args: &[&str], input: &str) -> Result<()> {
    use std::io::Write as _;
    let (mut command, printable) = command(cwd, args);
    let mut child = command
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()?;
    let write = child
        .stdin
        .take()
        .expect("piped stdin")
        .write_all(input.as_bytes());
    let out = child.wait_with_output()?;
    if !out.status.success() {
        return Err(GitError {
            args: printable,
            code: out.status.code(),
            stderr: String::from_utf8_lossy(&out.stderr).into_owned(),
        }
        .into());
    }
    write?;
    Ok(())
}

/// Run git with `input` on stdin and return raw stdout. Stdin is written on
/// its own thread so a large request cannot deadlock against a full stdout.
pub(crate) fn git_bytes_with_input(cwd: &Path, args: &[&str], input: Vec<u8>) -> Result<Vec<u8>> {
    use std::io::Write as _;
    let (mut command, printable) = command(cwd, args);
    let mut child = command
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()?;
    let mut stdin = child.stdin.take().expect("piped stdin");
    let writer = std::thread::spawn(move || stdin.write_all(&input));
    let out = child.wait_with_output()?;
    let written = writer
        .join()
        .map_err(|_| anyhow::anyhow!("Git stdin writer failed"))?;
    if !out.status.success() {
        return Err(GitError {
            args: printable,
            code: out.status.code(),
            stderr: String::from_utf8_lossy(&out.stderr).into_owned(),
        }
        .into());
    }
    written?;
    Ok(out.stdout)
}

/// Bound network operations, including credential and SSH subprocesses.
pub(crate) fn git_timeout(cwd: &Path, args: &[&str], timeout: std::time::Duration) -> Result<()> {
    use std::io::Read as _;
    use std::os::unix::process::CommandExt as _;
    let (mut command, printable) = command(cwd, args);
    let mut child = command
        .process_group(0)
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()?;
    let mut stdout = child.stdout.take().expect("piped stdout");
    let mut stderr = child.stderr.take().expect("piped stderr");
    let out = std::thread::spawn(move || std::io::copy(&mut stdout, &mut std::io::sink()));
    let err = std::thread::spawn(move || {
        let mut bytes = Vec::new();
        stderr.read_to_end(&mut bytes).map(|_| bytes)
    });
    let start = std::time::Instant::now();
    let status = loop {
        if let Some(status) = child.try_wait()? {
            break Some(status);
        }
        if start.elapsed() >= timeout {
            // SAFETY: this child owns its new process group; a negative PID targets only that group.
            unsafe {
                libc::kill(-(child.id() as i32), libc::SIGKILL);
            }
            let _ = child.kill();
            let _ = child.wait();
            break None;
        }
        std::thread::sleep(std::time::Duration::from_millis(50));
    };
    let _ = out.join();
    let stderr = err
        .join()
        .map_err(|_| anyhow::anyhow!("Git stderr reader failed"))??;
    let Some(status) = status else {
        anyhow::bail!(
            "Git operation timed out after {} seconds; local work was preserved",
            timeout.as_secs()
        );
    };
    if !status.success() {
        return Err(GitError {
            args: printable,
            code: status.code(),
            stderr: String::from_utf8_lossy(&stderr).into_owned(),
        }
        .into());
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    #[test]
    fn timeout_stops_git_and_its_subprocesses() {
        let temp = tempfile::tempdir().unwrap();
        super::git(temp.path(), ["init", "-q"]).unwrap();
        super::git(temp.path(), ["config", "alias.wait-test", "!sleep 10"]).unwrap();
        let start = std::time::Instant::now();
        let error = super::git_timeout(
            temp.path(),
            &["wait-test"],
            std::time::Duration::from_millis(100),
        )
        .unwrap_err();
        assert!(error.to_string().contains("timed out"));
        assert!(start.elapsed() < std::time::Duration::from_secs(3));
    }
}
