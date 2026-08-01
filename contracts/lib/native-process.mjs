import { spawn } from "node:child_process";

const defaultMaximumErrorBytes = 64 * 1024;
const defaultMaximumOutputBytes = 64 * 1024;
const defaultTimeoutMs = 5 * 60_000;
const defaultTerminationGraceMs = 1_000;

function validateDuration(value, label) {
  if (!Number.isSafeInteger(value) || value <= 0) {
    throw new TypeError(`${label} must be a positive safe integer`);
  }
}

function terminationError(message, code) {
  const error = new Error(message);
  error.code = code;
  return error;
}

export function runNativeProcess(
  executable,
  arguments_,
  {
    stdio = ["ignore", "ignore", "pipe"],
    maximumErrorBytes = defaultMaximumErrorBytes,
    maximumOutputBytes = defaultMaximumOutputBytes,
    input = undefined,
    signal = undefined,
    timeoutMs = defaultTimeoutMs,
    terminationGraceMs = defaultTerminationGraceMs,
  } = {},
) {
  if (input !== undefined && stdio[0] !== "pipe") {
    throw new TypeError("native process input requires piped standard input");
  }
  validateDuration(timeoutMs, "native process timeout");
  validateDuration(
    terminationGraceMs,
    "native process termination grace period",
  );
  if (signal !== undefined && !(signal instanceof AbortSignal)) {
    throw new TypeError("native process signal must be an AbortSignal");
  }
  if (signal?.aborted) {
    return Promise.reject(
      terminationError("native process was cancelled", "ABORT_ERR"),
    );
  }
  return new Promise((resolvePromise, rejectPromise) => {
    const child = spawn(executable, arguments_, { stdio, windowsHide: true });
    const errorChunks = [];
    let errorBytes = 0;
    const outputChunks = [];
    let outputBytes = 0;
    let outputError;
    let settled = false;
    let terminationTimer;

    const clearLifecycle = () => {
      clearTimeout(deadlineTimer);
      clearTimeout(terminationTimer);
      signal?.removeEventListener("abort", cancel);
    };

    const requestTermination = (error) => {
      if (!outputError) outputError = error;
      if (child.exitCode !== null || child.signalCode !== null) return;
      child.kill("SIGTERM");
      terminationTimer ??= setTimeout(() => {
        if (child.exitCode === null && child.signalCode === null) {
          child.kill("SIGKILL");
        }
      }, terminationGraceMs);
    };

    const cancel = () => {
      requestTermination(
        terminationError("native process was cancelled", "ABORT_ERR"),
      );
    };

    const deadlineTimer = setTimeout(() => {
      requestTermination(
        terminationError(
          `native process exceeded its ${timeoutMs} ms deadline`,
          "ETIMEDOUT",
        ),
      );
    }, timeoutMs);
    signal?.addEventListener("abort", cancel, { once: true });

    const rejectOnce = (error) => {
      if (settled) return;
      settled = true;
      clearLifecycle();
      rejectPromise(error);
    };
    child.on("error", (error) => {
      if (child.pid === undefined) {
        rejectOnce(error);
      } else {
        requestTermination(error);
      }
    });
    child.stdin?.on("error", (error) => {
      if (error.code !== "EPIPE") requestTermination(error);
    });
    child.stdout?.on("data", (chunk) => {
      outputBytes += chunk.byteLength;
      if (outputBytes > maximumOutputBytes && !outputError) {
        outputError = new Error("native process output exceeded its limit");
        outputError.code = "ERR_CHILD_PROCESS_STDIO_MAXBUFFER";
        requestTermination(outputError);
        return;
      }
      if (!outputError) outputChunks.push(chunk);
    });
    child.stderr.on("data", (chunk) => {
      errorBytes += chunk.byteLength;
      if (errorBytes > maximumErrorBytes && !outputError) {
        outputError = new Error("native process error output exceeded its limit");
        outputError.code = "ERR_CHILD_PROCESS_STDIO_MAXBUFFER";
        requestTermination(outputError);
        return;
      }
      if (!outputError) errorChunks.push(chunk);
    });
    child.on("close", (code, signal) => {
      if (settled) return;
      settled = true;
      clearLifecycle();
      if (outputError) {
        rejectPromise(outputError);
        return;
      }
      if (code === 0) {
        resolvePromise(Buffer.concat(outputChunks, outputBytes));
        return;
      }
      const detail = Buffer.concat(errorChunks, errorBytes).toString("utf8").trim();
      const error = new Error(
        detail ||
          `native process terminated ${
            signal ? `by signal ${signal}` : `with exit code ${code}`
          }`,
      );
      error.code = code;
      error.signal = signal;
      rejectPromise(error);
    });
    if (input !== undefined) child.stdin.end(input);
  });
}
