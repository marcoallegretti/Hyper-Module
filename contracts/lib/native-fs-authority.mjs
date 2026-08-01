import assert from "node:assert/strict";
import { execFile } from "node:child_process";
import { existsSync } from "node:fs";
import { resolve } from "node:path";
import { promisify } from "node:util";

const execFileAsync = promisify(execFile);
const workspaceRoot = resolve(import.meta.dirname, "..", "..");

function helperPath() {
  if (process.env.HYPER_FS_AUTHORITY) {
    return resolve(process.env.HYPER_FS_AUTHORITY);
  }
  const executable = process.platform === "win32"
    ? "hyper-fs-authority.exe"
    : "hyper-fs-authority";
  const debugPath = resolve(workspaceRoot, "target", "debug", executable);
  return existsSync(debugPath)
    ? debugPath
    : resolve(workspaceRoot, "target", "release", executable);
}

export async function removeRetainedDirectory(handle, path, label) {
  const status = await handle.stat({ bigint: true });
  assert(status.isDirectory(), `${label} authority is not a directory`);
  try {
    await execFileAsync(
      helperPath(),
      ["remove-directory", path, String(status.dev), String(status.ino)],
      { maxBuffer: 64 * 1024, windowsHide: true },
    );
  } catch (error) {
    throw new Error(`${label} native cleanup failed`, { cause: error });
  }
}

export async function renameDirectoryNoReplace(
  parentHandle,
  parentPath,
  sourceName,
  destinationName,
  label,
) {
  const status = await parentHandle.stat({ bigint: true });
  assert(status.isDirectory(), `${label} parent authority is not a directory`);
  try {
    await execFileAsync(
      helperPath(),
      [
        "rename-directory-no-replace",
        parentPath,
        String(status.dev),
        String(status.ino),
        sourceName,
        destinationName,
      ],
      { maxBuffer: 64 * 1024, windowsHide: true },
    );
  } catch (error) {
    const wrapped = new Error(`${label} native publication failed`, {
      cause: error,
    });
    if (error.code === 2) wrapped.code = "EEXIST";
    throw wrapped;
  }
}
