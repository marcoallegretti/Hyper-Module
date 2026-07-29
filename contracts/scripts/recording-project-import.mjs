import assert from "node:assert/strict";
import { createHash, randomUUID } from "node:crypto";
import {
  link,
  lstat,
  mkdir,
  open,
  readFile,
  readdir,
  rename,
  rm,
  rmdir,
  unlink,
} from "node:fs/promises";
import { isAbsolute, join, resolve } from "node:path";

import canonicalize from "canonicalize";

import {
  syncDirectoryTree,
  syncPublishedDirectory,
  syncPublishedEntry,
  syncRemovedEntry,
} from "../lib/durable-files.mjs";
import { collectRegularFiles } from "../lib/filesystem-tree.mjs";
import { assertRecordingMetadataInvariants } from "../lib/recording-metadata.mjs";
import { strictParse } from "../lib/strict-json.mjs";
import { readStableRegularFile } from "./verify-bundle.mjs";
import {
  verifyProject,
  verifyProjectCandidate,
} from "./verify-project.mjs";

const MAX_MANIFEST_BYTES = 1024 * 1024;
const MAX_HANDOFF_BYTES = 2 * 1024 * 1024;
const MAX_RECORDING_BYTES = 4_294_967_295;
const MAX_SAFE_INTEGER = 9_007_199_254_740_991;
const ID_PATTERN =
  /^[0-9a-f]{8}-[0-9a-f]{4}-[0-9a-f]{4}-[0-9a-f]{4}-[0-9a-f]{12}$/;
const SHA256_PATTERN = /^[0-9a-f]{64}$/;
const EXTENSION_KEY_PATTERN =
  /^[a-z0-9](?:[a-z0-9-]*[a-z0-9])?(?:\.[a-z0-9](?:[a-z0-9-]*[a-z0-9])?)+$/;
const TRANSACTION_PATTERN = new RegExp(
  `^\\.recording-import-(${ID_PATTERN.source.slice(1, -1)})$`,
);
const TRANSACTION_FILES = [
  "metadata.json",
  "previous-project.json",
  "project.json",
  "recording.wav",
  "transaction.json",
];
const FAULT_BOUNDARIES = new Set([
  "after-transaction-sync",
  "after-recording-publication",
  "after-metadata-publication",
  "after-candidate-verification",
  "after-manifest-sync",
  "after-manifest-publication",
]);
const HANDOFF_KEYS = [
  "extensions",
  "format",
  "format_version",
  "metadata",
  "recording",
];
const METADATA_KEYS = [
  "asset_id",
  "capture_kind",
  "captured_at_utc",
  "channels",
  "extensions",
  "format",
  "format_version",
  "frame_count",
  "id",
  "project_id",
  "sample_rate_hz",
  "source_input_indices",
  "timeline_start_tick",
  "track_id",
];

export class InjectedRecordingImportFault extends Error {
  constructor(boundary) {
    super(`injected recording import fault at ${boundary}`);
    this.code = "HYPER_INJECTED_RECORDING_IMPORT_FAULT";
    this.boundary = boundary;
  }
}

function injectFault(faultAt, boundary) {
  if (faultAt === boundary) {
    throw new InjectedRecordingImportFault(boundary);
  }
}

function validateFaultBoundary(faultAt) {
  assert(
    faultAt === undefined || FAULT_BOUNDARIES.has(faultAt),
    "faultAt must name a supported recording import boundary",
  );
}

function sha256(bytes) {
  return createHash("sha256").update(bytes).digest("hex");
}

function validateDescriptor(descriptor, label) {
  assert(
    descriptor !== null && typeof descriptor === "object",
    `${label} descriptor is required`,
  );
  assert(
    Number.isSafeInteger(descriptor.byte_length) &&
      descriptor.byte_length > 0 &&
      descriptor.byte_length <= MAX_RECORDING_BYTES,
    `${label} byte length is invalid`,
  );
  assert(
    typeof descriptor.sha256 === "string" &&
      SHA256_PATTERN.test(descriptor.sha256),
    `${label} SHA-256 is invalid`,
  );
  return {
    byte_length: descriptor.byte_length,
    sha256: descriptor.sha256,
  };
}

