---
description: Find what the current change leaves unwatched and propose the smallest test that closes it
argument-hint: [area or file; defaults to the diff]
---

Target: $ARGUMENTS (default `git diff HEAD`).

1. List the observable behaviours the change alters: output samples, timing,
   file contents, protocol messages, UI state, error cases.
2. For each, invert the code cheaply (flip a comparison, drop a guard, change a
   constant) and run the relevant tests. Classify: covered by a named test that
   fails / covered only incidentally / not covered.
3. Report a table (behaviour, covering test, verdict), then the single test
   worth writing first, where it goes, and what it asserts. Prefer a unit test,
   then a crate integration test, then a contracts script. Offer to write it;
   do not write it unasked.
