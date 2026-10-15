import assert from "node:assert/strict";
import { readFile } from "node:fs/promises";
import { dirname, resolve } from "node:path";
import { fileURLToPath } from "node:url";

import Ajv2020 from "ajv/dist/2020.js";
import canonicalize from "canonicalize";

import { assertArrangementInvariants } from "../lib/arrangement.mjs";
import { assertAutomationTakeInvariants } from "../lib/automation-take.mjs";
import { assertAudioClipInvariants } from "../lib/audio-clip.mjs";
import { assertControlMappingInvariants } from "../lib/control-mappings.mjs";
import { assertDeviceChainInvariants } from "../lib/device-chain.mjs";
import { assertMixerInvariants } from "../lib/mixer.mjs";
import { assertRecordingMetadataInvariants } from "../lib/recording-metadata.mjs";
import { assertSceneInvariants } from "../lib/scene.mjs";
import { assertIJson, strictParse } from "../lib/strict-json.mjs";
import { assertTimelineInvariants } from "../lib/timeline.mjs";
import {
  assertTimingFrameInvariants,
  assertTimingSimulatorProfile,
} from "../lib/timing-simulator.mjs";

const root = resolve(dirname(fileURLToPath(import.meta.url)), "..");

async function loadJson(relativePath) {
  const text = await readFile(resolve(root, relativePath), "utf8");
  return strictParse(text);
}

function assertUnique(values, label) {
  assert.equal(
    new Set(values).size,
    values.length,
    `${label} values must be unique`,
  );
}

function assertProjectInvariants(project) {
  const records = [...project.resources, ...project.assets];
  assertUnique(
    records.map((record) => record.id),
    "project entity identifier",
  );
  assertUnique(
    records.map((record) => record.path.toLowerCase()),
    "case-folded project path",
  );

  const resourceRoots = {
    timeline: "timeline/",
    track: "tracks/",
    clip: "clips/",
    scene: "scenes/",
    arrangement: "arrangement/",
    mixer: "mixer/",
    mapping: "mappings/",
    "automation-take": "takes/",
    "recording-metadata": "assets/recordings/",
    "device-chain": "devices/",
  };
  for (const resource of project.resources) {
    assert(
      resource.path.startsWith(resourceRoots[resource.role]),
      `${resource.path}: path does not match role ${resource.role}`,
    );
  }
  assert.equal(
    project.resources.filter((resource) => resource.role === "timeline").length,
    1,
    "project must contain exactly one timeline",
  );

  for (const asset of project.assets) {
    assert(
      asset.path.startsWith(`assets/${asset.role}s/`),
      `${asset.path}: path does not match role ${asset.role}`,
    );
  }
}

function assertRuntimeInvariants(manifest) {
  const foldedPaths = manifest.files.map((file) => file.path.toLowerCase());
  assertUnique(foldedPaths, "case-folded runtime path");
  assert(
    !foldedPaths.includes("manifest.json"),
    "manifest must not list itself",
  );

  const entrypoints = manifest.files.filter(
    (file) => file.path === manifest.entrypoint,
  );
  assert.equal(entrypoints.length, 1, "entrypoint must be listed exactly once");
  assert.equal(
    entrypoints[0].role,
    "runtime-project",
    "entrypoint must have the runtime-project role",
  );
  assert.equal(
    entrypoints[0].media_type,
    "application/json",
    "runtime project must be JSON in format version 1",
  );
}

const schemaPaths = [
  "schemas/v1/common.schema.json",
  "schemas/v1/hardware-capabilities.schema.json",
  "schemas/v1/project.schema.json",
  "schemas/v1/runtime-manifest.schema.json",
  "schemas/v1/runtime-build-plan.schema.json",
  "schemas/v1/track.schema.json",
  "schemas/v1/timeline.schema.json",
  "schemas/v1/audio-clip.schema.json",
  "schemas/v1/scene.schema.json",
  "schemas/v1/arrangement.schema.json",
  "schemas/v1/mixer.schema.json",
  "schemas/v1/control-mappings.schema.json",
  "schemas/v1/recording-metadata.schema.json",
  "schemas/v1/automation-take.schema.json",
  "schemas/v1/deployment-state.schema.json",
  "schemas/v1/deployment-storage-policy.schema.json",
  "schemas/v1/deployment-lock.schema.json",
  "schemas/v1/deployment-pairing-request.schema.json",
  "schemas/v1/deployment-request.schema.json",
  "schemas/v1/deployment-authorization-state.schema.json",
  "schemas/v1/timing-simulator-profile.schema.json",
  "schemas/v1/timing-frame.schema.json",
  "schemas/v1/finalized-recording-handoff.schema.json",
  "schemas/v1/device-chain.schema.json",
];
const schemas = await Promise.all(schemaPaths.map(loadJson));
const ajv = new Ajv2020({
  allErrors: true,
  strict: true,
  validateFormats: false,
});
schemas.forEach((schema) => ajv.addSchema(schema));

