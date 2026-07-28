import assert from "node:assert/strict";
import {
  mkdir,
  mkdtemp,
  readFile,
  rename,
  rm,
  symlink,
  writeFile,
} from "node:fs/promises";
import { tmpdir } from "node:os";
import { join } from "node:path";

import {
  syncDirectoryTree,
  syncPublishedDirectory,
  syncPublishedEntry,
  syncRemovedEntry,
} from "../lib/durable-files.mjs";

const root = await mkdtemp(join(tmpdir(), "hyper-durable-files-"));
try {
  const stage = join(root, "stage");
  const published = join(root, "published");
  await writeFile(stage, "durable");
  await syncPublishedEntry(stage);
  await rename(stage, published);
  await syncPublishedEntry(published);
  assert.equal(await readFile(published, "utf8"), "durable");

  const tree = join(root, "tree");
  await mkdir(join(tree, "nested"), { recursive: true });
  await writeFile(join(tree, "nested", "payload"), "payload");
  await syncDirectoryTree(tree, ["nested/payload"]);
  await syncPublishedDirectory(tree);

  await writeFile(join(tree, "undeclared"), "undeclared");
  await assert.rejects(
    syncDirectoryTree(tree, ["nested/payload"]),
    /filesystem tree exceeds 2 entries|undeclared file is forbidden/,
  );
  await rm(join(tree, "undeclared"));

  const outside = join(root, "outside");
  await mkdir(outside);
  await symlink(outside, join(tree, "link"), "junction");
  await assert.rejects(
    syncDirectoryTree(tree, ["nested/payload"]),
    /filesystem tree exceeds 2 entries|symbolic links and junctions are forbidden/,
  );
  await rm(join(tree, "link"));

  await rm(published);
  await syncRemovedEntry(root);
} finally {
  await rm(root, { recursive: true, force: true });
}

console.log("Durable filesystem primitives passed");