async function stableFileDescriptor(path, maximumBytes) {
  const pathBefore = await lstat(path);
  assert(
    pathBefore.isFile() && !pathBefore.isSymbolicLink(),
    `${path} must be a real regular file`,
  );
  assert(pathBefore.size <= maximumBytes, `${path} exceeds its size limit`);
  const handle = await open(path, "r");
  const digest = createHash("sha256");
  let byteLength = 0;
  const buffer = Buffer.allocUnsafe(64 * 1024);
  let before;
  let after;
  try {
    before = await handle.stat();
    for (;;) {
      const { bytesRead } = await handle.read(buffer, 0, buffer.length, null);
      if (bytesRead === 0) break;
      byteLength += bytesRead;
      assert(
        Number.isSafeInteger(byteLength) && byteLength <= maximumBytes,
        `${path} exceeds its size limit`,
      );
      digest.update(buffer.subarray(0, bytesRead));
    }
    after = await handle.stat();
  } finally {
    await handle.close();
  }
  const pathAfter = await lstat(path);
  for (const [name, left, right] of [
    ["device", before.dev, after.dev],
    ["inode", before.ino, after.ino],
    ["size", before.size, after.size],
    ["modification time", before.mtimeMs, after.mtimeMs],
    ["change time", before.ctimeMs, after.ctimeMs],
    ["path device", after.dev, pathAfter.dev],
    ["path inode", after.ino, pathAfter.ino],
    ["path size", after.size, pathAfter.size],
    ["path modification time", after.mtimeMs, pathAfter.mtimeMs],
  ]) {
    assert.equal(left, right, `${path} changed ${name} during reading`);
  }
  assert(
    pathAfter.isFile() && !pathAfter.isSymbolicLink(),
    `${path} changed type during reading`,
  );
  assert.equal(byteLength, after.size, `${path} changed size during reading`);
  return { byte_length: byteLength, sha256: digest.digest("hex") };
}

async function writeExclusiveSynced(path, bytes) {
  const handle = await open(path, "wx", 0o600);
  try {
    await handle.writeFile(bytes);
    await handle.sync();
  } finally {
    await handle.close();
  }
}

async function copyVerifiedRecording(sourcePath, destinationPath, expected) {
  const pathBefore = await lstat(sourcePath);
  assert(
    pathBefore.isFile() && !pathBefore.isSymbolicLink(),
    "recording source must be a real regular file",
  );
  const source = await open(sourcePath, "r");
  let destination;
  try {
    destination = await open(destinationPath, "wx", 0o600);
  } catch (error) {
    await source.close();
    throw error;
  }
  const digest = createHash("sha256");
  const buffer = Buffer.allocUnsafe(64 * 1024);
  let byteLength = 0;
  let before;
  let after;
  try {
    before = await source.stat();
    assert(before.isFile(), "recording source must be a regular file");
    for (;;) {
      const { bytesRead } = await source.read(buffer, 0, buffer.length, null);
      if (bytesRead === 0) break;
      byteLength += bytesRead;
      assert(
        Number.isSafeInteger(byteLength) && byteLength <= MAX_RECORDING_BYTES,
        "recording source exceeds the WAV container limit",
      );
      digest.update(buffer.subarray(0, bytesRead));
      let offset = 0;
      while (offset < bytesRead) {
        const result = await destination.write(
          buffer,
          offset,
          bytesRead - offset,
          null,
        );
        assert(result.bytesWritten > 0, "recording staging write stalled");
        offset += result.bytesWritten;
      }
    }
    await destination.sync();
    after = await source.stat();
  } finally {
    await Promise.all([source.close(), destination.close()]);
  }
  const pathAfter = await lstat(sourcePath);
  assert.equal(before.dev, after.dev, "recording source changed device");
  assert.equal(before.ino, after.ino, "recording source changed identity");
  assert.equal(before.size, after.size, "recording source changed size");
  assert.equal(before.mtimeMs, after.mtimeMs, "recording source changed");
  assert.equal(before.ctimeMs, after.ctimeMs, "recording source changed");
  assert(
    pathAfter.isFile() && !pathAfter.isSymbolicLink(),
    "recording source changed type",
  );
  assert.equal(after.dev, pathAfter.dev, "recording source path changed device");
  assert.equal(after.ino, pathAfter.ino, "recording source path changed identity");
  assert.equal(after.size, pathAfter.size, "recording source path changed size");
  const actual = { byte_length: byteLength, sha256: digest.digest("hex") };
  assert.deepEqual(actual, expected, "recording publication descriptor mismatch");
  assert.deepEqual(
    await stableFileDescriptor(destinationPath, MAX_RECORDING_BYTES),
    expected,
    "staged recording descriptor mismatch",
  );
}

