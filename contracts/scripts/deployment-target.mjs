import assert from "node:assert/strict";
import { randomUUID } from "node:crypto";
import {
  lstat,
  mkdir,
  open,
  readFile,
  realpath,
  rename,
  rm,
  stat,
  statfs,
  unlink,
} from "node:fs/promises";
import { basename, dirname, isAbsolute, join, relative, resolve, sep } from "node:path";
import { fileURLToPath, pathToFileURL } from "node:url";

import Ajv2020 from "ajv/dist/2020.js";
import canonicalize from "canonicalize";
import semver from "semver";

import {
  approvePairingRequest,
  authorizeDeploymentRequest,
  issuePairingChallenge,
  jwkThumbprint,
  recordDeploymentResult,
  revokePairedClient,
  validateDeploymentAuthorizationState,
  validateInitialDeploymentAuthorizationState,
} from "../lib/deployment-authorization.mjs";
import {
  assertRetainedDirectory,
  openRetainedDirectory,
  readStableFile,
  syncDirectoryTree,
  syncPublishedDirectory,
  syncPublishedEntry,
  syncRemovedEntry,
} from "../lib/durable-files.mjs";
import { strictParse } from "../lib/strict-json.mjs";
import { inspectDeploymentBundle } from "../lib/deployment-bundle.mjs";
import { readBoundedDirectory } from "../lib/filesystem-tree.mjs";
import {
  removeRetainedDirectory as removeRetainedDirectoryNative,
  renameDirectoryBetweenNoReplace,
} from "../lib/native-fs-authority.mjs";
import { copyVerifiedBundleFile } from "../lib/verified-bundle-copy.mjs";

const contractRoot = resolve(dirname(fileURLToPath(import.meta.url)), "..");

const SHA256_PATTERN = /^[0-9a-f]{64}$/;
const MAX_DEPLOYMENT_METADATA_BYTES = 64 * 1024;
const MAX_AUTHORIZATION_STATE_BYTES = 1024 * 1024;
const MAX_TRANSACTION_RESIDUES = 4096;
const MAX_TARGET_ROOT_ENTRIES = MAX_TRANSACTION_RESIDUES + 8;
const MAX_TARGET_ROOT_NAME_BYTES = MAX_TARGET_ROOT_ENTRIES * 1024;
const DEPLOYMENT_FAULT_BOUNDARIES = new Set([
  "after-authorization-publication",
  "after-authorization-sync",
  "after-authorization-state-publication",
  "after-deployment-publication-before-result",
  "after-staging-copy",
  "after-staging-verification",
  "after-bundle-publication",
  "after-bundle-installation",
  "after-state-sync",
  "after-state-publication",
]);

class InjectedDeploymentFault extends Error {
  constructor(boundary) {
    super(`injected deployment fault at ${boundary}`);
    this.code = "HYPER_INJECTED_DEPLOYMENT_FAULT";
    this.boundary = boundary;
  }
}

function validateFaultBoundary(faultAt) {
  assert(
    faultAt === undefined || DEPLOYMENT_FAULT_BOUNDARIES.has(faultAt),
    "faultAt must name a supported deployment transaction boundary",
  );
  return faultAt;
}

function validateDeploymentObserver(observer) {
  assert(
    observer === undefined || typeof observer === "function",
    "deployment observer must be a function",
  );
  return observer ?? (async () => {});
}

function injectFault(faultAt, boundary) {
  if (faultAt === boundary) {
    throw new InjectedDeploymentFault(boundary);
  }
}

async function createDeploymentValidators() {
  const schemas = await Promise.all(
    [
      "common.schema.json",
      "deployment-state.schema.json",
      "deployment-storage-policy.schema.json",
      "deployment-lock.schema.json",
      "deployment-authorization-state.schema.json",
    ].map(async (name) =>
      strictParse(
        await readFile(resolve(contractRoot, "schemas", "v1", name), "utf8"),
      ),
    ),
  );
  const ajv = new Ajv2020({ allErrors: true, strict: true });
  schemas.forEach((schema) => ajv.addSchema(schema));
  return {
    state: ajv.getSchema(schemas[1].$id),
    policy: ajv.getSchema(schemas[2].$id),
    lock: ajv.getSchema(schemas[3].$id),
    authorization: ajv.getSchema(schemas[4].$id),
  };
}

const deploymentValidators = createDeploymentValidators();

function retainedDirectoryPath(handle, ambientPath) {
  if (process.platform === "linux") {
    assert(Number.isInteger(handle.fd), "retained directory handle is closed");
    return `/proc/self/fd/${handle.fd}`;
  }
  return ambientPath;
}

async function withRetainedDeploymentLock(
  targetRoot,
  authorityRoot,
  targetHandle,
  operation,
) {
  const lockPath = join(authorityRoot, "deployment.lock");
  const ownerPath = join(lockPath, "owner.json");
  const owner = {
    format: "hyper-deployment-lock",
    format_version: 1,
    owner_pid: process.pid,
    owner_token: randomUUID(),
    extensions: {},
  };
  const ownerBytes = Buffer.from(canonicalize(owner), "utf8");
  try {
    await mkdir(lockPath, { mode: 0o700 });
  } catch (error) {
    if (error.code === "EEXIST") {
      throw new Error("another deployment transaction is active");
    }
    throw error;
  }

  let lockHandle;
  let ownerHandle;
  try {
    lockHandle = await openRetainedDirectory(lockPath);
    ownerHandle = await open(ownerPath, "wx", 0o600);
    await ownerHandle.writeFile(ownerBytes);
    await ownerHandle.sync();
    await lockHandle.sync();
    await assertRetainedDirectory(
      targetHandle,
      targetRoot,
      "deployment target directory",
    );
    await assertRetainedDirectory(
      lockHandle,
      lockPath,
      "deployment lock directory",
    );
    await targetHandle.sync();
    await ownerHandle.close();
    ownerHandle = undefined;
  } catch (error) {
    let cleanupError;
    try {
      await ownerHandle?.close();
      if (lockHandle) {
        await assertRetainedDirectory(
          lockHandle,
          lockPath,
          "deployment lock directory",
        );
        await removeRetainedDirectoryNative(
          lockHandle,
          lockPath,
          "deployment lock directory",
        );
        await targetHandle.sync();
      }
    } catch (source) {
      cleanupError = source;
    } finally {
      await Promise.allSettled([ownerHandle?.close(), lockHandle?.close()]);
    }
    if (cleanupError) {
      throw new AggregateError(
        [error, cleanupError],
        "deployment lock initialization and cleanup both failed",
      );
    }
    throw error;
  }
  let result;
  let operationError;
  try {
    result = await operation(authorityRoot, targetHandle);
  } catch (error) {
    operationError = error;
  }

  let releaseError;
  try {
    const publishedOwner = await readStableFile(
      ownerPath,
      MAX_DEPLOYMENT_METADATA_BYTES,
      "deployment lock",
    );
    assert(
      publishedOwner.equals(ownerBytes),
      "deployment lock ownership was compromised",
    );
    await assertRetainedDirectory(
      lockHandle,
      lockPath,
      "deployment lock directory",
    );
    await removeRetainedDirectoryNative(
      lockHandle,
      lockPath,
      "deployment lock directory",
    );
    await targetHandle.sync();
    await assertRetainedDirectory(
      targetHandle,
      targetRoot,
      "deployment target directory",
    );
  } catch (error) {
    releaseError = error;
  } finally {
    await lockHandle.close();
  }

  if (operationError && releaseError) {
    throw new AggregateError(
      [operationError, releaseError],
      "deployment transaction and lock release both failed",
    );
  }
  if (operationError) throw operationError;
  if (releaseError) throw releaseError;
  return result;
}

