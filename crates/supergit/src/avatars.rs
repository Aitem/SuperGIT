//! GitHub avatars for commit authors in the History tab.
//!
//! Resolved once per repository: GitHub's private `…@users.noreply.github.com`
//! emails name their account outright, the rest come from one `gh` GraphQL
//! call over the default branch's history (no emails are sent anywhere).
//! Images are downloaded with `curl` into `~/Library/Caches/SuperGIT/avatars`
//! (`~/.cache/supergit/avatars` on Linux) and refreshed weekly; a stale copy is kept when the network fails.
//! Which account an email belongs to (or that none does) is cached there
//! too, in `accounts.json`, for a week: `gh` is asked about new authors only.

use std::collections::{BTreeSet, HashMap};
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use serde::{Deserialize, Serialize};

/// Lowercased author email → avatar image file.
pub type Avatars = HashMap<String, PathBuf>;

const MAX_AGE: Duration = Duration::from_secs(7 * 24 * 3600);

fn cache_dir() -> Option<PathBuf> {
    if cfg!(target_os = "macos") {
        let home = std::env::var_os("HOME")?;
        return Some(PathBuf::from(home).join("Library/Caches/SuperGIT/avatars"));
    }
    Some(crate::settings::xdg_dir("XDG_CACHE_HOME", ".cache")?.join("supergit/avatars"))
}

/// Avatars for every author in the repository's history. Blocking: run it
/// on a background thread.
pub fn load(repo: &Path) -> Avatars {
    let Some(dir) = cache_dir() else {
        return Avatars::new();
    };
    let emails: BTreeSet<String> =
        sg_git::git(repo, ["log", "--all", "--format=%aE", "-n", "2000"])
            .map(|out| {
                out.lines()
                    .map(|line| line.trim().to_ascii_lowercase())
                    .filter(|line| !line.is_empty())
                    .collect()
            })
            .unwrap_or_default();
    // email → (login, avatar URL)
    let mut accounts: HashMap<String, (String, String)> = emails
        .iter()
        .filter_map(|email| Some((email.clone(), sg_github::noreply_avatar(email)?)))
        .collect();
    let mut known = KnownAccounts::read(&dir);
    let now = now_secs();
    let mut unknown = Vec::new();
    for email in &emails {
        if accounts.contains_key(email) {
            continue;
        }
        match known.emails.get(email).filter(|account| account.fresh(now)) {
            Some(account) if !account.login.is_empty() => {
                accounts.insert(email.clone(), (account.login.clone(), account.url.clone()));
            }
            Some(_) => {}
            None => unknown.push(email),
        }
    }
    let _ = std::fs::create_dir_all(&dir);
    if !unknown.is_empty()
        && sg_github::gh_available()
        && let Ok(authors) = sg_github::commit_authors(repo)
    {
        let found: HashMap<String, (String, String)> = authors
            .into_iter()
            .map(|author| {
                (
                    author.email.to_ascii_lowercase(),
                    (author.login, author.avatar_url),
                )
            })
            .collect();
        for email in unknown {
            // Not found is remembered too: asked again next week.
            let (login, url) = found.get(email).cloned().unwrap_or_default();
            if !login.is_empty() {
                accounts.insert(email.clone(), (login.clone(), url.clone()));
            }
            known.emails.insert(
                email.clone(),
                KnownAccount {
                    login,
                    url,
                    checked: now,
                },
            );
        }
        known.write(&dir);
    }
    // One download per account (several emails may share it), a few at a
    // time: each is a `curl` of up to ten seconds.
    let mut unique: HashMap<String, (String, String)> = HashMap::new();
    for (login, url) in accounts.values() {
        unique
            .entry(login.to_ascii_lowercase())
            .or_insert_with(|| (login.clone(), url.clone()));
    }
    let unique: Vec<_> = unique.into_iter().collect();
    let downloaded = sg_git::parallel::map(&unique, |(_, (login, url))| download(&dir, login, url));
    let files: HashMap<String, PathBuf> = unique
        .into_iter()
        .zip(downloaded)
        .filter_map(|((key, _), file)| Some((key, file.ok()??)))
        .collect();
    accounts
        .into_iter()
        .filter_map(|(email, (login, _))| {
            Some((email, files.get(&login.to_ascii_lowercase())?.clone()))
        })
        .collect()
}

