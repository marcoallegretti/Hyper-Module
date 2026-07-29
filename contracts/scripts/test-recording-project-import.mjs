import assert from "node:assert/strict";
import { createHash, randomUUID } from "node:crypto";
import {
  mkdir,
  mkdtemp,
  readFile,
  readdir,
  rm,
  symlink,
  unlink,
  writeFile,
} from "node:fs/promises";
import { tmpdir } from "node:os";
import { join } from "node:path";

import canonicalize from "canonicalize";

import {
  importFinalizedRecording,
  recoverRecordingImports,
} from "./recording-project-import.mjs";
import { createMinimalProject } from "./test-project-fixture.mjs";
import { verifyProject } from "./verify-project.mjs";

const projectId = "018f47a2-6b2d-7c11-8a44-001122334455";
const trackId = "018f47a2-6b2d-7c11-8a44-001122334456";
const metadataId = "018f47a2-6b2d-7c11-8a44-001122334477";
const assetId = "018f47a2-6b2d-7c11-8a44-001122334478";

function sha256(bytes) {
  return createHash("sha256").update(bytes).digest("hex");
}

function stereoWave(frameCount = 2) {
  const channels = 2;
  const bytesPerSample = 2;
  const blockAlign = channels * bytesPerSample;
  const dataBytes = frameCount * blockAlign;
  const buffer = Buffer.alloc(44 + dataBytes);
  buffer.write("RIFF", 0, "ascii");
  buffer.writeUInt32LE(36 + dataBytes, 4);
  buffer.write("WAVE", 8, "ascii");
  buffer.write("fmt ", 12, "ascii");
  buffer.writeUInt32LE(16, 16);
  buffer.writeUInt16LE(1, 20);
  buffer.writeUInt16LE(channels, 22);
  buffer.writeUInt32LE(48000, 24);
  buffer.writeUInt32LE(48000 * blockAlign, 28);
  buffer.writeUInt16LE(blockAlign, 32);
  buffer.writeUInt16LE(16, 34);
  buffer.write("data", 36, "ascii");
  buffer.writeUInt32LE(dataBytes, 40);
  return buffer;
}

function metadataBytes() {
  return Buffer.from(
    canonicalize({
      format: "hyper-recording-metadata",
      format_version: 1,
      id: metadataId,
      project_id: projectId,
      asset_id: assetId,
      track_id: trackId,
      capture_kind: "new",
      captured_at_utc: "2026-07-29T12:00:00.000Z",
      timeline_start_tick: 7680,
      source_input_indices: [0, 1],
      sample_rate_hz: 48000,
      channels: 2,
      frame_count: 2,
      extensions: {},
    }),
    "utf8",
  );
}

async function createProject(parent, name) {
  const fixture = await createMinimalProject(parent, name);
  const track = {
    format: "hyper-track",
    format_version: 1,
    id: trackId,
    project_id: projectId,
    name: "Audio 1",
    kind: "audio",
    order: 0,
    active: true,
    clip_ids: [],
    extensions: {},
  };
  const bytes = Buffer.from(canonicalize(track), "utf8");
  const path = `tracks/${trackId}.json`;
  await mkdir(join(fixture.root, "tracks"));
  await writeFile(join(fixture.root, ...path.split("/")), bytes);
  fixture.project.resources.push({
    id: trackId,
    role: "track",
    path,
    media_type: "application/json",
    byte_length: bytes.length,
    sha256: sha256(bytes),
    extensions: {},
  });
  await writeFile(
    join(fixture.root, "project.json"),
    canonicalize(fixture.project),
  );
  await verifyProject(fixture.root);
  return fixture.root;
}

function importRequest(recordingPath, recordingBytes, faultAt = undefined) {
  return {
    recordingPath,
    recordingDescriptor: {
      byte_length: recordingBytes.length,
      sha256: sha256(recordingBytes),
    },
    metadataBytes: metadataBytes(),
    faultAt,
  };
}

async function transactionResidue(projectRoot) {
  const entries = await readdir(join(projectRoot, "backups"));
  return entries.filter((name) => name.startsWith(".recording-import-"));
}