async function withTargetAuthority(targetRoot, operation) {
  const targetHandle = await openRetainedDirectory(targetRoot);
  const authorityRoot = retainedDirectoryPath(targetHandle, targetRoot);
  let result;
  let operationError;
  try {
    await assertRetainedDirectory(
      targetHandle,
      targetRoot,
      "deployment target directory",
    );
    result = await operation(authorityRoot, targetHandle);
  } catch (error) {
    operationError = error;
  }

  let releaseError;
  try {
    await targetHandle.close();
  } catch (error) {
    releaseError = error;
  }
  if (operationError && releaseError) {
    throw new AggregateError(
      [operationError, releaseError],
      "deployment transaction and target authority release both failed",
    );
  }
  if (operationError) throw operationError;
  if (releaseError) throw releaseError;
  return result;
}

async function withDeploymentLock(targetRoot, operation) {
  return withTargetAuthority(targetRoot, (authorityRoot, targetHandle) =>
    withRetainedDeploymentLock(
      targetRoot,
      authorityRoot,
      targetHandle,
      operation,
    ),
  );
}

async function inspectBundle(bundlePath, options = {}) {
  const inspected = await inspectDeploymentBundle(bundlePath, options);
  return {
    manifest: inspected.manifest,
    sha256: inspected.bundle_sha256,
    byte_length: inspected.bundle_byte_length,
  };
}

async function readCanonicalValidatedFile(
  path,
  validate,
  label,
  missingValue,
  maxBytes = MAX_DEPLOYMENT_METADATA_BYTES,
) {
  let bytes;
  try {
    bytes = await readStableFile(path, maxBytes, label);
  } catch (error) {
    if (error.code === "ENOENT") return missingValue;
    throw error;
  }
  const value = strictParse(
    new TextDecoder("utf-8", { fatal: true }).decode(bytes),
  );
  assert(
    bytes.equals(Buffer.from(canonicalize(value), "utf8")),
    `${label} is not canonical RFC 8785 JSON`,
  );
  assert(
    validate(value),
    `${label} schema validation failed: ${JSON.stringify(validate.errors)}`,
  );
  return value;
}

async function writeCanonicalFile(
  targetRoot,
  name,
  value,
  faultAt = undefined,
  maxBytes = MAX_DEPLOYMENT_METADATA_BYTES,
) {
  const temporaryPath = join(targetRoot, `.${name}-${randomUUID()}.json`);
  const bytes = Buffer.from(canonicalize(value), "utf8");
  assert(bytes.byteLength <= maxBytes, `${name} exceeds ${maxBytes} bytes`);
  const handle = await open(temporaryPath, "wx");
  try {
    await handle.writeFile(bytes);
    await handle.sync();
    if (name === "state") injectFault(faultAt, "after-state-sync");
    if (name === "authorization") {
      injectFault(faultAt, "after-authorization-sync");
    }
  } finally {
    await handle.close();
  }
  try {
    const publishedPath = join(targetRoot, `${name}.json`);
    await rename(temporaryPath, publishedPath);
    await syncPublishedEntry(publishedPath);
    if (name === "state") injectFault(faultAt, "after-state-publication");
    if (name === "authorization") {
      injectFault(faultAt, "after-authorization-state-publication");
    }
  } finally {
    await rm(temporaryPath, { force: true });
  }
}

async function readState(targetRoot) {
  const { state: validate } = await deploymentValidators;
  return readCanonicalValidatedFile(
    join(targetRoot, "state.json"),
    validate,
    "deployment state",
    null,
  );
}

async function writeState(targetRoot, state, faultAt = undefined) {
  const { state: validate } = await deploymentValidators;
  assert(
    validate(state),
    `deployment state schema validation failed: ${JSON.stringify(validate.errors)}`,
  );
  await writeCanonicalFile(targetRoot, "state", state, faultAt);
}

function nextDeploymentGeneration(current) {
  const generation = current?.generation ?? 0;
  assert(
    generation < Number.MAX_SAFE_INTEGER,
    "deployment generation is exhausted",
  );
  return generation + 1;
}

async function readStoragePolicy(targetRoot) {
  const { policy: validate } = await deploymentValidators;
  return readCanonicalValidatedFile(
    join(targetRoot, "policy.json"),
    validate,
    "deployment storage policy",
    null,
  );
}

async function readLockOwner(targetRoot) {
  const lockPath = join(targetRoot, "deployment.lock");
  let lockHandle;
  try {
    lockHandle = await openRetainedDirectory(lockPath);
  } catch (error) {
    if (error.code === "ENOENT") return null;
    throw error;
  }
  const { lock: validate } = await deploymentValidators;
  try {
    const owner = await readCanonicalValidatedFile(
      join(lockPath, "owner.json"),
      validate,
      "deployment lock",
      null,
    );
    assert(owner, "deployment lock owner is missing");
    await assertRetainedDirectory(
      lockHandle,
      lockPath,
      "deployment lock directory",
    );
    return owner;
  } finally {
    await lockHandle.close();
  }
}

async function readAuthorizationState(targetRoot) {
  const { authorization: validate } = await deploymentValidators;
  const state = await readCanonicalValidatedFile(
    join(targetRoot, "authorization.json"),
    validate,
    "deployment authorization state",
    null,
    MAX_AUTHORIZATION_STATE_BYTES,
  );
  if (state) await validateDeploymentAuthorizationState(state);
  return state;
}

async function writeAuthorizationState(
  targetRoot,
  state,
  faultAt = undefined,
) {
  await validateDeploymentAuthorizationState(state);
  await writeCanonicalFile(
    targetRoot,
    "authorization",
    state,
    faultAt,
    MAX_AUTHORIZATION_STATE_BYTES,
  );
}

async function assertDirectDeploymentAllowed(targetRoot) {
  assert(
    !(await readAuthorizationState(targetRoot)),
    "direct deployment is disabled after authorization is configured",
  );
}

