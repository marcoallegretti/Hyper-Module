import assert from "node:assert/strict";

import {
  findPrivateTrackedPaths,
  privatePathReason,
} from "../lib/public-boundary.mjs";

const rejected = [
  "int_docs/design.md",
  "INT_DOCS/design.md",
  "nested/Int_Docs/design.md",
  "LLM/prompt.md",
  ".OCR/session/final.md",
  ".Codex/settings.json",
  "nested/.Agents/state.json",
  "AGENTS.md",
  "nested/Claude.MD",
  ".GITHUB/Commands/review.md",
  ".github/INSTRUCTIONS/policy.md",
  ".github/Prompts/review.md",
  ".GitHub/Copilot-Instructions.md",
  "nested\\AI_WORKSPACE\\state.json",
  "I\u0307NT_DOCS/normalized-variant.md",
  "docs/architecture/overview.md",
  "DOCS/ARCHITECTURE/overview.md",
  "contracts/README.md",
  "Contracts/ReadMe.MD",
];

const accepted = [
  ".github/workflows/ci.yml",
  "contracts/scripts/check-public-boundary.mjs",
  "crates/audio-engine/src/lib.rs",
  "software/IDE/audio-backend/src/lib.rs",
  "tooling/public-build-script.mjs",
  "docs/user-guide.md",
  "agents.txt",
];

for (const path of rejected) {
  assert.notEqual(privatePathReason(path), null, `expected rejection: ${path}`);
}

for (const path of accepted) {
  assert.equal(privatePathReason(path), null, `expected acceptance: ${path}`);
}

assert.deepEqual(
  findPrivateTrackedPaths([...accepted, ...rejected]).map(({ path }) => path),
  rejected,
);

assert.throws(() => privatePathReason(""), /non-empty string/);
assert.throws(() => privatePathReason("safe\0unsafe"), /without NUL/);

console.log(
  `Public boundary policy passed: ${rejected.length} rejected and ${accepted.length} accepted cases`,
);
