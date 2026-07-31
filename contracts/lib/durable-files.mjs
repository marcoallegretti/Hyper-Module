import assert from "node:assert/strict";
import { constants, lstat, open } from "node:fs/promises";
import { dirname, join } from "node:path";

import {
  collectRegularFiles,
  directoryPrefixes,
} from "./filesystem-tree.mjs";

async function syncFile(path) {
  const handle = await openRetainedFile(path);
  try {
    await handle.sync();
  } finally {
    await handle.close();
  }
}

function sameIdentity(left, right) {
  return (
    left.dev === right.dev &&
    left.ino === right.ino &&
    left.mode === right.mode &&
    left.size === right.size
  );
}

function sameStableIdentity(left, right) {
  return (
    sameIdentity(left, right) &&
    left.mtimeNs === right.mtimeNs &&
    left.ctimeNs === right.ctimeNs
  );
}

function assertSameIdentity(expected, actual, path) {
  assert(
    sameIdentity(expected, actual),
    `filesystem object changed while being retained: ${path}`,
  );
}

async function openRetainedFile(path) {
  const before = await lstat(path, { bigint: true });
  assert(!before.isSymbolicLink(), `file path is a symbolic link: ${path}`);
  assert(before.isFile(), `file path is not a regular file: ${path}`);
  const flags =
    (process.platform === "win32" ? constants.O_RDWR : constants.O_RDONLY) |
    (constants.O_NOFOLLOW ?? 0);
  const handle = await open(path, flags);
  try {
    const opened = await handle.stat({ bigint: true });
    const after = await lstat(path, { bigint: true });
    assertSameIdentity(before, opened, path);
    assertSameIdentity(opened, after, path);
    return handle;
  } catch (error) {
    await handle.close();
    throw error;
  }
}

async function readOpenFile(handle, length, path) {
  const output = Buffer.alloc(length);
  let offset = 0;
  while (offset < output.byteLength) {
    const { bytesRead } = await handle.read(
      output,
      offset,
      output.byteLength - offset,
      offset,
    );
    assert(bytesRead > 0, `file became shorter while being read: ${path}`);
    offset += bytesRead;
  }
  const overflow = Buffer.alloc(1);
  const { bytesRead } = await handle.read(overflow, 0, 1, output.byteLength);
  assert.equal(bytesRead, 0, `file became longer while being read: ${path}`);
  return output;
}

export async function readStableFile(path, maximumBytes, label = "file") {
  assert(
    Number.isSafeInteger(maximumBytes) && maximumBytes >= 0,
    "stable file byte limit must be a non-negative safe integer",
  );
  const handle = await openRetainedFile(path);
  try {
    const before = await handle.stat({ bigint: true });
    assert(
      before.size <= BigInt(maximumBytes),
      `${label} exceeds ${maximumBytes} bytes`,
    );
    const length = Number(before.size);
    const bytes = await readOpenFile(handle, length, path);
    const confirmed = await readOpenFile(handle, length, path);
    assert(bytes.equals(confirmed), `file changed while being read: ${path}`);
    const after = await handle.stat({ bigint: true });
    const published = await lstat(path, { bigint: true });
    assert(
      sameStableIdentity(before, after),
      `file metadata changed while being read: ${path}`,
    );
    assertSameIdentity(after, published, path);
    return bytes;
  } finally {
    await handle.close();
  }
}

export async function openRetainedDirectory(path) {
  const before = await lstat(path, { bigint: true });
  assert(
    before.isDirectory() && !before.isSymbolicLink(),
    `directory path is not a real directory: ${path}`,
  );
  const flags =
    process.platform === "win32"
      ? constants.O_RDWR
      : constants.O_RDONLY |
        (constants.O_DIRECTORY ?? 0) |
        (constants.O_NOFOLLOW ?? 0);
  const handle = await open(path, flags);
  try {
    const opened = await handle.stat({ bigint: true });
    const after = await lstat(path, { bigint: true });
    assertSameIdentity(before, opened, path);
    assertSameIdentity(opened, after, path);
    return handle;
  } catch (error) {
    await handle.close();
    throw error;
  }
}

export async function assertRetainedDirectory(handle, path, label = "directory") {
  const [opened, published] = await Promise.all([
    handle.stat({ bigint: true }),
    lstat(path, { bigint: true }),
  ]);
  assert(
    opened.isDirectory() && published.isDirectory() && !published.isSymbolicLink(),
    `${label} must remain a real directory: ${path}`,
  );
  assert(
    opened.dev === published.dev &&
      opened.ino === published.ino &&
      opened.mode === published.mode,
    `${label} filesystem object changed: ${path}`,
  );
}

async function syncDirectory(path) {
  const handle = await openRetainedDirectory(path);
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
  const retainedFiles = [];
  const retainedDirectories = [];
  try {
    for (const path of files) {
      retainedFiles.push({
        path,
        handle: await openRetainedFile(join(rootPath, ...path.split("/"))),
      });
    }
    for (const path of [...directories].sort(
      (left, right) =>
        right.split("/").length - left.split("/").length ||
        right.localeCompare(left),
    )) {
      retainedDirectories.push({
        path,
        handle: await openRetainedDirectory(
          join(rootPath, ...path.split("/")),
        ),
      });
    }
    retainedDirectories.push({
      path: "",
      handle: await openRetainedDirectory(rootPath),
    });

    for (const { handle } of retainedFiles) {
      await handle.sync();
    }
    for (const { handle } of retainedDirectories) {
      await handle.sync();
    }

    await assertDeclaredTree(rootPath, files, traversal);
    for (const { path, handle } of retainedFiles) {
      assertSameIdentity(
        await handle.stat({ bigint: true }),
        await lstat(join(rootPath, ...path.split("/")), { bigint: true }),
        path,
      );
    }
    for (const { path, handle } of retainedDirectories) {
      const absolute = path
        ? join(rootPath, ...path.split("/"))
        : rootPath;
      assertSameIdentity(
        await handle.stat({ bigint: true }),
        await lstat(absolute, { bigint: true }),
        path || ".",
      );
    }
  } finally {
    await Promise.allSettled([
      ...retainedFiles.map(({ handle }) => handle.close()),
      ...retainedDirectories.map(({ handle }) => handle.close()),
    ]);
  }
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
