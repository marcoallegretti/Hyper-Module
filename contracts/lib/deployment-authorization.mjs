import assert from "node:assert/strict";
import {
  createHash,
  createPublicKey,
  randomUUID,
  sign as signBytes,
  verify as verifyBytes,
} from "node:crypto";
import { readFile } from "node:fs/promises";
import { dirname, resolve } from "node:path";
import { fileURLToPath } from "node:url";

import Ajv2020 from "ajv/dist/2020.js";
import canonicalize from "canonicalize";

import { inspectDeploymentBundle } from "./deployment-bundle.mjs";
import { strictParse } from "./strict-json.mjs";

const contractRoot = resolve(dirname(fileURLToPath(import.meta.url)), "..");
const PAIRING_DOMAIN = Buffer.from("Hyper Module pairing request v1\0", "utf8");
const DEPLOYMENT_DOMAIN = Buffer.from(
  "Hyper Module deployment request v1\0",
  "utf8",
);

async function createValidators() {
  const schemas = await Promise.all(
    [
      "common.schema.json",
      "deployment-pairing-request.schema.json",
      "deployment-request.schema.json",
      "deployment-authorization-state.schema.json",
    ].map(async (name) =>
      strictParse(
        await readFile(resolve(contractRoot, "schemas", "v1", name), "utf8"),
      ),
    ),
  );
  const ajv = new Ajv2020({
    allErrors: true,
    strict: true,
    validateFormats: false,
  });
  schemas.forEach((schema) => ajv.addSchema(schema));
  return {
    pairingRequest: ajv.getSchema(schemas[1].$id),
    deploymentRequest: ajv.getSchema(schemas[2].$id),
    state: ajv.getSchema(schemas[3].$id),
  };
}

const validators = createValidators();

function assertSafeTimestamp(value, label) {
  assert(
    Number.isSafeInteger(value) && value >= 0,
    `${label} must be a safe nonnegative integer`,
  );
}

function assertPublicKey(jwk) {
  const key = createPublicKey({ key: jwk, format: "jwk" });
  assert.equal(key.asymmetricKeyType, "ed25519", "public key must be Ed25519");
  const exported = key.export({ format: "jwk" });
  assert.deepEqual(
    { kty: exported.kty, crv: exported.crv, x: exported.x },
    jwk,
    "public key JWK is not canonical",
  );
  return key;
}

export function ed25519PublicJwk(key) {
  const publicKey = key.type === "public" ? key : createPublicKey(key);
  const exported = publicKey.export({ format: "jwk" });
  const jwk = { kty: exported.kty, crv: exported.crv, x: exported.x };
  assertPublicKey(jwk);
  return jwk;
}

export function jwkThumbprint(jwk) {
  assertPublicKey(jwk);
  const thumbprintInput = {
    crv: jwk.crv,
    kty: jwk.kty,
    x: jwk.x,
  };
  return createHash("sha256")
    .update(Buffer.from(canonicalize(thumbprintInput), "utf8"))
    .digest("base64url");
}

function unsignedEnvelope(envelope) {
  const { signature: _signature, ...unsigned } = envelope;
  return unsigned;
}

function signingBytes(domain, envelope) {
  return Buffer.concat([
    domain,
    Buffer.from(canonicalize(unsignedEnvelope(envelope)), "utf8"),
  ]);
}

function signEnvelope(domain, payload, privateKey) {
  const signature = signBytes(null, signingBytes(domain, payload), privateKey);
  assert.equal(signature.byteLength, 64, "Ed25519 signature must be 64 bytes");
  return { ...unsignedEnvelope(payload), signature: signature.toString("base64url") };
}

function verifyEnvelope(domain, envelope, publicJwk) {
  const signature = Buffer.from(envelope.signature, "base64url");
  if (
    signature.byteLength !== 64 ||
    signature.toString("base64url") !== envelope.signature
  ) {
    return false;
  }
  return verifyBytes(
    null,
    signingBytes(domain, envelope),
    assertPublicKey(publicJwk),
    signature,
  );
}

export function signPairingRequest(payload, privateKey) {
  return signEnvelope(PAIRING_DOMAIN, payload, privateKey);
}

export function signDeploymentRequest(payload, privateKey) {
  return signEnvelope(DEPLOYMENT_DOMAIN, payload, privateKey);
}

