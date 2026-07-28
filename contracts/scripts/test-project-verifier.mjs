import assert from "node:assert/strict";
import { createHash, randomUUID } from "node:crypto";
import {
  cp,
  mkdir,
  mkdtemp,
  readFile,
  realpath,
  rm,
  symlink,
  unlink,
  writeFile,
} from "node:fs/promises";
import { join } from "node:path";
import { tmpdir } from "node:os";

import canonicalize from "canonicalize";

import { verifyDescribedFile } from "./verify-bundle.mjs";
import { verifyProject } from "./verify-project.mjs";

// MIT-licensed reference fixture from music-metadata 11.14.0:
// test/samples/flac/libFLAC.flac at commit 959f0f3d464578ddd3dfc2a5cbcd0068799dd394.
const flacFixtureBase64 =
  "ZkxhQwAAACIQABAAAAALAAANCsRA8AADXVSKK2c+p/lnjCv6lOIyKK0KhAAAKCAAAAByZWZlcmVuY2UgbGliRkxBQyAxLjQuMiAyMDIyMTAyMgAAAAD/+MkIAJUAAAAhvf/4yQgBkgAAAE3F//jJCAKbAAAA+U3/+MkIA5wAAACVNf/4yQgEiQAAABBY//jJCAWOAAAAfCD/+MkIBocAAADIqP/4yQgHgAAAAKTQ//jJCAitAAAAQnf/+MkICaoAAAAuD//4yQgKowAAAJqH//jJCAukAAAA9v//+MkIDLEAAABzkv/4yQgNtgAAAB/q//jJCA6/AAAAq2L/+MkID7gAAADHGv/4yQgQ5QAAAOYp//jJCBHiAAAAilH/+MkIEusAAAA+2f/4yQgT7AAAAFKh//jJCBT5AAAA18z/+MkIFf4AAAC7tP/4yQgW9wAAAA88//jJCBfwAAAAY0T/+MkIGN0AAACF4//4yQgZ2gAAAOmb//jJCBrTAAAAXRP/+MkIG9QAAAAxa//4yQgcwQAAALQG//jJCB3GAAAA2H7/+MkIHs8AAABs9v/4yQgfyAAAAACO//jJCCB1AAAALpD/+MkIIXIAAABC6P/4yQgiewAAAPZg//jJCCN8AAAAmhj/+MkIJGkAAAAfdf/4yQglbgAAAHMN//jJCCZnAAAAx4X/+MkIJ2AAAACr/f/4yQgoTQAAAE1a//jJCClKAAAAISL/+MkIKkMAAACVqv/4yQgrRAAAAPnS//jJCCxRAAAAfL//+MkILVYAAAAQx//4yQguXwAAAKRP//jJCC9YAAAAyDf/+MkIMAUAAADpBP/4yQgxAgAAAIV8//jJCDILAAAAMfT/+MkIMwwAAABdjP/4yQg0GQAAANjh//h5CDUNUxcAAAAcSg==";

function record(id, role, path, mediaType, bytes) {
  return {
    id,
    role,
    path,
    media_type: mediaType,
    byte_length: bytes.length,
    sha256: createHash("sha256").update(bytes).digest("hex"),
    extensions: {},
  };
}

function pcm16Wave(frameCount, channels = 1, sampleRate = 48000) {
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
  buffer.writeUInt32LE(sampleRate, 24);
  buffer.writeUInt32LE(sampleRate * blockAlign, 28);
  buffer.writeUInt16LE(blockAlign, 32);
  buffer.writeUInt16LE(bytesPerSample * 8, 34);
  buffer.write("data", 36, "ascii");
  buffer.writeUInt32LE(dataBytes, 40);
  return buffer;
}

async function createValidProject(parent, name, audioFixture) {
  const fixture = audioFixture ?? {
    bytes: pcm16Wave(128),
    frameCount: 128,
    mediaType: "audio/wav",
    path: "assets/samples/kick.wav",
  };
  const root = join(parent, name);
  await mkdir(join(root, "arrangement"), { recursive: true });
  await mkdir(join(root, "clips"), { recursive: true });
  await mkdir(join(root, "mixer"), { recursive: true });
  await mkdir(join(root, "mappings"), { recursive: true });
  await mkdir(join(root, "scenes"), { recursive: true });
  await mkdir(join(root, "tracks"), { recursive: true });
  await mkdir(join(root, "timeline"), { recursive: true });
  await mkdir(join(root, "assets", "samples"), { recursive: true });
  const track = {
    format: "hyper-track",
    format_version: 1,
    id: "018f47a2-6b2d-7c11-8a44-001122334456",
    project_id: "018f47a2-6b2d-7c11-8a44-001122334455",
    name: "Audio 1",
    kind: "audio",
    order: 0,
    active: true,
    clip_ids: ["018f47a2-6b2d-7c11-8a44-001122334460"],
    extensions: {},
  };
  const trackBytes = Buffer.from(canonicalize(track), "utf8");
  const timeline = {
    format: "hyper-timeline",
    format_version: 1,
    id: "018f47a2-6b2d-7c11-8a44-001122334459",
    project_id: "018f47a2-6b2d-7c11-8a44-001122334455",
    ticks_per_quarter: 1920,
    tempo_events: [{ tick: 0, microseconds_per_quarter: 500000 }],
    meter_events: [{ tick: 0, numerator: 4, denominator: 4 }],
    extensions: {},
  };
  const timelineBytes = Buffer.from(canonicalize(timeline), "utf8");
  const clip = {
    format: "hyper-audio-clip",
    format_version: 1,
    id: "018f47a2-6b2d-7c11-8a44-001122334460",
    project_id: "018f47a2-6b2d-7c11-8a44-001122334455",
    track_id: track.id,
    name: "Kick Loop",
    enabled: true,
    duration_ticks: 7680,
    source: {
      asset_id: "018f47a2-6b2d-7c11-8a44-001122334457",
      start_frame: 0,
      frame_count: fixture.frameCount,
    },
    loop: { enabled: true, start_tick: 0, end_tick: 7680 },
    playback: {
      gain_decibels: 0,
      reverse: false,
      transpose_cents: 0,
    },
    fades: { curve: "linear-amplitude", in_ticks: 0, out_ticks: 0 },
    warp_markers: [
      { tick: 0, source_frame_offset: 0 },
      { tick: 7680, source_frame_offset: fixture.frameCount },
    ],
    extensions: {},
  };
  const clipBytes = Buffer.from(canonicalize(clip), "utf8");
  const scene = {
    format: "hyper-scene",
    format_version: 1,
    id: "018f47a2-6b2d-7c11-8a44-001122334461",
    project_id: "018f47a2-6b2d-7c11-8a44-001122334455",
    name: "Verse",
    order: 0,
    enabled: true,
    launch_quantization: { mode: "grid", quantum_ticks: 7680 },
    slots: [{ track_id: track.id, action: "play", clip_id: clip.id }],
    extensions: {},
  };
  const sceneBytes = Buffer.from(canonicalize(scene), "utf8");
  const arrangement = {
    format: "hyper-arrangement",
    format_version: 1,
    id: "018f47a2-6b2d-7c11-8a44-001122334463",
    project_id: "018f47a2-6b2d-7c11-8a44-001122334455",
    name: "Main",
    enabled: true,
    placements: [
      {
        id: "018f47a2-6b2d-7c11-8a44-001122334464",
        track_id: track.id,
        clip_id: clip.id,
        start_tick: 0,
        clip_start_tick: 0,
        duration_ticks: 7680,
        enabled: true,
      },
    ],
    scene_launches: [],
    extensions: {},
  };
  const arrangementBytes = Buffer.from(canonicalize(arrangement), "utf8");
  const mixer = {
    format: "hyper-mixer",
    format_version: 1,
    id: "018f47a2-6b2d-7c11-8a44-001122334468",
    project_id: "018f47a2-6b2d-7c11-8a44-001122334455",
    channels: [
      {
        track_id: track.id,
        gain_millionths: 1_000_000,
        pan_millionths: 0,
        mute: false,
        solo: false,
        destination: { kind: "main" },
        sends: [],
      },
    ],
    return_buses: [],
    main: { gain_millionths: 1_000_000, mute: false },
    cue: {
      source: "solo",
      gain_millionths: 1_000_000,
      mute: false,
    },
    extensions: {},
  };
  const mixerBytes = Buffer.from(canonicalize(mixer), "utf8");
  const mappings = {
    format: "hyper-control-mappings",
    format_version: 1,
    id: "018f47a2-6b2d-7c11-8a44-001122334474",
    project_id: "018f47a2-6b2d-7c11-8a44-001122334455",
    bindings: [],
    extensions: {},
  };
  const mappingBytes = Buffer.from(canonicalize(mappings), "utf8");
  const sampleBytes = fixture.bytes;
  const trackPath = "tracks/018f47a2-6b2d-7c11-8a44-001122334456.json";
  const timelinePath = "timeline/timeline.json";
  const clipPath = `clips/${clip.id}.json`;
  const scenePath = `scenes/${scene.id}.json`;
  const arrangementPath = "arrangement/main.json";
  const mixerPath = "mixer/mixer.json";
  const mappingPath = "mappings/controls.json";
  await writeFile(join(root, ...arrangementPath.split("/")), arrangementBytes);
  await writeFile(join(root, ...mixerPath.split("/")), mixerBytes);
  await writeFile(join(root, ...mappingPath.split("/")), mappingBytes);
  await writeFile(join(root, ...clipPath.split("/")), clipBytes);
  await writeFile(join(root, ...scenePath.split("/")), sceneBytes);
  await writeFile(join(root, ...trackPath.split("/")), trackBytes);
  await writeFile(join(root, ...timelinePath.split("/")), timelineBytes);
  await writeFile(join(root, ...fixture.path.split("/")), sampleBytes);

  const project = {
    format: "hyper-project",
    format_version: 1,
    project_id: "018f47a2-6b2d-7c11-8a44-001122334455",
    revision: 7,
    name: "Project Verifier Fixture",
    sample_rate_hz: 48000,
    required_capabilities: {
      format: "hyper-hardware-capabilities",
      format_version: 1,
      audio_inputs: 2,
      audio_outputs: 2,
      cv_inputs: 0,
      cv_outputs: 0,
      gate_outputs: 0,
      has_clock_input: false,
      has_reset_input: false,
      has_clock_output: false,
      usb_mode: "none",
      has_ethernet: true,
      extensions: {},
    },
    resources: [
      record(
        "018f47a2-6b2d-7c11-8a44-001122334456",
        "track",
        trackPath,
        "application/json",
        trackBytes,
      ),
      record(
        timeline.id,
        "timeline",
        timelinePath,
        "application/json",
        timelineBytes,
      ),
      record(clip.id, "clip", clipPath, "application/json", clipBytes),
      record(scene.id, "scene", scenePath, "application/json", sceneBytes),
      record(
        arrangement.id,
        "arrangement",
        arrangementPath,
        "application/json",
        arrangementBytes,
      ),
      record(mixer.id, "mixer", mixerPath, "application/json", mixerBytes),
      record(
        mappings.id,
        "mapping",
        mappingPath,
        "application/json",
        mappingBytes,
      ),
    ],
    assets: [
      record(
        "018f47a2-6b2d-7c11-8a44-001122334457",
        "sample",
        fixture.path,
        fixture.mediaType,
        sampleBytes,
      ),
    ],
    extensions: {},
  };
  await writeFile(join(root, "project.json"), canonicalize(project));
  return root;
}

