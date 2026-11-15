#!/bin/bash
# Claude Code gate: CI's clippy (warnings are errors) must pass before Claude
# ends its turn (`stop`) or runs `git commit` (`commit`), whenever Rust sources
# or manifests differ from HEAD. Exit 2 hands the errors back to Claude.
# Opt out per shell or in .claude/settings.local.json: SUPERGIT_NO_RUST_GATE=1.
set -u
input=$(cat)
[[ -n ${SUPERGIT_NO_RUST_GATE:-} ]] && exit 0

case "${1:-}" in
  stop)
    # Claude is already fixing what this hook reported once: let it stop.
    [[ $(jq -r '.stop_hook_active // false' <<<"$input") == true ]] && exit 0 ;;
  commit)
    jq -r '.tool_input.command // empty' <<<"$input" |
      grep -Eq '(^|[;&|(]|[[:space:]])git( -C [^ ]+)? commit' || exit 0 ;;
  *) echo "usage: rust-gate.sh stop|commit" >&2; exit 1 ;;
esac

cd "${CLAUDE_PROJECT_DIR:-.}" || exit 0
[[ -n $(git status --porcelain -- '*.rs' '*Cargo.toml' Cargo.lock clippy.toml 2>/dev/null) ]] || exit 0

# CI's flags minus --all-features: the `screenshot` feature re-checks GPUI with
# test-support (slow, separate cache); CI still covers it.
out=$(cargo clippy --workspace --all-targets --locked --message-format=short -- -D warnings 2>&1) && exit 0
{
  echo "cargo clippy --workspace --all-targets -- -D warnings fails; fix this first:"
  grep -E '(^|: )error(\[|:)|: warning:' <<<"$out" | head -40
} >&2
exit 2
