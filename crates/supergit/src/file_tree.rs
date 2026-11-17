//! The Changes tab's file list: filtering by path and the folder-tree view.
//! Folders come before files, and a chain of folders that only hold one
//! folder each is compacted into a single row (`crates/app/src`), the way
//! code editors show them. A changed path that also holds changed files —
//! a submodule with its own changes — is one folder row carrying the entry.

use std::collections::{BTreeMap, HashSet};

/// One row of the tree, in display order.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum TreeRow {
    Dir {
        /// Full path from the repository root; the key for collapsing.
        path: String,
        /// What the row shows: one folder, or a compacted chain `a/b/c`.
        name: String,
        depth: usize,
        collapsed: bool,
        /// Changed files anywhere below.
        files: usize,
        /// The change to this path itself (a submodule), by index.
        entry: Option<usize>,
    },
    File {
        /// Index of the file in the list given to [`rows`].
        ix: usize,
        name: String,
        depth: usize,
    },
}

/// Whether `path` contains every whitespace-separated term of `query`,
/// ignoring case. An empty query matches everything.
pub fn matches(path: &str, query: &str) -> bool {
    let path = path.to_lowercase();
    query
        .split_whitespace()
        .all(|term| path.contains(&term.to_lowercase()))
}

#[derive(Default)]
struct Node {
    dirs: BTreeMap<String, Node>,
    files: Vec<(String, usize)>,
    /// A file at this folder's own path.
    entry: Option<usize>,
}

impl Node {
    /// Changed paths below, not counting the folder's own entry.
    fn file_count(&self) -> usize {
        self.files.len()
            + self
                .dirs
                .values()
                .map(|dir| dir.file_count() + usize::from(dir.entry.is_some()))
                .sum::<usize>()
    }

    /// Moves files named like a sibling folder into that folder's entry.
    fn fold_entries(&mut self) {
        for (name, ix) in std::mem::take(&mut self.files) {
            match self.dirs.get_mut(&name) {
                Some(dir) if dir.entry.is_none() => dir.entry = Some(ix),
                _ => self.files.push((name, ix)),
            }
        }
        self.dirs.values_mut().for_each(Node::fold_entries);
    }
}

/// The tree rows for `files` (index, repository-relative path). Folders in
/// `collapsed` show without their contents.
pub fn rows<'a>(
    files: impl IntoIterator<Item = (usize, &'a str)>,
    collapsed: &HashSet<String>,
) -> Vec<TreeRow> {
    let mut root = Node::default();
    for (ix, path) in files {
        let mut parts: Vec<&str> = path.split('/').filter(|p| !p.is_empty()).collect();
        let Some(name) = parts.pop() else {
            continue;
        };
        let mut node = &mut root;
        for part in parts {
            node = node.dirs.entry(part.to_string()).or_default();
        }
        node.files.push((name.to_string(), ix));
    }
    root.fold_entries();
    let mut out = Vec::new();
    push_rows(&root, "", 0, collapsed, &mut out);
    out
}

fn push_rows(
    node: &Node,
    prefix: &str,
    depth: usize,
    collapsed: &HashSet<String>,
    out: &mut Vec<TreeRow>,
) {
    let mut dirs: Vec<(&String, &Node)> = node.dirs.iter().collect();
    dirs.sort_by_cached_key(|(name, _)| name.to_lowercase());
    for (name, mut dir) in dirs {
        let mut name = name.clone();
        // A folder holding nothing but one folder joins it: `a/b`.
        while dir.files.is_empty() && dir.dirs.len() == 1 && dir.entry.is_none() {
            let Some((sub, inner)) = dir.dirs.iter().next() else {
                break;
            };
            name = format!("{name}/{sub}");
            dir = inner;
        }
        let path = if prefix.is_empty() {
            name.clone()
        } else {
            format!("{prefix}/{name}")
        };
        let is_collapsed = collapsed.contains(&path);
        out.push(TreeRow::Dir {
            path: path.clone(),
            name,
            depth,
            collapsed: is_collapsed,
            files: dir.file_count(),
            entry: dir.entry,
        });
        if !is_collapsed {
            push_rows(dir, &path, depth + 1, collapsed, out);
        }
    }
    let mut files = node.files.clone();
    files.sort_by_cached_key(|(name, _)| name.to_lowercase());
    out.extend(
        files
            .into_iter()
            .map(|(name, ix)| TreeRow::File { ix, name, depth }),
    );
}

