import assert from "node:assert/strict";

import { runNativeProcess } from "../lib/native-process.mjs";

const started = Date.now();
await assert.rejects(
  runNativeProcess(
    process.execPath,
    ["-e", "setInterval(() => {}, 1000)"],
    { timeoutMs: 200, terminationGraceMs: 200 },
  ),
  { code: "ETIMEDOUT" },
);
assert(Date.now() - started < 5_000, "native process deadline was not bounded");

if (process.platform !== "win32") {
  const escalationStarted = Date.now();
  await assert.rejects(
    runNativeProcess(
      process.execPath,
      [
        "-e",
        "process.on('SIGTERM', () => {}); setInterval(() => {}, 1000)",
      ],
      { timeoutMs: 200, terminationGraceMs: 200 },
    ),
    { code: "ETIMEDOUT" },
  );
  assert(
    Date.now() - escalationStarted < 5_000,
    "native process escalation was not bounded",
  );
}

const controller = new AbortController();
const cancellation = runNativeProcess(
  process.execPath,
  ["-e", "setInterval(() => {}, 1000)"],
  { signal: controller.signal, timeoutMs: 5_000, terminationGraceMs: 200 },
);
controller.abort();
await assert.rejects(cancellation, { code: "ABORT_ERR" });

await assert.rejects(
  runNativeProcess(
    process.execPath,
    ["-e", "process.stdout.write('x'.repeat(1024))"],
    {
      stdio: ["ignore", "pipe", "pipe"],
      maximumOutputBytes: 16,
      timeoutMs: 5_000,
      terminationGraceMs: 200,
    },
  ),
  { code: "ERR_CHILD_PROCESS_STDIO_MAXBUFFER" },
);

console.log(
  "Native process lifecycle passed: timeout, escalation, cancellation, output limit",
);