const cases = [
  {
    path: "fixtures/valid/device-chain.json",
    schema: schemas[23].$id,
    valid: true,
    invariants: assertDeviceChainInvariants,
  },
  {
    path: "fixtures/invalid/device-chain-unknown-device.json",
    schema: schemas[23].$id,
    valid: false,
  },
  {
    path: "fixtures/valid/hardware-capabilities.json",
    schema: schemas[1].$id,
    valid: true,
  },
  {
    path: "fixtures/valid/project.json",
    schema: schemas[2].$id,
    valid: true,
    invariants: assertProjectInvariants,
  },
  {
    path: "fixtures/valid/runtime-manifest.json",
    schema: schemas[3].$id,
    valid: true,
    invariants: assertRuntimeInvariants,
  },
  {
    path: "fixtures/valid/runtime-build-plan.json",
    schema: schemas[4].$id,
    valid: true,
  },
  {
    path: "fixtures/valid/track.json",
    schema: schemas[5].$id,
    valid: true,
  },
  {
    path: "fixtures/valid/timeline.json",
    schema: schemas[6].$id,
    valid: true,
    invariants: assertTimelineInvariants,
  },
  {
    path: "fixtures/valid/audio-clip.json",
    schema: schemas[7].$id,
    valid: true,
    invariants: assertAudioClipInvariants,
  },
  {
    path: "fixtures/valid/audio-clip-sample-locked.json",
    schema: schemas[7].$id,
    valid: true,
    invariants: assertAudioClipInvariants,
  },
  {
    path: "fixtures/valid/audio-clip-musical-version-2.json",
    schema: schemas[7].$id,
    valid: true,
    invariants: assertAudioClipInvariants,
  },
  {
    path: "fixtures/valid/scene.json",
    schema: schemas[8].$id,
    valid: true,
    invariants: assertSceneInvariants,
  },
  {
    path: "fixtures/valid/arrangement.json",
    schema: schemas[9].$id,
    valid: true,
    invariants: assertArrangementInvariants,
  },
  {
    path: "fixtures/valid/mixer.json",
    schema: schemas[10].$id,
    valid: true,
    invariants: assertMixerInvariants,
  },
  {
    path: "fixtures/valid/control-mappings.json",
    schema: schemas[11].$id,
    valid: true,
    invariants: assertControlMappingInvariants,
  },
  {
    path: "fixtures/valid/recording-metadata.json",
    schema: schemas[12].$id,
    valid: true,
    invariants: assertRecordingMetadataInvariants,
  },
  {
    path: "fixtures/valid/automation-take.json",
    schema: schemas[13].$id,
    valid: true,
    invariants: assertAutomationTakeInvariants,
  },
  {
    path: "fixtures/valid/deployment-state.json",
    schema: schemas[14].$id,
    valid: true,
  },
  {
    path: "fixtures/valid/deployment-storage-policy.json",
    schema: schemas[15].$id,
    valid: true,
  },
  {
    path: "fixtures/valid/deployment-lock.json",
    schema: schemas[16].$id,
    valid: true,
  },
  {
    path: "fixtures/valid/deployment-pairing-request.json",
    schema: schemas[17].$id,
    valid: true,
  },
  {
    path: "fixtures/valid/deployment-request.json",
    schema: schemas[18].$id,
    valid: true,
  },
  {
    path: "fixtures/valid/deployment-authorization-state.json",
    schema: schemas[19].$id,
    valid: true,
  },
  {
    path: "fixtures/valid/deployment-authorization-terminal-state.json",
    schema: schemas[19].$id,
    valid: true,
  },
  {
    path: "fixtures/valid/timing-simulator-profile.json",
    schema: schemas[20].$id,
    valid: true,
    invariants: assertTimingSimulatorProfile,
  },
  {
    path: "fixtures/valid/timing-frame.json",
    schema: schemas[21].$id,
    valid: true,
    invariants: assertTimingFrameInvariants,
  },
  {
    path: "fixtures/invalid/hardware-capabilities-extra-property.json",
    schema: schemas[1].$id,
    valid: false,
  },
  {
    path: "fixtures/invalid/project-path-traversal.json",
    schema: schemas[2].$id,
    valid: false,
  },
  {
    path: "fixtures/invalid/project-unknown-version.json",
    schema: schemas[2].$id,
    valid: false,
  },
  {
    path: "fixtures/invalid/project-missing-timeline.json",
    schema: schemas[2].$id,
    valid: false,
  },
  {
    path: "fixtures/invalid/runtime-duplicate-path.json",
    schema: schemas[3].$id,
    valid: false,
    invariants: assertRuntimeInvariants,
  },
  {
    path: "fixtures/invalid/runtime-build-plan-wrong-media-type.json",
    schema: schemas[4].$id,
    valid: false,
  },
  {
    path: "fixtures/invalid/track-unknown-property.json",
    schema: schemas[5].$id,
    valid: false,
  },
  {
    path: "fixtures/invalid/timeline-unsorted-tempo.json",
    schema: schemas[6].$id,
    valid: false,
    invariants: assertTimelineInvariants,
  },
  {
    path: "fixtures/invalid/timeline-nonzero-origin.json",
    schema: schemas[6].$id,
    valid: false,
    invariants: assertTimelineInvariants,
  },
  {
    path: "fixtures/invalid/timeline-tempo-ramp.json",
    schema: schemas[6].$id,
    valid: false,
  },
  {
    path: "fixtures/invalid/audio-clip-invalid-loop.json",
    schema: schemas[7].$id,
    valid: false,
    invariants: assertAudioClipInvariants,
  },
  {
    path: "fixtures/invalid/audio-clip-invalid-warp.json",
    schema: schemas[7].$id,
    valid: false,
    invariants: assertAudioClipInvariants,
  },
  {
    path: "fixtures/invalid/audio-clip-version-1-timebase.json",
    schema: schemas[7].$id,
    valid: false,
    invariants: assertAudioClipInvariants,
  },
  {
    path: "fixtures/invalid/audio-clip-version-2-without-timebase.json",
    schema: schemas[7].$id,
    valid: false,
    invariants: assertAudioClipInvariants,
  },
  {
    path: "fixtures/invalid/audio-clip-sample-locked-loop.json",
    schema: schemas[7].$id,
    valid: false,
    invariants: assertAudioClipInvariants,
  },
  {
    path: "fixtures/invalid/audio-clip-sample-locked-fades.json",
    schema: schemas[7].$id,
    valid: false,
    invariants: assertAudioClipInvariants,
  },
  {
    path: "fixtures/invalid/audio-clip-sample-locked-warp.json",
    schema: schemas[7].$id,
    valid: false,
    invariants: assertAudioClipInvariants,
  },
  {
    path: "fixtures/invalid/scene-duplicate-track.json",
    schema: schemas[8].$id,
    valid: false,
    invariants: assertSceneInvariants,
  },
  {
    path: "fixtures/invalid/scene-ambiguous-stop.json",
    schema: schemas[8].$id,
    valid: false,
  },
  {
    path: "fixtures/invalid/arrangement-overlapping-placement.json",
    schema: schemas[9].$id,
    valid: false,
    invariants: assertArrangementInvariants,
  },
  {
    path: "fixtures/invalid/arrangement-unsorted-launch.json",
    schema: schemas[9].$id,
    valid: false,
    invariants: assertArrangementInvariants,
  },
  {
    path: "fixtures/invalid/mixer-routing-cycle.json",
    schema: schemas[10].$id,
    valid: false,
    invariants: assertMixerInvariants,
  },
  {
    path: "fixtures/invalid/mixer-duplicate-send-bus.json",
    schema: schemas[10].$id,
    valid: false,
    invariants: assertMixerInvariants,
  },
  {
    path: "fixtures/invalid/control-mappings-action-linear.json",
    schema: schemas[11].$id,
    valid: false,
    invariants: assertControlMappingInvariants,
  },
  {
    path: "fixtures/invalid/recording-metadata-channel-mismatch.json",
    schema: schemas[12].$id,
    valid: false,
    invariants: assertRecordingMetadataInvariants,
  },
  {
    path: "fixtures/invalid/automation-take-boolean-linear.json",
    schema: schemas[13].$id,
    valid: false,
    invariants: assertAutomationTakeInvariants,
  },
  {
    path: "fixtures/invalid/deployment-state-zero-generation.json",
    schema: schemas[14].$id,
    valid: false,
  },
  {
    path: "fixtures/invalid/deployment-storage-policy-insufficient-lifecycle-slots.json",
    schema: schemas[15].$id,
    valid: false,
  },
  {
    path: "fixtures/invalid/deployment-lock-zero-pid.json",
    schema: schemas[16].$id,
    valid: false,
  },
  {
    path: "fixtures/invalid/deployment-pairing-request-padded-signature.json",
    schema: schemas[17].$id,
    valid: false,
  },
  {
    path: "fixtures/invalid/deployment-pairing-request-private-key.json",
    schema: schemas[17].$id,
    valid: false,
  },
  {
    path: "fixtures/invalid/deployment-request-zero-sequence.json",
    schema: schemas[18].$id,
    valid: false,
  },
  {
    path: "fixtures/invalid/deployment-authorization-state-short-lifetime.json",
    schema: schemas[19].$id,
    valid: false,
  },
  {
    path: "fixtures/invalid/timing-simulator-profile-batch-exceeds-queue.json",
    schema: schemas[20].$id,
    valid: false,
    invariants: assertTimingSimulatorProfile,
  },
  {
    path: "fixtures/invalid/timing-frame-unknown-version.json",
    schema: schemas[21].$id,
    valid: false,
  },
  {
    path: "fixtures/invalid/timing-frame-unsorted-events.json",
    schema: schemas[21].$id,
    valid: false,
    invariants: assertTimingFrameInvariants,
  },
  {
    path: "fixtures/valid/finalized-recording-handoff.json",
    schema: schemas[22].$id,
    valid: true,
  },
  {
    path: "fixtures/invalid/finalized-recording-handoff-extra-property.json",
    schema: schemas[22].$id,
    valid: false,
  },
];

