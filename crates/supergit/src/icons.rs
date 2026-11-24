//! SVG icons: the components' default set (search, chevrons, folders, the
//! input's clear button…) plus the few extra Lucide icons SuperGIT draws.
//! Embedding only these keeps the full 7 MB catalog out of the binary.
//!
//! The agents are drawn with their official marks (`assets/agents`, from
//! Simple Icons, CC0): Claude's, and OpenAI's for Codex.

use std::borrow::Cow;

use gpui_kit::assets::Assets;
use gpui_kit::component::Icon;
use gpui_kit::{AssetSource, SharedString, Styled as _, rgb};
use sg_agents::Provider;

const CLAUDE_MARK: &str = "agents/claude.svg";
const OPENAI_MARK: &str = "agents/openai.svg";
const AGENT_MARKS: [(&str, &[u8]); 2] = [
    (
        CLAUDE_MARK,
        include_bytes!("../../../assets/agents/claude.svg"),
    ),
    (
        OPENAI_MARK,
        include_bytes!("../../../assets/agents/openai.svg"),
    ),
];

/// Claude's brand orange.
const CLAUDE_ORANGE: u32 = 0xD97757;

/// The agent a commit credits as co-author.
pub fn commit_provider(agent: sg_git::CommitAgent) -> Provider {
    match agent {
        sg_git::CommitAgent::Claude => Provider::Claude,
        sg_git::CommitAgent::Codex => Provider::Codex,
    }
}

/// An agent's mark in the color of the text around it (where the color
/// says something else, like a conversation's state).
pub fn agent_mark(provider: Provider) -> Icon {
    Icon::empty().path(match provider {
        Provider::Claude => CLAUDE_MARK,
        Provider::Codex => OPENAI_MARK,
    })
}

/// An agent's mark in its brand color: Claude orange; OpenAI's mark is
/// black or white, so it follows the text.
pub fn agent_icon(provider: Provider) -> Icon {
    match provider {
        Provider::Claude => agent_mark(provider).text_color(rgb(CLAUDE_ORANGE)),
        Provider::Codex => agent_mark(provider),
    }
}

gpui_kit::assets::icon_assets!(
    ExtraIcons,
    [
        List,
        ListTree,
        Tag,
        Rows2,
        Columns2,
        FileDiff,
        ChevronsUpDown,
        FolderGit2,
        Circle,
        // The header's branch actions.
        ArrowDownToLine,
        ArrowUpFromLine,
        GitBranch,
        Trash,
        // The sidebar's rows: a question's signal, changed files, running
        // checks, the main checkout.
        CircleQuestionMark,
        Diff,
        Clock,
        House
    ]
);

pub struct AppAssets;

impl AssetSource for AppAssets {
    fn load(&self, path: &str) -> anyhow::Result<Option<Cow<'static, [u8]>>> {
        if let Some((_, svg)) = AGENT_MARKS.iter().find(|(mark, _)| *mark == path) {
            return Ok(Some(Cow::Borrowed(*svg)));
        }
        match ExtraIcons.load(path)? {
            Some(svg) => Ok(Some(svg)),
            None => Assets.load(path),
        }
    }

    fn list(&self, path: &str) -> anyhow::Result<Vec<SharedString>> {
        let mut paths = Assets.list(path)?;
        paths.extend(ExtraIcons.list(path)?);
        paths.extend(
            AGENT_MARKS
                .iter()
                .filter(|(mark, _)| mark.starts_with(path))
                .map(|(mark, _)| SharedString::from(*mark)),
        );
        Ok(paths)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use gpui_kit::assets::IconName;

    #[test]
    fn icons_used_by_the_app_are_embedded() {
        for icon in [
            IconName::Search,
            IconName::List,
            IconName::ListTree,
            IconName::Tag,
            IconName::Rows2,
            IconName::Columns2,
            IconName::FileDiff,
            IconName::ChevronsUpDown,
            IconName::FolderGit2,
            IconName::ChevronRight,
            IconName::ChevronDown,
            IconName::Folder,
            IconName::FolderOpen,
            IconName::Ellipsis,
            IconName::Plus,
            IconName::ArrowUp,
            IconName::Square,
            // The chat: step marks, cards, the activity line, copy buttons.
            IconName::Check,
            IconName::Close,
            IconName::CircleX,
            IconName::CircleCheck,
            IconName::Circle,
            IconName::LoaderCircle,
            IconName::Loader,
            IconName::TriangleAlert,
            IconName::Map,
            IconName::Copy,
            // The header's branch actions.
            IconName::RefreshCw,
            IconName::ArrowDownToLine,
            IconName::ArrowUpFromLine,
            IconName::GitBranch,
            IconName::Trash,
            // The sidebar's rows: signals, counts, checks, the main checkout.
            IconName::CircleQuestionMark,
            IconName::Diff,
            IconName::ArrowDown,
            IconName::Clock,
            IconName::House,
        ] {
            let path = icon.path();
            assert!(
                matches!(AppAssets.load(&path), Ok(Some(_))),
                "{path} is not embedded"
            );
        }
    }

    #[test]
    fn agent_marks_are_embedded_svgs() {
        for mark in [CLAUDE_MARK, OPENAI_MARK] {
            let svg = AppAssets.load(mark).unwrap().unwrap();
            assert!(svg.starts_with(b"<svg"), "{mark}");
        }
    }
}
