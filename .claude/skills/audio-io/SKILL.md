---
name: audio-io
description: Device I/O — PipeWire and WASAPI sessions, the vendored pipewire bindings, capture and playback sessions. Use when touching software/IDE/audio-backend or third_party/pipewire.
---

# Audio I/O

`software/IDE/audio-backend` (hyper-desktop-audio) owns every device session:
`pipewire_{devices,input,output}.rs` and `wasapi_{devices,input,output}.rs`
behind `desktop_{input,output}.rs`, with playback, monitoring, recording and
test-tone sessions on top. Both applications use it; Runtime depends on it.

- Platform code is gated by `cfg` (`target_os = "linux"` little-endian, or
  `windows`). Windows-only imports are gated too, or the Linux build warns and
  clippy `-D warnings` fails. Check the other platform compiles by reading the
  gates, since CI is the only Windows run.
- Routes are resolved from active endpoints and pinned explicitly; recording
  never falls back to a default device. A missing or changed route is a typed
  error surfaced to the caller, not a silent switch.
- Capture discontinuities (xruns, dropped blocks) are counted and reported;
  never hidden. Do not claim a recording is lossless.
- `third_party/pipewire` is vendored, patched in through `[patch.crates-io]`,
  excluded from the workspace, and linted and tested on its own. Change it only
  for a defect that blocks this repository, and keep the diff minimal.
- `unsafe_code = deny` applies; the vendored bindings are the exception, not a
  precedent.

## Proof

Unit tests run with the workspace tests. The ignored `live_` tests drive real
PipeWire sessions on a virtual graph; the IDE worker tests use the same graph:

```bash
bash software/IDE/audio-backend/scripts/test-pipewire-live.sh
```

WASAPI runs only in Windows CI. A physical interface is manual: the pull
request says which device, system and what was observed, or says it was not
run.
