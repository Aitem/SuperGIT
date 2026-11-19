#!/bin/bash
# PostToolUse: rustfmt the Rust code Claude changed, so CI's
# `cargo fmt --check` never fails on an agent edit.
#  - Edit|MultiEdit|Write: the file it wrote.
#  - Bash: scripts, sed and heredocs edit files too, so every Rust file that
#    differs from HEAD, and every new one. Nothing changed: one `git diff`.
input=$(cat)
format() {
  [[ -f "$1" ]] && rustfmt --edition 2024 "$1" >/dev/null 2>&1
}
if [[ $(jq -r '.tool_name // empty' <<<"$input") == "Bash" ]]; then
  cd "${CLAUDE_PROJECT_DIR:-.}" 2>/dev/null || exit 0
  git rev-parse --is-inside-work-tree >/dev/null 2>&1 || exit 0
  while IFS= read -r -d '' file; do
    format "$file"
  done < <(
    git diff --name-only -z --diff-filter=ACMR HEAD -- '*.rs' 2>/dev/null
    git ls-files -z --others --exclude-standard -- '*.rs' 2>/dev/null
  )
else
  file=$(jq -r '.tool_input.file_path // empty' <<<"$input")
  case "$file" in
    *.rs) format "$file" ;;
  esac
fi
exit 0
