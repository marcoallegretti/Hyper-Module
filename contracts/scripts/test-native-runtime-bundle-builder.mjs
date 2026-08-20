import assert from "node:assert/strict";
import { execFile } from "node:child_process";
import { randomUUID } from "node:crypto";
import { mkdtemp, readFile, rm } from "node:fs/promises";
import { tmpdir } from "node:os";
import { dirname, join, resolve } from "node:path";
import { fileURLToPath } from "node:url";
import { promisify } from "node:util";

import { buildProjectBundle } from "./build-project-bundle.mjs";
import { createMinimalProject } from "./test-project-fixture.mjs";
import { verifyBundleArtifact } from "./verify-bundle.mjs";

const execFileAsync = promisify(execFile);
const scriptRoot = dirname(fileURLToPath(import.meta.url));
const repositoryRoot = resolve(scriptRoot, "..", "..");
const temporaryRoot = await mkdtemp(
  join(tmpdir(), `hyper-native-runtime-builder-${randomUUID()}-`),
);

try {
  const fixture = await createMinimalProject(temporaryRoot, "source", {
    playbackReady: true,
  });
  const javascriptBundle = join(temporaryRoot, "javascript.runtime");
  const nativeBundle = join(temporaryRoot, "native.runtime");
  const javascript = await buildProjectBundle(
    fixture.root,
    javascriptBundle,
    "1.0.0",
  );

  await execFileAsync(
    "cargo",
    [
      "test",
      "--quiet",
      "--locked",
      "-p",
      "hyper-runtime-bundle-builder",
      "--test",
      "builder",
      "builds_a_contract_project_for_cross_language_comparison",
      "--",
      "--ignored",
      "--exact",
    ],
    {
      cwd: repositoryRoot,
      env: {
        ...process.env,
        HYPER_CONTRACT_RUNTIME_PROJECT: fixture.root,
        HYPER_CONTRACT_NATIVE_RUNTIME_BUNDLE: nativeBundle,
      },
      maxBuffer: 1024 * 1024,
      timeout: 5 * 60_000,
      windowsHide: true,
    },
  );

  const native = await verifyBundleArtifact(nativeBundle);
  assert.equal(native.bundle_sha256, javascript.identity.slice("sha256:".length));
  assert(
    (await readFile(join(nativeBundle, "manifest.json"))).equals(
      await readFile(join(javascriptBundle, "manifest.json")),
    ),
    "native and JavaScript builders emitted different canonical manifests",
  );
  console.log(
    "Native Runtime bundle builder conformance passed: 1 identical artifact",
  );
} finally {
  await rm(temporaryRoot, { recursive: true, force: true });
}
