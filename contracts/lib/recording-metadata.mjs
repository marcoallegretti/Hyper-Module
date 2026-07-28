import assert from "node:assert/strict";

export function assertRecordingMetadataInvariants(metadata) {
  assert(
    metadata !== null &&
      typeof metadata === "object" &&
      !Array.isArray(metadata),
    "recording metadata must be an object",
  );
  assert(
    Array.isArray(metadata.source_input_indices),
    "recording source inputs must be an array",
  );
  assert.equal(
    new Set(metadata.source_input_indices).size,
    metadata.source_input_indices.length,
    "recording source inputs must be unique",
  );
  assert.equal(
    metadata.source_input_indices.length,
    metadata.channels,
    "recording source input count must match channel count",
  );

  const capturedAt = new Date(metadata.captured_at_utc);
  assert(
    !Number.isNaN(capturedAt.valueOf()) &&
      capturedAt.toISOString() === metadata.captured_at_utc,
    "recording capture time must be a canonical UTC instant",
  );
}
