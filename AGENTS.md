# Hyper Module

Audio DAW in Rust. The IDE (`software/IDE`, Slint) edits and verifies projects;
Runtime (`software/RUNTIME`) hosts an installed, verified bundle. Shared
libraries live in `crates/` and never depend on `software/`. Data formats,
schemas and their Node conformance tests live in `contracts/`.

`CLAUDE.md` imports this file.

## Rules

- **Never commit to `main`.** One change, one branch, one pull request.
- **Branch: `<kind>/<slug>`.** Kind is `fix`, `feat`, `perf`, `refactor`,
  `test`, `ci` or `build`. Slug is lowercase kebab naming the change
  (`fix/recording-journal-trim`). Never tool-, session- or task-derived
  (`claude/…`, `codex/…`, `wip`, `tmp`).
- **Commits.** One focused change, revertible alone. Imperative subject, at
  most 72 characters, saying what is now true ("Refuse unimportable Runtime
  stereo recordings"). No `type:` prefix, no trailing period. No body unless
  the reason cannot be read from the diff; then at most five lines. No
  trailers of any kind: no `Co-Authored-By`, no generated-with footer, no
  session links. No `wip`, `fixup`, `cleanup` or review-fix commits: fold them
  into the commit they belong to.
- **Private material is never tracked:** `int_docs/`, `.claude/settings.local.json`
  and similar stay git-ignored, and CI's `public-boundary` job rejects them.
  Never `git add -f`. No commits that only add or edit notes or documents;
  README changes ride the change that made it false.
- **A change names what proves it.** Write the test that fails without the
  fix, see it fail for the stated reason, then fix. If it cannot be tested
  automatically, say so in the pull request in one sentence and say what was
  run by hand.
- **No stubs, MVPs or ghosts.** No `todo!()`, `unimplemented!()`, placeholder
  returns, unused parameters, dead or unwired code, flags nothing reads.
  Finish the path end to end or do not ship it.
- **The UI never oversells.** A control is enabled only when it works end to
  end. Unavailable means disabled with the reason, or absent. Text never
  claims saved, verified, recording, installed or connected before that is
  true, and never claims more than README's product limits (no MIDI/CV
  execution, no third-party plug-in hosting, no physical-device acceptance).
- **Comments.** None that narrate or describe what code does. At most a short
  line for a non-obvious invariant or edge case, saying why. `///` only on
  public items, stating the contract. In new code a lint is silenced with
  `#[allow(lint, reason = "…")]`, never bare.
- **Real-time code** (audio callbacks, `audio-engine`, `mixer-runtime`,
  `native-device`, `clip-engine`, `arrangement-renderer`, `media-cache`,
  `recording-transport`): no allocation, locks, I/O or panics on the callback
  path. Prove it with the crate's `tests/realtime_allocation.rs`
  (`test-support/realtime_allocator.rs`).
- **Strictness is not negotiable.** `unsafe_code = deny`, clippy `all` and
  `pedantic` deny, `--locked` everywhere, edition 2024, Rust 1.93.0. A new
  dependency is pinned and added to `THIRD_PARTY_NOTICES.md`
  (`contracts/THIRD_PARTY_NOTICES.md` for npm).
- **A design choice is researched before it is proposed.** When a change turns
  on a choice nobody has settled, look in this order: this file and the
  skills, the existing code, the standard or protocol (RIFF/WAV, FLAC, JSON,
  PipeWire, WASAPI), then what comparable audio software chose. Sources and
  trade-offs go under **Prior art** in the issue or pull request. Alternatives
  offered without that are guesses.
- **Architectural changes are agreed first.** New crate, new cross-crate
  trait, new on-disk format, new thread or ownership rule: state the problem,
  the alternatives and the cost, then wait for the maintainer.
- **Pull request.** Merged by squash, so the title is the commit subject (same
  rules as commits) and the body is what lands in history: a few plain
  sentences, no headings, checklists or tooling footers. Branch commits are for
  review; keep them atomic anyway.
