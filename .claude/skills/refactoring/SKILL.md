---
name: refactoring
description: Behaviour-preserving structural change (move, split, rename, extract) on its own branch. Use when the task is a refactor or when debt-scout or review finds misplaced or duplicated code; never mixed with a feature or fix.
---

# Refactoring

One refactor, one `refactor/<slug>` branch, one pull request. A behaviour change
found on the way is a separate change.

Before touching code:

1. Inventory the module:
   `git grep -n -E '^\s*(pub(\([a-z]+\))? )?fn ' -- <path>`. Group by purpose.
   Outliers, and pairs that duplicate each other, are the candidates. Pick one.
2. Prove current behaviour: the module's tests pass first. A behaviour you will
   move that has no test gets a characterization test, in its own commit, before
   the move.
3. Stop and ask the maintainer if it needs a new crate, a new cross-crate trait,
   or changes who owns something or which thread runs it. `crates/` never depends
   on `software/`; keep it so.

Then:

- Pure moves and renames first, with no logic edit, in their own commit.
  `git diff --color-moved` must show only moves.
- The extraction or merge the move enables goes in a second commit.
- Visibility only narrows. A crate's public paths change only in the same commit
  as every call site.
- Tests change only in paths and imports. An assertion that must change means
  behaviour changed: stop.
- Real-time crates: `tests/realtime_allocation.rs` passes unchanged, and nothing
  adds allocation, dynamic dispatch or locks on the callback path.
- `cfg` gates move with the code they gate; read the other platform's gates,
  since CI is the only Windows run.
- `software/IDE/desktop-ui/src/main.rs` and
  `software/IDE/application/src/{lib,worker}.rs` are each tens of thousands of
  lines. One refactor takes one coherent slice, such as one feature's helpers,
  never "split the file".

Finish with fmt, clippy, `cargo test -p <crate> --locked` for every crate whose
paths changed, and the workspace tests for dependents. Report what moved where
and the test that proves nothing else changed.
