import assert from "node:assert/strict";
import {
  lstat,
  mkdtemp,
  realpath,
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

import {
  assertRetainedDirectory,
  openRetainedDirectory,
  syncDirectoryTree,
  syncPublishedDirectory,
} from "../lib/durable-files.mjs";
import {
  removeRetainedDirectory,
  renameDirectoryNoReplace,
} from "../lib/native-fs-authority.mjs";
import {
  runCleanupSteps,
  throwPreservingCleanupError,
} from "../lib/operation-cleanup.mjs";
import { PublishedDirectoryDurabilityError } from "../lib/published-directory.mjs";
import { copyVerifiedBundleFile } from "../lib/verified-bundle-copy.mjs";
import { verifyBundleArtifact } from "./verify-bundle.mjs";
import {
  defaultMaximumProjectBytes,
  verifyProject,
} from "./verify-project.mjs";

const faultBoundaries = new Set([
  "after-payload-copy",
  "after-project-verification",
  "before-publication",
  "after-publication-before-sync",
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

export async function restoreProject(
  bundlePath,
  outputPath,
  {
    faultAt,
    maximumProjectBytes = defaultMaximumProjectBytes,
  } = {},
) {
  assert(
    faultAt === undefined || faultBoundaries.has(faultAt),
    "unknown restoration fault boundary",
  );
  assert(
    Number.isSafeInteger(maximumProjectBytes) && maximumProjectBytes > 0,
    "maximum project bytes must be a positive safe integer",
  );

  const bundleRoot = resolve(bundlePath);
  const bundleStatus = await lstat(bundleRoot);
  assert(
    bundleStatus.isDirectory() && !bundleStatus.isSymbolicLink(),
    "bundle root must be a real directory",
  );
  const bundleRootReal = await realpath(bundleRoot);
  const verifiedBundle = await verifyBundleArtifact(bundleRoot, {
    maximumBundleBytes: maximumProjectBytes,
  });
  const manifest = verifiedBundle.manifest;

  const outputRoot = resolve(outputPath);
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
  const stageHandle = await openRetainedDirectory(stageRoot);
  const outputParentHandle = await openRetainedDirectory(outputParent);
  let published = false;
  let operationError;
  try {
    const payloads = [...manifest.files].sort((left, right) =>
      left.path < right.path ? -1 : left.path > right.path ? 1 : 0,
    );
    let copiedBytes = 0;
    for (const descriptor of payloads) {
      copiedBytes += await copyVerifiedBundleFile(
        bundleRoot,
        bundleRootReal,
        stageRoot,
        descriptor,
        maximumProjectBytes - copiedBytes,
      );
    }
    injectFault(faultAt, "after-payload-copy");

    const restored = await verifyProject(stageRoot, { maximumProjectBytes });
    assert.equal(
      restored.identity,
      `sha256:${manifest.source_project_sha256}`,
      "restored project identity does not match the runtime bundle",
    );
    injectFault(faultAt, "after-project-verification");

    injectFault(faultAt, "before-publication");
    await syncDirectoryTree(
      stageRoot,
      payloads.map((descriptor) => descriptor.path),
    );
    const finalRestored = await verifyProject(stageRoot, {
      maximumProjectBytes,
    });
    assert.equal(
      finalRestored.identity,
      restored.identity,
      "restored project identity changed before publication",
    );
    const result = {
      output_path: outputRoot,
      identity: restored.identity,
    };
    try {
      await renameDirectoryNoReplace(
        outputParentHandle,
        outputParent,
        basename(stageRoot),
        await stageHandle.stat({ bigint: true }),
        basename(outputRoot),
        "project",
      );
      published = true;
    } catch (error) {
      if (!new Set(["EACCES", "EEXIST", "ENOTEMPTY", "EPERM"]).has(error.code)) {
        try {
          await assertRetainedDirectory(
            stageHandle,
            outputRoot,
            "published project",
          );
          published = true;
        } catch {
          throw error;
        }
      } else {
        try {
          await lstat(outputRoot);
        } catch (statusError) {
          if (statusError.code === "ENOENT") throw error;
          throw statusError;
        }
        let existing;
        try {
          existing = await verifyProject(outputRoot);
        } catch (cause) {
          throw new Error(
            `output already exists and is not the requested project: ${outputRoot}`,
            { cause },
          );
        }
        assert.equal(
          existing.identity,
          result.identity,
          `output already exists with a different project identity: ${outputRoot}`,
        );
        await syncPublishedDirectory(outputRoot);
        return result;
      }
    }
    await assertRetainedDirectory(stageHandle, outputRoot, "published project");
    try {
      injectFault(faultAt, "after-publication-before-sync");
      await syncPublishedDirectory(outputRoot);
    } catch (error) {
      throw new PublishedDirectoryDurabilityError(
        outputRoot,
        result.identity,
        error,
      );
    }
    return result;
  } catch (error) {
    operationError = error;
    throw error;
  } finally {
    try {
      await runCleanupSteps(
        [
          async () => {
            if (published) return;
            let stagePresent = true;
            try {
              await lstat(stageRoot);
            } catch (error) {
              if (error.code === "ENOENT") {
                stagePresent = false;
              } else {
                throw error;
              }
            }
            if (stagePresent) {
              await removeRetainedDirectory(
                stageHandle,
                stageRoot,
                "project staging directory",
              );
            }
          },
          async () => stageHandle.close(),
          async () => outputParentHandle.close(),
        ],
        "project staging cleanup failed",
      );
    } catch (cleanupError) {
      throwPreservingCleanupError(
        operationError,
        cleanupError,
        "project restoration and cleanup both failed",
      );
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
