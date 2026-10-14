---
description: Turn a rough idea into an issue whose acceptance criterion is a test
argument-hint: <what you noticed or want to be true>
---

Write a specification for the following. No implementation.

$ARGUMENTS

1. **Verify it is real.** Read the code; locate the actual site. If it differs
   from the description, say so. If two readings lead to different work, ask
   once with both spelled out; otherwise decide and state the assumption.
2. **Research a real design fork** (state ownership, thread model, format
   shape) in this order: AGENTS.md and the skills, the existing code, the
   standard or protocol, comparable software. Put sources under **Prior art**.
   Skip for a defect with one obvious fix.
3. **Write it.** Title is a sentence stating the defect or the truth that
   should hold. Body, in order: **Observed** (file and line), **Expected**
   (checkable), **Acceptance** (the failing test: file, assertion; if it
   cannot be automated, say so and name the manual check), **Non-goals**,
   **Where it lives**, **Prior art** (only if needed).
4. Show the body, get agreement, then create the issue and print its URL. A
   new crate, trait, format or ownership rule is labelled `needs-design`.
