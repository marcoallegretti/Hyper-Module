import assert from "node:assert/strict";

const entityIdPattern =
  /^[0-9a-f]{8}-[0-9a-f]{4}-[0-9a-f]{4}-[0-9a-f]{4}-[0-9a-f]{12}$/;
const maximumSafeInteger = Number.MAX_SAFE_INTEGER;

function assertPlainObject(value, label) {
  assert(
    value !== null && typeof value === "object" && !Array.isArray(value),
    `${label} must be an object`,
  );
}

function assertExactKeys(value, expected, label) {
  assertPlainObject(value, label);
  assert.deepEqual(
    Object.keys(value).sort(),
    [...expected].sort(),
    `${label} has unknown or missing properties`,
  );
}

function assertIntegerRange(value, minimum, maximum, label) {
  assert(
    Number.isSafeInteger(value) && value >= minimum && value <= maximum,
    `${label} is outside its supported integer range`,
  );
}

function assertEntityId(value, label) {
  assert(
    typeof value === "string" && entityIdPattern.test(value),
    `${label} must be a lowercase UUID`,
  );
}

function safeAdd(value, delta, label) {
  assert(
    value <= maximumSafeInteger - delta,
    `${label} exceeds the safe timestamp range`,
  );
  return value + delta;
}

function assertCapabilities(capabilities) {
  assertExactKeys(
    capabilities,
    [
      "format",
      "format_version",
      "audio_inputs",
      "audio_outputs",
      "cv_inputs",
      "cv_outputs",
      "gate_outputs",
      "has_clock_input",
      "has_reset_input",
      "has_clock_output",
      "usb_mode",
      "has_ethernet",
      "extensions",
    ],
    "timing simulator capabilities",
  );
  assert.equal(
    capabilities.format,
    "hyper-hardware-capabilities",
    "capability format is unsupported",
  );
  assert.equal(
    capabilities.format_version,
    1,
    "capability format version is unsupported",
  );
  assertIntegerRange(capabilities.audio_inputs, 0, 2, "audio input count");
  assertIntegerRange(capabilities.audio_outputs, 0, 2, "audio output count");
  assertIntegerRange(capabilities.cv_inputs, 0, 4, "CV input count");
  assertIntegerRange(capabilities.cv_outputs, 0, 4, "CV output count");
  assertIntegerRange(capabilities.gate_outputs, 0, 4, "Gate output count");
  for (const property of [
    "has_clock_input",
    "has_reset_input",
    "has_clock_output",
    "has_ethernet",
  ]) {
    assert.equal(
      typeof capabilities[property],
      "boolean",
      `${property} must be Boolean`,
    );
  }
  assert(
    ["none", "host", "service", "host-and-service"].includes(
      capabilities.usb_mode,
    ),
    "USB mode is unsupported",
  );
  assertPlainObject(capabilities.extensions, "capability extensions");
}

export function assertTimingSimulatorProfile(profile) {
  assertExactKeys(
    profile,
    [
      "format",
      "format_version",
      "protocol_version",
      "capabilities",
      "max_batch_events",
      "output_queue_capacity",
      "input_queue_capacity",
      "max_schedule_horizon_us",
      "heartbeat_timeout_us",
      "extensions",
    ],
    "timing simulator profile",
  );
  assert.equal(
    profile.format,
    "hyper-timing-simulator-profile",
    "timing simulator profile format is unsupported",
  );
  assert.equal(
    profile.format_version,
    1,
    "timing simulator profile version is unsupported",
  );
  assert.equal(
    profile.protocol_version,
    1,
    "timing protocol version is unsupported",
  );
  assertCapabilities(profile.capabilities);
  assertIntegerRange(profile.max_batch_events, 1, 256, "maximum batch size");
  assertIntegerRange(
    profile.output_queue_capacity,
    1,
    4096,
    "output queue capacity",
  );
  assertIntegerRange(
    profile.input_queue_capacity,
    1,
    4096,
    "input queue capacity",
  );
  assert(
    profile.max_batch_events <= profile.output_queue_capacity,
    "maximum batch size must not exceed output queue capacity",
  );
  assertIntegerRange(
    profile.max_schedule_horizon_us,
    1,
    60_000_000,
    "maximum schedule horizon",
  );
  assertIntegerRange(
    profile.heartbeat_timeout_us,
    1,
    10_000_000,
    "heartbeat timeout",
  );
  assertPlainObject(profile.extensions, "timing simulator extensions");
}

