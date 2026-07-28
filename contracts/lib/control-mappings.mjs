import assert from "node:assert/strict";

function sourceScopeKey(binding) {
  const source =
    binding.source.kind === "cv-input"
      ? `cv:${binding.source.input_index}`
      : `${binding.source.kind}:${binding.source.edge}`;
  const scope =
    binding.scope.kind === "track"
      ? `track:${binding.scope.track_id}`
      : binding.scope.kind === "scene"
        ? `scene:${binding.scope.scene_id}`
        : binding.scope.kind;
  return `${source}|${scope}`;
}

function targetDomain(target) {
  if (target.kind === "scene" || target.kind === "transport") return "action";
  if (["mute", "solo", "enabled"].includes(target.parameter)) return "boolean";
  return "continuous";
}

export function assertControlMappingInvariants(mappingSet) {
  assert(
    mappingSet !== null &&
      typeof mappingSet === "object" &&
      !Array.isArray(mappingSet),
    "control mappings must be an object",
  );
  assert(Array.isArray(mappingSet.bindings), "bindings must be an array");
  const ids = mappingSet.bindings.map((binding) => binding.id);
  assert.equal(
    new Set(ids).size,
    ids.length,
    "binding identifiers must be unique",
  );
  for (let index = 1; index < ids.length; index += 1) {
    assert(
      ids[index - 1] < ids[index],
      "bindings must be sorted by identifier",
    );
  }
  const keys = mappingSet.bindings.map(sourceScopeKey);
  assert.equal(
    new Set(keys).size,
    keys.length,
    "source and scope combinations must be unique",
  );

  for (const binding of mappingSet.bindings) {
    const domain = targetDomain(binding.target);
    if (binding.source.kind === "cv-input") {
      assert.equal(
        binding.transform.kind,
        "linear",
        "CV sources require a linear transform",
      );
      assert.equal(
        domain,
        "continuous",
        "CV sources require a continuous target",
      );
      assert(
        binding.transform.input_min < binding.transform.input_max,
        "linear input minimum must precede input maximum",
      );
    } else {
      assert.equal(
        binding.transform.kind,
        "trigger",
        "edge sources require a trigger transform",
      );
      if (domain === "action") {
        assert.equal(
          binding.transform.behavior,
          "invoke",
          "action targets require invoke behavior",
        );
      } else {
        assert.equal(
          domain,
          "boolean",
          "edge sources require a boolean or action target",
        );
        assert.notEqual(
          binding.transform.behavior,
          "invoke",
          "boolean targets require toggle or momentary behavior",
        );
      }
    }
  }
}

export function mapLinearControl(transform, input) {
  assert(Number.isInteger(input), "control input must be an integer");
  assert(
    transform.input_min < transform.input_max,
    "linear input minimum must precede input maximum",
  );
  const clamped = Math.max(
    transform.input_min,
    Math.min(transform.input_max, input),
  );
  const numerator =
    BigInt(clamped - transform.input_min) *
    BigInt(transform.output_max - transform.output_min);
  const denominator = BigInt(transform.input_max - transform.input_min);
  return Number(BigInt(transform.output_min) + numerator / denominator);
}

export function normalizedTargetValue(target, normalized) {
  assert(
    Number.isInteger(normalized) && normalized >= 0 && normalized <= 1_000_000,
    "normalized target value must be from 0 through 1000000",
  );
  if (target.parameter === "gain") return normalized * 2;
  if (target.parameter === "pan") return normalized * 2 - 1_000_000;
  throw new Error("target is not continuous");
}
