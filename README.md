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

The target must already contain an installed and activated bundle. Application
archives include the tested local deployment kit, its locked dependencies, and
the matching native filesystem and audio-validation helpers;
running it requires Node.js 22 or newer (CI uses 24.13.0). For a new local
development target, create the target directory and choose a storage policy
appropriate to that disk. The kit includes
`deployment-kit/storage-policy.example.json`; review its bundle count, byte
quota, and free-space reserve before using it. From the extracted archive,
run:

```text
node deployment-kit/scripts/deployment-target.mjs configure-storage <DEPLOYMENT_DIRECTORY> <POLICY_JSON>
node deployment-kit/scripts/deployment-target.mjs deploy <BUNDLE_DIRECTORY> <DEPLOYMENT_DIRECTORY> <RUNTIME_VERSION>
node deployment-kit/scripts/deployment-target.mjs status <DEPLOYMENT_DIRECTORY>
```

`<RUNTIME_VERSION>` is the numeric version reported by
`hyper-runtime --version`. The direct deployment commands are for a local,
unauthenticated development target; direct install, deploy, activate, and
rollback refuse mutation after deployment authorization is configured. The
installed target is verified again at Runtime startup. The kit also provides
`storage <target>` to inspect quota use, `rollback <target> <version>` to
reactivate the previous bundle, and `gc <target>` to remove only unreferenced
bundles. `recover <target>` is an explicit stale-lock recovery operation; it
refuses a live owner, and `--force` is reserved for malformed lock ownership
after independent inspection. Use `hyper-runtime --help` for optional
output/input selection, panel,
operator, and external-update directories. Recording requires an explicitly
pinned output, selected input, and external recording-update directory; Runtime
does not edit the deployed project or automatically import handbacks.

## Recovering an interrupted recording

After the recording owner has stopped, the archive's `hyper-recording-recover`
command can publish committed journal audio to a separate WAV path:

```text
hyper-recording-recover <JOURNAL> <OUTPUT_WAV>
```

Choose a new `.wav` destination in an existing directory. The command verifies
the journal's committed chunks, may trim an incomplete trailing chunk from the
journal, and retains the journal. It refuses a live journal and never replaces
different existing output media; retrying the same publication is safe. A
recovered WAV is salvage for audition and deliberate import, not proof of
uninterrupted capture or a lossless take. Neither application automatically
imports this WAV or changes a deployed project during recovery.

## Building and evidence

The repository pins Rust 1.93.0 in `rust-toolchain.toml`. From the repository
root, run:

```text
cargo build --locked --release -p hyper-desktop-ui -p hyper-runtime -p hyper-recording-journal
```

This builds both applications and the recovery command. The CI workflow is
configured to run tests, strict Clippy, and platform-specific gates before
producing commit-addressed archives and adjacent SHA-256 manifests. Verify an
archive against its manifest before use.
The CI archive contains the two applications, the recording recovery command,
the local deployment kit, this guide, and third-party notices. It does not
bundle Node.js, a project, or native audio-device configuration.
On Ubuntu 24.04, the desktop UI's X11 backend also needs the system
`libxkbcommon-x11-0` runtime package; the archive does not bundle it.

The present product limit is eight track documents and native execution of
audio tracks, Groups, and Returns. MIDI/CV execution and third-party plug-in
hosting are not claimed. Hosted artifact delivery, installation on another
machine, and physical audio-device behavior need separate acceptance evidence
before this software can be called a stable daily-use release.