for (const testCase of cases) {
  const value = await loadJson(testCase.path);
  const validate = ajv.getSchema(testCase.schema);
  assert(validate, `schema is registered: ${testCase.schema}`);
  let valid = validate(value);
  if (valid && testCase.invariants) {
    try {
      testCase.invariants(value);
    } catch {
      valid = false;
    }
  }
  assert.equal(
    valid,
    testCase.valid,
    `${testCase.path}: ${JSON.stringify(validate.errors)}`,
  );
}

const rfcInput = {
  numbers: [333333333.33333329, 1e30, 4.5, 2e-3, 1e-27],
  string: '€$\u000f\nA\'B"\\\\"/',
  literals: [null, true, false],
};
const rfcExpected =
  '{"literals":[null,true,false],"numbers":[333333333.3333333,1e+30,4.5,0.002,1e-27],"string":"€$\\u000f\\nA\'B\\"\\\\\\\\\\"/"}';
assert.equal(canonicalize(rfcInput), rfcExpected, "RFC 8785 sample vector");

const orderA = { z: 1, nested: { beta: true, alpha: false }, a: 2 };
const orderB = { a: 2, nested: { alpha: false, beta: true }, z: 1 };
assert.equal(
  canonicalize(orderA),
  canonicalize(orderB),
  "object member order must not change canonical bytes",
);

const project = await loadJson("fixtures/valid/project.json");
const roundTripped = strictParse(canonicalize(project));
assert.deepEqual(
  roundTripped.extensions,
  project.extensions,
  "unknown namespaced extensions must survive canonical round trip",
);

assert.throws(
  () => strictParse('{"format":1,"format":2}'),
  /duplicate/i,
  "duplicate object names must be rejected",
);
assert.throws(
  () => assertIJson(JSON.parse('{"value":-0}'), "$"),
  /negative zero/,
  "negative zero must be rejected",
);
assert.throws(
  () => assertIJson(JSON.parse('{"value":9007199254740992}'), "$"),
  /exactly interoperable/,
  "unsafe integers must be rejected",
);
assert.throws(
  () => assertIJson(JSON.parse('{"value":"\\ud800"}'), "$"),
  /unpaired high surrogate/,
  "unpaired Unicode surrogates must be rejected",
);

console.log(`Contract verification passed: ${cases.length} fixtures`);
