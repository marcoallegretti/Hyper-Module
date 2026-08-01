import assert from "node:assert/strict";
import { createHash, randomUUID } from "node:crypto";
import { execFile } from "node:child_process";
import {
  access,
  cp,
  mkdir,
  mkdtemp,
  readFile,
  readdir,
  rm,
  writeFile,
} from "node:fs/promises";
import { tmpdir } from "node:os";
import { dirname, join } from "node:path";
import { fileURLToPath } from "node:url";
import { promisify } from "node:util";

import canonicalize from "canonicalize";

import { buildProjectBundle } from "./build-project-bundle.mjs";
import { restoreProject } from "./restore-project.mjs";
import { createMinimalProject } from "./test-project-fixture.mjs";
import { verifyBundle } from "./verify-bundle.mjs";
import { verifyProject } from "./verify-project.mjs";

const execFileAsync = promisify(execFile);
const scriptRoot = dirname(fileURLToPath(import.meta.url));

function canonicalBytes(value) {
  return Buffer.from(canonicalize(value), "utf8");
}

function sha256(bytes) {
  return createHash("sha256").update(bytes).digest("hex");
}

async function pathExists(path) {
  try {
    await access(path);
    return true;
  } catch (error) {
    if (error.code === "ENOENT") {
      return false;
    }
    throw error;
  }
}

