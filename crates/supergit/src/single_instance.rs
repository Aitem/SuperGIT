//! One SuperGIT per user on Linux, like the macOS app: a second launch hands
//! its folder to the running instance (which opens it as a project and comes
//! forward) and exits. macOS gets this from LaunchServices (`open -a`, Dock
//! drops → `on_open_urls`).
//!
//! The instance listens on a Unix socket; each connection sends one line:
//! a folder to open, or nothing to only raise the window.

use std::io::{BufRead as _, BufReader, Write as _};
use std::os::unix::net::{UnixListener, UnixStream};
use std::path::{Path, PathBuf};
use std::time::Duration;

/// Per profile: `SUPERGIT_STATE_DIR` runs stay separate from the user's app.
fn socket_path() -> Option<PathBuf> {
    if std::env::var_os("SUPERGIT_STATE_DIR").is_none()
        && let Some(runtime) = std::env::var_os("XDG_RUNTIME_DIR").map(PathBuf::from)
        && runtime.is_absolute()
    {
        return Some(runtime.join("supergit.sock"));
    }
    Some(crate::settings::support_dir()?.join("instance.sock"))
}

pub enum Role {
    /// Another instance took the request: exit.
    Forwarded,
    /// We are the instance; hand requests to `serve`.
    Primary(Option<UnixListener>),
}

/// Pass `folder` (None: just raise the window) to a running instance, or
/// become the instance. Never fails: without a socket we simply run alone.
pub fn claim(folder: Option<&Path>) -> Role {
    if cfg!(target_os = "macos") || std::env::var_os("SUPERGIT_NEW_INSTANCE").is_some() {
        return Role::Primary(None);
    }
    let Some(path) = socket_path() else {
        return Role::Primary(None);
    };
    if let Ok(mut stream) = UnixStream::connect(&path) {
        let _ = stream.set_write_timeout(Some(Duration::from_secs(2)));
        let line = folder
            .map(|f| f.to_string_lossy().into_owned())
            .unwrap_or_default();
        if writeln!(stream, "{line}").is_ok() {
            return Role::Forwarded;
        }
    }
    // Nobody answers: whatever is there is stale.
    let _ = std::fs::remove_file(&path);
    if let Some(dir) = path.parent() {
        let _ = std::fs::create_dir_all(dir);
    }
    Role::Primary(UnixListener::bind(&path).ok())
}

/// Accept requests on a background thread for the life of the process.
pub fn serve(listener: UnixListener, send: impl Fn(Option<PathBuf>) + Send + 'static) {
    let spawned = std::thread::Builder::new()
        .name("single-instance".into())
        .spawn(move || {
            for stream in listener.incoming().flatten() {
                let _ = stream.set_read_timeout(Some(Duration::from_secs(2)));
                let mut line = String::new();
                if BufReader::new(stream).read_line(&mut line).is_err() {
                    continue;
                }
                let line = line.trim_end_matches(['\n', '\r']);
                send((!line.is_empty()).then(|| PathBuf::from(line)));
            }
        });
    if let Err(e) = spawned {
        eprintln!("SuperGIT: cannot listen for other launches: {e}");
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn second_launch_forwards_its_folder() {
        let dir = std::env::temp_dir().join(format!("sg-instance-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("s.sock");
        let listener = UnixListener::bind(&path).unwrap();
        let (tx, rx) = std::sync::mpsc::channel();
        serve(listener, move |folder| tx.send(folder).unwrap());
        for folder in [Some("/tmp/репо"), None] {
            let mut stream = UnixStream::connect(&path).unwrap();
            writeln!(stream, "{}", folder.unwrap_or_default()).unwrap();
            drop(stream);
            let got = rx.recv_timeout(Duration::from_secs(5)).unwrap();
            assert_eq!(got, folder.map(PathBuf::from));
        }
        std::fs::remove_dir_all(&dir).unwrap();
    }
}
