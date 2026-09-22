import assert from "node:assert/strict";
import { createHash } from "node:crypto";
import { existsSync } from "node:fs";
import { join, resolve } from "node:path";

import { runNativeProcess } from "./native-process.mjs";

const workspaceRoot = resolve(import.meta.dirname, "..", "..");
const protocolVersion = "2";
const authorityOperationTimeoutMs = 30_000;
const transferOperationTimeoutMs = 10 * 60_000;

export function resolveFilesystemAuthorityPath() {
  if (process.env.HYPER_FS_AUTHORITY) {
    return resolve(process.env.HYPER_FS_AUTHORITY);
  }
  const executable =
    process.platform === "win32"
      ? "hyper-fs-authority.exe"
      : "hyper-fs-authority";
  const targetDirectory = process.env.CARGO_TARGET_DIR
    ? resolve(process.env.CARGO_TARGET_DIR)
    : resolve(workspaceRoot, "target");
  const debugPath = resolve(targetDirectory, "debug", executable);
  return existsSync(debugPath)
    ? debugPath
    : resolve(targetDirectory, "release", executable);
}

function retainedAuthority(handle, ambientPath, childDescriptor) {
  if (process.platform !== "linux") {
    return { path: ambientPath, descriptor: undefined };
  }
  assert(Number.isInteger(handle.fd), "retained directory handle is closed");
  return {
    path: `/proc/self/fd/${childDescriptor}`,
    descriptor: handle.fd,
  };
}

function runHelper(
  arguments_,
  descriptors = [],
  {
    input = undefined,
    captureOutput = false,
    timeoutMs = authorityOperationTimeoutMs,
  } = {},
) {
  return runNativeProcess(resolveFilesystemAuthorityPath(), [protocolVersion, ...arguments_], {
    stdio: [
      input === undefined ? "ignore" : "pipe",
      captureOutput ? "pipe" : "ignore",
      "pipe",
      ...descriptors,
    ],
    input,
    maximumOutputBytes: 256,
    timeoutMs,
  });
}

function retainedEntryIdentity(status, label) {
  assert.equal(typeof status?.dev, "bigint", `${label} device identity is missing`);
  assert.equal(typeof status?.ino, "bigint", `${label} file identity is missing`);
  return [String(status.dev), String(status.ino)];
}

function classifyNativeRenameError(wrapped, source) {
  if (source.code === 2) wrapped.code = "EEXIST";
  if (source.message.includes("source entry identity does not match")) {
    wrapped.code = "ESTALE";
  }
  return wrapped;
}

function parseTransferResult(output, label) {
  const match = /^([0-9]+) ([0-9a-f]{64})\r?\n$/.exec(output.toString("utf8"));
  assert(match, `${label} native transfer returned an invalid result`);
  const byteLength = Number(match[1]);
  assert(Number.isSafeInteger(byteLength), `${label} byte length is not exact`);
  return { byteLength, sha256: match[2] };
}

function parseIdentityResult(output, label) {
  const match = /^([0-9]+) ([0-9]+)\r?\n$/.exec(output.toString("utf8"));
  assert(match, `${label} native operation returned an invalid identity`);
  return { dev: BigInt(match[1]), ino: BigInt(match[2]) };
}

function retainedChildPath(handle, ambientPath, childName) {
  return process.platform === "linux"
    ? join(`/proc/self/fd/${handle.fd}`, childName)
    : join(ambientPath, childName);
}

export async function preflightFilesystemAuthority(
  label = "filesystem authority",
) {
  try {
    await runHelper(["preflight"]);
  } catch (error) {
    throw new Error(`${label} native helper compatibility check failed`, {
      cause: error,
    });
  }
}

export async function createRetainedDirectory(
  parentHandle,
  parentPath,
  childName,
  label,
) {
  const status = await parentHandle.stat({ bigint: true });
  assert(status.isDirectory(), `${label} parent authority is not a directory`);
  const authority = retainedAuthority(parentHandle, parentPath, 3);
  try {
    const output = await runHelper(
      [
        "create-directory",
        authority.path,
        String(status.dev),
        String(status.ino),
        childName,
      ],
      authority.descriptor === undefined ? [] : [authority.descriptor],
      { captureOutput: true },
    );
    return {
      path: retainedChildPath(parentHandle, parentPath, childName),
      identity: parseIdentityResult(output, label),
    };
  } catch (error) {
    const wrapped = new Error(`${label} native creation failed`, { cause: error });
    if (error.code === 2) wrapped.code = "EEXIST";
    throw wrapped;
  }
}

