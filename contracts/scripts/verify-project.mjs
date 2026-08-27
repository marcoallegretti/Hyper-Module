import assert from "node:assert/strict";
import { createHash } from "node:crypto";
import { existsSync } from "node:fs";
import { lstat, readFile, realpath } from "node:fs/promises";
import { dirname, join, resolve } from "node:path";
import { fileURLToPath, pathToFileURL } from "node:url";

import Ajv2020 from "ajv/dist/2020.js";
import canonicalize from "canonicalize";
import { parseStream } from "music-metadata";

import { assertArrangementInvariants } from "../lib/arrangement.mjs";
import {
  assertAutomationTakeInvariants,
  automationTargetKey,
} from "../lib/automation-take.mjs";
import { assertAudioClipInvariants } from "../lib/audio-clip.mjs";
import { assertControlMappingInvariants } from "../lib/control-mappings.mjs";
import { assertDeviceChainInvariants } from "../lib/device-chain.mjs";
import {
  collectRegularFiles,
  directoryPrefixes,
} from "../lib/filesystem-tree.mjs";
import { assertMixerInvariants } from "../lib/mixer.mjs";
import { runNativeProcess } from "../lib/native-process.mjs";
import { assertRecordingMetadataInvariants } from "../lib/recording-metadata.mjs";
import { assertSceneInvariants } from "../lib/scene.mjs";
import { strictParse } from "../lib/strict-json.mjs";
import { assertTimelineInvariants } from "../lib/timeline.mjs";
import {
  readStableRegularFile,
  verifyDescribedFile,
} from "./verify-bundle.mjs";

const contractRoot = resolve(dirname(fileURLToPath(import.meta.url)), "..");
const audioValidatorProtocolVersion = "1";
const workspaceRoot = resolve(contractRoot, "..");
const manifestLimitBytes = 1024 * 1024;
export const defaultMaximumProjectBytes = 1024 * 1024 * 1024;

