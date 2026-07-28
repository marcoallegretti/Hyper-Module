import assert from "node:assert/strict";

function assertUnique(values, label) {
  assert.equal(new Set(values).size, values.length, `${label} must be unique`);
}

export function assertMixerInvariants(mixer) {
  assert(
    mixer !== null && typeof mixer === "object" && !Array.isArray(mixer),
    "mixer must be an object",
  );
  assert(Array.isArray(mixer.channels), "mixer channels must be an array");
  assert(
    Array.isArray(mixer.return_buses),
    "mixer return buses must be an array",
  );

  assertUnique(
    mixer.channels.map((channel) => channel.track_id),
    "mixer channel track identifiers",
  );
  assertUnique(
    mixer.return_buses.map((returnBus) => returnBus.bus),
    "mixer return bus names",
  );
  assertUnique(
    mixer.return_buses.map((returnBus) => returnBus.track_id),
    "mixer return track identifiers",
  );

  const sendIds = [];
  for (const channel of mixer.channels) {
    assertUnique(
      channel.sends.map((send) => send.bus),
      `${channel.track_id}: send bus names`,
    );
    for (let index = 1; index < channel.sends.length; index += 1) {
      assert(
        channel.sends[index - 1].bus < channel.sends[index].bus,
        `${channel.track_id}: sends must be sorted by bus`,
      );
    }
    sendIds.push(...channel.sends.map((send) => send.id));
  }
  assertUnique(sendIds, "mixer send identifiers");

  const returnTrackByBus = new Map(
    mixer.return_buses.map((returnBus) => [returnBus.bus, returnBus.track_id]),
  );
  const destinations = new Map(
    mixer.channels.map((channel) => [channel.track_id, []]),
  );
  for (const channel of mixer.channels) {
    if (channel.destination.kind === "track") {
      assert.notEqual(
        channel.destination.track_id,
        channel.track_id,
        `${channel.track_id}: channel cannot route to itself`,
      );
      destinations.get(channel.track_id).push(channel.destination.track_id);
    }
    for (const send of channel.sends) {
      const returnTrackId = returnTrackByBus.get(send.bus);
      if (returnTrackId !== undefined) {
        destinations.get(channel.track_id).push(returnTrackId);
      }
    }
  }

  const complete = new Set();
  function visit(trackId, active) {
    if (complete.has(trackId)) {
      return;
    }
    active.add(trackId);
    for (const destination of destinations.get(trackId) ?? []) {
      assert(
        !active.has(destination),
        `${trackId}: mixer routing contains a cycle`,
      );
      visit(destination, active);
    }
    active.delete(trackId);
    complete.add(trackId);
  }
  for (const channel of mixer.channels) {
    visit(channel.track_id, new Set());
  }
}

export function gainCoefficient(gainMillionths) {
  assert(
    Number.isInteger(gainMillionths) &&
      gainMillionths >= 0 &&
      gainMillionths <= 2_000_000,
    "gain must be an integer from 0 through 2000000 millionths",
  );
  return gainMillionths / 1_000_000;
}

export function soloEligibleTrackIds(mixer) {
  assertMixerInvariants(mixer);
  const soloed = mixer.channels.filter((channel) => channel.solo);
  if (soloed.length === 0) {
    return mixer.channels.map((channel) => channel.track_id);
  }

  const channelByTrack = new Map(
    mixer.channels.map((channel) => [channel.track_id, channel]),
  );
  const returnTrackByBus = new Map(
    mixer.return_buses.map((returnBus) => [returnBus.bus, returnBus.track_id]),
  );
  const eligible = new Set();
  const pending = soloed.map((channel) => channel.track_id);
  while (pending.length > 0) {
    const trackId = pending.shift();
    if (eligible.has(trackId)) {
      continue;
    }
    eligible.add(trackId);
    const channel = channelByTrack.get(trackId);
    if (channel === undefined) {
      continue;
    }
    if (channel.destination.kind === "track") {
      pending.push(channel.destination.track_id);
    }
    for (const send of channel.sends) {
      if (send.enabled) {
        const returnTrackId = returnTrackByBus.get(send.bus);
        if (returnTrackId !== undefined) {
          pending.push(returnTrackId);
        }
      }
    }
  }
  return mixer.channels
    .map((channel) => channel.track_id)
    .filter((trackId) => eligible.has(trackId));
}
