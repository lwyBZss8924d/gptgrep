const ALLOWED_SURFACES = ["crates/", "skills/", "runtime/", "prompts/"];
export function validateMutationPaths(paths: string[]): void {
  if (!paths.length || new Set(paths).size !== paths.length) throw new Error("Explicit unique mutation paths are required");
  for (const path of paths) {
    if (path.startsWith("/") || path.includes("\\") || path.split("/").some(p => !p || p === "." || p === "..") ||
        !ALLOWED_SURFACES.some(prefix => path.startsWith(prefix))) throw new Error(`Mutation outside product/harness surfaces: ${path}`);
  }
}