function lockOwner() {
  return {
    format: "hyper-project-storage-lock",
    format_version: 1,
    owner_pid: process.pid,
    owner_token: randomUUID(),
    extensions: {},
  };
}

function validateLockOwner(owner) {
  assert.equal(owner?.format, "hyper-project-storage-lock");
  assert.equal(owner?.format_version, 1);
  assert(
    Number.isSafeInteger(owner?.owner_pid) && owner.owner_pid > 0,
    "project storage lock PID is invalid",
  );
  assert(
    typeof owner.owner_token === "string" &&
      ID_PATTERN.test(owner.owner_token),
    "project storage lock token is invalid",
  );
  assert.deepEqual(owner.extensions, {});
  assert.deepEqual(Object.keys(owner).sort(), [
    "extensions",
    "format",
    "format_version",
    "owner_pid",
    "owner_token",
  ]);
  return owner;
}

async function withProjectLock(backupsRoot, operation) {
  const lockPath = join(backupsRoot, "project-storage.lock");
  const ownerBytes = Buffer.from(canonicalize(lockOwner()), "utf8");
  let handle;
  try {
    handle = await open(lockPath, "wx", 0o600);
  } catch (error) {
    if (error.code === "EEXIST") {
      throw new Error("another project storage transaction is active");
    }
    throw error;
  }
  try {
    await handle.writeFile(ownerBytes);
    await handle.sync();
    await syncPublishedEntry(lockPath);
  } catch (error) {
    await handle.close().catch(() => {});
    await unlink(lockPath).catch(() => {});
    throw error;
  }

  let result;
  let operationError;
  try {
    result = await operation();
  } catch (error) {
    operationError = error;
  }
  let releaseError;
  try {
    await handle.close();
    assert(
      (await readFile(lockPath)).equals(ownerBytes),
      "project storage lock ownership was compromised",
    );
    await unlink(lockPath);
    await syncRemovedEntry(backupsRoot);
  } catch (error) {
    releaseError = error;
  }
  if (operationError && releaseError) {
    throw new AggregateError(
      [operationError, releaseError],
      "project transaction and lock release both failed",
    );
  }
  if (operationError) throw operationError;
  if (releaseError) throw releaseError;
  return result;
}

async function ensureBackupsRoot(projectPath) {
  const root = resolve(projectPath);
  const status = await lstat(root);
  assert(
    status.isDirectory() && !status.isSymbolicLink(),
    "project root must be a real directory",
  );
  const backupsRoot = join(root, "backups");
  let created = false;
  try {
    await mkdir(backupsRoot);
    created = true;
  } catch (error) {
    if (error.code !== "EEXIST") throw error;
    const backupStatus = await lstat(backupsRoot);
    assert(
      backupStatus.isDirectory() && !backupStatus.isSymbolicLink(),
      "project backups root must be a real directory",
    );
  }
  if (created) await syncPublishedDirectory(backupsRoot);
  return { root, backupsRoot };
}

