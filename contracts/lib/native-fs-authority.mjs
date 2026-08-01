import assert from "node:assert/strict";
import { spawn } from "node:child_process";
import { existsSync } from "node:fs";
import { resolve } from "node:path";

const workspaceRoot = resolve(import.meta.dirname, "..", "..");
const maximumHelperOutputBytes = 64 * 1024;

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

function retainedAuthority(handle, ambientPath) {
  if (process.platform !== "linux") {
    return { path: ambientPath, descriptor: undefined };
  }
  assert(Number.isInteger(handle.fd), "retained directory handle is closed");
  return { path: "/proc/self/fd/3", descriptor: handle.fd };
}

function runHelper(arguments_, descriptor = undefined) {
  return new Promise((resolvePromise, rejectPromise) => {
    const stdio = descriptor === undefined
      ? ["ignore", "ignore", "pipe"]
      : ["ignore", "ignore", "pipe", descriptor];
    const child = spawn(helperPath(), arguments_, {
      stdio,
      windowsHide: true,
    });
    const errorChunks = [];
    let errorBytes = 0;
    let outputError;
    let settled = false;

    const rejectOnce = (error) => {
      if (settled) return;
      settled = true;
      rejectPromise(error);
    };
    child.on("error", rejectOnce);
    child.stderr.on("data", (chunk) => {
      errorBytes += chunk.byteLength;
      if (errorBytes > maximumHelperOutputBytes && !outputError) {
        outputError = new Error(
          "filesystem authority helper output exceeded its limit",
        );
        outputError.code = "ERR_CHILD_PROCESS_STDIO_MAXBUFFER";
        child.kill();
        return;
      }
      if (!outputError) errorChunks.push(chunk);
    });
    child.on("close", (code, signal) => {
      if (settled) return;
      settled = true;
      if (outputError) {
        rejectPromise(outputError);
        return;
      }
      if (code === 0) {
        resolvePromise();
        return;
      }
      const detail = Buffer.concat(errorChunks, errorBytes).toString("utf8").trim();
      const error = new Error(
        detail ||
          `filesystem authority helper terminated ${
            signal ? `by signal ${signal}` : `with exit code ${code}`
          }`,
      );
      error.code = code;
      error.signal = signal;
      rejectPromise(error);
    });
  });
}

export async function removeRetainedDirectory(handle, path, label) {
  const status = await handle.stat({ bigint: true });
  assert(status.isDirectory(), `${label} authority is not a directory`);
  const authority = retainedAuthority(handle, path);
  try {
    await runHelper(
      [
        "remove-directory",
        authority.path,
        String(status.dev),
        String(status.ino),
      ],
      authority.descriptor,
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
  const authority = retainedAuthority(parentHandle, parentPath);
  try {
    await runHelper(
      [
        "rename-directory-no-replace",
        authority.path,
        String(status.dev),
        String(status.ino),
        sourceName,
        destinationName,
      ],
      authority.descriptor,
    );
  } catch (error) {
    const wrapped = new Error(`${label} native publication failed`, {
      cause: error,
    });
    if (error.code === 2) wrapped.code = "EEXIST";
    throw wrapped;
  }
}
