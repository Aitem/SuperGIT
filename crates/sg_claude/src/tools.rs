//! Presentation helpers for Claude Code tool calls: one-line summaries and
//! unified diffs for file edits.

use std::path::Path;

use serde_json::Value;

/// The tool Claude leaves plan mode with: its input is the plan.
pub const PLAN: &str = "ExitPlanMode";

fn str_field<'a>(input: &'a Value, key: &str) -> Option<&'a str> {
    input.get(key).and_then(Value::as_str)
}

/// Shorten absolute paths under `cwd` to relative ones.
pub fn short_path(path: &str, cwd: Option<&Path>) -> String {
    if let Some(cwd) = cwd
        && let Ok(rel) = Path::new(path).strip_prefix(cwd)
    {
        let rel = rel.to_string_lossy();
        if !rel.is_empty() {
            return rel.into_owned();
        }
    }
    path.to_string()
}

fn first_line(s: &str, max: usize) -> String {
    let line = s.lines().next().unwrap_or_default().trim();
    let mut out: String = line.chars().take(max).collect();
    if line.chars().count() > max || s.lines().nth(1).is_some() {
        out.push('…');
    }
    out
}

/// "Read src/main.rs", "Bash Run the tests", "Grep TODO" — the bit after the
/// tool name shown on a collapsed tool row. A command reads as what Claude
/// says it is for (`description`); [`detail`] has the command itself.
pub fn summary(name: &str, input: &Value, cwd: Option<&Path>) -> String {
    let path = |key| str_field(input, key).map(|p| short_path(p, cwd));
    let text = match name {
        "Read" | "Write" | "Edit" | "MultiEdit" => path("file_path"),
        "NotebookEdit" => path("notebook_path"),
        "Bash" => str_field(input, "description")
            .map(str::trim)
            .filter(|d| !d.is_empty())
            .map(|d| first_line(d, 120))
            .or_else(|| str_field(input, "command").map(|c| first_line(c, 120))),
        PLAN => str_field(input, "plan").map(plan_title),
        "Grep" => str_field(input, "pattern").map(|p| match path("path") {
            Some(where_) => format!("{p}  in {where_}"),
            None => p.to_string(),
        }),
        "Glob" => str_field(input, "pattern").map(String::from),
        "WebFetch" => str_field(input, "url").map(String::from),
        "WebSearch" => str_field(input, "query").map(String::from),
        "Task" | "Agent" => str_field(input, "description").map(String::from),
        "TodoWrite" => input
            .get("todos")
            .and_then(Value::as_array)
            .map(|t| format!("{} todos", t.len())),
        _ => None,
    };
    text.or_else(|| {
        // Unknown tools (MCP…): the first string argument.
        input
            .as_object()
            .and_then(|o| o.values().find_map(Value::as_str))
            .map(|s| first_line(s, 100))
    })
    .unwrap_or_default()
}

/// What an expanded tool row shows above the output: the whole command.
pub fn detail(name: &str, input: &Value) -> Option<String> {
    match name {
        "Bash" => str_field(input, "command")
            .map(str::trim)
            .filter(|c| !c.is_empty())
            .map(String::from),
        _ => None,
    }
}

/// The name a tool row shows: MCP tools without their wire prefix
/// ("mcp__github__create_issue" → "github · create_issue"), a plan as "Plan".
pub fn display_name(name: &str) -> String {
    if let Some(rest) = name.strip_prefix("mcp__")
        && let Some((server, tool)) = rest.split_once("__")
        && !server.is_empty()
        && !tool.is_empty()
    {
        return format!("{server} · {tool}");
    }
    match name {
        PLAN => "Plan".into(),
        "Task" => "Agent".into(),
        _ => name.to_string(),
    }
}

/// Calls that are a step of their own, never folded into a run of calls: a
/// subagent (its own calls follow it) and a plan.
pub fn stands_alone(name: &str) -> bool {
    matches!(name, "Agent" | "Task" | PLAN)
}