function metadataValue(metadataBytes) {
  assert(Buffer.isBuffer(metadataBytes), "recording metadata must be a Buffer");
  assert(
    metadataBytes.length > 0 && metadataBytes.length <= MAX_MANIFEST_BYTES,
    "recording metadata size is invalid",
  );
  const metadata = strictParse(
    new TextDecoder("utf-8", { fatal: true }).decode(metadataBytes),
  );
  assert.deepEqual(Object.keys(metadata).sort(), METADATA_KEYS);
  assert(
    metadataBytes.equals(Buffer.from(canonicalize(metadata), "utf8")),
    "recording metadata is not canonical RFC 8785 JSON",
  );
  assert.equal(metadata.format, "hyper-recording-metadata");
  assert.equal(metadata.format_version, 1);
  for (const field of ["id", "project_id", "asset_id", "track_id"]) {
    assert(
      typeof metadata[field] === "string" && ID_PATTERN.test(metadata[field]),
      `recording metadata ${field} is invalid`,
    );
  }
  assert(
    metadata.capture_kind === "new" || metadata.capture_kind === "overdub",
    "recording metadata capture kind is invalid",
  );
  assert(
    Number.isSafeInteger(metadata.timeline_start_tick) &&
      metadata.timeline_start_tick >= 0 &&
      metadata.timeline_start_tick <= MAX_SAFE_INTEGER,
    "recording metadata timeline start is invalid",
  );
  assert.equal(metadata.sample_rate_hz, 48_000);
  assert(
    Number.isSafeInteger(metadata.channels) &&
      metadata.channels >= 1 &&
      metadata.channels <= 2,
    "recording metadata channel count is invalid",
  );
  assert(
    Number.isSafeInteger(metadata.frame_count) &&
      metadata.frame_count > 0 &&
      metadata.frame_count <= MAX_SAFE_INTEGER,
    "recording metadata frame count is invalid",
  );
  assert(
    Array.isArray(metadata.source_input_indices) &&
      metadata.source_input_indices.length >= 1 &&
      metadata.source_input_indices.length <= 2 &&
      metadata.source_input_indices.every(
        (index) => Number.isSafeInteger(index) && index >= 0 && index <= 1,
      ),
    "recording metadata source inputs are invalid",
  );
  assert(
    metadata.extensions !== null &&
      typeof metadata.extensions === "object" &&
      !Array.isArray(metadata.extensions) &&
      Object.keys(metadata.extensions).every((key) =>
        EXTENSION_KEY_PATTERN.test(key),
      ),
    "recording metadata extensions are invalid",
  );
  assertRecordingMetadataInvariants(metadata);
  return metadata;
}

export function decodeFinalizedRecordingHandoff(handoffBytes) {
  assert(Buffer.isBuffer(handoffBytes), "recording handoff must be a Buffer");
  assert(
    handoffBytes.length > 0 && handoffBytes.length <= MAX_HANDOFF_BYTES,
    "recording handoff size is invalid",
  );
  const handoff = strictParse(
    new TextDecoder("utf-8", { fatal: true }).decode(handoffBytes),
  );
  assert(
    handoffBytes.equals(Buffer.from(canonicalize(handoff), "utf8")),
    "recording handoff is not canonical RFC 8785 JSON",
  );
  assert.deepEqual(Object.keys(handoff).sort(), HANDOFF_KEYS);
  assert.equal(handoff.format, "hyper-finalized-recording-handoff");
  assert.equal(handoff.format_version, 1);
  assert.deepEqual(handoff.extensions, {});
  assert.deepEqual(Object.keys(handoff.recording).sort(), [
    "byte_length",
    "media_type",
    "path",
    "sha256",
  ]);
  assert.equal(handoff.recording.media_type, "audio/wav");
  assert(
    typeof handoff.recording.path === "string" &&
      handoff.recording.path.length > 0 &&
      Buffer.byteLength(handoff.recording.path, "utf8") <= 4096 &&
      !handoff.recording.path.includes("\0"),
    "recording handoff path is invalid",
  );
  assert(
    isAbsolute(handoff.recording.path),
    "recording handoff path must be absolute",
  );
  const recordingDescriptor = validateDescriptor(
    handoff.recording,
    "recording handoff",
  );
  const metadataBytes = Buffer.from(canonicalize(handoff.metadata), "utf8");
  metadataValue(metadataBytes);
  return {
    recordingPath: resolve(handoff.recording.path),
    recordingDescriptor,
    metadataBytes,
  };
}

export async function importFinalizedRecordingHandoff(
  projectPath,
  handoffBytes,
  { faultAt = undefined } = {},
) {
  return importFinalizedRecording(projectPath, {
    ...decodeFinalizedRecordingHandoff(handoffBytes),
    faultAt,
  });
}

