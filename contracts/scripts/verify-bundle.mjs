import assert from "node:assert/strict";
import { createHash } from "node:crypto";
import {
  constants,
  lstat,
  open,
  readFile,
  realpath,
} from "node:fs/promises";
import { dirname, isAbsolute, join, relative, resolve, sep } from "node:path";
import { Readable } from "node:stream";
import { fileURLToPath, pathToFileURL } from "node:url";

import Ajv2020 from "ajv/dist/2020.js";
import canonicalize from "canonicalize";

import {
  collectRegularFiles,
  directoryPrefixes,
} from "../lib/filesystem-tree.mjs";
import { readStableFile } from "../lib/durable-files.mjs";
import { strictParse } from "../lib/strict-json.mjs";

const contractRoot = resolve(dirname(fileURLToPath(import.meta.url)), "..");
const manifestLimitBytes = 1024 * 1024;
const inspectionChunkBytes = 64 * 1024;
const sha256Pattern = /^[0-9a-f]{64}$/;

function createRetainedReadStream(handle, byteLength, label) {
  async function* readChunks() {
    let position = 0;
    while (position < byteLength) {
      const chunk = Buffer.allocUnsafe(
        Math.min(inspectionChunkBytes, byteLength - position),
      );
      const { bytesRead } = await handle.read(
        chunk,
        0,
        chunk.byteLength,
        position,
      );
      assert(bytesRead > 0, `${label}: payload became shorter during inspection`);
      position += bytesRead;
      yield bytesRead === chunk.byteLength ? chunk : chunk.subarray(0, bytesRead);
    }
    const overflow = Buffer.alloc(1);
    const { bytesRead } = await handle.read(overflow, 0, 1, byteLength);
    assert.equal(
      bytesRead,
      0,
      `${label}: payload became longer during inspection`,
    );
  }
  return Readable.from(readChunks(), { objectMode: false });
}

export async function readStableRegularFile(path, label, maximumBytes) {
  return readStableFile(path, maximumBytes, label);
}

