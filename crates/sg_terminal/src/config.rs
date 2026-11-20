//! The user's Ghostty configuration, as far as the embedded terminal follows
//! it: the font (`font-family`, `font-size`). Colors stay the app's.
//!
//! Ghostty reads `$XDG_CONFIG_HOME/ghostty/config` (`~/.config/…`), then on
//! macOS `~/Library/Application Support/com.mitchellh.ghostty/config`, each
//! also as `config.ghostty`; later files override earlier ones, and
//! `config-file = [?]path` includes another file after the current one.

use std::collections::HashSet;
use std::path::{Path, PathBuf};

/// Includes nested deeper than this are ignored (a cycle, most likely).
const MAX_DEPTH: usize = 8;

#[derive(Clone, Debug, Default, PartialEq)]
pub struct FontConfig {
    /// `font-family` entries in order: the first is the primary font, the
    /// rest are Ghostty's fallbacks. An empty value clears the list.
    pub families: Vec<String>,
    /// `font-size` in points.
    pub size: Option<f32>,
}

/// The font settings of the user's Ghostty configuration files (none:
/// Ghostty's defaults, which the caller replaces with its own).
pub fn font() -> FontConfig {
    let mut config = FontConfig::default();
    let mut seen = HashSet::new();
    for path in config_files() {
        load(&path, &mut config, &mut seen, 0);
    }
    config
}

/// Ghostty's own configuration files, in the order it loads them.
fn config_files() -> Vec<PathBuf> {
    let home = std::env::var_os("HOME").map(PathBuf::from);
    let xdg = std::env::var_os("XDG_CONFIG_HOME")
        .map(PathBuf::from)
        .filter(|dir| dir.is_absolute())
        .or_else(|| home.as_ref().map(|home| home.join(".config")));
    let mut dirs: Vec<PathBuf> = xdg.map(|dir| dir.join("ghostty")).into_iter().collect();
    if cfg!(target_os = "macos")
        && let Some(home) = &home
    {
        dirs.push(home.join("Library/Application Support/com.mitchellh.ghostty"));
    }
    dirs.into_iter()
        .flat_map(|dir| [dir.join("config"), dir.join("config.ghostty")])
        .collect()
}

fn load(path: &Path, config: &mut FontConfig, seen: &mut HashSet<PathBuf>, depth: usize) {
    if depth > MAX_DEPTH {
        return;
    }
    let canonical = path.canonicalize().unwrap_or_else(|_| path.to_path_buf());
    if !seen.insert(canonical) {
        return;
    }
    let Ok(text) = std::fs::read_to_string(path) else {
        return;
    };
    let includes = apply(&text, config);
    let base = path.parent().unwrap_or(Path::new("."));
    for include in includes {
        let include = expand_home(&include);
        let include = if include.is_absolute() {
            include
        } else {
            base.join(include)
        };
        load(&include, config, seen, depth + 1);
    }
}

/// Apply one file's lines; returns the files it includes (`config-file`),
/// which Ghostty loads after the file itself.
fn apply(text: &str, config: &mut FontConfig) -> Vec<String> {
    let mut includes = Vec::new();
    for line in text.lines() {
        let line = line.trim();
        if line.is_empty() || line.starts_with('#') {
            continue;
        }
        let Some((key, value)) = line.split_once('=') else {
            continue;
        };
        let value = unquote(value.trim());
        match key.trim() {
            "font-family" => {
                if value.is_empty() {
                    config.families.clear();
                } else {
                    config.families.push(value.to_string());
                }
            }
            "font-size" => {
                if value.is_empty() {
                    config.size = None;
                } else if let Ok(size) = value.parse::<f32>()
                    && size.is_finite()
                    && size > 0.
                {
                    config.size = Some(size);
                }
            }
            "config-file" => {
                // `?path`: optional; a missing file is skipped either way.
                let path = value.strip_prefix('?').unwrap_or(value);
                if !path.is_empty() {
                    includes.push(unquote(path).to_string());
                }
            }
            _ => {}
        }
    }
    includes
}

fn unquote(value: &str) -> &str {
    value
        .strip_prefix('"')
        .and_then(|rest| rest.strip_suffix('"'))
        .unwrap_or(value)
}

fn expand_home(path: &str) -> PathBuf {
    match (path.strip_prefix("~/"), std::env::var_os("HOME")) {
        (Some(rest), Some(home)) => PathBuf::from(home).join(rest),
        _ => PathBuf::from(path),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn parse(text: &str) -> FontConfig {
        let mut config = FontConfig::default();
        apply(text, &mut config);
        config
    }

    #[test]
    fn reads_the_font_and_ignores_comments() {
        let config = parse(
            "# font-family = Iosevka\n\
             theme = iTerm2 Solarized Light\n\
             font-family = Iosevka Term\n\
             font-size = 14.5\n",
        );
        assert_eq!(config.families, ["Iosevka Term"]);
        assert_eq!(config.size, Some(14.5));
    }

    #[test]
    fn families_accumulate_and_an_empty_value_resets_them() {
        let config = parse(
            "font-family = \"JetBrains Mono\"\n\
             font-family = Symbols Nerd Font\n\
             font-family = \"\"\n\
             font-family=Menlo\n\
             font-family = Apple Color Emoji\n",
        );
        assert_eq!(config.families, ["Menlo", "Apple Color Emoji"]);
    }

    #[test]
    fn bad_sizes_are_ignored() {
        assert_eq!(parse("font-size = big").size, None);
        assert_eq!(parse("font-size = 0").size, None);
        assert_eq!(parse("font-size = 13\nfont-size = -2").size, Some(13.));
    }

    #[test]
    fn includes_load_after_the_file_relative_to_it() {
        let dir = std::env::temp_dir().join(format!("sg-ghostty-config-{}", std::process::id()));
        std::fs::create_dir_all(dir.join("sub")).expect("temp dir");
        std::fs::write(
            dir.join("config"),
            "config-file = ?sub/fonts\nfont-family = Menlo\nfont-size = 12\n\
             config-file = ?missing\nconfig-file = config\n",
        )
        .expect("write config");
        std::fs::write(
            dir.join("sub/fonts"),
            "font-family = \"\"\nfont-family = Iosevka Term\n",
        )
        .expect("write include");
        let mut config = FontConfig::default();
        load(&dir.join("config"), &mut config, &mut HashSet::new(), 0);
        let _ = std::fs::remove_dir_all(&dir);
        // The include runs after the file: its reset drops Menlo.
        assert_eq!(config.families, ["Iosevka Term"]);
        assert_eq!(config.size, Some(12.));
    }
}
