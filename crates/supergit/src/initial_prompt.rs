//! The initial prompt: text SuperGIT adds to the system prompt of every
//! agent it starts (Claude's `--append-system-prompt`, Codex's developer
//! instructions). By default it tells the agent where it works: its
//! worktree, the main checkout, the other worktrees beside them.
//!
//! The template (`Settings::initial_prompt`, this default when unset) takes
//! placeholders — `{worktree}`, `{branch}`, `{repo}`, `{repo_name}` — and
//! three sections: `{#linked}…{/linked}` shows only in a linked worktree,
//! `{#main}…{/main}` only in the main checkout, `{#folder}…{/folder}` only
//! in a folder project (no Git). Any other brace is text.

use std::collections::HashMap;
use std::path::{Path, PathBuf};

use gpui_kit::{App, Global};

pub const DEFAULT: &str = "\
{#linked}You are working in a Git worktree, opened in SuperGIT.

Worktree: {worktree}, on branch {branch}. This is your working directory: make your changes and commits here.
Main checkout: {repo}. The repository's other worktrees sit next to it, and other agents may be working in them right now.

- Edit files only inside this worktree. Reading the main checkout or other worktrees is fine.
- Worktrees share one repository: commits, branches, tags and the stash are seen from all of them. The working tree, the index and untracked files (dependencies, build output, local config such as .env) are this worktree's own, so some may be missing here.
- Stay on {branch}: a branch checked out in another worktree can't be checked out here.{/linked}\
{#main}You are working in the main checkout of {repo_name}: {repo}, on branch {branch}, opened in SuperGIT.

The repository may have other worktrees, with other agents working in them. Edit files only here, and keep in mind that commits, branches and the stash are shared with them.{/main}\
{#folder}You are working in the folder {worktree}, opened in SuperGIT. It is not a Git repository itself, though it may hold some. Other agents may be working in it at the same time.{/folder}";

/// Where an agent works, for the placeholders.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Place {
    /// The worktree's root.
    pub worktree: PathBuf,
    /// `None` when its HEAD is detached.
    pub branch: Option<String>,
    /// The main checkout.
    pub repo: PathBuf,
    /// A linked worktree, not the main checkout.
    pub linked: bool,
    /// A folder project: `worktree` and `repo` are the folder, no Git.
    pub folder: bool,
}

impl Place {
    fn value(&self, name: &str) -> Option<String> {
        Some(match name {
            "worktree" => self.worktree.display().to_string(),
            "branch" if self.folder => "no branch".into(),
            "branch" => self
                .branch
                .clone()
                .unwrap_or_else(|| "a detached HEAD".into()),
            "repo" => self.repo.display().to_string(),
            "repo_name" => self.repo.file_name()?.to_string_lossy().into_owned(),
            _ => return None,
        })
    }

    fn holds(&self, section: &str) -> Option<bool> {
        match section {
            "linked" => Some(self.linked),
            "main" => Some(!self.linked && !self.folder),
            "folder" => Some(self.folder),
            _ => None,
        }
    }
}

/// The open repositories' worktrees by path, kept by the app as they load:
/// a conversation looks its own up when its agent starts.
#[derive(Default)]
pub struct Places(HashMap<PathBuf, Place>);

impl Global for Places {}

impl Places {
    /// The worktrees of the repository at `repo`, replacing those it had.
    pub fn set_repo(repo: &Path, places: impl IntoIterator<Item = Place>, cx: &mut App) {
        let map = &mut cx.default_global::<Self>().0;
        map.retain(|_, place| place.repo != repo);
        map.extend(
            places
                .into_iter()
                .map(|place| (place.worktree.clone(), place)),
        );
    }
}

/// The prompt for an agent starting in `worktree`: `None` when the template
/// is empty or the worktree is not (yet) known.
pub fn for_worktree(worktree: &Path, cx: &App) -> Option<String> {
    let place = cx.try_global::<Places>()?.0.get(worktree)?;
    let settings = crate::settings::load();
    let prompt = render(settings.initial_prompt(), place);
    (!prompt.is_empty()).then_some(prompt)
}

