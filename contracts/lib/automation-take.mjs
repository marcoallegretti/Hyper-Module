import assert from "node:assert/strict";

export function automationTargetKey(target) {
  if (target.kind === "mixer-channel") {
    return `channel:${target.track_id}:${target.parameter}`;
  }
  if (target.kind === "mixer-send") {
    return `send:${target.send_id}:${target.parameter}`;
  }
  return `${target.kind}:${target.parameter}`;
}

function targetDomain(target) {
  return ["mute", "solo", "enabled"].includes(target.parameter)
    ? "boolean"
    : "integer";
}

function targetBounds(target) {
  return target.parameter === "pan" ? [-1_000_000, 1_000_000] : [0, 2_000_000];
}

export function assertAutomationTakeInvariants(take) {
  assert(
    take !== null && typeof take === "object" && !Array.isArray(take),
    "automation take must be an object",
  );
  assert(
    Number.isSafeInteger(take.start_tick) &&
      Number.isSafeInteger(take.end_tick) &&
      take.start_tick < take.end_tick,
    "automation take must have a nonempty half-open interval",
  );
  const capturedAt = new Date(take.captured_at_utc);
  assert(
    !Number.isNaN(capturedAt.valueOf()) &&
      capturedAt.toISOString() === take.captured_at_utc,
    "automation capture time must be a canonical UTC instant",
  );
  assert(Array.isArray(take.lanes), "automation lanes must be an array");

  const laneIds = take.lanes.map((lane) => lane.id);
  assert.equal(
    new Set(laneIds).size,
    laneIds.length,
    "automation lane identifiers must be unique",
  );
  for (let index = 1; index < laneIds.length; index += 1) {
    assert(
      laneIds[index - 1] < laneIds[index],
      "automation lanes must be sorted by identifier",
    );
  }
  const targetKeys = take.lanes.map((lane) => automationTargetKey(lane.target));
  assert.equal(
    new Set(targetKeys).size,
    targetKeys.length,
    "automation targets must be unique within a take",
  );

  for (const lane of take.lanes) {
    assert(Array.isArray(lane.points), "automation points must be an array");
    const domain = targetDomain(lane.target);
    if (domain === "boolean") {
      assert.equal(
        lane.interpolation,
        "hold",
        "Boolean automation requires hold interpolation",
      );
    }
    const [minimum, maximum] =
      domain === "integer" ? targetBounds(lane.target) : [undefined, undefined];
    for (let index = 0; index < lane.points.length; index += 1) {
      const point = lane.points[index];
      assert(
        point.tick >= take.start_tick && point.tick < take.end_tick,
        "automation point must lie within the take interval",
      );
      if (index > 0) {
        assert(
          lane.points[index - 1].tick < point.tick,
          "automation points must be strictly ordered by tick",
        );
      }
      if (domain === "boolean") {
        assert.equal(
          typeof point.value,
          "boolean",
          "Boolean automation requires Boolean points",
        );
      } else {
        assert(
          Number.isSafeInteger(point.value) &&
            point.value >= minimum &&
            point.value <= maximum,
          "automation point exceeds the target value domain",
        );
      }
    }
  }
}

export function automationValueAt(lane, tick) {
  assert(
    Number.isSafeInteger(tick) && tick >= 0,
    "automation evaluation tick must be a nonnegative safe integer",
  );
  assert(
    Array.isArray(lane.points) && lane.points.length > 0,
    "automation lane must contain points",
  );
  if (tick <= lane.points[0].tick) return lane.points[0].value;

  for (let index = 1; index < lane.points.length; index += 1) {
    const right = lane.points[index];
    if (tick < right.tick) {
      const left = lane.points[index - 1];
      if (lane.interpolation === "hold") return left.value;
      assert(
        Number.isSafeInteger(left.value) && Number.isSafeInteger(right.value),
        "linear automation requires integer points",
      );
      const elapsed = BigInt(tick - left.tick);
      const duration = BigInt(right.tick - left.tick);
      return Number(
        BigInt(left.value) +
          (BigInt(right.value - left.value) * elapsed) / duration,
      );
    }
  }
  return lane.points.at(-1).value;
}
