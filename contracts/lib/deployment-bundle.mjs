import { verifyBundleArtifact } from "../scripts/verify-bundle.mjs";

export async function inspectDeploymentBundle(bundlePath, options = {}) {
  return verifyBundleArtifact(bundlePath, options);
}
