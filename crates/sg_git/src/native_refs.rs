//! Opt-in libgit2 adapter for the History cache key.

use std::path::Path;

use anyhow::{Context as _, Result};
use git2::{ErrorCode, Repository};

const HISTORY_PREFIXES: [&[u8]; 4] = [
    b"refs/heads/",
    b"refs/remotes/",
    b"refs/tags/",
    b"refs/replace/",
];

pub fn read(path: &Path) -> Result<Vec<u8>> {
    let repo = Repository::open(path)?;
    let head = match repo.head() {
        Ok(head) => Some(head),
        Err(error) if matches!(error.code(), ErrorCode::UnbornBranch | ErrorCode::NotFound) => None,
        Err(error) => return Err(error.into()),
    };
    let current = head.as_ref().map(|head| head.name_bytes());
    let mut refs = Vec::new();
    for entry in repo.references()? {
        let entry = entry?;
        let name = entry.name_bytes();
        if !HISTORY_PREFIXES
            .iter()
            .any(|prefix| name.starts_with(prefix))
        {
            continue;
        }
        let resolved = match entry.resolve() {
            Ok(reference) => reference,
            Err(error)
                if error.code() == ErrorCode::NotFound
                    && entry.symbolic_target_bytes().is_some() =>
            {
                continue;
            }
            Err(error) => return Err(error.into()),
        };
        let id = resolved.target().context("Reference has no object ID")?;
        let symref = entry
            .symbolic_target_bytes()
            .map(|_| resolved.name_bytes().to_vec());
        refs.push((name.to_vec(), id, symref));
    }
    refs.sort_unstable_by(|a, b| a.0.cmp(&b.0));
    let mut snapshot = Vec::with_capacity(refs.len() * 90);
    for (name, oid, symref) in refs {
        snapshot.extend_from_slice(if current == Some(name.as_slice()) {
            b"*\0"
        } else {
            b" \0"
        });
        snapshot.extend_from_slice(&name);
        snapshot.push(0);
        snapshot.extend_from_slice(oid.to_string().as_bytes());
        snapshot.push(0);
        if let Some(symref) = symref {
            snapshot.extend_from_slice(&symref);
        }
        snapshot.push(b'\n');
    }
    Ok(snapshot)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{git, log::history_refs_cli};

    #[test]
    fn linked_detached_packed_and_symbolic_refs_match_cli() {
        let temp = tempfile::tempdir().unwrap();
        let main = temp.path().join("main");
        std::fs::create_dir(&main).unwrap();
        git(&main, ["init", "-q", "-b", "main"]).unwrap();
        assert_eq!(read(&main).unwrap(), history_refs_cli(&main).unwrap());
        git(
            &main,
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
                "fixture",
            ],
        )
        .unwrap();
        git(&main, ["update-ref", "refs/remotes/origin/main", "HEAD"]).unwrap();
        git(
            &main,
            [
                "symbolic-ref",
                "refs/remotes/origin/HEAD",
                "refs/remotes/origin/main",
            ],
        )
        .unwrap();
        git(
            &main,
            [
                "symbolic-ref",
                "refs/remotes/other/HEAD",
                "refs/remotes/origin/HEAD",
            ],
        )
        .unwrap();
        git(
            &main,
            [
                "symbolic-ref",
                "refs/remotes/ghost/HEAD",
                "refs/remotes/ghost/missing",
            ],
        )
        .unwrap();
        git(&main, ["tag", "v1"]).unwrap();
        git(&main, ["pack-refs", "--all"]).unwrap();
        let linked = temp.path().join("linked");
        crate::add_worktree(&main, &linked, Some("feature"), Some("main")).unwrap();
        for path in [&main, &linked] {
            assert_eq!(
                String::from_utf8(read(path).unwrap()).unwrap(),
                String::from_utf8(history_refs_cli(path).unwrap()).unwrap()
            );
        }
        git(&linked, ["switch", "-q", "--detach", "HEAD"]).unwrap();
        assert_eq!(read(&linked).unwrap(), history_refs_cli(&linked).unwrap());
    }
}
