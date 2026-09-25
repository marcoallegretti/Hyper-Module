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

cargo test -p hyper-desktop-audio --lib live_ \
  -- --ignored --test-threads=1

cargo test -p hyper-runtime --bin hyper-runtime --jobs 1 \
  recording::tests::live_runtime_owner_records_publishes_and_restores_verified_playback \
  -- --ignored --exact
