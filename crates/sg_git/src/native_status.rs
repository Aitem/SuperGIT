//! Experimental read path for ordinary worktree status. The CLI handles index
//! layouts, rename heuristics, configured filters and gitlinks outside this set.

use std::path::Path;

use anyhow::{Context as _, Result, ensure};
use git2::{ErrorCode, Repository, Status, StatusOptions};

use crate::{FileChange, FileState, WorktreeStatus};

const INDEX_STATES: [(Status, FileState); 4] = [
    (Status::INDEX_NEW, FileState::Added),
    (Status::INDEX_MODIFIED, FileState::Modified),
    (Status::INDEX_DELETED, FileState::Deleted),
    (Status::INDEX_TYPECHANGE, FileState::TypeChanged),
];
const WORKTREE_STATES: [(Status, FileState); 4] = [
    (Status::WT_NEW, FileState::Untracked),
    (Status::WT_MODIFIED, FileState::Modified),
    (Status::WT_DELETED, FileState::Deleted),
    (Status::WT_TYPECHANGE, FileState::TypeChanged),
];

pub fn read(path: &Path) -> Result<WorktreeStatus> {
    read_repo(Repository::open(path)?)
}

/// A synthetic fixture benchmark can isolate the local Git config.
pub fn read_fixture(path: &Path) -> Result<WorktreeStatus> {
    let repo = Repository::open(path)?;
    let local = repo.config()?.open_level(git2::ConfigLevel::Local)?;
    repo.set_config(&local)?;
    read_repo(repo)
}

fn read_repo(repo: Repository) -> Result<WorktreeStatus> {
    ensure!(
        !repo.is_bare() && !repo.is_shallow(),
        "Bare or shallow repository"
    );
    ensure!(
        repo.references_glob("refs/replace/*")?.next().is_none()
            && !repo.commondir().join("info/grafts").exists(),
        "Rewritten Git history"
    );
    check_read_support(&repo)?;
    let mut result = read_head(&repo)?;
    let mut options = StatusOptions::new();
    options
        .include_untracked(true)
        .recurse_untracked_dirs(true)
        .include_ignored(false)
        .sort_case_sensitively(true)
        .update_index(false);
    let entries = repo.statuses(Some(&mut options))?;
    let mut has_addition = false;
    let mut has_deletion = false;
    for entry in entries.iter() {
        let status = entry.status();
        let path = std::str::from_utf8(entry.path_bytes())?.to_string();
        ensure!(
            !status.intersects(Status::WT_RENAMED | Status::INDEX_RENAMED | Status::WT_UNREADABLE),
            "Status needs Git CLI"
        );
        ensure!(
            !status.contains(Status::CONFLICTED),
            "Unmerged index entries"
        );
        let staged = decode(status, &INDEX_STATES);
        let unstaged = decode(status, &WORKTREE_STATES);
        if staged.is_none() && unstaged.is_none() {
            continue;
        }
        has_addition |= staged == Some(FileState::Added);
        has_deletion |= staged == Some(FileState::Deleted);
        result.files.push(FileChange {
            path,
            staged,
            unstaged,
            ..Default::default()
        });
    }
    ensure!(
        !has_addition || !has_deletion,
        "Git CLI rename detection required"
    );
    result.files.sort_by(|a, b| {
        let untracked = |change: &FileChange| change.unstaged == Some(FileState::Untracked);
        untracked(a)
            .cmp(&untracked(b))
            .then_with(|| a.path.cmp(&b.path))
    });
    Ok(result)
}

fn decode(status: Status, candidates: &[(Status, FileState)]) -> Option<FileState> {
    candidates
        .iter()
        .find_map(|(flag, state)| status.contains(*flag).then_some(*state))
}

fn check_read_support(repo: &Repository) -> Result<()> {
    let config = repo.config()?;
    for setting in [
        "core.sparseCheckout",
        "core.splitIndex",
        "extensions.worktreeConfig",
        "core.ignoreStat",
    ] {
        ensure!(
            !config.get_bool(setting).unwrap_or_default(),
            "Git CLI required for {setting}"
        );
    }
    ensure!(
        config.get_string("core.fsmonitor").is_err()
            && config.get_string("core.attributesFile").is_err(),
        "Custom worktree stat configuration"
    );
    ensure!(
        config.entries(Some(r"^filter\."))?.next().is_none(),
        "Custom Git filters"
    );
    let rename_choice = config
        .get_string("status.renames")
        .or_else(|_| config.get_string("diff.renames"));
    ensure!(
        !rename_choice.is_ok_and(|value| value.eq_ignore_ascii_case("copies")),
        "Copy detection requires Git CLI"
    );
    let index = repo.index()?;
    ensure!(!index.has_conflicts(), "Unmerged index entries");
    ensure!(
        !index.iter().any(|item| item.mode == 0o160000
            || item.flags_extended & ((1 << 14) | (1 << 13)) != 0
            || item.flags & (1 << 15) != 0),
        "Index uses gitlinks or special flags"
    );
    Ok(())
}

