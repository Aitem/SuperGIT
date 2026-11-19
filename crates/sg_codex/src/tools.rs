//! Presentation helpers for Codex items shown as tool calls: names, one-line
//! summaries, outputs and unified diffs. A tool call's `input` is the latest
//! `ThreadItem` Codex sent for it.

use std::path::Path;

use serde_json::Value;
use sg_agents::chat::{PermissionRequest, ToolCall, ToolResult};

/// `ThreadItem` types shown as tool rows.
pub fn is_tool(item_type: &str) -> bool {
    matches!(
        item_type,
        "commandExecution"
            | "fileChange"
            | "mcpToolCall"
            | "dynamicToolCall"
            | "collabAgentToolCall"
            | "webSearch"
            | "imageView"
            | "imageGeneration"
    )
}

fn str_field<'a>(v: &'a Value, key: &str) -> Option<&'a str> {
    v.get(key).and_then(Value::as_str)
}

/// The bold label of a tool row.
pub fn name(item: &Value) -> String {
    match str_field(item, "type").unwrap_or_default() {
        "commandExecution" => "Shell".into(),
        "fileChange" => "Edit".into(),
        "mcpToolCall" => format!(
            "{}.{}",
            str_field(item, "server").unwrap_or("mcp"),
            str_field(item, "tool").unwrap_or("tool")
        ),
        "dynamicToolCall" => str_field(item, "tool").unwrap_or("Tool").to_string(),
        "collabAgentToolCall" => "Agent".into(),
        "webSearch" => "Search".into(),
        "imageView" => "View image".into(),
        "imageGeneration" => "Image".into(),
        other => other.to_string(),
    }
}

/// The result of a completed item.
pub fn result(item: &Value) -> ToolResult {
    let status = str_field(item, "status").unwrap_or("completed");
    let failed = matches!(status, "failed" | "declined");
    let text = match str_field(item, "type").unwrap_or_default() {
        "commandExecution" => {
            let output = str_field(item, "aggregatedOutput").unwrap_or_default();
            match (status, item.get("exitCode").and_then(Value::as_i64)) {
                ("declined", _) => "Declined".to_string(),
                (_, Some(code)) if code != 0 && output.trim().is_empty() => {
                    format!("exit code {code}")
                }
                _ => output.to_string(),
            }
        }
        "mcpToolCall" => match item.pointer("/error/message").and_then(Value::as_str) {
            Some(message) => message.to_string(),
            None => item
                .pointer("/result/content")
                .and_then(Value::as_array)
                .into_iter()
                .flatten()
                .filter_map(|block| str_field(block, "text"))
                .collect::<Vec<_>>()
                .join("\n"),
        },
        "dynamicToolCall" => item
            .get("contentItems")
            .and_then(Value::as_array)
            .into_iter()
            .flatten()
            .filter_map(|block| str_field(block, "text"))
            .collect::<Vec<_>>()
            .join("\n"),
        _ => status.to_string(),
    };
    let is_error = failed
        || item
            .get("exitCode")
            .and_then(Value::as_i64)
            .is_some_and(|code| code != 0)
        || item.get("error").is_some_and(|e| !e.is_null())
        || item.get("success").and_then(Value::as_bool) == Some(false);
    // The item (with its whole output) stays in the call's `input`.
    ToolResult {
        text,
        is_error,
        meta: None,
    }
}

/// `file:///x` → `/x` (some Codex paths arrive as URLs).
pub fn file_url_path(path: &str) -> String {
    path.strip_prefix("file://").unwrap_or(path).to_string()
}

/// Shorten absolute paths under `cwd` to relative ones.
pub fn short_path(path: &str, cwd: Option<&Path>) -> String {
    let path = file_url_path(path);
    if let Some(cwd) = cwd
        && let Ok(rel) = Path::new(&path).strip_prefix(cwd)
    {
        let rel = rel.to_string_lossy();
        if !rel.is_empty() {
            return rel.into_owned();
        }
    }
    path
}

fn first_line(s: &str, max: usize) -> String {
    let line = s.lines().next().unwrap_or_default().trim();
    let mut out: String = line.chars().take(max).collect();
    if line.chars().count() > max || s.lines().nth(1).is_some() {
        out.push('…');
    }
    out
}

