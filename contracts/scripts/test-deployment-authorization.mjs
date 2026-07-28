import assert from "node:assert/strict";
import { generateKeyPairSync, randomUUID } from "node:crypto";
import { mkdtemp, readFile, rm } from "node:fs/promises";
import { tmpdir } from "node:os";
import { join } from "node:path";

import {
  approvePairingRequest,
  authorizeDeploymentRequest,
  createDeploymentAuthorizationState,
  ed25519PublicJwk,
  issuePairingChallenge,
  jwkThumbprint,
  recordDeploymentResult,
  revokePairedClient,
  signDeploymentRequest,
  signPairingRequest,
  validateDeploymentAuthorizationState,
  validateInitialDeploymentAuthorizationState,
  verifyDeploymentRequestBundle,
} from "../lib/deployment-authorization.mjs";
import { inspectDeploymentBundle } from "../lib/deployment-bundle.mjs";
import { strictParse } from "../lib/strict-json.mjs";
import { buildProjectBundle } from "./build-project-bundle.mjs";
import { createMinimalProject } from "./test-project-fixture.mjs";

const now = 1785153600000;
const deviceId = "10000000-0000-4000-8000-000000000001";
const clientId = "20000000-0000-4000-8000-000000000001";
const deviceKeys = generateKeyPairSync("ed25519");
const clientKeys = generateKeyPairSync("ed25519");
const secondClientKeys = generateKeyPairSync("ed25519");
const devicePublicKey = ed25519PublicJwk(deviceKeys.publicKey);
const clientPublicKey = ed25519PublicJwk(clientKeys.publicKey);
const secondClientPublicKey = ed25519PublicJwk(secondClientKeys.publicKey);

const policy = {
  max_paired_clients: 1,
  max_recent_requests: 2,
  max_audit_events: 16,
  max_request_lifetime_ms: 300000,
  max_clock_skew_ms: 30000,
  max_bundle_bytes: 1000,
};
const terminalFixture = strictParse(
  await readFile(
    new URL(
      "../fixtures/valid/deployment-authorization-terminal-state.json",
      import.meta.url,
    ),
    "utf8",
  ),
);
assert.equal(
  await validateDeploymentAuthorizationState(terminalFixture),
  terminalFixture,
);

let state = await createDeploymentAuthorizationState({
  deviceId,
  devicePublicKey,
  policy,
});
assert.equal(
  await validateInitialDeploymentAuthorizationState(state),
  state,
);
assert.equal(state.generation, 1);
assert.equal(jwkThumbprint(clientPublicKey).length, 43);
assert.equal(
  jwkThumbprint({
    kty: "OKP",
    crv: "Ed25519",
    x: "11qYAYKxCrfVS_7TyWQHOg7hcvPapiMlrwIaaPcHURo",
  }),
  "kPrK_qmxVWaYVA9wwBF6Iuo3vVzz7TxHCTwXBygrS4k",
);

await assert.rejects(
  issuePairingChallenge(state, {
    nowEpochMs: now,
    lifetimeMs: 60000,
    physicalPresence: false,
  }),
  /physical presence is required to open pairing/,
);
const challengeResult = await issuePairingChallenge(state, {
  nowEpochMs: now,
  lifetimeMs: 60000,
  physicalPresence: true,
});
state = challengeResult.state;
assert.equal(challengeResult.challenge.device_id, deviceId);

const pairingPayload = {
  format: "hyper-deployment-pairing-request",
  format_version: 1,
  device_id: deviceId,
  client_id: clientId,
  client_name: "Studio Desktop",
  client_public_key: clientPublicKey,
  challenge_id: challengeResult.challenge.challenge_id,
  issued_at_epoch_ms: now - 1000,
  expires_at_epoch_ms: now + 60000,
  extensions: {},
};
const pairingRequest = signPairingRequest(
  pairingPayload,
  clientKeys.privateKey,
);