async function createManifestValidator() {
  const schemaNames = [
    "common.schema.json",
    "hardware-capabilities.schema.json",
    "runtime-manifest.schema.json",
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

function assertRuntimeInvariants(manifest) {
  const foldedPaths = manifest.files.map((file) => file.path.toLowerCase());
  assert.equal(
    new Set(foldedPaths).size,
    foldedPaths.length,
    "runtime paths must be unique after case folding",
  );
  assert(
    !foldedPaths.includes("manifest.json"),
    "manifest must not list itself",
  );
  const entrypoints = manifest.files.filter(
    (file) => file.path === manifest.entrypoint,
  );
  assert.equal(entrypoints.length, 1, "entrypoint must be listed exactly once");
  assert.equal(
    entrypoints[0].role,
    "runtime-project",
    "entrypoint must have the runtime-project role",
  );
}

export async function verifyDescribedFile(
  rootPath,
  rootRealPath,
  descriptor,
  { captureBytes = false, inspect = undefined, validate = undefined } = {},
) {
  assert(
    inspect === undefined || typeof inspect === "function",
    "described-file inspector must be a function",
  );
  assert(
    validate === undefined || typeof validate === "function",
    "described-file validator must be a function",
  );
  const absolutePath = resolve(rootPath, ...descriptor.path.split("/"));
  const relativePath = relative(rootPath, absolutePath);
  assert(
    relativePath &&
      !relativePath.startsWith(`..${sep}`) &&
      relativePath !== ".." &&
      !isAbsolute(relativePath),
    `${descriptor.path}: path escapes bundle root`,
  );

  const status = await lstat(absolutePath);
  assert(
    status.isFile() && !status.isSymbolicLink(),
    `${descriptor.path}: payload must be a regular file`,
  );
  assert.equal(
    status.size,
    descriptor.byte_length,
    `${descriptor.path}: byte length mismatch`,
  );

  const resolvedFile = await realpath(absolutePath);
  const rootPrefix = rootRealPath.endsWith(sep)
    ? rootRealPath
    : `${rootRealPath}${sep}`;
  const comparableFile =
    process.platform === "win32" ? resolvedFile.toLowerCase() : resolvedFile;
  const comparableRoot =
    process.platform === "win32" ? rootPrefix.toLowerCase() : rootPrefix;
  assert(
    comparableFile.startsWith(comparableRoot),
    `${descriptor.path}: resolved path escapes bundle root`,
  );

  const noFollow = constants.O_NOFOLLOW ?? 0;
  const handle = await open(absolutePath, constants.O_RDONLY | noFollow);
  let before;
  let after;
  let inspection;
  const digest = createHash("sha256");
  const chunks = captureBytes ? [] : null;
  try {
    before = await handle.stat();
    let observedBytes = 0;
    for await (const chunk of handle.createReadStream({ autoClose: false })) {
      observedBytes += chunk.byteLength;
      assert(
        observedBytes <= descriptor.byte_length,
        `${descriptor.path}: payload exceeded declared byte length while reading`,
      );
      digest.update(chunk);
      chunks?.push(chunk);
    }
    assert.equal(
      observedBytes,
      descriptor.byte_length,
      `${descriptor.path}: payload byte length changed during verification`,
    );
    assert.equal(
      digest.digest("hex"),
      descriptor.sha256,
      `${descriptor.path}: SHA-256 mismatch`,
    );
    if (inspect) {
      const stream = createRetainedReadStream(
        handle,
        descriptor.byte_length,
        descriptor.path,
      );
      try {
        inspection = await inspect(stream, {
          byteLength: descriptor.byte_length,
          handle,
          mediaType: descriptor.media_type,
          path: descriptor.path,
        });
      } finally {
        stream.destroy();
      }
    }
    if (validate) {
      await validate({
        absolutePath,
        byteLength: descriptor.byte_length,
        handle,
        mediaType: descriptor.media_type,
        path: descriptor.path,
      });
    }
    after = await handle.stat();
  } finally {
    await handle.close();
  }
  assert(
    before.isFile() && after.isFile(),
    `${descriptor.path}: payload changed type during verification`,
  );
  assert.equal(
    before.size,
    after.size,
    `${descriptor.path}: payload size changed during verification`,
  );
  assert.equal(
    before.mtimeMs,
    after.mtimeMs,
    `${descriptor.path}: payload changed during verification`,
  );
  assert.equal(
    before.ctimeMs,
    after.ctimeMs,
    `${descriptor.path}: payload metadata changed during verification`,
  );
  const finalStatus = await lstat(absolutePath);
  assert(
    finalStatus.isFile() && !finalStatus.isSymbolicLink(),
    `${descriptor.path}: payload path changed type during verification`,
  );
  assert.equal(
    finalStatus.dev,
    after.dev,
    `${descriptor.path}: payload path changed during verification`,
  );
  assert.equal(
    finalStatus.ino,
    after.ino,
    `${descriptor.path}: payload path changed during verification`,
  );
  assert.equal(
    finalStatus.size,
    after.size,
    `${descriptor.path}: payload path changed size during verification`,
  );
  assert.equal(
    finalStatus.mtimeMs,
    after.mtimeMs,
    `${descriptor.path}: payload path changed during verification`,
  );

  if (inspect) {
    return {
      bytes: captureBytes ? Buffer.concat(chunks, after.size) : null,
      inspection,
    };
  }
  return captureBytes ? Buffer.concat(chunks, after.size) : null;
}

function checkedBundleByteLength(manifestBytes, manifest) {
  let byteLength = manifestBytes.byteLength;
  for (const descriptor of manifest.files) {
    byteLength += descriptor.byte_length;
    assert(
      Number.isSafeInteger(byteLength),
      "deployment bundle byte accounting exceeds safe integer range",
    );
  }
  return byteLength;
}

function freezeManifest(value) {
  if (value && typeof value === "object" && !Object.isFrozen(value)) {
    Object.values(value).forEach(freezeManifest);
    Object.freeze(value);
  }
  return value;
}

export async function verifyBundleArtifact(
  bundlePath,
  {
    maximumBundleBytes = Number.MAX_SAFE_INTEGER,
    expectedBundleSha256 = undefined,
    expectedBundleByteLength = undefined,
  } = {},
) {
  assert(
    Number.isSafeInteger(maximumBundleBytes) && maximumBundleBytes >= 0,
    "maximum bundle bytes must be a non-negative safe integer",
  );
  assert(
    expectedBundleSha256 === undefined ||
      (typeof expectedBundleSha256 === "string" &&
        sha256Pattern.test(expectedBundleSha256)),
    "expected bundle SHA-256 must be a lowercase hexadecimal digest",
  );
  assert(
    expectedBundleByteLength === undefined ||
      (Number.isSafeInteger(expectedBundleByteLength) &&
        expectedBundleByteLength >= 0 &&
        expectedBundleByteLength <= maximumBundleBytes),
    "expected bundle byte length must fit the verification limit",
  );
  const rootPath = resolve(bundlePath);
  const rootStatus = await lstat(rootPath);
  assert(
    rootStatus.isDirectory() && !rootStatus.isSymbolicLink(),
    "bundle root must be a real directory",
  );
  const rootRealPath = await realpath(rootPath);

  const manifestPath = join(rootPath, "manifest.json");
  const manifestBytes = await readStableRegularFile(
    manifestPath,
    "manifest.json",
    Math.min(manifestLimitBytes, maximumBundleBytes),
  );
  assert(
    !manifestBytes.subarray(0, 3).equals(Buffer.from([0xef, 0xbb, 0xbf])),
    "manifest.json must not contain a byte-order mark",
  );
  let manifestText;
  try {
    manifestText = new TextDecoder("utf-8", { fatal: true }).decode(
      manifestBytes,
    );
  } catch (error) {
    throw new Error("manifest.json is not valid UTF-8", { cause: error });
  }
  const manifest = strictParse(manifestText);
  const canonicalBytes = Buffer.from(canonicalize(manifest), "utf8");
  assert(
    manifestBytes.equals(canonicalBytes),
    "manifest.json is not canonical RFC 8785 JSON",
  );

  const validate = await createManifestValidator();
  assert(validate, "runtime manifest schema was not registered");
  assert(
    validate(manifest),
    `manifest schema validation failed: ${JSON.stringify(validate.errors)}`,
  );
  assertRuntimeInvariants(manifest);

  const bundleSha256 = createHash("sha256")
    .update(manifestBytes)
    .digest("hex");
  const bundleByteLength = checkedBundleByteLength(manifestBytes, manifest);
  assert(
    bundleByteLength <= maximumBundleBytes,
    `deployment bundle exceeds ${maximumBundleBytes} bytes`,
  );
  if (expectedBundleSha256 !== undefined) {
    assert.equal(
      bundleSha256,
      expectedBundleSha256,
      "signed deployment request bundle identity mismatch",
    );
  }
  if (expectedBundleByteLength !== undefined) {
    assert.equal(
      bundleByteLength,
      expectedBundleByteLength,
      "signed deployment request bundle byte length mismatch",
    );
  }

  const declaredFiles = new Set([
    "manifest.json",
    ...manifest.files.map(({ path }) => path),
  ]);
  const declaredDirectories = directoryPrefixes(declaredFiles);
  const actualFiles = await collectRegularFiles(rootPath, {
    maximumEntries: declaredFiles.size + declaredDirectories.size,
    maximumDirectories: declaredDirectories.size + 1,
    maximumFiles: declaredFiles.size,
    maximumPathBytes: [...declaredFiles, ...declaredDirectories].reduce(
      (total, path) => total + Buffer.byteLength(path, "utf8"),
      0,
    ),
    allowDirectory: (path) => declaredDirectories.has(path),
    allowFile: (path) => declaredFiles.has(path),
  });
  const actualPayloads = actualFiles
    .filter((path) => path !== "manifest.json")
    .sort();
  const declaredPayloads = manifest.files.map((file) => file.path).sort();
  assert.deepEqual(
    actualPayloads,
    declaredPayloads,
    "bundle contents do not exactly match manifest",
  );

  for (const descriptor of manifest.files) {
    await verifyDescribedFile(rootPath, rootRealPath, descriptor);
  }

  assert.equal(
    manifest.entrypoint,
    "project.json",
    "version 1 runtime entrypoint must be project.json",
  );
  const entrypoint = manifest.files.find(
    (file) => file.path === manifest.entrypoint,
  );
  assert.equal(
    entrypoint.sha256,
    manifest.source_project_sha256,
    "runtime entrypoint must preserve the canonical source project manifest",
  );
  const { verifyProject } = await import("./verify-project.mjs");
  const verifiedProject = await verifyProject(rootPath, {
    ignoredManagedPaths: ["manifest.json"],
  });
  assert.equal(
    verifiedProject.project.project_id,
    manifest.project_id,
    "runtime project identifier does not match bundle manifest",
  );
  assert.equal(
    verifiedProject.project.revision,
    manifest.project_revision,
    "runtime project revision does not match bundle manifest",
  );
  assert.deepEqual(
    verifiedProject.project.required_capabilities,
    manifest.required_capabilities,
    "runtime project capabilities do not match bundle manifest",
  );
  const descriptorsByPath = new Map(
    manifest.files.map((descriptor) => [descriptor.path, descriptor]),
  );
  assert.equal(
    descriptorsByPath.get("project.json")?.role,
    "runtime-project",
    "project.json must have the runtime-project role",
  );
  for (const resource of verifiedProject.project.resources) {
    assert.equal(
      descriptorsByPath.get(resource.path)?.role,
      "runtime-data",
      `${resource.path}: project resource must have the runtime-data role`,
    );
  }
  for (const asset of verifiedProject.project.assets) {
    assert.equal(
      descriptorsByPath.get(asset.path)?.role,
      "audio",
      `${asset.path}: project asset must have the audio role`,
    );
  }

  const finalFiles = (await collectRegularFiles(rootPath))
    .filter((path) => path !== "manifest.json")
    .sort();
  assert.deepEqual(
    finalFiles,
    declaredPayloads,
    "bundle contents changed during verification",
  );
  assert(
    (await readFile(manifestPath)).equals(manifestBytes),
    "manifest.json changed during verification",
  );

  return Object.freeze({
    manifest: freezeManifest(manifest),
    bundle_sha256: bundleSha256,
    bundle_byte_length: bundleByteLength,
  });
}

export async function verifyBundle(bundlePath) {
  return (await verifyBundleArtifact(bundlePath)).manifest;
}

const invokedPath = process.argv[1]
  ? pathToFileURL(resolve(process.argv[1])).href
  : undefined;
if (invokedPath === import.meta.url) {
  assert.equal(
    process.argv.length,
    3,
    "usage: node scripts/verify-bundle.mjs <bundle-directory>",
  );
  await verifyBundle(process.argv[2]);
  console.log("Runtime bundle verification passed");
}