async function missingRecordingDirectories(root) {
  const paths = ["assets", "assets/recordings"];
  const missing = [];
  for (const path of paths) {
    try {
      const status = await lstat(join(root, ...path.split("/")));
      assert(
        status.isDirectory() && !status.isSymbolicLink(),
        `${path} must be a real directory`,
      );
    } catch (error) {
      if (error.code !== "ENOENT") throw error;
      missing.push(path);
    }
  }
  return missing;
}

function transactionValue({
  id,
  beforeIdentity,
  afterIdentity,
  recording,
  metadata,
  createdDirectories,
}) {
  return {
    format: "hyper-recording-import-transaction",
    format_version: 1,
    id,
    before_project_sha256: beforeIdentity.slice("sha256:".length),
    after_project_sha256: afterIdentity,
    recording,
    metadata,
    created_directories: createdDirectories,
    extensions: {},
  };
}

function validateTransaction(value, directoryId) {
  assert.deepEqual(Object.keys(value).sort(), [
    "after_project_sha256",
    "before_project_sha256",
    "created_directories",
    "extensions",
    "format",
    "format_version",
    "id",
    "metadata",
    "recording",
  ]);
  assert.equal(value?.format, "hyper-recording-import-transaction");
  assert.equal(value?.format_version, 1);
  assert.equal(value?.id, directoryId);
  assert(SHA256_PATTERN.test(value.before_project_sha256));
  assert(SHA256_PATTERN.test(value.after_project_sha256));
  for (const [name, role, mediaType] of [
    ["recording", "recording", "audio/wav"],
    ["metadata", "recording-metadata", "application/json"],
  ]) {
    const record = value[name];
    assert.deepEqual(Object.keys(record).sort(), [
      "byte_length",
      "extensions",
      "id",
      "media_type",
      "path",
      "role",
      "sha256",
    ]);
    assert(record && ID_PATTERN.test(record.id));
    assert.equal(record.role, role);
    assert.equal(record.media_type, mediaType);
    assert(Number.isSafeInteger(record.byte_length) && record.byte_length > 0);
    assert(SHA256_PATTERN.test(record.sha256));
    assert.deepEqual(record.extensions, {});
  }
  assert.equal(
    value.recording.path,
    `assets/recordings/${value.recording.id}.wav`,
  );
  assert.equal(
    value.metadata.path,
    `assets/recordings/${value.metadata.id}.json`,
  );
  assert(
    Array.isArray(value.created_directories) &&
      value.created_directories.every((path) =>
        ["assets", "assets/recordings"].includes(path),
      ),
  );
  assert.equal(
    new Set(value.created_directories).size,
    value.created_directories.length,
  );
  assert.deepEqual(
    value.created_directories,
    value.created_directories.includes("assets")
      ? ["assets", "assets/recordings"]
      : value.created_directories,
  );
  assert.deepEqual(value.extensions, {});
  return value;
}

async function publishExclusive(source, destination) {
  await link(source, destination);
  await syncPublishedEntry(destination);
}

async function removeMatchingFile(root, record) {
  const path = join(root, ...record.path.split("/"));
  try {
    assert.deepEqual(
      await stableFileDescriptor(path, MAX_RECORDING_BYTES),
      { byte_length: record.byte_length, sha256: record.sha256 },
      `${record.path}: recovery refuses changed content`,
    );
  } catch (error) {
    if (error.code === "ENOENT") return;
    throw error;
  }
  await unlink(path);
  await syncRemovedEntry(join(root, ...record.path.split("/").slice(0, -1)));
}

async function removeTransactionDirectory(transactionRoot) {
  await collectRegularFiles(transactionRoot, {
    maximumEntries: TRANSACTION_FILES.length,
    maximumDirectories: 1,
    maximumFiles: TRANSACTION_FILES.length,
    maximumDepth: 1,
    maximumPathBytes: 256,
    allowDirectory: () => false,
    allowFile: (path) => TRANSACTION_FILES.includes(path),
  });
  await rm(transactionRoot, { recursive: true, force: false });
  await syncRemovedEntry(resolve(transactionRoot, ".."));
}