/// What the user would have typed: `/bin/zsh -lc 'cat a.txt'` → `cat a.txt`.
pub fn display_command(command: &str) -> String {
    for shell in ["bash", "zsh", "sh"] {
        for flag in ["-lc", "-c"] {
            let Some(at) = command.find(&format!("{shell} {flag} ")) else {
                continue;
            };
            let head = &command[..at];
            // Only a leading shell path (`/bin/zsh`), not a command that
            // happens to mention one.
            if head.contains(' ') || !(head.is_empty() || head.ends_with('/')) {
                continue;
            }
            let script = command[at + shell.len() + flag.len() + 2..].trim();
            if let Some(inner) = script.strip_prefix('\'').and_then(|s| s.strip_suffix('\'')) {
                return inner.replace("'\\''", "'");
            }
            if let Some(inner) = script.strip_prefix('"').and_then(|s| s.strip_suffix('"')) {
                return inner.replace("\\\"", "\"");
            }
            return script.to_string();
        }
    }
    command.to_string()
}

/// The text after the tool name on a collapsed row.
pub fn summary(tool: &ToolCall, cwd: Option<&Path>) -> String {
    let item = &tool.input;
    match str_field(item, "type").unwrap_or_default() {
        "commandExecution" => first_line(
            &display_command(str_field(item, "command").unwrap_or_default()),
            120,
        ),
        "fileChange" => item
            .get("changes")
            .and_then(Value::as_array)
            .into_iter()
            .flatten()
            .filter_map(|change| str_field(change, "path"))
            .map(|path| short_path(path, cwd))
            .collect::<Vec<_>>()
            .join(", "),
        "webSearch" => str_field(item, "query").unwrap_or_default().to_string(),
        "imageView" => short_path(str_field(item, "path").unwrap_or_default(), cwd),
        "collabAgentToolCall" => {
            first_line(str_field(item, "prompt").unwrap_or_default(), 100).to_string()
        }
        _ => item
            .get("arguments")
            .and_then(Value::as_object)
            .and_then(|args| args.values().find_map(Value::as_str))
            .map(|s| first_line(s, 100))
            .unwrap_or_default(),
    }
}

/// Edits get an inline diff once applied.
pub fn is_edit(tool: &ToolCall) -> bool {
    str_field(&tool.input, "type") == Some("fileChange")
}

/// A unified diff for a file change item (or the `changes` of an approval).
pub fn changes_patch(changes: &Value, cwd: Option<&Path>) -> Option<String> {
    let patch: String = changes
        .as_array()?
        .iter()
        .filter_map(|change| change_patch(change, cwd))
        .collect();
    (!patch.is_empty()).then_some(patch)
}

pub fn edit_patch(tool: &ToolCall, cwd: Option<&Path>) -> Option<String> {
    changes_patch(tool.input.get("changes")?, cwd)
}

/// The diff shown on an approval card, if it is about edits.
pub fn permission_patch(request: &PermissionRequest, cwd: Option<&Path>) -> Option<String> {
    (request.tool_name == "Edit")
        .then(|| changes_patch(request.input.get("changes")?, cwd))
        .flatten()
}

/// The text shown on an approval card that is not about edits.
pub fn permission_text(request: &PermissionRequest) -> Option<String> {
    match request.tool_name.as_str() {
        "Shell" => request
            .input
            .get("command")
            .and_then(Value::as_str)
            .map(display_command),
        "Edit" => None,
        _ => serde_json::to_string_pretty(&request.input).ok(),
    }
}

/// One file of a `FileUpdateChange`: update diffs arrive as bare hunks,
/// added and deleted files as their content.
fn change_patch(change: &Value, cwd: Option<&Path>) -> Option<String> {
    let path = short_path(str_field(change, "path")?, cwd);
    let diff = str_field(change, "diff").unwrap_or_default();
    if diff.starts_with("diff --git") {
        return Some(with_newline(diff));
    }
    let kind = change.pointer("/kind/type").and_then(Value::as_str);
    let has_hunks = diff.lines().any(|l| l.starts_with("@@"));
    let content = |sign: char| -> String {
        diff.lines()
            .map(|l| format!("{sign}{l}\n"))
            .collect::<String>()
    };
    let count = diff.lines().count();
    let patch = match kind {
        Some("add") if !has_hunks => format!(
            "diff --git a/{path} b/{path}\nnew file mode 100644\n--- /dev/null\n+++ b/{path}\n@@ -0,0 +1,{count} @@\n{}",
            content('+')
        ),
        Some("delete") if !has_hunks => format!(
            "diff --git a/{path} b/{path}\ndeleted file mode 100644\n--- a/{path}\n+++ /dev/null\n@@ -1,{count} +0,0 @@\n{}",
            content('-')
        ),
        _ => {
            let to = change
                .pointer("/kind/move_path")
                .and_then(Value::as_str)
                .map(|p| short_path(p, cwd))
                .unwrap_or_else(|| path.clone());
            if !has_hunks {
                return None;
            }
            format!(
                "diff --git a/{path} b/{to}\n--- a/{path}\n+++ b/{to}\n{}",
                with_newline(diff)
            )
        }
    };
    Some(patch)
}