const tamperedPairing = {
  ...pairingRequest,
  client_name: "Tampered Desktop",
};
const invalidPairing = await approvePairingRequest(state, tamperedPairing, {
  nowEpochMs: now,
  physicalAuthorization: true,
});
assert.equal(invalidPairing.accepted, false);
assert.equal(invalidPairing.reason, "invalid-signature");
assert(invalidPairing.state.pending_pairing_challenge);
state = invalidPairing.state;

const wrongDevicePairing = signPairingRequest(
  {
    ...pairingPayload,
    device_id: "10000000-0000-4000-8000-000000000002",
  },
  clientKeys.privateKey,
);
const wrongPairingDevice = await approvePairingRequest(
  state,
  wrongDevicePairing,
  {
    nowEpochMs: now,
    physicalAuthorization: true,
  },
);
assert.equal(wrongPairingDevice.accepted, false);
assert.equal(wrongPairingDevice.reason, "wrong-device");
state = wrongPairingDevice.state;

const expiredPairingRequest = signPairingRequest(
  {
    ...pairingPayload,
    issued_at_epoch_ms: now - 120000,
    expires_at_epoch_ms: now,
  },
  clientKeys.privateKey,
);
const expiredPairing = await approvePairingRequest(
  state,
  expiredPairingRequest,
  {
    nowEpochMs: now,
    physicalAuthorization: true,
  },
);
assert.equal(expiredPairing.accepted, false);
assert.equal(expiredPairing.reason, "expired");
state = expiredPairing.state;

const wrongChallengeRequest = signPairingRequest(
  { ...pairingPayload, challenge_id: randomUUID() },
  clientKeys.privateKey,
);
const wrongChallenge = await approvePairingRequest(
  state,
  wrongChallengeRequest,
  {
    nowEpochMs: now,
    physicalAuthorization: true,
  },
);
assert.equal(wrongChallenge.accepted, false);
assert.equal(wrongChallenge.reason, "invalid-pairing-challenge");
state = wrongChallenge.state;

const absentPresence = await approvePairingRequest(state, pairingRequest, {
  nowEpochMs: now,
  physicalAuthorization: false,
});
assert.equal(absentPresence.accepted, false);
assert.equal(absentPresence.reason, "physical-authorization-required");
assert(absentPresence.state.pending_pairing_challenge);
state = absentPresence.state;

const paired = await approvePairingRequest(state, pairingRequest, {
  nowEpochMs: now,
  physicalAuthorization: true,
});
assert.equal(paired.accepted, true);
assert.equal(paired.reason, "paired");
assert.equal(paired.state.clients.length, 1);
assert.equal(
  paired.client.key_thumbprint,
  jwkThumbprint(clientPublicKey),
);
assert.equal(paired.state.pending_pairing_challenge, null);
state = paired.state;

const repeatedPairing = await approvePairingRequest(state, pairingRequest, {
  nowEpochMs: now,
  physicalAuthorization: true,
});
assert.equal(repeatedPairing.accepted, true);
assert.equal(repeatedPairing.idempotent, true);
assert.deepEqual(repeatedPairing.state, state);
const expiredRepeatedPairing = await approvePairingRequest(
  state,
  pairingRequest,
  {
    nowEpochMs: pairingRequest.expires_at_epoch_ms + 1,
    physicalAuthorization: true,
  },
);
assert.equal(expiredRepeatedPairing.accepted, true);
assert.equal(expiredRepeatedPairing.idempotent, true);
assert.deepEqual(expiredRepeatedPairing.state, state);
await assert.rejects(
  validateInitialDeploymentAuthorizationState(state),
  /initial authorization state/,
);

