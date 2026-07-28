import assert from "node:assert/strict";
import { readFile } from "node:fs/promises";
import { dirname, resolve } from "node:path";
import { fileURLToPath } from "node:url";

import { assertAudioClipInvariants } from "../lib/audio-clip.mjs";
import { strictParse } from "../lib/strict-json.mjs";

const root = resolve(dirname(fileURLToPath(import.meta.url)), "..");
const clip = strictParse(
  await readFile(resolve(root, "fixtures/valid/audio-clip.json"), "utf8"),
);

assertAudioClipInvariants(clip);
assert.throws(
  () =>
    assertAudioClipInvariants({
      ...clip,
      loop: { enabled: true, start_tick: 20, end_tick: 20 },
    }),
  /loop start must precede/,
);
assert.throws(
  () =>
    assertAudioClipInvariants({
      ...clip,
      fades: {
        curve: "linear-amplitude",
        in_ticks: 4000,
        out_ticks: 4000,
      },
    }),
  /combined fades/,
);
assert.throws(
  () =>
    assertAudioClipInvariants({
      ...clip,
      source: {
        ...clip.source,
        start_frame: Number.MAX_SAFE_INTEGER,
        frame_count: 1,
      },
    }),
  /source frame range/,
);
assert.throws(
  () =>
    assertAudioClipInvariants({
      ...clip,
      warp_markers: [
        { tick: 0, source_frame_offset: 0 },
        { tick: 7680, source_frame_offset: 95999 },
      ],
    }),
  /last warp marker must equal source frame count/,
);
assert.throws(
  () =>
    assertAudioClipInvariants({
      ...clip,
      warp_markers: [
        { tick: 0, source_frame_offset: 0 },
        { tick: 4000, source_frame_offset: 50000 },
        { tick: 3000, source_frame_offset: 60000 },
        { tick: 7680, source_frame_offset: 96000 },
      ],
    }),
  /warp-marker ticks must be strictly increasing/,
);

console.log("Audio clip invariants passed: 6 deterministic cases");
