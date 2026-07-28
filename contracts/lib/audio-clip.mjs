import assert from "node:assert/strict";

const MAX_SAFE_INTEGER = Number.MAX_SAFE_INTEGER;

function assertSafeNonnegativeInteger(value, label) {
  assert(
    Number.isSafeInteger(value) && value >= 0,
    `${label} must be a nonnegative safe integer`,
  );
}

function assertStrictlyIncreasing(markers, property, label) {
  for (let index = 1; index < markers.length; index += 1) {
    assert(
      markers[index][property] > markers[index - 1][property],
      `${label} must be strictly increasing`,
    );
  }
}

export function assertAudioClipInvariants(clip) {
  assert(
    clip !== null && typeof clip === "object" && !Array.isArray(clip),
    "audio clip must be an object",
  );
  assertSafeNonnegativeInteger(clip.duration_ticks, "clip duration");
  assert(clip.duration_ticks > 0, "clip duration must be positive");

  assertSafeNonnegativeInteger(clip.source.start_frame, "source start frame");
  assertSafeNonnegativeInteger(clip.source.frame_count, "source frame count");
  assert(clip.source.frame_count > 0, "source frame count must be positive");
  assert(
    clip.source.start_frame <= MAX_SAFE_INTEGER - clip.source.frame_count,
    "source frame range exceeds the safe-integer domain",
  );

  assertSafeNonnegativeInteger(clip.loop.start_tick, "loop start");
  assertSafeNonnegativeInteger(clip.loop.end_tick, "loop end");
  assert(
    clip.loop.start_tick < clip.loop.end_tick,
    "loop start must precede loop end",
  );
  assert(
    clip.loop.end_tick <= clip.duration_ticks,
    "loop end must not exceed clip duration",
  );

  assertSafeNonnegativeInteger(clip.fades.in_ticks, "fade-in duration");
  assertSafeNonnegativeInteger(clip.fades.out_ticks, "fade-out duration");
  assert.equal(
    clip.fades.curve,
    "linear-amplitude",
    "version 1 fade curve must be linear-amplitude",
  );
  assert(
    clip.fades.in_ticks <= clip.duration_ticks - clip.fades.out_ticks,
    "combined fades must not exceed clip duration",
  );
  assert(
    Number.isFinite(clip.playback.gain_decibels),
    "clip gain must be finite",
  );
  assert(
    Number.isSafeInteger(clip.playback.transpose_cents),
    "clip transpose must be a safe integer number of cents",
  );

  assert(
    Array.isArray(clip.warp_markers) && clip.warp_markers.length >= 2,
    "audio clip must contain at least two warp markers",
  );
  for (const marker of clip.warp_markers) {
    assertSafeNonnegativeInteger(marker.tick, "warp-marker tick");
    assertSafeNonnegativeInteger(
      marker.source_frame_offset,
      "warp-marker source frame offset",
    );
  }
  assert.equal(
    clip.warp_markers[0].tick,
    0,
    "first warp marker must begin at tick zero",
  );
  assert.equal(
    clip.warp_markers[0].source_frame_offset,
    0,
    "first warp marker must begin at source-frame offset zero",
  );
  const lastMarker = clip.warp_markers.at(-1);
  assert.equal(
    lastMarker.tick,
    clip.duration_ticks,
    "last warp marker must equal clip duration",
  );
  assert.equal(
    lastMarker.source_frame_offset,
    clip.source.frame_count,
    "last warp marker must equal source frame count",
  );
  assertStrictlyIncreasing(clip.warp_markers, "tick", "warp-marker ticks");
  assertStrictlyIncreasing(
    clip.warp_markers,
    "source_frame_offset",
    "warp-marker source frame offsets",
  );
}