const collisionPairing = signPairingRequest(
  {
    ...pairingPayload,
    client_public_key: secondClientPublicKey,
  },
  secondClientKeys.privateKey,
);
const clientCollision = await approvePairingRequest(
  state,
  collisionPairing,
  {
    nowEpochMs: now,
    physicalAuthorization: true,
  },
);
assert.equal(clientCollision.accepted, false);
assert.equal(clientCollision.reason, "client-id-collision");
state = clientCollision.state;

const secondChallenge = await issuePairingChallenge(state, {
  nowEpochMs: now,
  lifetimeMs: 60000,
  physicalPresence: true,
});
state = secondChallenge.state;
const secondClientId = "20000000-0000-4000-8000-000000000002";
const duplicateKeyPairing = signPairingRequest(
  {
    ...pairingPayload,
    client_id: secondClientId,
    challenge_id: secondChallenge.challenge.challenge_id,
  },
  clientKeys.privateKey,
);
const duplicateKey = await approvePairingRequest(
  state,
  duplicateKeyPairing,
  {
    nowEpochMs: now,
    physicalAuthorization: true,
  },
);
assert.equal(duplicateKey.accepted, false);
assert.equal(duplicateKey.reason, "key-already-paired");
state = duplicateKey.state;

const clientLimitPairing = signPairingRequest(
  {
    ...pairingPayload,
    client_id: secondClientId,
    client_public_key: secondClientPublicKey,
    challenge_id: secondChallenge.challenge.challenge_id,
  },
  secondClientKeys.privateKey,
);
const clientLimit = await approvePairingRequest(state, clientLimitPairing, {
  nowEpochMs: now,
  physicalAuthorization: true,
});
assert.equal(clientLimit.accepted, false);
assert.equal(clientLimit.reason, "client-limit");
state = clientLimit.state;

function deploymentRequest(sequence, overrides = {}, key = clientKeys.privateKey) {
  return signDeploymentRequest(
    {
      format: "hyper-deployment-request",
      format_version: 1,
      device_id: deviceId,
      client_id: clientId,
      request_id: randomUUID(),
      sequence,
      operation: "deploy",
      bundle_sha256:
        "aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa",
      bundle_byte_length: 100,
      runtime_version: "1.0.0",
      issued_at_epoch_ms: now - 1000,
      expires_at_epoch_ms: now + 60000,
      extensions: {},
      ...overrides,
    },
    key,
  );
}

const request1 = deploymentRequest(1);
const authorized1 = await authorizeDeploymentRequest(state, request1, {
  nowEpochMs: now,
});
assert.equal(authorized1.accepted, true);
assert.equal(authorized1.idempotent, false);
assert.equal(authorized1.state.clients[0].last_request_sequence, 1);
state = authorized1.state;

const repeatedRequest = await authorizeDeploymentRequest(state, request1, {
  nowEpochMs: now,
});
assert.equal(repeatedRequest.accepted, true);
assert.equal(repeatedRequest.idempotent, true);
assert.deepEqual(repeatedRequest.state, state);
const expiredRepeatedRequest = await authorizeDeploymentRequest(
  state,
  request1,
  { nowEpochMs: request1.expires_at_epoch_ms + 1 },
);
assert.equal(expiredRepeatedRequest.accepted, true);
assert.equal(expiredRepeatedRequest.idempotent, true);
assert.deepEqual(expiredRepeatedRequest.state, state);

const inProgress = await authorizeDeploymentRequest(
  state,
  deploymentRequest(2),
  { nowEpochMs: now },
);
assert.equal(inProgress.accepted, false);
assert.equal(inProgress.reason, "request-in-progress");
state = inProgress.state;

