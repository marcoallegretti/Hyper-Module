---
name: ide
description: The Desktop IDE — application state, workers, Slint UI. Use when touching software/IDE/* or desktop-ui/ui/main.slint.
---

# Desktop IDE

- `application` owns state (single owner) and `worker.rs` runs bounded work off
  the UI thread; `recording.rs` drives recording. Logic lives here, with tests.
- `desktop-ui` is the Slint front end: `ui/main.slint` plus `src/main.rs`
  wiring callbacks to `application`. It holds no business rules; it renders
  state and forwards intent.
- `audio-backend` (hyper-desktop-audio) wraps PipeWire/WASAPI sessions.
  `runtime-bundle-builder` produces `.runtime` bundles; `project-history`
  persists recents; `mapping-editor` and `panel-simulator` serve mappings and
  panel simulation.

## The UI states only what is true

- Every control has a handler that does the real thing. No disabled-forever
  placeholder buttons, no "coming soon", no fake meters or sample data.
- State reflects the application's answer, not the user's click: show
  "Saved" after the save succeeded, "Recording" after capture started,
  "Unavailable" with a reason when a device or capability is missing.
- Errors surface with what failed and what to do; never swallowed into a
  success-looking state.
- Respect README's product limits. Do not add UI for MIDI/CV execution,
  third-party plug-ins, or installation that the IDE does not perform
  (**Prepare Runtime** builds a bundle; it does not install it).

## Proof

Behaviour: `application` and worker tests. Screens: `desktop-ui` tests on
`i-slint-backend-testing` that drive the real component and assert properties
and callbacks. Linux worker tests need the virtual PipeWire graph:

```bash
dbus-run-session -- bash -c '
  source software/IDE/audio-backend/scripts/pipewire-virtual-graph.sh
  cargo test -p hyper-desktop-ide --all-targets --locked -- --test-threads=1'
```

Pixels are not golden-tested; for a visual change, run `hyper-desktop-ui` and
say in the pull request what you looked at.