const temporaryRoot = await mkdtemp(
  join(tmpdir(), `hyper-project-roundtrip-${randomUUID()}-`),
);
try {
  const fixture = await createMinimalProject(temporaryRoot, "desktop-source");
  const timeline = JSON.parse(fixture.timelineBytes.toString("utf8"));
  timeline.extensions = {
    "com.hyper.test.runtime": {
      opaque_sequence: [3, 1, 4],
      preserve_me: true,
    },
  };
  const timelineBytes = canonicalBytes(timeline);
  await writeFile(
    join(fixture.root, ...fixture.timelinePath.split("/")),
    timelineBytes,
  );

  const project = JSON.parse(fixture.projectBytes.toString("utf8"));
  project.resources[0].byte_length = timelineBytes.length;
  project.resources[0].sha256 = sha256(timelineBytes);
  project.extensions = {
    "com.hyper.test.desktop": {
      opaque_object: {
        enabled: true,
        labels: ["alpha", "beta"],
      },
    },
  };
  const projectBytes = canonicalBytes(project);
  await writeFile(join(fixture.root, "project.json"), projectBytes);

  const sourceVerification = await verifyProject(fixture.root);
  const bundlePath = join(temporaryRoot, "project.runtime");
  await buildProjectBundle(fixture.root, bundlePath, "1.0.0");
  const manifest = await verifyBundle(bundlePath);
  const bundleByteLength =
    (await readFile(join(bundlePath, "manifest.json"))).byteLength +
    manifest.files.reduce(
      (total, descriptor) => total + descriptor.byte_length,
      0,
    );

  const boundedOutputPath = join(temporaryRoot, "bounded-restore");
  await assert.rejects(
    restoreProject(bundlePath, boundedOutputPath, {
      maximumProjectBytes: bundleByteLength - 1,
    }),
    /deployment bundle exceeds/,
  );
  assert.equal(await pathExists(boundedOutputPath), false);
  const exactLimitOutputPath = join(temporaryRoot, "exact-limit-restore");
  await restoreProject(bundlePath, exactLimitOutputPath, {
    maximumProjectBytes: bundleByteLength,
  });
  assert.equal(
    (await verifyProject(exactLimitOutputPath)).identity,
    sourceVerification.identity,
  );

  const restoredPath = join(temporaryRoot, "desktop-restored");
  const restored = await restoreProject(bundlePath, restoredPath);
  assert.equal(restored.identity, sourceVerification.identity);
  assert.deepEqual(
    await restoreProject(bundlePath, restoredPath, {
      maximumProjectBytes: bundleByteLength,
    }),
    restored,
  );
  const restoredVerification = await verifyProject(restoredPath);
  assert.equal(restoredVerification.identity, sourceVerification.identity);

  const oversizedCollisionPath = join(
    temporaryRoot,
    "oversized-project-collision",
  );
  await cp(restoredPath, oversizedCollisionPath, { recursive: true });
  const oversizedProjectPath = join(oversizedCollisionPath, "project.json");
  const oversizedProject = JSON.parse(
    await readFile(oversizedProjectPath, "utf8"),
  );
  oversizedProject.resources[0].byte_length = bundleByteLength;
  await writeFile(oversizedProjectPath, canonicalBytes(oversizedProject));
  let oversizedCollisionError;
  try {
    await restoreProject(bundlePath, oversizedCollisionPath, {
      maximumProjectBytes: bundleByteLength,
    });
  } catch (error) {
    oversizedCollisionError = error;
  }
  assert.match(
    oversizedCollisionError?.cause?.message ?? "",
    new RegExp(`project exceeds ${bundleByteLength} bytes`),
  );

  for (const descriptor of manifest.files) {
    const relativeParts = descriptor.path.split("/");
    assert(
      (await readFile(join(fixture.root, ...relativeParts))).equals(
        await readFile(join(restoredPath, ...relativeParts)),
      ),
      `${descriptor.path}: round trip changed payload bytes`,
    );
  }
  assert.equal(
    await pathExists(join(restoredPath, "manifest.json")),
    false,
    "desktop project must not contain the runtime manifest",
  );
  assert.deepEqual(
    restoredVerification.project.extensions,
    project.extensions,
    "project extensions were not preserved",
  );
  const restoredTimeline = JSON.parse(
    await readFile(
      join(restoredPath, ...fixture.timelinePath.split("/")),
      "utf8",
    ),
  );
  assert.deepEqual(
    restoredTimeline.extensions,
    timeline.extensions,
    "resource extensions were not preserved",
  );

  const secondRestoredPath = join(temporaryRoot, "desktop-restored-again");
  await restoreProject(bundlePath, secondRestoredPath);
  for (const descriptor of manifest.files) {
    const relativeParts = descriptor.path.split("/");
    assert(
      (await readFile(join(restoredPath, ...relativeParts))).equals(
        await readFile(join(secondRestoredPath, ...relativeParts)),
      ),
      `${descriptor.path}: repeated restoration changed payload bytes`,
    );
  }

  const occupiedPath = join(temporaryRoot, "occupied");
  await mkdir(occupiedPath);
  await writeFile(join(occupiedPath, "sentinel.txt"), "preserve");
  await assert.rejects(
    restoreProject(bundlePath, occupiedPath),
    /output already exists/,
  );
  assert.equal(
    await readFile(join(occupiedPath, "sentinel.txt"), "utf8"),
    "preserve",
  );

  await assert.rejects(
    restoreProject(bundlePath, join(bundlePath, "nested-project")),
    /output project must be outside the runtime bundle/,
  );
  await verifyBundle(bundlePath);

  const corruptedBundlePath = join(temporaryRoot, "corrupted.runtime");
  await cp(bundlePath, corruptedBundlePath, { recursive: true });
  const corruptedTimelinePath = join(
    corruptedBundlePath,
    ...fixture.timelinePath.split("/"),
  );
  const corruptedBytes = await readFile(corruptedTimelinePath);
  corruptedBytes[0] ^= 1;
  await writeFile(corruptedTimelinePath, corruptedBytes);
  const corruptOutputPath = join(temporaryRoot, "corrupt-output");
  await assert.rejects(
    restoreProject(corruptedBundlePath, corruptOutputPath),
    /SHA-256 mismatch/,
  );
  assert.equal(await pathExists(corruptOutputPath), false);

  await assert.rejects(
    restoreProject(bundlePath, join(temporaryRoot, "bad-fault"), {
      faultAt: "typo",
    }),
    /unknown restoration fault boundary/,
  );

  const faultBoundaries = [
    "after-payload-copy",
    "after-project-verification",
    "before-publication",
  ];
  for (const faultAt of faultBoundaries) {
    const outputPath = join(temporaryRoot, `fault-${faultAt}`);
    await assert.rejects(
      restoreProject(bundlePath, outputPath, { faultAt }),
      new RegExp(`injected fault at ${faultAt}`),
    );
    assert.equal(await pathExists(outputPath), false);
    const residues = (await readdir(temporaryRoot)).filter((name) =>
      name.startsWith(`.${`fault-${faultAt}`}.restore-`),
    );
    assert.deepEqual(residues, []);
  }

  const uncertainOutputPath = join(temporaryRoot, "uncertain-output");
  let publicationError;
  try {
    await restoreProject(bundlePath, uncertainOutputPath, {
      faultAt: "after-publication-before-sync",
    });
  } catch (error) {
    publicationError = error;
  }
  assert.equal(
    publicationError?.code,
    "HYPER_PUBLISHED_DIRECTORY_DURABILITY_UNCERTAIN",
  );
  assert.equal(publicationError?.output_path, uncertainOutputPath);
  assert.equal(publicationError?.identity, sourceVerification.identity);
  assert.equal(publicationError?.published, true);
  assert.equal(publicationError?.durability, "uncertain");
  assert.equal(
    (await verifyProject(uncertainOutputPath)).identity,
    sourceVerification.identity,
  );
  assert.deepEqual(
    await restoreProject(bundlePath, uncertainOutputPath),
    {
      output_path: uncertainOutputPath,
      identity: sourceVerification.identity,
    },
  );

  const cliOutputPath = join(temporaryRoot, "desktop-restored-cli");
  const { stdout } = await execFileAsync(process.execPath, [
    join(scriptRoot, "restore-project.mjs"),
    bundlePath,
    cliOutputPath,
  ]);
  assert.equal(stdout.trim(), sourceVerification.identity);
  assert.equal(
    (await verifyProject(cliOutputPath)).identity,
    sourceVerification.identity,
  );

  console.log("Project round trip passed: 22 preservation and recovery cases");
} finally {
  await rm(temporaryRoot, { recursive: true, force: true });
}