#[cfg(test)]
mod tests {
    use super::*;

    fn dir(path: &str, name: &str, depth: usize, collapsed: bool, files: usize) -> TreeRow {
        TreeRow::Dir {
            path: path.into(),
            name: name.into(),
            depth,
            collapsed,
            files,
            entry: None,
        }
    }

    fn file(ix: usize, name: &str, depth: usize) -> TreeRow {
        TreeRow::File {
            ix,
            name: name.into(),
            depth,
        }
    }

    const PATHS: [&str; 5] = [
        "crates/app/src/main.rs",
        "README.md",
        "crates/app/src/ui/view.rs",
        "crates/app/Cargo.toml",
        "crates/core/lib.rs",
    ];

    fn indexed() -> impl Iterator<Item = (usize, &'static str)> {
        PATHS.iter().copied().enumerate()
    }

    #[test]
    fn folders_first_and_single_child_chains_compacted() {
        assert_eq!(
            rows(indexed(), &HashSet::new()),
            vec![
                dir("crates", "crates", 0, false, 4),
                dir("crates/app", "app", 1, false, 3),
                dir("crates/app/src", "src", 2, false, 2),
                dir("crates/app/src/ui", "ui", 3, false, 1),
                file(2, "view.rs", 4),
                file(0, "main.rs", 3),
                file(3, "Cargo.toml", 2),
                dir("crates/core", "core", 1, false, 1),
                file(4, "lib.rs", 2),
                file(1, "README.md", 0),
            ]
        );
        // Only one folder at the top, holding one folder: compacted.
        let deep = [(0, "a/b/c/x.rs"), (1, "a/b/c/y.rs")];
        assert_eq!(
            rows(deep, &HashSet::new()),
            vec![
                dir("a/b/c", "a/b/c", 0, false, 2),
                file(0, "x.rs", 1),
                file(1, "y.rs", 1)
            ]
        );
    }

    #[test]
    fn collapsed_folders_hide_their_contents() {
        let collapsed = HashSet::from(["crates/app".to_string()]);
        assert_eq!(
            rows(indexed(), &collapsed),
            vec![
                dir("crates", "crates", 0, false, 4),
                dir("crates/app", "app", 1, true, 3),
                dir("crates/core", "core", 1, false, 1),
                file(4, "lib.rs", 2),
                file(1, "README.md", 0),
            ]
        );
    }

    #[test]
    fn a_submodule_with_changed_files_is_one_folder_row() {
        let paths = [
            (0, "vendor/lib"),
            (1, "vendor/lib/src/a.rs"),
            (2, "vendor/lib/b.rs"),
            (3, "main.rs"),
        ];
        assert_eq!(
            rows(paths, &HashSet::new()),
            vec![
                TreeRow::Dir {
                    path: "vendor/lib".into(),
                    name: "vendor/lib".into(),
                    depth: 0,
                    collapsed: false,
                    files: 2,
                    entry: Some(0),
                },
                dir("vendor/lib/src", "src", 1, false, 1),
                file(1, "a.rs", 2),
                file(2, "b.rs", 1),
                file(3, "main.rs", 0),
            ]
        );
        // Without files inside, it stays a file.
        assert_eq!(
            rows([(0, "vendor/lib")], &HashSet::new()),
            vec![dir("vendor", "vendor", 0, false, 1), file(0, "lib", 1)]
        );
    }

    #[test]
    fn filter_matches_all_terms_ignoring_case() {
        assert!(matches("crates/app/src/Main.rs", ""));
        assert!(matches("crates/app/src/Main.rs", "main"));
        assert!(matches("crates/app/src/Main.rs", " APP  main "));
        assert!(!matches("crates/app/src/Main.rs", "app lib"));
    }
}
