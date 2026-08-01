import assert from "node:assert/strict";
import { execFile } from "node:child_process";
import { generateKeyPairSync, randomUUID } from "node:crypto";
import {
  cp,
  mkdir,
  mkdtemp,
  readFile,
  readdir,
  rename,
  rm,
  symlink,
  unlink,
  writeFile,
} from "node:fs/promises";
import { tmpdir } from "node:os";
import { join } from "node:path";
import { fileURLToPath } from "node:url";
import { promisify } from "node:util";

import canonicalize from "canonicalize";

import {
  approvePairingRequest as approvePairingState,
  createDeploymentAuthorizationState,
  ed25519PublicJwk,
  issuePairingChallenge as issuePairingState,
  jwkThumbprint,
  signDeploymentRequest,
  signPairingRequest,
} from "../lib/deployment-authorization.mjs";
import { inspectDeploymentBundle } from "../lib/deployment-bundle.mjs";
import { buildProjectBundle } from "./build-project-bundle.mjs";
import {
  activateBundle,
  approveDeploymentPairing,
  authenticatedDeployBundle,
  collectDeploymentGarbage,
  configureDeploymentAuthorization,
  configureDeploymentStorage,
  deployBundle,
  installBundle,
  openDeploymentPairing,
  readDeploymentAuthorization,
  readDeploymentLock,
  readDeploymentState,
  readDeploymentStorage,
  recoverDeploymentTarget,
  resolveStartupDeployment,
  revokeDeploymentClient,
  rollbackDeployment,
} from "./deployment-target.mjs";
import { createMinimalProject } from "./test-project-fixture.mjs";

const temporaryRoot = await mkdtemp(
  join(tmpdir(), `hyper-deployment-target-${randomUUID()}-`),
);
const execFileAsync = promisify(execFile);
const deploymentTargetScript = fileURLToPath(
  new URL("./deployment-target.mjs", import.meta.url),
);
const storagePolicy = (overrides = {}) => ({
  format: "hyper-deployment-storage-policy",
  format_version: 1,
  max_installed_bundles: 8,
  max_installed_bytes: 1024 * 1024 * 1024,
  minimum_free_bytes: 0,
  extensions: {},
  ...overrides,
});

async function createTarget(root, name, policy = storagePolicy()) {
  const target = join(root, name);
  await mkdir(target);
  await configureDeploymentStorage(target, policy);
  return target;
}

async function writeDeploymentStateFixture(target, state) {
  await writeFile(
    join(target, "state.json"),
    Buffer.from(canonicalize(state), "utf8"),
  );
}

async function writeDeploymentLockFixture(target, owner) {
  const lockPath = join(target, "deployment.lock");
  await mkdir(lockPath);
  await writeFile(
    join(lockPath, "owner.json"),
    typeof owner === "string" ? owner : canonicalize(owner),
  );
}

async function assertGenerationExhaustionIsAtomic(target, operation) {
  const statePath = join(target, "state.json");
  const before = await readFile(statePath);
  await assert.rejects(operation, /deployment generation is exhausted/);
  assert.deepEqual(await readFile(statePath), before);
  assert.deepEqual(
    (await readdir(target)).filter((name) => name.startsWith(".state-")),
    [],
  );
}

