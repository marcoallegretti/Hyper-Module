#!/usr/bin/env bash
# Usage: lint-diff.sh [base]  — checks lines added since base (default: merge-base
# with origin/main, else HEAD), untracked files included, for what AGENTS.md
# forbids. Exit 1 on findings.
set -uo pipefail
base="${1:-}"
if [ -z "$base" ]; then
  base=$(git merge-base origin/main HEAD 2>/dev/null || echo HEAD)
fi
paths=('*.rs' '*.slint' '*.mjs' ':!third_party' ':!.claude')

added() {
  git diff --unified=0 --no-color --no-ext-diff --src-prefix=a/ --dst-prefix=b/ "$base" -- "${paths[@]}"
  git ls-files --others --exclude-standard -z -- "${paths[@]}" |
    xargs -0 -r -n1 git diff --no-index --unified=0 --no-color --no-ext-diff --src-prefix=a/ --dst-prefix=b/ -- /dev/null
}

stream=$(added)

findings=$(awk '
  /^\+\+\+ b\// { file = substr($0, 7); run = 0; next }
  /^@@/ { match($0, /\+[0-9]+/); line = substr($0, RSTART + 1, RLENGTH - 1) - 1; run = 0; next }
  /^\+/ {
    line++; text = substr($0, 2)
    doc = (text ~ /^[ \t]*\/\/[\/!]/)
    plain = (!doc && text ~ /^[ \t]*\/\//)
    if (!doc && text ~ /(todo|unimplemented)!\(/) print file ":" line ": stub macro"
    else if (!doc && text ~ /dbg!\(/) print file ":" line ": debug leftover"
    else if (text ~ /(^|[^A-Za-z])(TODO|FIXME|XXX|HACK)([^A-Za-z]|$)/) print file ":" line ": unfinished marker"
    else if (tolower(text) ~ /coming soon|not (yet )?implemented|lorem ipsum/) print file ":" line ": placeholder text"
    if (file ~ /\.rs$/ && plain) { run++; if (run == 4) print file ":" line ": narrating comment (four plain // lines in a row)" } else run = 0
    next
  }
' <<< "$stream")

bare=$(awk '
  /^\+\+\+ b\// { file = substr($0, 7); next }
  /^@@/ { match($0, /\+[0-9]+/); line = substr($0, RSTART + 1, RLENGTH - 1) - 1; next }
  /^\+/ { line++; if (file ~ /\.rs$/ && substr($0, 2) ~ /(#!?\[|cfg_attr\(.*)(allow|expect)\(/) print file "\t" line }
' <<< "$stream" |
  while IFS=$'\t' read -r f n; do
    [ -f "$f" ] && FILE="$f" LINE="$n" perl -ne 'BEGIN { $t = "" } if ($. >= $ENV{LINE}) { $t .= $_; if (/\]/) { print "$ENV{FILE}:$ENV{LINE}: lint silenced without reason\n" unless $t =~ /reason/; exit } }' "$f"
  done)

out=$(printf '%s\n%s\n' "$findings" "$bare" | sed '/^$/d')
[ -z "$out" ] && exit 0
printf '%s\n' "$out" >&2
exit 1
