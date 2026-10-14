#!/usr/bin/env bash
# Usage: worktree.sh <kind>/<slug> — new branch from origin/main in ../<repo>-<slug>.
set -euo pipefail
branch="${1:?usage: worktree.sh <kind>/<slug>}"
source "$(dirname "$0")/../hooks/lib.sh"
problem=$(branch_problem "$branch")
[ -z "$problem" ] || { echo "$problem" >&2; exit 1; }

main=$(dirname "$(git rev-parse --path-format=absolute --git-common-dir)")
git -C "$main" fetch origin main
path="$(dirname "$main")/$(basename "$main")-${branch#*/}"
git -C "$main" worktree add --no-track -b "$branch" "$path" origin/main
"$path/.claude/hooks/setup.sh"
echo "$path"
