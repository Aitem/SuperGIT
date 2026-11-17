//! Finding a working Codex CLI and checking it speaks the protocol we use.

use std::path::{Path, PathBuf};
use std::process::Command;
use std::sync::Mutex;

use anyhow::{Result, bail};

/// The oldest Codex CLI whose `app-server` protocol (v2 threads, turns,
/// item notifications, approvals with `availableDecisions`) SuperGIT was
/// tested against.
pub const MIN_VERSION: Version = Version(0, 157, 0);

#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord)]
pub struct Version(pub u32, pub u32, pub u32);

impl Version {
    /// Parse `codex-cli 0.157.1` (or a bare `0.157.1`, prerelease suffixes
    /// ignored).
    pub fn parse(text: &str) -> Option<Self> {
        let word = text
            .split_whitespace()
            .find(|w| w.chars().next().is_some_and(|c| c.is_ascii_digit()))?;
        let mut parts = word
            .split(|c: char| !c.is_ascii_digit())
            .map(|p| p.parse::<u32>().ok());
        Some(Self(parts.next()??, parts.next()??, parts.next()??))
    }
}

impl std::fmt::Display for Version {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "{}.{}.{}", self.0, self.1, self.2)
    }
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Installation {
    pub program: PathBuf,
    pub version: Version,
}

static FOUND: Mutex<Option<Installation>> = Mutex::new(None);

/// The Codex CLI to run: `SUPERGIT_CODEX`, `codex` on PATH, then the
/// standalone and desktop-app installs. Each candidate must answer
/// `--version` (a package-manager shim whose Node is gone does not). The
/// first working one is remembered; failures are not, so installing Codex
/// later works without a restart.
pub fn locate() -> Result<Installation> {
    if let Some(found) = FOUND.lock().unwrap_or_else(|e| e.into_inner()).clone() {
        return Ok(found);
    }
    let found = locate_in(&candidates())?;
    *FOUND.lock().unwrap_or_else(|e| e.into_inner()) = Some(found.clone());
    Ok(found)
}

fn candidates() -> Vec<PathBuf> {
    let mut out = Vec::new();
    if let Some(p) = std::env::var_os("SUPERGIT_CODEX") {
        out.push(PathBuf::from(p));
    }
    if let Some(path) = std::env::var_os("PATH") {
        out.extend(std::env::split_paths(&path).map(|dir| dir.join("codex")));
    }
    if let Some(home) = crate::codex_home() {
        out.push(home.join("packages/standalone/current/bin/codex"));
    }
    out.push("/Applications/Codex.app/Contents/Resources/codex".into());
    out.push("/opt/homebrew/bin/codex".into());
    out.push("/usr/local/bin/codex".into());
    let mut seen = std::collections::HashSet::new();
    out.retain(|p| seen.insert(p.clone()));
    out
}

/// The first candidate that runs and is new enough.
pub fn locate_in(candidates: &[PathBuf]) -> Result<Installation> {
    let mut problems = Vec::new();
    for program in candidates.iter().filter(|p| p.is_file()) {
        match probe(program) {
            Ok(version) if version >= MIN_VERSION => {
                return Ok(Installation {
                    program: program.clone(),
                    version,
                });
            }
            Ok(version) => problems.push(format!(
                "{} is Codex {version}; SuperGIT needs {MIN_VERSION} or newer",
                program.display()
            )),
            Err(e) => problems.push(format!("{} does not run: {e:#}", program.display())),
        }
    }
    if problems.is_empty() {
        bail!(
            "Codex CLI not found. Install it (`npm install -g @openai/codex` or \
             `brew install codex`) and sign in with `codex login`."
        );
    }
    bail!("No usable Codex CLI. {}", problems.join("; "))
}

fn probe(program: &Path) -> Result<Version> {
    let out = Command::new(program).arg("--version").output()?;
    let text = String::from_utf8_lossy(&out.stdout);
    if !out.status.success() {
        let err = String::from_utf8_lossy(&out.stderr);
        let line = err.lines().find(|l| !l.trim().is_empty()).unwrap_or("");
        bail!("{} {}", out.status, line.trim());
    }
    Version::parse(&text)
        .ok_or_else(|| anyhow::anyhow!("unexpected `--version` output: {}", text.trim()))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_versions() {
        assert_eq!(
            Version::parse("codex-cli 0.157.1\n"),
            Some(Version(0, 157, 1))
        );
        assert_eq!(Version::parse("0.160.0-alpha.2"), Some(Version(0, 160, 0)));
        assert_eq!(Version::parse("codex-cli"), None);
        assert!(Version(0, 157, 1) >= MIN_VERSION);
        assert!(Version(0, 99, 9) < MIN_VERSION);
        assert!(Version(1, 0, 0) > MIN_VERSION);
    }
}
