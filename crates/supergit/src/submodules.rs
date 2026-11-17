//! How a changed submodule reads in the review: a short summary on its row
//! in a file list, and a card under its entry in the diff naming the two
//! recorded commits, the commits the update brings in or drops, and what is
//! uncommitted in its checkout. Its own files follow it in the diff
//! (`sg_git::inline_submodules`).

use gpui_kit::assets::IconName;
use gpui_kit::component::{Icon, Sizable as _, Theme, h_flex, v_flex};
use gpui_kit::prelude::*;
use gpui_kit::*;
use sg_git::{Inline, SubmoduleChange};

/// Commits listed in a card; the rest are counted.
const CARD_COMMITS: usize = 30;

/// A commit id as shown: its first seven characters.
pub fn short(sha: &str) -> &str {
    sha.get(..7).unwrap_or(sha)
}

/// For a file-list row: `+2 commits · modified`.
pub fn summary(change: &SubmoduleChange) -> String {
    let mut parts = Vec::new();
    match (&change.old, &change.new) {
        (None, Some(_)) => parts.push("new".to_string()),
        (Some(_), None) => parts.push("removed".to_string()),
        (Some(old), Some(new)) if old != new => match commit_counts(change) {
            Some(counts) => parts.push(counts),
            None => parts.push("moved".to_string()),
        },
        _ => {}
    }
    if change.modified {
        parts.push("modified".to_string());
    }
    if change.untracked {
        parts.push("untracked".to_string());
    }
    parts.join(" · ")
}

/// `+2 commits`, `−1 commit`, `+3 −1 commits`; `None` when they are unknown.
fn commit_counts(change: &SubmoduleChange) -> Option<String> {
    let (added, removed) = (change.added_commits, change.removed_commits);
    let noun = |n: usize| if n == 1 { "commit" } else { "commits" };
    match (added, removed) {
        (0, 0) => None,
        (added, 0) => Some(format!("+{added} {}", noun(added))),
        (0, removed) => Some(format!("−{removed} {}", noun(removed))),
        (added, removed) => Some(format!("+{added} −{removed} commits")),
    }
}

/// `Submodule ec97fb1 → d45142b`.
pub fn headline(change: &SubmoduleChange) -> String {
    match (&change.old, &change.new) {
        (Some(old), Some(new)) if old == new => format!("Submodule at {}", short(new)),
        (Some(old), Some(new)) => format!("Submodule {} → {}", short(old), short(new)),
        (None, Some(new)) => format!("New submodule at {}", short(new)),
        (Some(old), None) => format!("Submodule removed, was at {}", short(old)),
        (None, None) => "Submodule".to_string(),
    }
}

/// What the update does to the submodule's history.
fn history(change: &SubmoduleChange) -> Option<String> {
    let (added, removed) = (change.added_commits, change.removed_commits);
    let commits = |n: usize| format!("{n} {}", if n == 1 { "commit" } else { "commits" });
    match (added, removed) {
        (0, 0) => None,
        (added, 0) => Some(format!("Brings in {}", commits(added))),
        (0, removed) => Some(format!("Rewinds {}", commits(removed))),
        (added, removed) => Some(format!(
            "Diverged: brings in {}, drops {}",
            commits(added),
            commits(removed)
        )),
    }
}

/// What is uncommitted in the checkout (its files follow in the diff).
fn checkout_state(change: &SubmoduleChange) -> Option<String> {
    match (change.modified, change.untracked) {
        (true, true) => Some("Uncommitted and untracked files in the checkout".to_string()),
        (true, false) => Some("Uncommitted changes in the checkout".to_string()),
        (false, true) => Some("Untracked files in the checkout".to_string()),
        (false, false) => None,
    }
}

/// Why the submodule's own diff is not shown.
fn notes(change: &SubmoduleChange) -> Vec<String> {
    let mut notes = Vec::new();
    match &change.inline {
        Inline::Shown | Inline::AddedOrRemoved => {}
        Inline::NotCheckedOut => notes.push(
            "Not checked out here: git submodule update --init shows what changed".to_string(),
        ),
        Inline::CommitsMissing => notes.push(
            "The checkout lacks these commits: fetch in the submodule to see them".to_string(),
        ),
        Inline::TooLarge(files) => {
            notes.push(format!("{files} files changed inside: too many to show"));
        }
        Inline::Failed(error) => notes.push(format!("Could not read the submodule: {error}")),
    }
    notes
}

fn more_commits(change: &SubmoduleChange) -> Option<String> {
    let listed = change.commits.len().min(CARD_COMMITS);
    let total = change.added_commits + change.removed_commits;
    (total > listed).then(|| format!("… and {} more", total - listed))
}

/// Identifies what a card shows, so the diff re-measures it when it changes.
pub fn card_id(change: &SubmoduleChange) -> SharedString {
    format!(
        "submodule:{}:{}:{}:{}{}:{}",
        change.path,
        change.old.as_deref().unwrap_or("-"),
        change.new.as_deref().unwrap_or("-"),
        u8::from(change.modified),
        u8::from(change.untracked),
        change.added_commits + change.removed_commits,
    )
    .into()
}