const terminal1 = await recordDeploymentResult(state, request1.request_id, {
  succeeded: true,
  deploymentGeneration: 1,
  nowEpochMs: now,
});
assert.equal(terminal1.request.status, "succeeded");
assert.equal(terminal1.request.deployment_generation, 1);
assert.equal(terminal1.state.audit_events.at(-1).result, "succeeded");
state = terminal1.state;
const expiredTerminalRequest = await authorizeDeploymentRequest(
  state,
  request1,
  { nowEpochMs: request1.expires_at_epoch_ms + 1 },
);
assert.equal(expiredTerminalRequest.accepted, true);
assert.equal(expiredTerminalRequest.idempotent, true);
assert.equal(expiredTerminalRequest.authorization.status, "succeeded");
assert.deepEqual(expiredTerminalRequest.state, state);
const repeatedTerminal1 = await recordDeploymentResult(
  state,
  request1.request_id,
  {
    succeeded: true,
    deploymentGeneration: 1,
    nowEpochMs: now,
  },
);
assert.equal(repeatedTerminal1.idempotent, true);
assert.deepEqual(repeatedTerminal1.state, state);

const invalidSignature = await authorizeDeploymentRequest(
  state,
  { ...request1, bundle_byte_length: 101 },
  { nowEpochMs: now },
);
assert.equal(invalidSignature.accepted, false);
assert.equal(invalidSignature.reason, "invalid-signature");
state = invalidSignature.state;

const forgedRequest = deploymentRequest(
  2,
  {},
  secondClientKeys.privateKey,
);
const forged = await authorizeDeploymentRequest(state, forgedRequest, {
  nowEpochMs: now,
});
assert.equal(forged.accepted, false);
assert.equal(forged.reason, "invalid-signature");
state = forged.state;

const requestCollision = deploymentRequest(2, {
  request_id: request1.request_id,
});
const collision = await authorizeDeploymentRequest(state, requestCollision, {
  nowEpochMs: now,
});
assert.equal(collision.accepted, false);
assert.equal(collision.reason, "request-id-collision");
state = collision.state;

const replay = await authorizeDeploymentRequest(
  state,
  deploymentRequest(1),
  { nowEpochMs: now },
);
assert.equal(replay.accepted, false);
assert.equal(replay.reason, "replay");
state = replay.state;

const expired = await authorizeDeploymentRequest(
  state,
  deploymentRequest(2, {
    issued_at_epoch_ms: now - 120000,
    expires_at_epoch_ms: now - 60000,
  }),
  { nowEpochMs: now },
);
assert.equal(expired.accepted, false);
assert.equal(expired.reason, "expired");
state = expired.state;

const future = await authorizeDeploymentRequest(
  state,
  deploymentRequest(2, {
    issued_at_epoch_ms: now + policy.max_clock_skew_ms + 1,
    expires_at_epoch_ms: now + policy.max_clock_skew_ms + 60001,
  }),
  { nowEpochMs: now },
);
assert.equal(future.accepted, false);
assert.equal(future.reason, "not-yet-valid");
state = future.state;

const invalidLifetime = await authorizeDeploymentRequest(
  state,
  deploymentRequest(2, {
    issued_at_epoch_ms: now,
    expires_at_epoch_ms: now + policy.max_request_lifetime_ms + 1,
  }),
  { nowEpochMs: now },
);
assert.equal(invalidLifetime.accepted, false);
assert.equal(invalidLifetime.reason, "invalid-lifetime");
state = invalidLifetime.state;

const oversized = await authorizeDeploymentRequest(
  state,
  deploymentRequest(2, { bundle_byte_length: policy.max_bundle_bytes + 1 }),
  { nowEpochMs: now },
);
assert.equal(oversized.accepted, false);
assert.equal(oversized.reason, "bundle-too-large");
state = oversized.state;

const wrongDevice = await authorizeDeploymentRequest(
  state,
  deploymentRequest(2, {
    device_id: "10000000-0000-4000-8000-000000000002",
  }),
  { nowEpochMs: now },
);
assert.equal(wrongDevice.accepted, false);
assert.equal(wrongDevice.reason, "wrong-device");
state = wrongDevice.state;

