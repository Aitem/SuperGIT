use std::ffi::OsStr;
use std::path::{Component, Path, PathBuf};

use anyhow::{Context as _, Result, ensure};

use crate::git;

pub fn clone_suggested_name(source: &str) -> Option<String> {
    let source = source.trim();
    if source.is_empty() || source.chars().any(char::is_control) {
        return None;
    }
    let path = if Path::new(source).is_absolute() {
        source
    } else if let Some(path) = source.strip_prefix("file://") {
        path.strip_prefix("localhost")
            .unwrap_or(path)
            .starts_with('/')
            .then_some(path)?
    } else if let Some((scheme, remote)) = source.split_once("://") {
        if !matches!(scheme, "https" | "http" | "ssh" | "git") {
            return None;
        }
        let (host, path) = remote.split_once('/')?;
        if host.is_empty() || host.chars().any(char::is_whitespace) {
            return None;
        }
        path.split(['?', '#']).next()?
    } else {
        let (remote, path) = source.split_once(':')?;
        let (user, host) = remote.rsplit_once('@')?;
        if user.is_empty()
            || host.is_empty()
            || remote.starts_with('-')
            || remote
                .chars()
                .any(|character| character.is_whitespace() || character == '/')
            || path.starts_with(':')
        {
            return None;
        }
        path
    };
    let filename = path.trim_end_matches('/').rsplit('/').next()?;
    let auto_name = filename.strip_suffix(".git").unwrap_or(filename);
    clone_folder_name(auto_name).then(|| auto_name.to_owned())
}

pub fn clone_folder_name(value: &str) -> bool {
    let mut components = Path::new(value).components();
    !value.chars().any(char::is_control)
        && matches!(components.next(), Some(Component::Normal(name)) if name == OsStr::new(value))
        && components.next().is_none()
}

pub fn clone_repository(source: &str, parent: &Path, name: &str) -> Result<PathBuf> {
    ensure!(
        clone_suggested_name(source).is_some(),
        "Use a Git repository URL or an absolute local repository path"
    );
    ensure!(
        clone_folder_name(name),
        "Enter a single destination folder name"
    );
    ensure!(
        parent.is_absolute() && parent.is_dir(),
        "Choose a destination directory"
    );
    let destination = parent.join(name);
    std::fs::create_dir(&destination)
        .with_context(|| format!("Cannot create {}", destination.display()))?;
    let result = git(
        parent,
        [
            OsStr::new("clone"),
            OsStr::new("--"),
            OsStr::new(source.trim()),
            destination.as_os_str(),
        ],
    );
    if result.is_err() {
        // Clone may leave files after a checkout error. Remove only an empty reservation.
        let _ = std::fs::remove_dir(&destination);
    }
    result.map(|_| destination)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn derives_folder_names_from_supported_git_sources() {
        assert_eq!(
            clone_suggested_name("https://github.com/team/app.git?foo=4").as_deref(),
            Some("app")
        );
        assert_eq!(
            clone_suggested_name("git@github.com:team/app.git").as_deref(),
            Some("app")
        );
        assert_eq!(
            clone_suggested_name("ssh://git@example.org:2222/team/app").as_deref(),
            Some("app")
        );
        assert_eq!(
            clone_suggested_name("/home/user/my project").as_deref(),
            Some("my project")
        );
        assert!(clone_suggested_name("ext::shell").is_none());
        assert!(clone_suggested_name("https://github.com/").is_none());
        assert!(!clone_folder_name("../app"));
        assert!(!clone_folder_name("app/subfolder"));
    }

    #[test]
    fn clones_into_new_directory_and_never_reuses_a_destination() {
        let root = tempfile::tempdir().unwrap();
        let source = root.path().join("source");
        git(
            root.path(),
            ["init", "-q", "-b", "main", source.to_str().unwrap()],
        )
        .unwrap();
        git(
            &source,
            [
                "-c",
                "user.name=Fixture",
                "-c",
                "user.email=fixture@example.test",
                "-c",
                "core.hooksPath=/dev/null",
                "commit",
                "--no-gpg-sign",
                "--allow-empty",
                "-qm",
                "Initial",
            ],
        )
        .unwrap();
        let cloned = clone_repository(source.to_str().unwrap(), root.path(), "New folder").unwrap();
        assert_eq!(
            git(&cloned, ["log", "-1", "--format=%s"]).unwrap().trim(),
            "Initial"
        );
        assert!(clone_repository(source.to_str().unwrap(), root.path(), "New folder").is_err());
        assert!(clone_repository("/invalid/source/repo", root.path(), "failed").is_err());
        assert!(!root.path().join("failed").exists());
    }
}
