---
name: realtime
description: Audio-thread rules and their tests. Use when touching audio-engine, mixer-runtime, native-device, clip-engine, arrangement-renderer, media-cache, recording-transport, or any audio callback in software/IDE/audio-backend or software/RUNTIME/runtime.
---

# Real-time code

The callback path never allocates, locks, does I/O, logs, or panics. Everything
it needs is built before the stream starts and handed over through bounded,
preallocated channels (`recording-transport`, `loudness_command_channel`,
`output_meter_channel`).

- Data prepared off-thread, consumed on-thread. Compile/validate in
  `playback-compiler`, `media-cache`, `mapping-compiler`; the callback only
  indexes and mixes.
- Bounds are explicit (`MAX_PROCESS_FRAMES`, fixed track counts). Reject
  oversize input with a typed error; never grow a buffer in the callback.
- Parameter changes arrive as commands applied at block boundaries and
  crossfaded; never as shared mutable state behind a lock.
- Non-real-time work (disk, decoding, journals) lives in `recording-worker`
  and threads owned by `thread-supervisor`.

## Proof

Each real-time crate has `tests/realtime_allocation.rs`, including
`test-support/realtime_allocator.rs` through `#[path]` and using
`HeapMeasurement` to assert the processing call performs no heap operations. A new
callback-path function gets a case there, covering the first call as well as
steady state. Run optimized too:

```bash
cargo test --locked --release -p hyper-audio-engine -p hyper-mixer-runtime -p hyper-desktop-audio
```

Device code (`pipewire_*`, `wasapi_*`) is tested on a virtual PipeWire graph
on Linux (`software/IDE/audio-backend/scripts/`); WASAPI only in Windows CI.
Physical devices are manual: report what was run.
