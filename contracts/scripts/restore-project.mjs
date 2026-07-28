import assert from "node:assert/strict";
import { createHash } from "node:crypto";
import {
  access,
  chmod,
  constants,
  lstat,
  mkdir,
  mkdtemp,
  open,
  realpath,
  rename,
  rm,
} from "node:fs/promises";
import {
  basename,
  dirname,
  isAbsolute,
  relative,
  resolve,
  sep,
} from "node:path";
import { pathToFileURL } from "node:url";

import { verifyBundle } from "./verify-bundle.mjs";
import { verifyProject } from "./verify-project.mjs";

const faultBoundaries = new Set([
  "after-payload-copy",
  "after-project-verification",
  "before-publication",
]);

function injectFault(faultAt, boundary) {
  if (faultAt === boundary) {
    throw new Error(`injected fault at ${boundary}`);
  }
}

function assertContained(rootPath, candidatePath, label) {
  const relativePath = relative(rootPath, candidatePath);
  assert(
    relativePath &&
      !relativePath.startsWith(`..${sep}`) &&
      relativePath !== ".." &&
      !isAbsolute(relativePath),
    `${label}: path escapes its root`,
  );
}

function isContainedOrEqual(rootPath, candidatePath) {
  const relativePath = relative(rootPath, candidatePath);
  return (
    relativePath === "" ||
    (!relativePath.startsWith(`..${sep}`) &&
      relativePath !== ".." &&
      !isAbsolute(relativePath))
  );
}

async function assertPathAbsent(path) {
  try {
    await access(path);
  } catch (error) {
    if (error.code === "ENOENT") {
      return;
    }
    throw error;
  }
  assert.fail(`output already exists: ${path}`);
}

async function writeAll(handle, bytes) {
  let offset = 0;
  while (offset < bytes.length) {
    const { bytesWritten } = await handle.write(
      bytes,
      offset,
      bytes.length - offset,
    );
    assert(bytesWritten > 0, "write made no forward progress");
    offset += bytesWritten;
  }
}

async function copyVerifiedPayload(
  bundleRoot,
  bundleRootReal,
  stageRoot,
  descriptor,
) {
  const sourcePath = resolve(bundleRoot, ...descriptor.path.split("/"));
  assertContained(bundleRoot, sourcePath, descriptor.path);
  const sourceStatus = await lstat(sourcePath);
  assert(
    sourceStatus.isFile() && !sourceStatus.isSymbolicLink(),
    `${descriptor.path}: source must be a regular file`,
  );
  const sourceReal = await realpath(sourcePath);
  assertContained(bundleRootReal, sourceReal, descriptor.path);

  const destinationPath = resolve(stageRoot, ...descriptor.path.split("/"));
  assertContained(stageRoot, destinationPath, descriptor.path);
  await mkdir(dirname(destinationPath), { recursive: true, mode: 0o755 });

  const noFollow = constants.O_NOFOLLOW ?? 0;
  const sourceHandle = await open(sourcePath, constants.O_RDONLY | noFollow);
  const destinationHandle = await open(
    destinationPath,
    constants.O_WRONLY | constants.O_CREAT | constants.O_EXCL,
    0o644,
  );
  const digest = createHash("sha256");
  let byteLength = 0;
  let before;
  let after;
  try {
    before = await sourceHandle.stat();
    for await (const chunk of sourceHandle.createReadStream({
      autoClose: false,
    })) {
      digest.update(chunk);
      byteLength += chunk.length;
      await writeAll(destinationHandle, chunk);
    }
    await destinationHandle.sync();
    after = await sourceHandle.stat();
  } finally {
    await Promise.allSettled([sourceHandle.close(), destinationHandle.close()]);
  }

  assert(
    before.isFile() && after.isFile(),
    `${descriptor.path}: source changed type during restoration`,
  );
  assert.equal(
    before.size,
    after.size,
    `${descriptor.path}: source size changed during restoration`,
  );
  assert.equal(
    before.mtimeMs,
    after.mtimeMs,
    `${descriptor.path}: source changed during restoration`,
  );
  assert.equal(
    before.ctimeMs,
    after.ctimeMs,
    `${descriptor.path}: source metadata changed during restoration`,
  );
  assert.equal(
    byteLength,
    descriptor.byte_length,
    `${descriptor.path}: restored byte length mismatch`,
  );
  assert.equal(
    digest.digest("hex"),
    descriptor.sha256,
    `${descriptor.path}: restored SHA-256 mismatch`,
  );

  const finalSourceStatus = await lstat(sourcePath);
  assert(
    finalSourceStatus.isFile() && !finalSourceStatus.isSymbolicLink(),
    `${descriptor.path}: source path changed type during restoration`,
  );
  assert.equal(
    finalSourceStatus.dev,
    after.dev,
    `${descriptor.path}: source path changed during restoration`,
  );
  assert.equal(
    finalSourceStatus.ino,
    after.ino,
    `${descriptor.path}: source path changed during restoration`,
  );
  await chmod(destinationPath, 0o644);
}

