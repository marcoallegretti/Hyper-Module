import assert from "node:assert/strict";
import { readFile } from "node:fs/promises";
import { dirname, resolve } from "node:path";
import { fileURLToPath } from "node:url";

import { strictParse } from "../lib/strict-json.mjs";
import {
  assertTimelineInvariants,
  frameToTick,
  tickToFrame,
} from "../lib/timeline.mjs";

const root = resolve(dirname(fileURLToPath(import.meta.url)), "..");
const timeline = strictParse(
  await readFile(resolve(root, "fixtures/valid/timeline.json"), "utf8"),
);

assertTimelineInvariants(timeline);
assert.equal(tickToFrame(timeline, 0), 0);
assert.equal(tickToFrame(timeline, 480), 6000);
assert.equal(tickToFrame(timeline, 1920), 24000);
assert.equal(tickToFrame(timeline, 3840), 48000);
assert.equal(tickToFrame(timeline, 5760), 96000);
assert.equal(frameToTick(timeline, 0), 0);
assert.equal(frameToTick(timeline, 12000), 960);
assert.equal(frameToTick(timeline, 48000), 3840);
assert.equal(frameToTick(timeline, 72000), 4800);

const fractional = {
  ...timeline,
  tempo_events: [{ tick: 0, microseconds_per_quarter: 500001 }],
};
assert.equal(tickToFrame(fractional, 1), 12);
assert.equal(frameToTick(fractional, 12), 0);
assert.equal(frameToTick(fractional, 13), 1);
assert.equal(tickToFrame(fractional, 1920), 24000);

const fractionalBoundary = {
  ...timeline,
  tempo_events: [
    { tick: 0, microseconds_per_quarter: 500001 },
    { tick: 1, microseconds_per_quarter: 500003 },
  ],
};
assert.equal(tickToFrame(fractionalBoundary, 2), 25);
assert.equal(frameToTick(fractionalBoundary, 25), 1);
assert.equal(frameToTick(fractionalBoundary, 26), 2);

assert.throws(
  () =>
    assertTimelineInvariants({
      ...timeline,
      tempo_events: [
        { tick: 0, microseconds_per_quarter: 500000 },
        { tick: 0, microseconds_per_quarter: 750000 },
      ],
    }),
  /strictly increasing/,
);
assert.throws(() => tickToFrame(timeline, -1), /nonnegative safe integer/);
assert.throws(() => frameToTick(timeline, -1), /nonnegative safe integer/);

console.log("Timeline conversion passed: 18 deterministic cases");
