#!/usr/bin/env bash
set -euo pipefail

if [[ "${HYPER_PIPEWIRE_TEST_DBUS_SESSION:-}" != 1 ]]; then
  dbus_runtime_directory="$(mktemp -d)"
  trap 'rm -rf "$dbus_runtime_directory"' EXIT
  XDG_RUNTIME_DIR="$dbus_runtime_directory" HYPER_PIPEWIRE_TEST_DBUS_SESSION=1 \
    dbus-run-session -- bash "${BASH_SOURCE[0]}" "$@"
  exit
fi

source "$(dirname -- "${BASH_SOURCE[0]}")/pipewire-virtual-graph.sh"

cargo test -p hyper-desktop-audio \
  pipewire_output::tests::live_pipewire_null_sink_processes_audio \
  -- --ignored --exact

cargo test -p hyper-desktop-audio \
  playback_session::tests::live_desktop_recovery_rebuilds_one_operable_session \
  -- --ignored --exact

cargo test -p hyper-desktop-audio \
  pipewire_input::tests::live_pipewire_source_captures_audio \
  -- --ignored --exact
