---
description: Take an issue from acceptance criterion to pull request
argument-hint: <issue number>
---

Implement issue $ARGUMENTS end to end.

1. **Read it.** No acceptance criterion: stop and offer `/spec`. Labelled
   `needs-design`: stop; the maintainer decides first.
2. **Worktree.** `.claude/scripts/worktree.sh <kind>/<slug>`; work only there.
   Load the skill for the area touched.
3. **`debt-scout`** over the area, before any code. Fix here what sits at a
   definition you will edit; report the rest as issue or ignore.
4. **Failing test first.** Run it; confirm it fails for the stated reason.
5. **Change.** Inside the non-goals, following the file's existing patterns.
   No stubs, dead code or narrating comments.
6. **`continuous-simplify`**, before the checks, so they run on the final code.
   A refactor task uses `refactoring` on its own branch instead.
7. **Checks** from AGENTS.md for every area touched, then
   `.claude/scripts/lint-diff.sh`. Fix; never skip, loosen or delete a test to
   get green.
8. **Commit** atomically, in build order. Subjects per AGENTS.md; no body, no
   trailers.
9. **`reviewer` subagent**, once, over the commits. Zero blocking findings is
   the pass; blocking ones stop the work. Act on worth-answering ones or say why
   not in the PR. If it did not run, say that, not "nothing found".
10. **Push and open the PR** with the template filled in. Run
    `continuous-simplify` first if code changed since step 6. Title like a
    commit subject; `Closes #$ARGUMENTS`. Open an issue only for what must
    outlive the PR and needs its own acceptance test.

Report in five lines: what changed, what proves it, review result, what was
checked by hand, what is left and why.