export async function removeRetainedDirectoryEntry(
  parentHandle,
  parentPath,
  childName,
  childIdentity,
  label,
) {
  const status = await parentHandle.stat({ bigint: true });
  assert(status.isDirectory(), `${label} parent authority is not a directory`);
  const authority = retainedAuthority(parentHandle, parentPath, 3);
  const identity = retainedEntryIdentity(childIdentity, label);
  try {
    await runHelper(
      [
        "remove-directory-entry",
        authority.path,
        String(status.dev),
        String(status.ino),
        childName,
        ...identity,
      ],
      authority.descriptor === undefined ? [] : [authority.descriptor],
    );
  } catch (error) {
    throw new Error(`${label} native cleanup failed`, { cause: error });
  }
}

export async function copyRetainedFile(
  sourceHandle,
  sourcePath,
  destinationRootHandle,
  destinationRootPath,
  destinationPath,
  expectedByteLength,
  expectedSha256,
  maximumBytes,
  label,
) {
  assert(
    Number.isSafeInteger(expectedByteLength) && expectedByteLength >= 0,
    `${label} expected byte length is invalid`,
  );
  assert(
    expectedSha256 === undefined || /^[0-9a-f]{64}$/.test(expectedSha256),
    `${label} expected SHA-256 is invalid`,
  );
  assert(
    Number.isSafeInteger(maximumBytes) && maximumBytes >= 0,
    `${label} byte budget is invalid`,
  );
  const [sourceStatus, destinationStatus] = await Promise.all([
    sourceHandle.stat({ bigint: true }),
    destinationRootHandle.stat({ bigint: true }),
  ]);
  assert(sourceStatus.isFile(), `${label} source authority is not a file`);
  assert(
    destinationStatus.isDirectory(),
    `${label} destination authority is not a directory`,
  );
  const sourceAuthority = retainedAuthority(sourceHandle, sourcePath, 3);
  const destinationAuthority = retainedAuthority(
    destinationRootHandle,
    destinationRootPath,
    4,
  );
  const descriptors = [sourceAuthority.descriptor, destinationAuthority.descriptor]
    .filter((descriptor) => descriptor !== undefined);
  try {
    const output = await runHelper(
      [
        "copy-file",
        sourceAuthority.path,
        String(sourceStatus.dev),
        String(sourceStatus.ino),
        destinationAuthority.path,
        String(destinationStatus.dev),
        String(destinationStatus.ino),
        destinationPath,
        String(expectedByteLength),
        expectedSha256 ?? "-",
        String(maximumBytes),
      ],
      descriptors,
      {
        captureOutput: true,
        timeoutMs: transferOperationTimeoutMs,
      },
    );
    const result = parseTransferResult(output, label);
    assert.equal(
      result.byteLength,
      expectedByteLength,
      `${label} native transfer byte length mismatch`,
    );
    if (expectedSha256 !== undefined) {
      assert.equal(
        result.sha256,
        expectedSha256,
        `${label} native transfer SHA-256 mismatch`,
      );
    }
    return result;
  } catch (error) {
    throw new Error(`${label} native copy failed`, { cause: error });
  }
}

export async function writeRetainedFile(
  destinationRootHandle,
  destinationRootPath,
  destinationPath,
  bytes,
  maximumBytes,
  label,
) {
  assert(Buffer.isBuffer(bytes), `${label} bytes must be a buffer`);
  assert(
    Number.isSafeInteger(maximumBytes) && maximumBytes >= 0,
    `${label} byte budget is invalid`,
  );
  assert(bytes.byteLength <= maximumBytes, `${label} exceeds its byte budget`);
  const destinationStatus = await destinationRootHandle.stat({ bigint: true });
  assert(
    destinationStatus.isDirectory(),
    `${label} destination authority is not a directory`,
  );
  const destinationAuthority = retainedAuthority(
    destinationRootHandle,
    destinationRootPath,
    3,
  );
  const sha256 = createHash("sha256").update(bytes).digest("hex");
  try {
    const output = await runHelper(
      [
        "write-file",
        destinationAuthority.path,
        String(destinationStatus.dev),
        String(destinationStatus.ino),
        destinationPath,
        String(bytes.byteLength),
        sha256,
        String(maximumBytes),
      ],
      destinationAuthority.descriptor === undefined
        ? []
        : [destinationAuthority.descriptor],
      {
        input: bytes,
        captureOutput: true,
        timeoutMs: transferOperationTimeoutMs,
      },
    );
    const result = parseTransferResult(output, label);
    assert.deepEqual(
      result,
      { byteLength: bytes.byteLength, sha256 },
      `${label} native write result mismatch`,
    );
    return result;
  } catch (error) {
    throw new Error(`${label} native write failed`, { cause: error });
  }
}