export async function verifyDeploymentRequestBundle(
  request,
  bundlePath,
  runtimeVersion,
) {
  await assertDeploymentRequest(request);
  assert.equal(
    runtimeVersion,
    request.runtime_version,
    "signed deployment request runtime version mismatch",
  );
  const bundle = await inspectDeploymentBundle(bundlePath);
  assert.equal(
    bundle.bundle_sha256,
    request.bundle_sha256,
    "signed deployment request bundle identity mismatch",
  );
  assert.equal(
    bundle.bundle_byte_length,
    request.bundle_byte_length,
    "signed deployment request bundle byte length mismatch",
  );
  return bundle;
}

function assertUnique(values, label) {
  assert.equal(
    new Set(values).size,
    values.length,
    `${label} values must be unique`,
  );
}

function assertSorted(values, label) {
  assert.deepEqual(
    values,
    [...values].sort((left, right) => left.localeCompare(right)),
    `${label} values must be sorted`,
  );
}

export async function validateDeploymentAuthorizationState(state) {
  const { state: validate } = await validators;
  assert(
    validate(state),
    `deployment authorization state schema validation failed: ${JSON.stringify(validate.errors)}`,
  );
  assert(
    state.clients.length <= state.policy.max_paired_clients,
    "paired client policy limit exceeded",
  );
  assert(
    state.recent_requests.length <= state.policy.max_recent_requests,
    "recent request policy limit exceeded",
  );
  assert(
    state.audit_events.length <= state.policy.max_audit_events,
    "audit event policy limit exceeded",
  );

  const clientIds = state.clients.map((client) => client.client_id);
  const thumbprints = state.clients.map(
    (client) => client.key_thumbprint,
  );
  assertUnique(clientIds, "client identifier");
  assertUnique(thumbprints, "client key thumbprint");
  assertSorted(clientIds, "client identifier");
  for (const client of state.clients) {
    assert.equal(
      jwkThumbprint(client.public_key),
      client.key_thumbprint,
      "client key thumbprint mismatch",
    );
  }

  assertUnique(
    state.recent_requests.map((request) => request.request_id),
    "recent request identifier",
  );
  assert(
    state.recent_requests.filter((request) => request.status === "authorized")
      .length <= 1,
    "only one deployment request may be in progress",
  );
  for (const request of state.recent_requests) {
    const client = state.clients.find(
      (candidate) => candidate.client_id === request.client_id,
    );
    assert(client, "recent request references an unknown client");
    assert(
      request.sequence <= client.last_request_sequence,
      "recent request exceeds client replay sequence",
    );
    if (request.status === "succeeded") {
      assert(
        request.deployment_generation !== null &&
          request.failure_code === null,
        "successful request must record only deployment generation",
      );
    } else if (request.status === "failed") {
      assert(
        request.deployment_generation === null &&
          request.failure_code === "deployment-failed",
        "failed request must record only its failure code",
      );
    } else {
      assert(
        request.deployment_generation === null &&
          request.failure_code === null,
        "authorized request cannot have a terminal result",
      );
    }
  }

  let previousAuditSequence = 0;
  for (const event of state.audit_events) {
    assert(
      event.sequence > previousAuditSequence,
      "audit event sequences must strictly increase",
    );
    previousAuditSequence = event.sequence;
    if (event.event_type === "deployment-result") {
      assert(
        event.result === "succeeded" || event.result === "failed",
        "deployment result audit must record succeeded or failed",
      );
    } else {
      assert(
        event.result === "accepted" || event.result === "rejected",
        "authorization audit must record accepted or rejected",
      );
    }
  }
  assert(
    previousAuditSequence <= state.audit_sequence,
    "audit sequence precedes retained events",
  );
  if (state.audit_events.length > 0) {
    assert.equal(
      previousAuditSequence,
      state.audit_sequence,
      "latest audit event must match audit sequence",
    );
  } else {
    assert.equal(state.audit_sequence, 0, "empty audit history must start at zero");
  }
  return state;
}