/// A plan's first heading or line, without Markdown marks.
fn plan_title(plan: &str) -> String {
    let line = plan
        .lines()
        .map(|line| line.trim().trim_start_matches('#').trim())
        .find(|line| !line.is_empty())
        .unwrap_or_default();
    first_line(line, 100)
}

/// Output worth reading from its end: a build's or a test run's last lines
/// say how it went.
pub fn reads_from_end(name: &str) -> bool {
    name == "Bash"
}

/// Whether the tool changes files (and so deserves an inline diff).
pub fn is_edit(name: &str) -> bool {
    matches!(name, "Edit" | "MultiEdit" | "Write")
}

/// A unified diff for an edit tool call. Prefers the CLI's
/// `structuredPatch` (real line numbers) from the tool result; falls back to
/// one synthetic hunk built from the input.
pub fn edit_patch(
    name: &str,
    input: &Value,
    result_meta: Option<&Value>,
    cwd: Option<&Path>,
) -> Option<String> {
    let file = str_field(input, "file_path")?;
    let rel = short_path(file, cwd);
    let header = format!("diff --git a/{rel} b/{rel}\n--- a/{rel}\n+++ b/{rel}\n");

    if let Some(hunks) = result_meta
        .and_then(|m| m.get("structuredPatch"))
        .and_then(Value::as_array)
        .filter(|h| !h.is_empty())
    {
        let mut patch = header;
        for h in hunks {
            let n = |k| h.get(k).and_then(Value::as_u64).unwrap_or(0);
            patch.push_str(&format!(
                "@@ -{},{} +{},{} @@\n",
                n("oldStart"),
                n("oldLines"),
                n("newStart"),
                n("newLines")
            ));
            for line in h
                .get("lines")
                .and_then(Value::as_array)
                .into_iter()
                .flatten()
            {
                patch.push_str(line.as_str().unwrap_or_default());
                patch.push('\n');
            }
        }
        return Some(patch);
    }

    match name {
        "Write" => {
            let content = str_field(input, "content")?;
            let lines: Vec<&str> = content.lines().collect();
            let mut patch = format!(
                "diff --git a/{rel} b/{rel}\nnew file mode 100644\n--- /dev/null\n+++ b/{rel}\n@@ -0,0 +1,{} @@\n",
                lines.len()
            );
            for l in lines {
                patch.push('+');
                patch.push_str(l);
                patch.push('\n');
            }
            Some(patch)
        }
        "Edit" => Some(
            header
                + &hunk(
                    str_field(input, "old_string")?,
                    str_field(input, "new_string")?,
                ),
        ),
        "MultiEdit" => {
            let mut patch = header;
            for e in input.get("edits")?.as_array()? {
                patch.push_str(&hunk(
                    str_field(e, "old_string")?,
                    str_field(e, "new_string")?,
                ));
            }
            Some(patch)
        }
        _ => None,
    }
}

fn hunk(old: &str, new: &str) -> String {
    let old: Vec<&str> = old.lines().collect();
    let new: Vec<&str> = new.lines().collect();
    let mut out = format!("@@ -1,{} +1,{} @@\n", old.len(), new.len());
    for l in old {
        out.push('-');
        out.push_str(l);
        out.push('\n');
    }
    for l in new {
        out.push('+');
        out.push_str(l);
        out.push('\n');
    }
    out
}

/// Plain text of a `tool_result` content (a string or text blocks).
pub fn result_text(content: &Value) -> String {
    match content {
        Value::String(s) => unwrap_error(s).to_string(),
        Value::Array(blocks) => blocks
            .iter()
            .filter_map(|b| match b.get("type").and_then(Value::as_str) {
                Some("text") => b.get("text").and_then(Value::as_str).map(String::from),
                Some("image") => Some("[image]".into()),
                _ => None,
            })
            .collect::<Vec<_>>()
            .join("\n"),
        Value::Null => String::new(),
        other => other.to_string(),
    }
}