async function readTransaction(transactionRoot, directoryId) {
  const bytes = await readStableRegularFile(
    join(transactionRoot, "transaction.json"),
    "recording import transaction",
    MAX_MANIFEST_BYTES,
  );
  const value = strictParse(
    new TextDecoder("utf-8", { fatal: true }).decode(bytes),
  );
  assert(
    bytes.equals(Buffer.from(canonicalize(value), "utf8")),
    "recording import transaction is not canonical",
  );
  return validateTransaction(value, directoryId);
}

async function rollbackTransaction(root, backupsRoot, transactionRoot, tx) {
  const activeBytes = await readStableRegularFile(
    join(root, "project.json"),
    "project.json",
    MAX_MANIFEST_BYTES,
  );
  assert.equal(
    sha256(activeBytes),
    tx.before_project_sha256,
    "recording import rollback requires the original project identity",
  );
  await removeMatchingFile(root, tx.metadata);
  await removeMatchingFile(root, tx.recording);
  const manifestTemporary = join(backupsRoot, `.project-${tx.id}.json`);
  await rm(manifestTemporary, { force: true });
  await syncRemovedEntry(backupsRoot);
  for (const path of [...tx.created_directories].reverse()) {
    const absolutePath = join(root, ...path.split("/"));
    try {
      await rmdir(absolutePath);
      await syncRemovedEntry(resolve(absolutePath, ".."));
    } catch (error) {
      if (error.code !== "ENOENT") throw error;
    }
  }
  await removeTransactionDirectory(transactionRoot);
  const verified = await verifyProject(root);
  assert.equal(
    verified.identity,
    `sha256:${tx.before_project_sha256}`,
    "recording import rollback changed project identity",
  );
  return "rolled-back";
}

async function recoverTransaction(root, backupsRoot, transactionRoot, id) {
  const tx = await readTransaction(transactionRoot, id);
  let activeBytes;
  try {
    activeBytes = await readStableRegularFile(
      join(root, "project.json"),
      "project.json",
      MAX_MANIFEST_BYTES,
    );
  } catch (error) {
    if (error.code !== "ENOENT") throw error;
    const previousPath = join(transactionRoot, "previous-project.json");
    const previous = await readStableRegularFile(
      previousPath,
      "previous project manifest",
      MAX_MANIFEST_BYTES,
    );
    assert.equal(
      sha256(previous),
      tx.before_project_sha256,
      "previous project manifest does not match transaction identity",
    );
    const restorePath = join(backupsRoot, `.project-${tx.id}.json`);
    await rm(restorePath, { force: true });
    await link(previousPath, restorePath);
    await syncPublishedEntry(restorePath);
    await rename(restorePath, join(root, "project.json"));
    await syncPublishedEntry(join(root, "project.json"));
    activeBytes = previous;
  }
  const active = sha256(activeBytes);
  if (active === tx.before_project_sha256) {
    return rollbackTransaction(root, backupsRoot, transactionRoot, tx);
  }
  assert.equal(
    active,
    tx.after_project_sha256,
    "recording import recovery found an unrelated project identity",
  );
  const verified = await verifyProject(root);
  assert.equal(
    verified.identity,
    `sha256:${tx.after_project_sha256}`,
    "committed recording import does not verify",
  );
  await rm(join(backupsRoot, `.project-${tx.id}.json`), { force: true });
  await removeTransactionDirectory(transactionRoot);
  return "committed";
}

