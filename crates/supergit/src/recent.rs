//! Recently opened repositories, so a Dock/Finder launch reopens the last one.
//! Stored in `~/Library/Application Support/SuperGIT/recent.json`.

use std::path::{Path, PathBuf};

const MAX: usize = 12;

fn store() -> Option<PathBuf> {
    Some(crate::settings::support_dir()?.join("recent.json"))
}

/// Recent repository roots that still exist, most recent first.
pub fn load() -> Vec<PathBuf> {
    let Some(file) = store() else {
        return Vec::new();
    };
    let Ok(text) = std::fs::read_to_string(file) else {
        return Vec::new();
    };
    serde_json::from_str::<Vec<PathBuf>>(&text)
        .unwrap_or_default()
        .into_iter()
        .filter(|p| p.is_dir())
        .collect()
}

pub fn remember(root: &Path) {
    let Some(file) = store() else {
        return;
    };
    let mut list = load();
    list.retain(|p| p != root);
    list.insert(0, root.to_path_buf());
    list.truncate(MAX);
    if let Some(dir) = file.parent() {
        let _ = std::fs::create_dir_all(dir);
    }
    if let Ok(json) = serde_json::to_string_pretty(&list) {
        let _ = std::fs::write(file, json);
    }
}
