import assert from "node:assert/strict";

export async function runCleanupSteps(steps, message) {
  assert(Array.isArray(steps), "cleanup steps must be an array");
  assert(
    steps.every((step) => typeof step === "function"),
    "cleanup steps must be functions",
  );
  const errors = [];
  for (const step of steps) {
    try {
      await step();
    } catch (error) {
      errors.push(error);
    }
  }
  if (errors.length > 1) throw new AggregateError(errors, message);
  if (errors.length === 1) throw errors[0];
}

export function throwPreservingCleanupError(
  operationError,
  cleanupError,
  message,
) {
  if (operationError) {
    throw new AggregateError([operationError, cleanupError], message);
  }
  throw cleanupError;
}

export async function finishOperationWithCleanup(
  operationError,
  cleanupSteps,
  cleanupMessage,
  combinedMessage,
) {
  try {
    await runCleanupSteps(cleanupSteps, cleanupMessage);
  } catch (cleanupError) {
    throwPreservingCleanupError(
      operationError,
      cleanupError,
      combinedMessage,
    );
  }
  if (operationError) throw operationError;
}
