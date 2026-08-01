import assert from "node:assert/strict";

import {
  finishOperationWithCleanup,
  runCleanupSteps,
  throwPreservingCleanupError,
} from "../lib/operation-cleanup.mjs";

const primary = new Error("primary failure");
const cleanup = new Error("cleanup failure");
let combined;
try {
  throwPreservingCleanupError(
    primary,
    cleanup,
    "combined failure",
  );
} catch (error) {
  combined = error;
}
assert(combined instanceof AggregateError);
assert.deepEqual(combined.errors, [primary, cleanup]);

assert.throws(
  () => throwPreservingCleanupError(undefined, cleanup, "cleanup only"),
  (error) => error === cleanup,
);

const cleanupOrder = [];
const firstCleanup = new Error("first cleanup failure");
const thirdCleanup = new Error("third cleanup failure");
let cleanupCombined;
try {
  await runCleanupSteps(
    [
      async () => {
        cleanupOrder.push(1);
        throw firstCleanup;
      },
      async () => {
        cleanupOrder.push(2);
      },
      async () => {
        cleanupOrder.push(3);
        throw thirdCleanup;
      },
    ],
    "cleanup failures",
  );
} catch (error) {
  cleanupCombined = error;
}
assert(cleanupCombined instanceof AggregateError);
assert.deepEqual(cleanupCombined.errors, [firstCleanup, thirdCleanup]);
assert.deepEqual(cleanupOrder, [1, 2, 3]);

await assert.rejects(
  finishOperationWithCleanup(
    undefined,
    [async () => {
      throw cleanup;
    }],
    "release failure",
    "combined release failure",
  ),
  (error) => error === cleanup,
);

let operationAndRelease;
try {
  await finishOperationWithCleanup(
    primary,
    [async () => {
      throw cleanup;
    }],
    "release failure",
    "combined release failure",
  );
} catch (error) {
  operationAndRelease = error;
}
assert(operationAndRelease instanceof AggregateError);
assert.deepEqual(operationAndRelease.errors, [primary, cleanup]);

console.log("Operation cleanup passed: 5 failure-preservation cases");
