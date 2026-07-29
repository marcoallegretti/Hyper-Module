import assert from "node:assert/strict";
import { lstat, opendir } from "node:fs/promises";
import { join } from "node:path";

export async function readBoundedDirectory(
  path,
  {
    maximumEntries,
    maximumNameBytes,
    label = "directory",
  },
) {
  for (const [name, value] of Object.entries({
    maximumEntries,
    maximumNameBytes,
  })) {
    assert(
      Number.isSafeInteger(value) && value >= 0,
      `${name} must be a non-negative safe integer`,
    );
  }
  const entries = [];
  let nameBytes = 0;
  const directory = await opendir(path);
  for await (const entry of directory) {
    assert(
      entries.length < maximumEntries,
      `${label} exceeds ${maximumEntries} entries`,
    );
    nameBytes += Buffer.byteLength(entry.name, "utf8");
    assert(
      nameBytes <= maximumNameBytes,
      `${label} entry names exceed ${maximumNameBytes} bytes`,
    );
    entries.push(entry);
  }
  return entries;
}

export function directoryPrefixes(paths) {
  const directories = new Set();
  for (const path of paths) {
    const segments = path.split("/");
    for (let length = 1; length < segments.length; length += 1) {
      directories.add(segments.slice(0, length).join("/"));
    }
  }
  return directories;
}

export async function collectRegularFiles(
  rootPath,
  {
    maximumEntries = 32_768,
    maximumDirectories = 8_192,
    maximumFiles = 16_384,
    maximumDepth = 64,
    maximumPathBytes = 8 * 1024 * 1024,
    allowDirectory = () => true,
    allowFile = () => true,
  } = {},
) {
  for (const [name, value] of Object.entries({
    maximumEntries,
    maximumDirectories,
    maximumFiles,
    maximumDepth,
    maximumPathBytes,
  })) {
    assert(
      Number.isSafeInteger(value) && value > 0,
      `${name} must be a positive safe integer`,
    );
  }
  assert.equal(
    typeof allowDirectory,
    "function",
    "allowDirectory must be a function",
  );
  assert.equal(typeof allowFile, "function", "allowFile must be a function");

  const files = [];
  const pending = [{ directory: rootPath, prefix: "", depth: 0 }];
  let entryCount = 0;
  let directoryCount = 1;
  let pathBytes = 0;

  while (pending.length > 0) {
    const { directory, prefix, depth } = pending.pop();
    const entries = await opendir(directory);
    for await (const entry of entries) {
      const relativePath = prefix ? `${prefix}/${entry.name}` : entry.name;
      const absolutePath = join(directory, entry.name);
      entryCount += 1;
      pathBytes += Buffer.byteLength(relativePath, "utf8");
      assert(
        entryCount <= maximumEntries,
        `filesystem tree exceeds ${maximumEntries} entries`,
      );
      assert(
        pathBytes <= maximumPathBytes,
        `filesystem tree paths exceed ${maximumPathBytes} bytes`,
      );
      const status = await lstat(absolutePath);
      assert(
        !status.isSymbolicLink(),
        `${relativePath}: symbolic links and junctions are forbidden`,
      );
      if (status.isDirectory()) {
        const childDepth = depth + 1;
        directoryCount += 1;
        assert(
          childDepth <= maximumDepth,
          `filesystem tree exceeds depth ${maximumDepth}`,
        );
        assert(
          directoryCount <= maximumDirectories,
          `filesystem tree exceeds ${maximumDirectories} directories`,
        );
        assert(
          allowDirectory(relativePath),
          `${relativePath}: undeclared directory is forbidden`,
        );
        pending.push({
          directory: absolutePath,
          prefix: relativePath,
          depth: childDepth,
        });
      } else {
        assert(
          status.isFile(),
          `${relativePath}: unsupported filesystem entry`,
        );
        assert(
          allowFile(relativePath),
          `${relativePath}: undeclared file is forbidden`,
        );
        files.push(relativePath);
        assert(
          files.length <= maximumFiles,
          `filesystem tree exceeds ${maximumFiles} files`,
        );
      }
    }
  }
  return files;
}