export async function restoreProject(bundlePath, outputPath, { faultAt } = {}) {
  assert(
    faultAt === undefined || faultBoundaries.has(faultAt),
    "unknown restoration fault boundary",
  );

  const bundleRoot = resolve(bundlePath);
  const bundleStatus = await lstat(bundleRoot);
  assert(
    bundleStatus.isDirectory() && !bundleStatus.isSymbolicLink(),
    "bundle root must be a real directory",
  );
  const bundleRootReal = await realpath(bundleRoot);
  const manifest = await verifyBundle(bundleRoot);

  const outputRoot = resolve(outputPath);
  await assertPathAbsent(outputRoot);
  const outputParent = dirname(outputRoot);
  const parentStatus = await lstat(outputParent);
  assert(
    parentStatus.isDirectory() && !parentStatus.isSymbolicLink(),
    "output parent must be a real directory",
  );
  const outputParentReal = await realpath(outputParent);
  const resolvedOutputCandidate = resolve(
    outputParentReal,
    basename(outputRoot),
  );
  assert(
    !isContainedOrEqual(bundleRootReal, resolvedOutputCandidate),
    "output project must be outside the runtime bundle",
  );

  const stageRoot = await mkdtemp(
    resolve(outputParent, `.${basename(outputRoot)}.restore-`),
  );
  let published = false;
  try {
    const payloads = [...manifest.files].sort((left, right) =>
      left.path < right.path ? -1 : left.path > right.path ? 1 : 0,
    );
    for (const descriptor of payloads) {
      await copyVerifiedPayload(
        bundleRoot,
        bundleRootReal,
        stageRoot,
        descriptor,
      );
    }
    injectFault(faultAt, "after-payload-copy");

    const restored = await verifyProject(stageRoot);
    assert.equal(
      restored.identity,
      `sha256:${manifest.source_project_sha256}`,
      "restored project identity does not match the runtime bundle",
    );
    injectFault(faultAt, "after-project-verification");

    await assertPathAbsent(outputRoot);
    injectFault(faultAt, "before-publication");
    await rename(stageRoot, outputRoot);
    published = true;

    return {
      output_path: outputRoot,
      identity: restored.identity,
    };
  } finally {
    if (!published) {
      await rm(stageRoot, { recursive: true, force: true });
    }
  }
}

const invokedPath = process.argv[1]
  ? pathToFileURL(resolve(process.argv[1])).href
  : undefined;
if (invokedPath === import.meta.url) {
  assert.equal(
    process.argv.length,
    4,
    "usage: node scripts/restore-project.mjs <runtime-bundle> <output-project>",
  );
  const result = await restoreProject(process.argv[2], process.argv[3]);
  console.log(result.identity);
}
