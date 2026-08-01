import assert from "node:assert/strict";
import { randomUUID } from "node:crypto";

import { openRetainedDirectory } from "./durable-files.mjs";
import {
  createRetainedDirectory,
  removeRetainedDirectoryEntry,
} from "./native-fs-authority.mjs";
import {
  runCleanupSteps,
  throwPreservingCleanupError,
} from "./operation-cleanup.mjs";

const maximumNameAttempts = 4;

async function createUnderAuthority(
  parentHandle,
  parentPath,
  prefix,
  label,
  closeParentOnFailure,
) {
  let created;
  let stageHandle;
  let operationError;
  try {
    for (let attempt = 0; attempt < maximumNameAttempts; attempt += 1) {
      const name = `${prefix}${randomUUID()}`;
      try {
        created = await createRetainedDirectory(
          parentHandle,
          parentPath,
          name,
          label,
        );
        created.name = name;
        break;
      } catch (error) {
        if (error.code !== "EEXIST") throw error;
      }
    }
    assert(created, `${label} could not allocate a unique name`);

    stageHandle = await openRetainedDirectory(created.path);
    const status = await stageHandle.stat({ bigint: true });
    assert(
      status.isDirectory() &&
        status.dev === created.identity.dev &&
        status.ino === created.identity.ino,
      `${label} identity changed while being retained`,
    );
    return {
      parentHandle,
      handle: stageHandle,
      path: created.path,
      name: created.name,
    };
  } catch (error) {
    operationError = error;
    try {
      await runCleanupSteps(
        [
          async () => {
            if (!created) return;
            await removeRetainedDirectoryEntry(
              parentHandle,
              parentPath,
              created.name,
              created.identity,
              label,
            );
          },
          async () => stageHandle?.close(),
          async () => {
            if (closeParentOnFailure) await parentHandle.close();
          },
        ],
        `${label} initialization cleanup failed`,
      );
    } catch (cleanupError) {
      throwPreservingCleanupError(
        operationError,
        cleanupError,
        `${label} initialization and cleanup both failed`,
      );
    }
    throw error;
  }
}

export async function createStagingDirectory(parentPath, prefix, label) {
  const parentHandle = await openRetainedDirectory(parentPath);
  return createUnderAuthority(parentHandle, parentPath, prefix, label, true);
}

export async function createStagingDirectoryUnder(
  parentHandle,
  parentPath,
  prefix,
  label,
) {
  return createUnderAuthority(parentHandle, parentPath, prefix, label, false);
}