async function requireTarget(targetPath) {
  const targetRoot = resolve(targetPath);
  const status = await lstat(targetRoot);
  assert(
    status.isDirectory() && !status.isSymbolicLink(),
    "deployment target must be a real directory",
  );
  await mkdir(join(targetRoot, "bundles"), { recursive: true });
  await requireBundlesRoot(targetRoot);
  return targetRoot;
}

async function requireBundlesRoot(targetRoot) {
  const targetStatus =
    process.platform === "linux" &&
    /^\/proc\/self\/fd\/[0-9]+$/.test(targetRoot)
      ? await stat(targetRoot)
      : await lstat(targetRoot);
  assert(
    targetStatus.isDirectory() && !targetStatus.isSymbolicLink(),
    "deployment target must remain a real directory",
  );
  const bundlesRoot = join(targetRoot, "bundles");
  const bundlesStatus = await lstat(bundlesRoot);
  assert(
    bundlesStatus.isDirectory() && !bundlesStatus.isSymbolicLink(),
    "deployment bundles directory must be a real directory",
  );
  const [targetRealPath, bundlesRealPath] = await Promise.all([
    realpath(targetRoot),
    realpath(bundlesRoot),
  ]);
  const relativePath = relative(targetRealPath, bundlesRealPath);
  assert(
    relativePath &&
      relativePath !== ".." &&
      !relativePath.startsWith(`..${sep}`) &&
      !isAbsolute(relativePath),
    "deployment bundles directory escapes the target",
  );
  return bundlesRoot;
}

async function retainDeploymentDirectories(targetRoot, targetHandle) {
  const bundlesPath = await requireBundlesRoot(targetRoot);
  await assertRetainedDirectory(
    targetHandle,
    targetRoot,
    "deployment target directory",
  );
  const bundlesHandle = await openRetainedDirectory(bundlesPath);
  try {
    await assertRetainedDirectory(
      bundlesHandle,
      bundlesPath,
      "deployment bundles directory",
    );
    return {
      bundlesHandle,
      bundlesPublishedRoot: bundlesPath,
      bundlesRoot: retainedDirectoryPath(bundlesHandle, bundlesPath),
      targetHandle,
    };
  } catch (error) {
    await bundlesHandle.close();
    throw error;
  }
}

async function assertDeploymentDirectoriesRetained(targetRoot, retained) {
  await Promise.all([
    assertRetainedDirectory(
      retained.targetHandle,
      targetRoot,
      "deployment target directory",
    ),
    assertRetainedDirectory(
      retained.bundlesHandle,
      retained.bundlesPublishedRoot,
      "deployment bundles directory",
    ),
  ]);
}

async function closeRetainedDeploymentDirectories(retained) {
  await retained.bundlesHandle.close();
}

async function removeRetainedDirectory(path, parentRoot, handle, label) {
  const quarantinePath = join(parentRoot, `.delete-${randomUUID()}`);
  await rename(path, quarantinePath);
  try {
    await assertRetainedDirectory(handle, quarantinePath, label);
    await removeRetainedDirectoryNative(handle, quarantinePath, label);
    await syncRemovedEntry(parentRoot);
  } catch (error) {
    let restoreError;
    try {
      await rename(quarantinePath, path);
    } catch (source) {
      restoreError = source;
    }
    if (restoreError) {
      throw new AggregateError(
        [error, restoreError],
        `${label} cleanup and quarantine restoration both failed`,
      );
    }
    throw error;
  }
}

async function withRetainedDeploymentDirectories(retained, operation) {
  let result;
  let operationError;
  try {
    result = await operation();
  } catch (error) {
    operationError = error;
  }
  let releaseError;
  try {
    await closeRetainedDeploymentDirectories(retained);
  } catch (error) {
    releaseError = error;
  }
  if (operationError && releaseError) {
    throw new AggregateError(
      [operationError, releaseError],
      "deployment installation and directory authority release both failed",
    );
  }
  if (operationError) throw operationError;
  if (releaseError) throw releaseError;
  return result;
}

function protectedBundleIdentities(state) {
  return new Set(
    state
      ? [
          state.active.bundle_sha256,
          ...(state.previous_bundle_sha256
            ? [state.previous_bundle_sha256]
            : []),
        ]
      : [],
  );
}

async function inspectInstalledBundles(targetRoot, maximumBundles) {
  const bundlesRoot = await requireBundlesRoot(targetRoot);
  const entries = await readBoundedDirectory(bundlesRoot, {
    maximumEntries: maximumBundles,
    maximumNameBytes: maximumBundles * 64,
    label: "installed bundle directory",
  });
  const bundles = [];
  for (const entry of entries.sort((left, right) =>
    left.name.localeCompare(right.name),
  )) {
    assert(
      SHA256_PATTERN.test(entry.name),
      `invalid installed bundle entry: ${entry.name}`,
    );
    assert(
      entry.isDirectory() && !entry.isSymbolicLink(),
      `installed bundle entry is not a real directory: ${entry.name}`,
    );
    const bundle = await inspectBundle(join(bundlesRoot, entry.name));
    assert.equal(
      bundle.sha256,
      entry.name,
      `installed bundle identity mismatch: ${entry.name}`,
    );
    bundles.push(bundle);
  }
  return bundles;
}

function summarizeStorage(bundles, protectedIdentities) {
  let installedBytes = 0;
  for (const bundle of bundles) {
    installedBytes += bundle.byte_length;
    assert(
      Number.isSafeInteger(installedBytes),
      "installed bundle byte accounting exceeds safe integer range",
    );
  }
  return {
    installed_bundle_count: bundles.length,
    installed_bytes: installedBytes,
    protected_bundle_sha256: bundles
      .filter((bundle) => protectedIdentities.has(bundle.sha256))
      .map((bundle) => bundle.sha256),
  };
}

async function availableBytes(targetRoot) {
  const status = await statfs(targetRoot, { bigint: true });
  const bytes = status.bavail * status.bsize;
  return bytes > BigInt(Number.MAX_SAFE_INTEGER)
    ? Number.MAX_SAFE_INTEGER
    : Number(bytes);
}

function assertPolicyCapacity(policy, usage, freeBytes, incomingBytes = 0) {
  assert(
    usage.installed_bundle_count + (incomingBytes > 0 ? 1 : 0) <=
      policy.max_installed_bundles,
    "deployment storage bundle-count quota exceeded",
  );
  assert(
    usage.installed_bytes + incomingBytes <= policy.max_installed_bytes,
    "deployment storage byte quota exceeded",
  );
  assert(
    freeBytes - incomingBytes >= policy.minimum_free_bytes,
    "deployment storage free-space reserve would be violated",
  );
}

