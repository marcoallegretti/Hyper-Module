import assert from "node:assert/strict";
import { createHash, randomUUID } from "node:crypto";
import {
  mkdir,
  mkdtemp,
  readFile,
  readdir,
  rm,
  symlink,
  writeFile,
} from "node:fs/promises";
import { join } from "node:path";
import { tmpdir } from "node:os";

import canonicalize from "canonicalize";

import { buildBundle } from "./build-bundle.mjs";
import { createMinimalProject } from "./test-project-fixture.mjs";
import { verifyBundle } from "./verify-bundle.mjs";

async function createInputs(root) {
  const fixture = await createMinimalProject(root, "source");
  const source = fixture.root;
  const sourceProjectSha256 = createHash("sha256")
    .update(fixture.projectBytes)
    .digest("hex");

  const plan = {
    extensions: {
      "org.example.fixture": {
        preserved: true,
      },
    },
    payloads: [
      {
        media_type: "application/json",
        role: "runtime-data",
        path: fixture.timelinePath,
        source_path: fixture.timelinePath,
      },
      {
        media_type: "application/json",
        role: "runtime-project",
        path: "project.json",
        source_path: "project.json",
      },
    ],
    required_capabilities: {
      extensions: {},
      has_ethernet: true,
      usb_mode: "none",
      has_clock_output: false,
      has_reset_input: false,
      has_clock_input: false,
      gate_outputs: 0,
      cv_outputs: 0,
      cv_inputs: 0,
      audio_outputs: 2,
      audio_inputs: 2,
      format_version: 1,
      format: "hyper-hardware-capabilities",
    },
    entrypoint: "project.json",
    minimum_runtime_version: "1.0.0",
    source_project_sha256: sourceProjectSha256,
    project_revision: 7,
    project_id: "018f47a2-6b2d-7c11-8a44-001122334455",
    format_version: 1,
    format: "hyper-runtime-build-plan",
  };
  const planPath = join(root, "plan.json");
  await writeFile(planPath, JSON.stringify(plan, null, 2));
  return { plan, planPath, source };
}

async function snapshotFiles(root) {
  const result = new Map();
  async function visit(directory, prefix) {
    const entries = await readdir(directory, { withFileTypes: true });
    for (const entry of entries) {
      const relativePath = prefix ? `${prefix}/${entry.name}` : entry.name;
      const absolutePath = join(directory, entry.name);
      if (entry.isDirectory()) {
        await visit(absolutePath, relativePath);
      } else {
        result.set(relativePath, await readFile(absolutePath));
      }
    }
  }
  await visit(root, "");
  return result;
}

const temporaryRoot = await mkdtemp(
  join(tmpdir(), `hyper-bundle-builder-${randomUUID()}-`),
);
try {
  const { plan, planPath, source } = await createInputs(temporaryRoot);
  const firstPath = join(temporaryRoot, "first.runtime");
  const secondPath = join(temporaryRoot, "second.runtime");
  const first = await buildBundle(planPath, source, firstPath);
  const reorderedPlanPath = join(temporaryRoot, "reordered-plan.json");
  await writeFile(reorderedPlanPath, canonicalize(plan));
  const second = await buildBundle(reorderedPlanPath, source, secondPath);
  assert.equal(first.identity, second.identity);
  await verifyBundle(firstPath);
  await verifyBundle(secondPath);

  const firstFiles = await snapshotFiles(firstPath);
  const secondFiles = await snapshotFiles(secondPath);
  assert.deepEqual([...firstFiles.keys()], [...secondFiles.keys()]);
  for (const [path, bytes] of firstFiles) {
    assert(
      bytes.equals(secondFiles.get(path)),
      `${path}: repeated builds differ`,
    );
  }

  assert.deepEqual(await buildBundle(planPath, source, firstPath), first);

  const uncertainPath = join(temporaryRoot, "uncertain.runtime");
  let publicationError;
  try {
    await buildBundle(planPath, source, uncertainPath, {
      faultAt: "after-publication-before-sync",
    });
  } catch (error) {
    publicationError = error;
  }
  assert.equal(
    publicationError?.code,
    "HYPER_PUBLISHED_DIRECTORY_DURABILITY_UNCERTAIN",
  );
  assert.equal(publicationError?.output_path, uncertainPath);
  assert.equal(publicationError?.identity, first.identity);
  assert.equal(publicationError?.published, true);
  assert.equal(publicationError?.durability, "uncertain");
  await verifyBundle(uncertainPath);
  const recoveredPublication = await buildBundle(
    planPath,
    source,
    uncertainPath,
  );
  assert.equal(recoveredPublication.output_path, uncertainPath);
  assert.equal(recoveredPublication.identity, first.identity);

  const boundedPath = join(temporaryRoot, "bounded.runtime");
  await assert.rejects(
    buildBundle(planPath, source, boundedPath, { maximumBundleBytes: 1 }),
    /runtime bundle exceeds 1 bytes/,
  );
  await assert.rejects(
    buildBundle(planPath, source, boundedPath, { maximumBundleBytes: 0 }),
    /maximum bundle bytes must be a positive safe integer/,
  );

  const unsafeSource = join(temporaryRoot, "unsafe-source");
  await mkdir(unsafeSource);
  await symlink(source, join(unsafeSource, "linked"), "junction");
  const unsafePlan = {
    ...plan,
    payloads: [
      {
        source_path: "linked/project.json",
        path: "project.json",
        role: "runtime-project",
        media_type: "application/json",
      },
    ],
  };
  const unsafePlanPath = join(temporaryRoot, "unsafe-plan.json");
  await writeFile(unsafePlanPath, JSON.stringify(unsafePlan));
  await assert.rejects(
    buildBundle(
      unsafePlanPath,
      unsafeSource,
      join(temporaryRoot, "unsafe.runtime"),
    ),
    /path escapes its root/,
  );

  const duplicatePlan = {
    ...plan,
    payloads: [plan.payloads[0], { ...plan.payloads[0] }],
    entrypoint: "audio/kick.flac",
  };
  const duplicatePlanPath = join(temporaryRoot, "duplicate-plan.json");
  await writeFile(duplicatePlanPath, JSON.stringify(duplicatePlan));
  await assert.rejects(
    buildBundle(
      duplicatePlanPath,
      source,
      join(temporaryRoot, "duplicate.runtime"),
    ),
    /payload paths must be unique/,
  );

  const bomPlanPath = join(temporaryRoot, "bom-plan.json");
  await writeFile(
    bomPlanPath,
    Buffer.concat([
      Buffer.from([0xef, 0xbb, 0xbf]),
      Buffer.from(JSON.stringify(plan)),
    ]),
  );
  await assert.rejects(
    buildBundle(bomPlanPath, source, join(temporaryRoot, "bom.runtime")),
    /byte-order mark/,
  );

  const stagingResidue = (await readdir(temporaryRoot)).filter((name) =>
    name.includes(".staging-"),
  );
  assert.deepEqual(stagingResidue, [], "failed builds left staging data");

  console.log("Bundle builder passed: 10 construction cases");
} finally {
  await rm(temporaryRoot, { recursive: true, force: true });
}
