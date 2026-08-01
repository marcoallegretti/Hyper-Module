import assert from "node:assert/strict";
import { constants, lstat, open, realpath } from "node:fs/promises";
import { isAbsolute, relative, resolve, sep } from "node:path";

import { copyRetainedFile } from "./native-fs-authority.mjs";

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

function sameFileIdentity(left, right) {
  return (
    left.dev === right.dev &&
    left.ino === right.ino &&
    left.mode === right.mode &&
    left.size === right.size &&
    left.mtimeNs === right.mtimeNs &&
    left.ctimeNs === right.ctimeNs &&
    (process.platform !== "win32" || left.birthtimeNs === right.birthtimeNs)
  );
}

export async function copyVerifiedBundleFile(
  bundleRoot,
  bundleRootReal,
  stageHandle,
  stageRoot,
  descriptor,
  remainingBytes,
) {
  assert(
    Number.isSafeInteger(remainingBytes) && remainingBytes >= 0,
    "remaining copy budget must be a non-negative safe integer",
  );
  assert(
    descriptor.byte_length <= remainingBytes,
    `${descriptor.path}: copy exceeds remaining byte budget`,
  );
  const sourceRelativePath = descriptor.source_path ?? descriptor.path;
  const sourcePath = resolve(bundleRoot, ...sourceRelativePath.split("/"));
  assertContained(bundleRoot, sourcePath, sourceRelativePath);
  const sourceStatus = await lstat(sourcePath, { bigint: true });
  assert(
    sourceStatus.isFile() && !sourceStatus.isSymbolicLink(),
    `${sourceRelativePath}: source must be a regular file`,
  );
  if (descriptor.source_identity) {
    assert(
      sameFileIdentity(descriptor.source_identity, sourceStatus),
      `${sourceRelativePath}: source changed after preflight`,
    );
  }
  const sourceReal = await realpath(sourcePath);
  assertContained(bundleRootReal, sourceReal, sourceRelativePath);

  const noFollow = constants.O_NOFOLLOW ?? 0;
  const sourceHandle = await open(sourcePath, constants.O_RDONLY | noFollow);
  let result;
  let operationError;
  try {
    const before = await sourceHandle.stat({ bigint: true });
    assert(
      before.isFile() && sameFileIdentity(sourceStatus, before),
      `${sourceRelativePath}: source changed before copy`,
    );
    assert.equal(
      before.size,
      BigInt(descriptor.byte_length),
      `${sourceRelativePath}: source size changed before copy`,
    );

    result = await copyRetainedFile(
      sourceHandle,
      sourcePath,
      stageHandle,
      stageRoot,
      descriptor.path,
      descriptor.byte_length,
      descriptor.sha256,
      remainingBytes,
      descriptor.path,
    );
    const after = await sourceHandle.stat({ bigint: true });
    assert(
      sameFileIdentity(before, after),
      `${sourceRelativePath}: source changed during copy`,
    );
    const published = await lstat(sourcePath, { bigint: true });
    assert(
      published.isFile() &&
        !published.isSymbolicLink() &&
        sameFileIdentity(after, published),
      `${sourceRelativePath}: source path changed during copy`,
    );
  } catch (error) {
    operationError = error;
  }

  let closeError;
  try {
    await sourceHandle.close();
  } catch (error) {
    closeError = error;
  }
  if (operationError && closeError) {
    throw new AggregateError(
      [operationError, closeError],
      `${sourceRelativePath}: copy and source authority release both failed`,
    );
  }
  if (operationError) throw operationError;
  if (closeError) throw closeError;
  return result;
}