async function assertStorageWithinPolicy(targetRoot) {
  const policy = await readStoragePolicy(targetRoot);
  assert(policy, "deployment storage policy is not configured");
  const state = await readState(targetRoot);
  const bundles = await inspectInstalledBundles(
    targetRoot,
    policy.max_installed_bundles,
  );
  assertPolicyCapacity(
    policy,
    summarizeStorage(bundles, protectedBundleIdentities(state)),
    await availableBytes(targetRoot),
  );
}

async function collectUnprotectedBundles(targetRoot, policy) {
  const state = await readState(targetRoot);
  const protectedIdentities = protectedBundleIdentities(state);
  const bundles = await inspectInstalledBundles(
    targetRoot,
    policy.max_installed_bundles,
  );
  const removed = [];
  const retained = [];
  for (const bundle of bundles) {
    if (protectedIdentities.has(bundle.sha256)) {
      retained.push(bundle);
      continue;
    }
    const bundlesRoot = await requireBundlesRoot(targetRoot);
    const bundlePath = join(bundlesRoot, bundle.sha256);
    const quarantinePath = join(bundlesRoot, `.garbage-${randomUUID()}`);
    await rename(bundlePath, quarantinePath);
    let quarantineHandle;
    try {
      quarantineHandle = await openRetainedDirectory(quarantinePath);
      const inspected = await inspectBundle(quarantinePath, {
        maximumBundleBytes: policy.max_installed_bytes,
        expectedBundleSha256: bundle.sha256,
        expectedBundleByteLength: bundle.byte_length,
      });
      assert.equal(inspected.sha256, bundle.sha256);
      await removeRetainedDirectory(
        quarantinePath,
        bundlesRoot,
        quarantineHandle,
        `unprotected bundle ${bundle.sha256}`,
      );
    } catch (error) {
      try {
        await rename(quarantinePath, bundlePath);
      } catch (restoreError) {
        if (restoreError.code !== "ENOENT") {
          throw new AggregateError(
            [error, restoreError],
            "bundle collection and restoration both failed",
          );
        }
      }
      throw error;
    } finally {
      await quarantineHandle?.close();
    }
    removed.push({
      bundle_sha256: bundle.sha256,
      byte_length: bundle.byte_length,
    });
  }
  return {
    removed,
    bundles: retained,
    protectedIdentities,
  };
}

export async function configureDeploymentStorage(targetPath, policy) {
  const targetRoot = await requireTarget(targetPath);
  return withDeploymentLock(targetRoot, async (authorityRoot) => {
    const { policy: validate } = await deploymentValidators;
    assert(
      validate(policy),
      `deployment storage policy schema validation failed: ${JSON.stringify(validate.errors)}`,
    );
    const authorizationState = await readAuthorizationState(authorityRoot);
    if (authorizationState) {
      assert(
        authorizationState.policy.max_bundle_bytes <=
          policy.max_installed_bytes,
        "deployment storage byte quota is below authorization bundle limit",
      );
    }
    const state = await readState(authorityRoot);
    const protectedIdentities = protectedBundleIdentities(state);
    const bundles = await inspectInstalledBundles(
      authorityRoot,
      policy.max_installed_bundles,
    );
    const usage = summarizeStorage(bundles, protectedIdentities);
    assertPolicyCapacity(policy, usage, await availableBytes(authorityRoot));
    await writeCanonicalFile(authorityRoot, "policy", policy);
    return policy;
  });
}

export async function readDeploymentStorage(targetPath) {
  const targetRoot = await requireTarget(targetPath);
  return withDeploymentLock(targetRoot, async (authorityRoot) => {
    const policy = await readStoragePolicy(authorityRoot);
    assert(policy, "deployment storage policy is not configured");
    const state = await readState(authorityRoot);
    const bundles = await inspectInstalledBundles(
      authorityRoot,
      policy.max_installed_bundles,
    );
    return {
      policy,
      ...summarizeStorage(bundles, protectedBundleIdentities(state)),
      available_bytes: await availableBytes(authorityRoot),
    };
  });
}

export async function collectDeploymentGarbage(targetPath) {
  const targetRoot = await requireTarget(targetPath);
  return withDeploymentLock(targetRoot, async (authorityRoot) => {
    const policy = await readStoragePolicy(authorityRoot);
    assert(policy, "deployment storage policy is not configured");
    const collected = await collectUnprotectedBundles(authorityRoot, policy);
    const usage = summarizeStorage(
      collected.bundles,
      collected.protectedIdentities,
    );
    assertPolicyCapacity(policy, usage, await availableBytes(authorityRoot));
    return {
      removed: collected.removed,
      ...usage,
    };
  });
}

export async function configureDeploymentAuthorization(targetPath, state) {
  const targetRoot = await requireTarget(targetPath);
  return withDeploymentLock(targetRoot, async (authorityRoot) => {
    await validateInitialDeploymentAuthorizationState(state);
    const storagePolicy = await readStoragePolicy(authorityRoot);
    assert(storagePolicy, "deployment storage policy is not configured");
    assert(
      state.policy.max_bundle_bytes <= storagePolicy.max_installed_bytes,
      "authorization bundle limit exceeds deployment storage byte quota",
    );
    try {
      await lstat(join(authorityRoot, "authorization.json"));
      assert.fail("deployment authorization is already configured");
    } catch (error) {
      if (error.code !== "ENOENT") throw error;
    }
    await writeAuthorizationState(authorityRoot, state);
    return state;
  });
}

export async function readDeploymentAuthorization(targetPath) {
  const targetRoot = await requireTarget(targetPath);
  return withDeploymentLock(targetRoot, (authorityRoot) =>
    readAuthorizationState(authorityRoot),
  );
}

export async function openDeploymentPairing(targetPath, options) {
  const targetRoot = await requireTarget(targetPath);
  return withDeploymentLock(targetRoot, async (authorityRoot) => {
    const state = await readAuthorizationState(authorityRoot);
    assert(state, "deployment authorization is not configured");
    const result = await issuePairingChallenge(state, options);
    await writeAuthorizationState(authorityRoot, result.state);
    return result;
  });
}

export async function approveDeploymentPairing(
  targetPath,
  request,
  options,
) {
  const targetRoot = await requireTarget(targetPath);
  return withDeploymentLock(targetRoot, async (authorityRoot) => {
    const state = await readAuthorizationState(authorityRoot);
    assert(state, "deployment authorization is not configured");
    const result = await approvePairingRequest(state, request, options);
    if (!result.idempotent) {
      await writeAuthorizationState(authorityRoot, result.state);
    }
    return result;
  });
}

export async function revokeDeploymentClient(
  targetPath,
  clientId,
  options,
) {
  const targetRoot = await requireTarget(targetPath);
  return withDeploymentLock(targetRoot, async (authorityRoot) => {
    const state = await readAuthorizationState(authorityRoot);
    assert(state, "deployment authorization is not configured");
    const result = await revokePairedClient(state, clientId, options);
    await writeAuthorizationState(authorityRoot, result.state);
    return result;
  });
}

