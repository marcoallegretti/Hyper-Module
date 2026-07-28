import assert from "node:assert/strict";
import { readFile } from "node:fs/promises";
import { dirname, resolve } from "node:path";
import { fileURLToPath } from "node:url";

import { assertSceneInvariants, nextSceneLaunchTick } from "../lib/scene.mjs";
import { strictParse } from "../lib/strict-json.mjs";

const root = resolve(dirname(fileURLToPath(import.meta.url)), "..");
const scene = strictParse(
  await readFile(resolve(root, "fixtures/valid/scene.json"), "utf8"),
);

assertSceneInvariants(scene);
assert.equal(nextSceneLaunchTick(scene, 0), 7680);
assert.equal(nextSceneLaunchTick(scene, 7679), 7680);
assert.equal(nextSceneLaunchTick(scene, 7680), 15360);
assert.equal(
  nextSceneLaunchTick(
    {
      ...scene,
      launch_quantization: { mode: "immediate" },
    },
    7680,
  ),
  7680,
);
assert.throws(
  () =>
    assertSceneInvariants({
      ...scene,
      slots: [
        scene.slots[0],
        { ...scene.slots[1], track_id: scene.slots[0].track_id },
      ],
    }),
  /track identifiers must be unique/,
);
assert.throws(
  () =>
    nextSceneLaunchTick(
      {
        ...scene,
        launch_quantization: {
          mode: "grid",
          quantum_ticks: Number.MAX_SAFE_INTEGER,
        },
      },
      Number.MAX_SAFE_INTEGER,
    ),
  /exceeds the safe-integer domain/,
);

console.log("Scene scheduling passed: 7 deterministic cases");
