BRANCH_KINDS='fix|feat|perf|refactor|test|ci|build'
BRANCH_RE="^($BRANCH_KINDS)/[a-z0-9]+(-[a-z0-9]+)*\$"
SLUG_RE='^(claude|codex|gemini|copilot|cursor|chatgpt|llm|ai|wip|tmp|misc|stuff|changes|update|fixes)(-|$)'

branch_problem() {
  local name="$1"
  if [[ ! "$name" =~ $BRANCH_RE ]]; then
    echo "branch '$name' must be <kind>/<kebab-slug>, kind one of: ${BRANCH_KINDS//|/ }"
  elif [[ "${name#*/}" =~ $SLUG_RE ]]; then
    echo "branch '$name' names a tool or says nothing about the change"
  fi
}

text_problem() {
  local re="(^|[\"'[:space:]])(co-authored-by|claude-session):|generated (with|by) (\[?claude|an? ai|ai|anthropic|copilot|codex|gemini|chatgpt)\b|claude\.(ai|com)/code|🤖|noreply@anthropic"
  if printf '%s' "$1" | grep -qiE "$re"; then
    echo "tool attribution does not belong in commits or pull requests"
  fi
}

deny() {
  jq -n --arg reason "$1" '{hookSpecificOutput:{hookEventName:"PreToolUse",permissionDecision:"deny",permissionDecisionReason:$reason}}'
  exit 0
}
