---
name: debt-scout
description: Read-only scan of the area a contribution will touch for debt it will run into, with a fix-here, issue or ignore verdict for each. Use before writing code, when starting a contribution or picking up an issue.
---

# Debt scout

Read-only. Scope is the paths the task names (the issue's "Where it lives", or
the crates about to be edited), never the whole repository.

Deterministic first, with `git grep` and the tools, not by reading:

- Stubs and markers: `todo!`, `unimplemented!`, `dbg!`, `TODO|FIXME|XXX|HACK`,
  "not implemented", "coming soon".
- Lints silenced without a `reason`, and `#[allow(dead_code)]`.
- `#[ignore]` tests that no step of `.github/workflows/ci.yml` runs with
  `--ignored`.
- UI that oversells (`.slint`, README): callbacks declared and never set,
  controls enabled with no handler, text claiming saved, verified, recording,
  installed or connected, anything beyond README's product limits.
- Duplicate dependency versions: `cargo tree -d --locked` for the crates in scope.
- Platform gaps: `cfg(windows)` code with no Linux counterpart, or the reverse.
- Size: the single largest file in scope. Report only; splitting is
  `refactoring`.

Read only the hits inside scope. Give each a verdict by the issue bar in
AGENTS.md:

- **Fix here:** a defect at a definition this change will edit.
- **Issue:** someone is worse off today and the fix needs its own acceptance
  test. Give a title plus Observed and Acceptance in two lines. Do not open it
  unasked.
- **Ignore:** nobody is worse off. Say nothing about it.

Output at most ten lines, ranked. "Nothing in scope" is a valid result. Edit
nothing and do not start the change.
