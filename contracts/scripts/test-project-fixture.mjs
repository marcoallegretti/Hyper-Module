import { createHash } from "node:crypto";
import { mkdir, writeFile } from "node:fs/promises";
import { join } from "node:path";

import canonicalize from "canonicalize";

export async function createMinimalProject(
  parent,
  name = "project",
  { playbackReady = false, revision = 7 } = {},
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
  if (playbackReady) {
    const mixer = {
      format: "hyper-mixer",
      format_version: 1,
      id: "018f47a2-6b2d-7c11-8a44-001122334468",
      project_id: projectId,
      channels: [],
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
    assets: [],
    extensions: {},
  };
  const projectBytes = Buffer.from(canonicalize(project), "utf8");
  await writeFile(join(root, "project.json"), projectBytes);
  return { project, projectBytes, root, timelineBytes, timelinePath };
}
