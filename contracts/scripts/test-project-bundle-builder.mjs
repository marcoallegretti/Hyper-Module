import assert from "node:assert/strict";
import { randomUUID } from "node:crypto";
import {
  cp,
  mkdir,
  mkdtemp,
  readFile,
  rename,
  rm,
  writeFile,
} from "node:fs/promises";
import { tmpdir } from "node:os";
import { join } from "node:path";

import { buildProjectBundle } from "./build-project-bundle.mjs";
import { createMinimalProject } from "./test-project-fixture.mjs";
import { verifyBundle } from "./verify-bundle.mjs";

const temporaryRoot = await mkdtemp(
  join(tmpdir(), `hyper-project-bundle-${randomUUID()}-`),
);
try {
  const fixture = await createMinimalProject(temporaryRoot, "source");
  const originalProjectBytes = await readFile(
    join(fixture.root, "project.json"),
  );
  const firstPath = join(temporaryRoot, "first.runtime");
  const secondPath = join(temporaryRoot, "second.runtime");
  const first = await buildProjectBundle(fixture.root, firstPath, "1.0.0");
  const second = await buildProjectBundle(fixture.root, secondPath, "1.0.0");
  assert.equal(first.identity, second.identity);
  const firstManifestBytes = await readFile(join(firstPath, "manifest.json"));
  const secondManifestBytes = await readFile(join(secondPath, "manifest.json"));
  assert(firstManifestBytes.equals(secondManifestBytes));
  const manifest = await verifyBundle(firstPath);
  assert.equal(manifest.entrypoint, "project.json");
  assert.equal(manifest.files.length, 2);
  assert(
    (await readFile(join(fixture.root, "project.json"))).equals(
      originalProjectBytes,
    ),
    "bundle compilation changed the source project",
  );

  const noncanonical = join(temporaryRoot, "noncanonical-source");
  await cp(fixture.root, noncanonical, { recursive: true });
  const project = JSON.parse(
    await readFile(join(noncanonical, "project.json"), "utf8"),
  );
  await writeFile(
    join(noncanonical, "project.json"),
    JSON.stringify(project, null, 2),
  );
  await assert.rejects(
    buildProjectBundle(
      noncanonical,
      join(temporaryRoot, "noncanonical.runtime"),
      "1.0.0",
    ),
    /project.json is not canonical/,
  );

  await assert.rejects(
    buildProjectBundle(
      fixture.root,
      join(temporaryRoot, "invalid-version.runtime"),
      "latest",
    ),
    /build-plan schema validation failed/,
  );

  await assert.rejects(
    buildProjectBundle(
      fixture.root,
      join(temporaryRoot, "bounded.runtime"),
      "1.0.0",
      { maximumBundleBytes: originalProjectBytes.byteLength },
    ),
    /project exceeds .* bytes/,
  );

  const cleanupOutput = join(temporaryRoot, "retained-cleanup.runtime");
  let replacementPlanDirectory;
  let displacedPlanDirectory;
  let cleanupResult;
  let cleanupError;
  try {
    cleanupResult = await buildProjectBundle(
      fixture.root,
      cleanupOutput,
      "1.0.0",
      {
        planObserver: async (boundary, { planDirectory }) => {
          assert.equal(boundary, "before-plan-cleanup");
          replacementPlanDirectory = planDirectory;
          displacedPlanDirectory = `${planDirectory}-displaced`;
          await rename(planDirectory, displacedPlanDirectory);
          await mkdir(planDirectory);
          await writeFile(join(planDirectory, "sentinel"), "preserve");
        },
      },
    );
  } catch (error) {
    cleanupError = error;
  }
  if (process.platform === "linux") {
    assert(cleanupResult);
    assert.equal(cleanupError, undefined);
  } else {
    assert.match(cleanupError?.message ?? "", /native cleanup failed/);
  }
  assert.equal(
    await readFile(join(replacementPlanDirectory, "sentinel"), "utf8"),
    "preserve",
  );

  console.log("Project bundle builder passed: 9 compilation cases");
} finally {
  await rm(temporaryRoot, { recursive: true, force: true });
}
