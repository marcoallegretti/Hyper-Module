import assert from "node:assert/strict";
import { createHash } from "node:crypto";
import {
  chmod,
  constants,
  lstat,
  mkdir,
  mkdtemp,
  open,
  readFile,
  realpath,
} from "node:fs/promises";
import {
  basename,
  dirname,
  isAbsolute,
  join,
  relative,
  resolve,
  sep,
} from "node:path";
import { fileURLToPath, pathToFileURL } from "node:url";

import Ajv2020 from "ajv/dist/2020.js";
import canonicalize from "canonicalize";

import {
  assertRetainedDirectory,
  openRetainedDirectory,
  readStableFile,
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
import { strictParse } from "../lib/strict-json.mjs";
import {
  verifyBundle,
  verifyBundleArtifact,
} from "./verify-bundle.mjs";

const contractRoot = resolve(dirname(fileURLToPath(import.meta.url)), "..");
const planLimitBytes = 1024 * 1024;
const defaultMaximumBundleBytes = 1024 * 1024 * 1024;
const builderId = "hyper-bundle-builder";

async function createPlanValidator() {
  const schemaNames = [
    "common.schema.json",
    "hardware-capabilities.schema.json",
    "runtime-build-plan.schema.json",
  ];
  const schemas = await Promise.all(
    schemaNames.map(async (name) =>
      strictParse(
        await readFile(resolve(contractRoot, "schemas", "v1", name), "utf8"),
      ),
    ),
  );
  const ajv = new Ajv2020({
    allErrors: true,
    strict: true,
    validateFormats: false,
  });
  schemas.forEach((schema) => ajv.addSchema(schema));
  return ajv.getSchema(schemas[2].$id);
}

async function readBuildPlan(planPath) {
  const bytes = await readStableFile(planPath, planLimitBytes, "build plan");
  assert(
    !bytes.subarray(0, 3).equals(Buffer.from([0xef, 0xbb, 0xbf])),
    "build plan must not contain a byte-order mark",
  );
  let text;
  try {
    text = new TextDecoder("utf-8", { fatal: true }).decode(bytes);
  } catch (error) {
    throw new Error("build plan is not valid UTF-8", { cause: error });
  }
  const plan = strictParse(text);
  const validate = await createPlanValidator();
  assert(validate, "runtime build-plan schema was not registered");
  assert(
    validate(plan),
    `build-plan schema validation failed: ${JSON.stringify(validate.errors)}`,
  );

  const foldedPaths = plan.payloads.map((payload) =>
    payload.path.toLowerCase(),
  );
  assert.equal(
    new Set(foldedPaths).size,
    foldedPaths.length,
    "bundle payload paths must be unique after case folding",
  );
  const entrypoints = plan.payloads.filter(
    (payload) => payload.path === plan.entrypoint,
  );
  assert.equal(entrypoints.length, 1, "entrypoint must be listed exactly once");
  assert.equal(
    entrypoints[0].role,
    "runtime-project",
    "entrypoint must have the runtime-project role",
  );
  return plan;
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

function createManifest(plan, files, builderVersion) {
  return {
    format: "hyper-runtime-bundle",
    format_version: 1,
    project_id: plan.project_id,
    project_revision: plan.project_revision,
    source_project_sha256: plan.source_project_sha256,
    minimum_runtime_version: plan.minimum_runtime_version,
    builder: {
      id: builderId,
      version: builderVersion,
    },
    entrypoint: plan.entrypoint,
    required_capabilities: plan.required_capabilities,
    files,
    extensions: plan.extensions,
  };
}

async function preflightPayload(sourceRoot, sourceRootReal, payload) {
  const sourcePath = resolve(sourceRoot, ...payload.source_path.split("/"));
  assertContained(sourceRoot, sourcePath, payload.source_path);
  const before = await lstat(sourcePath, { bigint: true });
  assert(
    before.isFile() && !before.isSymbolicLink(),
    `${payload.source_path}: source must be a regular file`,
  );
  const noFollow = constants.O_NOFOLLOW ?? 0;
  const sourceHandle = await open(sourcePath, constants.O_RDONLY | noFollow);
  let opened;
  try {
    opened = await sourceHandle.stat({ bigint: true });
    const published = await lstat(sourcePath, { bigint: true });
    assert(
      opened.isFile() && sameFileIdentity(before, opened),
      `${payload.source_path}: source changed during preflight`,
    );
    assert(
      published.isFile() &&
        !published.isSymbolicLink() &&
        sameFileIdentity(opened, published),
      `${payload.source_path}: source path changed during preflight`,
    );
  } finally {
    await sourceHandle.close();
  }
  assert(
    opened.size <= BigInt(Number.MAX_SAFE_INTEGER),
    `${payload.source_path}: source is too large for an exact byte count`,
  );
  const sourceReal = await realpath(sourcePath);
  assertContained(sourceRootReal, sourceReal, payload.source_path);
  return {
    identity: opened,
    byteLength: Number(opened.size),
  };
}

async function preflightBundle(
  sourceRoot,
  sourceRootReal,
  plan,
  payloads,
  builderVersion,
  maximumBundleBytes,
) {
  const sources = new Map();
  const files = [];
  let payloadBytes = 0;
  for (const payload of payloads) {
    let source = sources.get(payload.source_path);
    if (!source) {
      source = await preflightPayload(sourceRoot, sourceRootReal, payload);
      sources.set(payload.source_path, source);
    }
    payloadBytes += source.byteLength;
    assert(
      Number.isSafeInteger(payloadBytes),
      "runtime bundle byte accounting exceeds safe integer range",
    );
    files.push({
      path: payload.path,
      role: payload.role,
      media_type: payload.media_type,
      byte_length: source.byteLength,
      sha256: "0".repeat(64),
    });
  }
  const manifestBytes = Buffer.byteLength(
    canonicalize(createManifest(plan, files, builderVersion)),
    "utf8",
  );
  assert(
    manifestBytes <= 1024 * 1024,
    "generated manifest exceeds one mebibyte",
  );
  const bundleBytes = payloadBytes + manifestBytes;
  assert(
    Number.isSafeInteger(bundleBytes),
    "runtime bundle byte accounting exceeds safe integer range",
  );
  assert(
    bundleBytes <= maximumBundleBytes,
    `runtime bundle exceeds ${maximumBundleBytes} bytes`,
  );
  return sources;
}

async function copyPayload(
  sourceRoot,
  sourceRootReal,
  stageRoot,
  payload,
  expectedSource,
  currentBundleBytes,
  maximumBundleBytes,
) {
  const sourcePath = resolve(sourceRoot, ...payload.source_path.split("/"));
  assertContained(sourceRoot, sourcePath, payload.source_path);
  const sourceStatus = await lstat(sourcePath, { bigint: true });
  assert(
    sourceStatus.isFile() && !sourceStatus.isSymbolicLink(),
    `${payload.source_path}: source must be a regular file`,
  );
  assert(
    sameFileIdentity(expectedSource.identity, sourceStatus),
    `${payload.source_path}: source changed after preflight`,
  );
  assert(
    expectedSource.byteLength <= maximumBundleBytes - currentBundleBytes,
    `runtime bundle exceeds ${maximumBundleBytes} bytes`,
  );
  const sourceReal = await realpath(sourcePath);
  assertContained(sourceRootReal, sourceReal, payload.source_path);

  const destinationPath = resolve(stageRoot, ...payload.path.split("/"));
  assertContained(stageRoot, destinationPath, payload.path);
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
    before = await sourceHandle.stat({ bigint: true });
    assert(
      sameFileIdentity(expectedSource.identity, before),
      `${payload.source_path}: source changed after preflight`,
    );
    for await (const chunk of sourceHandle.createReadStream({
      autoClose: false,
    })) {
      digest.update(chunk);
      byteLength += chunk.length;
      assert(
        currentBundleBytes + byteLength <= maximumBundleBytes,
        `runtime bundle exceeds ${maximumBundleBytes} bytes`,
      );
      await writeAll(destinationHandle, chunk);
    }
    await destinationHandle.sync();
    after = await sourceHandle.stat({ bigint: true });
  } finally {
    await Promise.allSettled([sourceHandle.close(), destinationHandle.close()]);
  }
  assert(
    sameFileIdentity(before, after),
    `${payload.source_path}: source changed during build`,
  );
  const finalSourceStatus = await lstat(sourcePath, { bigint: true });
  assert(
    finalSourceStatus.isFile() && !finalSourceStatus.isSymbolicLink(),
    `${payload.source_path}: source path changed type during build`,
  );
  assert(
    sameFileIdentity(finalSourceStatus, after),
    `${payload.source_path}: source path changed during build`,
  );
  await chmod(destinationPath, 0o644);

  return {
    path: payload.path,
    role: payload.role,
    media_type: payload.media_type,
    byte_length: byteLength,
    sha256: digest.digest("hex"),
  };
}

export async function buildBundle(
  planPath,
  sourcePath,
  outputPath,
  {
    faultAt,
    maximumBundleBytes = defaultMaximumBundleBytes,
  } = {},
) {
  assert(
    faultAt === undefined || faultAt === "after-publication-before-sync",
    "unknown bundle build fault boundary",
  );
  assert(
    Number.isSafeInteger(maximumBundleBytes) && maximumBundleBytes > 0,
    "maximum bundle bytes must be a positive safe integer",
  );
  const plan = await readBuildPlan(resolve(planPath));
  const sourceRoot = resolve(sourcePath);
  const sourceStatus = await lstat(sourceRoot);
  assert(
    sourceStatus.isDirectory() && !sourceStatus.isSymbolicLink(),
    "payload source root must be a real directory",
  );
  const sourceRootReal = await realpath(sourceRoot);
  const payloads = [...plan.payloads].sort((left, right) =>
    left.path < right.path ? -1 : left.path > right.path ? 1 : 0,
  );
  const packageMetadata = strictParse(
    await readFile(resolve(contractRoot, "package.json"), "utf8"),
  );
  const preflightSources = await preflightBundle(
    sourceRoot,
    sourceRootReal,
    plan,
    payloads,
    packageMetadata.version,
    maximumBundleBytes,
  );

  const outputRoot = resolve(outputPath);
  const outputParent = dirname(outputRoot);
  const parentStatus = await lstat(outputParent);
  assert(
    parentStatus.isDirectory() && !parentStatus.isSymbolicLink(),
    "output parent must be a real directory",
  );

  const stageRoot = await mkdtemp(
    join(outputParent, `.${basename(outputRoot)}.staging-`),
  );
  const stageHandle = await openRetainedDirectory(stageRoot);
  const outputParentHandle = await openRetainedDirectory(outputParent);
  let published = false;
  let operationError;
  try {
    const files = [];
    let payloadBytes = 0;
    for (const payload of payloads) {
      const descriptor = await copyPayload(
        sourceRoot,
        sourceRootReal,
        stageRoot,
        payload,
        preflightSources.get(payload.source_path),
        payloadBytes,
        maximumBundleBytes,
      );
      files.push(descriptor);
      payloadBytes += descriptor.byte_length;
      assert(
        Number.isSafeInteger(payloadBytes),
        "runtime bundle byte accounting exceeds safe integer range",
      );
    }

    const manifest = createManifest(plan, files, packageMetadata.version);
    const manifestBytes = Buffer.from(canonicalize(manifest), "utf8");
    assert(
      manifestBytes.length <= 1024 * 1024,
      "generated manifest exceeds one mebibyte",
    );
    const bundleBytes = payloadBytes + manifestBytes.length;
    assert(
      Number.isSafeInteger(bundleBytes),
      "runtime bundle byte accounting exceeds safe integer range",
    );
    assert(
      bundleBytes <= maximumBundleBytes,
      `runtime bundle exceeds ${maximumBundleBytes} bytes`,
    );
    const manifestHandle = await open(
      join(stageRoot, "manifest.json"),
      constants.O_WRONLY | constants.O_CREAT | constants.O_EXCL,
      0o644,
    );
    try {
      await writeAll(manifestHandle, manifestBytes);
      await manifestHandle.sync();
    } finally {
      await manifestHandle.close();
    }
    await chmod(join(stageRoot, "manifest.json"), 0o644);

    await verifyBundle(stageRoot);
    const declaredFiles = [
      "manifest.json",
      ...manifest.files.map((file) => file.path),
    ];
    await syncDirectoryTree(stageRoot, declaredFiles);
    await verifyBundle(stageRoot);
    const result = {
      output_path: outputRoot,
      identity: `sha256:${createHash("sha256")
        .update(manifestBytes)
        .digest("hex")}`,
    };
    try {
      await renameDirectoryNoReplace(
        outputParentHandle,
        outputParent,
        basename(stageRoot),
        basename(outputRoot),
        "bundle",
      );
      published = true;
    } catch (error) {
      if (!new Set(["EACCES", "EEXIST", "ENOTEMPTY", "EPERM"]).has(error.code)) {
        try {
          await assertRetainedDirectory(
            stageHandle,
            outputRoot,
            "published bundle",
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
          existing = await verifyBundleArtifact(outputRoot);
        } catch (cause) {
          throw new Error(
            `output already exists and is not the requested bundle: ${outputRoot}`,
            { cause },
          );
        }
        assert.equal(
          `sha256:${existing.bundle_sha256}`,
          result.identity,
          `output already exists with a different bundle identity: ${outputRoot}`,
        );
        await syncPublishedDirectory(outputRoot);
        return result;
      }
    }
    await assertRetainedDirectory(stageHandle, outputRoot, "published bundle");
    try {
      if (faultAt === "after-publication-before-sync") {
        throw new Error("injected fault after bundle publication");
      }
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
                "bundle staging directory",
              );
            }
          },
          async () => stageHandle.close(),
          async () => outputParentHandle.close(),
        ],
        "bundle staging cleanup failed",
      );
    } catch (cleanupError) {
      throwPreservingCleanupError(
        operationError,
        cleanupError,
        "bundle build and cleanup both failed",
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
    5,
    "usage: node scripts/build-bundle.mjs <plan.json> <payload-source> <output-directory>",
  );
  const result = await buildBundle(
    process.argv[2],
    process.argv[3],
    process.argv[4],
  );
  console.log(result.identity);
}
