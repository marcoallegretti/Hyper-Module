import assert from "node:assert/strict";
import { randomUUID } from "node:crypto";
import {
  cp,
  mkdtemp,
  mkdir,
  readFile,
  rm,
  symlink,
  unlink,
  writeFile,
} from "node:fs/promises";
import { join } from "node:path";
import { tmpdir } from "node:os";

import canonicalize from "canonicalize";

import { buildProjectBundle } from "./build-project-bundle.mjs";
import { createMinimalProject } from "./test-project-fixture.mjs";
import { collectRegularFiles, verifyBundle } from "./verify-bundle.mjs";

async function createValidBundle(parent, name) {
  const fixture = await createMinimalProject(parent, `${name}-source`);
  const root = join(parent, name);
  await buildProjectBundle(fixture.root, root, "1.0.0");
  return root;
}

const temporaryRoot = await mkdtemp(
  join(tmpdir(), `hyper-bundle-verifier-${randomUUID()}-`),
);
try {
  const valid = await createValidBundle(temporaryRoot, "valid");
  await verifyBundle(valid);

  const noncanonical = join(temporaryRoot, "noncanonical");
  await cp(valid, noncanonical, { recursive: true });
  const manifest = JSON.parse(
    await readFile(join(noncanonical, "manifest.json"), "utf8"),
  );
  await writeFile(
    join(noncanonical, "manifest.json"),
    JSON.stringify(manifest, null, 2),
  );
  await assert.rejects(
    verifyBundle(noncanonical),
    /not canonical RFC 8785 JSON/,
  );

  const modified = join(temporaryRoot, "modified");
  await cp(valid, modified, { recursive: true });
  const originalTimeline = await readFile(
    join(modified, "timeline", "timeline.json"),
  );
  await writeFile(
    join(modified, "timeline", "timeline.json"),
    Buffer.alloc(originalTimeline.length, 0x78),
  );
  await assert.rejects(verifyBundle(modified), /SHA-256 mismatch/);

  const wrongSourceIdentity = join(temporaryRoot, "wrong-source-identity");
  await cp(valid, wrongSourceIdentity, { recursive: true });
  const wrongSourceManifestPath = join(wrongSourceIdentity, "manifest.json");
  const wrongSourceManifest = JSON.parse(
    await readFile(wrongSourceManifestPath, "utf8"),
  );
  wrongSourceManifest.source_project_sha256 =
    "1111111111111111111111111111111111111111111111111111111111111111";
  await writeFile(wrongSourceManifestPath, canonicalize(wrongSourceManifest));
  await assert.rejects(
    verifyBundle(wrongSourceIdentity),
    /runtime entrypoint must preserve the canonical source project manifest/,
  );

  const wrongResourceRole = join(temporaryRoot, "wrong-resource-role");
  await cp(valid, wrongResourceRole, { recursive: true });
  const wrongRoleManifestPath = join(wrongResourceRole, "manifest.json");
  const wrongRoleManifest = JSON.parse(
    await readFile(wrongRoleManifestPath, "utf8"),
  );
  wrongRoleManifest.files.find(
    (file) => file.path === "timeline/timeline.json",
  ).role = "runtime-project";
  await writeFile(wrongRoleManifestPath, canonicalize(wrongRoleManifest));
  await assert.rejects(
    verifyBundle(wrongResourceRole),
    /project resource must have the runtime-data role/,
  );

  const extra = join(temporaryRoot, "extra");
  await cp(valid, extra, { recursive: true });
  await writeFile(join(extra, "undeclared.bin"), "undeclared");
  await assert.rejects(verifyBundle(extra), /undeclared file is forbidden/);

  const boundedTree = join(temporaryRoot, "bounded-tree");
  await mkdir(join(boundedTree, "a", "b", "c"), { recursive: true });
  await writeFile(join(boundedTree, "one.bin"), "one");
  await writeFile(join(boundedTree, "two.bin"), "two");
  await assert.rejects(
    collectRegularFiles(boundedTree, { maximumDepth: 2 }),
    /filesystem tree exceeds depth 2/,
  );
  await assert.rejects(
    collectRegularFiles(boundedTree, { maximumEntries: 2 }),
    /filesystem tree exceeds 2 entries/,
  );
  await assert.rejects(
    collectRegularFiles(boundedTree, { maximumPathBytes: 4 }),
    /filesystem tree paths exceed 4 bytes/,
  );

  const missing = join(temporaryRoot, "missing");
  await cp(valid, missing, { recursive: true });
  await unlink(join(missing, "timeline", "timeline.json"));
  await assert.rejects(verifyBundle(missing), /do not exactly match manifest/);

  const linked = join(temporaryRoot, "linked");
  await cp(valid, linked, { recursive: true });
  await mkdir(join(temporaryRoot, "outside"));
  await symlink(
    join(temporaryRoot, "outside"),
    join(linked, "unsafe-link"),
    "junction",
  );
  await assert.rejects(
    verifyBundle(linked),
    /symbolic links and junctions are forbidden/,
  );

  const invalidUtf8 = join(temporaryRoot, "invalid-utf8");
  await cp(valid, invalidUtf8, { recursive: true });
  await writeFile(
    join(invalidUtf8, "manifest.json"),
    Buffer.from([0x7b, 0x22, 0xff, 0x22, 0x7d]),
  );
  await assert.rejects(verifyBundle(invalidUtf8), /not valid UTF-8/);

  const bom = join(temporaryRoot, "bom");
  await cp(valid, bom, { recursive: true });
  const validManifest = await readFile(join(bom, "manifest.json"));
  await writeFile(
    join(bom, "manifest.json"),
    Buffer.concat([Buffer.from([0xef, 0xbb, 0xbf]), validManifest]),
  );
  await assert.rejects(verifyBundle(bom), /byte-order mark/);

  const oversized = join(temporaryRoot, "oversized");
  await cp(valid, oversized, { recursive: true });
  await writeFile(
    join(oversized, "manifest.json"),
    Buffer.alloc(1024 * 1024 + 1, 0x20),
  );
  await assert.rejects(verifyBundle(oversized), /exceeds 1048576 bytes/);

  console.log("Bundle verifier passed: 11 filesystem cases");
} finally {
  await rm(temporaryRoot, { recursive: true, force: true });
}
