import assert from "node:assert/strict";
import { readFile } from "node:fs/promises";
import { dirname, resolve } from "node:path";
import { fileURLToPath } from "node:url";

import {
  assertTimingSimulatorProfile,
  TimingSimulator,
} from "../lib/timing-simulator.mjs";
import { strictParse } from "../lib/strict-json.mjs";

const root = resolve(dirname(fileURLToPath(import.meta.url)), "..");
const profile = strictParse(
  await readFile(
    resolve(root, "fixtures/valid/timing-simulator-profile.json"),
    "utf8",
  ),
);
const firstSessionId = "018f47a2-6b2d-7c11-8a44-001122334490";
const secondSessionId = "018f47a2-6b2d-7c11-8a44-001122334491";

function frame(kind, sessionId, sessionEpoch, sequence, extra = {}) {
  return {
    format: "hyper-timing-frame",
    format_version: 1,
    protocol_version: 1,
    kind,
    session_id: sessionId,
    session_epoch: sessionEpoch,
    sequence,
    ...extra,
  };
}

function open(simulator, epoch = 1, sessionId = firstSessionId) {
  return simulator.receive(frame("session-open", sessionId, epoch, 1));
}

function schedule(
  simulator,
  sequence,
  coverageUntilUs,
  events,
  epoch = 1,
  sessionId = firstSessionId,
) {
  return simulator.receive(
    frame("schedule", sessionId, epoch, sequence, {
      coverage_until_us: coverageUntilUs,
      events,
    }),
  );
}

assertTimingSimulatorProfile(profile);
assert.throws(
  () =>
    assertTimingSimulatorProfile({
      ...profile,
      max_batch_events: profile.output_queue_capacity + 1,
    }),
  /batch size must not exceed output queue capacity/,
);

const initial = new TimingSimulator(profile);
assert.deepEqual(initial.status().outputs, {
  cv: [0, 0],
  gate: [false, false],
  clock: false,
});
assert.equal(initial.status().connected, false);
const detachedStatus = initial.status();
detachedStatus.outputs.gate[0] = true;
assert.equal(initial.status().outputs.gate[0], false);
assert.throws(
  () =>
    initial.receive({
      ...frame("session-open", firstSessionId, 1, 1),
      protocol_version: 2,
    }),
  /protocol version is unsupported/,
);

const nearTimestampLimit = new TimingSimulator(profile);
nearTimestampLimit.advanceTo(
  Number.MAX_SAFE_INTEGER - profile.heartbeat_timeout_us + 1,
);
assert.throws(
  () => open(nearTimestampLimit),
  /heartbeat deadline exceeds the safe timestamp range/,
);
assert.equal(nearTimestampLimit.status().connected, false);

const ordered = new TimingSimulator(profile);
open(ordered);
schedule(ordered, 2, 100_000, [
  {
    kind: "clock-output",
    level: true,
    timestamp_us: 10,
  },
  {
    kind: "gate-output",
    output_index: 0,
    level: true,
    timestamp_us: 20,
  },
  {
    kind: "cv-output",
    output_index: 0,
    value: 500_000,
    timestamp_us: 30,
  },
]);
assert.equal(ordered.status().queued_outputs, 3);
assert.deepEqual(
  ordered.advanceTo(30).map((event) => event.kind),
  ["clock-output", "gate-output", "cv-output"],
);
assert.deepEqual(ordered.status().outputs, {
  cv: [500_000, 0],
  gate: [true, false],
  clock: true,
});
assert.throws(() => ordered.advanceTo(29), /virtual clock target/);
assert.throws(
  () => ordered.receive(frame("heartbeat", firstSessionId, 1, 4)),
  /sequence is not contiguous/,
);
ordered.receive(frame("heartbeat", firstSessionId, 1, 3));
assert.equal(ordered.status().session.last_sequence, 3);

const capability = new TimingSimulator(profile);
open(capability);
assert.throws(
  () =>
    schedule(capability, 2, 1000, [
      {
        kind: "gate-output",
        output_index: 2,
        level: true,
        timestamp_us: 10,
      },
    ]),
  /Gate output is unavailable/,
);
assert.equal(capability.status().queued_outputs, 0);
schedule(capability, 2, 1000, []);
assert.equal(capability.status().session.last_sequence, 2);

