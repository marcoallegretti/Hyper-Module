#!/usr/bin/env bash
# Catches the common spellings of: committing on or pushing to main, badly named
# branches, hook bypasses, forced adds, tool trailers in commit messages. Server
# side branch protection remains the real guard for main.
set -uo pipefail
command -v jq >/dev/null || { echo "jq is required by the repository hooks" >&2; exit 2; }
source "$(dirname "$0")/lib.sh"

payload=$(cat)
command=$(printf '%s' "$payload" | jq -r '.tool_input.command // ""')
session_cwd=$(printf '%s' "$payload" | jq -r '.cwd // "."')
dir="$session_cwd"

if [[ "$command" =~ git([[:space:]]+-[^[:space:]]+([[:space:]]+[^-][^[:space:]]*)?)*[[:space:]]+commit ]]; then
  problem=$(text_problem "$command"); [ -n "$problem" ] && deny "$problem"
fi

body=$(awk '
  skip { if ($0 == delim) skip = 0; next }
  { print }
  match($0, /<<-?[ ]*["\x27]?[A-Za-z_][A-Za-z0-9_]*["\x27]?/) {
    w = substr($0, RSTART, RLENGTH); gsub(/^<<-?[ ]*|["\x27]/, "", w); delim = w; skip = 1
  }
' <<< "$command" | tr -d "\"'" | sed -E 's/(\|\||&&|;|\||\(|\)|`|\$\()/\n/g; s/(^|[[:space:]])(then|do|else)[[:space:]]/\n/g; s/bash -c|sh -c/\n/g')

resolve() { ( cd "$1" 2>/dev/null && cd "$2" 2>/dev/null && pwd ) || printf '%s' "$2"; }
current_branch() { git -C "$1" symbolic-ref --quiet --short HEAD 2>/dev/null || true; }

while IFS= read -r segment; do
  read -ra t <<< "$segment"
  i=0
  while [ $i -lt ${#t[@]} ]; do
    case "${t[$i]}" in env|*=*) i=$((i + 1)) ;; *) break ;; esac
  done
  [ $i -ge ${#t[@]} ] && continue

  if [ "${t[$i]}" = cd ]; then dir=$(resolve "$dir" "${t[$((i + 1))]:-.}"); continue; fi
  [ "${t[$i]}" = git ] || continue

  target="$dir"
  j=$((i + 1))
  while [ $j -lt ${#t[@]} ]; do
    case "${t[$j]}" in
      -C) target=$(resolve "$dir" "${t[$((j + 1))]:-.}"); j=$((j + 2)) ;;
      -c) j=$((j + 2)) ;;
      -*) j=$((j + 1)) ;;
      *) break ;;
    esac
  done
  sub="${t[$j]:-}"
  args=("${t[@]:$((j + 1))}")
  branch=$(current_branch "$target")

  has() { local a; for a in "${args[@]}"; do [ "$a" = "$1" ] && return 0; done; return 1; }
  short() { local a; for a in "${args[@]}"; do [[ "$a" =~ ^-[a-zA-Z]*$1[a-zA-Z]*$ ]] && return 0; done; return 1; }
  flags() { local a; for a in "${args[@]}"; do [[ "$a" == -* ]] && printf '%s ' "$a"; done; }
  names() { local a; for a in "${args[@]}"; do [[ "$a" == -* ]] || printf '%s\n' "$a"; done; }

  case "$sub" in
    commit)
      [ "$branch" = main ] && deny "On main in $target. Branch first: .claude/scripts/worktree.sh <kind>/<slug>."
      { has --no-verify || short n; } && deny "Commit hooks are not bypassed."
      ;;
    push)
      has --no-verify && deny "Push hooks are not bypassed."
      { has --force || short f; } && deny "Use --force-with-lease, and only on your own branch."
      [ "$branch" = main ] && deny "Never push main."
      for a in "${args[@]}"; do
        ref="${a#+}"; ref="${ref#refs/heads/}"; ref="${ref##*:}"; ref="${ref#refs/heads/}"
        [[ "$ref" == main || "$ref" == master ]] && deny "Never push to main."
      done
      if [ -n "$branch" ]; then
        problem=$(branch_problem "$branch"); [ -n "$problem" ] && deny "$problem. Rename with: git branch -m <kind>/<slug>"
      fi
      ;;
    add)
      { has --force || short f; } && deny "Ignored files are private by design; never add -f."
      ;;
    checkout|switch)
      for ((k = 0; k < ${#args[@]}; k++)); do
        case "${args[$k]}" in -b|-B|-c|-C)
          problem=$(branch_problem "${args[$((k + 1))]:-}"); [ -n "$problem" ] && deny "$problem" ;;
        esac
      done
      ;;
    branch)
      other=$(flags | tr ' ' '\n' | grep -vxE -- '-m|-M|-c|-C|-f|--force|--copy|--move' | grep -c . || true)
      if [ "$other" = 0 ]; then
        last=$(names | tail -n1)
        if [ -n "$last" ]; then
          problem=$(branch_problem "$last"); [ -n "$problem" ] && deny "$problem"
        fi
      fi
      ;;
    worktree)
      for ((k = 0; k < ${#args[@]}; k++)); do
        case "${args[$k]}" in -b|-B)
          problem=$(branch_problem "${args[$((k + 1))]:-}"); [ -n "$problem" ] && deny "$problem" ;;
        esac
      done
      ;;
  esac
done <<< "$body"
exit 0
