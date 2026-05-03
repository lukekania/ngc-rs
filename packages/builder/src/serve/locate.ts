import * as fs from 'node:fs';
import * as path from 'node:path';

export interface LocateResult {
  binary: string;
  source: 'option' | 'env' | 'node-modules' | 'workspace-target' | 'path';
}

const BINARY_NAME = process.platform === 'win32' ? 'ngc-rs.exe' : 'ngc-rs';

export function locateNgcRs(
  workspaceRoot: string,
  optionOverride: string | null,
  env: NodeJS.ProcessEnv = process.env,
): LocateResult {
  if (optionOverride) {
    const resolved = path.resolve(workspaceRoot, optionOverride);
    return { binary: resolved, source: 'option' };
  }

  const fromEnv = env['NGC_RS_BINARY'];
  if (fromEnv) {
    const resolved = path.isAbsolute(fromEnv)
      ? fromEnv
      : path.resolve(workspaceRoot, fromEnv);
    return { binary: resolved, source: 'env' };
  }

  // npm-distributed binary: `@ngc-rs/cli` ships a wrapper plus a
  // platform-specific `@ngc-rs/cli-{platform}-{arch}` package via
  // optionalDependencies (esbuild/biome/swc pattern). Resolved against
  // the workspace root so we don't accidentally pick up a binary from
  // this builder package's own node_modules in development.
  const platformPkg = `@ngc-rs/cli-${process.platform}-${process.arch}`;
  try {
    const pkgJson = require.resolve(`${platformPkg}/package.json`, {
      paths: [workspaceRoot],
    });
    const candidate = path.join(path.dirname(pkgJson), 'bin', BINARY_NAME);
    if (fileIsExecutable(candidate)) {
      return { binary: candidate, source: 'node-modules' };
    }
  } catch {
    // Platform package not installed — fall through.
  }

  const workspaceCandidate = path.join(workspaceRoot, 'target', 'release', BINARY_NAME);
  if (fileIsExecutable(workspaceCandidate)) {
    return { binary: workspaceCandidate, source: 'workspace-target' };
  }

  const upwards = findUpwards(workspaceRoot, ['target', 'release', BINARY_NAME]);
  if (upwards) {
    return { binary: upwards, source: 'workspace-target' };
  }

  return { binary: BINARY_NAME, source: 'path' };
}

function findUpwards(start: string, segments: string[]): string | null {
  let current = start;
  while (true) {
    const candidate = path.join(current, ...segments);
    if (fileIsExecutable(candidate)) {
      return candidate;
    }
    const parent = path.dirname(current);
    if (parent === current) {
      return null;
    }
    current = parent;
  }
}

function fileIsExecutable(p: string): boolean {
  try {
    const stat = fs.statSync(p);
    if (!stat.isFile()) {
      return false;
    }
  } catch {
    return false;
  }
  if (process.platform === 'win32') {
    return true;
  }
  try {
    fs.accessSync(p, fs.constants.X_OK);
    return true;
  } catch {
    return false;
  }
}
