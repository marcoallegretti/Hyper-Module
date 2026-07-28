import { execFileSync } from "node:child_process";
import { fileURLToPath } from "node:url";
import { dirname, resolve } from "node:path";

import { findPrivateTrackedPaths } from "../lib/public-boundary.mjs";

const scriptDirectory = dirname(fileURLToPath(import.meta.url));
const repositoryRoot = resolve(scriptDirectory, "..", "..");

const output = execFileSync(
  "git",
  ["-c", "core.quotepath=false", "ls-files", "-z"],
  {
    cwd: repositoryRoot,
    encoding: "utf8",
    maxBuffer: 16 * 1024 * 1024,
    windowsHide: true,
  },
);

const trackedPaths = output.split("\0").filter(Boolean);
const violations = findPrivateTrackedPaths(trackedPaths);

if (violations.length > 0) {
  console.error("private paths are tracked:");
  for (const { path, reason } of violations) {
    console.error(`- ${path}: ${reason}`);
  }
  process.exitCode = 1;
} else {
  console.log(`Public repository boundary passed: ${trackedPaths.length} tracked paths`);
}