/// The CLI wraps its own refusals in `<tool_use_error>`: keep the message.
fn unwrap_error(text: &str) -> &str {
    text.trim()
        .strip_prefix("<tool_use_error>")
        .and_then(|rest| rest.strip_suffix("</tool_use_error>"))
        .map_or(text, str::trim)
}

#[cfg(test)]
mod tests {
    use serde_json::json;

    use super::*;

    #[test]
    fn summaries() {
        let cwd = Path::new("/repo");
        assert_eq!(
            summary("Read", &json!({"file_path": "/repo/src/a.rs"}), Some(cwd)),
            "src/a.rs"
        );
        assert_eq!(
            summary("Bash", &json!({"command": "cargo test\necho hi"}), None),
            "cargo test…"
        );
        // What the command is for, when Claude says; the command on demand.
        let bash = json!({"command": "cargo test -p sg_git 2>&1 | tail -5", "description": "Run the git tests"});
        assert_eq!(summary("Bash", &bash, None), "Run the git tests");
        assert_eq!(
            detail("Bash", &bash).as_deref(),
            Some("cargo test -p sg_git 2>&1 | tail -5")
        );
        assert_eq!(detail("Read", &json!({"file_path": "/a"})), None);
        assert_eq!(
            summary(
                "ExitPlanMode",
                &json!({"plan": "\n# Add a README\n\n1. Write it"}),
                None
            ),
            "Add a README"
        );
        assert_eq!(
            summary(
                "Grep",
                &json!({"pattern": "TODO", "path": "/repo/src"}),
                Some(cwd)
            ),
            "TODO  in src"
        );
        assert_eq!(
            summary("mcp__x__do", &json!({"q": "find it"}), None),
            "find it"
        );
        assert_eq!(
            summary("TodoWrite", &json!({"todos": [1, 2]}), None),
            "2 todos"
        );
    }

    #[test]
    fn edit_patch_prefers_structured_patch() {
        let input =
            json!({"file_path": "/repo/hello.txt", "old_string": "hi", "new_string": "hello"});
        let meta = json!({"structuredPatch": [{"oldStart": 3, "oldLines": 1, "newStart": 3, "newLines": 1, "lines": ["-hi", "+hello"]}]});
        let p = edit_patch("Edit", &input, Some(&meta), Some(Path::new("/repo"))).unwrap();
        assert!(p.contains("+++ b/hello.txt"));
        assert!(p.contains("@@ -3,1 +3,1 @@\n-hi\n+hello\n"));
        let synthetic = edit_patch("Edit", &input, None, None).unwrap();
        assert!(synthetic.contains("@@ -1,1 +1,1 @@\n-hi\n+hello\n"));
        let write = edit_patch(
            "Write",
            &json!({"file_path": "/x/n.txt", "content": "a\nb\n"}),
            None,
            None,
        )
        .unwrap();
        assert!(write.contains("new file mode") && write.contains("+a\n+b\n"));
    }

    #[test]
    fn tool_names_read_without_wire_prefixes() {
        assert_eq!(
            display_name("mcp__chrome-devtools__take_screenshot"),
            "chrome-devtools · take_screenshot"
        );
        assert_eq!(display_name("mcp__odd"), "mcp__odd");
        assert_eq!(display_name("ExitPlanMode"), "Plan");
        assert_eq!(display_name("Task"), "Agent");
        assert_eq!(display_name("Bash"), "Bash");
    }

    #[test]
    fn result_text_from_blocks() {
        assert_eq!(result_text(&json!("ok")), "ok");
        assert_eq!(
            result_text(&json!("<tool_use_error>Blocked: sleep 30</tool_use_error>")),
            "Blocked: sleep 30"
        );
        assert_eq!(
            result_text(&json!([{"type": "text", "text": "a"}, {"type": "text", "text": "b"}])),
            "a\nb"
        );
    }
}
