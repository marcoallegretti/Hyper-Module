# Hyper Module DAW

Hyper Module is an audio-focused DAW in development. The Desktop IDE edits and
verifies projects; the separate Runtime hosts an installed, verified project
bundle. Current CI archives are software previews, not installers or a claim of
physical-device acceptance.

## Desktop IDE

Run `hyper-desktop-ui` to create or open a project from the application. To open
an existing project package directly, run
`hyper-desktop-ui <PROJECT_DIRECTORY>`. Use `hyper-desktop-ui --help` for the
exact invocation. The IDE's **Prepare Runtime** action saves the current
project and builds a verified `.runtime` bundle at a destination you choose;
it does not install or activate that bundle.

## Runtime

Run `hyper-runtime --list-audio-outputs` and
`hyper-runtime --list-audio-inputs` to inspect current desktop routes without
starting the host. Runtime takes a **deployment target directory**, not the
IDE's `.runtime` bundle directly:

```text
hyper-runtime <DEPLOYMENT_DIRECTORY> --operator-ui
```

The target must already contain an installed and activated bundle. The current
local installation workflow lives in the source checkout under
`contracts/scripts/deployment-target.mjs` and requires Node.js and the locked
`contracts` dependencies; it is **not included in the application archive**.
For a new local development target, create the target directory, choose a
storage policy appropriate to that disk (see
`contracts/schemas/v1/deployment-storage-policy.schema.json`), then run from
the source checkout's `contracts` directory:

```text
npm ci --ignore-scripts
npm run deploy:local -- configure-storage <DEPLOYMENT_DIRECTORY> <POLICY_JSON>
npm run deploy:local -- deploy <BUNDLE_DIRECTORY> <DEPLOYMENT_DIRECTORY> <RUNTIME_VERSION>
npm run deploy:local -- status <DEPLOYMENT_DIRECTORY>
```

`<RUNTIME_VERSION>` is the numeric version reported by
`hyper-runtime --version`. The direct deployment commands are for a local,
unauthenticated development target; they refuse mutation after deployment
authorization is configured. The installed target is verified again at Runtime
startup. Use `hyper-runtime --help` for optional output/input selection, panel,
operator, and external-update directories. Recording requires an explicitly
pinned output, selected input, and external recording-update directory; Runtime
does not edit the deployed project or automatically import handbacks.

## Building and evidence

The repository pins Rust 1.93.0 in `rust-toolchain.toml`. From the repository
root, `cargo build --locked --release -p hyper-desktop-ui -p hyper-runtime`
builds both applications. CI additionally runs tests, strict Clippy, and
platform-specific gates before publishing commit-addressed archives and
adjacent SHA-256 manifests. Verify the archive against its manifest before use.
The CI archive contains the two applications, this guide, and
`THIRD_PARTY_NOTICES.md`; it does not bundle a deployment installer, project,
or native audio-device configuration.

The present product limit is eight track documents and native execution of
audio tracks, Groups, and Returns. MIDI/CV execution and third-party plug-in
hosting are not claimed. Hosted artifact delivery, installation on another
machine, and physical audio-device behavior need separate acceptance evidence
before this software can be called a stable daily-use release.
