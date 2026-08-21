import assert from "node:assert/strict";

export function assertDeviceChainInvariants(chain) {
  assert.equal(
    new Set(chain.devices.map((device) => device.id)).size,
    chain.devices.length,
    "device identities must be unique within a chain",
  );
}
