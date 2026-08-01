import assert from "node:assert/strict";
import { existsSync } from "node:fs";
import { resolve } from "node:path";

import { runNativeProcess } from "./native-process.mjs";

const workspaceRoot = resolve(import.meta.dirname, "..", "..");
const protocolVersion = "2";

function helperPath() {
  if (process.env.HYPER_FS_AUTHORITY) {
    return resolve(process.env.HYPER_FS_AUTHORITY);
  }
  const executable = process.platform === "win32"
    ? "hyper-fs-authority.exe"
    : "hyper-fs-authority";
  const debugPath = resolve(workspaceRoot, "target", "debug", executable);
  return existsSync(debugPath)
    ? debugPath
    : resolve(workspaceRoot, "target", "release", executable);
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

function runHelper(arguments_, descriptors = []) {
  return runNativeProcess(helperPath(), [protocolVersion, ...arguments_], {
    stdio: ["ignore", "ignore", "pipe", ...descriptors],
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
