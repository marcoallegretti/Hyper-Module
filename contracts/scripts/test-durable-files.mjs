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
  readStableFile,
  syncDirectoryTree,
  syncPublishedDirectory,
  syncPublishedEntry,
  syncRemovedEntry,
} from "../lib/durable-files.mjs";
import { readBoundedDirectory } from "../lib/filesystem-tree.mjs";

const root = await mkdtemp(join(tmpdir(), "hyper-durable-files-"));
try {
  const stage = join(root, "stage");
  const published = join(root, "published");
  await writeFile(stage, "durable");
  await syncPublishedEntry(stage);
  await rename(stage, published);
  await syncPublishedEntry(published);
  assert.equal(await readFile(published, "utf8"), "durable");
  assert.equal((await readStableFile(published, 7)).toString("utf8"), "durable");
  await assert.rejects(readStableFile(published, 6), /exceeds 6 bytes/);

  const bounded = join(root, "bounded");
  await mkdir(bounded);
  await writeFile(join(bounded, "a"), "a");
  await writeFile(join(bounded, "b"), "b");
  await assert.rejects(
    readBoundedDirectory(bounded, {
      maximumEntries: 1,
      maximumNameBytes: 2,
      label: "test directory",
    }),
    /test directory exceeds 1 entries/,
  );

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