export async function readDeploymentLock(targetPath) {
  const targetRoot = await requireTarget(targetPath);
  return readLockOwner(targetRoot);
}

function defaultProcessIsAlive(ownerPid) {
  try {
    process.kill(ownerPid, 0);
    return true;
  } catch (error) {
    if (error.code === "ESRCH") return false;
    if (error.code === "EPERM") return true;
    throw error;
  }
}

const UUID_FRAGMENT =
  "[0-9a-f]{8}-[0-9a-f]{4}-[0-9a-f]{4}-[0-9a-f]{4}-[0-9a-f]{12}";
const INSTALL_RESIDUE_PATTERN = new RegExp(`^\\.install-${UUID_FRAGMENT}$`);
const FILE_RESIDUE_PATTERN = new RegExp(
  `^\\.(?:state|policy|authorization)-${UUID_FRAGMENT}\\.json$`,
);

async function removeTransactionResidue(targetRoot) {
  const removed = [];
  const entries = await readBoundedDirectory(targetRoot, {
    maximumEntries: MAX_TARGET_ROOT_ENTRIES,
    maximumNameBytes: MAX_TARGET_ROOT_NAME_BYTES,
    label: "deployment recovery directory",
  });
  for (const entry of entries.sort((left, right) =>
    left.name.localeCompare(right.name),
  )) {
    const installResidue = INSTALL_RESIDUE_PATTERN.test(entry.name);
    const fileResidue = FILE_RESIDUE_PATTERN.test(entry.name);
    if (!installResidue && !fileResidue) continue;
    const residuePath = join(targetRoot, entry.name);
    const status = await lstat(residuePath);
    if (installResidue) {
      assert(
        status.isDirectory() || status.isSymbolicLink(),
        `install residue has an unsupported file type: ${entry.name}`,
      );
      if (status.isSymbolicLink()) {
        await unlink(residuePath);
      } else {
        const residueHandle = await openRetainedDirectory(residuePath);
        try {
          await removeRetainedDirectory(
            residuePath,
            targetRoot,
            residueHandle,
            `install residue ${entry.name}`,
          );
        } finally {
          await residueHandle.close();
        }
      }
    } else {
      assert(
        status.isFile() || status.isSymbolicLink(),
        `metadata residue has an unsupported file type: ${entry.name}`,
      );
      await unlink(residuePath);
    }
    removed.push(entry.name);
  }
  return removed;
}

export async function recoverDeploymentTarget(
  targetPath,
  { force = false, processIsAlive = defaultProcessIsAlive } = {},
) {
  assert.equal(typeof force, "boolean", "force must be Boolean");
  assert.equal(
    typeof processIsAlive,
    "function",
    "process liveness inspector must be a function",
  );
  const targetRoot = await requireTarget(targetPath);
  return withTargetAuthority(targetRoot, async (authorityRoot, targetHandle) => {
    const lockPath = join(authorityRoot, "deployment.lock");
    let owner = null;
    let forced = false;
    let lockHandle;
    try {
      try {
        lockHandle = await openRetainedDirectory(lockPath);
      } catch (error) {
        if (error.code !== "ENOENT") throw error;
      }
      if (lockHandle) owner = await readLockOwner(authorityRoot);
    } catch (error) {
      if (!force) {
        await lockHandle?.close();
        assert.fail(
          `deployment lock ownership is invalid; explicit force is required: ${error.message}`,
        );
      }
      try {
        const status = await lstat(lockPath);
        assert(
          status.isDirectory() && !status.isSymbolicLink() && lockHandle,
          "deployment lock is not a recoverable directory",
        );
        forced = true;
      } catch (validationError) {
        await lockHandle?.close();
        throw validationError;
      }
    }

    try {
      if (owner) {
        assert.equal(
          await processIsAlive(owner.owner_pid),
          false,
          `deployment lock owner process ${owner.owner_pid} is still active`,
        );
      }
      if (owner || forced) {
        await assertRetainedDirectory(
          lockHandle,
          lockPath,
          "deployment lock directory",
        );
        await removeRetainedDirectoryNative(
          lockHandle,
          lockPath,
          "deployment lock directory",
        );
        await targetHandle.sync();
      }
    } finally {
      await lockHandle?.close();
    }

    return withRetainedDeploymentLock(
      targetRoot,
      authorityRoot,
      targetHandle,
      async () => {
        const removedTemporaryPaths =
          await removeTransactionResidue(authorityRoot);
        const policy = await readStoragePolicy(authorityRoot);
        let storage = null;
        if (policy) {
          const state = await readState(authorityRoot);
          const bundles = await inspectInstalledBundles(
            authorityRoot,
            policy.max_installed_bundles,
          );
          const usage = summarizeStorage(
            bundles,
            protectedBundleIdentities(state),
          );
          assertPolicyCapacity(
            policy,
            usage,
            await availableBytes(authorityRoot),
          );
          storage = usage;
        }
        return {
          recovered_lock_owner: owner,
          forced,
          removed_temporary_paths: removedTemporaryPaths,
          storage,
        };
      },
    );
  });
}

