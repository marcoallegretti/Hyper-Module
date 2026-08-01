import assert from "node:assert/strict";
import { mkdtemp, writeFile } from "node:fs/promises";
import { basename, dirname, join, resolve } from "node:path";
import { pathToFileURL } from "node:url";

import canonicalize from "canonicalize";

import { openRetainedDirectory } from "../lib/durable-files.mjs";
import { removeRetainedDirectory } from "../lib/native-fs-authority.mjs";
import { buildBundle } from "./build-bundle.mjs";
import {
  defaultMaximumProjectBytes,
  verifyProject,
} from "./verify-project.mjs";

export async function buildProjectBundle(
  projectPath,
  outputPath,
  minimumRuntimeVersion,
  options = {},
) {
  assert(
    typeof minimumRuntimeVersion === "string" &&
      minimumRuntimeVersion.length > 0,
    "minimum runtime version is required",
  );
  const sourceRoot = resolve(projectPath);
  const outputRoot = resolve(outputPath);
  const maximumBundleBytes =
    options.maximumBundleBytes ?? defaultMaximumProjectBytes;
  const planObserver = options.planObserver ?? (async () => {});
  assert(
    Number.isSafeInteger(maximumBundleBytes) && maximumBundleBytes > 0,
    "maximum bundle bytes must be a positive safe integer",
  );
  assert.equal(typeof planObserver, "function", "plan observer must be a function");
  const verified = await verifyProject(sourceRoot, {
    maximumProjectBytes: maximumBundleBytes,
  });
  const project = verified.project;
  const sourceProjectSha256 = verified.identity.slice("sha256:".length);

  const payloads = [
    {
      source_path: "project.json",
      path: "project.json",
      role: "runtime-project",
      media_type: "application/json",
    },
    ...project.resources.map((resource) => ({
      source_path: resource.path,
      path: resource.path,
      role: "runtime-data",
      media_type: "application/json",
    })),
    ...project.assets.map((asset) => ({
      source_path: asset.path,
      path: asset.path,
      role: "audio",
      media_type: asset.media_type,
    })),
  ];
  const plan = {
    format: "hyper-runtime-build-plan",
    format_version: 1,
    project_id: project.project_id,
    project_revision: project.revision,
    source_project_sha256: sourceProjectSha256,
    minimum_runtime_version: minimumRuntimeVersion,
    entrypoint: "project.json",
    required_capabilities: project.required_capabilities,
    payloads,
    extensions: {},
  };

  const planDirectory = await mkdtemp(
    join(dirname(outputRoot), `.${basename(outputRoot)}.plan-`),
  );
  const planHandle = await openRetainedDirectory(planDirectory);
  let result;
  let operationError;
  try {
    const planPath = join(planDirectory, "build-plan.json");
    await writeFile(planPath, canonicalize(plan), { flag: "wx" });
    result = await buildBundle(planPath, sourceRoot, outputRoot, options);
    await planObserver("before-plan-cleanup", { planDirectory });
  } catch (error) {
    operationError = error;
  }

  let cleanupError;
  try {
    await removeRetainedDirectory(
      planHandle,
      planDirectory,
      "project build-plan directory",
    );
  } catch (error) {
    cleanupError = error;
  } finally {
    await planHandle.close();
  }
  if (operationError && cleanupError) {
    throw new AggregateError(
      [operationError, cleanupError],
      "project bundle construction and plan cleanup both failed",
    );
  }
  if (operationError) throw operationError;
  if (cleanupError) throw cleanupError;
  return result;
}

const invokedPath = process.argv[1]
  ? pathToFileURL(resolve(process.argv[1])).href
  : undefined;
if (invokedPath === import.meta.url) {
  assert.equal(
    process.argv.length,
    5,
    "usage: node scripts/build-project-bundle.mjs <project-directory> <output-directory> <minimum-runtime-version>",
  );
  const result = await buildProjectBundle(
    process.argv[2],
    process.argv[3],
    process.argv[4],
  );
  console.log(result.identity);
}
