#!/usr/bin/env bash
# repo-commit-msg-hook (installed by setup.sh)
set -uo pipefail
file="$1"
msg=$(sed '/^# -* >8 -*$/,$d' "$file" | grep -v '^#')
subject=$(printf '%s\n' "$msg" | sed -n '1p')
body=$(printf '%s\n' "$msg" | sed '1d' | grep -c '[^[:space:]]' || true)
fail() { echo "commit-msg: $1" >&2; exit 1; }

[ -n "$subject" ] || fail "empty subject"
[ ${#subject} -le 72 ] || fail "subject is over 72 characters"
[[ "$subject" =~ \.$ ]] && fail "subject ends with a period"
[[ "$subject" =~ ^[a-zA-Z]+(\([^\)]*\))?!?:\  ]] && fail "no type: prefix; write a sentence saying what is now true"
[[ "$subject" =~ ^(wip|fixup!|squash!|tmp)([^a-zA-Z]|$) ]] && fail "fold fixups into the commit they belong to"
[ "$body" -le 5 ] || fail "body is over five lines; most commits need none"
if printf '%s' "$msg" | grep -qiE '^(co-authored-by|claude-session|signed-off-by):|generated (with|by) (\[?claude|an? ai|ai|anthropic|copilot|codex|gemini|chatgpt)\b|claude\.(ai|com)/code|🤖|noreply@anthropic'; then
  fail "no trailers or tool attribution"
fi
exit 0
