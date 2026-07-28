import assert from "node:assert/strict";
import { readFile } from "node:fs/promises";
import { dirname, resolve } from "node:path";
import { fileURLToPath } from "node:url";

import {
  assertAutomationTakeInvariants,
  automationValueAt,
} from "../lib/automation-take.mjs";
import { strictParse } from "../lib/strict-json.mjs";

const root = resolve(dirname(fileURLToPath(import.meta.url)), "..");
const take = strictParse(
  await readFile(resolve(root, "fixtures/valid/automation-take.json"), "utf8"),
);
assertAutomationTakeInvariants(take);

const gainLane = take.lanes[0];
assert.equal(automationValueAt(gainLane, 0), 0);
assert.equal(automationValueAt(gainLane, 1920), 500_000);
assert.equal(automationValueAt(gainLane, 3840), 1_000_000);
assert.equal(automationValueAt(gainLane, 5760), 1_500_130);
assert.equal(automationValueAt(gainLane, 9000), 2_000_000);

const muteLane = take.lanes[1];
assert.equal(automationValueAt(muteLane, 5759), false);
assert.equal(automationValueAt(muteLane, 5760), true);

const decreasingLane = {
  interpolation: "linear",
  points: [
    { tick: 0, value: 1_000_000 },
    { tick: 3, value: 0 },
  ],
};
assert.equal(automationValueAt(decreasingLane, 1), 666_667);
assert.equal(automationValueAt(decreasingLane, 2), 333_334);

assert.throws(
  () =>
    assertAutomationTakeInvariants({
      ...take,
      lanes: [
        {
          ...muteLane,
          interpolation: "linear",
        },
      ],
    }),
  /Boolean automation requires hold interpolation/,
);
assert.throws(
  () =>
    assertAutomationTakeInvariants({
      ...take,
      lanes: [
        {
          ...gainLane,
          points: [{ tick: take.end_tick, value: 1_000_000 }],
        },
      ],
    }),
  /within the take interval/,
);
assert.throws(
  () =>
    assertAutomationTakeInvariants({
      ...take,
      lanes: [
        {
          ...gainLane,
          target: { ...gainLane.target, parameter: "pan" },
          points: [{ tick: 0, value: 1_000_001 }],
        },
      ],
    }),
  /exceeds the target value domain/,
);

console.log("Automation take invariants passed: 13 deterministic cases");