fn with_newline(s: &str) -> String {
    if s.ends_with('\n') {
        s.to_string()
    } else {
        format!("{s}\n")
    }
}

#[cfg(test)]
mod tests {
    use serde_json::json;

    use super::*;

    #[test]
    fn unwraps_shell_commands() {
        assert_eq!(display_command("/bin/zsh -lc 'cat a.txt'"), "cat a.txt");
        assert_eq!(
            display_command("/bin/bash -lc 'echo '\\''hi'\\'''"),
            "echo 'hi'"
        );
        assert_eq!(display_command("bash -c \"ls -la\""), "ls -la");
        assert_eq!(display_command("git status"), "git status");
        assert_eq!(
            display_command("echo /bin/zsh -lc 'x'"),
            "echo /bin/zsh -lc 'x'"
        );
    }

    #[test]
    fn builds_patches_for_added_and_updated_files() {
        let changes = json!([
            {"path": "/r/a.txt", "kind": {"type": "add"}, "diff": "hello\nworld\n"},
            {"path": "/r/src/b.rs", "kind": {"type": "update", "move_path": null},
             "diff": "@@ -1,2 +1,2 @@\n fn main() {\n-    old();\n+    new();\n"},
            {"path": "/r/c.txt", "kind": {"type": "update", "move_path": "/r/d.txt"},
             "diff": "@@ -1 +1 @@\n-x\n+y"},
        ]);
        let patch = changes_patch(&changes, Some(Path::new("/r"))).unwrap();
        assert!(patch.contains("--- /dev/null\n+++ b/a.txt\n@@ -0,0 +1,2 @@\n+hello\n+world\n"));
        assert!(patch.contains("--- a/src/b.rs\n+++ b/src/b.rs\n@@ -1,2 +1,2 @@\n"));
        assert!(patch.contains("diff --git a/c.txt b/d.txt\n"));
        assert!(patch.ends_with("+y\n"));
    }

    #[test]
    fn summarizes_items() {
        let tool = |input: Value| ToolCall {
            id: "i".into(),
            name: name(&input),
            input,
            result: None,
            parent: None,
            checking: false,
        };
        let cwd = Some(Path::new("/r"));
        let cmd = tool(json!({"type": "commandExecution", "command": "/bin/zsh -lc 'cargo test'"}));
        assert_eq!(cmd.name, "Shell");
        assert_eq!(summary(&cmd, cwd), "cargo test");
        let edit = tool(json!({"type": "fileChange", "changes": [
            {"path": "/r/a.rs", "kind": {"type": "add"}, "diff": ""},
            {"path": "/elsewhere/b.rs", "kind": {"type": "add"}, "diff": ""}]}));
        assert_eq!(summary(&edit, cwd), "a.rs, /elsewhere/b.rs");
        let mcp = tool(
            json!({"type": "mcpToolCall", "server": "gh", "tool": "search", "arguments": {"q": "bug"}}),
        );
        assert_eq!(mcp.name, "gh.search");
        assert_eq!(summary(&mcp, cwd), "bug");
    }

    #[test]
    fn results_flag_failures() {
        let ok = result(
            &json!({"type": "commandExecution", "status": "completed", "aggregatedOutput": "hi\n", "exitCode": 0}),
        );
        assert!(!ok.is_error);
        assert_eq!(ok.text, "hi\n");
        let failed = result(
            &json!({"type": "commandExecution", "status": "failed", "aggregatedOutput": "", "exitCode": 2}),
        );
        assert!(failed.is_error);
        assert_eq!(failed.text, "exit code 2");
        let declined = result(&json!({"type": "fileChange", "status": "declined", "changes": []}));
        assert!(declined.is_error);
        let mcp = result(
            &json!({"type": "mcpToolCall", "status": "failed", "error": {"message": "boom"}, "result": null}),
        );
        assert_eq!((mcp.text.as_str(), mcp.is_error), ("boom", true));
    }
}
