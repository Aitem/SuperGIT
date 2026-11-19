//! Syntax colors for whole files, as the theme's highlight names.
//!
//! The built-in diff gets its colors from gpui-kit's `syntect` highlighter;
//! the difftastic view lays out its own lines and asks here. Syntect's
//! TextMate scopes are mapped onto the names the theme defines (`keyword`,
//! `string`), as gpui-kit does, so both views color code the same way and
//! follow light/dark themes.

use std::collections::HashMap;
use std::ops::Range;
use std::sync::LazyLock;

use gpui_kit::component::diff::{DiffFile, DiffParseError};
use syntect::parsing::{ParseState, Scope, ScopeStack, SyntaxReference, SyntaxSet};

/// Deserializing the syntax set takes tens of milliseconds: once, on first use.
static SYNTAXES: LazyLock<SyntaxSet> = LazyLock::new(two_face::syntax::extra_newlines);

/// Lines longer than this (minified code) are left plain.
const MAX_LINE_BYTES: usize = 2000;

/// Highlight names over byte ranges of one line, ordered and non-overlapping.
pub type LineHighlights = Vec<(Range<usize>, &'static str)>;

/// Highlights for each of `lines`, a file at `path` split into lines, or
/// nothing when the language is unknown.
pub fn highlight_lines<S: AsRef<str>>(path: &str, lines: &[S]) -> Vec<LineHighlights> {
    let Some(syntax) = find_syntax(path, lines.first().map(AsRef::as_ref)) else {
        return Vec::new();
    };
    let syntaxes = &*SYNTAXES;
    let mut parser = ParseState::new(syntax);
    let mut scopes = ScopeStack::new();
    let mut names = HashMap::<Scope, Option<&'static str>>::new();
    let mut result = Vec::with_capacity(lines.len());
    let mut buffer = String::new();
    for line in lines {
        let line = line.as_ref();
        let mut highlights = LineHighlights::new();
        if line.len() > MAX_LINE_BYTES {
            result.push(highlights);
            continue;
        }
        buffer.clear();
        buffer.push_str(line);
        buffer.push('\n');
        // A line syntect cannot parse keeps the scopes it started with.
        if let Ok(operations) = parser.parse_line(&buffer, syntaxes) {
            let mut cursor = 0;
            let mut push = |range: Range<usize>, scopes: &ScopeStack| {
                let range = range.start.min(line.len())..range.end.min(line.len());
                if range.is_empty() {
                    return;
                }
                // The innermost scope that names something wins, so a
                // string's quotes color as the string.
                let name = scopes.scopes.iter().rev().find_map(|scope| {
                    *names.entry(*scope).or_insert_with(|| semantic_name(*scope))
                });
                if let Some(name) = name {
                    match highlights.last_mut() {
                        Some((last, last_name))
                            if last.end == range.start && *last_name == name =>
                        {
                            last.end = range.end;
                        }
                        _ => highlights.push((range, name)),
                    }
                }
            };
            for (index, operation) in operations {
                push(cursor..index, &scopes);
                let _ = scopes.apply(&operation);
                cursor = index;
            }
            push(cursor..buffer.len(), &scopes);
        }
        result.push(highlights);
    }
    result
}

/// Parses a patch for gpui-kit's diff, naming the language of files its own
/// detector does not know (it recognizes a fixed list of extensions, without
/// Clojure, Nix, Haskell…): the extension, or the file name, is a token the
/// syntect highlighter looks up itself.
pub fn parse_patch(patch: &str) -> Result<Vec<DiffFile>, DiffParseError> {
    Ok(DiffFile::parse(patch)?
        .into_iter()
        .map(|file| {
            if file.language() != "text" {
                return file;
            }
            let name = file.path().rsplit('/').next().unwrap_or_default();
            let token = name
                .rsplit_once('.')
                .map_or(name, |(_, extension)| extension)
                .to_string();
            if token.is_empty() {
                file
            } else {
                file.with_language(token)
            }
        })
        .collect())
}

/// By extension, then whole file name (`Makefile`, `Dockerfile`), then the
/// first line (a shebang).
fn find_syntax(path: &str, first_line: Option<&str>) -> Option<&'static SyntaxReference> {
    let syntaxes = &*SYNTAXES;
    let name = path.rsplit('/').next().unwrap_or(path);
    let extension = name.rsplit_once('.').map(|(_, extension)| extension);
    extension
        .and_then(|extension| {
            syntaxes
                .find_syntax_by_extension(extension)
                .or_else(|| syntaxes.find_syntax_by_extension(&extension.to_lowercase()))
        })
        .or_else(|| syntaxes.find_syntax_by_extension(name))
        .or_else(|| first_line.and_then(|line| syntaxes.find_syntax_by_first_line(line)))
        .filter(|syntax| syntax.name != "Plain Text")
}