async function mutateFirstTrack(root, mutate) {
  const projectPath = join(root, "project.json");
  const project = JSON.parse(await readFile(projectPath, "utf8"));
  const trackRecord = project.resources.find(
    (record) => record.role === "track",
  );
  const trackPath = join(root, ...trackRecord.path.split("/"));
  const track = JSON.parse(await readFile(trackPath, "utf8"));
  mutate(track, project, trackRecord);
  const bytes = Buffer.from(canonicalize(track), "utf8");
  await writeFile(trackPath, bytes);
  trackRecord.byte_length = bytes.length;
  trackRecord.sha256 = createHash("sha256").update(bytes).digest("hex");
  await writeFile(projectPath, canonicalize(project));
}

async function mutateTimeline(root, mutate) {
  const projectPath = join(root, "project.json");
  const project = JSON.parse(await readFile(projectPath, "utf8"));
  const timelineRecord = project.resources.find(
    (record) => record.role === "timeline",
  );
  const timelinePath = join(root, ...timelineRecord.path.split("/"));
  const timeline = JSON.parse(await readFile(timelinePath, "utf8"));
  mutate(timeline, project, timelineRecord);
  const bytes = Buffer.from(canonicalize(timeline), "utf8");
  await writeFile(timelinePath, bytes);
  timelineRecord.byte_length = bytes.length;
  timelineRecord.sha256 = createHash("sha256").update(bytes).digest("hex");
  await writeFile(projectPath, canonicalize(project));
}

async function mutateFirstClip(root, mutate) {
  const projectPath = join(root, "project.json");
  const project = JSON.parse(await readFile(projectPath, "utf8"));
  const clipRecord = project.resources.find((record) => record.role === "clip");
  const clipPath = join(root, ...clipRecord.path.split("/"));
  const clip = JSON.parse(await readFile(clipPath, "utf8"));
  mutate(clip, project, clipRecord);
  const bytes = Buffer.from(canonicalize(clip), "utf8");
  await writeFile(clipPath, bytes);
  clipRecord.byte_length = bytes.length;
  clipRecord.sha256 = createHash("sha256").update(bytes).digest("hex");
  await writeFile(projectPath, canonicalize(project));
}

async function mutateFirstScene(root, mutate) {
  const projectPath = join(root, "project.json");
  const project = JSON.parse(await readFile(projectPath, "utf8"));
  const sceneRecord = project.resources.find(
    (record) => record.role === "scene",
  );
  const scenePath = join(root, ...sceneRecord.path.split("/"));
  const scene = JSON.parse(await readFile(scenePath, "utf8"));
  mutate(scene, project, sceneRecord);
  const bytes = Buffer.from(canonicalize(scene), "utf8");
  await writeFile(scenePath, bytes);
  sceneRecord.byte_length = bytes.length;
  sceneRecord.sha256 = createHash("sha256").update(bytes).digest("hex");
  await writeFile(projectPath, canonicalize(project));
}

async function mutateArrangement(root, mutate) {
  const projectPath = join(root, "project.json");
  const project = JSON.parse(await readFile(projectPath, "utf8"));
  const arrangementRecord = project.resources.find(
    (record) => record.role === "arrangement",
  );
  const arrangementPath = join(root, ...arrangementRecord.path.split("/"));
  const arrangement = JSON.parse(await readFile(arrangementPath, "utf8"));
  mutate(arrangement, project, arrangementRecord);
  const bytes = Buffer.from(canonicalize(arrangement), "utf8");
  await writeFile(arrangementPath, bytes);
  arrangementRecord.byte_length = bytes.length;
  arrangementRecord.sha256 = createHash("sha256").update(bytes).digest("hex");
  await writeFile(projectPath, canonicalize(project));
}

async function addSecondAudioTrack(root) {
  const projectPath = join(root, "project.json");
  const project = JSON.parse(await readFile(projectPath, "utf8"));
  const firstTrackRecord = project.resources.find(
    (record) => record.role === "track",
  );
  const firstTrack = JSON.parse(
    await readFile(join(root, ...firstTrackRecord.path.split("/")), "utf8"),
  );
  const secondTrack = {
    ...firstTrack,
    id: "018f47a2-6b2d-7c11-8a44-001122334458",
    name: "Audio 2",
    order: 1,
    clip_ids: [],
  };
  const bytes = Buffer.from(canonicalize(secondTrack), "utf8");
  const path = `tracks/${secondTrack.id}.json`;
  await writeFile(join(root, ...path.split("/")), bytes);
  project.resources.push(
    record(secondTrack.id, "track", path, "application/json", bytes),
  );
  await writeFile(projectPath, canonicalize(project));
  return secondTrack.id;
}

async function mutateMixer(root, mutate) {
  const projectPath = join(root, "project.json");
  const project = JSON.parse(await readFile(projectPath, "utf8"));
  const mixerRecord = project.resources.find(
    (record) => record.role === "mixer",
  );
  const mixerPath = join(root, ...mixerRecord.path.split("/"));
  const mixer = JSON.parse(await readFile(mixerPath, "utf8"));
  mutate(mixer, project, mixerRecord);
  const bytes = Buffer.from(canonicalize(mixer), "utf8");
  await writeFile(mixerPath, bytes);
  mixerRecord.byte_length = bytes.length;
  mixerRecord.sha256 = createHash("sha256").update(bytes).digest("hex");
  await writeFile(projectPath, canonicalize(project));
}