/// `template` filled in for `place`, trimmed.
pub fn render(template: &str, place: &Place) -> String {
    let mut out = String::new();
    // The section being left out, until its end.
    let mut skipping: Option<&str> = None;
    let mut rest = template;
    while let Some(open) = rest.find('{') {
        let (text, tail) = rest.split_at(open);
        if skipping.is_none() {
            out.push_str(text);
        }
        let tag = tail[1..].find('}').map(|end| &tail[1..1 + end]);
        let Some(tag) = tag else {
            rest = tail;
            break;
        };
        let next = &tail[tag.len() + 2..];
        if let Some(section) = tag.strip_prefix('#')
            && let Some(holds) = place.holds(section)
        {
            if skipping.is_none() && !holds {
                skipping = Some(section);
            }
        } else if let Some(section) = tag.strip_prefix('/')
            && place.holds(section).is_some()
        {
            if skipping == Some(section) {
                skipping = None;
            }
        } else if let Some(value) = place.value(tag) {
            if skipping.is_none() {
                out.push_str(&value);
            }
        } else {
            // A brace that is no tag: text, and what follows it too.
            if skipping.is_none() {
                out.push('{');
            }
            rest = &tail[1..];
            continue;
        }
        rest = next;
    }
    if skipping.is_none() {
        out.push_str(rest);
    }
    out.trim().to_string()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn linked() -> Place {
        Place {
            worktree: "/src/app-login".into(),
            branch: Some("feature/login".into()),
            repo: "/src/app".into(),
            linked: true,
            folder: false,
        }
    }

    fn main_checkout() -> Place {
        Place {
            worktree: "/src/app".into(),
            branch: Some("main".into()),
            repo: "/src/app".into(),
            linked: false,
            folder: false,
        }
    }

    fn folder() -> Place {
        Place {
            worktree: "/work/space".into(),
            branch: None,
            repo: "/work/space".into(),
            linked: false,
            folder: true,
        }
    }

    #[test]
    fn placeholders_and_sections_fill_in() {
        let template = "{repo_name}: {worktree} on {branch}{#linked} (from {repo}){/linked}{#main} (main){/main}.";
        assert_eq!(
            render(template, &linked()),
            "app: /src/app-login on feature/login (from /src/app)."
        );
        assert_eq!(
            render(template, &main_checkout()),
            "app: /src/app on main (main)."
        );
        let detached = Place {
            branch: None,
            ..linked()
        };
        assert_eq!(render("on {branch}", &detached), "on a detached HEAD");
    }

    #[test]
    fn other_braces_stay_text() {
        let place = linked();
        assert_eq!(
            render("fn f() { {x} {#other}y{/other} {branch", &place),
            "fn f() { {x} {#other}y{/other} {branch"
        );
        // A value is not read as a template.
        let odd = Place {
            branch: Some("{repo}".into()),
            ..linked()
        };
        assert_eq!(render("{branch}", &odd), "{repo}");
    }

    #[test]
    fn the_default_speaks_of_the_place() {
        let worktree = render(DEFAULT, &linked());
        assert!(worktree.starts_with("You are working in a Git worktree"));
        assert!(worktree.contains("Worktree: /src/app-login, on branch feature/login."));
        assert!(worktree.contains("Main checkout: /src/app."));
        assert!(!worktree.contains('{'));
        let main = render(DEFAULT, &main_checkout());
        assert!(main.starts_with("You are working in the main checkout of app: /src/app"));
        assert!(!main.contains("Worktree:"));
        assert!(!main.contains('{'));
        assert_eq!(render("{#main}only main{/main}", &linked()), "");
        let in_folder = render(DEFAULT, &folder());
        assert!(in_folder.starts_with("You are working in the folder /work/space"));
        assert!(!in_folder.contains("Git worktree") && !in_folder.contains("main checkout"));
        assert!(!in_folder.contains('{'));
        assert_eq!(
            render(
                "{#main}main{/main}{#folder}{repo_name} on {branch}{/folder}",
                &folder()
            ),
            "space on no branch"
        );
    }
}
