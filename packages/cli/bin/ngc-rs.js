#!/usr/bin/env node
// Wrapper for `@ngc-rs/cli`. Resolves the platform-specific package
// installed via `optionalDependencies` (esbuild/biome/swc style — no
// postinstall, no network on install) and execs the bundled binary,
// forwarding argv, stdio, and exit code.

'use strict';

const { spawnSync } = require('node:child_process');
const path = require('node:path');

const platform = process.platform;
const arch = process.arch;
const pkgName = `@ngc-rs/cli-${platform}-${arch}`;
const binaryName = platform === 'win32' ? 'ngc-rs.exe' : 'ngc-rs';

let pkgJsonPath;
try {
  pkgJsonPath = require.resolve(`${pkgName}/package.json`);
} catch (err) {
  console.error(
    `@ngc-rs/cli: no prebuilt binary found for ${platform}-${arch}.\n` +
      `Expected the optional dependency \`${pkgName}\` to be installed.\n` +
      `Supported targets: darwin-arm64, darwin-x64, linux-arm64, linux-x64, win32-x64.\n` +
      `If you are on a supported platform, try reinstalling without skipping optional deps:\n` +
      `  npm install --include=optional --force\n` +
      `(original error: ${err && err.message})`,
  );
  process.exit(1);
}

const binaryPath = path.join(path.dirname(pkgJsonPath), 'bin', binaryName);

const result = spawnSync(binaryPath, process.argv.slice(2), {
  stdio: 'inherit',
  windowsHide: true,
});

if (result.error) {
  console.error(`@ngc-rs/cli: failed to execute ${binaryPath}: ${result.error.message}`);
  process.exit(1);
}

process.exit(result.status ?? 1);
