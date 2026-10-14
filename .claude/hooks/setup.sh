#!/usr/bin/env bash
# Idempotent: installs the commit-msg hook unless another one is already there.
set -uo pipefail
root=$(git rev-parse --show-toplevel 2>/dev/null) || exit 0
hooks=$(git -C "$root" rev-parse --path-format=absolute --git-path hooks)
src="$root/.claude/hooks/commit-msg.sh"
dest="$hooks/commit-msg"
[ -f "$src" ] || exit 0
if [ -e "$dest" ] && ! grep -q 'repo-commit-msg-hook' "$dest"; then
  echo "commit-msg hook already present at $dest; not replaced" >&2
  exit 0
fi
mkdir -p "$hooks"
cmp -s "$src" "$dest" || install -m 755 "$src" "$dest"
exit 0
