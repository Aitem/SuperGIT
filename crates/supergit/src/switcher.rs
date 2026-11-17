//! Navigation data and Unicode-aware fuzzy matching, independent of the UI.

use std::path::PathBuf;

#[derive(Clone)]
pub enum Destination {
    Worktree,
    Conversation(String),
    Transcript(sg_agents::Session),
    /// Another project in the sidebar (`Entry::path` is its root).
    Project,
}

#[derive(Clone)]
pub struct Entry {
    pub key: String,
    pub path: PathBuf,
    pub title: String,
    pub detail: String,
    pub search: String,
    pub destination: Destination,
}

pub fn ranked(entries: &[Entry], query: &str, recent: &[String]) -> Vec<Entry> {
    let query = query.trim().to_lowercase();
    let mut matches: Vec<_> = entries
        .iter()
        .enumerate()
        .filter_map(|(ix, entry)| {
            let score = fuzzy_score(&entry.search, &query)?;
            let recency = recent
                .iter()
                .position(|key| key == &entry.key)
                .unwrap_or(usize::MAX);
            Some((std::cmp::Reverse(score), recency, ix, entry.clone()))
        })
        .collect();
    matches.sort_by_key(|(score, recency, ix, _)| (*score, *recency, *ix));
    matches.into_iter().map(|(_, _, _, entry)| entry).collect()
}

fn fuzzy_score(text: &str, query: &str) -> Option<i64> {
    if query.is_empty() {
        return Some(0);
    }
    let text: Vec<char> = text.to_lowercase().chars().collect();
    let mut offset = 0;
    let mut score = 0;
    let mut previous = None;
    for needle in query.chars().filter(|c| !c.is_whitespace()) {
        let found = text[offset..].iter().position(|c| *c == needle)? + offset;
        score += if previous == Some(found.saturating_sub(1)) {
            12
        } else {
            1
        };
        if found == 0 || !text[found - 1].is_alphanumeric() {
            score += 8;
        }
        score -= (found - offset) as i64;
        previous = Some(found);
        offset = found + 1;
    }
    Some(score)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn entry(key: &str, title: &str) -> Entry {
        Entry {
            key: key.into(),
            path: format!("/repo/{key}").into(),
            title: title.into(),
            detail: key.into(),
            search: title.into(),
            destination: Destination::Worktree,
        }
    }

    #[test]
    fn matches_unicode_fuzzy_branch_pr_and_title() {
        assert!(fuzzy_score("feature/Исправление #42 Save drafts", "испр42sd").is_some());
        assert!(fuzzy_score("feature/fix", "missing").is_none());
        assert!(fuzzy_score("fix", "fix").unwrap() > fuzzy_score("far/in/x", "fix").unwrap());
    }

    #[test]
    fn empty_query_uses_recency_and_duplicate_titles_keep_identity() {
        let items = vec![entry("a", "Same title"), entry("b", "Same title")];
        let results = ranked(&items, "", &["b".into()]);
        assert_eq!(results[0].key, "b");
        assert_ne!(results[0].path, results[1].path);
        assert!(ranked(&items, "xyz", &[]).is_empty());
    }
}