/// The theme's highlight name for a TextMate scope, or `None` to let an
/// enclosing scope decide. Mirrors gpui-kit's syntect mapping.
fn semantic_name(scope: Scope) -> Option<&'static str> {
    let scope = scope.build_string();
    let is = |prefix: &str| {
        scope == prefix
            || scope
                .strip_prefix(prefix)
                .is_some_and(|rest| rest.starts_with('.'))
    };
    let name = if is("punctuation.definition.string")
        || is("punctuation.definition.comment")
        || is("invalid")
        || is("meta")
        || is("source")
        || is("text")
    {
        return None;
    } else if is("comment.block.documentation") || is("comment.line.documentation") {
        "comment.doc"
    } else if is("comment") {
        "comment"
    } else if is("constant.character.escape") {
        "string.escape"
    } else if is("string.regexp") {
        "string.regex"
    } else if is("string") {
        "string"
    } else if is("constant.numeric") {
        "number"
    } else if is("constant.language.boolean") {
        "boolean"
    } else if is("constant") || is("support.constant") || is("entity.name.constant") {
        "constant"
    } else if is("keyword.operator") {
        "operator"
    } else if is("keyword.control.import") || is("meta.preprocessor") {
        "preproc"
    } else if is("keyword") || is("storage.modifier") {
        "keyword"
    } else if is("storage.type") {
        // `fn`, `class` and `let` are declarations, `int` and `bool` types.
        if [
            "class",
            "enum",
            "function",
            "impl",
            "interface",
            "module",
            "namespace",
            "struct",
            "trait",
            "type",
            "let",
            "var",
            "const",
        ]
        .iter()
        .any(|word| scope.contains(word))
        {
            "keyword"
        } else {
            "type"
        }
    } else if is("entity.name.function")
        || is("support.function")
        || is("variable.function")
        || is("meta.function-call")
    {
        "function"
    } else if is("entity.name.type")
        || is("entity.name.class")
        || is("entity.name.struct")
        || is("entity.name.enum")
        || is("entity.name.trait")
        || is("entity.name.interface")
        || is("entity.other.inherited-class")
        || is("support.type")
        || is("support.class")
    {
        "type"
    } else if is("entity.name.tag") {
        "tag"
    } else if is("entity.other.attribute-name") {
        "attribute"
    } else if is("entity.name.label") {
        "label"
    } else if is("entity.name.section") || is("markup.heading") {
        "title"
    } else if is("variable.language") {
        "variable.special"
    } else if is("variable.other.member")
        || is("variable.other.property")
        || is("support.variable.property")
        || is("meta.mapping.key")
    {
        "property"
    } else if is("variable") {
        "variable"
    } else if is("markup.bold") {
        "emphasis.strong"
    } else if is("markup.italic") {
        "emphasis"
    } else if is("markup.underline.link") {
        "link_uri"
    } else if is("markup.raw") {
        "text.literal"
    } else if is("markup.list") {
        "punctuation.list_marker"
    } else if is("punctuation.section") || is("punctuation.definition.block") {
        "punctuation.bracket"
    } else if is("punctuation.separator") || is("punctuation.terminator") {
        "punctuation.delimiter"
    } else if is("punctuation") {
        "punctuation"
    } else {
        return None;
    };
    Some(name)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn names(path: &str, lines: &[&str]) -> Vec<Vec<(String, &'static str)>> {
        highlight_lines(path, lines)
            .into_iter()
            .zip(lines)
            .map(|(highlights, line)| {
                highlights
                    .into_iter()
                    .map(|(range, name)| (line[range].to_string(), name))
                    .collect()
            })
            .collect()
    }

    #[test]
    fn colors_keywords_strings_and_comments_across_lines() {
        let lines = names(
            "src/main.rs",
            &["fn main() {", "    /* a", "    b */ \"x\"", "}"],
        );
        assert!(lines[0].contains(&("fn".into(), "keyword")));
        assert!(lines[1].iter().any(|(_, name)| *name == "comment"));
        // The block comment carries over to the next line.
        assert!(
            lines[2]
                .iter()
                .any(|(text, name)| text.contains('b') && *name == "comment")
        );
        assert!(lines[2].contains(&("\"x\"".into(), "string")));
    }

    #[test]
    fn names_languages_the_diff_does_not_detect() {
        let patch = "diff --git a/src/core.clj b/src/core.clj\n--- a/src/core.clj\n+++ b/src/core.clj\n@@ -1 +1 @@\n-(def x 1)\n+(def x 2)\n\
                     diff --git a/main.rs b/main.rs\n--- a/main.rs\n+++ b/main.rs\n@@ -1 +1 @@\n-a\n+b\n";
        let files = parse_patch(patch).unwrap();
        assert_eq!(files[0].language().as_ref(), "clj");
        assert_eq!(files[1].language().as_ref(), "rust");
    }

    /// syntect compiles patterns on first use and panics when one fails; the
    /// syntaxes are two-face's fancy-regex set while the engine is Oniguruma
    /// (it takes precedence, for memory), so every pattern must compile.
    #[test]
    fn every_syntax_pattern_compiles_with_the_regex_engine() {
        use syntect::parsing::Regex;
        use syntect::parsing::syntax_definition::Pattern;
        let builder = two_face::syntax::extra_newlines().into_builder();
        let mut failed = Vec::new();
        let mut checked = 0;
        for syntax in builder.syntaxes() {
            for (name, context) in &syntax.contexts {
                for pattern in &context.patterns {
                    // Back-references are filled in from the match that
                    // opened the context before these compile.
                    let Pattern::Match(pattern) = pattern else {
                        continue;
                    };
                    if pattern.has_captures {
                        continue;
                    }
                    checked += 1;
                    if let Some(error) = Regex::try_compile(pattern.regex.regex_str()) {
                        failed.push(format!("{} / {name}: {error}", syntax.name));
                    }
                }
            }
        }
        assert!(checked > 10_000, "only {checked} patterns found");
        assert!(failed.is_empty(), "{failed:#?}");
    }

    #[test]
    fn finds_languages_by_name_and_shebang() {
        assert!(!highlight_lines("src/core.clj", &["(defn f [x] (+ x 1))"]).is_empty());
        assert!(!highlight_lines("Makefile", &["all:", "\techo hi"]).is_empty());
        assert!(!highlight_lines("bin/tool", &["#!/usr/bin/env python3", "x = 1"]).is_empty());
        assert!(highlight_lines("notes.unknown-ext", &["plain"]).is_empty());
    }
}
