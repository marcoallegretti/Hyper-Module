import assert from "node:assert/strict";
import { createHash } from "node:crypto";
import {
  lstat,
  mkdtemp,
  mkdir,
  open,
  readFile,
  rename,
  rm,
  symlink,
  writeFile,
} from "node:fs/promises";
import { tmpdir } from "node:os";
import { join } from "node:path";

import { openRetainedDirectory } from "../lib/durable-files.mjs";
import { runNativeProcess } from "../lib/native-process.mjs";
import {
  copyRetainedFile,
  createRetainedDirectory,
  preflightFilesystemAuthority,
  removeRetainedDirectory,
  removeRetainedDirectoryEntry,
  renameDirectoryBetweenNoReplace,
  renameDirectoryNoReplace,
  renameEntryBetweenReplace,
  resolveFilesystemAuthorityPath,
  writeRetainedFile,
} from "../lib/native-fs-authority.mjs";

const temporaryRoot = await mkdtemp(join(tmpdir(), "hyper-fs-authority-"));
try {
  await preflightFilesystemAuthority("native authority test");
  const authorityExecutable = resolveFilesystemAuthorityPath();
  await assert.rejects(
    runNativeProcess(authorityExecutable, ["0"]),
    /unsupported filesystem authority protocol version/,
  );

  const removable = join(temporaryRoot, "removable");
  await mkdir(join(removable, "nested"), { recursive: true });
  await writeFile(join(removable, "nested", "payload"), "owned");
  const removableHandle = await openRetainedDirectory(removable);
  try {
    await removeRetainedDirectory(
      removableHandle,
      removable,
      "test directory",
    );
  } finally {
    await removableHandle.close();
  }
  await assert.rejects(open(removable, "r"), { code: "ENOENT" });

  const retainedPath = join(temporaryRoot, "retained");
  const movedPath = join(temporaryRoot, "moved");
  await mkdir(retainedPath);
  const retainedHandle = await openRetainedDirectory(retainedPath);
  try {
    await rename(retainedPath, movedPath);
    await mkdir(retainedPath);
    await writeFile(join(retainedPath, "replacement"), "preserve");

    if (process.platform === "linux") {
      await removeRetainedDirectory(
        retainedHandle,
        retainedPath,
        "replaced test directory",
      );
      await assert.rejects(open(movedPath, "r"), { code: "ENOENT" });
    } else {
      await assert.rejects(
        removeRetainedDirectory(
          retainedHandle,
          retainedPath,
          "replaced test directory",
        ),
        /native cleanup failed/,
      );
      await removeRetainedDirectory(
        retainedHandle,
        movedPath,
        "moved test directory",
      );
    }
    assert.equal(
      await readFile(join(retainedPath, "replacement"), "utf8"),
      "preserve",
    );
  } finally {
    await retainedHandle.close();
  }
  await assert.rejects(open(movedPath, "r"), { code: "ENOENT" });

  const parentHandle = await openRetainedDirectory(temporaryRoot);
  try {
    const created = await createRetainedDirectory(
      parentHandle,
      temporaryRoot,
      "created-under-authority",
      "test retained directory",
    );
    const createdStatus = await lstat(
      join(temporaryRoot, "created-under-authority"),
      { bigint: true },
    );
    assert.equal(createdStatus.dev, created.identity.dev);
    assert.equal(createdStatus.ino, created.identity.ino);
    await assert.rejects(
      createRetainedDirectory(
        parentHandle,
        temporaryRoot,
        "created-under-authority",
        "colliding retained directory",
      ),
      { code: "EEXIST" },
    );
    await removeRetainedDirectoryEntry(
      parentHandle,
      temporaryRoot,
      "created-under-authority",
      created.identity,
      "test retained directory entry",
    );
    await assert.rejects(
      open(join(temporaryRoot, "created-under-authority"), "r"),
      { code: "ENOENT" },
    );

    const boundCreated = await createRetainedDirectory(
      parentHandle,
      temporaryRoot,
      "created-bound",
      "identity-bound created directory",
    );
    await rename(
      join(temporaryRoot, "created-bound"),
      join(temporaryRoot, "created-bound-displaced"),
    );
    await mkdir(join(temporaryRoot, "created-bound"));
    await assert.rejects(
      removeRetainedDirectoryEntry(
        parentHandle,
        temporaryRoot,
        "created-bound",
        boundCreated.identity,
        "identity-bound directory cleanup",
      ),
      /native cleanup failed/,
    );
    await open(join(temporaryRoot, "created-bound"), "r").then((handle) =>
      handle.close(),
    );

    await mkdir(join(temporaryRoot, "publish-source"));
    await renameDirectoryNoReplace(
      parentHandle,
      temporaryRoot,
      "publish-source",
      await lstat(join(temporaryRoot, "publish-source"), { bigint: true }),
      "published",
      "test publication",
    );
    await assert.rejects(open(join(temporaryRoot, "publish-source"), "r"), {
      code: "ENOENT",
    });

    await mkdir(join(temporaryRoot, "blocked-source"));
    await mkdir(join(temporaryRoot, "occupied"));
    await writeFile(join(temporaryRoot, "occupied", "marker"), "preserve");
    await assert.rejects(
      renameDirectoryNoReplace(
        parentHandle,
        temporaryRoot,
        "blocked-source",
        await lstat(join(temporaryRoot, "blocked-source"), { bigint: true }),
        "occupied",
        "blocked test publication",
      ),
      { code: "EEXIST" },
    );
    assert.equal(
      await readFile(join(temporaryRoot, "occupied", "marker"), "utf8"),
      "preserve",
    );
    await open(join(temporaryRoot, "blocked-source"), "r").then((handle) =>
      handle.close(),
    );

    const identitySource = join(temporaryRoot, "identity-source");
    const identityDisplaced = join(temporaryRoot, "identity-displaced");
    await mkdir(identitySource);
    await writeFile(join(identitySource, "marker"), "retained");
    const identitySourceStatus = await lstat(identitySource, { bigint: true });
    await rename(identitySource, identityDisplaced);
    await mkdir(identitySource);
    await writeFile(join(identitySource, "marker"), "replacement");
    await assert.rejects(
      renameDirectoryNoReplace(
        parentHandle,
        temporaryRoot,
        "identity-source",
        identitySourceStatus,
        "identity-published",
        "identity-bound test publication",
      ),
      (error) => {
        assert.match(error.cause?.message ?? "", /source entry identity/);
        return true;
      },
    );
    assert.equal(
      await readFile(join(identitySource, "marker"), "utf8"),
      "replacement",
    );
    assert.equal(
      await readFile(join(identityDisplaced, "marker"), "utf8"),
      "retained",
    );
    await assert.rejects(open(join(temporaryRoot, "identity-published"), "r"), {
      code: "ENOENT",
    });

    const transferRoot = join(temporaryRoot, "transfer-root");
    const transferSource = join(temporaryRoot, "transfer-source");
    const transferBytes = Buffer.from("capability-confined transfer", "utf8");
    await mkdir(transferRoot);
    await writeFile(transferSource, transferBytes);
    const [transferRootHandle, transferSourceHandle] = await Promise.all([
      openRetainedDirectory(transferRoot),
      open(transferSource, "r"),
    ]);
    try {
      const transferSha256 = createHash("sha256")
        .update(transferBytes)
        .digest("hex");
      assert.deepEqual(
        await copyRetainedFile(
          transferSourceHandle,
          transferSource,
          transferRootHandle,
          transferRoot,
          "nested/payload.bin",
          transferBytes.byteLength,
          transferSha256,
          transferBytes.byteLength,
          "test retained copy",
        ),
        { byteLength: transferBytes.byteLength, sha256: transferSha256 },
      );
      assert.deepEqual(
        await readFile(join(transferRoot, "nested", "payload.bin")),
        transferBytes,
      );

      const generatedBytes = Buffer.from('{"retained":true}', "utf8");
      await writeRetainedFile(
        transferRootHandle,
        transferRoot,
        "generated/state.json",
        generatedBytes,
        generatedBytes.byteLength,
        "test retained write",
      );
      assert.deepEqual(
        await readFile(join(transferRoot, "generated", "state.json")),
        generatedBytes,
      );

      const escapedRoot = join(temporaryRoot, "transfer-escape");
      await mkdir(escapedRoot);
      await symlink(
        escapedRoot,
        join(transferRoot, "linked"),
        process.platform === "win32" ? "junction" : "dir",
      );
      await assert.rejects(
        copyRetainedFile(
          transferSourceHandle,
          transferSource,
          transferRootHandle,
          transferRoot,
          "linked/escape.bin",
          transferBytes.byteLength,
          transferSha256,
          transferBytes.byteLength,
          "linked retained copy",
        ),
        /native copy failed/,
      );
      await assert.rejects(
        writeRetainedFile(
          transferRootHandle,
          transferRoot,
          "linked/generated.bin",
          generatedBytes,
          generatedBytes.byteLength,
          "linked retained write",
        ),
        /native write failed/,
      );
      await assert.rejects(open(join(escapedRoot, "escape.bin"), "r"), {
        code: "ENOENT",
      });
      await assert.rejects(open(join(escapedRoot, "generated.bin"), "r"), {
        code: "ENOENT",
      });
    } finally {
      await Promise.all([
        transferRootHandle.close(),
        transferSourceHandle.close(),
      ]);
    }

    const sourceParent = join(temporaryRoot, "source-parent");
    const destinationParent = join(temporaryRoot, "destination-parent");
    await Promise.all([mkdir(sourceParent), mkdir(destinationParent)]);
    const [sourceParentHandle, destinationParentHandle] = await Promise.all([
      openRetainedDirectory(sourceParent),
      openRetainedDirectory(destinationParent),
    ]);
    try {
      await mkdir(join(sourceParent, "transfer"));
      const transferStatus = await lstat(join(sourceParent, "transfer"), {
        bigint: true,
      });
      await renameDirectoryBetweenNoReplace(
        sourceParentHandle,
        sourceParent,
        "transfer",
        transferStatus,
        destinationParentHandle,
        destinationParent,
        "transferred",
        "cross-parent test publication",
      );
      await assert.rejects(open(join(sourceParent, "transfer"), "r"), {
        code: "ENOENT",
      });
      await open(join(destinationParent, "transferred"), "r").then((handle) =>
        handle.close(),
      );

      await mkdir(join(sourceParent, "blocked-transfer"));
      await mkdir(join(destinationParent, "occupied-transfer"));
      const blockedTransferStatus = await lstat(
        join(sourceParent, "blocked-transfer"),
        { bigint: true },
      );
      await assert.rejects(
        renameDirectoryBetweenNoReplace(
          sourceParentHandle,
          sourceParent,
          "blocked-transfer",
          blockedTransferStatus,
          destinationParentHandle,
          destinationParent,
          "occupied-transfer",
          "blocked cross-parent test publication",
        ),
        { code: "EEXIST" },
      );
      await open(join(sourceParent, "blocked-transfer"), "r").then((handle) =>
        handle.close(),
      );

      await mkdir(join(sourceParent, "file-collision-source"));
      const fileCollisionStatus = await lstat(
        join(sourceParent, "file-collision-source"),
        { bigint: true },
      );
      await writeFile(join(destinationParent, "occupied-file"), "preserve");
      await assert.rejects(
        renameDirectoryBetweenNoReplace(
          sourceParentHandle,
          sourceParent,
          "file-collision-source",
          fileCollisionStatus,
          destinationParentHandle,
          destinationParent,
          "occupied-file",
          "file collision publication",
        ),
        { code: "EEXIST" },
      );
      assert.equal(
        await readFile(join(destinationParent, "occupied-file"), "utf8"),
        "preserve",
      );

      const linkTarget = join(temporaryRoot, "collision-link-target");
      await mkdir(linkTarget);
      await writeFile(join(linkTarget, "marker"), "preserve");
      await symlink(
        linkTarget,
        join(destinationParent, "occupied-link"),
        process.platform === "win32" ? "junction" : "dir",
      );
      await mkdir(join(sourceParent, "link-collision-source"));
      const linkCollisionStatus = await lstat(
        join(sourceParent, "link-collision-source"),
        { bigint: true },
      );
      await assert.rejects(
        renameDirectoryBetweenNoReplace(
          sourceParentHandle,
          sourceParent,
          "link-collision-source",
          linkCollisionStatus,
          destinationParentHandle,
          destinationParent,
          "occupied-link",
          "link collision publication",
        ),
        { code: "EEXIST" },
      );
      assert.equal(
        await readFile(join(linkTarget, "marker"), "utf8"),
        "preserve",
      );

      await writeFile(join(sourceParent, "replacement-source"), "new");
      await writeFile(join(destinationParent, "replacement-target"), "old");
      const replacementSourceStatus = await lstat(
        join(sourceParent, "replacement-source"),
        { bigint: true },
      );
      await renameEntryBetweenReplace(
        sourceParentHandle,
        sourceParent,
        "replacement-source",
        replacementSourceStatus,
        destinationParentHandle,
        destinationParent,
        "replacement-target",
        "cross-parent test replacement",
      );
      await assert.rejects(open(join(sourceParent, "replacement-source"), "r"), {
        code: "ENOENT",
      });
      assert.equal(
        await readFile(join(destinationParent, "replacement-target"), "utf8"),
        "new",
      );

      const boundSource = join(sourceParent, "bound-source");
      const displacedSource = join(sourceParent, "bound-source-displaced");
      await writeFile(boundSource, "retained");
      const boundSourceStatus = await lstat(boundSource, { bigint: true });
      await rename(boundSource, displacedSource);
      await writeFile(boundSource, "replacement");
      await writeFile(join(destinationParent, "bound-target"), "preserve");
      await assert.rejects(
        renameEntryBetweenReplace(
          sourceParentHandle,
          sourceParent,
          "bound-source",
          boundSourceStatus,
          destinationParentHandle,
          destinationParent,
          "bound-target",
          "identity-bound test replacement",
        ),
        (error) => {
          assert.match(error.cause?.message ?? "", /source entry identity/);
          return true;
        },
      );
      assert.equal(await readFile(boundSource, "utf8"), "replacement");
      assert.equal(await readFile(displacedSource, "utf8"), "retained");
      assert.equal(
        await readFile(join(destinationParent, "bound-target"), "utf8"),
        "preserve",
      );
    } finally {
      await Promise.all([
        sourceParentHandle.close(),
        destinationParentHandle.close(),
      ]);
    }
  } finally {
    await parentHandle.close();
  }
  console.log("Native filesystem authority passed: 20 identity cases");
} finally {
  await rm(temporaryRoot, { recursive: true, force: true });
}
