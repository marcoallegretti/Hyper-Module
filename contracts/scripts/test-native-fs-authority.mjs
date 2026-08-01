import assert from "node:assert/strict";
import { mkdtemp, mkdir, open, readFile, rename, rm, writeFile } from "node:fs/promises";
import { tmpdir } from "node:os";
import { join } from "node:path";

import { openRetainedDirectory } from "../lib/durable-files.mjs";
import {
  removeRetainedDirectory,
  renameDirectoryBetweenNoReplace,
  renameDirectoryNoReplace,
} from "../lib/native-fs-authority.mjs";

const temporaryRoot = await mkdtemp(join(tmpdir(), "hyper-fs-authority-"));
try {
  const removable = join(temporaryRoot, "removable");
  await mkdir(join(removable, "nested"), { recursive: true });
  await writeFile(join(removable, "nested", "payload"), "owned");
  const removableHandle = await openRetainedDirectory(removable);
  try {
    await removeRetainedDirectory(
      removableHandle,
      removable,
      "test directory",
    );
  } finally {
    await removableHandle.close();
  }
  await assert.rejects(open(removable, "r"), { code: "ENOENT" });

  const retainedPath = join(temporaryRoot, "retained");
  const movedPath = join(temporaryRoot, "moved");
  await mkdir(retainedPath);
  const retainedHandle = await openRetainedDirectory(retainedPath);
  try {
    await rename(retainedPath, movedPath);
    await mkdir(retainedPath);
    await writeFile(join(retainedPath, "replacement"), "preserve");

    if (process.platform === "linux") {
      await removeRetainedDirectory(
        retainedHandle,
        retainedPath,
        "replaced test directory",
      );
      await assert.rejects(open(movedPath, "r"), { code: "ENOENT" });
    } else {
      await assert.rejects(
        removeRetainedDirectory(
          retainedHandle,
          retainedPath,
          "replaced test directory",
        ),
        /native cleanup failed/,
      );
      await removeRetainedDirectory(
        retainedHandle,
        movedPath,
        "moved test directory",
      );
    }
    assert.equal(
      await readFile(join(retainedPath, "replacement"), "utf8"),
      "preserve",
    );
  } finally {
    await retainedHandle.close();
  }
  await assert.rejects(open(movedPath, "r"), { code: "ENOENT" });

  const parentHandle = await openRetainedDirectory(temporaryRoot);
  try {
    await mkdir(join(temporaryRoot, "publish-source"));
    await renameDirectoryNoReplace(
      parentHandle,
      temporaryRoot,
      "publish-source",
      "published",
      "test publication",
    );
    await assert.rejects(open(join(temporaryRoot, "publish-source"), "r"), {
      code: "ENOENT",
    });

    await mkdir(join(temporaryRoot, "blocked-source"));
    await mkdir(join(temporaryRoot, "occupied"));
    await writeFile(join(temporaryRoot, "occupied", "marker"), "preserve");
    await assert.rejects(
      renameDirectoryNoReplace(
        parentHandle,
        temporaryRoot,
        "blocked-source",
        "occupied",
        "blocked test publication",
      ),
      { code: "EEXIST" },
    );
    assert.equal(
      await readFile(join(temporaryRoot, "occupied", "marker"), "utf8"),
      "preserve",
    );
    await open(join(temporaryRoot, "blocked-source"), "r").then((handle) =>
      handle.close(),
    );

    const sourceParent = join(temporaryRoot, "source-parent");
    const destinationParent = join(temporaryRoot, "destination-parent");
    await Promise.all([mkdir(sourceParent), mkdir(destinationParent)]);
    const [sourceParentHandle, destinationParentHandle] = await Promise.all([
      openRetainedDirectory(sourceParent),
      openRetainedDirectory(destinationParent),
    ]);
    try {
      await mkdir(join(sourceParent, "transfer"));
      await renameDirectoryBetweenNoReplace(
        sourceParentHandle,
        sourceParent,
        "transfer",
        destinationParentHandle,
        destinationParent,
        "transferred",
        "cross-parent test publication",
      );
      await assert.rejects(open(join(sourceParent, "transfer"), "r"), {
        code: "ENOENT",
      });
      await open(join(destinationParent, "transferred"), "r").then((handle) =>
        handle.close(),
      );

      await mkdir(join(sourceParent, "blocked-transfer"));
      await mkdir(join(destinationParent, "occupied-transfer"));
      await assert.rejects(
        renameDirectoryBetweenNoReplace(
          sourceParentHandle,
          sourceParent,
          "blocked-transfer",
          destinationParentHandle,
          destinationParent,
          "occupied-transfer",
          "blocked cross-parent test publication",
        ),
        { code: "EEXIST" },
      );
      await open(join(sourceParent, "blocked-transfer"), "r").then((handle) =>
        handle.close(),
      );
    } finally {
      await Promise.all([
        sourceParentHandle.close(),
        destinationParentHandle.close(),
      ]);
    }
  } finally {
    await parentHandle.close();
  }
  console.log("Native filesystem authority passed: 7 identity cases");
} finally {
  await rm(temporaryRoot, { recursive: true, force: true });
}
