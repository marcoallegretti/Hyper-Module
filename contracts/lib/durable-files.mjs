import assert from "node:assert/strict";
import { constants, lstat, open, readdir } from "node:fs/promises";
import { dirname, join } from "node:path";

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

export async function syncDirectoryTree(rootPath) {
  const pending = [rootPath];
  const directories = [];
  while (pending.length > 0) {
    const directory = pending.pop();
    const status = await lstat(directory);
    assert(
      status.isDirectory() && !status.isSymbolicLink(),
      `durable tree contains a non-directory root: ${directory}`,
    );
    directories.push(directory);
    const entries = await readdir(directory, { withFileTypes: true });
    for (const entry of entries) {
      const path = join(directory, entry.name);
      assert(
        !entry.isSymbolicLink(),
        `durable tree contains a symbolic link: ${path}`,
      );
      if (entry.isDirectory()) {
        pending.push(path);
      } else {
        assert(entry.isFile(), `durable tree contains a special file: ${path}`);
        await syncFile(path);
      }
    }
  }

  for (const directory of directories.reverse()) {
    await syncDirectory(directory);
  }
}

export async function syncPublishedEntry(path) {
  const status = await lstat(path);
  assert(!status.isSymbolicLink(), `published path is a symbolic link: ${path}`);
  if (status.isDirectory()) {
    await syncDirectoryTree(path);
  } else {
    assert(status.isFile(), `published path is not a regular file: ${path}`);
    await syncFile(path);
  }
  await syncDirectory(dirname(path));
}

export async function syncRemovedEntry(parentPath) {
  await syncDirectory(parentPath);
}