export async function validateInitialDeploymentAuthorizationState(state) {
  await validateDeploymentAuthorizationState(state);
  assert.equal(
    state.generation,
    1,
    "initial authorization state must start at generation 1",
  );
  assert.equal(
    state.pending_pairing_challenge,
    null,
    "initial authorization state cannot contain a pairing challenge",
  );
  assert.equal(
    state.clients.length,
    0,
    "initial authorization state cannot contain paired clients",
  );
  assert.equal(
    state.recent_requests.length,
    0,
    "initial authorization state cannot contain deployment requests",
  );
  assert.equal(
    state.audit_sequence,
    0,
    "initial authorization state cannot contain an audit sequence",
  );
  assert.equal(
    state.audit_events.length,
    0,
    "initial authorization state cannot contain audit events",
  );
  return state;
}

async function assertPairingRequest(request) {
  const { pairingRequest: validate } = await validators;
  assert(
    validate(request),
    `deployment pairing request schema validation failed: ${JSON.stringify(validate.errors)}`,
  );
  assertPublicKey(request.client_public_key);
  return request;
}

async function assertDeploymentRequest(request) {
  const { deploymentRequest: validate } = await validators;
  assert(
    validate(request),
    `deployment request schema validation failed: ${JSON.stringify(validate.errors)}`,
  );
  return request;
}

function nextGeneration(state) {
  const generation = state.generation + 1;
  assert(Number.isSafeInteger(generation), "authorization generation overflow");
  return generation;
}

function appendAudit(state, event) {
  const sequence = state.audit_sequence + 1;
  assert(Number.isSafeInteger(sequence), "audit sequence overflow");
  const auditEvent = {
    sequence,
    event_id: randomUUID(),
    event_type: event.event_type,
    client_id: event.client_id ?? null,
    request_id: event.request_id ?? null,
    result: event.result,
    reason: event.reason,
    occurred_at_epoch_ms: event.occurred_at_epoch_ms,
  };
  const auditEvents = [...state.audit_events, auditEvent].slice(
    -state.policy.max_audit_events,
  );
  return {
    ...state,
    generation: nextGeneration(state),
    audit_sequence: sequence,
    audit_events: auditEvents,
  };
}

async function decision(state, accepted, reason, event) {
  const nextState = appendAudit(state, {
    ...event,
    result: accepted ? "accepted" : "rejected",
    reason,
  });
  await validateDeploymentAuthorizationState(nextState);
  return {
    accepted,
    idempotent: false,
    reason,
    state: nextState,
  };
}

function temporalRejection(state, request, now) {
  if (
    request.expires_at_epoch_ms <= request.issued_at_epoch_ms ||
    request.expires_at_epoch_ms - request.issued_at_epoch_ms >
      state.policy.max_request_lifetime_ms
  ) {
    return "invalid-lifetime";
  }
  if (request.issued_at_epoch_ms > now + state.policy.max_clock_skew_ms) {
    return "not-yet-valid";
  }
  if (request.expires_at_epoch_ms <= now) return "expired";
  return null;
}

export async function createDeploymentAuthorizationState({
  deviceId,
  policy,
}) {
  const state = {
    format: "hyper-deployment-authorization-state",
    format_version: 1,
    generation: 1,
    device_id: deviceId,
    policy,
    pending_pairing_challenge: null,
    clients: [],
    recent_requests: [],
    audit_sequence: 0,
    audit_events: [],
    extensions: {},
  };
  return validateDeploymentAuthorizationState(state);
}

export async function issuePairingChallenge(
  state,
  {
    nowEpochMs,
    lifetimeMs = 120000,
    challengeId = randomUUID(),
    physicalPresence,
  },
) {
  await validateDeploymentAuthorizationState(state);
  assertSafeTimestamp(nowEpochMs, "current time");
  assert.equal(
    physicalPresence,
    true,
    "physical presence is required to open pairing",
  );
  assert(
    Number.isSafeInteger(lifetimeMs) &&
      lifetimeMs >= 1000 &&
      lifetimeMs <= state.policy.max_request_lifetime_ms,
    "pairing challenge lifetime is outside policy",
  );
  const expiresAt = nowEpochMs + lifetimeMs;
  assert(Number.isSafeInteger(expiresAt), "pairing challenge expiry overflow");
  const nextState = {
    ...state,
    generation: nextGeneration(state),
    pending_pairing_challenge: {
      challenge_id: challengeId,
      expires_at_epoch_ms: expiresAt,
    },
  };
  await validateDeploymentAuthorizationState(nextState);
  return {
    state: nextState,
    challenge: {
      device_id: state.device_id,
      challenge_id: challengeId,
      expires_at_epoch_ms: expiresAt,
    },
  };
}

