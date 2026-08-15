#!/usr/bin/env bash
# PreToolUse hook (Bash matcher, if: "Bash(git *)"): before a `git commit`,
# verify fmt and clippy so the forgot-fmt → fix → recommit loop never starts.
# Exit 2 blocks the commit; stderr is fed back to Claude.
set -u
cmd=$(jq -r '.tool_input.command // empty' 2>/dev/null) || exit 0
grep -qE '(^|[;&|[:space:]])git([[:space:]]+-C[[:space:]]+[^[:space:]]+)?[[:space:]]+commit' <<<"$cmd" || exit 0
cd "${CLAUDE_PROJECT_DIR:-$(dirname "$0")/../..}" || exit 0
if ! out=$(cargo +nightly fmt --all -- --check 2>&1); then
  { echo "commit blocked: cargo fmt --check failed — run 'make fmt', re-stage, retry."
    printf '%s\n' "$out" | head -20; } >&2
  exit 2
fi
if ! out=$(cargo clippy --workspace --all-targets --no-deps -- -D warnings 2>&1); then
  { echo "commit blocked: clippy failed — fix the warnings, then retry."
    printf '%s\n' "$out" | tail -30; } >&2
  exit 2
fi
exit 0
