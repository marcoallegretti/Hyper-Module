import { spawnSync } from "node:child_process";
import { existsSync, readFileSync, readdirSync, statSync } from "node:fs";
import { dirname, join, relative, resolve } from "node:path";
import { fileURLToPath } from "node:url";

const root = resolve(dirname(fileURLToPath(import.meta.url)), "..", "..");
const problems = [];
const fail = (message) => problems.push(message);
const read = (path) => readFileSync(join(root, path), "utf8");

function walk(directory, accept, found = []) {
  if (!existsSync(directory)) return found;
  for (const name of readdirSync(directory)) {
    if (name === "node_modules" || name === "target" || name === ".git") continue;
    const path = join(directory, name);
    if (statSync(path).isDirectory()) walk(path, accept, found);
    else if (accept(path)) found.push(path);
  }
  return found;
}

const documents = [
  "AGENTS.md",
  "CLAUDE.md",
  ".github/pull_request_template.md",
];

const tracked = spawnSync("git", ["ls-files", "--stage"], { cwd: root, encoding: "utf8" });
if (tracked.status !== 0 || typeof tracked.stdout !== "string") {
  console.error(`git ls-files failed: ${tracked.error?.message ?? tracked.stderr}`);
  process.exit(1);
}
const modes = new Map(
  tracked.stdout.split("\n").flatMap((line) => {
    const m = /^(\d+) \S+ \d\t(.+)$/.exec(line);
    return m ? [[m[2], m[1]]] : [];
  }),
);
for (const path of modes.keys()) {
  if (path.startsWith(".claude/") && path.endsWith(".md")) documents.push(path);
}

