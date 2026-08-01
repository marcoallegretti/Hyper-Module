import assert from "node:assert/strict";
import { existsSync } from "node:fs";
import { resolve } from "node:path";

import { runNativeProcess } from "./native-process.mjs";

const workspaceRoot = resolve(import.meta.dirname, "..", "..");

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
  return runNativeProcess(helperPath(), arguments_, {
    stdio: ["ignore", "ignore", "pipe", ...descriptors],
  });
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
  destinationName,
  label,
) {
  const status = await parentHandle.stat({ bigint: true });
  assert(status.isDirectory(), `${label} parent authority is not a directory`);
  const authority = retainedAuthority(parentHandle, parentPath, 3);
  try {
    await runHelper(
      [
        "rename-directory-no-replace",
        authority.path,
        String(status.dev),
        String(status.ino),
        sourceName,
        destinationName,
      ],
      authority.descriptor === undefined ? [] : [authority.descriptor],
    );
  } catch (error) {
    const wrapped = new Error(`${label} native publication failed`, {
      cause: error,
    });
    if (error.code === 2) wrapped.code = "EEXIST";
    throw wrapped;
  }
}

export async function renameDirectoryBetweenNoReplace(
  sourceParentHandle,
  sourceParentPath,
  sourceName,
  destinationParentHandle,
  destinationParentPath,
  destinationName,
  label,
) {
  const [sourceStatus, destinationStatus] = await Promise.all([
    sourceParentHandle.stat({ bigint: true }),
    destinationParentHandle.stat({ bigint: true }),
  ]);
  assert(sourceStatus.isDirectory(), `${label} source parent is not a directory`);
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
  try {
    await runHelper(
      [
        "rename-directory-between-no-replace",
        sourceAuthority.path,
        String(sourceStatus.dev),
        String(sourceStatus.ino),
        sourceName,
        destinationAuthority.path,
        String(destinationStatus.dev),
        String(destinationStatus.ino),
        destinationName,
      ],
      descriptors,
    );
  } catch (error) {
    const wrapped = new Error(`${label} native publication failed`, {
      cause: error,
    });
    if (error.code === 2) wrapped.code = "EEXIST";
    throw wrapped;
  }
}
