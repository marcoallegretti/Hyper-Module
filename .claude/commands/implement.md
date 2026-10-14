---
description: Take an issue from acceptance criterion to pull request
argument-hint: <issue number>
---

Implement issue $ARGUMENTS end to end.

1. **Read it.** No acceptance criterion: stop and offer `/spec`. Labelled
   `needs-design`: stop; the maintainer decides first.
2. **Worktree.** `.claude/scripts/worktree.sh <kind>/<slug>`; work only there.
   Load the skill for the area touched.
3. **Failing test first.** Run it; confirm it fails for the stated reason.
4. **Change.** Inside the non-goals, following the file's existing patterns.
   No stubs, dead code or narrating comments.
5. **`/simplify`**, before the checks, so they run on the final code.
6. **Checks** from AGENTS.md for every area touched, then
   `.claude/scripts/lint-diff.sh`. Fix; never skip, loosen or
   delete a test to get green.
7. **Commit** atomically, in build order. Subjects per AGENTS.md; no body, no
   trailers.
8. **`reviewer` subagent**, once, over the commits. Zero blocking findings is
   the pass; blocking ones stop the work. Act on worth-answering ones or say why not in the PR. If it did not
   run, say that, not "nothing found".
9. **Push and open the PR** with the template filled in. Title like a commit
   subject; `Closes #$ARGUMENTS`. Open an issue only for what must outlive
   the PR and needs its own acceptance test.

Report in five lines: what changed, what proves it, review result, what was
checked by hand, what is left and why.
