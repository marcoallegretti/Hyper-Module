import assert from "node:assert/strict";
import { readFile } from "node:fs/promises";
import { dirname, resolve } from "node:path";
import { fileURLToPath } from "node:url";

import {
  assertArrangementInvariants,
  placementClipTick,
} from "../lib/arrangement.mjs";
import { strictParse } from "../lib/strict-json.mjs";

const root = resolve(dirname(fileURLToPath(import.meta.url)), "..");
const arrangement = strictParse(
  await readFile(resolve(root, "fixtures/valid/arrangement.json"), "utf8"),
);
const sceneLaunch = {
  id: "018f47a2-6b2d-7c11-8a44-001122334465",
  tick: 15360,
  scene_id: "018f47a2-6b2d-7c11-8a44-001122334461",
};

assertArrangementInvariants(arrangement);
assertArrangementInvariants({
  ...arrangement,
  placements: [
    arrangement.placements[0],
    {
      ...arrangement.placements[0],
      id: "018f47a2-6b2d-7c11-8a44-001122334466",
      start_tick: 7680,
    },
  ],
  scene_launches: [],
});
assert.throws(
  () =>
    assertArrangementInvariants({
      ...arrangement,
      placements: [
        arrangement.placements[0],
        {
          ...arrangement.placements[0],
          id: "018f47a2-6b2d-7c11-8a44-001122334466",
          start_tick: 3840,
        },
        {
          ...arrangement.placements[0],
          id: "018f47a2-6b2d-7c11-8a44-001122334467",
          start_tick: 5760,
        },
      ],
      scene_launches: [],
    }),
  /exceed the 2-voice/,
);
assert.throws(
  () =>
    assertArrangementInvariants({
      ...arrangement,
      placements: [
        {
          ...arrangement.placements[0],
          start_tick: Number.MAX_SAFE_INTEGER,
        },
      ],
      scene_launches: [],
    }),
  /safe-integer domain/,
);
assertArrangementInvariants({
  ...arrangement,
  placements: [
    {
      ...arrangement.placements[0],
      fade_in_ticks: 1920,
      fade_out_ticks: 1920,
    },
  ],
  scene_launches: [],
});
assert.throws(
  () =>
    assertArrangementInvariants({
      ...arrangement,
      placements: [
        {
          ...arrangement.placements[0],
          fade_in_ticks: 4000,
          fade_out_ticks: 4000,
        },
      ],
      scene_launches: [],
    }),
  /combined placement fades/,
);
assert.throws(
  () =>
    assertArrangementInvariants({
      ...arrangement,
      placements: [],
      scene_launches: [
        sceneLaunch,
        {
          ...sceneLaunch,
          id: "018f47a2-6b2d-7c11-8a44-001122334466",
        },
      ],
    }),
  /strictly increasing/,
);
assert.throws(
  () =>
    assertArrangementInvariants({
      ...arrangement,
      placements: [],
      scene_launches: [
        sceneLaunch,
        {
          ...sceneLaunch,
          tick: 30720,
        },
      ],
    }),
  /event identifiers must be unique/,
);

const clip = strictParse(
  await readFile(resolve(root, "fixtures/valid/audio-clip.json"), "utf8"),
);
const loopingClip = {
  ...clip,
  loop: { enabled: true, start_tick: 1920, end_tick: 7680 },
};
const longPlacement = {
  ...arrangement.placements[0],
  duration_ticks: 20000,
};
assert.equal(placementClipTick(longPlacement, loopingClip, 7679), 7679);
assert.equal(placementClipTick(longPlacement, loopingClip, 7680), 1920);
assert.equal(placementClipTick(longPlacement, loopingClip, 8000), 2240);
assert.equal(placementClipTick(longPlacement, loopingClip, 13440), 1920);
assert.throws(
  () => placementClipTick(longPlacement, loopingClip, 20000),
  /inside the placement/,
);

console.log("Arrangement invariants passed: 13 deterministic cases");