async function installIdentity(
  sourceRoot,
  targetRoot,
  targetHandle,
  {
    faultAt = undefined,
    expectedIdentity = undefined,
    observer = async () => {},
    copyObserver = async () => {},
  } = {},
) {
  assert.equal(typeof copyObserver, "function", "copy observer must be a function");
  const policy = await readStoragePolicy(targetRoot);
  assert(policy, "deployment storage policy is not configured");
  const maximumBundleBytes =
    expectedIdentity?.byte_length ?? policy.max_installed_bytes;
  const inspectionOptions = {
    maximumBundleBytes,
    ...(expectedIdentity
      ? {
          expectedBundleSha256: expectedIdentity.sha256,
          expectedBundleByteLength: expectedIdentity.byte_length,
        }
      : {}),
  };
  const source = await inspectBundle(sourceRoot, inspectionOptions);
  const retained = await retainDeploymentDirectories(targetRoot, targetHandle);
  const { bundlesRoot } = retained;
  const installedPath = join(bundlesRoot, source.sha256);
  return withRetainedDeploymentDirectories(retained, async () => {
    try {
      const status = await lstat(installedPath);
      assert(status.isDirectory(), "installed bundle path is not a directory");
      const installed = await inspectBundle(installedPath, inspectionOptions);
      assert.equal(installed.sha256, source.sha256);
      await assertDeploymentDirectoriesRetained(targetRoot, retained);
      return source.sha256;
    } catch (error) {
      if (error.code !== "ENOENT") throw error;
    }

    const state = await readState(targetRoot);
    const bundles = await inspectInstalledBundles(
      targetRoot,
      policy.max_installed_bundles,
    );
    const usage = summarizeStorage(bundles, protectedBundleIdentities(state));
    assertPolicyCapacity(
      policy,
      usage,
      await availableBytes(targetRoot),
      source.byte_length,
    );

    const stagingPath = join(targetRoot, `.install-${randomUUID()}`);
    await mkdir(stagingPath, { mode: 0o700 });
    const stagingHandle = await openRetainedDirectory(stagingPath);
    let published = false;
    let accepted = false;
    try {
      await copyObserver("before-staging-copy", { sourceRoot, stagingPath });
      const sourceRootReal = await realpath(sourceRoot);
      const payloadBytes = source.manifest.files.reduce(
        (total, descriptor) => total + descriptor.byte_length,
        0,
      );
      const manifestByteLength = source.byte_length - payloadBytes;
      assert(
        Number.isSafeInteger(manifestByteLength) && manifestByteLength > 0,
        "runtime bundle manifest byte length is invalid",
      );
      const descriptors = [
        {
          path: "manifest.json",
          byte_length: manifestByteLength,
          sha256: source.sha256,
        },
        ...source.manifest.files,
      ].sort((left, right) =>
        left.path < right.path ? -1 : left.path > right.path ? 1 : 0,
      );
      let copiedBytes = 0;
      for (const descriptor of descriptors) {
        copiedBytes += await copyVerifiedBundleFile(
          sourceRoot,
          sourceRootReal,
          stagingPath,
          descriptor,
          source.byte_length - copiedBytes,
        );
      }
      assert.equal(
        copiedBytes,
        source.byte_length,
        "staged deployment bundle byte length mismatch",
      );
      injectFault(faultAt, "after-staging-copy");
      const staged = await inspectBundle(stagingPath, inspectionOptions);
      assert.equal(
        staged.sha256,
        source.sha256,
        "staged bundle identity changed during transfer",
      );
      injectFault(faultAt, "after-staging-verification");
      await syncDirectoryTree(stagingPath, [
        "manifest.json",
        ...staged.manifest.files.map((file) => file.path),
      ]);
      const durable = await inspectBundle(stagingPath, inspectionOptions);
      assert.equal(
        durable.sha256,
        source.sha256,
        "staged bundle identity changed before publication",
      );
      await assertDeploymentDirectoriesRetained(targetRoot, retained);
      await observer("before-bundle-publication", { installedPath });
      try {
        await renameDirectoryBetweenNoReplace(
          targetHandle,
          targetRoot,
          basename(stagingPath),
          retained.bundlesHandle,
          bundlesRoot,
          source.sha256,
          "deployment bundle",
        );
        published = true;
      } catch (error) {
        if (error.code !== "EEXIST") throw error;
        const existing = await inspectBundle(installedPath, inspectionOptions);
        assert.equal(
          existing.sha256,
          source.sha256,
          "deployment bundle destination has a different identity",
        );
        await assertDeploymentDirectoriesRetained(targetRoot, retained);
        return source.sha256;
      }
      await assertRetainedDirectory(
        stagingHandle,
        installedPath,
        "published deployment bundle",
      );
      await observer("after-bundle-publication", { installedPath });
      await assertDeploymentDirectoriesRetained(targetRoot, retained);
      const publishedInspection = await inspectBundle(
        installedPath,
        inspectionOptions,
      );
      assert.equal(
        publishedInspection.sha256,
        source.sha256,
        "published bundle identity changed during installation",
      );
      await assertDeploymentDirectoriesRetained(targetRoot, retained);
      await assertRetainedDirectory(
        stagingHandle,
        join(retained.bundlesPublishedRoot, source.sha256),
        "published deployment bundle",
      );
      await syncPublishedDirectory(installedPath);
      injectFault(faultAt, "after-bundle-publication");
      assertPolicyCapacity(
        policy,
        {
          ...usage,
          installed_bundle_count: usage.installed_bundle_count + 1,
          installed_bytes: usage.installed_bytes + source.byte_length,
        },
        await availableBytes(targetRoot),
      );
      accepted = true;
      injectFault(faultAt, "after-bundle-installation");
    } finally {
      try {
        if (!accepted) {
          await removeRetainedDirectory(
            published ? installedPath : stagingPath,
            published ? bundlesRoot : targetRoot,
            stagingHandle,
            published ? "published deployment bundle" : "staged deployment bundle",
          );
        }
      } finally {
        await stagingHandle.close();
      }
    }
    return source.sha256;
  });
}

export async function installBundle(bundlePath, targetPath, options = {}) {
  const faultAt = validateFaultBoundary(options.faultAt);
  const observer = validateDeploymentObserver(options.observer);
  const copyObserver = validateDeploymentObserver(options.copyObserver);
  const sourceRoot = resolve(bundlePath);
  const targetRoot = await requireTarget(targetPath);
  return withDeploymentLock(targetRoot, async (authorityRoot, targetHandle) => {
    await assertDirectDeploymentAllowed(authorityRoot);
    return installIdentity(
      sourceRoot,
      authorityRoot,
      targetHandle,
      { faultAt, observer, copyObserver },
    );
  });
}

async function activateIdentity(
  targetRoot,
  bundleSha256,
  runtimeVersion,
  rollback,
  faultAt = undefined,
) {
  assert(semver.valid(runtimeVersion), "runtime version must be valid SemVer");
  await assertStorageWithinPolicy(targetRoot);
  const installedPath = join(targetRoot, "bundles", bundleSha256);
  const installed = await inspectBundle(installedPath);
  assert.equal(
    installed.sha256,
    bundleSha256,
    "installed bundle identity mismatch",
  );
  assert(
    semver.gte(runtimeVersion, installed.manifest.minimum_runtime_version),
    "bundle requires a newer runtime version",
  );
  const current = await readState(targetRoot);
  if (current?.active.bundle_sha256 === bundleSha256) return current;
  if (
    !rollback &&
    current?.active.project_id === installed.manifest.project_id
  ) {
    assert(
      installed.manifest.project_revision > current.active.project_revision,
      "same-project deployment revision must increase",
    );
  }
  const state = {
    format: "hyper-deployment-state",
    format_version: 1,
    generation: nextDeploymentGeneration(current),
    active: {
      bundle_sha256: bundleSha256,
      project_id: installed.manifest.project_id,
      project_revision: installed.manifest.project_revision,
      minimum_runtime_version: installed.manifest.minimum_runtime_version,
      status: "pending",
    },
    previous_bundle_sha256: current?.active.bundle_sha256 ?? null,
    extensions: {},
  };
  await writeState(targetRoot, state, faultAt);
  return state;
}

export async function activateBundle(
  targetPath,
  bundleSha256,
  runtimeVersion,
  options = {},
) {
  const faultAt = validateFaultBoundary(options.faultAt);
  const targetRoot = await requireTarget(targetPath);
  return withDeploymentLock(targetRoot, async (authorityRoot) => {
    await assertDirectDeploymentAllowed(authorityRoot);
    return activateIdentity(
      authorityRoot,
      bundleSha256,
      runtimeVersion,
      false,
      faultAt,
    );
  });
}

