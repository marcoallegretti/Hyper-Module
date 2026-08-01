import assert from "node:assert/strict";
import { createHash } from "node:crypto";
import {
  chmod,
  constants,
  lstat,
  mkdir,
  open,
  realpath,
} from "node:fs/promises";
import { dirname, isAbsolute, relative, resolve, sep } from "node:path";

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

export async function copyVerifiedBundleFile(
  bundleRoot,
  bundleRootReal,
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
  const sourcePath = resolve(bundleRoot, ...descriptor.path.split("/"));
  assertContained(bundleRoot, sourcePath, descriptor.path);
  const sourceStatus = await lstat(sourcePath);
  assert(
    sourceStatus.isFile() && !sourceStatus.isSymbolicLink(),
    `${descriptor.path}: source must be a regular file`,
  );
  const sourceReal = await realpath(sourcePath);
  assertContained(bundleRootReal, sourceReal, descriptor.path);

  const noFollow = constants.O_NOFOLLOW ?? 0;
  const sourceHandle = await open(sourcePath, constants.O_RDONLY | noFollow);
  let destinationHandle;
  let before;
  let after;
  let byteLength = 0;
  const digest = createHash("sha256");
  try {
    before = await sourceHandle.stat();
    assert(before.isFile(), `${descriptor.path}: source changed type before copy`);
    assert.equal(
      before.size,
      descriptor.byte_length,
      `${descriptor.path}: source size changed before copy`,
    );

    const destinationPath = resolve(
      stageRoot,
      ...descriptor.path.split("/"),
    );
    assertContained(stageRoot, destinationPath, descriptor.path);
    await mkdir(dirname(destinationPath), { recursive: true, mode: 0o755 });
    destinationHandle = await open(
      destinationPath,
      constants.O_WRONLY | constants.O_CREAT | constants.O_EXCL,
      0o644,
    );
    for await (const chunk of sourceHandle.createReadStream({
      autoClose: false,
    })) {
      const nextByteLength = byteLength + chunk.byteLength;
      assert(
        nextByteLength <= descriptor.byte_length &&
          nextByteLength <= remainingBytes,
        `${descriptor.path}: source exceeded its copy budget`,
      );
      digest.update(chunk);
      await writeAll(destinationHandle, chunk);
      byteLength = nextByteLength;
    }
    await destinationHandle.sync();
    after = await sourceHandle.stat();
    await chmod(destinationPath, 0o644);
  } finally {
    await Promise.allSettled([
      sourceHandle.close(),
      destinationHandle?.close(),
    ]);
  }

  assert(
    before.isFile() && after.isFile(),
    `${descriptor.path}: source changed type during copy`,
  );
  assert.equal(
    before.size,
    after.size,
    `${descriptor.path}: source size changed during copy`,
  );
  assert.equal(
    before.mtimeMs,
    after.mtimeMs,
    `${descriptor.path}: source changed during copy`,
  );
  assert.equal(
    before.ctimeMs,
    after.ctimeMs,
    `${descriptor.path}: source metadata changed during copy`,
  );
  assert.equal(
    byteLength,
    descriptor.byte_length,
    `${descriptor.path}: copied byte length mismatch`,
  );
  assert.equal(
    digest.digest("hex"),
    descriptor.sha256,
    `${descriptor.path}: copied SHA-256 mismatch`,
  );

  const finalSourceStatus = await lstat(sourcePath);
  assert(
    finalSourceStatus.isFile() && !finalSourceStatus.isSymbolicLink(),
    `${descriptor.path}: source path changed type during copy`,
  );
  assert.equal(
    finalSourceStatus.dev,
    after.dev,
    `${descriptor.path}: source path changed during copy`,
  );
  assert.equal(
    finalSourceStatus.ino,
    after.ino,
    `${descriptor.path}: source path changed during copy`,
  );
  return byteLength;
}
