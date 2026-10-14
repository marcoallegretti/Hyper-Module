---
name: runtime
description: The Runtime host, bundle verification and deployment. Use when touching software/RUNTIME/*, crates/fs-authority, storage-engine, or deployment-kit scripts in contracts/.
---

# Runtime and deployment

- `runtime` is the host binary; `runtime-host`, `panel-session`,
  `control-runtime` serve panels and control mappings; `runtime-bundle`
  verifies and prepares bundles; `deployment` selects and health-checks
  installed bundles.
- Runtime takes a **deployment target directory**, never an IDE `.runtime`
  bundle directly. It verifies the installed target again at startup and does
  not edit the deployed project or auto-import handbacks.
- Mutating a deployment (install, deploy, activate, rollback, gc, recover) is
  fail-closed: atomic publication, lock ownership, storage policy limits,
  and refusal once deployment authorization is configured. Keep it that way;
  a new mutation path must honour the same checks and have a refusal test.
- Runtime reports back through the `runtime-*-update` crates: bounded,
  provenance-bound artifacts the IDE verifies. The IDE stays the only project
  writer; Runtime never edits the deployed project.
- Filesystem authority goes through `fs-authority` and the durable-file
  helpers in `contracts/lib`; do not hand-roll rename/fsync sequences.

## Proof

`software/RUNTIME/deployment/tests/contract.rs` plus the Node deployment tests
(`test-deployment-*.mjs`, `test-native-runtime-deployment.mjs`). The packaged
smoke (`contracts/scripts/smoke-packaged-deployment.mjs`) is what CI runs on
the archive; changes to packaging or the kit are checked by running it, not by
reading it. Recording behaviour needs an explicitly pinned output, input and
update directory; keep that requirement and its refusal tests.
