import assert from "node:assert/strict";

import { assertAudioClipInvariants } from "./audio-clip.mjs";

const MAX_SAFE_INTEGER = Number.MAX_SAFE_INTEGER;
const MAX_CONCURRENT_PLACEMENTS_PER_TRACK = 2;

function assertSafeNonnegativeInteger(value, label) {
  assert(
    Number.isSafeInteger(value) && value >= 0,
    `${label} must be a nonnegative safe integer`,
  );
}

function comparePlacements(left, right) {
  const trackOrder =
    left.track_id === right.track_id
      ? 0
      : left.track_id < right.track_id
        ? -1
        : 1;
  const identifierOrder =
    left.id === right.id ? 0 : left.id < right.id ? -1 : 1;
  return left.start_tick - right.start_tick || trackOrder || identifierOrder;
}

export function assertArrangementInvariants(arrangement) {
  assert(
    arrangement !== null &&
      typeof arrangement === "object" &&
      !Array.isArray(arrangement),
    "arrangement must be an object",
  );
  assert(
    Array.isArray(arrangement.placements),
    "arrangement placements must be an array",
  );
  assert(
    Array.isArray(arrangement.scene_launches),
    "arrangement scene launches must be an array",
  );

  const entityIds = [
    ...arrangement.placements.map((placement) => placement.id),
    ...arrangement.scene_launches.map((launch) => launch.id),
  ];
  assert.equal(
    new Set(entityIds).size,
    entityIds.length,
    "arrangement event identifiers must be unique",
  );

  for (const placement of arrangement.placements) {
    assertSafeNonnegativeInteger(placement.start_tick, "placement start");
    assertSafeNonnegativeInteger(
      placement.clip_start_tick,
      "placement clip start",
    );
    assertSafeNonnegativeInteger(
      placement.duration_ticks,
      "placement duration",
    );
    assert(placement.duration_ticks > 0, "placement duration must be positive");
    assert(
      placement.start_tick <= MAX_SAFE_INTEGER - placement.duration_ticks,
      "placement end exceeds the safe-integer domain",
    );
    assert(
      placement.clip_start_tick <= MAX_SAFE_INTEGER - placement.duration_ticks,
      "placement clip range exceeds the safe-integer domain",
    );
  }

  for (let index = 1; index < arrangement.placements.length; index += 1) {
    assert(
      comparePlacements(
        arrangement.placements[index - 1],
        arrangement.placements[index],
      ) < 0,
      "arrangement placements must be sorted by start tick, track, and identifier",
    );
  }

  const trackEnds = new Map();
  for (const placement of arrangement.placements) {
    const activeEnds = (trackEnds.get(placement.track_id) ?? []).filter(
      (endTick) => endTick > placement.start_tick,
    );
    assert(
      activeEnds.length < MAX_CONCURRENT_PLACEMENTS_PER_TRACK,
      `placements must not exceed the ${MAX_CONCURRENT_PLACEMENTS_PER_TRACK}-voice track overlap limit`,
    );
    activeEnds.push(placement.start_tick + placement.duration_ticks);
    trackEnds.set(placement.track_id, activeEnds);
  }

  let previousLaunchTick = -1;
  for (const launch of arrangement.scene_launches) {
    assertSafeNonnegativeInteger(launch.tick, "scene launch tick");
    assert(
      launch.tick > previousLaunchTick,
      "scene launch ticks must be strictly increasing",
    );
    previousLaunchTick = launch.tick;
  }
}

export function placementClipTick(placement, clip, arrangementTick) {
  assertAudioClipInvariants(clip);
  assertSafeNonnegativeInteger(placement.start_tick, "placement start");
  assertSafeNonnegativeInteger(placement.duration_ticks, "placement duration");
  assertSafeNonnegativeInteger(
    placement.clip_start_tick,
    "placement clip start",
  );
  assertSafeNonnegativeInteger(arrangementTick, "arrangement tick");
  assert(
    arrangementTick >= placement.start_tick &&
      arrangementTick - placement.start_tick < placement.duration_ticks,
    "arrangement tick must lie inside the placement",
  );

  const localTick =
    BigInt(placement.clip_start_tick) +
    BigInt(arrangementTick - placement.start_tick);
  if (!clip.loop.enabled || localTick < BigInt(clip.loop.end_tick)) {
    assert(
      localTick < BigInt(clip.duration_ticks),
      "non-looping placement exceeds clip duration",
    );
    return Number(localTick);
  }

  const loopStart = BigInt(clip.loop.start_tick);
  const loopEnd = BigInt(clip.loop.end_tick);
  return Number(loopStart + ((localTick - loopEnd) % (loopEnd - loopStart)));
}
