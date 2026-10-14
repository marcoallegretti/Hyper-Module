---
name: continuous-simplify
description: Behaviour-preserving cleanup of only the code a change touched, before it is committed or pushed. Use after the tests pass and before committing, or before pushing a branch; not for whole-repo sweeps.
---

# Continuous simplify

Scope is the diff: `git diff HEAD` (working tree and staged), or
`git diff origin/main...HEAD` when the tree is clean. Never the rest of the
file or a crate-wide sweep; that is `refactoring`, on its own branch.

Deterministic first, and stop if anything is red:

1. `cargo fmt --all` and `.claude/scripts/lint-diff.sh`.
2. `cargo test -p <crate> --locked` for each crate touched. Simplifying red code
   hides which change broke it.

Then one reading of the added lines, in this order:

- Ghosts: unused parameters, unwired options, dead branches, helpers with no
  caller, `pub` that can be private.
- Duplication this change introduced or deepened: 10 or more lines, or 3 or more
  sites, becomes one helper beside the existing one. Less is left alone.
- Control flow: early returns, `?`, `let … else`, matches that collapse.
- Hand-rolled std: iterator, `Option` and slice methods, `clamp`, `abs_diff`,
  `checked_*`.
- Narrating comments are deleted; an invariant keeps one line saying why.
- Real-time crates: no edit may add allocation, locks or panics on the callback
  path. If unsure, leave it.

Rules: behaviour identical; a test changes only by rename, never to pass; no
abstraction for a single use; no formatting-only churn; no more than about ten
edits per run.

Apply the edits, rerun the same tests, report one line per edit
(`file:line`, what) or "nothing to simplify". A dirty tree takes the edits into
the commit being built. Already committed: amend only if the edit belongs to
the tip (`git commit --amend --no-edit`); otherwise say so and leave it. No
cleanup commits.
