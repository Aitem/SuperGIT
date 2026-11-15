//! SVG icons: the components' default set (search, chevrons, folders, the
//! input's clear button…) plus the few extra Lucide icons SuperGIT draws.
//! Embedding only these keeps the full 7 MB catalog out of the binary.

use std::borrow::Cow;

use gpui_kit::assets::Assets;
use gpui_kit::{AssetSource, SharedString};

gpui_kit::assets::icon_assets!(ExtraIcons, [List, ListTree, Tag]);

pub struct AppAssets;

impl AssetSource for AppAssets {
    fn load(&self, path: &str) -> anyhow::Result<Option<Cow<'static, [u8]>>> {
        match ExtraIcons.load(path)? {
            Some(svg) => Ok(Some(svg)),
            None => Assets.load(path),
        }
    }

    fn list(&self, path: &str) -> anyhow::Result<Vec<SharedString>> {
        let mut paths = Assets.list(path)?;
        paths.extend(ExtraIcons.list(path)?);
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
            IconName::ChevronRight,
            IconName::ChevronDown,
            IconName::Folder,
            IconName::FolderOpen,
        ] {
            let path = icon.path();
            assert!(
                matches!(AppAssets.load(&path), Ok(Some(_))),
                "{path} is not embedded"
            );
        }
    }
}
