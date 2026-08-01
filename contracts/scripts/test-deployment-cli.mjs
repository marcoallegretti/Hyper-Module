import assert from "node:assert/strict";
import { mkdtemp, mkdir, readdir, rm, writeFile } from "node:fs/promises";
import { tmpdir } from "node:os";
import { join, resolve } from "node:path";

import { runNativeProcess } from "../lib/native-process.mjs";

const metadataLimit = 64 * 1024;
const authorizationLimit = 1024 * 1024;
const temporaryRoot = await mkdtemp(join(tmpdir(), "hyper-deployment-cli-"));
const target = join(temporaryRoot, "target");
const command = resolve(import.meta.dirname, "deployment-target.mjs");

function paddedJson(byteLength) {
  assert(byteLength >= 2);
  return Buffer.from(`{}${" ".repeat(byteLength - 2)}`, "utf8");
}

async function invoke(arguments_) {
  return runNativeProcess(process.execPath, [command, ...arguments_], {
    timeoutMs: 30_000,
  });
}

async function assertBoundary(
  name,
  limit,
  argumentsForPath,
  exactError,
  oversizedError,
) {
  const exact = join(temporaryRoot, `${name}-exact.json`);
  const oversized = join(temporaryRoot, `${name}-oversized.json`);
  await writeFile(exact, paddedJson(limit));
  await writeFile(oversized, paddedJson(limit + 1));
  await assert.rejects(invoke(argumentsForPath(exact)), exactError);
  await assert.rejects(invoke(argumentsForPath(oversized)), oversizedError);
}

try {
  await mkdir(target);
  await assertBoundary(
    "policy",
    metadataLimit,
    (path) => ["configure-storage", target, path],
    /schema validation failed/,
    /deployment storage policy input exceeds 65536 bytes/,
  );
  await assertBoundary(
    "authorization",
    authorizationLimit,
    (path) => ["configure-authorization", target, path],
    /deployment authorization state schema validation failed/,
    /deployment authorization input exceeds 1048576 bytes/,
  );
  await assertBoundary(
    "pairing",
    metadataLimit,
    (path) => ["pairing-approve", target, path, "0", "invalid"],
    /JSON Web Key|key\.key|deployment authorization/,
    /pairing approval input exceeds 65536 bytes/,
  );
  await assertBoundary(
    "request",
    metadataLimit,
    (path) => ["authenticated-deploy", target, target, path, "0"],
    /bundle root|deployment authorization|signed deployment request/,
    /authenticated deployment request input exceeds 65536 bytes/,
  );
  assert.deepEqual(
    await readdir(target),
    [],
    "rejected deployment CLI inputs mutated the target",
  );
  console.log("Deployment CLI passed: 8 bounded input cases");
} finally {
  await rm(temporaryRoot, { recursive: true, force: true });
}