const unknownClient = await authorizeDeploymentRequest(
  state,
  deploymentRequest(
    2,
    { client_id: "20000000-0000-4000-8000-000000000003" },
    secondClientKeys.privateKey,
  ),
  { nowEpochMs: now },
);
assert.equal(unknownClient.accepted, false);
assert.equal(unknownClient.reason, "unknown-client");
state = unknownClient.state;

const acceptedRequests = [];
for (const sequence of [2, 3, 4]) {
  const request = deploymentRequest(sequence);
  const result = await authorizeDeploymentRequest(state, request, {
    nowEpochMs: now,
  });
  assert.equal(result.accepted, true);
  acceptedRequests.push(request);
  const terminal = await recordDeploymentResult(
    result.state,
    request.request_id,
    {
      succeeded: true,
      deploymentGeneration: sequence,
      nowEpochMs: now,
    },
  );
  state = terminal.state;
}
assert.equal(state.recent_requests.length, policy.max_recent_requests);
assert.deepEqual(
  state.recent_requests.map((request) => request.sequence),
  [3, 4],
);
assert.equal(state.audit_events.length, policy.max_audit_events);

const trimmedReplay = await authorizeDeploymentRequest(
  state,
  acceptedRequests[0],
  { nowEpochMs: now },
);
assert.equal(trimmedReplay.accepted, false);
assert.equal(trimmedReplay.reason, "replay");
state = trimmedReplay.state;

const refusedRevocation = await revokePairedClient(state, clientId, {
  nowEpochMs: now,
  physicalAuthorization: false,
});
assert.equal(refusedRevocation.accepted, false);
assert.equal(refusedRevocation.reason, "physical-authorization-required");
state = refusedRevocation.state;

const revoked = await revokePairedClient(state, clientId, {
  nowEpochMs: now,
  physicalAuthorization: true,
});
assert.equal(revoked.accepted, true);
assert.equal(revoked.reason, "revoked");
assert.equal(revoked.state.clients.length, 0);
assert.equal(revoked.state.recent_requests.length, 0);
state = revoked.state;

const afterRevocation = await authorizeDeploymentRequest(
  state,
  deploymentRequest(5),
  { nowEpochMs: now },
);
assert.equal(afterRevocation.accepted, false);
assert.equal(afterRevocation.reason, "unknown-client");
assert(afterRevocation.state.audit_events.length <= policy.max_audit_events);

const bundleRoot = await mkdtemp(join(tmpdir(), "hyper-auth-bundle-"));
try {
  const source = await createMinimalProject(bundleRoot, "source", {
    revision: 1,
  });
  const bundlePath = join(bundleRoot, "authorized.runtime");
  await buildProjectBundle(source.root, bundlePath, "1.0.0");
  const inspected = await inspectDeploymentBundle(bundlePath);
  const boundRequest = deploymentRequest(5, {
    bundle_sha256: inspected.bundle_sha256,
    bundle_byte_length: inspected.bundle_byte_length,
  });
  const verifiedBinding = await verifyDeploymentRequestBundle(
    boundRequest,
    bundlePath,
    "1.0.0",
  );
  assert.equal(verifiedBinding.bundle_sha256, boundRequest.bundle_sha256);
  await assert.rejects(
    verifyDeploymentRequestBundle(
      {
        ...boundRequest,
        bundle_sha256:
          "bbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbb",
      },
      bundlePath,
      "1.0.0",
    ),
    /bundle identity mismatch/,
  );
  await assert.rejects(
    verifyDeploymentRequestBundle(
      {
        ...boundRequest,
        bundle_byte_length: boundRequest.bundle_byte_length + 1,
      },
      bundlePath,
      "1.0.0",
    ),
    /bundle byte length mismatch/,
  );
  await assert.rejects(
    verifyDeploymentRequestBundle(boundRequest, bundlePath, "2.0.0"),
    /runtime version mismatch/,
  );
} finally {
  await rm(bundleRoot, { recursive: true, force: true });
}

console.log("Deployment authorization passed: 57 security cases");
