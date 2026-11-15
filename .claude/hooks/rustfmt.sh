#!/bin/bash
# PostToolUse(Edit|MultiEdit|Write): rustfmt the Rust file Claude just changed,
# so CI's `cargo fmt --check` never fails on an agent edit.
file=$(jq -r '.tool_input.file_path // empty')
case "$file" in
  *.rs) [[ -f "$file" ]] && rustfmt --edition 2024 "$file" >/dev/null 2>&1 ;;
esac
exit 0
