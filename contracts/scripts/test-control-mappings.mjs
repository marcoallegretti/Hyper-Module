import assert from "node:assert/strict";
import { readFile } from "node:fs/promises";
import { dirname, resolve } from "node:path";
import { fileURLToPath } from "node:url";
import {
  assertControlMappingInvariants,
  mapLinearControl,
  normalizedTargetValue,
} from "../lib/control-mappings.mjs";
import { strictParse } from "../lib/strict-json.mjs";

const root = resolve(dirname(fileURLToPath(import.meta.url)), "..");
const mappings = strictParse(
  await readFile(resolve(root, "fixtures/valid/control-mappings.json"), "utf8"),
);
const transformVectors = strictParse(
  await readFile(
    resolve(root, "fixtures/vectors/control-mapping-transforms.json"),
    "utf8",
  ),
);
const transform = mappings.bindings[0].transform;
assertControlMappingInvariants(mappings);
assert.equal(mapLinearControl(transform, -2_000_000), 0);
assert.equal(mapLinearControl(transform, -1_000_000), 0);
assert.equal(mapLinearControl(transform, 0), 500_000);
assert.equal(mapLinearControl(transform, 1_000_000), 1_000_000);
assert.equal(mapLinearControl(transform, 2_000_000), 1_000_000);
assert.equal(
  normalizedTargetValue(mappings.bindings[0].target, 500_000),
  1_000_000,
);
assert.equal(
  normalizedTargetValue({ kind: "mixer-channel", parameter: "pan" }, 0),
  -1_000_000,
);
assert.throws(
  () =>
    assertControlMappingInvariants({
      ...mappings,
      bindings: [
        mappings.bindings[0],
        {
          ...mappings.bindings[0],
          id: "018f47a2-6b2d-7c11-8a44-001122334476",
        },
      ],
    }),
  /source and scope combinations must be unique/,
);
assert.equal(transformVectors.format, "hyper-control-mapping-transform-vectors");
assert.equal(transformVectors.format_version, 1);
assert.equal(
  new Set(transformVectors.cases.map(({ name }) => name)).size,
  transformVectors.cases.length,
);
for (const vector of transformVectors.cases) {
  const normalized = mapLinearControl(vector.transform, vector.input);
  assert.equal(normalized, vector.normalized, vector.name);
  assert.equal(
    normalizedTargetValue({ kind: "mixer-main", parameter: "gain" }, normalized),
    vector.gain_millionths,
    vector.name,
  );
  assert.equal(
    normalizedTargetValue(
      { kind: "mixer-channel", parameter: "pan" },
      normalized,
    ),
    vector.pan_millionths,
    vector.name,
  );
}
console.log(
  `Control mapping invariants passed: 9 deterministic cases and ${transformVectors.cases.length} shared vectors`,
);