async function addReturnTrack(root) {
  const projectPath = join(root, "project.json");
  const project = JSON.parse(await readFile(projectPath, "utf8"));
  const firstTrackRecord = project.resources.find(
    (record) => record.role === "track",
  );
  const firstTrack = JSON.parse(
    await readFile(join(root, ...firstTrackRecord.path.split("/")), "utf8"),
  );
  const returnTrack = {
    ...firstTrack,
    id: "018f47a2-6b2d-7c11-8a44-001122334469",
    name: "Return A",
    kind: "return",
    order: 1,
    clip_ids: [],
  };
  const bytes = Buffer.from(canonicalize(returnTrack), "utf8");
  const path = `tracks/${returnTrack.id}.json`;
  await writeFile(join(root, ...path.split("/")), bytes);
  project.resources.push(
    record(returnTrack.id, "track", path, "application/json", bytes),
  );
  await writeFile(projectPath, canonicalize(project));
  return returnTrack;
}

async function mutateMappings(root, mutate) {
  const projectPath = join(root, "project.json");
  const project = JSON.parse(await readFile(projectPath, "utf8"));
  const mappingRecord = project.resources.find(
    (record) => record.role === "mapping",
  );
  const mappingPath = join(root, ...mappingRecord.path.split("/"));
  const mappings = JSON.parse(await readFile(mappingPath, "utf8"));
  mutate(mappings, project, mappingRecord);
  const bytes = Buffer.from(canonicalize(mappings), "utf8");
  await writeFile(mappingPath, bytes);
  mappingRecord.byte_length = bytes.length;
  mappingRecord.sha256 = createHash("sha256").update(bytes).digest("hex");
  await writeFile(projectPath, canonicalize(project));
}

async function addFinalizedRecording(root) {
  const projectPath = join(root, "project.json");
  const project = JSON.parse(await readFile(projectPath, "utf8"));
  const trackRecord = project.resources.find(
    (record) => record.role === "track",
  );
  const recordingAssetId = "018f47a2-6b2d-7c11-8a44-001122334478";
  const metadataId = "018f47a2-6b2d-7c11-8a44-001122334477";
  const recordingPath = `assets/recordings/${recordingAssetId}.wav`;
  const metadataPath = `assets/recordings/${metadataId}.json`;
  const recordingBytes = pcm16Wave(256, 2);
  const metadata = {
    format: "hyper-recording-metadata",
    format_version: 1,
    id: metadataId,
    project_id: project.project_id,
    asset_id: recordingAssetId,
    track_id: trackRecord.id,
    capture_kind: "new",
    captured_at_utc: "2026-07-27T12:00:00.000Z",
    timeline_start_tick: 7680,
    source_input_indices: [0, 1],
    sample_rate_hz: 48000,
    channels: 2,
    frame_count: 256,
    extensions: {},
  };
  const metadataBytes = Buffer.from(canonicalize(metadata), "utf8");
  await mkdir(join(root, "assets", "recordings"), { recursive: true });
  await writeFile(join(root, ...recordingPath.split("/")), recordingBytes);
  await writeFile(join(root, ...metadataPath.split("/")), metadataBytes);
  project.resources.push(
    record(
      metadataId,
      "recording-metadata",
      metadataPath,
      "application/json",
      metadataBytes,
    ),
  );
  project.assets.push(
    record(
      recordingAssetId,
      "recording",
      recordingPath,
      "audio/wav",
      recordingBytes,
    ),
  );
  await writeFile(projectPath, canonicalize(project));
  return { metadata, metadataPath, recordingPath };
}

async function mutateRecordingMetadata(root, mutate) {
  const projectPath = join(root, "project.json");
  const project = JSON.parse(await readFile(projectPath, "utf8"));
  const metadataRecord = project.resources.find(
    (record) => record.role === "recording-metadata",
  );
  const metadataPath = join(root, ...metadataRecord.path.split("/"));
  const metadata = JSON.parse(await readFile(metadataPath, "utf8"));
  mutate(metadata, project, metadataRecord);
  const bytes = Buffer.from(canonicalize(metadata), "utf8");
  await writeFile(metadataPath, bytes);
  metadataRecord.byte_length = bytes.length;
  metadataRecord.sha256 = createHash("sha256").update(bytes).digest("hex");
  await writeFile(projectPath, canonicalize(project));
}

async function addAutomationTake(
  root,
  {
    id = "018f47a2-6b2d-7c11-8a44-001122334479",
    laneId = "018f47a2-6b2d-7c11-8a44-001122334480",
    startTick = 0,
    endTick = 7680,
    enabled = true,
  } = {},
) {
  const projectPath = join(root, "project.json");
  const project = JSON.parse(await readFile(projectPath, "utf8"));
  const trackRecord = project.resources.find(
    (record) => record.role === "track",
  );
  const take = {
    format: "hyper-automation-take",
    format_version: 1,
    id,
    project_id: project.project_id,
    name: `Automation ${startTick}`,
    enabled,
    captured_at_utc: "2026-07-27T12:05:00.000Z",
    start_tick: startTick,
    end_tick: endTick,
    lanes: [
      {
        id: laneId,
        target: {
          kind: "mixer-channel",
          track_id: trackRecord.id,
          parameter: "gain",
        },
        interpolation: "linear",
        points: [
          { tick: startTick, value: 500_000 },
          { tick: endTick - 1, value: 1_500_000 },
        ],
      },
    ],
    extensions: {},
  };
  const path = `takes/${id}.json`;
  const bytes = Buffer.from(canonicalize(take), "utf8");
  await mkdir(join(root, "takes"), { recursive: true });
  await writeFile(join(root, ...path.split("/")), bytes);
  project.resources.push(
    record(id, "automation-take", path, "application/json", bytes),
  );
  await writeFile(projectPath, canonicalize(project));
  return take;
}

async function mutateAutomationTake(root, mutate) {
  const projectPath = join(root, "project.json");
  const project = JSON.parse(await readFile(projectPath, "utf8"));
  const takeRecord = project.resources.find(
    (record) => record.role === "automation-take",
  );
  const takePath = join(root, ...takeRecord.path.split("/"));
  const take = JSON.parse(await readFile(takePath, "utf8"));
  mutate(take, project, takeRecord);
  const bytes = Buffer.from(canonicalize(take), "utf8");
  await writeFile(takePath, bytes);
  takeRecord.byte_length = bytes.length;
  takeRecord.sha256 = createHash("sha256").update(bytes).digest("hex");
  await writeFile(projectPath, canonicalize(project));
}

function cvGainBinding() {
  return {
    id: "018f47a2-6b2d-7c11-8a44-001122334475",
    enabled: true,
    source: { kind: "cv-input", input_index: 0 },
    scope: { kind: "global" },
    transform: {
      kind: "linear",
      input_min: -1_000_000,
      input_max: 1_000_000,
      output_min: 0,
      output_max: 1_000_000,
    },
    target: {
      kind: "mixer-channel",
      track_id: "018f47a2-6b2d-7c11-8a44-001122334456",
      parameter: "gain",
    },
  };
}