export async function importFinalizedRecording(
  projectPath,
  {
    recordingPath,
    recordingDescriptor,
    metadataBytes,
    faultAt = undefined,
  },
) {
  validateFaultBoundary(faultAt);
  const expectedRecording = validateDescriptor(
    recordingDescriptor,
    "recording",
  );
  const metadata = metadataValue(metadataBytes);
  const { root, backupsRoot } = await ensureBackupsRoot(projectPath);
  return withProjectLock(backupsRoot, async () => {
    const verified = await verifyProject(root);
    const project = verified.project;
    assert.equal(metadata.project_id, project.project_id);
    assert(
      project.revision < Number.MAX_SAFE_INTEGER,
      "project revision cannot be incremented",
    );
    const recordingPathInProject =
      `assets/recordings/${metadata.asset_id}.wav`;
    const metadataPathInProject = `assets/recordings/${metadata.id}.json`;
    const identifiers = new Set([
      ...project.resources.map(({ id }) => id),
      ...project.assets.map(({ id }) => id),
    ]);
    assert(!identifiers.has(metadata.id), "metadata identity already exists");
    assert(!identifiers.has(metadata.asset_id), "recording identity already exists");
    const foldedPaths = new Set(
      [...project.resources, ...project.assets].map(({ path }) =>
        path.toLowerCase(),
      ),
    );
    assert(
      !foldedPaths.has(recordingPathInProject.toLowerCase()) &&
        !foldedPaths.has(metadataPathInProject.toLowerCase()),
      "recording import path already exists",
    );

    const id = randomUUID();
    const transactionRoot = join(backupsRoot, `.recording-import-${id}`);
    await mkdir(transactionRoot);
    const stageRecording = join(transactionRoot, "recording.wav");
    const stageMetadata = join(transactionRoot, "metadata.json");
    const stageProject = join(transactionRoot, "project.json");
    const previousProject = join(transactionRoot, "previous-project.json");
    const transactionPath = join(transactionRoot, "transaction.json");
    let transactionWritten = false;
    try {
      await copyVerifiedRecording(
        resolve(recordingPath),
        stageRecording,
        expectedRecording,
      );
      await writeExclusiveSynced(stageMetadata, metadataBytes);
      const metadataDescriptor = {
        id: metadata.id,
        role: "recording-metadata",
        path: metadataPathInProject,
        media_type: "application/json",
        byte_length: metadataBytes.length,
        sha256: sha256(metadataBytes),
        extensions: {},
      };
      const recordingRecord = {
        id: metadata.asset_id,
        role: "recording",
        path: recordingPathInProject,
        media_type: "audio/wav",
        ...expectedRecording,
        extensions: {},
      };
      const candidateProject = structuredClone(project);
      candidateProject.revision += 1;
      candidateProject.resources.push(metadataDescriptor);
      candidateProject.assets.push(recordingRecord);
      const candidateBytes = Buffer.from(canonicalize(candidateProject), "utf8");
      assert(
        candidateBytes.length <= MAX_MANIFEST_BYTES,
        "candidate project manifest exceeds its size limit",
      );
      const activeBytes = await readStableRegularFile(
        join(root, "project.json"),
        "project.json",
        MAX_MANIFEST_BYTES,
      );
      assert.equal(
        `sha256:${sha256(activeBytes)}`,
        verified.identity,
        "active project changed before recording staging",
      );
      await writeExclusiveSynced(stageProject, candidateBytes);
      await writeExclusiveSynced(previousProject, activeBytes);
      const createdDirectories = await missingRecordingDirectories(root);
      const tx = transactionValue({
        id,
        beforeIdentity: verified.identity,
        afterIdentity: sha256(candidateBytes),
        recording: recordingRecord,
        metadata: metadataDescriptor,
        createdDirectories,
      });
      await writeExclusiveSynced(
        transactionPath,
        Buffer.from(canonicalize(tx), "utf8"),
      );
      transactionWritten = true;
      await syncDirectoryTree(transactionRoot, TRANSACTION_FILES);
      await syncPublishedDirectory(transactionRoot);
      injectFault(faultAt, "after-transaction-sync");

      await mkdir(join(root, "assets", "recordings"), { recursive: true });
      for (const path of createdDirectories) {
        await syncPublishedDirectory(join(root, ...path.split("/")));
      }
      await publishExclusive(
        stageRecording,
        join(root, ...recordingPathInProject.split("/")),
      );
      injectFault(faultAt, "after-recording-publication");
      await publishExclusive(
        stageMetadata,
        join(root, ...metadataPathInProject.split("/")),
      );
      injectFault(faultAt, "after-metadata-publication");
      const candidate = await verifyProjectCandidate(root, candidateBytes);
      assert.equal(
        candidate.identity,
        `sha256:${tx.after_project_sha256}`,
        "candidate project identity changed",
      );
      injectFault(faultAt, "after-candidate-verification");

      const manifestTemporary = join(backupsRoot, `.project-${id}.json`);
      await link(stageProject, manifestTemporary);
      await syncPublishedEntry(manifestTemporary);
      injectFault(faultAt, "after-manifest-sync");
      await rename(manifestTemporary, join(root, "project.json"));
      await syncPublishedEntry(join(root, "project.json"));
      injectFault(faultAt, "after-manifest-publication");
      const committed = await verifyProject(root);
      assert.equal(
        committed.identity,
        `sha256:${tx.after_project_sha256}`,
        "committed recording import identity changed",
      );
      await removeTransactionDirectory(transactionRoot);
      return {
        project_identity: committed.identity,
        project_revision: committed.project.revision,
        recording: recordingRecord,
        metadata: metadataDescriptor,
      };
    } catch (error) {
      if (
        transactionWritten &&
        error.code !== "HYPER_INJECTED_RECORDING_IMPORT_FAULT"
      ) {
        try {
          const tx = await readTransaction(transactionRoot, id);
          await rollbackTransaction(
            root,
            backupsRoot,
            transactionRoot,
            tx,
          );
        } catch (rollbackError) {
          throw new AggregateError(
            [error, rollbackError],
            "recording import and rollback both failed",
          );
        }
      } else if (!transactionWritten) {
        await rm(transactionRoot, { recursive: true, force: true });
      }
      throw error;
    }
  });
}

