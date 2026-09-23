import assert from "node:assert/strict";
import { mkdir } from "node:fs/promises";
import { join, resolve } from "node:path";

import { resolveFilesystemAuthorityPath } from "../lib/native-fs-authority.mjs";
import { runNativeProcess } from "../lib/native-process.mjs";
import { buildProjectBundle } from "./build-project-bundle.mjs";
import { createMinimalProject } from "./test-project-fixture.mjs";
import { resolveAudioValidatorPath } from "./verify-project.mjs";

assert.equal(
  process.argv.length,
  4,
  "usage: node smoke-packaged-deployment.mjs <scratch-directory> <runtime-version>",
);
const scratch = resolve(process.argv[2]);
const runtimeVersion = process.argv[3];
const contractRoot = resolve(import.meta.dirname, "..");
const executable = process.platform === "win32" ? "hyper-fs-authority.exe" : "hyper-fs-authority";
assert.equal(
  resolveFilesystemAuthorityPath(),
  join(contractRoot, "bin", executable),
  "packaged filesystem authority must be selected",
);
assert.equal(
  resolveAudioValidatorPath(),
  join(
    contractRoot,
    "bin",
    process.platform === "win32" ? "hyper-audio-validate.exe" : "hyper-audio-validate",
  ),
  "packaged audio validator must be selected",
);

const project = await createMinimalProject(scratch, "project", {
  audioClip: true,
});
assert.equal(
  project.project.resources.filter((resource) => resource.role === "track").length,
  1,
  "packaged deployment smoke must exercise an audio track",
);
assert.equal(
  project.project.resources.filter((resource) => resource.role === "clip").length,
  1,
  "packaged deployment smoke must exercise an audio clip",
);
assert.equal(
  project.project.resources.filter((resource) => resource.role === "arrangement").length,
  1,
  "packaged deployment smoke must exercise an arrangement placement",
);
assert.equal(project.project.assets.length, 1);
const bundle = join(scratch, "project.runtime");
const target = join(scratch, "target");
await buildProjectBundle(project.root, bundle, runtimeVersion);
await mkdir(target);

async function invoke(...arguments_) {
  const output = await runNativeProcess(
    process.execPath,
    [join(contractRoot, "scripts", "deployment-target.mjs"), ...arguments_],
    { stdio: ["ignore", "pipe", "pipe"] },
  );
  return JSON.parse(output.toString("utf8"));
}

await invoke(
  "configure-storage",
  target,
  join(contractRoot, "storage-policy.example.json"),
);
const deployed = await invoke("deploy", bundle, target, runtimeVersion);
const readback = await invoke("status", target);
assert.deepEqual(readback, deployed, "installed deployment state must round-trip");
assert.equal(readback.generation, 1);
assert.equal(readback.active.project_revision, project.project.revision);
assert.equal(readback.active.status, "pending");
console.log("Packaged deployment smoke passed");
