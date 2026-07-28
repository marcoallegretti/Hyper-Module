import assert from "node:assert/strict";
import { createHash } from "node:crypto";
import { readFile } from "node:fs/promises";
import { join } from "node:path";

import { verifyBundle } from "../scripts/verify-bundle.mjs";

export async function inspectDeploymentBundle(bundlePath) {
  const manifest = await verifyBundle(bundlePath);
  const manifestBytes = await readFile(join(bundlePath, "manifest.json"));
  const byteLength = manifest.files.reduce((total, file) => {
    const next = total + file.byte_length;
    assert(
      Number.isSafeInteger(next),
      "deployment bundle byte accounting exceeds safe integer range",
    );
    return next;
  }, manifestBytes.byteLength);
  return {
    manifest,
    bundle_sha256: createHash("sha256").update(manifestBytes).digest("hex"),
    bundle_byte_length: byteLength,
  };
}
