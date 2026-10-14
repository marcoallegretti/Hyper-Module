#!/usr/bin/env bash
# Nothing stops with stubs added, Rust unformatted or clippy unhappy.
# Skipped when nothing relevant changed since the last green run.
set -uo pipefail
payload=$(cat)
[ "$(printf '%s' "$payload" | jq -r '.stop_hook_active // false')" = true ] && exit 0
cd "$(printf '%s' "$payload" | jq -r '.cwd // "."')" 2>/dev/null || exit 0
git rev-parse --git-dir >/dev/null 2>&1 || exit 0

[ -n "$(git status --porcelain -- '*.rs' '*.slint' Cargo.toml '*/Cargo.toml' 2>/dev/null)" ] || exit 0
marker="$(git rev-parse --git-dir)/verify-ok"
fingerprint=$( (git diff HEAD -- '*.rs' '*.slint' '*Cargo.toml'; git ls-files --others --exclude-standard -- '*.rs' '*.slint' | xargs -r cat) | sha1sum | cut -d' ' -f1)
[ -f "$marker" ] && [ "$(cat "$marker")" = "$fingerprint" ] && exit 0

if ! out=$("$(dirname "$0")/../scripts/lint-diff.sh" 2>&1); then
  echo "The change adds what AGENTS.md forbids:" >&2; echo "$out" | head -n 30 >&2; exit 2
fi
if ! out=$(cargo fmt --all -- --check 2>&1); then
  echo "cargo fmt --all is not clean:" >&2; echo "$out" | head -n 30 >&2; exit 2
fi
if ! out=$(cargo clippy --workspace --all-targets --all-features --locked -- -D warnings 2>&1); then
  if grep -qE 'pkg-config|could not find system library|The system library' <<< "$out"; then
    echo "clippy could not build here (system libraries); run it where they exist." >&2
    printf '%s' "$fingerprint" > "$marker"; exit 0
  fi
  echo "clippy denies warnings in CI:" >&2
  echo "$out" | grep -E '^(error|warning)' -A 6 | head -n 80 >&2; exit 2
fi
printf '%s' "$fingerprint" > "$marker"
exit 0
