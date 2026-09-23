import assert from "node:assert/strict";
import { mkdir } from "node:fs/promises";
import { join, resolve } from "node:path";

import { resolveFilesystemAuthorityPath } from "../lib/native-fs-authority.mjs";
import { runNativeProcess } from "../lib/native-process.mjs";
import { buildProjectBundle } from "./build-project-bundle.mjs";
import { createMinimalProject } from "./test-project-fixture.mjs";

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

const project = await createMinimalProject(scratch, "project", {
  playbackReady: true,
});
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
