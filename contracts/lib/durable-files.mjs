import assert from "node:assert/strict";
import { constants, lstat, open } from "node:fs/promises";
import { dirname, join } from "node:path";

import {
  collectRegularFiles,
  directoryPrefixes,
} from "./filesystem-tree.mjs";

async function syncFile(path) {
  const handle = await open(path, process.platform === "win32" ? "r+" : "r");
  try {
    await handle.sync();
  } finally {
    await handle.close();
  }
}

async function syncDirectory(path) {
  if (process.platform === "win32") {
    return;
  }
  const handle = await open(path, constants.O_RDONLY);
  try {
    await handle.sync();
  } finally {
    await handle.close();
  }
}

function declaredTreeOptions(declaredFiles) {
  assert(
    Array.isArray(declaredFiles) && declaredFiles.length > 0,
    "durable tree requires at least one declared file",
  );
  const files = new Set(declaredFiles);
  assert.equal(
    files.size,
    declaredFiles.length,
    "durable tree file declarations must be unique",
  );
  const directories = directoryPrefixes(files);
  return {
    files,
    directories,
    traversal: {
      maximumEntries: files.size + directories.size,
      maximumDirectories: directories.size + 1,
      maximumFiles: files.size,
      maximumDepth: Math.max(
        1,
        ...[...files].map((path) => path.split("/").length - 1),
      ),
      maximumPathBytes: [...files, ...directories].reduce(
        (total, path) => total + Buffer.byteLength(path, "utf8"),
        0,
      ),
      allowDirectory: (path) => directories.has(path),
      allowFile: (path) => files.has(path),
    },
  };
}

async function assertDeclaredTree(rootPath, expected, traversal) {
  const actual = (await collectRegularFiles(rootPath, traversal)).sort();
  assert.deepEqual(
    actual,
    [...expected].sort(),
    "durable tree does not exactly match its declarations",
  );
}

export async function syncDirectoryTree(rootPath, declaredFiles) {
  const { files, directories, traversal } = declaredTreeOptions(declaredFiles);
  await assertDeclaredTree(rootPath, files, traversal);

  for (const path of files) {
    await syncFile(join(rootPath, ...path.split("/")));
  }
  for (const path of [...directories].sort(
    (left, right) =>
      right.split("/").length - left.split("/").length ||
      right.localeCompare(left),
  )) {
    await syncDirectory(join(rootPath, ...path.split("/")));
  }
  await syncDirectory(rootPath);

  await assertDeclaredTree(rootPath, files, traversal);
}

export async function syncPublishedEntry(path) {
  const status = await lstat(path);
  assert(!status.isSymbolicLink(), `published path is a symbolic link: ${path}`);
  assert(status.isFile(), `published path is not a regular file: ${path}`);
  await syncFile(path);
  await syncDirectory(dirname(path));
}

export async function syncPublishedDirectory(path) {
  const status = await lstat(path);
  assert(
    status.isDirectory() && !status.isSymbolicLink(),
    `published path is not a real directory: ${path}`,
  );
  await syncDirectory(dirname(path));
}

export async function syncRemovedEntry(parentPath) {
  await syncDirectory(parentPath);
}