function assertFrameBase(frame) {
  assert.equal(
    frame.format,
    "hyper-timing-frame",
    "timing frame format failed",
  );
  assert.equal(frame.format_version, 1, "timing frame version is unsupported");
  assert.equal(
    frame.protocol_version,
    1,
    "timing protocol version is unsupported",
  );
  assertEntityId(frame.session_id, "timing session identifier");
  assertIntegerRange(
    frame.session_epoch,
    1,
    maximumSafeInteger,
    "timing session epoch",
  );
  assertIntegerRange(
    frame.sequence,
    1,
    maximumSafeInteger,
    "timing frame sequence",
  );
}

function assertOutputEvent(event) {
  assertPlainObject(event, "timing output event");
  if (event.kind === "cv-output") {
    assertExactKeys(
      event,
      ["kind", "output_index", "value", "timestamp_us"],
      "CV output event",
    );
    assertIntegerRange(event.output_index, 0, 3, "CV output index");
    assertIntegerRange(event.value, -1_000_000, 1_000_000, "CV output value");
  } else if (event.kind === "gate-output") {
    assertExactKeys(
      event,
      ["kind", "output_index", "level", "timestamp_us"],
      "Gate output event",
    );
    assertIntegerRange(event.output_index, 0, 3, "Gate output index");
    assert.equal(typeof event.level, "boolean", "Gate level must be Boolean");
  } else if (event.kind === "clock-output") {
    assertExactKeys(
      event,
      ["kind", "level", "timestamp_us"],
      "Clock output event",
    );
    assert.equal(typeof event.level, "boolean", "Clock level must be Boolean");
  } else {
    assert.fail("timing output event kind is unsupported");
  }
  assertIntegerRange(
    event.timestamp_us,
    0,
    maximumSafeInteger,
    "output event timestamp",
  );
}

function assertFrameShape(frame) {
  assertPlainObject(frame, "timing frame");
  if (frame.kind === "session-open") {
    assertExactKeys(
      frame,
      [
        "format",
        "format_version",
        "protocol_version",
        "kind",
        "session_id",
        "session_epoch",
        "sequence",
      ],
      "session-open frame",
    );
    assertFrameBase(frame);
    assert.equal(frame.sequence, 1, "session-open sequence must be one");
  } else if (frame.kind === "heartbeat") {
    assertExactKeys(
      frame,
      [
        "format",
        "format_version",
        "protocol_version",
        "kind",
        "session_id",
        "session_epoch",
        "sequence",
      ],
      "heartbeat frame",
    );
    assertFrameBase(frame);
  } else if (frame.kind === "schedule") {
    assertExactKeys(
      frame,
      [
        "format",
        "format_version",
        "protocol_version",
        "kind",
        "session_id",
        "session_epoch",
        "sequence",
        "coverage_until_us",
        "events",
      ],
      "schedule frame",
    );
    assertFrameBase(frame);
    assertIntegerRange(
      frame.coverage_until_us,
      1,
      maximumSafeInteger,
      "schedule coverage",
    );
    assert(Array.isArray(frame.events), "schedule events must be an array");
    assert(frame.events.length <= 256, "schedule exceeds schema batch limit");
    frame.events.forEach(assertOutputEvent);
  } else {
    assert.fail("timing frame kind is unsupported");
  }
}

export function assertTimingFrameInvariants(frame) {
  assertFrameShape(frame);
  if (frame.kind !== "schedule") {
    return;
  }
  let previousTimestamp;
  for (const event of frame.events) {
    assert(
      event.timestamp_us <= frame.coverage_until_us,
      "output event exceeds declared schedule coverage",
    );
    if (previousTimestamp !== undefined) {
      assert(
        event.timestamp_us >= previousTimestamp,
        "schedule events must be timestamp ordered",
      );
    }
    previousTimestamp = event.timestamp_us;
  }
}

function sourceKey(event) {
  return event.kind === "cv-input" ? `cv:${event.input_index}` : event.kind;
}

