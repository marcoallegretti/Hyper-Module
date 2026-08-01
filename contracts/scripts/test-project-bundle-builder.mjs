import assert from "node:assert/strict";
import { randomUUID } from "node:crypto";
import {
  cp,
  mkdtemp,
  readFile,
  readdir,
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

  const directPlanOutput = join(temporaryRoot, "direct-plan.runtime");
  await buildProjectBundle(fixture.root, directPlanOutput, "1.0.0");
  await verifyBundle(directPlanOutput);
  assert.deepEqual(
    (await readdir(temporaryRoot)).filter((name) => name.includes(".plan-")),
    [],
  );

  console.log("Project bundle builder passed: 9 compilation cases");
} finally {
  await rm(temporaryRoot, { recursive: true, force: true });
}
