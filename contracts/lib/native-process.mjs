import { spawn } from "node:child_process";

const defaultMaximumErrorBytes = 64 * 1024;
const defaultMaximumOutputBytes = 64 * 1024;

export function runNativeProcess(
  executable,
  arguments_,
  {
    stdio = ["ignore", "ignore", "pipe"],
    maximumErrorBytes = defaultMaximumErrorBytes,
    maximumOutputBytes = defaultMaximumOutputBytes,
    input = undefined,
  } = {},
) {
  if (input !== undefined && stdio[0] !== "pipe") {
    throw new TypeError("native process input requires piped standard input");
  }
  return new Promise((resolvePromise, rejectPromise) => {
    const child = spawn(executable, arguments_, { stdio, windowsHide: true });
    const errorChunks = [];
    let errorBytes = 0;
    const outputChunks = [];
    let outputBytes = 0;
    let outputError;
    let settled = false;

    const rejectOnce = (error) => {
      if (settled) return;
      settled = true;
      rejectPromise(error);
    };
    child.on("error", rejectOnce);
    child.stdin?.on("error", (error) => {
      if (error.code !== "EPIPE") rejectOnce(error);
    });
    child.stdout?.on("data", (chunk) => {
      outputBytes += chunk.byteLength;
      if (outputBytes > maximumOutputBytes && !outputError) {
        outputError = new Error("native process output exceeded its limit");
        outputError.code = "ERR_CHILD_PROCESS_STDIO_MAXBUFFER";
        child.kill();
        return;
      }
      if (!outputError) outputChunks.push(chunk);
    });
    child.stderr.on("data", (chunk) => {
      errorBytes += chunk.byteLength;
      if (errorBytes > maximumErrorBytes && !outputError) {
        outputError = new Error("native process error output exceeded its limit");
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
