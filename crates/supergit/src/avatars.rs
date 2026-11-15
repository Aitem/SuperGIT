//! GitHub avatars for commit authors in the History tab.
//!
//! Resolved once per repository: GitHub's private `…@users.noreply.github.com`
//! emails name their account outright, the rest come from one `gh` GraphQL
//! call over the default branch's history (no emails are sent anywhere).
//! Images are downloaded with `curl` into `~/Library/Caches/SuperGIT/avatars`
//! and refreshed weekly; a stale copy is kept when the network fails.

use std::collections::{BTreeSet, HashMap};
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::time::{Duration, SystemTime};

/// Lowercased author email → avatar image file.
pub type Avatars = HashMap<String, PathBuf>;

const MAX_AGE: Duration = Duration::from_secs(7 * 24 * 3600);

fn cache_dir() -> Option<PathBuf> {
    let home = std::env::var_os("HOME")?;
    Some(PathBuf::from(home).join("Library/Caches/SuperGIT/avatars"))
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
    if emails.iter().any(|email| !accounts.contains_key(email))
        && sg_github::gh_available()
        && let Ok(authors) = sg_github::commit_authors(repo)
    {
        for author in authors {
            accounts
                .entry(author.email.to_ascii_lowercase())
                .or_insert((author.login, author.avatar_url));
        }
    }
    let _ = std::fs::create_dir_all(&dir);
    let mut files: HashMap<String, Option<PathBuf>> = HashMap::new();
    accounts
        .into_iter()
        .filter_map(|(email, (login, url))| {
            let file = files
                .entry(login.to_ascii_lowercase())
                .or_insert_with(|| download(&dir, &login, &url))
                .clone()?;
            Some((email, file))
        })
        .collect()
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