const knownPackages = new Set(
  walk(root, (p) => p.endsWith("Cargo.toml") && !p.includes("third_party"))
    .flatMap((p) => [...readFileSync(p, "utf8").matchAll(/^name\s*=\s*"([^"]+)"/gm)])
    .map((m) => m[1]),
);
const binaries = walk(root, (p) => /[\\/]src[\\/]bin[\\/][^\\/]+\.rs$/.test(p)).map(
  (p) => p.split(/[\\/]/).at(-1).replace(/\.rs$/, ""),
);
binaries.forEach((name) => knownPackages.add(name));

const testDirectories = walk(root, (p) => p.endsWith(".rs") && /[\\/]tests[\\/]/.test(p)).map(
  (p) => relative(root, p).replaceAll("\\", "/"),
);

const notFiles = new Set([".claude/settings.local.json"]);
const notPackages = new Set(["hyper-ci-test-report-v1"]);
const rootPath =
  /(?:^|[\s'"`(])((?:\.claude|\.github|crates|software|contracts|test-support|third_party)\/[^\s'"`)<>]+)/g;
const rootFiles = /(?:^|[\s'"`(])((?:AGENTS|CLAUDE|README|THIRD_PARTY_NOTICES)\.md|Cargo\.(?:toml|lock)|rust-toolchain\.toml)(?=[\s'"`).,;:]|$)/g;

for (const document of documents) {
  const text = read(document);

  for (const match of text.matchAll(rootPath)) {
    const path = match[1].replace(/:\d+$/, "").replace(/[.,;:]+$/, "");
    if (notFiles.has(path) || /[*{}$]/.test(path)) continue;
    if (!existsSync(join(root, path))) fail(`${document}: names ${path}, which does not exist`);
  }
  for (const match of text.matchAll(rootFiles)) {
    if (!existsSync(join(root, match[1]))) fail(`${document}: names ${match[1]}, which does not exist`);
  }
  for (const match of text.matchAll(/`(tests\/[^`*{}<> ]+)`/g)) {
    if (!testDirectories.some((p) => p.endsWith(`/${match[1]}`))) {
      fail(`${document}: names ${match[1]}, which no crate has`);
    }
  }
  for (const match of text.matchAll(/\bhyper-[a-z0-9]+(?:-[a-z0-9]+)*\b/g)) {
    if (!knownPackages.has(match[0]) && !notPackages.has(match[0])) {
      fail(`${document}: names ${match[0]}, which is not a package or binary here`);
    }
  }
}

function frontmatter(path) {
  const match = /^---\n([\s\S]*?)\n---\n/.exec(read(path));
  if (!match) return null;
  return Object.fromEntries(
    match[1].split("\n").flatMap((line) => {
      const pair = /^([a-z-]+):\s*(.*)$/.exec(line);
      return pair ? [[pair[1], pair[2]]] : [];
    }),
  );
}

for (const document of documents.filter((d) => d.startsWith(".claude/"))) {
  const data = frontmatter(document);
  if (!data) {
    fail(`${document}: missing frontmatter`);
    continue;
  }
  if (!data.description || data.description.length < 30) fail(`${document}: description missing or too short`);
  if (document.includes("/skills/")) {
    const directory = document.split("/").at(-2);
    if (data.name !== directory) fail(`${document}: name '${data.name}' is not its directory '${directory}'`);
  }
  if (document.includes("/agents/") && (!data.name || !data.tools)) fail(`${document}: agent needs name and tools`);
}

const settings = JSON.parse(read(".claude/settings.json"));
const hookCommands = Object.values(settings.hooks ?? {})
  .flat()
  .flatMap((entry) => entry.hooks.map((hook) => hook.command));
for (const command of hookCommands) {
  const path = command.replace(/^"?\$\{CLAUDE_PROJECT_DIR\}"?\//, "");
  if (!existsSync(join(root, path))) fail(`settings.json: hook ${path} does not exist`);
}
for (const [path, mode] of modes) {
  if (path.startsWith(".claude/") && path.endsWith(".sh") && mode !== "100755") {
    fail(`${path}: tracked without the executable bit`);
  }
}
for (const rule of settings.permissions?.allow ?? []) {
  if (!/^[A-Za-z_]+\(.+\)$/.test(rule)) fail(`settings.json: malformed permission '${rule}'`);
}

const kinds = /BRANCH_KINDS='([^']+)'/.exec(read(".claude/hooks/lib.sh"))?.[1].split("|") ?? [];
const documentedKinds = [...(/Kind is ([^.]*)\./s.exec(read("AGENTS.md"))?.[1] ?? "").matchAll(/`([a-z]+)`/g)].map((m) => m[1]);
if (kinds.length === 0 || kinds.join() !== documentedKinds.join()) {
  fail(`AGENTS.md branch kinds (${documentedKinds}) differ from hooks/lib.sh (${kinds})`);
}
function numbered(text, heading) {
  const start = text.indexOf(heading);
  const section = start < 0 ? text : text.slice(start);
  const items = [...section.matchAll(/^(\d+)\. (.*(?:\n(?!\d+\. |\n|#).*)*)/gm)];
  return items.map((m) => ({ n: Number(m[1]), text: m[2] }));
}
function inOrder(label, items, needles) {
  items.forEach((item, i) => item.n === i + 1 || fail(`${label}: list numbering breaks at ${item.n}`));
  let last = -1;
  for (const needle of needles) {
    const index = items.findIndex((item) => item.text.includes(needle));
    if (index < 0) fail(`${label}: no step mentions '${needle}'`);
    else if (index < last) fail(`${label}: '${needle}' comes too early`);
    else last = index;
  }
}
inOrder("AGENTS.md", numbered(read("AGENTS.md"), "## Working on a change"), [
  "/spec",
  "worktree.sh",
  "debt-scout",
  "Failing test",
  "continuous-simplify",
  "commands above",
  "Commit atomically",
  "reviewer",
  "Push the branch",
]);
inOrder("implement.md", numbered(read(".claude/commands/implement.md"), "Implement issue"), [
  "debt-scout",
  "Failing test first",
  "continuous-simplify",
  "Checks",
  "Commit",
  "reviewer",
  "Push and open",
]);
for (const document of ["AGENTS.md", ".claude/commands/implement.md"]) {
  if (!read(document).replace(/\s+/g, " ").includes("Zero blocking findings is the pass")) {
    fail(`${document}: lost the rule that zero blocking findings is the pass`);
  }
}
const agents = read("AGENTS.md");
for (const path of modes.keys()) {
  const skill = /^\.claude\/skills\/([^/]+)\/SKILL\.md$/.exec(path)?.[1];
  if (skill && !agents.includes(`\`${skill}\``)) fail(`AGENTS.md does not list the ${skill} skill`);
}
for (const level of ["Blocks", "Worth answering", "Note"]) {
  if (!read(".claude/agents/reviewer.md").includes(`**${level}`)) fail(`reviewer.md: lost the '${level}' level`);
}

const specLabels = [...read(".github/ISSUE_TEMPLATE/spec.yml").matchAll(/label: (.+)/g)].map((m) => m[1]);
for (const label of specLabels) {
  if (!read(".claude/commands/spec.md").includes(`**${label}**`)) fail(`spec.md does not ask for '${label}'`);
}

if (problems.length > 0) {
  console.error(problems.map((p) => `- ${p}`).join("\n"));
  process.exitCode = 1;
} else {
  console.log(`Contributor workflow passed: ${documents.length} documents, ${hookCommands.length} hooks`);
}