export async function deployBundle(
  bundlePath,
  targetPath,
  runtimeVersion,
  options = {},
) {
  const faultAt = validateFaultBoundary(options.faultAt);
  const observer = validateDeploymentObserver(options.observer);
  const sourceRoot = resolve(bundlePath);
  const targetRoot = await requireTarget(targetPath);
  return withDeploymentLock(targetRoot, async (authorityRoot, targetHandle) => {
    await assertDirectDeploymentAllowed(authorityRoot);
    const identity = await installIdentity(
      sourceRoot,
      authorityRoot,
      targetHandle,
      { faultAt, observer },
    );
    return activateIdentity(
      authorityRoot,
      identity,
      runtimeVersion,
      false,
      faultAt,
    );
  });
}

function authenticatedDeploymentResult(request, terminalRequest, state = null) {
  return {
    request_id: request.request_id,
    bundle_sha256: request.bundle_sha256,
    status: terminalRequest.status,
    deployment_generation: terminalRequest.deployment_generation,
    failure_code: terminalRequest.failure_code,
    deployment_state: state,
  };
}

export async function authenticatedDeployBundle(
  bundlePath,
  targetPath,
  request,
  nowEpochMs,
  options = {},
) {
  const faultAt = validateFaultBoundary(options.faultAt);
  const observer = validateDeploymentObserver(options.observer);
  const sourceRoot = resolve(bundlePath);
  const targetRoot = await requireTarget(targetPath);
  return withDeploymentLock(targetRoot, async (authorityRoot, targetHandle) => {
    const authorizationState = await readAuthorizationState(authorityRoot);
    assert(
      authorizationState,
      "deployment authorization is not configured",
    );
    const authorization = await authorizeDeploymentRequest(
      authorizationState,
      request,
      { nowEpochMs },
    );
    if (!authorization.accepted) {
      await writeAuthorizationState(
        authorityRoot,
        authorization.state,
        faultAt,
      );
      return {
        request_id: request.request_id,
        bundle_sha256: request.bundle_sha256,
        status: "rejected",
        deployment_generation: null,
        failure_code: authorization.reason,
        deployment_state: null,
      };
    }

    const requestRecord = authorization.authorization;
    if (requestRecord.status !== "authorized") {
      return authenticatedDeploymentResult(request, requestRecord);
    }
    if (!authorization.idempotent) {
      await writeAuthorizationState(
        authorityRoot,
        authorization.state,
        faultAt,
      );
    }
    injectFault(faultAt, "after-authorization-publication");

    let deploymentState = null;
    let succeeded = false;
    try {
      const identity = await installIdentity(
        sourceRoot,
        authorityRoot,
        targetHandle,
        {
          faultAt,
          expectedIdentity: {
            sha256: request.bundle_sha256,
            byte_length: request.bundle_byte_length,
          },
          observer,
        },
      );
      assert.equal(
        identity,
        request.bundle_sha256,
        "installed deployment identity does not match the authorized bundle",
      );
      deploymentState = await activateIdentity(
        authorityRoot,
        identity,
        request.runtime_version,
        false,
        faultAt,
      );
      succeeded = true;
    } catch (error) {
      if (error.code === "HYPER_INJECTED_DEPLOYMENT_FAULT") {
        if (error.boundary !== "after-state-publication") throw error;
        const current = await readState(authorityRoot);
        assert.equal(
          current?.active.bundle_sha256,
          request.bundle_sha256,
          "published deployment state does not match the authorized bundle",
        );
        deploymentState = current;
        succeeded = true;
      }
    }

    injectFault(faultAt, "after-deployment-publication-before-result");
    const terminal = await recordDeploymentResult(
      authorization.state,
      request.request_id,
      {
        succeeded,
        deploymentGeneration: succeeded ? deploymentState.generation : null,
        nowEpochMs,
      },
    );
    await writeAuthorizationState(authorityRoot, terminal.state, faultAt);
    return authenticatedDeploymentResult(
      request,
      terminal.request,
      succeeded ? deploymentState : null,
    );
  });
}

export async function rollbackDeployment(targetPath, runtimeVersion) {
  const targetRoot = await requireTarget(targetPath);
  return withDeploymentLock(targetRoot, async (authorityRoot) => {
    await assertDirectDeploymentAllowed(authorityRoot);
    const current = await readState(authorityRoot);
    assert(current, "deployment target has no active bundle");
    assert(
      current.previous_bundle_sha256,
      "deployment target has no previous bundle",
    );
    return activateIdentity(
      authorityRoot,
      current.previous_bundle_sha256,
      runtimeVersion,
      true,
    );
  });
}

export async function readDeploymentState(targetPath) {
  const targetRoot = await requireTarget(targetPath);
  return readState(targetRoot);
}

async function probeInstalledBundle(
  targetRoot,
  targetHandle,
  bundleSha256,
  runtimeVersion,
  probe,
) {
  const retained = await retainDeploymentDirectories(targetRoot, targetHandle);
  const bundlePath = join(retained.bundlesRoot, bundleSha256);
  const publishedBundlePath = join(
    retained.bundlesPublishedRoot,
    bundleSha256,
  );
  let bundleHandle;
  try {
    bundleHandle = await openRetainedDirectory(bundlePath);
    const installed = await inspectBundle(bundlePath);
    assert.equal(
      installed.sha256,
      bundleSha256,
      "installed bundle identity mismatch",
    );
    assert(
      semver.gte(runtimeVersion, installed.manifest.minimum_runtime_version),
      "bundle requires a newer runtime version",
    );
    assert.equal(
      await probe({ manifest: installed.manifest }),
      true,
      "runtime health probe rejected the bundle",
    );
    await assertDeploymentDirectoriesRetained(targetRoot, retained);
    await assertRetainedDirectory(
      bundleHandle,
      publishedBundlePath,
      "probed deployment bundle",
    );
    return installed;
  } finally {
    if (bundleHandle) await bundleHandle.close();
    await closeRetainedDeploymentDirectories(retained);
  }
}

