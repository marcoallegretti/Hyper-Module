#!/usr/bin/env bash
file=$(jq -r '.tool_input.file_path // ""')
case "$file" in
  *.rs) [ -f "$file" ] && rustfmt --edition 2024 "$file" >/dev/null 2>&1 ;;
esac
exit 0
