import assert from "node:assert/strict";
import { execFile } from "node:child_process";
import { randomUUID } from "node:crypto";
import {
  mkdir,
  mkdtemp,
  readFile,
  rm,
  writeFile,
} from "node:fs/promises";
import { tmpdir } from "node:os";
import { dirname, join, resolve } from "node:path";
import { fileURLToPath } from "node:url";
import { promisify } from "node:util";

import canonicalize from "canonicalize";

import { buildProjectBundle } from "./build-project-bundle.mjs";
import {
  configureDeploymentStorage,
  deployBundle,
  readDeploymentState,
} from "./deployment-target.mjs";
import { createMinimalProject } from "./test-project-fixture.mjs";

const execFileAsync = promisify(execFile);
const scriptRoot = dirname(fileURLToPath(import.meta.url));
const repositoryRoot = resolve(scriptRoot, "..", "..");
const temporaryRoot = await mkdtemp(
  join(tmpdir(), `hyper-native-runtime-deployment-${randomUUID()}-`),
);

async function runNative(
  target,
  testName = "resolves_a_javascript_managed_deployment_target",
  mode = undefined,
) {
  await execFileAsync(
    "cargo",
    [
      "test",
      "--quiet",
      "--locked",
      "-p",
      "hyper-runtime-deployment",
      "--test",
      "contract",
      testName,
      "--",
      "--ignored",
      "--exact",
    ],
    {
      cwd: repositoryRoot,
      env: {
        ...process.env,
        HYPER_CONTRACT_DEPLOYMENT_TARGET: target,
        ...(mode === undefined
          ? {}
          : { HYPER_CONTRACT_DEPLOYMENT_MODE: mode }),
      },
      maxBuffer: 1024 * 1024,
      timeout: 5 * 60_000,
      windowsHide: true,
    },
  );
}

try {
  const source7 = await createMinimalProject(temporaryRoot, "source-7", {
    playbackReady: true,
    revision: 7,
  });
  const source8 = await createMinimalProject(temporaryRoot, "source-8", {
    playbackReady: true,
    revision: 8,
  });
  const bundle7 = join(temporaryRoot, "revision-7.runtime");
  const bundle8 = join(temporaryRoot, "revision-8.runtime");
  await buildProjectBundle(source7.root, bundle7, "1.0.0");
  await buildProjectBundle(source8.root, bundle8, "1.0.0");

  const target = join(temporaryRoot, "target");
  await mkdir(target);
  await configureDeploymentStorage(target, {
    format: "hyper-deployment-storage-policy",
    format_version: 1,
    max_installed_bundles: 8,
    max_installed_bytes: 1024 * 1024 * 1024,
    minimum_free_bytes: 0,
    extensions: {},
  });

  await deployBundle(bundle7, target, "1.0.0");
  await runNative(
    target,
    "resolves_a_javascript_managed_deployment_target",
    "promote",
  );
  await runNative(
    target,
    "leaves_a_healthy_javascript_managed_deployment_unchanged",
  );

  const pending8 = await deployBundle(bundle8, target, "1.0.0");
  assert.equal(pending8.active.status, "pending");
  const manifestPath = join(
    target,
    "bundles",
    pending8.active.bundle_sha256,
    "manifest.json",
  );
  const manifest = await readFile(manifestPath);
  const corrupted = Buffer.from(manifest);
  corrupted[0] ^= 1;
  await writeFile(manifestPath, corrupted);
  await runNative(
    target,
    "resolves_a_javascript_managed_deployment_target",
    "recover",
  );

  const recovered = await readDeploymentState(target);
  assert.equal(recovered.active.project_revision, 7);
  assert.equal(recovered.active.status, "healthy");
  const recoveredBytes = canonicalize(recovered);
  await writeFile(
    join(target, "state.json"),
    canonicalize({
      ...recovered,
      generation: Number.MAX_SAFE_INTEGER,
      active: { ...recovered.active, status: "pending" },
    }),
  );
  await runNative(
    target,
    "rejects_an_exhausted_javascript_managed_deployment_atomically",
  );
  await writeFile(join(target, "state.json"), recoveredBytes);

  const recoveredManifestPath = join(
    target,
    "bundles",
    recovered.active.bundle_sha256,
    "manifest.json",
  );
  const recoveredManifest = await readFile(recoveredManifestPath);
  const corruptedRecoveredManifest = Buffer.from(recoveredManifest);
  corruptedRecoveredManifest[0] ^= 1;
  await writeFile(recoveredManifestPath, corruptedRecoveredManifest);
  await runNative(target, "preserves_both_javascript_candidate_failures");

  const lockPath = join(target, "deployment.lock");
  await mkdir(lockPath);
  await writeFile(
    join(lockPath, "owner.json"),
    canonicalize({
      format: "hyper-deployment-lock",
      format_version: 1,
      owner_pid: process.pid,
      owner_token: randomUUID(),
      extensions: {},
    }),
  );
  await runNative(
    target,
    "refuses_a_javascript_owned_deployment_lock_without_mutation",
  );
  await rm(lockPath, { recursive: true });

  console.log(
    "Native Runtime deployment conformance passed: promotion, idempotence, recovery, failure preservation, exhaustion, and lock contention",
  );
} finally {
  await rm(temporaryRoot, { recursive: true, force: true });
}