export async function approvePairingRequest(
  state,
  request,
  { nowEpochMs, physicalAuthorization },
) {
  await validateDeploymentAuthorizationState(state);
  await assertPairingRequest(request);
  assertSafeTimestamp(nowEpochMs, "current time");
  assert.equal(
    typeof physicalAuthorization,
    "boolean",
    "physical authorization must be Boolean",
  );
  const auditBase = {
    event_type: "pairing",
    client_id: request.client_id,
    request_id: null,
    occurred_at_epoch_ms: nowEpochMs,
  };

  if (!verifyEnvelope(PAIRING_DOMAIN, request, request.client_public_key)) {
    return decision(state, false, "invalid-signature", auditBase);
  }
  if (request.device_id !== state.device_id) {
    return decision(state, false, "wrong-device", auditBase);
  }
  const thumbprint = jwkThumbprint(request.client_public_key);
  const existing = state.clients.find(
    (client) => client.client_id === request.client_id,
  );
  if (existing) {
    if (existing.key_thumbprint === thumbprint) {
      return {
        accepted: true,
        idempotent: true,
        reason: "already-paired",
        state,
        client: existing,
      };
    }
    return decision(state, false, "client-id-collision", auditBase);
  }
  if (
    state.clients.some(
      (client) => client.key_thumbprint === thumbprint,
    )
  ) {
    return decision(state, false, "key-already-paired", auditBase);
  }
  const temporalReason = temporalRejection(state, request, nowEpochMs);
  if (temporalReason) {
    return decision(state, false, temporalReason, auditBase);
  }
  if (
    !state.pending_pairing_challenge ||
    state.pending_pairing_challenge.challenge_id !== request.challenge_id ||
    state.pending_pairing_challenge.expires_at_epoch_ms <= nowEpochMs
  ) {
    return decision(
      state,
      false,
      "invalid-pairing-challenge",
      auditBase,
    );
  }
  if (!physicalAuthorization) {
    return decision(
      state,
      false,
      "physical-authorization-required",
      auditBase,
    );
  }
  if (state.clients.length >= state.policy.max_paired_clients) {
    return decision(state, false, "client-limit", auditBase);
  }

  const client = {
    client_id: request.client_id,
    client_name: request.client_name,
    public_key: request.client_public_key,
    key_thumbprint: thumbprint,
    paired_at_epoch_ms: nowEpochMs,
    last_request_sequence: 0,
  };
  const pairedState = {
    ...state,
    pending_pairing_challenge: null,
    clients: [...state.clients, client].sort((left, right) =>
      left.client_id.localeCompare(right.client_id),
    ),
  };
  const result = await decision(pairedState, true, "paired", auditBase);
  return { ...result, client };
}

export async function authorizeDeploymentRequest(
  state,
  request,
  { nowEpochMs },
) {
  await validateDeploymentAuthorizationState(state);
  await assertDeploymentRequest(request);
  assertSafeTimestamp(nowEpochMs, "current time");
  const auditBase = {
    event_type: "deployment-authorization",
    client_id: request.client_id,
    request_id: request.request_id,
    occurred_at_epoch_ms: nowEpochMs,
  };

  if (request.device_id !== state.device_id) {
    return decision(state, false, "wrong-device", auditBase);
  }
  const client = state.clients.find(
    (candidate) => candidate.client_id === request.client_id,
  );
  if (!client) {
    return decision(state, false, "unknown-client", auditBase);
  }
  if (!verifyEnvelope(DEPLOYMENT_DOMAIN, request, client.public_key)) {
    return decision(state, false, "invalid-signature", auditBase);
  }
  const envelopeSha256 = createHash("sha256")
    .update(Buffer.from(canonicalize(request), "utf8"))
    .digest("hex");
  const existing = state.recent_requests.find(
    (candidate) => candidate.request_id === request.request_id,
  );
  if (existing) {
    if (
      existing.client_id === request.client_id &&
      existing.envelope_sha256 === envelopeSha256
    ) {
      return {
        accepted: true,
        idempotent: true,
        reason: "authorized",
        state,
        authorization: existing,
      };
    }
    return decision(state, false, "request-id-collision", auditBase);
  }
  const temporalReason = temporalRejection(state, request, nowEpochMs);
  if (temporalReason) {
    return decision(state, false, temporalReason, auditBase);
  }
  if (
    state.recent_requests.some((request) => request.status === "authorized")
  ) {
    return decision(state, false, "request-in-progress", auditBase);
  }
  if (request.sequence <= client.last_request_sequence) {
    return decision(state, false, "replay", auditBase);
  }
  if (request.bundle_byte_length > state.policy.max_bundle_bytes) {
    return decision(state, false, "bundle-too-large", auditBase);
  }

  const authorization = {
    request_id: request.request_id,
    client_id: request.client_id,
    sequence: request.sequence,
    envelope_sha256: envelopeSha256,
    bundle_sha256: request.bundle_sha256,
    status: "authorized",
    deployment_generation: null,
    failure_code: null,
  };
  const clients = state.clients.map((candidate) =>
    candidate.client_id === request.client_id
      ? { ...candidate, last_request_sequence: request.sequence }
      : candidate,
  );
  const authorizedState = {
    ...state,
    clients,
    recent_requests: [...state.recent_requests, authorization].slice(
      -state.policy.max_recent_requests,
    ),
  };
  const result = await decision(
    authorizedState,
    true,
    "authorized",
    auditBase,
  );
  return { ...result, authorization };
}

