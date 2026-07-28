import assert from "node:assert/strict";
import { readFile } from "node:fs/promises";
import { dirname, resolve } from "node:path";
import { fileURLToPath } from "node:url";

import { assertRecordingMetadataInvariants } from "../lib/recording-metadata.mjs";
import { strictParse } from "../lib/strict-json.mjs";

const root = resolve(dirname(fileURLToPath(import.meta.url)), "..");
const metadata = strictParse(
  await readFile(
    resolve(root, "fixtures/valid/recording-metadata.json"),
    "utf8",
  ),
);

assertRecordingMetadataInvariants(metadata);
assert.throws(
  () =>
    assertRecordingMetadataInvariants({
      ...metadata,
      captured_at_utc: "2026-02-29T12:00:00.000Z",
    }),
  /canonical UTC instant/,
);
assert.throws(
  () =>
    assertRecordingMetadataInvariants({
      ...metadata,
      source_input_indices: [0],
    }),
  /input count must match channel count/,
);
assert.throws(
  () =>
    assertRecordingMetadataInvariants({
      ...metadata,
      source_input_indices: [0, 0],
    }),
  /source inputs must be unique/,
);

console.log("Recording metadata invariants passed: 4 deterministic cases");