async function createProjectValidator() {
  const schemaNames = [
    "common.schema.json",
    "hardware-capabilities.schema.json",
    "project.schema.json",
    "track.schema.json",
    "timeline.schema.json",
    "audio-clip.schema.json",
    "scene.schema.json",
    "arrangement.schema.json",
    "mixer.schema.json",
    "control-mappings.schema.json",
    "recording-metadata.schema.json",
    "automation-take.schema.json",
    "device-chain.schema.json",
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
  return {
    project: ajv.getSchema(schemas[2].$id),
    track: ajv.getSchema(schemas[3].$id),
    timeline: ajv.getSchema(schemas[4].$id),
    clip: ajv.getSchema(schemas[5].$id),
    scene: ajv.getSchema(schemas[6].$id),
    arrangement: ajv.getSchema(schemas[7].$id),
    mixer: ajv.getSchema(schemas[8].$id),
    mapping: ajv.getSchema(schemas[9].$id),
    "recording-metadata": ajv.getSchema(schemas[10].$id),
    "automation-take": ajv.getSchema(schemas[11].$id),
    "device-chain": ajv.getSchema(schemas[12].$id),
  };
}

function assertProjectInvariants(project) {
  const records = [...project.resources, ...project.assets];
  const identifiers = records.map((record) => record.id);
  assert.equal(
    new Set(identifiers).size,
    identifiers.length,
    "project entity identifiers must be unique",
  );
  const foldedPaths = records.map((record) => record.path.toLowerCase());
  assert.equal(
    new Set(foldedPaths).size,
    foldedPaths.length,
    "project paths must be unique after case folding",
  );

  const resourceRoots = {
    timeline: "timeline/",
    track: "tracks/",
    clip: "clips/",
    scene: "scenes/",
    arrangement: "arrangement/",
    mixer: "mixer/",
    mapping: "mappings/",
    "automation-take": "takes/",
    "recording-metadata": "assets/recordings/",
    "device-chain": "devices/",
  };
  for (const resource of project.resources) {
    assert(
      resource.path.startsWith(resourceRoots[resource.role]),
      `${resource.path}: path does not match role ${resource.role}`,
    );
  }
  assert.equal(
    project.resources.filter((resource) => resource.role === "timeline").length,
    1,
    "project must contain exactly one timeline",
  );
  for (const asset of project.assets) {
    assert(
      asset.path.startsWith(`assets/${asset.role}s/`),
      `${asset.path}: path does not match role ${asset.role}`,
    );
  }
}

function isLocalOnlyPath(path) {
  return path.startsWith("cache/") || path.startsWith("backups/");
}

async function readAudioAssetFormat(stream, asset, fileInfo) {
  let metadata;
  try {
    metadata = await parseStream(
      stream,
      {
        path: asset.path,
        size: fileInfo.byteLength,
      },
      {
        duration: true,
        skipCovers: true,
        skipPostHeaders: true,
      },
    );
  } catch (error) {
    throw new Error(`${asset.path}: audio metadata parsing failed`, {
      cause: error,
    });
  }

  const expectedContainers = {
    "audio/flac": new Set(["flac"]),
    "audio/wav": new Set(["wave", "wav"]),
  };
  const container = metadata.format.container?.toLowerCase();
  assert(
    expectedContainers[asset.media_type]?.has(container),
    `${asset.path}: audio container does not match declared media type`,
  );
  if (asset.media_type === "audio/wav") {
    const codec = metadata.format.codec;
    assert(
      codec === "PCM" || codec === "IEEE_FLOAT",
      `${asset.path}: WAV codec is unsupported`,
    );
    const bitsPerSample = metadata.format.bitsPerSample;
    const supportedWidth =
      codec === "PCM"
        ? new Set([8, 16, 24, 32]).has(bitsPerSample)
        : bitsPerSample === 32;
    assert(supportedWidth, `${asset.path}: WAV sample width is unsupported`);
  }
  const sampleRate = metadata.format.sampleRate;
  assert(
    Number.isSafeInteger(sampleRate) && sampleRate > 0,
    `${asset.path}: audio sample rate is missing or invalid`,
  );
  const channels = metadata.format.numberOfChannels;
  assert(
    Number.isSafeInteger(channels) && channels >= 1 && channels <= 2,
    `${asset.path}: version 1 supports only mono or stereo audio`,
  );

  let frameCount = metadata.format.numberOfSamples;
  if (frameCount === undefined) {
    const duration = metadata.format.duration;
    assert(
      Number.isFinite(duration) && duration > 0,
      `${asset.path}: audio frame count cannot be determined`,
    );
    const estimatedFrameCount = duration * sampleRate;
    frameCount = Math.round(estimatedFrameCount);
    assert(
      Math.abs(estimatedFrameCount - frameCount) <= 0.000001,
      `${asset.path}: audio duration does not resolve to an integer frame count`,
    );
  }
  assert(
    Number.isSafeInteger(frameCount) && frameCount > 0,
    `${asset.path}: audio frame count is missing or invalid`,
  );
  return { channels, frameCount, sampleRate };
}

export function resolveAudioValidatorPath() {
  if (process.env.HYPER_AUDIO_VALIDATOR) {
    return resolve(process.env.HYPER_AUDIO_VALIDATOR);
  }
  const executable = process.platform === "win32"
    ? "hyper-audio-validate.exe"
    : "hyper-audio-validate";
  const debugPath = resolve(workspaceRoot, "target", "debug", executable);
  return existsSync(debugPath)
    ? debugPath
    : resolve(workspaceRoot, "target", "release", executable);
}

async function validateNativeAudio(
  asset,
  format,
  fileInfo,
  nativeAudioObserver,
) {
  const decodedSampleBytes = format.frameCount * format.channels * 4;
  assert(
    Number.isSafeInteger(decodedSampleBytes) && decodedSampleBytes > 0,
    `${asset.path}: decoded audio byte length is invalid`,
  );
  await nativeAudioObserver("before-native-audio-validation", {
    absolutePath: fileInfo.absolutePath,
    path: asset.path,
  });
  try {
    await runNativeProcess(
      resolveAudioValidatorPath(),
      [
        audioValidatorProtocolVersion,
        "-",
        asset.media_type === "audio/wav" ? "wav" : "flac",
        String(format.sampleRate),
        String(fileInfo.byteLength),
        String(format.frameCount),
        String(decodedSampleBytes),
      ],
      { stdio: [fileInfo.handle.fd, "ignore", "pipe"] },
    );
  } catch (error) {
    throw new Error(`${asset.path}: native audio validation failed`, {
      cause: error,
    });
  } finally {
    await nativeAudioObserver("after-native-audio-validation", {
      absolutePath: fileInfo.absolutePath,
      path: asset.path,
    });
  }
}

export async function verifyProject(
  projectPath,
  {
    ignoredManagedPaths = [],
    maximumProjectBytes = defaultMaximumProjectBytes,
    nativeAudioObserver = async () => {},
  } = {},
) {
  assert.equal(
    typeof nativeAudioObserver,
    "function",
    "native audio observer must be a function",
  );
  return verifyProjectManifest(
    projectPath,
    ignoredManagedPaths,
    undefined,
    maximumProjectBytes,
    nativeAudioObserver,
  );
}

export async function verifyProjectCandidate(
  projectPath,
  candidateManifestBytes,
  {
    ignoredManagedPaths = [],
    maximumProjectBytes = defaultMaximumProjectBytes,
    nativeAudioObserver = async () => {},
  } = {},
) {
  assert(
    Buffer.isBuffer(candidateManifestBytes),
    "candidate project manifest must be a Buffer",
  );
  assert(
    candidateManifestBytes.length <= manifestLimitBytes,
    `candidate project manifest exceeds ${manifestLimitBytes} bytes`,
  );
  assert.equal(
    typeof nativeAudioObserver,
    "function",
    "native audio observer must be a function",
  );
  return verifyProjectManifest(
    projectPath,
    ignoredManagedPaths,
    Buffer.from(candidateManifestBytes),
    maximumProjectBytes,
    nativeAudioObserver,
  );
}

async function verifyProjectManifest(
  projectPath,
  ignoredManagedPaths,
  candidateManifestBytes = undefined,
  maximumProjectBytes = defaultMaximumProjectBytes,
  nativeAudioObserver = async () => {},
) {
  assert(
    Number.isSafeInteger(maximumProjectBytes) && maximumProjectBytes > 0,
    "project byte limit must be a positive safe integer",
  );
  const ignoredManagedPathSet = new Set(ignoredManagedPaths);
  const rootPath = resolve(projectPath);
  const rootStatus = await lstat(rootPath);
  assert(
    rootStatus.isDirectory() && !rootStatus.isSymbolicLink(),
    "project root must be a real directory",
  );
  const rootRealPath = await realpath(rootPath);

  const manifestPath = join(rootPath, "project.json");
  const activeManifestBytes = await readStableRegularFile(
    manifestPath,
    "project.json",
    manifestLimitBytes,
  );
  const manifestBytes = candidateManifestBytes ?? activeManifestBytes;
  assert(
    !manifestBytes.subarray(0, 3).equals(Buffer.from([0xef, 0xbb, 0xbf])),
    "project.json must not contain a byte-order mark",
  );
  let manifestText;
  try {
    manifestText = new TextDecoder("utf-8", { fatal: true }).decode(
      manifestBytes,
    );
  } catch (error) {
    throw new Error("project.json is not valid UTF-8", { cause: error });
  }
  const project = strictParse(manifestText);
  const canonicalBytes = Buffer.from(canonicalize(project), "utf8");
  assert(
    manifestBytes.equals(canonicalBytes),
    "project.json is not canonical RFC 8785 JSON",
  );

  const validators = await createProjectValidator();
  const validate = validators.project;
  assert(validate, "project schema was not registered");
  assert(
    validate(project),
    `project schema validation failed: ${JSON.stringify(validate.errors)}`,
  );
  assertProjectInvariants(project);

  const records = [...project.resources, ...project.assets];
  let declaredProjectBytes = manifestBytes.byteLength;
  for (const record of records) {
    assert(
      record.byte_length <= maximumProjectBytes - declaredProjectBytes,
      `project exceeds ${maximumProjectBytes} bytes`,
    );
    declaredProjectBytes += record.byte_length;
  }
  const declaredPaths = records.map((record) => record.path).sort();
  const acceptedManagedPaths = new Set([
    "project.json",
    ...declaredPaths,
    ...ignoredManagedPathSet,
  ]);
  const acceptedManagedDirectories = directoryPrefixes(acceptedManagedPaths);
  const projectTreeOptions = {
    allowDirectory: (path) =>
      acceptedManagedDirectories.has(path) || isLocalOnlyPath(`${path}/`),
    allowFile: (path) =>
      acceptedManagedPaths.has(path) || isLocalOnlyPath(path),
  };
  const allFiles = await collectRegularFiles(rootPath, projectTreeOptions);
  const managedFiles = allFiles
    .filter(
      (path) =>
        path !== "project.json" &&
        !isLocalOnlyPath(path) &&
        !ignoredManagedPathSet.has(path),
    )
    .sort();
  assert.deepEqual(
    managedFiles,
    declaredPaths,
    "managed project files do not exactly match project.json",
  );

  const resources = new Map();
  for (const record of project.resources) {
    const validator = validators[record.role];
    assert(
      validator,
      `${record.path}: resource role ${record.role} has no published payload schema`,
    );
    const bytes = await verifyDescribedFile(rootPath, rootRealPath, record, {
      captureBytes: true,
    });
    assert(bytes, `${record.path}: verified resource bytes are unavailable`);
    assert(
      bytes.length <= manifestLimitBytes,
      `${record.path}: resource exceeds ${manifestLimitBytes} bytes`,
    );
    assert(
      !bytes.subarray(0, 3).equals(Buffer.from([0xef, 0xbb, 0xbf])),
      `${record.path}: resource must not contain a byte-order mark`,
    );
    let text;
    try {
      text = new TextDecoder("utf-8", { fatal: true }).decode(bytes);
    } catch (error) {
      throw new Error(`${record.path}: resource is not valid UTF-8`, {
        cause: error,
      });
    }
    const resource = strictParse(text);
    assert(
      bytes.equals(Buffer.from(canonicalize(resource), "utf8")),
      `${record.path}: resource is not canonical RFC 8785 JSON`,
    );
    assert(
      validator(resource),
      `${record.path}: ${record.role} schema validation failed: ${JSON.stringify(
        validator.errors,
      )}`,
    );
    assert.equal(
      resource.id,
      record.id,
      `${record.path}: resource identifier does not match project.json`,
    );
    assert.equal(
      resource.project_id,
      project.project_id,
      `${record.path}: resource belongs to another project`,
    );
    if (record.role === "timeline") {
      assertTimelineInvariants(resource);
    } else if (record.role === "clip") {
      assertAudioClipInvariants(resource);
    } else if (record.role === "scene") {
      assertSceneInvariants(resource);
    } else if (record.role === "arrangement") {
      assertArrangementInvariants(resource);
    } else if (record.role === "mixer") {
      assertMixerInvariants(resource);
    } else if (record.role === "mapping") {
      assertControlMappingInvariants(resource);
    } else if (record.role === "recording-metadata") {
      assertRecordingMetadataInvariants(resource);
    } else if (record.role === "automation-take") {
      assertAutomationTakeInvariants(resource);
    } else if (record.role === "device-chain") {
      assertDeviceChainInvariants(resource);
    }
    resources.set(resource.id, resource);
  }

  const tracks = project.resources
    .filter((record) => record.role === "track")
    .map((record) => resources.get(record.id));
  const clips = project.resources
    .filter((record) => record.role === "clip")
    .map((record) => resources.get(record.id));
  const scenes = project.resources
    .filter((record) => record.role === "scene")
    .map((record) => resources.get(record.id));
  const arrangements = project.resources
    .filter((record) => record.role === "arrangement")
    .map((record) => resources.get(record.id));
  const mixers = project.resources
    .filter((record) => record.role === "mixer")
    .map((record) => resources.get(record.id));
  const mappingSets = project.resources
    .filter((record) => record.role === "mapping")
    .map((record) => resources.get(record.id));
  const recordingMetadata = project.resources
    .filter((record) => record.role === "recording-metadata")
    .map((record) => resources.get(record.id));
  const automationTakes = project.resources
    .filter((record) => record.role === "automation-take")
    .map((record) => resources.get(record.id));
  const deviceChains = project.resources
    .filter((record) => record.role === "device-chain")
    .map((record) => resources.get(record.id));
  const audioFormats = new Map();

  for (const record of project.assets) {
    if (
      (record.media_type === "audio/flac" || record.media_type === "audio/wav")
    ) {
      let decodedFormat;
      const verified = await verifyDescribedFile(rootPath, rootRealPath, record, {
        inspect: async (stream, fileInfo) => {
          decodedFormat = await readAudioAssetFormat(stream, record, fileInfo);
          return decodedFormat;
        },
        validate: (fileInfo) => {
          assert(decodedFormat, `${record.path}: audio metadata is unavailable`);
          return validateNativeAudio(
            record,
            decodedFormat,
            fileInfo,
            nativeAudioObserver,
          );
        },
      });
      audioFormats.set(record.id, verified.inspection);
    } else {
      await verifyDescribedFile(rootPath, rootRealPath, record);
    }
  }
  const trackOrders = tracks.map((track) => track.order);
  assert.equal(
    new Set(trackOrders).size,
    trackOrders.length,
    "track order values must be unique",
  );
  for (const track of tracks) {
    if (track.parent_track_id !== undefined) {
      const parent = resources.get(track.parent_track_id);
      assert(parent, `${track.name}: parent track does not exist`);
      assert.equal(
        parent.kind,
        "group",
        `${track.name}: parent track must be a group`,
      );
      const visited = new Set([track.id]);
      let cursor = parent;
      while (cursor) {
        assert(
          !visited.has(cursor.id),
          `${track.name}: track hierarchy contains a cycle`,
        );
        visited.add(cursor.id);
        cursor =
          cursor.parent_track_id === undefined
            ? undefined
            : resources.get(cursor.parent_track_id);
      }
    }
    for (const clipId of track.clip_ids) {
      const clip = resources.get(clipId);
      assert(
        clip?.format === "hyper-audio-clip",
        `${track.name}: referenced clip does not exist`,
      );
      assert.equal(
        clip.track_id,
        track.id,
        `${clip.name}: clip owner does not match referencing track`,
      );
    }
  }
  for (const clip of clips) {
    const owner = resources.get(clip.track_id);
    assert(owner, `${clip.name}: owning track does not exist`);
    assert.equal(
      owner.kind,
      "audio",
      `${clip.name}: owning track must be audio`,
    );
    assert(
      owner.clip_ids.includes(clip.id),
      `${clip.name}: owning track does not reference clip`,
    );
    const referenceCount = tracks.filter((track) =>
      track.clip_ids.includes(clip.id),
    ).length;
    assert.equal(
      referenceCount,
      1,
      `${clip.name}: clip must be referenced by exactly one track`,
    );
    const asset = project.assets.find(
      (candidate) => candidate.id === clip.source.asset_id,
    );
    assert(asset, `${clip.name}: source asset does not exist`);
    assert(
      asset.media_type === "audio/flac" || asset.media_type === "audio/wav",
      `${clip.name}: source asset must be audio`,
    );
    const audioFormat = audioFormats.get(asset.id);
    assert(audioFormat, `${asset.path}: verified audio format is unavailable`);
    assert(
      clip.source.start_frame <=
        audioFormat.frameCount - clip.source.frame_count,
      `${clip.name}: source frame range exceeds audio asset`,
    );
  }
  const recordingAssets = project.assets.filter(
    (asset) => asset.role === "recording",
  );
  assert.equal(
    recordingMetadata.length,
    recordingAssets.length,
    "every recording asset must have exactly one metadata resource",
  );
  assert.equal(
    new Set(recordingMetadata.map((metadata) => metadata.asset_id)).size,
    recordingMetadata.length,
    "recording metadata asset references must be unique",
  );
  for (const metadata of recordingMetadata) {
    const asset = project.assets.find(
      (candidate) => candidate.id === metadata.asset_id,
    );
    assert.equal(
      asset?.role,
      "recording",
      "recording metadata must reference a recording asset",
    );
    const track = resources.get(metadata.track_id);
    assert.equal(
      track?.kind,
      "audio",
      "recording metadata must reference an audio track",
    );
    for (const inputIndex of metadata.source_input_indices) {
      assert(
        inputIndex < project.required_capabilities.audio_inputs,
        "recording metadata requires an unavailable audio input",
      );
    }

    const audioFormat = audioFormats.get(asset.id);
    assert(audioFormat, `${asset.path}: verified audio format is unavailable`);
    assert.equal(
      metadata.sample_rate_hz,
      project.sample_rate_hz,
      "recording metadata sample rate must match the project",
    );
    assert.equal(
      audioFormat.sampleRate,
      metadata.sample_rate_hz,
      "recording media sample rate does not match its metadata",
    );
    assert.equal(
      audioFormat.channels,
      metadata.channels,
      "recording media channel count does not match its metadata",
    );
    assert.equal(
      audioFormat.frameCount,
      metadata.frame_count,
      "recording media frame count does not match its metadata",
    );
  }
  const sceneOrders = scenes.map((scene) => scene.order);
  assert.equal(
    new Set(sceneOrders).size,
    sceneOrders.length,
    "scene order values must be unique",
  );
  for (const scene of scenes) {
    for (const slot of scene.slots) {
      const track = resources.get(slot.track_id);
      assert(
        track?.format === "hyper-track",
        `${scene.name}: scene slot track does not exist`,
      );
      if (slot.action === "play") {
        const clip = resources.get(slot.clip_id);
        assert(
          clip?.format === "hyper-audio-clip",
          `${scene.name}: scene slot clip does not exist`,
        );
        assert.equal(
          clip.track_id,
          track.id,
          `${scene.name}: scene slot clip belongs to another track`,
        );
      }
    }
  }
  assert(
    arrangements.length <= 1,
    "project must contain at most one arrangement",
  );
  for (const arrangement of arrangements) {
    const nestedIds = [
      ...arrangement.placements.map((placement) => placement.id),
      ...arrangement.scene_launches.map((launch) => launch.id),
    ];
    for (const nestedId of nestedIds) {
      assert(
        !resources.has(nestedId) &&
          !project.assets.some((asset) => asset.id === nestedId),
        `${arrangement.name}: arrangement event identifier collides with a project entity`,
      );
    }

    for (const placement of arrangement.placements) {
      const track = resources.get(placement.track_id);
      assert(
        track?.format === "hyper-track",
        `${arrangement.name}: placement track does not exist`,
      );
      const clip = resources.get(placement.clip_id);
      assert(
        clip?.format === "hyper-audio-clip",
        `${arrangement.name}: placement clip does not exist`,
      );
      assert.equal(
        clip.track_id,
        track.id,
        `${arrangement.name}: placement clip belongs to another track`,
      );
      const playableEnd = clip.loop.enabled
        ? clip.loop.end_tick
        : clip.duration_ticks;
      assert(
        placement.clip_start_tick < playableEnd,
        `${arrangement.name}: placement starts outside playable clip time`,
      );
      if (!clip.loop.enabled) {
        assert(
          placement.duration_ticks <=
            clip.duration_ticks - placement.clip_start_tick,
          `${arrangement.name}: placement exceeds non-looping clip duration`,
        );
      }
    }

    const sceneDrivenTrackIds = new Set();
    for (const launch of arrangement.scene_launches) {
      const scene = resources.get(launch.scene_id);
      assert(
        scene?.format === "hyper-scene",
        `${arrangement.name}: scene launch target does not exist`,
      );
      if (scene.launch_quantization.mode === "grid") {
        assert.equal(
          launch.tick % scene.launch_quantization.quantum_ticks,
          0,
          `${arrangement.name}: recorded scene launch is off its quantization grid`,
        );
      }
      for (const slot of scene.slots) {
        sceneDrivenTrackIds.add(slot.track_id);
      }
    }
    for (const placement of arrangement.placements) {
      assert(
        !sceneDrivenTrackIds.has(placement.track_id),
        `${arrangement.name}: track cannot mix placements and scene launches`,
      );
    }
  }
  const arrangementEventIds = new Set(
    arrangements.flatMap((arrangement) => [
      ...arrangement.placements.map((placement) => placement.id),
      ...arrangement.scene_launches.map((launch) => launch.id),
    ]),
  );
  const deviceInstanceIds = new Set();
  const deviceTrackIds = new Set();
  const mixableDeviceTrackKinds = new Set([
    "audio",
    "external-input",
    "group",
    "return",
  ]);
  for (const chain of deviceChains) {
    const track = resources.get(chain.track_id);
    assert(
      track?.format === "hyper-track" && mixableDeviceTrackKinds.has(track.kind),
      "device chain must belong to a mixable track",
    );
    assert(
      !deviceTrackIds.has(chain.track_id),
      "a track must have at most one device chain",
    );
    deviceTrackIds.add(chain.track_id);
    for (const device of chain.devices) {
      assert(
        !resources.has(device.id) &&
          !project.assets.some((asset) => asset.id === device.id) &&
          !arrangementEventIds.has(device.id) &&
          !deviceInstanceIds.has(device.id),
        "native device identifier collides with a project entity",
      );
      deviceInstanceIds.add(device.id);
    }
  }
  assert(mixers.length <= 1, "project must contain at most one mixer");
  for (const mixer of mixers) {
    const mixableKinds = new Set([
      "audio",
      "external-input",
      "group",
      "return",
    ]);
    const expectedChannelIds = tracks
      .filter((track) => mixableKinds.has(track.kind))
      .sort((left, right) => left.order - right.order)
      .map((track) => track.id);
    assert.deepEqual(
      mixer.channels.map((channel) => channel.track_id),
      expectedChannelIds,
      "mixer channels must cover mixable tracks in track order",
    );

    const returnBusByName = new Map(
      mixer.return_buses.map((returnBus) => [returnBus.bus, returnBus]),
    );
    const nestedSendIds = mixer.channels.flatMap((channel) =>
      channel.sends.map((send) => send.id),
    );
    for (const sendId of nestedSendIds) {
      assert(
        !resources.has(sendId) &&
          !project.assets.some((asset) => asset.id === sendId) &&
          !arrangementEventIds.has(sendId),
        "mixer send identifier collides with a project entity",
      );
    }

    for (const returnBus of mixer.return_buses) {
      const returnTrack = resources.get(returnBus.track_id);
      assert.equal(
        returnTrack?.kind,
        "return",
        `return bus ${returnBus.bus} must reference a return track`,
      );
    }
    for (const channel of mixer.channels) {
      const track = resources.get(channel.track_id);
      if (track.kind === "group" || track.kind === "return") {
        assert(
          !channel.solo,
          `${track.name}: version 1 supports solo only on source tracks`,
        );
      }
      if (channel.destination.kind === "track") {
        const destination = resources.get(channel.destination.track_id);
        assert.equal(
          destination?.kind,
          "group",
          `${track.name}: primary destination must be a group track`,
        );
      }
      if (track.kind === "return") {
        assert.equal(
          channel.sends.length,
          0,
          `${track.name}: return tracks cannot feed sends`,
        );
      }
      for (const send of channel.sends) {
        assert(
          returnBusByName.has(send.bus),
          `${track.name}: send references an unconfigured return bus`,
        );
      }
    }
  }
  const mixer = mixers[0];
  const mixerSendIds = new Set(
    mixer?.channels.flatMap((channel) =>
      channel.sends.map((send) => send.id),
    ) ?? [],
  );
  const mappingBindingIds = new Set(
    mappingSets.flatMap((mappingSet) =>
      mappingSet.bindings.map((binding) => binding.id),
    ),
  );
  const automationLaneIds = new Set();
  const enabledIntervalsByTarget = new Map();
  for (const take of automationTakes) {
    for (const lane of take.lanes) {
      assert(
        !resources.has(lane.id) &&
          !project.assets.some((asset) => asset.id === lane.id) &&
          !mixerSendIds.has(lane.id) &&
          !arrangementEventIds.has(lane.id) &&
          !mappingBindingIds.has(lane.id) &&
          !automationLaneIds.has(lane.id),
        "automation lane identifier collides with another project entity",
      );
      automationLaneIds.add(lane.id);

      const target = lane.target;
      if (target.kind === "mixer-channel") {
        const channel = mixer?.channels.find(
          (candidate) => candidate.track_id === target.track_id,
        );
        assert(channel, "automation target mixer channel does not exist");
        if (target.parameter === "solo") {
          const track = resources.get(target.track_id);
          assert(
            track?.kind === "audio" || track?.kind === "external-input",
            "automation solo target must be a source track",
          );
        }
      } else if (target.kind === "mixer-send") {
        assert(
          mixerSendIds.has(target.send_id),
          "automation target mixer send does not exist",
        );
      } else {
        assert(mixer, "automation target requires a mixer");
      }

      if (take.enabled) {
        const targetKey = automationTargetKey(target);
        const intervals = enabledIntervalsByTarget.get(targetKey) ?? [];
        intervals.push({
          start: take.start_tick,
          end: take.end_tick,
          takeId: take.id,
        });
        enabledIntervalsByTarget.set(targetKey, intervals);
      }
    }
  }
  for (const intervals of enabledIntervalsByTarget.values()) {
    intervals.sort(
      (left, right) =>
        left.start - right.start ||
        left.end - right.end ||
        left.takeId.localeCompare(right.takeId),
    );
    for (let index = 1; index < intervals.length; index += 1) {
      assert(
        intervals[index - 1].end <= intervals[index].start,
        "enabled automation takes must not overlap on the same target",
      );
    }
  }
  assert(
    mappingSets.length <= 1,
    "project must contain at most one control-mapping resource",
  );
  for (const mappingSet of mappingSets) {
    for (const binding of mappingSet.bindings) {
      assert(
        !resources.has(binding.id) &&
          !project.assets.some((asset) => asset.id === binding.id) &&
          !mixerSendIds.has(binding.id) &&
          !arrangementEventIds.has(binding.id) &&
          !automationLaneIds.has(binding.id),
        "mapping binding identifier collides with another project entity",
      );
      if (binding.source.kind === "cv-input") {
        assert(
          binding.source.input_index < project.required_capabilities.cv_inputs,
          "mapping source requires an unavailable CV input",
        );
      } else if (binding.source.kind === "clock-input") {
        assert(
          project.required_capabilities.has_clock_input,
          "mapping source requires an unavailable Clock input",
        );
      } else {
        assert(
          project.required_capabilities.has_reset_input,
          "mapping source requires an unavailable Reset input",
        );
      }

      if (binding.scope.kind === "track") {
        assert(
          resources.get(binding.scope.track_id)?.format === "hyper-track",
          "mapping track scope does not exist",
        );
      } else if (binding.scope.kind === "scene") {
        assert(
          resources.get(binding.scope.scene_id)?.format === "hyper-scene",
          "mapping scene scope does not exist",
        );
      } else if (binding.scope.kind === "mixer-page") {
        assert(mixer, "mixer-page mapping requires a mixer");
      }

      const target = binding.target;
      if (target.kind === "mixer-channel") {
        const channel = mixer?.channels.find(
          (candidate) => candidate.track_id === target.track_id,
        );
        assert(channel, "mapping target mixer channel does not exist");
        if (target.parameter === "solo") {
          const track = resources.get(target.track_id);
          assert(
            track?.kind === "audio" || track?.kind === "external-input",
            "mapping solo target must be a source track",
          );
        }
      } else if (target.kind === "mixer-send") {
        assert(
          mixerSendIds.has(target.send_id),
          "mapping target mixer send does not exist",
        );
      } else if (target.kind === "mixer-main" || target.kind === "mixer-cue") {
        assert(mixer, "mapping target requires a mixer");
      } else if (target.kind === "scene") {
        assert(
          resources.get(target.scene_id)?.format === "hyper-scene",
          "mapping target scene does not exist",
        );
      }
    }
  }

  const finalManagedFiles = (await collectRegularFiles(rootPath, projectTreeOptions))
    .filter(
      (path) =>
        path !== "project.json" &&
        !isLocalOnlyPath(path) &&
        !ignoredManagedPathSet.has(path),
    )
    .sort();
  assert.deepEqual(
    finalManagedFiles,
    declaredPaths,
    "managed project files changed during verification",
  );
  assert(
    (await readFile(manifestPath)).equals(activeManifestBytes),
    "project.json changed during verification",
  );

  return {
    project,
    identity: `sha256:${createHash("sha256")
      .update(manifestBytes)
      .digest("hex")}`,
  };
}

const invokedPath = process.argv[1]
  ? pathToFileURL(resolve(process.argv[1])).href
  : undefined;
if (invokedPath === import.meta.url) {
  assert.equal(
    process.argv.length,
    3,
    "usage: node scripts/verify-project.mjs <project-directory>",
  );
  const result = await verifyProject(process.argv[2]);
  console.log(result.identity);
}