/// Email → GitHub account as `gh` resolved it, kept between runs.
#[derive(Default, Serialize, Deserialize)]
struct KnownAccounts {
    #[serde(default)]
    emails: HashMap<String, KnownAccount>,
}

#[derive(Clone, Serialize, Deserialize)]
struct KnownAccount {
    /// Empty when GitHub had no account for the email.
    login: String,
    url: String,
    /// When `gh` was asked, in seconds since the Unix epoch.
    checked: u64,
}

impl KnownAccount {
    fn fresh(&self, now: u64) -> bool {
        now.saturating_sub(self.checked) < MAX_AGE.as_secs()
    }
}

impl KnownAccounts {
    const FILE: &str = "accounts.json";

    fn read(dir: &Path) -> Self {
        std::fs::read(dir.join(Self::FILE))
            .ok()
            .and_then(|bytes| serde_json::from_slice(&bytes).ok())
            .unwrap_or_default()
    }

    fn write(&self, dir: &Path) {
        let Ok(bytes) = serde_json::to_vec(self) else {
            return;
        };
        let partial = dir.join(format!(".{}.partial", Self::FILE));
        if std::fs::write(&partial, bytes).is_ok() {
            let _ = std::fs::rename(&partial, dir.join(Self::FILE));
        }
    }
}

fn now_secs() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map_or(0, |since| since.as_secs())
}

/// The cached image for `login`, downloading it when missing or old.
fn download(dir: &Path, login: &str, url: &str) -> Option<PathBuf> {
    // GitHub logins are letters, digits and dashes; avatars are served over https.
    if login.is_empty()
        || !login.chars().all(|c| c.is_ascii_alphanumeric() || c == '-')
        || !url.starts_with("https://")
    {
        return None;
    }
    let login = login.to_ascii_lowercase();
    let file = dir.join(format!("{login}.png"));
    let fresh = file
        .metadata()
        .and_then(|m| m.modified())
        .ok()
        .and_then(|modified| SystemTime::now().duration_since(modified).ok())
        .is_some_and(|age| age < MAX_AGE);
    if fresh {
        return Some(file);
    }
    let partial = dir.join(format!(".{login}.download"));
    let downloaded = Command::new("curl")
        .args(["-sfL", "--max-time", "10", "-o"])
        .arg(&partial)
        .arg(url)
        .stdin(Stdio::null())
        .status()
        .is_ok_and(|status| status.success());
    if downloaded && std::fs::rename(&partial, &file).is_ok() {
        return Some(file);
    }
    let _ = std::fs::remove_file(&partial);
    file.exists().then_some(file)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn known_accounts_round_trip_and_expire_after_a_week() {
        let dir = std::env::temp_dir().join(format!("supergit-avatars-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let mut known = KnownAccounts::default();
        let account = |login: &str, checked| KnownAccount {
            login: login.into(),
            url: format!("https://avatars.example/{login}"),
            checked,
        };
        known.emails.insert("a@x".into(), account("alice", 1_000));
        known.emails.insert("nobody@x".into(), account("", 1_000));
        known.write(&dir);
        let read = KnownAccounts::read(&dir);
        assert_eq!(read.emails["a@x"].login, "alice");
        assert!(read.emails["nobody@x"].login.is_empty());
        assert!(read.emails["a@x"].fresh(1_000 + MAX_AGE.as_secs() - 1));
        assert!(!read.emails["a@x"].fresh(1_000 + MAX_AGE.as_secs()));
        // A missing or broken file is an empty cache, not an error.
        std::fs::write(dir.join(KnownAccounts::FILE), b"{").unwrap();
        assert!(KnownAccounts::read(&dir).emails.is_empty());
        std::fs::remove_dir_all(&dir).unwrap();
    }
}
