//! Apps started from Finder, the Dock or Spotlight get launchd's minimal
//! environment (`PATH=/usr/bin:/bin:/usr/sbin:/sbin`), so `gh`, `claude` and
//! Homebrew's git would not be found. Like Zed and VS Code, take the user's
//! login-shell environment at startup instead.

use std::io::Read as _;
use std::process::{Command, Stdio};
use std::sync::mpsc;
use std::time::Duration;

const MARKER: &str = "__SUPERGIT_ENV__";
const TIMEOUT: Duration = Duration::from_secs(5);
/// Variables that describe the probe shell itself, not the user's setup.
const SKIP: &[&str] = &[
    "PWD",
    "OLDPWD",
    "SHLVL",
    "_",
    "TERM",
    "TERM_PROGRAM",
    "TERM_SESSION_ID",
];

/// Import the login shell's environment unless we were started from a terminal.
///
/// Must run at the very start of `main`, before any other thread exists.
pub fn import_if_needed() {
    if std::env::var_os("TERM").is_some() {
        return; // Started from a terminal: the environment is already right.
    }
    let Some(vars) = login_shell_env() else {
        return;
    };
    for (key, value) in vars {
        if SKIP.contains(&key.as_str()) || key.is_empty() || key.contains('=') {
            continue;
        }
        // SAFETY: called first thing in `main`; the only other thread (the
        // probe reader) has finished and never touches the environment.
        unsafe { std::env::set_var(key, value) };
    }
}

fn login_shell_env() -> Option<Vec<(String, String)>> {
    let shell = std::env::var("SHELL").unwrap_or_else(|_| "/bin/zsh".into());
    // -l: profile files, -i: rc files (where most PATH tweaks live).
    let script = format!("printf '%s\\0' {MARKER}; /usr/bin/env -0");
    let mut child = Command::new(&shell)
        .args(["-l", "-i", "-c", &script])
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::null())
        .spawn()
        .ok()?;
    let mut stdout = child.stdout.take()?;
    let (tx, rx) = mpsc::channel();
    std::thread::spawn(move || {
        let mut buf = Vec::new();
        let _ = stdout.read_to_end(&mut buf);
        let _ = tx.send(buf);
    });
    let Ok(output) = rx.recv_timeout(TIMEOUT) else {
        let _ = child.kill();
        return None;
    };
    let _ = child.wait();
    Some(parse(&String::from_utf8_lossy(&output)))
}

fn parse(output: &str) -> Vec<(String, String)> {
    // rc files may print banners before the marker; ignore everything up to it.
    let Some((_, env)) = output.split_once(&format!("{MARKER}\0")) else {
        return Vec::new();
    };
    env.split('\0')
        .filter_map(|entry| entry.split_once('='))
        .map(|(k, v)| (k.to_string(), v.to_string()))
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_env_after_marker() {
        let out =
            format!("motd noise\n{MARKER}\0PATH=/opt/homebrew/bin:/usr/bin\0EMPTY=\0WEIRD=a=b\0");
        let vars = parse(&out);
        assert_eq!(
            vars,
            vec![
                ("PATH".into(), "/opt/homebrew/bin:/usr/bin".into()),
                ("EMPTY".into(), "".into()),
                ("WEIRD".into(), "a=b".into()),
            ]
        );
        assert!(parse("no marker here").is_empty());
    }
}
