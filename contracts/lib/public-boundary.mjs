const PRIVATE_DIRECTORY_NAMES = new Set([
  ".agents",
  ".claude",
  ".codex",
  ".cursor",
  ".gemini",
  ".mcp",
  ".ocr",
  "ai_workspace",
  "int_docs",
  "llm",
]);

const PRIVATE_INSTRUCTION_FILES = new Set([
  "agents.md",
  "claude.md",
  "codex.md",
  "gemini.md",
]);

const PRIVATE_GITHUB_DIRECTORIES = new Set([
  "commands",
  "instructions",
  "prompts",
]);

const PRIVATE_GITHUB_FILES = new Set([
  "copilot-instructions.md",
]);

function policyName(value) {
  return value
    .normalize("NFKD")
    .replace(/\p{Mark}/gu, "")
    .toLowerCase();
}

function pathSegments(path) {
  if (typeof path !== "string" || path.length === 0 || path.includes("\0")) {
    throw new TypeError("tracked path must be a non-empty string without NUL");
  }

  return path.replaceAll("\\", "/").split("/").filter(Boolean);
}

export function privatePathReason(path) {
  const segments = pathSegments(path);
  const names = segments.map(policyName);

  for (const name of names) {
    if (PRIVATE_DIRECTORY_NAMES.has(name)) {
      return `private directory component '${name}'`;
    }
  }

  const basename = names.at(-1);
  if (PRIVATE_INSTRUCTION_FILES.has(basename)) {
    return `private instruction file '${basename}'`;
  }

  if (names[0] === ".github") {
    if (names.length >= 2 && PRIVATE_GITHUB_DIRECTORIES.has(names[1])) {
      return `private GitHub directory '.github/${names[1]}'`;
    }
    if (names.length === 2 && PRIVATE_GITHUB_FILES.has(names[1])) {
      return `private GitHub file '.github/${names[1]}'`;
    }
  }

  return null;
}

export function findPrivateTrackedPaths(paths) {
  const violations = [];
  for (const path of paths) {
    const reason = privatePathReason(path);
    if (reason !== null) {
      violations.push({ path, reason });
    }
  }
  return violations;
}

