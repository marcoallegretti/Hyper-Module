#!/usr/bin/env bash
# Source this from one Linux test shell to keep its virtual graph alive until exit.
set -euo pipefail

runtime_directory="$(mktemp -d)"
export XDG_RUNTIME_DIR="$runtime_directory"
daemon_pid=""
session_manager_pid=""

cleanup_pipewire_virtual_graph() {
  local status="$?"
  if (( status != 0 )); then
    printf 'PipeWire graph failed (exit %s); daemon diagnostics follow.\n' "$status" >&2
    tail -n 80 "$runtime_directory/pipewire.log" >&2 || true
    tail -n 80 "$runtime_directory/wireplumber.log" >&2 || true
  fi
  if [[ -n "$session_manager_pid" ]]; then
    kill "$session_manager_pid" 2>/dev/null || true
    wait "$session_manager_pid" 2>/dev/null || true
  fi
  if [[ -n "$daemon_pid" ]]; then
    kill "$daemon_pid" 2>/dev/null || true
    wait "$daemon_pid" 2>/dev/null || true
  fi
  rm -rf "$runtime_directory"
}
trap cleanup_pipewire_virtual_graph EXIT

pipewire >"$runtime_directory/pipewire.log" 2>&1 &
daemon_pid="$!"

for _ in $(seq 1 50); do
  if pw-cli info 0 >/dev/null 2>&1; then
    break
  fi
  sleep 0.1
done
pw-cli info 0 >/dev/null

wireplumber >"$runtime_directory/wireplumber.log" 2>&1 &
session_manager_pid="$!"

pw-cli create-node adapter '{
  factory.name = support.null-audio-sink
  node.name = hyper-ci-sink
  node.description = "Hyper CI Sink"
  media.class = Audio/Sink
  object.linger = true
  audio.position = [ FL FR ]
}' >/dev/null

pw-cli create-node adapter '{
  factory.name = support.null-audio-sink
  node.name = hyper-ci-source
  node.description = "Hyper CI Source"
  media.class = Audio/Source/Virtual
  object.linger = true
  audio.position = [ FL FR ]
}' >/dev/null

for _ in $(seq 1 50); do
  pw-dump >"$runtime_directory/graph.json"
  if grep -q '"node.name": "hyper-ci-sink"' "$runtime_directory/graph.json" \
    && grep -q '"node.name": "hyper-ci-source"' "$runtime_directory/graph.json"; then
    break
  fi
  sleep 0.1
done
pw-dump >"$runtime_directory/graph.json"
grep -q '"node.name": "hyper-ci-sink"' "$runtime_directory/graph.json"
grep -q '"node.name": "hyper-ci-source"' "$runtime_directory/graph.json"