const batchProfile = {
  ...profile,
  max_batch_events: 2,
  output_queue_capacity: 4,
};
const boundedBatch = new TimingSimulator(batchProfile);
open(boundedBatch);
assert.throws(
  () =>
    schedule(boundedBatch, 2, 1000, [
      { kind: "clock-output", level: true, timestamp_us: 10 },
      { kind: "clock-output", level: false, timestamp_us: 20 },
      { kind: "clock-output", level: true, timestamp_us: 30 },
    ]),
  /configured batch limit/,
);
assert.equal(boundedBatch.status().queued_outputs, 0);

const queueProfile = {
  ...profile,
  max_batch_events: 4,
  output_queue_capacity: 4,
};
const boundedQueue = new TimingSimulator(queueProfile);
open(boundedQueue);
schedule(
  boundedQueue,
  2,
  1000,
  [10, 20, 30, 40].map((timestamp_us, index) => ({
    kind: "gate-output",
    output_index: index % 2,
    level: true,
    timestamp_us,
  })),
);
assert.throws(
  () =>
    schedule(boundedQueue, 3, 2000, [
      {
        kind: "clock-output",
        level: true,
        timestamp_us: 50,
      },
    ]),
  /output queue capacity exceeded/,
);
assert.equal(boundedQueue.status().queued_outputs, 4);
assert.equal(boundedQueue.status().coverage_until_us, 1000);
boundedQueue.receive(frame("heartbeat", firstSessionId, 1, 3));

const horizon = new TimingSimulator(profile);
open(horizon);
assert.throws(
  () => schedule(horizon, 2, 1_000_001, []),
  /coverage exceeds configured horizon/,
);
assert.throws(
  () =>
    schedule(horizon, 2, 1000, [
      {
        kind: "clock-output",
        level: true,
        timestamp_us: 1001,
      },
    ]),
  /event exceeds declared schedule coverage/,
);
schedule(horizon, 2, 1000, []);
assert.throws(() => schedule(horizon, 3, 1000, []), /coverage must increase/);
const unsorted = new TimingSimulator(profile);
open(unsorted);
assert.throws(
  () =>
    schedule(unsorted, 2, 1000, [
      { kind: "clock-output", level: true, timestamp_us: 20 },
      { kind: "clock-output", level: false, timestamp_us: 10 },
    ]),
  /schedule events must be timestamp ordered/,
);

const heartbeat = new TimingSimulator(profile);
open(heartbeat);
heartbeat.advanceTo(100_000);
heartbeat.receive(frame("heartbeat", firstSessionId, 1, 2));
schedule(heartbeat, 3, 300_000, []);
heartbeat.advanceTo(250_000);
assert.equal(heartbeat.status().connected, true);
assert.equal(heartbeat.status().telemetry.watchdog_trips, 0);

const watchdog = new TimingSimulator(profile);
open(watchdog);
schedule(watchdog, 2, 500_000, [
  {
    kind: "gate-output",
    output_index: 0,
    level: true,
    timestamp_us: 249_999,
  },
  {
    kind: "gate-output",
    output_index: 1,
    level: true,
    timestamp_us: 250_000,
  },
]);
assert.deepEqual(
  watchdog.advanceTo(250_000).map((event) => event.timestamp_us),
  [249_999],
);
assert.equal(watchdog.status().connected, false);
assert.deepEqual(watchdog.status().outputs.gate, [false, false]);
assert.equal(watchdog.status().last_fault.kind, "watchdog-timeout");
assert.equal(watchdog.status().telemetry.watchdog_trips, 1);

const underrun = new TimingSimulator(profile);
open(underrun);
schedule(underrun, 2, 100, [
  {
    kind: "clock-output",
    level: true,
    timestamp_us: 100,
  },
]);
assert.deepEqual(
  underrun.advanceTo(101).map((event) => event.timestamp_us),
  [100],
);
assert.equal(underrun.status().connected, false);
assert.equal(underrun.status().outputs.clock, false);
assert.equal(underrun.status().last_fault.kind, "queue-underrun");
assert.equal(underrun.status().telemetry.queue_underruns, 1);

const resync = new TimingSimulator(profile);
open(resync);
schedule(resync, 2, 1000, [
  {
    kind: "gate-output",
    output_index: 0,
    level: true,
    timestamp_us: 100,
  },
]);
open(resync, 2, secondSessionId);
assert.equal(resync.status().queued_outputs, 0);
assert.deepEqual(resync.status().outputs.gate, [false, false]);
assert.equal(resync.status().telemetry.session_resyncs, 1);
assert.throws(
  () => open(resync, 1, firstSessionId),
  /session epoch must increase/,
);
assert.throws(
  () => resync.receive(frame("heartbeat", firstSessionId, 1, 2)),
  /another session|stale session epoch/,
);

