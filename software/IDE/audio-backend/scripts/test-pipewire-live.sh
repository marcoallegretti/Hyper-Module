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

cargo test --locked -p hyper-desktop-audio --lib live_ \
  -- --ignored --test-threads=1

cargo test --locked -p hyper-runtime --bin hyper-runtime --jobs 1 \
  recording::tests::live_runtime_recording_round_trips_through_ide \
  -- --ignored --exact 2>&1 | tee "$runtime_directory/runtime-recording-handoff.log"
if ! grep -q '^test result: ok\. 1 passed; 0 failed; 0 ignored; 0 measured;' \
  "$runtime_directory/runtime-recording-handoff.log"; then
  printf 'Expected exactly one passing Runtime recording handoff test.\n' >&2
  exit 1
fi
