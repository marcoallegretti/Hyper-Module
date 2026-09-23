import { createHash } from "node:crypto";
import { mkdir, writeFile } from "node:fs/promises";
import { join } from "node:path";

import canonicalize from "canonicalize";

export async function createMinimalProject(
  parent,
  name = "project",
  {
    audioAsset = false,
    audioTrack = false,
    playbackReady = false,
    revision = 7,
  } = {},
) {
  const root = join(parent, name);
  await mkdir(join(root, "timeline"), { recursive: true });
  const projectId = "018f47a2-6b2d-7c11-8a44-001122334455";
  const timeline = {
    format: "hyper-timeline",
    format_version: 1,
    id: "018f47a2-6b2d-7c11-8a44-001122334459",
    project_id: projectId,
    ticks_per_quarter: 1920,
    tempo_events: [{ tick: 0, microseconds_per_quarter: 500000 }],
    meter_events: [{ tick: 0, numerator: 4, denominator: 4 }],
    extensions: {},
  };
  const timelineBytes = Buffer.from(canonicalize(timeline), "utf8");
  const timelinePath = "timeline/timeline.json";
  await writeFile(join(root, ...timelinePath.split("/")), timelineBytes);
  const resources = [
    {
      id: timeline.id,
      role: "timeline",
      path: timelinePath,
      media_type: "application/json",
      byte_length: timelineBytes.length,
      sha256: createHash("sha256").update(timelineBytes).digest("hex"),
      extensions: {},
    },
  ];
  const trackId = "018f47a2-6b2d-7c11-8a44-001122334456";
  if (audioTrack) {
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
    const trackBytes = Buffer.from(canonicalize(track), "utf8");
    const trackPath = `tracks/${trackId}.json`;
    await mkdir(join(root, "tracks"), { recursive: true });
    await writeFile(join(root, ...trackPath.split("/")), trackBytes);
    resources.push({
      id: trackId,
      role: "track",
      path: trackPath,
      media_type: "application/json",
      byte_length: trackBytes.length,
      sha256: createHash("sha256").update(trackBytes).digest("hex"),
      extensions: {},
    });
  }
  if (playbackReady || audioTrack) {
    const mixer = {
      format: "hyper-mixer",
      format_version: 1,
      id: "018f47a2-6b2d-7c11-8a44-001122334468",
      project_id: projectId,
      channels: audioTrack
        ? [
            {
              track_id: trackId,
              gain_millionths: 1_000_000,
              pan_millionths: 0,
              mute: false,
              solo: false,
              destination: { kind: "main" },
              sends: [],
            },
          ]
        : [],
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
    const mixerPath = "mixer/mixer.json";
    await mkdir(join(root, "mixer"), { recursive: true });
    await writeFile(join(root, ...mixerPath.split("/")), mixerBytes);
    resources.push({
      id: mixer.id,
      role: "mixer",
      path: mixerPath,
      media_type: "application/json",
      byte_length: mixerBytes.length,
      sha256: createHash("sha256").update(mixerBytes).digest("hex"),
      extensions: {},
    });
  }
  const assets = [];
  if (audioAsset) {
    const assetId = "018f47a2-6b2d-7c11-8a44-001122334457";
    const assetPath = "assets/samples/reference.wav";
    const assetBytes = pcm16Wave(128);
    await mkdir(join(root, "assets", "samples"), { recursive: true });
    await writeFile(join(root, ...assetPath.split("/")), assetBytes);
    assets.push({
      id: assetId,
      role: "sample",
      path: assetPath,
      media_type: "audio/wav",
      byte_length: assetBytes.length,
      sha256: createHash("sha256").update(assetBytes).digest("hex"),
      extensions: {},
    });
  }
  const project = {
    format: "hyper-project",
    format_version: 1,
    project_id: projectId,
    revision,
    name: "Runtime fixture",
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
    resources,
    assets,
    extensions: {},
  };
  const projectBytes = Buffer.from(canonicalize(project), "utf8");
  await writeFile(join(root, "project.json"), projectBytes);
  return { project, projectBytes, root, timelineBytes, timelinePath };
}

function pcm16Wave(frameCount) {
  const channels = 1;
  const sampleRate = 48_000;
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