try {
  const source7 = await createMinimalProject(temporaryRoot, "source-7", {
    revision: 7,
  });
  const source8 = await createMinimalProject(temporaryRoot, "source-8", {
    revision: 8,
  });
  const bundle7 = join(temporaryRoot, "revision-7.runtime");
  const bundle8 = join(temporaryRoot, "revision-8.runtime");
  await buildProjectBundle(source7.root, bundle7, "1.0.0");
  await buildProjectBundle(source8.root, bundle8, "1.0.0");
  const target = await createTarget(temporaryRoot, "target");

  const identity7 = await installBundle(bundle7, target);
  assert.equal(await readDeploymentState(target), null);
  const state1 = await activateBundle(target, identity7, "1.0.0");
  assert.equal(state1.generation, 1);
  assert.equal(state1.previous_bundle_sha256, null);

  const repeated = await deployBundle(bundle7, target, "1.0.0");
  assert.deepEqual(repeated, state1);

  const state2 = await deployBundle(bundle8, target, "1.0.0");
  assert.equal(state2.generation, 2);
  assert.equal(state2.previous_bundle_sha256, identity7);
  await assert.rejects(
    deployBundle(bundle7, target, "1.0.0"),
    /revision must increase/,
  );

  const rolledBack = await rollbackDeployment(target, "1.0.0");
  assert.equal(rolledBack.generation, 3);
  assert.equal(rolledBack.active.bundle_sha256, identity7);
  assert.equal(rolledBack.previous_bundle_sha256, state2.active.bundle_sha256);

  const generationTarget = await createTarget(
    temporaryRoot,
    "generation-target",
  );
  await installBundle(bundle7, generationTarget);
  await installBundle(bundle8, generationTarget);
  await writeDeploymentStateFixture(generationTarget, {
    ...state1,
    generation: Number.MAX_SAFE_INTEGER,
    active: { ...state1.active, status: "healthy" },
  });
  await assertGenerationExhaustionIsAtomic(generationTarget, () =>
    activateBundle(
      generationTarget,
      state2.active.bundle_sha256,
      "1.0.0",
    ),
  );

  await writeDeploymentStateFixture(generationTarget, {
    ...state1,
    generation: Number.MAX_SAFE_INTEGER,
    active: { ...state1.active, status: "pending" },
  });
  let exhaustedPromotionProbeCalls = 0;
  await assertGenerationExhaustionIsAtomic(generationTarget, () =>
    resolveStartupDeployment(generationTarget, "1.0.0", async () => {
      exhaustedPromotionProbeCalls += 1;
      return true;
    }),
  );
  assert.equal(exhaustedPromotionProbeCalls, 0);

  await writeDeploymentStateFixture(generationTarget, {
    ...state2,
    generation: Number.MAX_SAFE_INTEGER,
    active: { ...state2.active, status: "healthy" },
    previous_bundle_sha256: identity7,
  });
  await assertGenerationExhaustionIsAtomic(generationTarget, () =>
    rollbackDeployment(generationTarget, "1.0.0"),
  );

  await assert.rejects(
    activateBundle(target, state2.active.bundle_sha256, "0.9.0"),
    /requires a newer runtime version/,
  );

  const corruptBundle = join(temporaryRoot, "corrupt.runtime");
  await cp(bundle8, corruptBundle, { recursive: true });
  const manifestPath = join(corruptBundle, "manifest.json");
  const manifestBytes = await readFile(manifestPath);
  await writeFile(manifestPath, Buffer.alloc(manifestBytes.length, 0x20));
  await assert.rejects(
    installBundle(corruptBundle, target),
    /manifest.json is not canonical|Unexpected end|JSON/,
  );

  await mkdir(join(target, "deployment.lock"));
  await assert.rejects(
    activateBundle(target, identity7, "1.0.0"),
    /another deployment transaction is active/,
  );
  await rm(join(target, "deployment.lock"), { recursive: true });

  const residue = (await readdir(target)).filter((name) =>
    name.startsWith(".install-"),
  );
  assert.deepEqual(residue, []);

  const healthTarget = await createTarget(temporaryRoot, "health-target");
  const healthPending7 = await deployBundle(bundle7, healthTarget, "1.0.0");
  assert.equal(healthPending7.active.status, "pending");
  const healthConfirmed7 = await resolveStartupDeployment(
    healthTarget,
    "1.0.0",
    async (authority) => {
      assert.deepEqual(Object.keys(authority), ["manifest"]);
      assert(Object.isFrozen(authority.manifest));
      return true;
    },
  );
  assert.equal(healthConfirmed7.state.generation, 2);
  assert.equal(healthConfirmed7.state.active.status, "healthy");
  assert.equal(healthConfirmed7.recovered_from_bundle_sha256, null);
  const healthRepeated = await resolveStartupDeployment(
    healthTarget,
    "1.0.0",
    async () => true,
  );
  assert.deepEqual(healthRepeated, healthConfirmed7);

  const substitutedHealthTarget = await createTarget(
    temporaryRoot,
    "substituted-health-target",
  );
  await deployBundle(bundle7, substitutedHealthTarget, "1.0.0");
  const displacedHealthBundles = join(
    substitutedHealthTarget,
    "bundles-displaced",
  );
  let healthReplacementPrevented = false;
  const substitutedHealth = resolveStartupDeployment(
    substitutedHealthTarget,
    "1.0.0",
    async () => {
      try {
        await rename(
          join(substitutedHealthTarget, "bundles"),
          displacedHealthBundles,
        );
        await mkdir(join(substitutedHealthTarget, "bundles"));
        await writeFile(
          join(substitutedHealthTarget, "bundles", "sentinel"),
          "preserve",
        );
      } catch (error) {
        healthReplacementPrevented =
          process.platform === "win32" &&
          (error.code === "EACCES" || error.code === "EPERM");
        if (!healthReplacementPrevented) throw error;
      }
      return true;
    },
  );
  let substitutedHealthError;
  try {
    await substitutedHealth;
  } catch (error) {
    substitutedHealthError = error;
  }
  if (healthReplacementPrevented) {
    assert.equal(substitutedHealthError, undefined);
  } else {
    assert.match(
      substitutedHealthError?.message ?? "",
      /deployment bundles directory filesystem object changed|probed deployment bundle filesystem object changed|no previous bundle is available/,
    );
    assert.equal(
      await readFile(
        join(substitutedHealthTarget, "bundles", "sentinel"),
        "utf8",
      ),
      "preserve",
    );
    assert.equal(
      (await readDeploymentState(substitutedHealthTarget)).active.status,
      "pending",
    );
  }

  const healthPending8 = await deployBundle(bundle8, healthTarget, "1.0.0");
  assert.equal(healthPending8.generation, 3);
  const recovered = await resolveStartupDeployment(
    healthTarget,
    "1.0.0",
    async ({ manifest }) => manifest.project_revision === 7,
  );
  assert.equal(recovered.state.generation, 4);
  assert.equal(recovered.state.active.project_revision, 7);
  assert.equal(recovered.state.active.status, "healthy");
  assert.equal(
    recovered.recovered_from_bundle_sha256,
    healthPending8.active.bundle_sha256,
  );

  await deployBundle(bundle8, healthTarget, "1.0.0");
  const beforeDoubleFailure = await readDeploymentState(healthTarget);
  await assert.rejects(
    resolveStartupDeployment(healthTarget, "1.0.0", async () => false),
    /active and previous bundles both failed/,
  );
  assert.deepEqual(
    await readDeploymentState(healthTarget),
    beforeDoubleFailure,
    "double startup failure changed deployment state",
  );

  const noPreviousTarget = await createTarget(
    temporaryRoot,
    "no-previous-target",
  );
  await deployBundle(bundle7, noPreviousTarget, "1.0.0");
  const beforeNoPreviousFailure = await readDeploymentState(noPreviousTarget);
  await assert.rejects(
    resolveStartupDeployment(noPreviousTarget, "1.0.0", async () => false),
    /no previous bundle is available/,
  );
  assert.deepEqual(
    await readDeploymentState(noPreviousTarget),
    beforeNoPreviousFailure,
  );

  const corruptActiveTarget = await createTarget(
    temporaryRoot,
    "corrupt-active-target",
  );
  await deployBundle(bundle7, corruptActiveTarget, "1.0.0");
  await resolveStartupDeployment(
    corruptActiveTarget,
    "1.0.0",
    async () => true,
  );
  const corruptPending = await deployBundle(
    bundle8,
    corruptActiveTarget,
    "1.0.0",
  );
  const corruptActiveManifestPath = join(
    corruptActiveTarget,
    "bundles",
    corruptPending.active.bundle_sha256,
    "manifest.json",
  );
  const corruptActiveManifest = await readFile(corruptActiveManifestPath);
  await writeFile(
    corruptActiveManifestPath,
    Buffer.alloc(corruptActiveManifest.length, 0x20),
  );
  const corruptionRecovery = await resolveStartupDeployment(
    corruptActiveTarget,
    "1.0.0",
    async () => true,
  );
  assert.equal(corruptionRecovery.state.active.project_revision, 7);
  assert.equal(corruptionRecovery.state.active.status, "healthy");
  assert.equal(
    corruptionRecovery.recovered_from_bundle_sha256,
    corruptPending.active.bundle_sha256,
  );

  const unconfiguredTarget = join(temporaryRoot, "unconfigured-target");
  await mkdir(unconfiguredTarget);
  await assert.rejects(
    installBundle(bundle7, unconfiguredTarget),
    /storage policy is not configured/,
  );
  await assert.rejects(
    activateBundle(unconfiguredTarget, "0".repeat(64), "1.0.0"),
    /storage policy is not configured/,
  );

  const oversizedPolicyTarget = join(temporaryRoot, "oversized-policy-target");
  await mkdir(oversizedPolicyTarget);
  await writeFile(
    join(oversizedPolicyTarget, "policy.json"),
    Buffer.alloc(64 * 1024 + 1, 0x20),
  );
  await assert.rejects(
    readDeploymentStorage(oversizedPolicyTarget),
    /deployment storage policy exceeds 65536 bytes/,
  );

  const redirectedTarget = join(temporaryRoot, "redirected-target");
  const redirectedOutside = join(temporaryRoot, "redirected-outside");
  await mkdir(redirectedTarget);
  await mkdir(redirectedOutside);
  await symlink(
    redirectedOutside,
    join(redirectedTarget, "bundles"),
    "junction",
  );
  await assert.rejects(
    configureDeploymentStorage(redirectedTarget, storagePolicy()),
    /bundles directory must be a real directory|bundles directory escapes/,
  );

  const replacedBundlesTarget = await createTarget(
    temporaryRoot,
    "replaced-bundles-target",
  );
  const replacedBundlesOutside = join(
    temporaryRoot,
    "replaced-bundles-outside",
  );
  await mkdir(replacedBundlesOutside);
  await writeFile(join(replacedBundlesOutside, "sentinel"), "preserve");
  await rm(join(replacedBundlesTarget, "bundles"), { recursive: true });
  await symlink(
    replacedBundlesOutside,
    join(replacedBundlesTarget, "bundles"),
    "junction",
  );
  await assert.rejects(
    readDeploymentStorage(replacedBundlesTarget),
    /bundles directory must be a real directory|bundles directory escapes/,
  );
  assert.equal(
    await readFile(join(replacedBundlesOutside, "sentinel"), "utf8"),
    "preserve",
  );

  const substitutedInstallTarget = await createTarget(
    temporaryRoot,
    "substituted-install-target",
  );
  const displacedInstallation = join(
    substitutedInstallTarget,
    "displaced-installation",
  );
  let replacementPrevented = false;
  const substitutedInstall = installBundle(
    bundle7,
    substitutedInstallTarget,
    {
      observer: async (boundary, { installedPath }) => {
        if (boundary === "before-bundle-publication") return;
        assert.equal(boundary, "after-bundle-publication");
        try {
          await rename(installedPath, displacedInstallation);
          await mkdir(installedPath);
          await writeFile(join(installedPath, "sentinel"), "preserve");
        } catch (error) {
          replacementPrevented =
            process.platform === "win32" &&
            (error.code === "EACCES" || error.code === "EPERM");
          if (!replacementPrevented) throw error;
        }
      },
    },
  );
  let substitutedInstallError;
  try {
    await substitutedInstall;
  } catch (error) {
    substitutedInstallError = error;
  }
  if (replacementPrevented) {
    assert.equal(substitutedInstallError, undefined);
  } else {
    assert.match(
      substitutedInstallError?.message ?? "",
      /filesystem object changed|bundle root must be a real directory/,
    );
    assert.equal(
      await readFile(
        join(substitutedInstallTarget, "bundles", identity7, "sentinel"),
        "utf8",
      ),
      "preserve",
    );
    if (process.platform === "linux") {
      await assert.rejects(inspectDeploymentBundle(displacedInstallation), /ENOENT/);
    } else {
      await inspectDeploymentBundle(displacedInstallation);
    }
  }

  const substitutedLockTarget = await createTarget(
    temporaryRoot,
    "substituted-lock-target",
  );
  const displacedLock = join(substitutedLockTarget, "displaced-lock");
  let lockReplacementPrevented = false;
  let substitutedLockError;
  try {
    await installBundle(bundle7, substitutedLockTarget, {
      observer: async (boundary) => {
        if (boundary !== "before-bundle-publication") return;
        const lockPath = join(substitutedLockTarget, "deployment.lock");
        try {
          await rename(lockPath, displacedLock);
          await mkdir(lockPath);
          await writeFile(join(lockPath, "owner.json"), "preserve");
        } catch (error) {
          lockReplacementPrevented =
            process.platform === "win32" &&
            (error.code === "EACCES" || error.code === "EPERM");
          if (!lockReplacementPrevented) throw error;
        }
      },
    });
  } catch (error) {
    substitutedLockError = error;
  }
  if (lockReplacementPrevented) {
    assert.equal(substitutedLockError, undefined);
  } else {
    assert.match(
      substitutedLockError?.message ?? "",
      /lock ownership was compromised|filesystem object changed/,
    );
    assert.equal(
      await readFile(
        join(substitutedLockTarget, "deployment.lock", "owner.json"),
        "utf8",
      ),
      "preserve",
    );
    await readFile(join(displacedLock, "owner.json"));
  }

  const substitutedBundlesTarget = await createTarget(
    temporaryRoot,
    "substituted-bundles-target",
  );
  const displacedBundles = join(
    substitutedBundlesTarget,
    "bundles-displaced",
  );
  let bundlesReplacementPrevented = false;
  const substitutedBundlesInstall = installBundle(
    bundle7,
    substitutedBundlesTarget,
    {
      observer: async (boundary) => {
        if (boundary !== "before-bundle-publication") return;
        try {
          await rename(
            join(substitutedBundlesTarget, "bundles"),
            displacedBundles,
          );
          await mkdir(join(substitutedBundlesTarget, "bundles"));
          await writeFile(
            join(substitutedBundlesTarget, "bundles", "sentinel"),
            "preserve",
          );
        } catch (error) {
          bundlesReplacementPrevented =
            process.platform === "win32" &&
            (error.code === "EACCES" || error.code === "EPERM");
          if (!bundlesReplacementPrevented) throw error;
        }
      },
    },
  );
  let substitutedBundlesError;
  try {
    await substitutedBundlesInstall;
  } catch (error) {
    substitutedBundlesError = error;
  }
  if (bundlesReplacementPrevented) {
    assert.equal(substitutedBundlesError, undefined);
  } else {
    assert.match(
      substitutedBundlesError?.message ?? "",
      /deployment bundles directory filesystem object changed|deployment bundle native publication failed/,
    );
    assert.equal(
      await readFile(
        join(substitutedBundlesTarget, "bundles", "sentinel"),
        "utf8",
      ),
      "preserve",
    );
    assert.deepEqual(await readdir(displacedBundles), []);
  }

  const publicationCollisionTarget = await createTarget(
    temporaryRoot,
    "publication-collision-target",
  );
  let collisionPath;
  await assert.rejects(
    installBundle(bundle7, publicationCollisionTarget, {
      observer: async (boundary, { installedPath }) => {
        if (boundary !== "before-bundle-publication") return;
        collisionPath = installedPath;
        await mkdir(installedPath);
      },
    }),
    /manifest\.json|runtime bundle root|ENOENT/,
  );
  assert(collisionPath);
  assert.deepEqual(await readdir(collisionPath), []);
  assert.equal(await readDeploymentLock(publicationCollisionTarget), null);

  const growingSource = join(temporaryRoot, "growing-source.runtime");
  await cp(bundle7, growingSource, { recursive: true });
  const growingTarget = await createTarget(temporaryRoot, "growing-source-target");
  await assert.rejects(
    installBundle(growingSource, growingTarget, {
      copyObserver: async (boundary) => {
        assert.equal(boundary, "before-staging-copy");
        const projectPath = join(growingSource, "project.json");
        await writeFile(
          projectPath,
          Buffer.concat([await readFile(projectPath), Buffer.from("x")]),
        );
      },
    }),
    /project\.json: source size changed before copy/,
  );
  assert.equal(await readDeploymentLock(growingTarget), null);
  assert.deepEqual(
    (await readdir(growingTarget)).filter((name) => name.startsWith(".install-")),
    [],
  );

  const stagingEscapeTarget = await createTarget(
    temporaryRoot,
    "staging-escape-target",
  );
  const stagingEscapeDestination = join(
    temporaryRoot,
    "staging-escape-destination",
  );
  await mkdir(stagingEscapeDestination);
  await assert.rejects(
    installBundle(bundle7, stagingEscapeTarget, {
      copyObserver: async (boundary, { stagingPath }) => {
        assert.equal(boundary, "before-staging-copy");
        await symlink(
          stagingEscapeDestination,
          join(stagingPath, "timeline"),
          process.platform === "win32" ? "junction" : "dir",
        );
      },
    }),
    /native copy failed/,
  );
  await assert.rejects(
    readFile(join(stagingEscapeDestination, "timeline.json")),
    { code: "ENOENT" },
  );
  assert.equal(await readDeploymentLock(stagingEscapeTarget), null);
  assert.deepEqual(
    (await readdir(stagingEscapeTarget)).filter((name) =>
      name.startsWith(".install-"),
    ),
    [],
  );

  const garbageTarget = await createTarget(temporaryRoot, "garbage-target");
  const garbageIdentity = await installBundle(bundle7, garbageTarget);
  assert.equal((await readDeploymentStorage(garbageTarget)).installed_bundle_count, 1);
  const garbageResult = await collectDeploymentGarbage(garbageTarget);
  assert.deepEqual(garbageResult.removed, [
    {
      bundle_sha256: garbageIdentity,
      byte_length: garbageResult.removed[0].byte_length,
    },
  ]);
  assert.equal(garbageResult.installed_bundle_count, 0);

  const source9 = await createMinimalProject(temporaryRoot, "source-9", {
    revision: 9,
  });
  const bundle9 = join(temporaryRoot, "revision-9.runtime");
  await buildProjectBundle(source9.root, bundle9, "1.0.0");
  const quotaTarget = await createTarget(
    temporaryRoot,
    "quota-target",
    storagePolicy({ max_installed_bundles: 3 }),
  );
  await deployBundle(bundle7, quotaTarget, "1.0.0");
  await deployBundle(bundle8, quotaTarget, "1.0.0");
  const identity9 = await installBundle(bundle9, quotaTarget);
  assert.equal((await readDeploymentStorage(quotaTarget)).installed_bundle_count, 3);
  const source10 = await createMinimalProject(temporaryRoot, "source-10", {
    revision: 10,
  });
  const bundle10 = join(temporaryRoot, "revision-10.runtime");
  await buildProjectBundle(source10.root, bundle10, "1.0.0");
  await assert.rejects(
    installBundle(bundle10, quotaTarget),
    /bundle-count quota exceeded/,
  );
  assert(
    (await readdir(join(quotaTarget, "bundles"))).includes(identity9),
    "failed installation removed an installed candidate",
  );
  const quotaGarbage = await collectDeploymentGarbage(quotaTarget);
  assert.deepEqual(
    quotaGarbage.removed.map(({ bundle_sha256 }) => bundle_sha256),
    [identity9],
  );
  const identity10 = await installBundle(bundle10, quotaTarget);
  const quotaStorage = await readDeploymentStorage(quotaTarget);
  assert.equal(quotaStorage.installed_bundle_count, 3);
  assert.equal(quotaStorage.protected_bundle_sha256.length, 2);
  assert(!quotaStorage.protected_bundle_sha256.includes(identity9));
  assert(
    (await readdir(join(quotaTarget, "bundles"))).includes(identity10),
    "replacement candidate was not retained",
  );
  assert(
    !(await readdir(join(quotaTarget, "bundles"))).includes(identity9),
    "unreferenced candidate was not reclaimed",
  );

  const byteTarget = await createTarget(temporaryRoot, "byte-target");
  await deployBundle(bundle7, byteTarget, "1.0.0");
  const byteUsage = await readDeploymentStorage(byteTarget);
  await configureDeploymentStorage(
    byteTarget,
    storagePolicy({ max_installed_bytes: byteUsage.installed_bytes }),
  );
  await assert.rejects(
    installBundle(bundle8, byteTarget),
    /byte quota exceeded/,
  );
  assert.equal((await readDeploymentStorage(byteTarget)).installed_bundle_count, 1);

  const reserveTarget = join(temporaryRoot, "reserve-target");
  await mkdir(reserveTarget);
  await assert.rejects(
    configureDeploymentStorage(
      reserveTarget,
      storagePolicy({ minimum_free_bytes: Number.MAX_SAFE_INTEGER }),
    ),
    /free-space reserve would be violated/,
  );

  const corruptStorageTarget = await createTarget(
    temporaryRoot,
    "corrupt-storage-target",
  );
  const corruptStorageIdentity = await installBundle(
    bundle9,
    corruptStorageTarget,
  );
  const corruptStorageManifest = join(
    corruptStorageTarget,
    "bundles",
    corruptStorageIdentity,
    "manifest.json",
  );
  const corruptStorageBytes = await readFile(corruptStorageManifest);
  await writeFile(
    corruptStorageManifest,
    Buffer.alloc(corruptStorageBytes.length, 0x20),
  );
  await assert.rejects(
    collectDeploymentGarbage(corruptStorageTarget),
    /manifest.json is not canonical|Unexpected end|JSON/,
  );

  const protectedGarbage = await collectDeploymentGarbage(target);
  assert.equal(protectedGarbage.removed.length, 0);
  assert.equal(protectedGarbage.protected_bundle_sha256.length, 2);

  const recoveryTarget = await createTarget(temporaryRoot, "recovery-target");
  const recoveryCandidate = await installBundle(bundle9, recoveryTarget);
  const staleOwner = {
    format: "hyper-deployment-lock",
    format_version: 1,
    owner_pid: 424242,
    owner_token: randomUUID(),
    extensions: {},
  };
  await writeDeploymentLockFixture(recoveryTarget, staleOwner);
  const installResidue = `.install-${randomUUID()}`;
  const metadataResidue = `.metadata-${randomUUID()}`;
  const stateResidue = `.state-${randomUUID()}.json`;
  const policyResidue = `.policy-${randomUUID()}.json`;
  await mkdir(join(recoveryTarget, installResidue));
  await mkdir(join(recoveryTarget, metadataResidue));
  await writeFile(join(recoveryTarget, stateResidue), "{}");
  await writeFile(join(recoveryTarget, policyResidue), "{}");
  await writeFile(join(recoveryTarget, ".install-not-a-transaction"), "keep");
  assert.deepEqual(await readDeploymentLock(recoveryTarget), staleOwner);
  const recoveredTarget = await recoverDeploymentTarget(recoveryTarget, {
    processIsAlive: async (pid) => {
      assert.equal(pid, staleOwner.owner_pid);
      return false;
    },
  });
  assert.deepEqual(recoveredTarget.recovered_lock_owner, staleOwner);
  assert.equal(recoveredTarget.forced, false);
  assert.deepEqual(recoveredTarget.removed_temporary_paths, [
    installResidue,
    metadataResidue,
    policyResidue,
    stateResidue,
  ].sort());
  assert.equal(recoveredTarget.storage.installed_bundle_count, 1);
  assert(
    (await readdir(join(recoveryTarget, "bundles"))).includes(
      recoveryCandidate,
    ),
    "recovery removed a retained candidate",
  );
  assert.equal(await readDeploymentLock(recoveryTarget), null);
  assert(
    (await readdir(recoveryTarget)).includes(".install-not-a-transaction"),
    "recovery removed an unrecognized path",
  );

  const substitutedResidueTarget = await createTarget(
    temporaryRoot,
    "substituted-recovery-residue-target",
  );
  const substitutedResidueName = `.state-${randomUUID()}.json`;
  const substitutedResiduePath = join(
    substitutedResidueTarget,
    substitutedResidueName,
  );
  const displacedResiduePath = join(
    substitutedResidueTarget,
    "displaced-residue",
  );
  await writeFile(substitutedResiduePath, "original");
  let residueSubstituted = false;
  await assert.rejects(
    recoverDeploymentTarget(substitutedResidueTarget, {
      observer: async (boundary, detail) => {
        if (boundary !== "before-file-residue-quarantine") return;
        assert.equal(detail.residuePath, substitutedResiduePath);
        await rename(substitutedResiduePath, displacedResiduePath);
        await writeFile(substitutedResiduePath, "replacement");
        residueSubstituted = true;
      },
    }),
    /metadata residue identity changed/,
  );
  assert.equal(residueSubstituted, true);
  assert.equal(await readFile(substitutedResiduePath, "utf8"), "replacement");
  assert.equal(await readFile(displacedResiduePath, "utf8"), "original");
  assert.equal(
    (await readdir(substitutedResidueTarget)).some((entry) =>
      entry.startsWith(".recovery-")
    ),
    false,
  );
  assert.equal(await readDeploymentLock(substitutedResidueTarget), null);

  const activeLockTarget = await createTarget(
    temporaryRoot,
    "active-lock-target",
  );
  await writeDeploymentLockFixture(activeLockTarget, staleOwner);
  await assert.rejects(
    recoverDeploymentTarget(activeLockTarget, {
      processIsAlive: async () => true,
    }),
    /owner process 424242 is still active/,
  );
  assert.deepEqual(await readDeploymentLock(activeLockTarget), staleOwner);
  await rm(join(activeLockTarget, "deployment.lock"), { recursive: true });

  const substitutedRecoveryTarget = await createTarget(
    temporaryRoot,
    "substituted-recovery-lock-target",
  );
  await writeDeploymentLockFixture(substitutedRecoveryTarget, staleOwner);
  const displacedRecoveryLock = join(
    substitutedRecoveryTarget,
    "displaced-lock",
  );
  let recoveryReplacementPrevented = false;
  let substitutedRecoveryError;
  try {
    await recoverDeploymentTarget(substitutedRecoveryTarget, {
      processIsAlive: async () => {
        const lockPath = join(substitutedRecoveryTarget, "deployment.lock");
        try {
          await rename(lockPath, displacedRecoveryLock);
          await mkdir(lockPath);
          await writeFile(join(lockPath, "owner.json"), "preserve");
        } catch (error) {
          recoveryReplacementPrevented =
            process.platform === "win32" &&
            (error.code === "EACCES" || error.code === "EPERM");
          if (!recoveryReplacementPrevented) throw error;
        }
        return false;
      },
    });
  } catch (error) {
    substitutedRecoveryError = error;
  }
  if (recoveryReplacementPrevented) {
    assert.equal(substitutedRecoveryError, undefined);
  } else {
    assert.match(
      substitutedRecoveryError?.message ?? "",
      /filesystem object changed/,
    );
    assert.equal(
      await readFile(
        join(substitutedRecoveryTarget, "deployment.lock", "owner.json"),
        "utf8",
      ),
      "preserve",
    );
    await readFile(join(displacedRecoveryLock, "owner.json"));
  }

  const malformedLockTarget = await createTarget(
    temporaryRoot,
    "malformed-lock-target",
  );
  await writeDeploymentLockFixture(malformedLockTarget, "invalid");
  await assert.rejects(
    recoverDeploymentTarget(malformedLockTarget),
    /explicit force is required/,
  );
  const forcedRecovery = await recoverDeploymentTarget(malformedLockTarget, {
    force: true,
  });
  assert.equal(forcedRecovery.forced, true);
  assert.equal(forcedRecovery.recovered_lock_owner, null);
  assert.equal(await readDeploymentLock(malformedLockTarget), null);

  const legacyLockTarget = await createTarget(
    temporaryRoot,
    "legacy-file-lock-target",
  );
  await writeFile(join(legacyLockTarget, "deployment.lock"), "preserve");
  await assert.rejects(
    recoverDeploymentTarget(legacyLockTarget, { force: true }),
    /not a recoverable directory/,
  );
  assert.equal(
    await readFile(join(legacyLockTarget, "deployment.lock"), "utf8"),
    "preserve",
  );

  const linkedLockTarget = await createTarget(
    temporaryRoot,
    "linked-lock-target",
  );
  const linkedLockOutside = join(temporaryRoot, "linked-lock-outside");
  await mkdir(linkedLockOutside);
  await writeFile(join(linkedLockOutside, "sentinel"), "preserve");
  await symlink(
    linkedLockOutside,
    join(linkedLockTarget, "deployment.lock"),
    "junction",
  );
  await assert.rejects(
    recoverDeploymentTarget(linkedLockTarget, { force: true }),
    /not a recoverable directory/,
  );
  assert.equal(
    await readFile(join(linkedLockOutside, "sentinel"), "utf8"),
    "preserve",
  );

  const faultBoundaries = [
    "after-staging-copy",
    "after-staging-verification",
    "after-bundle-publication",
    "after-bundle-installation",
    "after-state-sync",
    "after-state-publication",
  ];
  for (const boundary of faultBoundaries) {
    const faultTarget = await createTarget(
      temporaryRoot,
      `fault-${boundary}`,
    );
    await assert.rejects(
      deployBundle(bundle7, faultTarget, "1.0.0", { faultAt: boundary }),
      new RegExp(`injected deployment fault at ${boundary}`),
    );
    assert.equal(await readDeploymentLock(faultTarget), null);
    const stateAfterFault = await readDeploymentState(faultTarget);
    const publishedState = boundary === "after-state-publication";
    assert.equal(stateAfterFault !== null, publishedState);
    const rootResidue = (await readdir(faultTarget)).filter(
      (name) =>
        name.startsWith(".install-") ||
        name.startsWith(".metadata-") ||
        name.startsWith(".state-"),
    );
    assert.equal(rootResidue.length, 0);
    const recovery = await recoverDeploymentTarget(faultTarget);
    assert.equal(recovery.removed_temporary_paths.length, rootResidue.length);
    const installedAfterRecovery = await readdir(
      join(faultTarget, "bundles"),
    );
    const retainedInstallation = [
      "after-bundle-installation",
      "after-state-sync",
      "after-state-publication",
    ].includes(boundary);
    assert.equal(installedAfterRecovery.length, retainedInstallation ? 1 : 0);
    if (publishedState) {
      const retry = await deployBundle(bundle7, faultTarget, "1.0.0");
      assert.deepEqual(retry, stateAfterFault);
    } else if (retainedInstallation) {
      const retry = await deployBundle(bundle7, faultTarget, "1.0.0");
      assert.equal(retry.active.bundle_sha256, installedAfterRecovery[0]);
    }
  }
  await assert.rejects(
    deployBundle(bundle7, target, "1.0.0", {
      faultAt: "unknown-boundary",
    }),
    /faultAt must name a supported deployment transaction boundary/,
  );

  const authorizationTarget = await createTarget(
    temporaryRoot,
    "authorization-target",
  );
  const clientKeys = generateKeyPairSync("ed25519");
  const deviceId = "10000000-0000-4000-8000-000000000001";
  const clientId = "20000000-0000-4000-8000-000000000001";
  const authorizationNow = 1785153600000;
  const clientPublicKey = ed25519PublicJwk(clientKeys.publicKey);
  const authorizationPolicy = {
    max_paired_clients: 4,
    max_recent_requests: 16,
    max_audit_events: 64,
    max_request_lifetime_ms: 300000,
    max_clock_skew_ms: 30000,
    max_bundle_bytes: 1024 * 1024 * 1024,
  };
  const initialAuthorization = await createDeploymentAuthorizationState({
    deviceId,
    policy: authorizationPolicy,
  });
  const cliAuthorizationTarget = await createTarget(
    temporaryRoot,
    "cli-authorization-target",
  );
  const cliInitialAuthorizationPath = join(
    temporaryRoot,
    "cli-initial-authorization.json",
  );
  await writeFile(
    cliInitialAuthorizationPath,
    canonicalize(initialAuthorization),
  );
  await execFileAsync(process.execPath, [
    deploymentTargetScript,
    "configure-authorization",
    cliAuthorizationTarget,
    cliInitialAuthorizationPath,
  ]);
  const cliPairingOpen = JSON.parse(
    (
      await execFileAsync(process.execPath, [
        deploymentTargetScript,
        "pairing-open",
        cliAuthorizationTarget,
        String(authorizationNow),
        "60000",
        "--physical-presence",
      ])
    ).stdout,
  );
  const cliPairingRequest = signPairingRequest(
    {
      format: "hyper-deployment-pairing-request",
      format_version: 1,
      device_id: deviceId,
      client_id: clientId,
      client_name: "Studio Desktop",
      client_public_key: clientPublicKey,
      challenge_id: cliPairingOpen.challenge.challenge_id,
      issued_at_epoch_ms: authorizationNow - 1000,
      expires_at_epoch_ms: authorizationNow + 60000,
      extensions: {},
    },
    clientKeys.privateKey,
  );
  const cliPairingRequestPath = join(
    temporaryRoot,
    "cli-pairing-request.json",
  );
  await writeFile(cliPairingRequestPath, canonicalize(cliPairingRequest));
  const cliPairingAccepted = JSON.parse(
    (
      await execFileAsync(process.execPath, [
        deploymentTargetScript,
        "pairing-approve",
        cliAuthorizationTarget,
        cliPairingRequestPath,
        String(authorizationNow),
        jwkThumbprint(clientPublicKey),
      ])
    ).stdout,
  );
  assert.equal(cliPairingAccepted.accepted, true);
  const cliAuthorizationStatus = JSON.parse(
    (
      await execFileAsync(process.execPath, [
        deploymentTargetScript,
        "authorization",
        cliAuthorizationTarget,
      ])
    ).stdout,
  );
  assert.equal(cliAuthorizationStatus.clients.length, 1);
  const cliRevocation = JSON.parse(
    (
      await execFileAsync(process.execPath, [
        deploymentTargetScript,
        "client-revoke",
        cliAuthorizationTarget,
        clientId,
        String(authorizationNow),
        "--physical-authorization",
      ])
    ).stdout,
  );
  assert.equal(cliRevocation.accepted, true);
  const bootstrapChallenge = await issuePairingState(initialAuthorization, {
    nowEpochMs: authorizationNow,
    lifetimeMs: 60000,
    physicalPresence: true,
  });
  const bootstrapPairingRequest = signPairingRequest(
    {
      format: "hyper-deployment-pairing-request",
      format_version: 1,
      device_id: deviceId,
      client_id: clientId,
      client_name: "Studio Desktop",
      client_public_key: clientPublicKey,
      challenge_id: bootstrapChallenge.challenge.challenge_id,
      issued_at_epoch_ms: authorizationNow - 1000,
      expires_at_epoch_ms: authorizationNow + 60000,
      extensions: {},
    },
    clientKeys.privateKey,
  );
  const prepairedAuthorization = await approvePairingState(
    bootstrapChallenge.state,
    bootstrapPairingRequest,
    {
      nowEpochMs: authorizationNow,
      physicalAuthorization: true,
    },
  );
  const prepairedAuthorizationTarget = await createTarget(
    temporaryRoot,
    "prepaired-authorization-target",
  );
  await assert.rejects(
    configureDeploymentAuthorization(
      prepairedAuthorizationTarget,
      prepairedAuthorization.state,
    ),
    /initial authorization state/,
  );
  const noncanonicalAuthorizationTarget = await createTarget(
    temporaryRoot,
    "noncanonical-authorization-target",
  );
  await writeFile(
    join(noncanonicalAuthorizationTarget, "authorization.json"),
    JSON.stringify(initialAuthorization, null, 2),
  );
  await assert.rejects(
    readDeploymentAuthorization(noncanonicalAuthorizationTarget),
    /not canonical RFC 8785 JSON/,
  );

  const oversizedAuthorizationTarget = await createTarget(
    temporaryRoot,
    "oversized-authorization-target",
  );
  await writeFile(
    join(oversizedAuthorizationTarget, "authorization.json"),
    Buffer.alloc(1024 * 1024 + 1, 0x20),
  );
  await assert.rejects(
    readDeploymentAuthorization(oversizedAuthorizationTarget),
    /exceeds 1048576 bytes/,
  );

  const inconsistentAuthorizationTarget = await createTarget(
    temporaryRoot,
    "inconsistent-authorization-target",
  );
  const inconsistentAuthorization = {
    ...initialAuthorization,
    recent_requests: [
      {
        request_id: randomUUID(),
        client_id: randomUUID(),
        sequence: 1,
        envelope_sha256:
          "aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa",
        bundle_sha256:
          "bbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbb",
        status: "authorized",
        deployment_generation: null,
        failure_code: null,
      },
    ],
  };
  await writeFile(
    join(inconsistentAuthorizationTarget, "authorization.json"),
    canonicalize(inconsistentAuthorization),
  );
  await assert.rejects(
    readDeploymentAuthorization(inconsistentAuthorizationTarget),
    /unknown client/,
  );

  await configureDeploymentAuthorization(
    authorizationTarget,
    initialAuthorization,
  );
  assert.deepEqual(
    await readDeploymentAuthorization(authorizationTarget),
    initialAuthorization,
  );
  await assert.rejects(
    configureDeploymentAuthorization(
      authorizationTarget,
      initialAuthorization,
    ),
    /already configured/,
  );
  await assert.rejects(
    configureDeploymentStorage(
      authorizationTarget,
      storagePolicy({ max_installed_bytes: 512 * 1024 * 1024 }),
    ),
    /storage byte quota is below authorization bundle limit/,
  );
  await assert.rejects(
    installBundle(bundle7, authorizationTarget),
    /direct deployment is disabled/,
  );
  await assert.rejects(
    activateBundle(
      authorizationTarget,
      "aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa",
      "1.0.0",
    ),
    /direct deployment is disabled/,
  );
  await assert.rejects(
    deployBundle(bundle7, authorizationTarget, "1.0.0"),
    /direct deployment is disabled/,
  );
  await assert.rejects(
    rollbackDeployment(authorizationTarget, "1.0.0"),
    /direct deployment is disabled/,
  );

  await assert.rejects(
    openDeploymentPairing(authorizationTarget, {
      nowEpochMs: authorizationNow,
      lifetimeMs: 60000,
      physicalPresence: false,
    }),
    /physical presence is required/,
  );
  const openedPairing = await openDeploymentPairing(authorizationTarget, {
    nowEpochMs: authorizationNow,
    lifetimeMs: 60000,
    physicalPresence: true,
  });
  const pairingRequest = signPairingRequest(
    {
      format: "hyper-deployment-pairing-request",
      format_version: 1,
      device_id: deviceId,
      client_id: clientId,
      client_name: "Studio Desktop",
      client_public_key: clientPublicKey,
      challenge_id: openedPairing.challenge.challenge_id,
      issued_at_epoch_ms: authorizationNow - 1000,
      expires_at_epoch_ms: authorizationNow + 60000,
      extensions: {},
    },
    clientKeys.privateKey,
  );
  const pairingRefused = await approveDeploymentPairing(
    authorizationTarget,
    pairingRequest,
    {
      nowEpochMs: authorizationNow,
      physicalAuthorization: false,
    },
  );
  assert.equal(pairingRefused.accepted, false);
  const pairingAccepted = await approveDeploymentPairing(
    authorizationTarget,
    pairingRequest,
    {
      nowEpochMs: authorizationNow,
      physicalAuthorization: true,
    },
  );
  assert.equal(pairingAccepted.accepted, true);
  assert.equal(
    (await readDeploymentAuthorization(authorizationTarget)).clients.length,
    1,
  );

  async function signedRequest(bundlePath, sequence, overrides = {}) {
    const bundle = await inspectDeploymentBundle(bundlePath);
    return signDeploymentRequest(
      {
        format: "hyper-deployment-request",
        format_version: 1,
        device_id: deviceId,
        client_id: clientId,
        request_id: randomUUID(),
        sequence,
        operation: "deploy",
        bundle_sha256: bundle.bundle_sha256,
        bundle_byte_length: bundle.bundle_byte_length,
        runtime_version: "1.0.0",
        issued_at_epoch_ms: authorizationNow - 1000,
        expires_at_epoch_ms: authorizationNow + 60000,
        extensions: {},
        ...overrides,
      },
      clientKeys.privateKey,
    );
  }

  const authenticatedRequest7 = await signedRequest(bundle7, 1);
  const authenticated7 = await authenticatedDeployBundle(
    bundle7,
    authorizationTarget,
    authenticatedRequest7,
    authorizationNow,
  );
  assert.equal(authenticated7.status, "succeeded");
  assert.equal(authenticated7.deployment_state.active.project_revision, 7);
  const authorizationAfter7 = await readDeploymentAuthorization(
    authorizationTarget,
  );
  const repeatedAuthenticated7 = await authenticatedDeployBundle(
    bundle7,
    authorizationTarget,
    authenticatedRequest7,
    authorizationNow,
  );
  assert.equal(repeatedAuthenticated7.status, "succeeded");
  assert.equal(repeatedAuthenticated7.deployment_state, null);
  assert.deepEqual(
    await readDeploymentAuthorization(authorizationTarget),
    authorizationAfter7,
  );

  const mismatchedRequest = await signedRequest(bundle7, 2, {
    bundle_sha256:
      "bbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbb",
  });
  const mismatchedDeployment = await authenticatedDeployBundle(
    bundle7,
    authorizationTarget,
    mismatchedRequest,
    authorizationNow,
  );
  assert.equal(mismatchedDeployment.status, "failed");
  assert.equal(mismatchedDeployment.failure_code, "deployment-failed");
  assert.equal(
    (
      await readDeploymentAuthorization(authorizationTarget)
    ).audit_events.at(-1).result,
    "failed",
  );
  const mismatchedRetry = await authenticatedDeployBundle(
    bundle7,
    authorizationTarget,
    mismatchedRequest,
    authorizationNow,
  );
  assert.equal(mismatchedRetry.status, "failed");

  const authenticatedRequest8 = await signedRequest(bundle8, 3);
  const authenticated8 = await authenticatedDeployBundle(
    bundle8,
    authorizationTarget,
    authenticatedRequest8,
    authorizationNow,
  );
  assert.equal(authenticated8.status, "succeeded");
  assert.equal(authenticated8.deployment_state.active.project_revision, 8);

  const authenticatedRequest9 = await signedRequest(bundle9, 4);
  await assert.rejects(
    authenticatedDeployBundle(
      bundle9,
      authorizationTarget,
      authenticatedRequest9,
      authorizationNow,
      { faultAt: "after-authorization-sync" },
    ),
    /after-authorization-sync/,
  );
  assert.equal(
    (await readdir(authorizationTarget)).some((entry) =>
      entry.startsWith(".metadata-")
    ),
    false,
  );
  assert.equal(
    (
      await readDeploymentAuthorization(authorizationTarget)
    ).recent_requests.some(
      (request) => request.request_id === authenticatedRequest9.request_id,
    ),
    false,
  );
  const recoveredAuthorizationSync = await recoverDeploymentTarget(
    authorizationTarget,
  );
  assert.deepEqual(recoveredAuthorizationSync.removed_temporary_paths, []);
  await assert.rejects(
    authenticatedDeployBundle(
      bundle9,
      authorizationTarget,
      authenticatedRequest9,
      authorizationNow,
      { faultAt: "after-authorization-state-publication" },
    ),
    /after-authorization-state-publication/,
  );
  const authorizedResidue = (
    await readDeploymentAuthorization(authorizationTarget)
  ).recent_requests.find(
    (request) => request.request_id === authenticatedRequest9.request_id,
  );
  assert.equal(authorizedResidue.status, "authorized");
  const expiredAuthorizationRetryNow =
    authenticatedRequest9.expires_at_epoch_ms + 1;
  await assert.rejects(
    authenticatedDeployBundle(
      bundle9,
      authorizationTarget,
      authenticatedRequest9,
      expiredAuthorizationRetryNow,
      { faultAt: "after-authorization-publication" },
    ),
    /after-authorization-publication/,
  );
  await assert.rejects(
    authenticatedDeployBundle(
      bundle9,
      authorizationTarget,
      authenticatedRequest9,
      expiredAuthorizationRetryNow,
      { faultAt: "after-authorization-state-publication" },
    ),
    /after-authorization-state-publication/,
  );
  assert.equal(
    (
      await readDeploymentAuthorization(authorizationTarget)
    ).recent_requests.find(
      (request) => request.request_id === authenticatedRequest9.request_id,
    ).status,
    "succeeded",
  );
  const resumed9 = await authenticatedDeployBundle(
    bundle9,
    authorizationTarget,
    authenticatedRequest9,
    expiredAuthorizationRetryNow,
  );
  assert.equal(resumed9.status, "succeeded");
  assert.equal(resumed9.deployment_state, null);

  const authenticatedRequest10 = await signedRequest(bundle10, 5);
  await assert.rejects(
    authenticatedDeployBundle(
      bundle10,
      authorizationTarget,
      authenticatedRequest10,
      authorizationNow,
      { faultAt: "after-deployment-publication-before-result" },
    ),
    /after-deployment-publication-before-result/,
  );
  assert.equal(
    (await readDeploymentState(authorizationTarget)).active.project_revision,
    10,
  );
  const resumed10 = await authenticatedDeployBundle(
    bundle10,
    authorizationTarget,
    authenticatedRequest10,
    authorizationNow,
  );
  assert.equal(resumed10.status, "succeeded");
  assert.equal(resumed10.deployment_generation, resumed10.deployment_state.generation);

  const source11 = await createMinimalProject(temporaryRoot, "source-11", {
    revision: 11,
  });
  const bundle11 = join(temporaryRoot, "revision-11.runtime");
  await buildProjectBundle(source11.root, bundle11, "1.0.0");
  const authenticatedRequest11 = await signedRequest(bundle11, 6);
  const committed11 = await authenticatedDeployBundle(
    bundle11,
    authorizationTarget,
    authenticatedRequest11,
    authorizationNow,
    { faultAt: "after-state-publication" },
  );
  assert.equal(committed11.status, "succeeded");
  assert.equal(committed11.deployment_state.active.project_revision, 11);

  const incompatibleRequestForActiveBundle = await signedRequest(bundle11, 7, {
    runtime_version: "0.9.0",
  });
  const incompatibleActiveDeployment = await authenticatedDeployBundle(
    bundle11,
    authorizationTarget,
    incompatibleRequestForActiveBundle,
    authorizationNow,
  );
  assert.equal(incompatibleActiveDeployment.status, "failed");
  assert.equal(
    incompatibleActiveDeployment.failure_code,
    "deployment-failed",
  );

  const revokedClient = await revokeDeploymentClient(
    authorizationTarget,
    clientId,
    {
      nowEpochMs: authorizationNow,
      physicalAuthorization: true,
    },
  );
  assert.equal(revokedClient.accepted, true);
  const requestAfterRevocation = await signedRequest(bundle11, 8);
  const rejectedAfterRevocation = await authenticatedDeployBundle(
    bundle11,
    authorizationTarget,
    requestAfterRevocation,
    authorizationNow,
  );
  assert.equal(rejectedAfterRevocation.status, "rejected");
  assert.equal(rejectedAfterRevocation.failure_code, "unknown-client");

  const authorizationResidue = `.authorization-${randomUUID()}.json`;
  await writeFile(join(authorizationTarget, authorizationResidue), "{}");
  const recoveredAuthorizationTarget = await recoverDeploymentTarget(
    authorizationTarget,
  );
  assert(
    recoveredAuthorizationTarget.removed_temporary_paths.includes(
      authorizationResidue,
    ),
  );

  console.log("Deployment target passed: 161 transaction cases");
} finally {
  await rm(temporaryRoot, { recursive: true, force: true });
}
