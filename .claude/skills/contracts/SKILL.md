---
name: contracts
description: JSON formats, schemas, fixtures, and the Node conformance tests. Use when changing anything under contracts/, a data format read or written by Rust crates, bundle building, or deployment state.
---

# Contracts

`contracts/` is the source of truth for on-disk and wire formats. Rust crates
(`project-model`, `control-mappings`, `recording-metadata`, `strict-json`,
`storage-engine`, …) and the Node libraries must agree on it.

A format change moves together, in one commit series:

1. `contracts/schemas/v1/<name>.schema.json`, registered in the `schemaPaths`
   list of `scripts/verify-contracts.mjs`.
2. `contracts/fixtures/valid/` and `fixtures/invalid/` (an invalid case per new
   rule, named for the rule it breaks).
3. `contracts/lib/<name>.mjs` and `scripts/test-<name>.mjs`, added to the
   `npm test` chain in `contracts/package.json`.
4. The Rust reader/writer and its tests, using the same fixtures.

Rules: version-1 stays compatible unless the maintainer agrees a new version;
decoding is strict and bounded (`strict-json`: unknown fields, duplicate keys
and oversize input are rejected); canonical JSON where hashes are taken; no
network or private paths in fixtures.

```bash
(cd contracts && npm ci --ignore-scripts && npm test)    # Node >= 22
node contracts/scripts/check-public-boundary.mjs         # nothing private tracked
```

`npm test` first builds the native helpers (`hyper-audio-validate`,
`hyper-fs-authority`); the Node tests exercise them for real, so do not stub
them. A new npm dependency is pinned exactly and listed in
`contracts/THIRD_PARTY_NOTICES.md`.
