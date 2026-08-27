import assert from "node:assert/strict";

const MICROSECONDS_PER_SECOND = 1_000_000n;
const MAX_SAFE_INTEGER = BigInt(Number.MAX_SAFE_INTEGER);
const METER_DENOMINATORS = new Set([1, 2, 4, 8, 16, 32, 64, 128, 256]);
const ENTITY_ID_PATTERN = /^[0-9a-f]{8}-[0-9a-f]{4}-[0-9a-f]{4}-[0-9a-f]{4}-[0-9a-f]{12}$/;
const MAX_TIMELINE_MARKERS = 65_536;
const MAX_MARKER_NAME_CODE_POINTS = 120;

function assertSafeNonnegativeInteger(value, label) {
  assert(
    Number.isSafeInteger(value) && value >= 0,
    `${label} must be a nonnegative safe integer`,
  );
}

function assertStrictlyIncreasingFromZero(events, label) {
  assert(events.length > 0, `${label} must not be empty`);
  assert.equal(events[0].tick, 0, `${label} must begin at tick zero`);
  for (let index = 1; index < events.length; index += 1) {
    assert(
      events[index].tick > events[index - 1].tick,
      `${label} ticks must be strictly increasing`,
    );
  }
}

export function assertTimelineInvariants(timeline) {
  assert(
    timeline !== null &&
      typeof timeline === "object" &&
      !Array.isArray(timeline),
    "timeline must be an object",
  );
  assert.equal(
    timeline.ticks_per_quarter,
    1920,
    "timeline resolution must be 1920 ticks per quarter",
  );
  assert(Array.isArray(timeline.tempo_events), "tempo events must be an array");
  assert(Array.isArray(timeline.meter_events), "meter events must be an array");
  const markers = timeline.markers ?? [];
  assert(Array.isArray(markers), "timeline markers must be an array when present");
  assert(
    markers.length <= MAX_TIMELINE_MARKERS,
    "timeline markers exceed the version 1 limit",
  );
  for (const event of timeline.tempo_events) {
    assertSafeNonnegativeInteger(event.tick, "tempo event tick");
    assert(
      Number.isInteger(event.microseconds_per_quarter) &&
        event.microseconds_per_quarter >= 1 &&
        event.microseconds_per_quarter <= 16_777_215,
      "tempo must be an integer from 1 through 16777215 microseconds per quarter",
    );
  }
  for (const event of timeline.meter_events) {
    assertSafeNonnegativeInteger(event.tick, "meter event tick");
    assert(
      Number.isInteger(event.numerator) &&
        event.numerator >= 1 &&
        event.numerator <= 255,
      "meter numerator must be an integer from 1 through 255",
    );
    assert(
      METER_DENOMINATORS.has(event.denominator),
      "meter denominator must be a supported power of two",
    );
  }
  assertStrictlyIncreasingFromZero(timeline.tempo_events, "tempo events");
  assertStrictlyIncreasingFromZero(timeline.meter_events, "meter events");

  const markerIds = new Set();
  for (const [index, marker] of markers.entries()) {
    assert(
      marker !== null && typeof marker === "object" && !Array.isArray(marker),
      "timeline marker must be an object",
    );
    assert(
      typeof marker.id === "string" && ENTITY_ID_PATTERN.test(marker.id),
      "timeline marker identifier is outside the version 1 domain",
    );
    assertSafeNonnegativeInteger(marker.tick, "timeline marker tick");
    assert(typeof marker.name === "string", "timeline marker name must be a string");
    const nameLength = Array.from(marker.name).length;
    assert(
      nameLength >= 1 && nameLength <= MAX_MARKER_NAME_CODE_POINTS,
      "timeline marker name is outside the version 1 domain",
    );
    assert(
      !markerIds.has(marker.id),
      "timeline marker identifiers must be unique",
    );
    markerIds.add(marker.id);
    if (index > 0) {
      const previous = markers[index - 1];
      assert(
        previous.tick < marker.tick ||
          (previous.tick === marker.tick && previous.id < marker.id),
        "timeline markers must be ordered by tick and identifier",
      );
    }
  }
}

function exactFrameNumeratorAtTick(timeline, tick, sampleRateHz) {
  const denominator =
    BigInt(timeline.ticks_per_quarter) * MICROSECONDS_PER_SECOND;
  const sampleRate = BigInt(sampleRateHz);
  let numerator = 0n;
  let cursorTick = 0;
  let tempoIndex = 0;

  while (
    tempoIndex + 1 < timeline.tempo_events.length &&
    timeline.tempo_events[tempoIndex + 1].tick <= tick
  ) {
    const current = timeline.tempo_events[tempoIndex];
    const next = timeline.tempo_events[tempoIndex + 1];
    numerator +=
      BigInt(next.tick - cursorTick) *
      BigInt(current.microseconds_per_quarter) *
      sampleRate;
    cursorTick = next.tick;
    tempoIndex += 1;
  }

  const current = timeline.tempo_events[tempoIndex];
  numerator +=
    BigInt(tick - cursorTick) *
    BigInt(current.microseconds_per_quarter) *
    sampleRate;
  return { denominator, numerator };
}

function safeNumber(value, label) {
  assert(
    value <= MAX_SAFE_INTEGER,
    `${label} exceeds the JSON safe-integer domain`,
  );
  return Number(value);
}

export function tickToFrame(timeline, tick, sampleRateHz = 48000) {
  assertTimelineInvariants(timeline);
  assertSafeNonnegativeInteger(tick, "tick");
  assertSafeNonnegativeInteger(sampleRateHz, "sample rate");
  assert(sampleRateHz > 0, "sample rate must be positive");
  const { denominator, numerator } = exactFrameNumeratorAtTick(
    timeline,
    tick,
    sampleRateHz,
  );
  return safeNumber(numerator / denominator, "frame");
}

export function frameToTick(timeline, frame, sampleRateHz = 48000) {
  assertTimelineInvariants(timeline);
  assertSafeNonnegativeInteger(frame, "frame");
  assertSafeNonnegativeInteger(sampleRateHz, "sample rate");
  assert(sampleRateHz > 0, "sample rate must be positive");

  const denominator =
    BigInt(timeline.ticks_per_quarter) * MICROSECONDS_PER_SECOND;
  const target = BigInt(frame) * denominator;
  const sampleRate = BigInt(sampleRateHz);
  let numerator = 0n;
  let cursorTick = 0;

  for (let index = 0; index < timeline.tempo_events.length; index += 1) {
    const current = timeline.tempo_events[index];
    const unit = BigInt(current.microseconds_per_quarter) * sampleRate;
    const next = timeline.tempo_events[index + 1];
    if (next === undefined) {
      return safeNumber(
        BigInt(cursorTick) + (target - numerator) / unit,
        "tick",
      );
    }
    const segment = BigInt(next.tick - cursorTick) * unit;
    if (target < numerator + segment) {
      return safeNumber(
        BigInt(cursorTick) + (target - numerator) / unit,
        "tick",
      );
    }
    numerator += segment;
    cursorTick = next.tick;
  }

  throw new Error("timeline must contain at least one tempo event");
}
