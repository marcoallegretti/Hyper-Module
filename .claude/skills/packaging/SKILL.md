---
name: packaging
description: CI jobs, application archives, the deployment kit, and platform gates. Use when touching .github/workflows, archive contents, the deployment kit, dependencies, or THIRD_PARTY_NOTICES.
---

# Packaging and CI

- `ci.yml` jobs: `public-boundary`, `change-hygiene`, `rust` (Ubuntu 24.04,
  Ubuntu 26.04 experimental, Windows), `arm64-ide`, `contracts`, `package`.
  `package` runs only after the others pass and builds commit-addressed
  archives with adjacent SHA-256 manifests. The test jobs write a
  `hyper-ci-test-report-v1` report; keep that when adding a step to one.
- Actions are pinned by commit SHA and checkouts use `persist-credentials:
  false`. New third-party actions follow suit; never a floating tag.
- Archive contents are a promise. Windows and x86-64 Ubuntu: both
  applications, the recovery command, the deployment kit, README and notices.
  Ubuntu ARM64: Wayland-only IDE, README and notices; no Runtime, no kit. No
  Node.js, project or audio configuration is bundled. A change that alters
  these also changes README in the same pull request.
- ARM64 packaging is gated on a native compile; do not weaken the gate to get
  an archive.
- The kit's Node dependencies are installed locked (`npm ci --ignore-scripts`)
  and audited; a new dependency is pinned exactly and listed in
  `contracts/THIRD_PARTY_NOTICES.md`, a new crate in `THIRD_PARTY_NOTICES.md`.
- Release builds: `cargo build --locked --release -p hyper-desktop-ui -p
  hyper-runtime -p hyper-recording-journal -p hyper-fs-authority -p
  hyper-audio-source`.

## Proof

A workflow change is proved by running the changed command locally, then by the
job itself. Packaging is proved by the packaged deployment smoke
(`contracts/scripts/smoke-packaged-deployment.mjs`) run against the extracted
archive, not against the source tree.