export async function resolveStartupDeployment(
  targetPath,
  runtimeVersion,
  probe,
) {
  assert(semver.valid(runtimeVersion), "runtime version must be valid SemVer");
  assert.equal(typeof probe, "function", "runtime health probe is required");
  const targetRoot = await requireTarget(targetPath);
  return withDeploymentLock(targetRoot, async (authorityRoot, targetHandle) => {
    const current = await readState(authorityRoot);
    assert(current, "deployment target has no active bundle");
    const promotionGeneration =
      current.active.status === "healthy"
        ? null
        : nextDeploymentGeneration(current);
    let activeProbeSucceeded = false;
    let activeError;
    try {
      await probeInstalledBundle(
        authorityRoot,
        targetHandle,
        current.active.bundle_sha256,
        runtimeVersion,
        probe,
      );
      activeProbeSucceeded = true;
    } catch (error) {
      activeError = error;
    }
    if (activeProbeSucceeded) {
      if (current.active.status === "healthy") {
        return { state: current, recovered_from_bundle_sha256: null };
      }
      const healthy = {
        ...current,
        generation: promotionGeneration,
        active: { ...current.active, status: "healthy" },
      };
      await writeState(authorityRoot, healthy);
      return { state: healthy, recovered_from_bundle_sha256: null };
    }
    assert(
      current.previous_bundle_sha256,
      "active bundle failed and no previous bundle is available",
    );
    const recoveryGeneration = nextDeploymentGeneration(current);
    let previous;
    try {
      previous = await probeInstalledBundle(
        authorityRoot,
        targetHandle,
        current.previous_bundle_sha256,
        runtimeVersion,
        probe,
      );
    } catch (previousError) {
      throw new AggregateError(
        [activeError, previousError],
        "active and previous bundles both failed startup health checks",
      );
    }
    const recovered = {
      format: "hyper-deployment-state",
      format_version: 1,
      generation: recoveryGeneration,
      active: {
        bundle_sha256: current.previous_bundle_sha256,
        project_id: previous.manifest.project_id,
        project_revision: previous.manifest.project_revision,
        minimum_runtime_version: previous.manifest.minimum_runtime_version,
        status: "healthy",
      },
      previous_bundle_sha256: current.active.bundle_sha256,
      extensions: {},
    };
    await writeState(authorityRoot, recovered);
    return {
      state: recovered,
      recovered_from_bundle_sha256: current.active.bundle_sha256,
    };
  });
}

const invokedPath = process.argv[1]
  ? pathToFileURL(resolve(process.argv[1])).href
  : undefined;
if (invokedPath === import.meta.url) {
  const [command, ...args] = process.argv.slice(2);
  let result;
  if (command === "deploy") {
    assert.equal(
      args.length,
      3,
      "usage: deployment-target.mjs deploy <bundle> <target> <runtime-version>",
    );
    result = await deployBundle(args[0], args[1], args[2]);
  } else if (command === "install") {
    assert.equal(
      args.length,
      2,
      "usage: deployment-target.mjs install <bundle> <target>",
    );
    result = { bundle_sha256: await installBundle(args[0], args[1]) };
  } else if (command === "activate") {
    assert.equal(
      args.length,
      3,
      "usage: deployment-target.mjs activate <target> <bundle-sha256> <runtime-version>",
    );
    result = await activateBundle(args[0], args[1], args[2]);
  } else if (command === "rollback") {
    assert.equal(
      args.length,
      2,
      "usage: deployment-target.mjs rollback <target> <runtime-version>",
    );
    result = await rollbackDeployment(args[0], args[1]);
  } else if (command === "status") {
    assert.equal(
      args.length,
      1,
      "usage: deployment-target.mjs status <target>",
    );
    result = await readDeploymentState(args[0]);
  } else if (command === "configure-storage") {
    assert.equal(
      args.length,
      2,
      "usage: deployment-target.mjs configure-storage <target> <policy-json>",
    );
    result = await configureDeploymentStorage(
      args[0],
      strictParse(await readFile(args[1], "utf8")),
    );
  } else if (command === "storage") {
    assert.equal(
      args.length,
      1,
      "usage: deployment-target.mjs storage <target>",
    );
    result = await readDeploymentStorage(args[0]);
  } else if (command === "gc") {
    assert.equal(
      args.length,
      1,
      "usage: deployment-target.mjs gc <target>",
    );
    result = await collectDeploymentGarbage(args[0]);
  } else if (command === "lock-status") {
    assert.equal(
      args.length,
      1,
      "usage: deployment-target.mjs lock-status <target>",
    );
    result = await readDeploymentLock(args[0]);
  } else if (command === "recover") {
    assert(
      args.length === 1 || (args.length === 2 && args[1] === "--force"),
      "usage: deployment-target.mjs recover <target> [--force]",
    );
    result = await recoverDeploymentTarget(args[0], {
      force: args[1] === "--force",
    });
  } else if (command === "configure-authorization") {
    assert.equal(
      args.length,
      2,
      "usage: deployment-target.mjs configure-authorization <target> <state-json>",
    );
    result = await configureDeploymentAuthorization(
      args[0],
      strictParse(await readFile(args[1], "utf8")),
    );
  } else if (command === "authorization") {
    assert.equal(
      args.length,
      1,
      "usage: deployment-target.mjs authorization <target>",
    );
    result = await readDeploymentAuthorization(args[0]);
  } else if (command === "pairing-open") {
    assert(
      args.length === 4 && args[3] === "--physical-presence",
      "usage: deployment-target.mjs pairing-open <target> <now-epoch-ms> <lifetime-ms> --physical-presence",
    );
    result = await openDeploymentPairing(args[0], {
      nowEpochMs: Number(args[1]),
      lifetimeMs: Number(args[2]),
      physicalPresence: true,
    });
  } else if (command === "pairing-approve") {
    assert.equal(
      args.length,
      4,
      "usage: deployment-target.mjs pairing-approve <target> <request-json> <now-epoch-ms> <approved-thumbprint>",
    );
    const pairingRequest = strictParse(await readFile(args[1], "utf8"));
    result = await approveDeploymentPairing(
      args[0],
      pairingRequest,
      {
        nowEpochMs: Number(args[2]),
        physicalAuthorization:
          jwkThumbprint(pairingRequest.client_public_key) === args[3],
      },
    );
  } else if (command === "client-revoke") {
    assert(
      args.length === 4 && args[3] === "--physical-authorization",
      "usage: deployment-target.mjs client-revoke <target> <client-id> <now-epoch-ms> --physical-authorization",
    );
    result = await revokeDeploymentClient(args[0], args[1], {
      nowEpochMs: Number(args[2]),
      physicalAuthorization: true,
    });
  } else if (command === "authenticated-deploy") {
    assert.equal(
      args.length,
      4,
      "usage: deployment-target.mjs authenticated-deploy <bundle> <target> <request-json> <now-epoch-ms>",
    );
    result = await authenticatedDeployBundle(
      args[0],
      args[1],
      strictParse(await readFile(args[2], "utf8")),
      Number(args[3]),
    );
  } else {
    assert.fail(
      "usage: deployment-target.mjs <deploy|install|activate|rollback|status|configure-storage|storage|gc|lock-status|recover|configure-authorization|authorization|pairing-open|pairing-approve|client-revoke|authenticated-deploy> ...",
    );
  }
  console.log(canonicalize(result));
}
