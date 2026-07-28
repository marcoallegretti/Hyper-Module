import assert from "node:assert/strict";

const MAX_SAFE_INTEGER = BigInt(Number.MAX_SAFE_INTEGER);

function assertSafeNonnegativeInteger(value, label) {
  assert(
    Number.isSafeInteger(value) && value >= 0,
    `${label} must be a nonnegative safe integer`,
  );
}

export function assertSceneInvariants(scene) {
  assert(
    scene !== null && typeof scene === "object" && !Array.isArray(scene),
    "scene must be an object",
  );
  assert(Array.isArray(scene.slots), "scene slots must be an array");
  const trackIds = scene.slots.map((slot) => slot.track_id);
  assert.equal(
    new Set(trackIds).size,
    trackIds.length,
    "scene slot track identifiers must be unique",
  );
  if (scene.launch_quantization.mode === "grid") {
    assertSafeNonnegativeInteger(
      scene.launch_quantization.quantum_ticks,
      "scene launch quantum",
    );
    assert(
      scene.launch_quantization.quantum_ticks > 0,
      "scene launch quantum must be positive",
    );
  } else {
    assert.equal(
      scene.launch_quantization.mode,
      "immediate",
      "scene launch quantization mode is unsupported",
    );
  }
}

export function nextSceneLaunchTick(scene, requestTick) {
  assertSceneInvariants(scene);
  assertSafeNonnegativeInteger(requestTick, "scene launch request tick");
  if (scene.launch_quantization.mode === "immediate") {
    return requestTick;
  }
  const request = BigInt(requestTick);
  const quantum = BigInt(scene.launch_quantization.quantum_ticks);
  const launch = (request / quantum + 1n) * quantum;
  assert(
    launch <= MAX_SAFE_INTEGER,
    "scene launch tick exceeds the safe-integer domain",
  );
  return Number(launch);
}
