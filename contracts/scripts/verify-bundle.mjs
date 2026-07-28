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
import { fileURLToPath, pathToFileURL } from "node:url";

import Ajv2020 from "ajv/dist/2020.js";
import canonicalize from "canonicalize";

import {
  collectRegularFiles,
  directoryPrefixes,
} from "../lib/filesystem-tree.mjs";
import { strictParse } from "../lib/strict-json.mjs";

const contractRoot = resolve(dirname(fileURLToPath(import.meta.url)), "..");
const manifestLimitBytes = 1024 * 1024;

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
  { captureBytes = false } = {},
) {
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
  const digest = createHash("sha256");
  const chunks = captureBytes ? [] : null;
  try {
    before = await handle.stat();
    for await (const chunk of handle.createReadStream({ autoClose: false })) {
      digest.update(chunk);
      chunks?.push(chunk);
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
  assert.equal(
    digest.digest("hex"),
    descriptor.sha256,
    `${descriptor.path}: SHA-256 mismatch`,
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

  return captureBytes ? Buffer.concat(chunks, after.size) : null;
}

export async function verifyBundle(bundlePath) {
  const rootPath = resolve(bundlePath);
  const rootStatus = await lstat(rootPath);
  assert(
    rootStatus.isDirectory() && !rootStatus.isSymbolicLink(),
    "bundle root must be a real directory",
  );
  const rootRealPath = await realpath(rootPath);

  const manifestPath = join(rootPath, "manifest.json");
  const manifestStatus = await lstat(manifestPath);
  assert(
    manifestStatus.isFile() && !manifestStatus.isSymbolicLink(),
    "manifest.json must be a regular file",
  );
  assert(
    manifestStatus.size <= manifestLimitBytes,
    `manifest.json exceeds ${manifestLimitBytes} bytes`,
  );

  const manifestBytes = await readFile(manifestPath);
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

  return manifest;
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
