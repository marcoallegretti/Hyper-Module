import assert from "node:assert/strict";
import { readFile } from "node:fs/promises";
import { dirname, resolve } from "node:path";
import { fileURLToPath } from "node:url";

import {
  assertMixerInvariants,
  gainCoefficient,
  soloEligibleTrackIds,
} from "../lib/mixer.mjs";
import { strictParse } from "../lib/strict-json.mjs";

const root = resolve(dirname(fileURLToPath(import.meta.url)), "..");
const mixer = strictParse(
  await readFile(resolve(root, "fixtures/valid/mixer.json"), "utf8"),
);
const cycle = strictParse(
  await readFile(
    resolve(root, "fixtures/invalid/mixer-routing-cycle.json"),
    "utf8",
  ),
);
const duplicateSend = strictParse(
  await readFile(
    resolve(root, "fixtures/invalid/mixer-duplicate-send-bus.json"),
    "utf8",
  ),
);

assertMixerInvariants(mixer);
assert.equal(gainCoefficient(0), 0);
assert.equal(gainCoefficient(1_000_000), 1);
assert.equal(gainCoefficient(2_000_000), 2);
assert.throws(() => gainCoefficient(2_000_001), /0 through 2000000/);
assert.throws(() => assertMixerInvariants(cycle), /contains a cycle/);
assert.throws(() => assertMixerInvariants(duplicateSend), /must be unique/);
assert.deepEqual(soloEligibleTrackIds(mixer), [
  "018f47a2-6b2d-7c11-8a44-001122334456",
]);
assert.deepEqual(
  soloEligibleTrackIds({
    ...mixer,
    channels: [{ ...mixer.channels[0], solo: true }],
  }),
  ["018f47a2-6b2d-7c11-8a44-001122334456"],
);
const routedSoloMixer = {
  ...mixer,
  channels: [
    {
      ...mixer.channels[0],
      solo: true,
      destination: {
        kind: "track",
        track_id: "018f47a2-6b2d-7c11-8a44-001122334458",
      },
    },
    {
      ...mixer.channels[0],
      track_id: "018f47a2-6b2d-7c11-8a44-001122334458",
      sends: [
        {
          id: "018f47a2-6b2d-7c11-8a44-001122334471",
          bus: "a",
          tap: "post-fader",
          gain_millionths: 500_000,
          pan_millionths: 0,
          enabled: true,
        },
      ],
    },
    {
      ...mixer.channels[0],
      track_id: "018f47a2-6b2d-7c11-8a44-001122334469",
    },
  ],
  return_buses: [
    { bus: "a", track_id: "018f47a2-6b2d-7c11-8a44-001122334469" },
  ],
};
assert.deepEqual(soloEligibleTrackIds(routedSoloMixer), [
  "018f47a2-6b2d-7c11-8a44-001122334456",
  "018f47a2-6b2d-7c11-8a44-001122334458",
  "018f47a2-6b2d-7c11-8a44-001122334469",
]);
assert.throws(
  () =>
    assertMixerInvariants({
      ...routedSoloMixer,
      channels: routedSoloMixer.channels.map((channel) =>
        channel.track_id === "018f47a2-6b2d-7c11-8a44-001122334469"
          ? {
              ...channel,
              destination: {
                kind: "track",
                track_id: "018f47a2-6b2d-7c11-8a44-001122334458",
              },
            }
          : channel,
      ),
    }),
  /routing contains a cycle/,
);

console.log("Mixer invariants passed: 11 deterministic cases");