/// The `submodule` tag after the path in its diff header.
pub fn tag(theme: &Theme) -> Div {
    div()
        .px_1()
        .rounded_sm()
        .border_1()
        .border_color(theme.border)
        .text_xs()
        .text_color(theme.muted_foreground)
        .child("submodule")
}

/// The card under a submodule's entry in the diff.
pub fn card(change: &SubmoduleChange, theme: &Theme) -> Div {
    let muted = theme.muted_foreground;
    let mono = theme.mono_font_family.clone();
    let commit_rows = change
        .commits
        .iter()
        .take(CARD_COMMITS)
        .map(|commit| {
            let (sign, color) = if commit.removed {
                ("−", theme.danger)
            } else {
                ("+", theme.success)
            };
            h_flex()
                .gap_2()
                .min_w_0()
                .child(
                    div()
                        .w(px(10.))
                        .flex_none()
                        .font_family(mono.clone())
                        .text_color(color)
                        .child(sign),
                )
                .child(
                    div()
                        .flex_none()
                        .font_family(mono.clone())
                        .text_color(muted)
                        .child(short(&commit.sha).to_string()),
                )
                .child(div().min_w_0().truncate().child(commit.subject.clone()))
        })
        .collect::<Vec<_>>();
    v_flex()
        .w_full()
        .gap_1()
        .text_sm()
        .child(
            h_flex()
                .gap_2()
                .child(Icon::new(IconName::FolderGit2).small().text_color(muted))
                .child(
                    div()
                        .font_weight(FontWeight::MEDIUM)
                        .child(headline(change)),
                ),
        )
        .when_some(history(change), |el, history| {
            el.child(div().pl_6().text_xs().text_color(muted).child(history))
        })
        .when(!commit_rows.is_empty(), |el| {
            el.child(v_flex().pl_6().gap_0p5().children(commit_rows))
        })
        .when_some(more_commits(change), |el, more| {
            el.child(div().pl_6().text_xs().text_color(muted).child(more))
        })
        .when_some(checkout_state(change), |el, state| {
            el.child(div().pl_6().text_xs().text_color(muted).child(state))
        })
        .children(
            notes(change)
                .into_iter()
                .map(|note| div().pl_6().text_xs().text_color(theme.warning).child(note)),
        )
}

#[cfg(test)]
mod tests {
    // Not `super::*`: GPUI's glob import brings its own `test` attribute.
    use super::{checkout_state, headline, history, more_commits, notes, summary};
    use sg_git::{Inline, SubmoduleChange, SubmoduleCommit};

    fn change(old: Option<&str>, new: Option<&str>) -> SubmoduleChange {
        SubmoduleChange {
            path: "vendor/lib".into(),
            old: old.map(str::to_string),
            new: new.map(str::to_string),
            modified: false,
            untracked: false,
            commits: Vec::new(),
            added_commits: 0,
            removed_commits: 0,
            inline: Inline::Shown,
            files: Vec::new(),
        }
    }

    #[test]
    fn summarizes_updates_for_file_rows() {
        let mut update = change(Some("ec97fb1aaaa"), Some("d45142bbbbb"));
        update.added_commits = 2;
        update.modified = true;
        assert_eq!(summary(&update), "+2 commits · modified");
        assert_eq!(headline(&update), "Submodule ec97fb1 → d45142b");
        update.removed_commits = 1;
        assert_eq!(summary(&update), "+2 −1 commits · modified");

        let mut missing = change(Some("ec97fb1aaaa"), Some("d45142bbbbb"));
        missing.inline = Inline::CommitsMissing;
        assert_eq!(summary(&missing), "moved");
        assert!(notes(&missing).iter().any(|note| note.contains("fetch")));

        assert_eq!(summary(&change(None, Some("d45142b"))), "new");
        let mut dirty = change(Some("d45142b"), Some("d45142b"));
        dirty.untracked = true;
        assert_eq!(summary(&dirty), "untracked");
        assert_eq!(
            checkout_state(&dirty).as_deref(),
            Some("Untracked files in the checkout")
        );
        assert_eq!(headline(&dirty), "Submodule at d45142b");
    }

    #[test]
    fn describes_the_history_an_update_changes() {
        let mut rewind = change(Some("d45142bbbbb"), Some("ec97fb1aaaa"));
        rewind.removed_commits = 1;
        rewind.commits = vec![SubmoduleCommit {
            sha: "1234567890".into(),
            subject: "Добавить кеш".into(),
            removed: true,
        }];
        assert_eq!(headline(&rewind), "Submodule d45142b → ec97fb1");
        assert_eq!(history(&rewind).as_deref(), Some("Rewinds 1 commit"));
        assert_eq!(more_commits(&rewind), None);
        rewind.removed_commits = 40;
        assert_eq!(more_commits(&rewind).as_deref(), Some("… and 39 more"));
        rewind.added_commits = 2;
        assert_eq!(
            history(&rewind).as_deref(),
            Some("Diverged: brings in 2 commits, drops 40 commits")
        );
    }
}
