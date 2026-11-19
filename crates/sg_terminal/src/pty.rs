//! A child process on a pseudo-terminal. The child gets the PTY's slave as
//! its stdio and controlling terminal (its own session), we keep the master:
//! a reader thread forwards its output, a writer thread feeds it input (a
//! large paste never blocks the caller), and resizing goes through
//! `TIOCSWINSZ`, which also signals the child (`SIGWINCH`).

use std::ffi::OsString;
use std::fs::File;
use std::io::{self, Read as _, Write as _};
use std::os::fd::OwnedFd;
use std::os::unix::process::CommandExt as _;
use std::path::PathBuf;
use std::process::{Command, Stdio};
use std::sync::mpsc;

use anyhow::{Context as _, Result};
use futures::channel::mpsc::{UnboundedReceiver, UnboundedSender, unbounded};
use rustix::fs::{Mode, OFlags};
use rustix::io::FdFlags;
use rustix::pty::OpenptFlags;
use rustix::termios::Winsize;

/// The grid size, and the cell size in pixels (programs may ask for it).
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct Size {
    pub cols: u16,
    pub rows: u16,
    pub cell_width: u16,
    pub cell_height: u16,
}

impl Size {
    fn winsize(self) -> Winsize {
        Winsize {
            ws_row: self.rows,
            ws_col: self.cols,
            ws_xpixel: self.cols.saturating_mul(self.cell_width),
            ws_ypixel: self.rows.saturating_mul(self.cell_height),
        }
    }
}

/// What to run.
#[derive(Clone, Debug, Default)]
pub struct Spawn {
    pub program: PathBuf,
    pub args: Vec<OsString>,
    pub cwd: PathBuf,
    /// Added to (or overriding) the app's environment.
    pub env: Vec<(OsString, OsString)>,
    /// Removed from the app's environment.
    pub env_remove: Vec<OsString>,
    pub size: Size,
}

#[derive(Debug, PartialEq, Eq)]
pub enum PtyEvent {
    Output(Vec<u8>),
    /// The child exited: its exit code, or `None` when a signal ended it.
    Exited(Option<i32>),
}

pub struct Pty {
    master: OwnedFd,
    input: mpsc::Sender<Vec<u8>>,
    pid: u32,
}

impl Pty {
    /// Start `spawn.program`; its output and exit arrive on the receiver.
    pub fn spawn(spawn: &Spawn) -> Result<(Self, UnboundedReceiver<PtyEvent>)> {
        let master = rustix::pty::openpt(OpenptFlags::RDWR | OpenptFlags::NOCTTY)
            .context("open a pseudo-terminal")?;
        rustix::io::fcntl_setfd(&master, FdFlags::CLOEXEC)?;
        rustix::pty::grantpt(&master)?;
        rustix::pty::unlockpt(&master)?;
        let name = rustix::pty::ptsname(&master, Vec::new())?;
        let slave = rustix::fs::open(
            name.as_c_str(),
            OFlags::RDWR | OFlags::NOCTTY | OFlags::CLOEXEC,
            Mode::empty(),
        )
        .context("open the terminal's slave side")?;
        rustix::termios::tcsetwinsize(&master, spawn.size.winsize())?;

        let mut command = Command::new(&spawn.program);
        for name in &spawn.env_remove {
            command.env_remove(name);
        }
        command
            .args(&spawn.args)
            .current_dir(&spawn.cwd)
            .envs(spawn.env.iter().map(|(k, v)| (k, v)))
            .stdin(Stdio::from(slave.try_clone()?))
            .stdout(Stdio::from(slave.try_clone()?))
            .stderr(Stdio::from(slave));
        // SAFETY: the closure runs in the forked child before `exec` and only
        // makes raw system calls (no allocation, no locks): a new session,
        // then stdin (already the slave) as its controlling terminal.
        unsafe {
            command.pre_exec(|| {
                rustix::process::setsid()?;
                rustix::process::ioctl_tiocsctty(rustix::stdio::stdin())?;
                Ok(())
            });
        }
        let mut child = command
            .spawn()
            .with_context(|| format!("start {}", spawn.program.display()))?;
        // The child holds the slave now; our copies are closed with `command`.
        drop(command);
        let pid = child.id();

        let (tx, rx) = unbounded();
        let reader = File::from(master.try_clone()?);
        let output = tx.clone();
        std::thread::Builder::new()
            .name("pty-reader".into())
            .spawn(move || read_output(reader, &output))?;
        std::thread::Builder::new()
            .name("pty-wait".into())
            .spawn(move || {
                let code = child.wait().ok().and_then(|status| status.code());
                let _ = tx.unbounded_send(PtyEvent::Exited(code));
            })?;
        let (input, queue) = mpsc::channel::<Vec<u8>>();
        let mut writer = File::from(master.try_clone()?);
        std::thread::Builder::new()
            .name("pty-writer".into())
            .spawn(move || {
                for bytes in queue {
                    if writer.write_all(&bytes).is_err() {
                        break;
                    }
                }
            })?;
        Ok((Self { master, input, pid }, rx))
    }

    /// Queue input for the child (keys, pastes, replies to its queries).
    pub fn write(&self, bytes: Vec<u8>) {
        if !bytes.is_empty() {
            let _ = self.input.send(bytes);
        }
    }

    pub fn resize(&self, size: Size) -> io::Result<()> {
        rustix::termios::tcsetwinsize(&self.master, size.winsize()).map_err(io::Error::from)
    }

    pub fn pid(&self) -> u32 {
        self.pid
    }

    /// Hang up: what closing a terminal window does to the programs in it.
    pub fn hang_up(&self) {
        if let Some(pid) = i32::try_from(self.pid)
            .ok()
            .and_then(rustix::process::Pid::from_raw)
        {
            let _ = rustix::process::kill_process_group(pid, rustix::process::Signal::HUP);
        }
    }
}

impl Drop for Pty {
    fn drop(&mut self) {
        self.hang_up();
    }
}

fn read_output(mut reader: File, tx: &UnboundedSender<PtyEvent>) {
    let mut buf = vec![0u8; 64 * 1024];
    loop {
        match reader.read(&mut buf) {
            Err(e) if e.kind() == io::ErrorKind::Interrupted => {}
            // EOF, or EIO on Linux once every slave descriptor is closed.
            Ok(0) | Err(_) => break,
            Ok(n) => {
                if tx
                    .unbounded_send(PtyEvent::Output(buf[..n].to_vec()))
                    .is_err()
                {
                    break;
                }
            }
        }
    }
}