export async function removeRetainedDirectory(handle, path, label) {
  const status = await handle.stat({ bigint: true });
  assert(status.isDirectory(), `${label} authority is not a directory`);
  const authority = retainedAuthority(handle, path, 3);
  try {
    await runHelper(
      [
        "remove-directory",
        authority.path,
        String(status.dev),
        String(status.ino),
      ],
      authority.descriptor === undefined ? [] : [authority.descriptor],
    );
  } catch (error) {
    throw new Error(`${label} native cleanup failed`, { cause: error });
  }
}

export async function renameDirectoryNoReplace(
  parentHandle,
  parentPath,
  sourceName,
  sourceStatus,
  destinationName,
  label,
) {
  const status = await parentHandle.stat({ bigint: true });
  assert(status.isDirectory(), `${label} parent authority is not a directory`);
  const authority = retainedAuthority(parentHandle, parentPath, 3);
  const sourceIdentity = retainedEntryIdentity(sourceStatus, `${label} source`);
  try {
    await runHelper(
      [
        "rename-directory-no-replace",
        authority.path,
        String(status.dev),
        String(status.ino),
        sourceName,
        ...sourceIdentity,
        destinationName,
      ],
      authority.descriptor === undefined ? [] : [authority.descriptor],
    );
  } catch (error) {
    throw classifyNativeRenameError(
      new Error(`${label} native publication failed`, { cause: error }),
      error,
    );
  }
}

export async function renameDirectoryBetweenNoReplace(
  sourceParentHandle,
  sourceParentPath,
  sourceName,
  sourceEntryStatus,
  destinationParentHandle,
  destinationParentPath,
  destinationName,
  label,
) {
  const [sourceParentStatus, destinationStatus] = await Promise.all([
    sourceParentHandle.stat({ bigint: true }),
    destinationParentHandle.stat({ bigint: true }),
  ]);
  assert(
    sourceParentStatus.isDirectory(),
    `${label} source parent is not a directory`,
  );
  assert(
    destinationStatus.isDirectory(),
    `${label} destination parent is not a directory`,
  );
  const sourceAuthority = retainedAuthority(
    sourceParentHandle,
    sourceParentPath,
    3,
  );
  const destinationAuthority = retainedAuthority(
    destinationParentHandle,
    destinationParentPath,
    4,
  );
  const descriptors = [sourceAuthority.descriptor, destinationAuthority.descriptor]
    .filter((descriptor) => descriptor !== undefined);
  const sourceIdentity = retainedEntryIdentity(
    sourceEntryStatus,
    `${label} source`,
  );
  try {
    await runHelper(
      [
        "rename-directory-between-no-replace",
        sourceAuthority.path,
        String(sourceParentStatus.dev),
        String(sourceParentStatus.ino),
        sourceName,
        ...sourceIdentity,
        destinationAuthority.path,
        String(destinationStatus.dev),
        String(destinationStatus.ino),
        destinationName,
      ],
      descriptors,
    );
  } catch (error) {
    throw classifyNativeRenameError(
      new Error(`${label} native publication failed`, { cause: error }),
      error,
    );
  }
}

export async function renameEntryBetweenReplace(
  sourceParentHandle,
  sourceParentPath,
  sourceName,
  sourceEntryStatus,
  destinationParentHandle,
  destinationParentPath,
  destinationName,
  label,
) {
  const [sourceParentStatus, destinationStatus] = await Promise.all([
    sourceParentHandle.stat({ bigint: true }),
    destinationParentHandle.stat({ bigint: true }),
  ]);
  assert(
    sourceParentStatus.isDirectory(),
    `${label} source parent is not a directory`,
  );
  assert(
    destinationStatus.isDirectory(),
    `${label} destination parent is not a directory`,
  );
  const sourceAuthority = retainedAuthority(
    sourceParentHandle,
    sourceParentPath,
    3,
  );
  const destinationAuthority = retainedAuthority(
    destinationParentHandle,
    destinationParentPath,
    4,
  );
  const descriptors = [sourceAuthority.descriptor, destinationAuthority.descriptor]
    .filter((descriptor) => descriptor !== undefined);
  const sourceIdentity = retainedEntryIdentity(
    sourceEntryStatus,
    `${label} source`,
  );
  try {
    await runHelper(
      [
        "rename-entry-between-replace",
        sourceAuthority.path,
        String(sourceParentStatus.dev),
        String(sourceParentStatus.ino),
        sourceName,
        ...sourceIdentity,
        destinationAuthority.path,
        String(destinationStatus.dev),
        String(destinationStatus.ino),
        destinationName,
      ],
      descriptors,
    );
  } catch (error) {
    throw classifyNativeRenameError(
      new Error(`${label} native replacement failed`, { cause: error }),
      error,
    );
  }
}