export class TimingSimulator {
  #profile;
  #nowUs = 0;
  #currentSession;
  #highestSessionEpoch = 0;
  #lastSequence = 0;
  #lastHeartbeatUs = 0;
  #coverageUntilUs;
  #lastScheduledTimestampUs;
  #outputQueue = [];
  #inputQueue = [];
  #inputSequence = 0;
  #sourceSequences = new Map();
  #nextOutputOrder = 0;
  #nextOutputDelayUs;
  #dropNextInput = false;
  #lastFault;
  #outputs;
  #telemetry = {
    accepted_frames: 0,
    rejected_frames: 0,
    accepted_batches: 0,
    input_overflows: 0,
    injected_input_drops: 0,
    emitted_outputs: 0,
    queue_underruns: 0,
    watchdog_trips: 0,
    manual_disconnects: 0,
    session_resyncs: 0,
  };

  constructor(profile) {
    assertTimingSimulatorProfile(profile);
    this.#profile = structuredClone(profile);
    this.#outputs = {
      cv: Array(profile.capabilities.cv_outputs).fill(0),
      gate: Array(profile.capabilities.gate_outputs).fill(false),
      clock: false,
    };
  }

  receive(frame) {
    try {
      const result = this.#receive(frame);
      this.#telemetry.accepted_frames += 1;
      return result;
    } catch (error) {
      this.#telemetry.rejected_frames += 1;
      throw error;
    }
  }

  #receive(frame) {
    assertTimingFrameInvariants(frame);
    if (frame.kind === "session-open") {
      assert(
        frame.session_epoch > this.#highestSessionEpoch,
        "session epoch must increase",
      );
      safeAdd(
        this.#nowUs,
        this.#profile.heartbeat_timeout_us,
        "heartbeat deadline",
      );
      if (this.#currentSession !== undefined) {
        this.#telemetry.session_resyncs += 1;
      }
      this.#setSafeOutputs();
      this.#outputQueue = [];
      this.#inputQueue = [];
      this.#currentSession = {
        id: frame.session_id,
        epoch: frame.session_epoch,
      };
      this.#highestSessionEpoch = frame.session_epoch;
      this.#lastSequence = frame.sequence;
      this.#lastHeartbeatUs = this.#nowUs;
      this.#coverageUntilUs = undefined;
      this.#lastScheduledTimestampUs = undefined;
      this.#nextOutputDelayUs = undefined;
      this.#dropNextInput = false;
      this.#lastFault = undefined;
      return this.status();
    }

    this.#assertActiveFrame(frame);
    if (frame.kind === "heartbeat") {
      safeAdd(
        this.#nowUs,
        this.#profile.heartbeat_timeout_us,
        "heartbeat deadline",
      );
      this.#lastSequence = frame.sequence;
      this.#lastHeartbeatUs = this.#nowUs;
      return this.status();
    }

    assert(
      frame.events.length <= this.#profile.max_batch_events,
      "schedule exceeds configured batch limit",
    );
    assert(
      this.#outputQueue.length + frame.events.length <=
        this.#profile.output_queue_capacity,
      "output queue capacity exceeded",
    );
    assert(
      frame.coverage_until_us > this.#nowUs,
      "schedule coverage must extend beyond current time",
    );
    const maximumScheduleTimestamp = safeAdd(
      this.#nowUs,
      this.#profile.max_schedule_horizon_us,
      "schedule horizon",
    );
    assert(
      frame.coverage_until_us <= maximumScheduleTimestamp,
      "schedule coverage exceeds configured horizon",
    );
    if (this.#coverageUntilUs !== undefined) {
      assert(
        frame.coverage_until_us > this.#coverageUntilUs,
        "schedule coverage must increase",
      );
    }

    const prepared = [];
    let previousTimestamp = this.#lastScheduledTimestampUs;
    let delayPending = this.#nextOutputDelayUs;
    for (const event of frame.events) {
      this.#assertOutputCapability(event);
      assert(
        event.timestamp_us >= this.#nowUs,
        "output event is scheduled in the past",
      );
      assert(
        event.timestamp_us <= frame.coverage_until_us,
        "output event exceeds declared schedule coverage",
      );
      assert(
        event.timestamp_us <= maximumScheduleTimestamp,
        "output event exceeds configured horizon",
      );
      if (previousTimestamp !== undefined) {
        assert(
          event.timestamp_us >= previousTimestamp,
          "output events must be globally timestamp ordered",
        );
      }
      let effectiveTimestampUs = event.timestamp_us;
      if (delayPending !== undefined) {
        assert(
          effectiveTimestampUs <= maximumSafeInteger - delayPending,
          "injected output delay exceeds the timestamp range",
        );
        effectiveTimestampUs += delayPending;
        delayPending = undefined;
      }
      prepared.push({
        ...structuredClone(event),
        effective_timestamp_us: effectiveTimestampUs,
        order: this.#nextOutputOrder + prepared.length,
      });
      previousTimestamp = event.timestamp_us;
    }

    this.#nextOutputDelayUs = delayPending;
    this.#nextOutputOrder += prepared.length;
    this.#outputQueue.push(...prepared);
    this.#outputQueue.sort(
      (left, right) =>
        left.effective_timestamp_us - right.effective_timestamp_us ||
        left.order - right.order,
    );
    this.#lastScheduledTimestampUs = previousTimestamp;
    this.#coverageUntilUs = frame.coverage_until_us;
    this.#lastSequence = frame.sequence;
    this.#telemetry.accepted_batches += 1;
    return this.status();
  }

  #assertActiveFrame(frame) {
    assert(this.#currentSession !== undefined, "timing session is not open");
    assert.equal(
      frame.session_id,
      this.#currentSession.id,
      "timing frame belongs to another session",
    );
    assert.equal(
      frame.session_epoch,
      this.#currentSession.epoch,
      "timing frame has a stale session epoch",
    );
    assert(
      this.#lastSequence < maximumSafeInteger,
      "timing sequence is exhausted; open a new session",
    );
    assert.equal(
      frame.sequence,
      this.#lastSequence + 1,
      "timing frame sequence is not contiguous",
    );
  }

  #assertOutputCapability(event) {
    const capabilities = this.#profile.capabilities;
    if (event.kind === "cv-output") {
      assert(
        event.output_index < capabilities.cv_outputs,
        "CV output is unavailable",
      );
    } else if (event.kind === "gate-output") {
      assert(
        event.output_index < capabilities.gate_outputs,
        "Gate output is unavailable",
      );
    } else {
      assert(capabilities.has_clock_output, "Clock output is unavailable");
    }
  }

  delayNextOutput(delayUs) {
    assertIntegerRange(
      delayUs,
      1,
      this.#profile.max_schedule_horizon_us,
      "injected output delay",
    );
    assert.equal(
      this.#nextOutputDelayUs,
      undefined,
      "an output-delay fault is already armed",
    );
    this.#nextOutputDelayUs = delayUs;
  }

  dropNextCapturedInput() {
    assert.equal(
      this.#dropNextInput,
      false,
      "an input-drop fault is already armed",
    );
    this.#dropNextInput = true;
  }

  injectInput(event) {
    this.#assertInputEvent(event);
    assert(
      this.#inputSequence < maximumSafeInteger,
      "input event sequence is exhausted",
    );
    const key = sourceKey(event);
    const previousSourceSequence = this.#sourceSequences.get(key) ?? 0;
    assert(
      previousSourceSequence < maximumSafeInteger,
      "input source sequence is exhausted",
    );
    this.#inputSequence += 1;
    const sourceSequence = previousSourceSequence + 1;
    this.#sourceSequences.set(key, sourceSequence);
    const captured = {
      ...structuredClone(event),
      timestamp_us: this.#nowUs,
      sequence: this.#inputSequence,
      source_sequence: sourceSequence,
    };

    if (this.#dropNextInput) {
      this.#dropNextInput = false;
      this.#telemetry.injected_input_drops += 1;
      return { accepted: false, reason: "injected-drop" };
    }
    if (this.#inputQueue.length >= this.#profile.input_queue_capacity) {
      this.#telemetry.input_overflows += 1;
      return { accepted: false, reason: "queue-overflow" };
    }
    this.#inputQueue.push(captured);
    return { accepted: true, event: structuredClone(captured) };
  }

  #assertInputEvent(event) {
    assertPlainObject(event, "timing input event");
    const capabilities = this.#profile.capabilities;
    if (event.kind === "cv-input") {
      assertExactKeys(
        event,
        ["kind", "input_index", "value"],
        "CV input event",
      );
      assertIntegerRange(event.input_index, 0, 3, "CV input index");
      assert(
        event.input_index < capabilities.cv_inputs,
        "CV input is unavailable",
      );
      assertIntegerRange(event.value, -1_000_000, 1_000_000, "CV input value");
    } else if (event.kind === "clock-input") {
      assertExactKeys(event, ["kind", "edge"], "Clock input event");
      assert(capabilities.has_clock_input, "Clock input is unavailable");
      assert(
        event.edge === "rising" || event.edge === "falling",
        "Clock input edge is unsupported",
      );
    } else if (event.kind === "reset-input") {
      assertExactKeys(event, ["kind", "edge"], "Reset input event");
      assert(capabilities.has_reset_input, "Reset input is unavailable");
      assert(
        event.edge === "rising" || event.edge === "falling",
        "Reset input edge is unsupported",
      );
    } else {
      assert.fail("timing input event kind is unsupported");
    }
  }

  readInputs(maxEvents) {
    assertIntegerRange(
      maxEvents,
      1,
      this.#profile.max_batch_events,
      "input read limit",
    );
    return this.#inputQueue
      .splice(0, maxEvents)
      .map((event) => structuredClone(event));
  }

  advanceTo(targetUs) {
    assertIntegerRange(
      targetUs,
      this.#nowUs,
      maximumSafeInteger,
      "virtual clock target",
    );
    if (this.#currentSession === undefined) {
      this.#nowUs = targetUs;
      return [];
    }

    const watchdogAt = safeAdd(
      this.#lastHeartbeatUs,
      this.#profile.heartbeat_timeout_us,
      "heartbeat deadline",
    );
    let failure;
    if (targetUs >= watchdogAt) {
      failure = { kind: "watchdog-timeout", timestamp_us: watchdogAt };
    }
    if (
      this.#coverageUntilUs !== undefined &&
      targetUs > this.#coverageUntilUs &&
      (failure === undefined || this.#coverageUntilUs < failure.timestamp_us)
    ) {
      failure = {
        kind: "queue-underrun",
        timestamp_us: this.#coverageUntilUs,
      };
    }

    const emitted = [];
    const limit = failure?.timestamp_us ?? targetUs;
    const inclusive = failure?.kind !== "watchdog-timeout";
    while (this.#outputQueue.length > 0) {
      const next = this.#outputQueue[0];
      const isDue = inclusive
        ? next.effective_timestamp_us <= limit
        : next.effective_timestamp_us < limit;
      if (!isDue) {
        break;
      }
      this.#outputQueue.shift();
      this.#applyOutput(next);
      emitted.push({
        kind: next.kind,
        ...(next.output_index === undefined
          ? {}
          : { output_index: next.output_index }),
        ...(next.level === undefined ? {} : { level: next.level }),
        ...(next.value === undefined ? {} : { value: next.value }),
        timestamp_us: next.timestamp_us,
        emitted_timestamp_us: next.effective_timestamp_us,
      });
      this.#telemetry.emitted_outputs += 1;
    }

    if (failure !== undefined) {
      this.#nowUs = failure.timestamp_us;
      if (failure.kind === "watchdog-timeout") {
        this.#telemetry.watchdog_trips += 1;
      } else {
        this.#telemetry.queue_underruns += 1;
      }
      this.#transitionToSafe(failure);
    } else {
      this.#nowUs = targetUs;
    }
    return emitted;
  }

  #applyOutput(event) {
    if (event.kind === "cv-output") {
      this.#outputs.cv[event.output_index] = event.value;
    } else if (event.kind === "gate-output") {
      this.#outputs.gate[event.output_index] = event.level;
    } else {
      this.#outputs.clock = event.level;
    }
  }

  disconnect() {
    if (this.#currentSession !== undefined) {
      this.#telemetry.manual_disconnects += 1;
      this.#transitionToSafe({
        kind: "manual-disconnect",
        timestamp_us: this.#nowUs,
      });
    }
    return this.status();
  }

  #transitionToSafe(fault) {
    this.#setSafeOutputs();
    this.#outputQueue = [];
    this.#inputQueue = [];
    this.#currentSession = undefined;
    this.#coverageUntilUs = undefined;
    this.#lastScheduledTimestampUs = undefined;
    this.#nextOutputDelayUs = undefined;
    this.#dropNextInput = false;
    this.#lastFault = fault;
  }

  #setSafeOutputs() {
    this.#outputs.cv.fill(0);
    this.#outputs.gate.fill(false);
    this.#outputs.clock = false;
  }

  status() {
    return structuredClone({
      format: "hyper-timing-simulator-status",
      format_version: 1,
      protocol_version: this.#profile.protocol_version,
      now_us: this.#nowUs,
      connected: this.#currentSession !== undefined,
      session:
        this.#currentSession === undefined
          ? null
          : {
              id: this.#currentSession.id,
              epoch: this.#currentSession.epoch,
              last_sequence: this.#lastSequence,
            },
      coverage_until_us: this.#coverageUntilUs ?? null,
      queued_outputs: this.#outputQueue.length,
      queued_inputs: this.#inputQueue.length,
      outputs: this.#outputs,
      last_fault: this.#lastFault ?? null,
      telemetry: this.#telemetry,
    });
  }
}