function defaultProcessIsAlive(pid) {
  try {
    process.kill(pid, 0);
    return true;
  } catch (error) {
    if (error.code === "ESRCH") return false;
    if (error.code === "EPERM") return true;
    throw error;
  }
}

export async function recoverRecordingImports(
  projectPath,
  { force = false, processIsAlive = defaultProcessIsAlive } = {},
) {
  assert.equal(typeof force, "boolean", "force must be Boolean");
  assert.equal(typeof processIsAlive, "function");
  const { root, backupsRoot } = await ensureBackupsRoot(projectPath);
  const lockPath = join(backupsRoot, "project-storage.lock");
  try {
    const status = await lstat(lockPath);
    if (!status.isFile() || status.isSymbolicLink()) {
      assert(force, "invalid project storage lock requires explicit force");
      await unlink(lockPath);
    } else {
      const bytes = await readFile(lockPath);
      let owner;
      try {
        owner = validateLockOwner(
          strictParse(new TextDecoder("utf-8", { fatal: true }).decode(bytes)),
        );
        assert(bytes.equals(Buffer.from(canonicalize(owner), "utf8")));
      } catch (error) {
        assert(force, "invalid project storage lock requires explicit force");
        owner = null;
      }
      if (owner) {
        assert.equal(
          await processIsAlive(owner.owner_pid),
          false,
          `project storage lock owner process ${owner.owner_pid} is still active`,
        );
      }
      await unlink(lockPath);
    }
  } catch (error) {
    if (error.code !== "ENOENT") throw error;
  }

  return withProjectLock(backupsRoot, async () => {
    const results = [];
    const entries = await readdir(backupsRoot, { withFileTypes: true });
    for (const entry of entries.sort((left, right) =>
      left.name.localeCompare(right.name),
    )) {
      const match = TRANSACTION_PATTERN.exec(entry.name);
      if (!match) continue;
      assert(
        entry.isDirectory() && !entry.isSymbolicLink(),
        `recording import residue is not a real directory: ${entry.name}`,
      );
      const transactionRoot = join(backupsRoot, entry.name);
      try {
        await lstat(join(transactionRoot, "transaction.json"));
      } catch (error) {
        if (error.code !== "ENOENT") throw error;
        await removeTransactionDirectory(transactionRoot);
        results.push({ id: match[1], outcome: "discarded-unpublished" });
        continue;
      }
      results.push({
        id: match[1],
        outcome: await recoverTransaction(
          root,
          backupsRoot,
          transactionRoot,
          match[1],
        ),
      });
    }
    return results;
  });
}