const manual = new TimingSimulator(profile);
open(manual);
schedule(manual, 2, 1000, [
  {
    kind: "clock-output",
    level: true,
    timestamp_us: 0,
  },
]);
manual.advanceTo(0);
assert.equal(manual.status().outputs.clock, true);
manual.disconnect();
assert.equal(manual.status().connected, false);
assert.equal(manual.status().outputs.clock, false);
assert.equal(manual.status().telemetry.manual_disconnects, 1);
manual.disconnect();
assert.equal(manual.status().telemetry.manual_disconnects, 1);

const inputs = new TimingSimulator({
  ...profile,
  input_queue_capacity: 2,
});
assert.equal(
  inputs.injectInput({
    kind: "cv-input",
    input_index: 0,
    value: -250_000,
  }).accepted,
  true,
);
inputs.advanceTo(10);
assert.equal(
  inputs.injectInput({ kind: "clock-input", edge: "rising" }).accepted,
  true,
);
assert.deepEqual(inputs.injectInput({ kind: "reset-input", edge: "falling" }), {
  accepted: false,
  reason: "queue-overflow",
});
const firstInputs = inputs.readInputs(1);
assert.equal(firstInputs[0].timestamp_us, 0);
assert.equal(firstInputs[0].sequence, 1);
const acceptedAfterGap = inputs.injectInput({
  kind: "reset-input",
  edge: "rising",
});
assert.equal(acceptedAfterGap.event.sequence, 4);
assert.equal(acceptedAfterGap.event.source_sequence, 2);
assert.deepEqual(
  inputs.readInputs(2).map((event) => event.sequence),
  [2, 4],
);
assert.equal(inputs.status().telemetry.input_overflows, 1);
assert.throws(() => inputs.readInputs(0), /input read limit/);
assert.throws(
  () =>
    inputs.injectInput({
      kind: "cv-input",
      input_index: 2,
      value: 0,
    }),
  /CV input is unavailable/,
);

const droppedInput = new TimingSimulator(profile);
droppedInput.dropNextCapturedInput();
assert.deepEqual(
  droppedInput.injectInput({ kind: "clock-input", edge: "rising" }),
  { accepted: false, reason: "injected-drop" },
);
const inputAfterDrop = droppedInput.injectInput({
  kind: "clock-input",
  edge: "falling",
});
assert.equal(inputAfterDrop.event.sequence, 2);
assert.equal(inputAfterDrop.event.source_sequence, 2);
assert.equal(droppedInput.status().telemetry.injected_input_drops, 1);
assert.throws(() => {
  droppedInput.dropNextCapturedInput();
  droppedInput.dropNextCapturedInput();
}, /already armed/);

const delayed = new TimingSimulator(profile);
open(delayed);
delayed.delayNextOutput(50);
assert.throws(() => delayed.delayNextOutput(10), /already armed/);
schedule(delayed, 2, 1000, [
  {
    kind: "gate-output",
    output_index: 0,
    level: true,
    timestamp_us: 100,
  },
  {
    kind: "gate-output",
    output_index: 0,
    level: false,
    timestamp_us: 120,
  },
]);
const delayedEvents = delayed.advanceTo(150);
assert.deepEqual(
  delayedEvents.map((event) => [
    event.timestamp_us,
    event.emitted_timestamp_us,
  ]),
  [
    [120, 120],
    [100, 150],
  ],
);
assert.equal(delayed.status().outputs.gate[0], true);

const delayedPastCoverage = new TimingSimulator(profile);
open(delayedPastCoverage);
delayedPastCoverage.delayNextOutput(200);
schedule(delayedPastCoverage, 2, 1000, [
  {
    kind: "gate-output",
    output_index: 0,
    level: true,
    timestamp_us: 900,
  },
]);
assert.deepEqual(delayedPastCoverage.advanceTo(1001), []);
assert.equal(delayedPastCoverage.status().last_fault.kind, "queue-underrun");
assert.equal(delayedPastCoverage.status().outputs.gate[0], false);

const malformed = new TimingSimulator(profile);
open(malformed);
assert.throws(
  () =>
    malformed.receive({
      ...frame("heartbeat", firstSessionId, 1, 2),
      unexpected: true,
    }),
  /unknown or missing properties/,
);
assert.equal(malformed.status().telemetry.rejected_frames, 1);

console.log("Timing simulator passed: 51 deterministic protocol cases");