fn read_head(repo: &Repository) -> Result<WorktreeStatus> {
    let reference = repo.find_reference("HEAD")?;
    let symbolic = reference.symbolic_target()?;
    let branch = symbolic
        .map(|name| {
            name.strip_prefix("refs/heads/")
                .context("Non-branch symbolic HEAD")
        })
        .transpose()?
        .map(str::to_string);
    let head = match repo.head() {
        Ok(head) => Some(head.target().context("HEAD has no object ID")?),
        Err(error) if error.code() == ErrorCode::UnbornBranch => None,
        Err(error) => return Err(error.into()),
    };
    let mut result = WorktreeStatus {
        head_oid: head.map(|id| id.to_string()),
        branch,
        ..Default::default()
    };
    if let Some(reference) = symbolic {
        match repo.branch_upstream_name(reference) {
            Ok(name) => {
                let name = name.as_str().context("Upstream name is not valid UTF-8")?;
                result.upstream = Some(
                    name.strip_prefix("refs/remotes/")
                        .or_else(|| name.strip_prefix("refs/heads/"))
                        .unwrap_or(name)
                        .to_string(),
                );
                let include_counts = repo
                    .config()?
                    .get_bool("status.aheadBehind")
                    .unwrap_or(true);
                if include_counts && let Some(head) = head {
                    match repo.refname_to_id(name) {
                        Ok(upstream) => {
                            let (ahead, behind) = repo.graph_ahead_behind(head, upstream)?;
                            result.ahead = ahead.try_into()?;
                            result.behind = behind.try_into()?;
                        }
                        Err(error) if error.code() == ErrorCode::NotFound => (),
                        Err(error) => return Err(error.into()),
                    }
                }
            }
            Err(error) if error.code() == ErrorCode::NotFound => (),
            Err(error) => return Err(error.into()),
        }
    }
    Ok(result)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{git, status};

    fn repository() -> tempfile::TempDir {
        let temp = tempfile::tempdir().unwrap();
        let path = temp.path();
        git(path, ["init", "-q", "-b", "main"]).unwrap();
        for (key, value) in [
            ("user.name", "Fixture"),
            ("user.email", "fixture@example.test"),
            ("commit.gpgsign", "false"),
            ("core.hooksPath", "/dev/null"),
        ] {
            git(path, ["config", key, value]).unwrap();
        }
        temp
    }

    fn assert_parity(path: &Path) {
        assert_eq!(read_fixture(path).unwrap(), status(path).unwrap());
    }

    #[test]
    fn native_poll_matches_partial_edits_and_linked_or_detached_heads() {
        let temp = repository();
        let main = temp.path();
        std::fs::write(main.join(".gitignore"), "ignored.txt\n").unwrap();
        std::fs::write(main.join("pick.txt"), "base\n").unwrap();
        assert_parity(main);
        git(main, ["add", "."]).unwrap();
        git(main, ["commit", "-qm", "base"]).unwrap();
        git(
            main,
            [
                "remote",
                "add",
                "origin",
                "https://example.invalid/example.git",
            ],
        )
        .unwrap();
        git(main, ["update-ref", "refs/remotes/origin/main", "HEAD"]).unwrap();
        let linked = tempfile::tempdir().unwrap();
        let branch = linked.path().join("feature");
        crate::add_worktree(main, &branch, Some("feature"), Some("HEAD")).unwrap();
        git(&branch, ["branch", "--set-upstream-to", "origin/main"]).unwrap();
        git(&branch, ["commit", "--allow-empty", "-qm", "ahead"]).unwrap();
        std::fs::write(branch.join("pick.txt"), "staged\n").unwrap();
        git(&branch, ["add", "pick.txt"]).unwrap();
        std::fs::write(branch.join("pick.txt"), "working\n").unwrap();
        std::fs::write(branch.join("untracked.txt"), "new\n").unwrap();
        std::fs::write(branch.join("ignored.txt"), "ignored\n").unwrap();
        assert_parity(&branch);
        git(&branch, ["switch", "-q", "--detach", "HEAD"]).unwrap();
        assert_parity(&branch);
    }

    #[test]
    fn gitlinks_and_ambiguous_renames_leave_the_native_path() {
        let temp = repository();
        let main = temp.path();
        std::fs::write(main.join("old.txt"), "rename me\n").unwrap();
        git(main, ["add", "."]).unwrap();
        git(main, ["commit", "-qm", "initial"]).unwrap();
        git(main, ["mv", "old.txt", "new.txt"]).unwrap();
        assert!(read_fixture(main).is_err());
        let tip = git(main, ["rev-parse", "HEAD"]).unwrap();
        let gitlink = format!("160000,{},module", tip.trim());
        git(main, ["update-index", "--add", "--cacheinfo", &gitlink]).unwrap();
        assert!(
            read_fixture(main)
                .unwrap_err()
                .to_string()
                .contains("gitlinks")
        );
    }
}