- **An issue is opened only when someone is worse off today and the fix needs
  an acceptance test this change cannot state.** A nit or tidier spelling is a
  sentence in the pull request or nothing. A defect at a definition you are
  already editing is fixed in this change, however many lines it takes.

## Commands

Hooks and scripts need `bash`, `jq` and `perl`.

```bash
cargo fmt --all
cargo clippy --workspace --all-targets --all-features --locked -- -D warnings
cargo test --workspace --exclude hyper-desktop-ide --all-targets --locked
cargo test -p <crate> --locked                       # one crate
cargo test --locked --release -p hyper-audio-engine -p hyper-mixer-runtime -p hyper-desktop-audio
cargo doc --workspace --no-deps --locked
cargo test --manifest-path third_party/pipewire/Cargo.toml

# IDE worker tests need a PipeWire graph on Linux
dbus-run-session -- bash -c '
  source software/IDE/audio-backend/scripts/pipewire-virtual-graph.sh
  cargo test -p hyper-desktop-ide --all-targets --locked -- --test-threads=1'

(cd contracts && npm ci --ignore-scripts && npm test)  # Node >= 22
node contracts/scripts/check-public-boundary.mjs
```

## What proves what

| Change to | Proved by |
| --- | --- |
| DSP, mixing, rendering, recording transport | unit tests beside the code and `tests/realtime_allocation.rs` |
| JSON formats, schemas, bundles, deployment | `contracts/` schema, `fixtures/{valid,invalid}`, `scripts/test-*.mjs`, and the Rust loader tests |
| IDE state and workers | `software/IDE/application` tests |
| IDE screens | `software/IDE/desktop-ui` tests on the Slint testing backend |
| Runtime host, deployment, panels | tests under `software/RUNTIME/*` and the packaged deployment smoke |
| PipeWire / WASAPI paths | virtual-graph tests on Linux, CI on Windows; physical devices are manual, say what was run |
| Packaging, CI | the workflow itself; run the changed command locally first |

## Working on a change

1. `/spec` turns a rough idea into an issue (`.github/ISSUE_TEMPLATE/spec.yml`)
   with an acceptance test. Read the code, not the description.
2. `.claude/scripts/worktree.sh <kind>/<slug>` creates the branch in its own
   worktree.
3. `debt-scout` over the area to be touched, before any code: each finding is
   fix here, issue, or ignore.
4. Failing test, then the change. Follow the pattern already in the file.
5. `continuous-simplify`: it edits the code, so everything below runs after it.
   A task that is itself a refactor uses `refactoring` on a `refactor/` branch
   instead, never mixed with a feature or fix.
6. Run the commands above for what you touched, and
   `.claude/scripts/lint-diff.sh` for stubs, markers and bare lint allows.
7. Commit atomically, in build order: data and types, behaviour, then the API
   or screen that exposes it.
8. `reviewer` subagent over the commits, once, before the pull request exists.
   Zero blocking findings is the pass; notes are not cleared for their own
   sake. An architectural finding stops the work for the maintainer.
9. Push the branch and open the pull request with the template filled in. If
   code changed since step 5, run `continuous-simplify` again before pushing.

`/implement <issue>` does all of it; `/harness` finds what a change leaves
unwatched. Skills in `.claude/skills/` (`debt-scout`, `continuous-simplify`,
`refactoring`, `realtime`, `audio-io`, `contracts`, `ide`, `runtime`,
`packaging`) load when the task or area calls for them.

## What watches the workflow

`contracts/scripts/test-contributor-workflow.mjs` (part of `npm test`) fails
when this file, the commands, skills or templates name a path or package that
no longer exists, when branch kinds drift between here and the hooks, when a
skill is not listed here, when a hook is missing or not executable, or when the
steps fall out of order. `.claude/hooks/guard.test.sh` is the table for the guards,
the commit-msg hook and `lint-diff.sh`; the `change-hygiene` CI job runs it,
and checks every pull request's lines, commits, branch and title.