const temporaryRoot = await mkdtemp(
  join(tmpdir(), `hyper-project-verifier-${randomUUID()}-`),
);
try {
  const valid = await createValidProject(temporaryRoot, "valid");
  const first = await verifyProject(valid);
  const second = await verifyProject(valid);
  assert.equal(first.identity, second.identity);

  const validFlac = await createValidProject(temporaryRoot, "valid-flac", {
    bytes: Buffer.from(flacFixtureBase64, "base64"),
    frameCount: 220500,
    mediaType: "audio/flac",
    path: "assets/samples/reference.flac",
  });
  await verifyProject(validFlac);

  const validRecording = join(temporaryRoot, "valid-recording");
  await cp(valid, validRecording, { recursive: true });
  await addFinalizedRecording(validRecording);
  await verifyProject(validRecording);

  const recordingFrameMismatch = join(
    temporaryRoot,
    "recording-frame-mismatch",
  );
  await cp(validRecording, recordingFrameMismatch, { recursive: true });
  await mutateRecordingMetadata(recordingFrameMismatch, (metadata) => {
    metadata.frame_count += 1;
  });
  await assert.rejects(
    verifyProject(recordingFrameMismatch),
    /recording media frame count does not match its metadata/,
  );

  const recordingUnavailableInput = join(
    temporaryRoot,
    "recording-unavailable-input",
  );
  await cp(validRecording, recordingUnavailableInput, { recursive: true });
  await mutateRecordingMetadata(
    recordingUnavailableInput,
    (metadata, project) => {
      project.required_capabilities.audio_inputs = 1;
    },
  );
  await assert.rejects(
    verifyProject(recordingUnavailableInput),
    /recording metadata requires an unavailable audio input/,
  );

  const recordingWrongTrack = join(temporaryRoot, "recording-wrong-track");
  await cp(validRecording, recordingWrongTrack, { recursive: true });
  await mutateRecordingMetadata(recordingWrongTrack, (metadata) => {
    metadata.track_id = "018f47a2-6b2d-7c11-8a44-001122334499";
  });
  await assert.rejects(
    verifyProject(recordingWrongTrack),
    /recording metadata must reference an audio track/,
  );

  const recordingWithoutMetadata = join(
    temporaryRoot,
    "recording-without-metadata",
  );
  await cp(validRecording, recordingWithoutMetadata, { recursive: true });
  const recordingWithoutMetadataProjectPath = join(
    recordingWithoutMetadata,
    "project.json",
  );
  const recordingWithoutMetadataProject = JSON.parse(
    await readFile(recordingWithoutMetadataProjectPath, "utf8"),
  );
  const metadataRecord = recordingWithoutMetadataProject.resources.find(
    (record) => record.role === "recording-metadata",
  );
  recordingWithoutMetadataProject.resources =
    recordingWithoutMetadataProject.resources.filter(
      (record) => record !== metadataRecord,
    );
  await unlink(
    join(recordingWithoutMetadata, ...metadataRecord.path.split("/")),
  );
  await writeFile(
    recordingWithoutMetadataProjectPath,
    canonicalize(recordingWithoutMetadataProject),
  );
  await assert.rejects(
    verifyProject(recordingWithoutMetadata),
    /every recording asset must have exactly one metadata resource/,
  );

  const validAutomation = join(temporaryRoot, "valid-automation");
  await cp(valid, validAutomation, { recursive: true });
  await addAutomationTake(validAutomation);
  await verifyProject(validAutomation);

  const missingAutomationTarget = join(
    temporaryRoot,
    "missing-automation-target",
  );
  await cp(validAutomation, missingAutomationTarget, { recursive: true });
  await mutateAutomationTake(missingAutomationTarget, (take) => {
    take.lanes[0].target.track_id = "018f47a2-6b2d-7c11-8a44-001122334499";
  });
  await assert.rejects(
    verifyProject(missingAutomationTarget),
    /automation target mixer channel does not exist/,
  );

  const overlappingAutomation = join(temporaryRoot, "overlapping-automation");
  await cp(validAutomation, overlappingAutomation, { recursive: true });
  await addAutomationTake(overlappingAutomation, {
    id: "018f47a2-6b2d-7c11-8a44-001122334482",
    laneId: "018f47a2-6b2d-7c11-8a44-001122334483",
    startTick: 3840,
    endTick: 11520,
  });
  await assert.rejects(
    verifyProject(overlappingAutomation),
    /enabled automation takes must not overlap on the same target/,
  );

  const adjacentAutomation = join(temporaryRoot, "adjacent-automation");
  await cp(validAutomation, adjacentAutomation, { recursive: true });
  await addAutomationTake(adjacentAutomation, {
    id: "018f47a2-6b2d-7c11-8a44-001122334482",
    laneId: "018f47a2-6b2d-7c11-8a44-001122334483",
    startTick: 7680,
    endTick: 15360,
  });
  await verifyProject(adjacentAutomation);

  const collidingAutomationLane = join(
    temporaryRoot,
    "colliding-automation-lane",
  );
  await cp(validAutomation, collidingAutomationLane, { recursive: true });
  await mutateAutomationTake(collidingAutomationLane, (take) => {
    take.lanes[0].id = "018f47a2-6b2d-7c11-8a44-001122334456";
  });
  await assert.rejects(
    verifyProject(collidingAutomationLane),
    /automation lane identifier collides with another project entity/,
  );

  const invalidAutomationSolo = join(temporaryRoot, "invalid-automation-solo");
  await cp(valid, invalidAutomationSolo, { recursive: true });
  const automationReturnTrack = await addReturnTrack(invalidAutomationSolo);
  await mutateMixer(invalidAutomationSolo, (mixer) => {
    mixer.channels.push({
      track_id: automationReturnTrack.id,
      gain_millionths: 1_000_000,
      pan_millionths: 0,
      mute: false,
      solo: false,
      destination: { kind: "main" },
      sends: [],
    });
  });
  await addAutomationTake(invalidAutomationSolo);
  await mutateAutomationTake(invalidAutomationSolo, (take) => {
    take.lanes[0].target.track_id = automationReturnTrack.id;
    take.lanes[0].target.parameter = "solo";
    take.lanes[0].interpolation = "hold";
    take.lanes[0].points = [{ tick: 0, value: false }];
  });
  await assert.rejects(
    verifyProject(invalidAutomationSolo),
    /automation solo target must be a source track/,
  );

  const localOnly = join(temporaryRoot, "local-only");
  await cp(valid, localOnly, { recursive: true });
  await mkdir(join(localOnly, "cache"), { recursive: true });
  await mkdir(join(localOnly, "backups"), { recursive: true });
  await writeFile(join(localOnly, "cache", "waveform.bin"), "regenerable");
  await writeFile(join(localOnly, "backups", "project.previous"), "backup");
  assert.equal((await verifyProject(localOnly)).identity, first.identity);

  const modified = join(temporaryRoot, "modified");
  await cp(valid, modified, { recursive: true });
  const samplePath = join(modified, "assets", "samples", "kick.wav");
  const sample = await readFile(samplePath);
  await writeFile(samplePath, Buffer.alloc(sample.length, 0x78));
  await assert.rejects(verifyProject(modified), /SHA-256 mismatch/);

  const capturedBytesRoot = join(temporaryRoot, "captured-resource-bytes");
  await mkdir(capturedBytesRoot);
  const originalBytes = Buffer.from('{"value":"original"}');
  const replacementBytes = Buffer.from('{"value":"replaced"}');
  const capturedPath = join(capturedBytesRoot, "resource.json");
  await writeFile(capturedPath, originalBytes);
  const captured = await verifyDescribedFile(
    capturedBytesRoot,
    await realpath(capturedBytesRoot),
    {
      path: "resource.json",
      byte_length: originalBytes.length,
      sha256: createHash("sha256").update(originalBytes).digest("hex"),
    },
    { captureBytes: true },
  );
  await writeFile(capturedPath, replacementBytes);
  assert.deepEqual(captured, originalBytes);

  const undeclared = join(temporaryRoot, "undeclared");
  await cp(valid, undeclared, { recursive: true });
  await writeFile(join(undeclared, "tracks", "undeclared.json"), "{}");
  await assert.rejects(
    verifyProject(undeclared),
    /undeclared file is forbidden/,
  );

  const missing = join(temporaryRoot, "missing");
  await cp(valid, missing, { recursive: true });
  await unlink(join(missing, "assets", "samples", "kick.wav"));
  await assert.rejects(
    verifyProject(missing),
    /do not exactly match project.json/,
  );

  const noncanonical = join(temporaryRoot, "noncanonical");
  await cp(valid, noncanonical, { recursive: true });
  const project = JSON.parse(
    await readFile(join(noncanonical, "project.json"), "utf8"),
  );
  await writeFile(
    join(noncanonical, "project.json"),
    JSON.stringify(project, null, 2),
  );
  await assert.rejects(
    verifyProject(noncanonical),
    /not canonical RFC 8785 JSON/,
  );

  const linkedCache = join(temporaryRoot, "linked-cache");
  await cp(valid, linkedCache, { recursive: true });
  await mkdir(join(temporaryRoot, "outside-cache"));
  await symlink(
    join(temporaryRoot, "outside-cache"),
    join(linkedCache, "cache"),
    "junction",
  );
  await assert.rejects(
    verifyProject(linkedCache),
    /symbolic links and junctions are forbidden/,
  );

  const wrongRoleRoot = join(temporaryRoot, "wrong-role-root");
  await cp(valid, wrongRoleRoot, { recursive: true });
  const wrongRoleProject = JSON.parse(
    await readFile(join(wrongRoleRoot, "project.json"), "utf8"),
  );
  wrongRoleProject.resources[0].path =
    "clips/018f47a2-6b2d-7c11-8a44-001122334456.json";
  await writeFile(
    join(wrongRoleRoot, "project.json"),
    canonicalize(wrongRoleProject),
  );
  await assert.rejects(
    verifyProject(wrongRoleRoot),
    /path does not match role track/,
  );

  const duplicateId = join(temporaryRoot, "duplicate-id");
  await cp(valid, duplicateId, { recursive: true });
  const duplicateIdProject = JSON.parse(
    await readFile(join(duplicateId, "project.json"), "utf8"),
  );
  duplicateIdProject.assets[0].id = duplicateIdProject.resources[0].id;
  await writeFile(
    join(duplicateId, "project.json"),
    canonicalize(duplicateIdProject),
  );
  await assert.rejects(
    verifyProject(duplicateId),
    /entity identifiers must be unique/,
  );

  const mismatchedResourceId = join(temporaryRoot, "mismatched-resource-id");
  await cp(valid, mismatchedResourceId, { recursive: true });
  await mutateFirstTrack(mismatchedResourceId, (track) => {
    track.id = "018f47a2-6b2d-7c11-8a44-001122334499";
  });
  await assert.rejects(
    verifyProject(mismatchedResourceId),
    /resource identifier does not match project.json/,
  );

  const foreignProject = join(temporaryRoot, "foreign-project");
  await cp(valid, foreignProject, { recursive: true });
  await mutateFirstTrack(foreignProject, (track) => {
    track.project_id = "018f47a2-6b2d-7c11-8a44-001122334499";
  });
  await assert.rejects(
    verifyProject(foreignProject),
    /resource belongs to another project/,
  );

  const unsupportedRole = join(temporaryRoot, "unsupported-role");
  await cp(valid, unsupportedRole, { recursive: true });
  const unsupportedProjectPath = join(unsupportedRole, "project.json");
  const unsupportedProject = JSON.parse(
    await readFile(unsupportedProjectPath, "utf8"),
  );
  const unsupportedRecord = unsupportedProject.resources[0];
  unsupportedRecord.role = "device";
  await writeFile(unsupportedProjectPath, canonicalize(unsupportedProject));
  await assert.rejects(
    verifyProject(unsupportedRole),
    /project schema validation failed/,
  );

  const missingTimeline = join(temporaryRoot, "missing-timeline");
  await cp(valid, missingTimeline, { recursive: true });
  const missingTimelineProjectPath = join(missingTimeline, "project.json");
  const missingTimelineProject = JSON.parse(
    await readFile(missingTimelineProjectPath, "utf8"),
  );
  const timelineRecord = missingTimelineProject.resources.find(
    (record) => record.role === "timeline",
  );
  missingTimelineProject.resources = missingTimelineProject.resources.filter(
    (record) => record.role !== "timeline",
  );
  await unlink(join(missingTimeline, ...timelineRecord.path.split("/")));
  await writeFile(
    missingTimelineProjectPath,
    canonicalize(missingTimelineProject),
  );
  await assert.rejects(
    verifyProject(missingTimeline),
    /project schema validation failed/,
  );

  const duplicateTimeline = join(temporaryRoot, "duplicate-timeline");
  await cp(valid, duplicateTimeline, { recursive: true });
  const duplicateTimelineProjectPath = join(duplicateTimeline, "project.json");
  const duplicateTimelineProject = JSON.parse(
    await readFile(duplicateTimelineProjectPath, "utf8"),
  );
  const originalTimelineRecord = duplicateTimelineProject.resources.find(
    (record) => record.role === "timeline",
  );
  const duplicateTimelinePath = "timeline/duplicate.json";
  const duplicateTimelineValue = JSON.parse(
    await readFile(
      join(duplicateTimeline, ...originalTimelineRecord.path.split("/")),
      "utf8",
    ),
  );
  duplicateTimelineValue.id = "018f47a2-6b2d-7c11-8a44-001122334460";
  const duplicateTimelineBytes = Buffer.from(
    canonicalize(duplicateTimelineValue),
    "utf8",
  );
  await writeFile(
    join(duplicateTimeline, ...duplicateTimelinePath.split("/")),
    duplicateTimelineBytes,
  );
  duplicateTimelineProject.resources.push(
    record(
      duplicateTimelineValue.id,
      "timeline",
      duplicateTimelinePath,
      "application/json",
      duplicateTimelineBytes,
    ),
  );
  await writeFile(
    duplicateTimelineProjectPath,
    canonicalize(duplicateTimelineProject),
  );
  await assert.rejects(
    verifyProject(duplicateTimeline),
    /project schema validation failed/,
  );

  const unsortedTimeline = join(temporaryRoot, "unsorted-timeline");
  await cp(valid, unsortedTimeline, { recursive: true });
  await mutateTimeline(unsortedTimeline, (timeline) => {
    timeline.tempo_events.push(
      { tick: 3840, microseconds_per_quarter: 750000 },
      { tick: 1920, microseconds_per_quarter: 1000000 },
    );
  });
  await assert.rejects(
    verifyProject(unsortedTimeline),
    /tempo events ticks must be strictly increasing/,
  );

  const duplicateOrder = join(temporaryRoot, "duplicate-order");
  await cp(valid, duplicateOrder, { recursive: true });
  const duplicateOrderPath = join(duplicateOrder, "project.json");
  const duplicateOrderProject = JSON.parse(
    await readFile(duplicateOrderPath, "utf8"),
  );
  const secondTrack = {
    ...JSON.parse(
      await readFile(
        join(
          duplicateOrder,
          ...duplicateOrderProject.resources[0].path.split("/"),
        ),
        "utf8",
      ),
    ),
    id: "018f47a2-6b2d-7c11-8a44-001122334458",
    name: "Audio 2",
  };
  const secondTrackBytes = Buffer.from(canonicalize(secondTrack), "utf8");
  const secondTrackPath = "tracks/018f47a2-6b2d-7c11-8a44-001122334458.json";
  await writeFile(
    join(duplicateOrder, ...secondTrackPath.split("/")),
    secondTrackBytes,
  );
  duplicateOrderProject.resources.push(
    record(
      secondTrack.id,
      "track",
      secondTrackPath,
      "application/json",
      secondTrackBytes,
    ),
  );
  await writeFile(duplicateOrderPath, canonicalize(duplicateOrderProject));
  await assert.rejects(
    verifyProject(duplicateOrder),
    /track order values must be unique/,
  );

  const missingParent = join(temporaryRoot, "missing-parent");
  await cp(valid, missingParent, { recursive: true });
  await mutateFirstTrack(missingParent, (track) => {
    track.parent_track_id = "018f47a2-6b2d-7c11-8a44-001122334499";
  });
  await assert.rejects(
    verifyProject(missingParent),
    /parent track does not exist/,
  );

  const hierarchyCycle = join(temporaryRoot, "hierarchy-cycle");
  await cp(valid, hierarchyCycle, { recursive: true });
  const secondGroupId = "018f47a2-6b2d-7c11-8a44-001122334458";
  await mutateFirstTrack(hierarchyCycle, (track) => {
    track.kind = "group";
    track.clip_ids = [];
    track.parent_track_id = secondGroupId;
  });
  const cycleProjectPath = join(hierarchyCycle, "project.json");
  const cycleProject = JSON.parse(await readFile(cycleProjectPath, "utf8"));
  const firstGroup = JSON.parse(
    await readFile(
      join(hierarchyCycle, ...cycleProject.resources[0].path.split("/")),
      "utf8",
    ),
  );
  const secondGroup = {
    ...firstGroup,
    id: secondGroupId,
    name: "Group 2",
    order: 1,
    parent_track_id: firstGroup.id,
  };
  const secondGroupBytes = Buffer.from(canonicalize(secondGroup), "utf8");
  const secondGroupPath = `tracks/${secondGroupId}.json`;
  await writeFile(
    join(hierarchyCycle, ...secondGroupPath.split("/")),
    secondGroupBytes,
  );
  cycleProject.resources.push(
    record(
      secondGroupId,
      "track",
      secondGroupPath,
      "application/json",
      secondGroupBytes,
    ),
  );
  await writeFile(cycleProjectPath, canonicalize(cycleProject));
  await assert.rejects(
    verifyProject(hierarchyCycle),
    /track hierarchy contains a cycle/,
  );

  const missingClip = join(temporaryRoot, "missing-clip-reference");
  await cp(valid, missingClip, { recursive: true });
  await mutateFirstTrack(missingClip, (track) => {
    track.clip_ids = ["018f47a2-6b2d-7c11-8a44-001122334499"];
  });
  await assert.rejects(
    verifyProject(missingClip),
    /referenced clip does not exist/,
  );

  const invalidClipWarp = join(temporaryRoot, "invalid-clip-warp");
  await cp(valid, invalidClipWarp, { recursive: true });
  await mutateFirstClip(invalidClipWarp, (clip) => {
    clip.warp_markers.at(-1).source_frame_offset -= 1;
  });
  await assert.rejects(
    verifyProject(invalidClipWarp),
    /last warp marker must equal source frame count/,
  );

  const foreignClipAsset = join(temporaryRoot, "foreign-clip-asset");
  await cp(valid, foreignClipAsset, { recursive: true });
  await mutateFirstClip(foreignClipAsset, (clip) => {
    clip.source.asset_id = "018f47a2-6b2d-7c11-8a44-001122334499";
  });
  await assert.rejects(
    verifyProject(foreignClipAsset),
    /source asset does not exist/,
  );

  const oversizedClipSource = join(temporaryRoot, "oversized-clip-source");
  await cp(valid, oversizedClipSource, { recursive: true });
  await mutateFirstClip(oversizedClipSource, (clip) => {
    clip.source.start_frame = 1;
  });
  await assert.rejects(
    verifyProject(oversizedClipSource),
    /source frame range exceeds audio asset/,
  );

  const wrongAudioMediaType = join(temporaryRoot, "wrong-audio-media-type");
  await cp(valid, wrongAudioMediaType, { recursive: true });
  const wrongAudioProjectPath = join(wrongAudioMediaType, "project.json");
  const wrongAudioProject = JSON.parse(
    await readFile(wrongAudioProjectPath, "utf8"),
  );
  wrongAudioProject.assets[0].media_type = "audio/flac";
  await writeFile(wrongAudioProjectPath, canonicalize(wrongAudioProject));
  await assert.rejects(
    verifyProject(wrongAudioMediaType),
    /audio container does not match declared media type/,
  );

  const wrongClipOwner = join(temporaryRoot, "wrong-clip-owner");
  await cp(valid, wrongClipOwner, { recursive: true });
  await mutateFirstClip(wrongClipOwner, (clip) => {
    clip.track_id = "018f47a2-6b2d-7c11-8a44-001122334499";
  });
  await assert.rejects(
    verifyProject(wrongClipOwner),
    /clip owner does not match referencing track/,
  );

  const nonAudioOwner = join(temporaryRoot, "non-audio-clip-owner");
  await cp(valid, nonAudioOwner, { recursive: true });
  await mutateFirstTrack(nonAudioOwner, (track) => {
    track.kind = "midi";
  });
  await assert.rejects(
    verifyProject(nonAudioOwner),
    /owning track must be audio/,
  );

  const orphanClip = join(temporaryRoot, "orphan-clip");
  await cp(valid, orphanClip, { recursive: true });
  await mutateFirstTrack(orphanClip, (track) => {
    track.clip_ids = [];
  });
  await assert.rejects(
    verifyProject(orphanClip),
    /owning track does not reference clip/,
  );

  const missingSceneTrack = join(temporaryRoot, "missing-scene-track");
  await cp(valid, missingSceneTrack, { recursive: true });
  await mutateFirstScene(missingSceneTrack, (scene) => {
    scene.slots[0].track_id = "018f47a2-6b2d-7c11-8a44-001122334499";
  });
  await assert.rejects(
    verifyProject(missingSceneTrack),
    /scene slot track does not exist/,
  );

  const missingSceneClip = join(temporaryRoot, "missing-scene-clip");
  await cp(valid, missingSceneClip, { recursive: true });
  await mutateFirstScene(missingSceneClip, (scene) => {
    scene.slots[0].clip_id = "018f47a2-6b2d-7c11-8a44-001122334499";
  });
  await assert.rejects(
    verifyProject(missingSceneClip),
    /scene slot clip does not exist/,
  );

  const crossTrackSceneClip = join(temporaryRoot, "cross-track-scene-clip");
  await cp(valid, crossTrackSceneClip, { recursive: true });
  const crossTrackProjectPath = join(crossTrackSceneClip, "project.json");
  const crossTrackProject = JSON.parse(
    await readFile(crossTrackProjectPath, "utf8"),
  );
  const firstTrackRecord = crossTrackProject.resources.find(
    (candidate) => candidate.role === "track",
  );
  const secondSceneTrack = {
    ...JSON.parse(
      await readFile(
        join(crossTrackSceneClip, ...firstTrackRecord.path.split("/")),
        "utf8",
      ),
    ),
    id: "018f47a2-6b2d-7c11-8a44-001122334458",
    name: "Audio 2",
    order: 1,
    clip_ids: [],
  };
  const secondSceneTrackBytes = Buffer.from(
    canonicalize(secondSceneTrack),
    "utf8",
  );
  const secondSceneTrackPath = `tracks/${secondSceneTrack.id}.json`;
  await writeFile(
    join(crossTrackSceneClip, ...secondSceneTrackPath.split("/")),
    secondSceneTrackBytes,
  );
  crossTrackProject.resources.push(
    record(
      secondSceneTrack.id,
      "track",
      secondSceneTrackPath,
      "application/json",
      secondSceneTrackBytes,
    ),
  );
  await writeFile(crossTrackProjectPath, canonicalize(crossTrackProject));
  await mutateFirstScene(crossTrackSceneClip, (scene) => {
    scene.slots[0].track_id = secondSceneTrack.id;
  });
  await assert.rejects(
    verifyProject(crossTrackSceneClip),
    /scene slot clip belongs to another track/,
  );

  const duplicateSceneOrder = join(temporaryRoot, "duplicate-scene-order");
  await cp(valid, duplicateSceneOrder, { recursive: true });
  const duplicateSceneProjectPath = join(duplicateSceneOrder, "project.json");
  const duplicateSceneProject = JSON.parse(
    await readFile(duplicateSceneProjectPath, "utf8"),
  );
  const originalSceneRecord = duplicateSceneProject.resources.find(
    (candidate) => candidate.role === "scene",
  );
  const duplicateScene = JSON.parse(
    await readFile(
      join(duplicateSceneOrder, ...originalSceneRecord.path.split("/")),
      "utf8",
    ),
  );
  duplicateScene.id = "018f47a2-6b2d-7c11-8a44-001122334462";
  duplicateScene.name = "Chorus";
  const duplicateSceneBytes = Buffer.from(canonicalize(duplicateScene), "utf8");
  const duplicateScenePath = `scenes/${duplicateScene.id}.json`;
  await writeFile(
    join(duplicateSceneOrder, ...duplicateScenePath.split("/")),
    duplicateSceneBytes,
  );
  duplicateSceneProject.resources.push(
    record(
      duplicateScene.id,
      "scene",
      duplicateScenePath,
      "application/json",
      duplicateSceneBytes,
    ),
  );
  await writeFile(
    duplicateSceneProjectPath,
    canonicalize(duplicateSceneProject),
  );
  await assert.rejects(
    verifyProject(duplicateSceneOrder),
    /scene order values must be unique/,
  );

  const missingPlacementTrack = join(temporaryRoot, "missing-placement-track");
  await cp(valid, missingPlacementTrack, { recursive: true });
  await mutateArrangement(missingPlacementTrack, (arrangement) => {
    arrangement.placements[0].track_id = "018f47a2-6b2d-7c11-8a44-001122334499";
  });
  await assert.rejects(
    verifyProject(missingPlacementTrack),
    /placement track does not exist/,
  );

  const missingPlacementClip = join(temporaryRoot, "missing-placement-clip");
  await cp(valid, missingPlacementClip, { recursive: true });
  await mutateArrangement(missingPlacementClip, (arrangement) => {
    arrangement.placements[0].clip_id = "018f47a2-6b2d-7c11-8a44-001122334499";
  });
  await assert.rejects(
    verifyProject(missingPlacementClip),
    /placement clip does not exist/,
  );

  const crossTrackPlacement = join(temporaryRoot, "cross-track-placement");
  await cp(valid, crossTrackPlacement, { recursive: true });
  const placementTrackId = await addSecondAudioTrack(crossTrackPlacement);
  await mutateArrangement(crossTrackPlacement, (arrangement) => {
    arrangement.placements[0].track_id = placementTrackId;
  });
  await assert.rejects(
    verifyProject(crossTrackPlacement),
    /placement clip belongs to another track/,
  );

  const overlappingPlacements = join(temporaryRoot, "overlapping-placements");
  await cp(valid, overlappingPlacements, { recursive: true });
  await mutateArrangement(overlappingPlacements, (arrangement) => {
    arrangement.placements.push({
      ...arrangement.placements[0],
      id: "018f47a2-6b2d-7c11-8a44-001122334466",
      start_tick: 3840,
    });
  });
  await assert.rejects(
    verifyProject(overlappingPlacements),
    /must not overlap/,
  );

  const invalidPlacementStart = join(temporaryRoot, "invalid-placement-start");
  await cp(valid, invalidPlacementStart, { recursive: true });
  await mutateArrangement(invalidPlacementStart, (arrangement) => {
    arrangement.placements[0].clip_start_tick = 7680;
  });
  await assert.rejects(
    verifyProject(invalidPlacementStart),
    /starts outside playable clip time/,
  );

  const oversizedPlacement = join(temporaryRoot, "oversized-placement");
  await cp(valid, oversizedPlacement, { recursive: true });
  await mutateFirstClip(oversizedPlacement, (clip) => {
    clip.loop.enabled = false;
  });
  await mutateArrangement(oversizedPlacement, (arrangement) => {
    arrangement.placements[0].duration_ticks = 7681;
  });
  await assert.rejects(
    verifyProject(oversizedPlacement),
    /exceeds non-looping clip duration/,
  );

  const validSceneLaunch = join(temporaryRoot, "valid-scene-launch");
  await cp(valid, validSceneLaunch, { recursive: true });
  await mutateArrangement(validSceneLaunch, (arrangement) => {
    arrangement.placements = [];
    arrangement.scene_launches = [
      {
        id: "018f47a2-6b2d-7c11-8a44-001122334465",
        tick: 7680,
        scene_id: "018f47a2-6b2d-7c11-8a44-001122334461",
      },
    ];
  });
  await verifyProject(validSceneLaunch);

  const offGridSceneLaunch = join(temporaryRoot, "off-grid-scene-launch");
  await cp(validSceneLaunch, offGridSceneLaunch, { recursive: true });
  await mutateArrangement(offGridSceneLaunch, (arrangement) => {
    arrangement.scene_launches[0].tick = 1;
  });
  await assert.rejects(
    verifyProject(offGridSceneLaunch),
    /recorded scene launch is off its quantization grid/,
  );

  const missingLaunchScene = join(temporaryRoot, "missing-launch-scene");
  await cp(validSceneLaunch, missingLaunchScene, { recursive: true });
  await mutateArrangement(missingLaunchScene, (arrangement) => {
    arrangement.scene_launches[0].scene_id =
      "018f47a2-6b2d-7c11-8a44-001122334499";
  });
  await assert.rejects(
    verifyProject(missingLaunchScene),
    /scene launch target does not exist/,
  );

  const mixedArrangementModes = join(temporaryRoot, "mixed-arrangement-modes");
  await cp(valid, mixedArrangementModes, { recursive: true });
  await mutateArrangement(mixedArrangementModes, (arrangement) => {
    arrangement.scene_launches = [
      {
        id: "018f47a2-6b2d-7c11-8a44-001122334465",
        tick: 7680,
        scene_id: "018f47a2-6b2d-7c11-8a44-001122334461",
      },
    ];
  });
  await assert.rejects(
    verifyProject(mixedArrangementModes),
    /track cannot mix placements and scene launches/,
  );

  const collidingArrangementId = join(
    temporaryRoot,
    "colliding-arrangement-id",
  );
  await cp(valid, collidingArrangementId, { recursive: true });
  await mutateArrangement(collidingArrangementId, (arrangement) => {
    arrangement.placements[0].id = "018f47a2-6b2d-7c11-8a44-001122334456";
  });
  await assert.rejects(
    verifyProject(collidingArrangementId),
    /arrangement event identifier collides with a project entity/,
  );

  const duplicateArrangement = join(temporaryRoot, "duplicate-arrangement");
  await cp(valid, duplicateArrangement, { recursive: true });
  const duplicateArrangementProjectPath = join(
    duplicateArrangement,
    "project.json",
  );
  const duplicateArrangementProject = JSON.parse(
    await readFile(duplicateArrangementProjectPath, "utf8"),
  );
  const originalArrangementRecord = duplicateArrangementProject.resources.find(
    (candidate) => candidate.role === "arrangement",
  );
  const secondArrangement = JSON.parse(
    await readFile(
      join(duplicateArrangement, ...originalArrangementRecord.path.split("/")),
      "utf8",
    ),
  );
  secondArrangement.id = "018f47a2-6b2d-7c11-8a44-001122334467";
  secondArrangement.name = "Alternate";
  const secondArrangementBytes = Buffer.from(
    canonicalize(secondArrangement),
    "utf8",
  );
  const secondArrangementPath = "arrangement/alternate.json";
  await writeFile(
    join(duplicateArrangement, ...secondArrangementPath.split("/")),
    secondArrangementBytes,
  );
  duplicateArrangementProject.resources.push(
    record(
      secondArrangement.id,
      "arrangement",
      secondArrangementPath,
      "application/json",
      secondArrangementBytes,
    ),
  );
  await writeFile(
    duplicateArrangementProjectPath,
    canonicalize(duplicateArrangementProject),
  );
  await assert.rejects(
    verifyProject(duplicateArrangement),
    /project must contain at most one arrangement/,
  );

  const missingMixerChannel = join(temporaryRoot, "missing-mixer-channel");
  await cp(valid, missingMixerChannel, { recursive: true });
  await mutateMixer(missingMixerChannel, (mixer) => {
    mixer.channels = [];
  });
  await assert.rejects(
    verifyProject(missingMixerChannel),
    /mixer channels must cover mixable tracks in track order/,
  );

  const missingMixerDestination = join(
    temporaryRoot,
    "missing-mixer-destination",
  );
  await cp(valid, missingMixerDestination, { recursive: true });
  await mutateMixer(missingMixerDestination, (mixer) => {
    mixer.channels[0].destination = {
      kind: "track",
      track_id: "018f47a2-6b2d-7c11-8a44-001122334499",
    };
  });
  await assert.rejects(
    verifyProject(missingMixerDestination),
    /primary destination must be a group track/,
  );

  const wrongReturnBus = join(temporaryRoot, "wrong-return-bus");
  await cp(valid, wrongReturnBus, { recursive: true });
  await mutateMixer(wrongReturnBus, (mixer) => {
    mixer.return_buses = [{ bus: "a", track_id: mixer.channels[0].track_id }];
  });
  await assert.rejects(
    verifyProject(wrongReturnBus),
    /return bus a must reference a return track/,
  );

  const unconfiguredSend = join(temporaryRoot, "unconfigured-send");
  await cp(valid, unconfiguredSend, { recursive: true });
  await mutateMixer(unconfiguredSend, (mixer) => {
    mixer.channels[0].sends = [
      {
        id: "018f47a2-6b2d-7c11-8a44-001122334471",
        bus: "a",
        tap: "post-fader",
        gain_millionths: 500_000,
        pan_millionths: 0,
        enabled: true,
      },
    ];
  });
  await assert.rejects(
    verifyProject(unconfiguredSend),
    /send references an unconfigured return bus/,
  );

  const collidingSendId = join(temporaryRoot, "colliding-send-id");
  await cp(valid, collidingSendId, { recursive: true });
  await mutateMixer(collidingSendId, (mixer) => {
    mixer.channels[0].sends = [
      {
        id: mixer.channels[0].track_id,
        bus: "a",
        tap: "post-fader",
        gain_millionths: 500_000,
        pan_millionths: 0,
        enabled: true,
      },
    ];
  });
  await assert.rejects(
    verifyProject(collidingSendId),
    /mixer send identifier collides with a project entity/,
  );

  const validReturnSend = join(temporaryRoot, "valid-return-send");
  await cp(valid, validReturnSend, { recursive: true });
  const returnTrack = await addReturnTrack(validReturnSend);
  await mutateMixer(validReturnSend, (mixer) => {
    mixer.channels.push({
      track_id: returnTrack.id,
      gain_millionths: 1_000_000,
      pan_millionths: 0,
      mute: false,
      solo: false,
      destination: { kind: "main" },
      sends: [],
    });
    mixer.return_buses = [{ bus: "a", track_id: returnTrack.id }];
    mixer.channels[0].sends = [
      {
        id: "018f47a2-6b2d-7c11-8a44-001122334471",
        bus: "a",
        tap: "post-fader",
        gain_millionths: 500_000,
        pan_millionths: 0,
        enabled: true,
      },
    ];
  });
  await verifyProject(validReturnSend);

  const duplicateMixer = join(temporaryRoot, "duplicate-mixer");
  await cp(valid, duplicateMixer, { recursive: true });
  const duplicateMixerProjectPath = join(duplicateMixer, "project.json");
  const duplicateMixerProject = JSON.parse(
    await readFile(duplicateMixerProjectPath, "utf8"),
  );
  const originalMixerRecord = duplicateMixerProject.resources.find(
    (candidate) => candidate.role === "mixer",
  );
  const secondMixer = JSON.parse(
    await readFile(
      join(duplicateMixer, ...originalMixerRecord.path.split("/")),
      "utf8",
    ),
  );
  secondMixer.id = "018f47a2-6b2d-7c11-8a44-001122334473";
  const secondMixerBytes = Buffer.from(canonicalize(secondMixer), "utf8");
  const secondMixerPath = "mixer/alternate.json";
  await writeFile(
    join(duplicateMixer, ...secondMixerPath.split("/")),
    secondMixerBytes,
  );
  duplicateMixerProject.resources.push(
    record(
      secondMixer.id,
      "mixer",
      secondMixerPath,
      "application/json",
      secondMixerBytes,
    ),
  );
  await writeFile(
    duplicateMixerProjectPath,
    canonicalize(duplicateMixerProject),
  );
  await assert.rejects(
    verifyProject(duplicateMixer),
    /project must contain at most one mixer/,
  );

  const validCvMapping = join(temporaryRoot, "valid-cv-mapping");
  await cp(valid, validCvMapping, { recursive: true });
  await mutateMappings(validCvMapping, (mappings, project) => {
    project.required_capabilities.cv_inputs = 1;
    mappings.bindings = [cvGainBinding()];
  });
  await verifyProject(validCvMapping);

  const unavailableCvMapping = join(temporaryRoot, "unavailable-cv-mapping");
  await cp(valid, unavailableCvMapping, { recursive: true });
  await mutateMappings(unavailableCvMapping, (mappings) => {
    mappings.bindings = [cvGainBinding()];
  });
  await assert.rejects(
    verifyProject(unavailableCvMapping),
    /mapping source requires an unavailable CV input/,
  );

  const invalidMappingSolo = join(temporaryRoot, "invalid-mapping-solo");
  await cp(valid, invalidMappingSolo, { recursive: true });
  const mappingReturnTrack = await addReturnTrack(invalidMappingSolo);
  await mutateMixer(invalidMappingSolo, (mixer) => {
    mixer.channels.push({
      track_id: mappingReturnTrack.id,
      gain_millionths: 1_000_000,
      pan_millionths: 0,
      mute: false,
      solo: false,
      destination: { kind: "main" },
      sends: [],
    });
  });
  await mutateMappings(invalidMappingSolo, (mappings, project) => {
    project.required_capabilities.has_clock_input = true;
    mappings.bindings = [
      {
        id: "018f47a2-6b2d-7c11-8a44-001122334475",
        enabled: true,
        source: { kind: "clock-input", edge: "rising" },
        scope: { kind: "global" },
        transform: { kind: "trigger", behavior: "toggle" },
        target: {
          kind: "mixer-channel",
          track_id: mappingReturnTrack.id,
          parameter: "solo",
        },
      },
    ];
  });
  await assert.rejects(
    verifyProject(invalidMappingSolo),
    /mapping solo target must be a source track/,
  );

  const unavailableClockMapping = join(
    temporaryRoot,
    "unavailable-clock-mapping",
  );
  await cp(valid, unavailableClockMapping, { recursive: true });
  await mutateMappings(unavailableClockMapping, (mappings) => {
    mappings.bindings = [
      {
        id: "018f47a2-6b2d-7c11-8a44-001122334475",
        enabled: true,
        source: { kind: "clock-input", edge: "rising" },
        scope: { kind: "global" },
        transform: { kind: "trigger", behavior: "invoke" },
        target: { kind: "transport", action: "play" },
      },
    ];
  });
  await assert.rejects(
    verifyProject(unavailableClockMapping),
    /mapping source requires an unavailable Clock input/,
  );

  const missingMappingTarget = join(temporaryRoot, "missing-mapping-target");
  await cp(validCvMapping, missingMappingTarget, { recursive: true });
  await mutateMappings(missingMappingTarget, (mappings) => {
    mappings.bindings[0].target.track_id =
      "018f47a2-6b2d-7c11-8a44-001122334499";
  });
  await assert.rejects(
    verifyProject(missingMappingTarget),
    /mapping target mixer channel does not exist/,
  );

  const missingMappingScope = join(temporaryRoot, "missing-mapping-scope");
  await cp(validCvMapping, missingMappingScope, { recursive: true });
  await mutateMappings(missingMappingScope, (mappings) => {
    mappings.bindings[0].scope = {
      kind: "scene",
      scene_id: "018f47a2-6b2d-7c11-8a44-001122334499",
    };
  });
  await assert.rejects(
    verifyProject(missingMappingScope),
    /mapping scene scope does not exist/,
  );

  const collidingBindingId = join(temporaryRoot, "colliding-binding-id");
  await cp(validCvMapping, collidingBindingId, { recursive: true });
  await mutateMappings(collidingBindingId, (mappings) => {
    mappings.bindings[0].id = "018f47a2-6b2d-7c11-8a44-001122334456";
  });
  await assert.rejects(
    verifyProject(collidingBindingId),
    /mapping binding identifier collides with another project entity/,
  );

  const duplicateMappingSet = join(temporaryRoot, "duplicate-mapping-set");
  await cp(valid, duplicateMappingSet, { recursive: true });
  const duplicateMappingProjectPath = join(duplicateMappingSet, "project.json");
  const duplicateMappingProject = JSON.parse(
    await readFile(duplicateMappingProjectPath, "utf8"),
  );
  const originalMappingRecord = duplicateMappingProject.resources.find(
    (candidate) => candidate.role === "mapping",
  );
  const secondMappingSet = JSON.parse(
    await readFile(
      join(duplicateMappingSet, ...originalMappingRecord.path.split("/")),
      "utf8",
    ),
  );
  secondMappingSet.id = "018f47a2-6b2d-7c11-8a44-001122334476";
  const secondMappingBytes = Buffer.from(
    canonicalize(secondMappingSet),
    "utf8",
  );
  const secondMappingPath = "mappings/alternate.json";
  await writeFile(
    join(duplicateMappingSet, ...secondMappingPath.split("/")),
    secondMappingBytes,
  );
  duplicateMappingProject.resources.push(
    record(
      secondMappingSet.id,
      "mapping",
      secondMappingPath,
      "application/json",
      secondMappingBytes,
    ),
  );
  await writeFile(
    duplicateMappingProjectPath,
    canonicalize(duplicateMappingProject),
  );
  await assert.rejects(
    verifyProject(duplicateMappingSet),
    /project must contain at most one control-mapping resource/,
  );

  console.log("Project verifier passed: 69 filesystem cases");
} finally {
  await rm(temporaryRoot, { recursive: true, force: true });
}
