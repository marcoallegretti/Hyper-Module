import assert from "node:assert/strict";
import { execFile } from "node:child_process";
import { randomUUID } from "node:crypto";
import { mkdtemp, rm } from "node:fs/promises";
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
  join(tmpdir(), `hyper-native-runtime-bundle-${randomUUID()}-`),
);

try {
  const fixture = await createMinimalProject(temporaryRoot, "source", {
    playbackReady: true,
  });
  const bundleRoot = join(temporaryRoot, "contract.runtime");
  const built = await buildProjectBundle(fixture.root, bundleRoot, "1.0.0");
  const verified = await verifyBundleArtifact(bundleRoot);
  assert.equal(verified.bundle_sha256, built.identity.slice("sha256:".length));

  await execFileAsync(
    "cargo",
    [
      "test",
      "--quiet",
      "--locked",
      "-p",
      "hyper-runtime-bundle",
      "--test",
      "bundle",
      "consumes_a_javascript_built_contract_bundle",
      "--",
      "--ignored",
      "--exact",
    ],
    {
      cwd: repositoryRoot,
      env: {
        ...process.env,
        HYPER_CONTRACT_RUNTIME_BUNDLE: bundleRoot,
      },
      maxBuffer: 1024 * 1024,
      timeout: 5 * 60_000,
      windowsHide: true,
    },
  );

  console.log("Native Runtime bundle conformance passed: 1 JavaScript-built artifact");
} finally {
  await rm(temporaryRoot, { recursive: true, force: true });
}