const temporaryRoot = await mkdtemp(
  join(tmpdir(), `hyper-recording-import-${randomUUID()}-`),
);
try {
  const recordingBytes = stereoWave();
  const recordingPath = join(temporaryRoot, "published.wav");
  await writeFile(recordingPath, recordingBytes);

  const committedRoot = await createProject(temporaryRoot, "committed");
  const before = await verifyProject(committedRoot);
  const committed = await importFinalizedRecording(
    committedRoot,
    importRequest(recordingPath, recordingBytes),
  );
  assert.equal(committed.project_revision, before.project.revision + 1);
  assert.equal(committed.recording.sha256, sha256(recordingBytes));
  assert.equal(
    (await verifyProject(committedRoot)).identity,
    committed.project_identity,
  );
  assert.deepEqual(await transactionResidue(committedRoot), []);
  assert(
    (await readFile(recordingPath)).equals(recordingBytes),
    "recording import changed its source publication",
  );

  const invalidDescriptorRoot = await createProject(
    temporaryRoot,
    "invalid-descriptor",
  );
  const invalidBefore = await verifyProject(invalidDescriptorRoot);
  await assert.rejects(
    importFinalizedRecording(invalidDescriptorRoot, {
      ...importRequest(recordingPath, recordingBytes),
      recordingDescriptor: {
        byte_length: recordingBytes.length,
        sha256: "0".repeat(64),
      },
    }),
    /recording publication descriptor mismatch/,
  );
  assert.equal(
    (await verifyProject(invalidDescriptorRoot)).identity,
    invalidBefore.identity,
  );
  assert.deepEqual(await transactionResidue(invalidDescriptorRoot), []);

  const boundaries = [
    "after-transaction-sync",
    "after-recording-publication",
    "after-metadata-publication",
    "after-candidate-verification",
    "after-manifest-sync",
    "after-manifest-publication",
  ];
  for (const boundary of boundaries) {
    const root = await createProject(temporaryRoot, boundary);
    const original = await verifyProject(root);
    await assert.rejects(
      importFinalizedRecording(
        root,
        importRequest(recordingPath, recordingBytes, boundary),
      ),
      (error) =>
        error.code === "HYPER_INJECTED_RECORDING_IMPORT_FAULT" &&
        error.boundary === boundary,
    );
    assert.equal((await transactionResidue(root)).length, 1);
    const recovered = await recoverRecordingImports(root);
    assert.equal(recovered.length, 1);
    if (boundary === "after-manifest-publication") {
      assert.equal(recovered[0].outcome, "committed");
      assert.notEqual((await verifyProject(root)).identity, original.identity);
    } else {
      assert.equal(recovered[0].outcome, "rolled-back");
      assert.equal((await verifyProject(root)).identity, original.identity);
      assert.equal(
        await readFile(
          join(root, "project.json"),
          "utf8",
        ).then((text) => JSON.parse(text).revision),
        original.project.revision,
      );
    }
    assert.deepEqual(await transactionResidue(root), []);
  }

  const corruptRoot = await createProject(temporaryRoot, "corrupt-residue");
  await assert.rejects(
    importFinalizedRecording(
      corruptRoot,
      importRequest(
        recordingPath,
        recordingBytes,
        "after-recording-publication",
      ),
    ),
    /injected recording import fault/,
  );
  await writeFile(
    join(corruptRoot, "assets", "recordings", `${assetId}.wav`),
    Buffer.alloc(recordingBytes.length, 0xff),
  );
  await assert.rejects(
    recoverRecordingImports(corruptRoot),
    /recovery refuses changed content/,
  );
  assert.equal((await transactionResidue(corruptRoot)).length, 1);

  const missingManifestRoot = await createProject(
    temporaryRoot,
    "missing-manifest",
  );
  await assert.rejects(
    importFinalizedRecording(
      missingManifestRoot,
      importRequest(
        recordingPath,
        recordingBytes,
        "after-transaction-sync",
      ),
    ),
    /injected recording import fault/,
  );
  await unlink(join(missingManifestRoot, "project.json"));
  assert.equal(
    (await recoverRecordingImports(missingManifestRoot))[0].outcome,
    "rolled-back",
  );
  await verifyProject(missingManifestRoot);

  const partialRoot = await createProject(temporaryRoot, "partial-journal");
  const partialId = randomUUID();
  const partialTransaction = join(
    partialRoot,
    "backups",
    `.recording-import-${partialId}`,
  );
  await mkdir(partialTransaction, { recursive: true });
  await writeFile(join(partialTransaction, "recording.wav"), recordingBytes);
  assert.deepEqual(await recoverRecordingImports(partialRoot), [
    { id: partialId, outcome: "discarded-unpublished" },
  ]);

  const hostileRoot = await createProject(temporaryRoot, "hostile-journal");
  await assert.rejects(
    importFinalizedRecording(
      hostileRoot,
      importRequest(
        recordingPath,
        recordingBytes,
        "after-transaction-sync",
      ),
    ),
    /injected recording import fault/,
  );
  const [hostileDirectory] = await transactionResidue(hostileRoot);
  const hostileTransactionPath = join(
    hostileRoot,
    "backups",
    hostileDirectory,
    "transaction.json",
  );
  const hostileTransaction = JSON.parse(
    await readFile(hostileTransactionPath, "utf8"),
  );
  hostileTransaction.recording.path = "../../outside.wav";
  await writeFile(hostileTransactionPath, canonicalize(hostileTransaction));
  await assert.rejects(
    recoverRecordingImports(hostileRoot),
    /Expected values to be strictly equal/,
  );
  assert.equal((await transactionResidue(hostileRoot)).length, 1);

  if (process.platform !== "win32") {
    const linkedRoot = await createProject(temporaryRoot, "linked-source");
    const linkedSource = join(temporaryRoot, "linked-source.wav");
    await symlink(recordingPath, linkedSource);
    await assert.rejects(
      importFinalizedRecording(
        linkedRoot,
        importRequest(linkedSource, recordingBytes),
      ),
      /recording source must be a real regular file/,
    );
  }

  const lockedRoot = await createProject(temporaryRoot, "locked");
  await mkdir(join(lockedRoot, "backups"), { recursive: true });
  const lockPath = join(lockedRoot, "backups", "project-storage.lock");
  await writeFile(
    lockPath,
    canonicalize({
      format: "hyper-project-storage-lock",
      format_version: 1,
      owner_pid: process.pid,
      owner_token: randomUUID(),
      extensions: {},
    }),
  );
  await assert.rejects(
    recoverRecordingImports(lockedRoot),
    /owner process .* is still active/,
  );
  await recoverRecordingImports(lockedRoot, {
    processIsAlive: () => false,
  });

  await writeFile(lockPath, "invalid lock");
  await assert.rejects(
    recoverRecordingImports(lockedRoot),
    /requires explicit force/,
  );
  assert.deepEqual(
    await recoverRecordingImports(lockedRoot, { force: true }),
    [],
  );

  await assert.rejects(
    importFinalizedRecording(
      lockedRoot,
      importRequest(recordingPath, recordingBytes, "unknown"),
    ),
    /supported recording import boundary/,
  );

  console.log(
    "Recording project import passed: 23 commit, recovery, and refusal cases",
  );
} finally {
  await rm(temporaryRoot, { recursive: true, force: true });
}
