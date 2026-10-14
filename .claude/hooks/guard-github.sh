#!/usr/bin/env bash
# Same rules as guard-bash.sh for changes made through the GitHub tools.
set -uo pipefail
command -v jq >/dev/null || { echo "jq is required by the repository hooks" >&2; exit 2; }
source "$(dirname "$0")/lib.sh"

input=$(jq -c '.tool_input // {}')
field() { printf '%s' "$input" | jq -r --arg k "$1" '.[$k] // empty'; }

for key in branch head; do
  name=$(field "$key")
  if [ -n "$name" ]; then
    problem=$(branch_problem "$name"); [ -n "$problem" ] && deny "$problem"
  fi
done
for key in title body message; do
  problem=$(text_problem "$(field "$key")"); [ -n "$problem" ] && deny "$problem"
done
msg=$(field message)
if [ -n "$msg" ]; then
  subject=$(printf '%s' "$msg" | head -n1)
  [ ${#subject} -gt 72 ] && deny "Commit subject is over 72 characters."
  [[ "$subject" =~ ^[a-z]+(\([^\)]*\))?!?: ]] && deny "Commit subjects are plain sentences, not type: prefixes."
fi
exit 0