export async function revokePairedClient(
  state,
  clientId,
  { nowEpochMs, physicalAuthorization },
) {
  await validateDeploymentAuthorizationState(state);
  assertSafeTimestamp(nowEpochMs, "current time");
  assert.equal(
    typeof physicalAuthorization,
    "boolean",
    "physical authorization must be Boolean",
  );
  const auditBase = {
    event_type: "client-revocation",
    client_id: clientId,
    request_id: null,
    occurred_at_epoch_ms: nowEpochMs,
  };
  const client = state.clients.find(
    (candidate) => candidate.client_id === clientId,
  );
  if (!client) return decision(state, false, "unknown-client", auditBase);
  if (!physicalAuthorization) {
    return decision(
      state,
      false,
      "physical-authorization-required",
      auditBase,
    );
  }
  const revokedState = {
    ...state,
    clients: state.clients.filter(
      (candidate) => candidate.client_id !== clientId,
    ),
    recent_requests: state.recent_requests.filter(
      (request) => request.client_id !== clientId,
    ),
  };
  return decision(revokedState, true, "revoked", auditBase);
}

export async function recordDeploymentResult(
  state,
  requestId,
  { succeeded, deploymentGeneration = null, nowEpochMs },
) {
  await validateDeploymentAuthorizationState(state);
  assertSafeTimestamp(nowEpochMs, "current time");
  assert.equal(typeof succeeded, "boolean", "succeeded must be Boolean");
  const existing = state.recent_requests.find(
    (request) => request.request_id === requestId,
  );
  assert(existing, "deployment result references an unknown request");
  const requestedStatus = succeeded ? "succeeded" : "failed";
  if (existing.status !== "authorized") {
    assert.equal(
      existing.status,
      requestedStatus,
      "deployment request already has a different terminal result",
    );
    if (succeeded) {
      assert.equal(
        existing.deployment_generation,
        deploymentGeneration,
        "deployment request already has a different generation",
      );
    }
    return { state, idempotent: true, request: existing };
  }
  if (succeeded) {
    assert(
      Number.isSafeInteger(deploymentGeneration) && deploymentGeneration > 0,
      "successful deployment generation must be a safe positive integer",
    );
  } else {
    assert.equal(
      deploymentGeneration,
      null,
      "failed deployment cannot record a generation",
    );
  }
  const terminalRequest = {
    ...existing,
    status: requestedStatus,
    deployment_generation: succeeded ? deploymentGeneration : null,
    failure_code: succeeded ? null : "deployment-failed",
  };
  const terminalState = {
    ...state,
    recent_requests: state.recent_requests.map((request) =>
      request.request_id === requestId ? terminalRequest : request,
    ),
  };
  const resultState = appendAudit(terminalState, {
    event_type: "deployment-result",
    client_id: existing.client_id,
    request_id: requestId,
    result: succeeded ? "succeeded" : "failed",
    reason: succeeded ? "deployment-succeeded" : "deployment-failed",
    occurred_at_epoch_ms: nowEpochMs,
  });
  await validateDeploymentAuthorizationState(resultState);
  return {
    state: resultState,
    idempotent: false,
    request: terminalRequest,
  };
}
