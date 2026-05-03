import * as fs from 'node:fs';
import * as os from 'node:os';
import * as path from 'node:path';
import { describe, expect, it } from 'vitest';

import { locateNgcRs } from '../locate';

function makeWorkspace(): { dir: string; cleanup: () => void } {
  const dir = fs.mkdtempSync(path.join(os.tmpdir(), 'ngcrs-locate-'));
  return {
    dir,
    cleanup: () => fs.rmSync(dir, { recursive: true, force: true }),
  };
}

describe('locateNgcRs', () => {
  it('honors an explicit option override', () => {
    const ws = makeWorkspace();
    try {
      const out = locateNgcRs(ws.dir, '/explicit/path/ngc-rs', {});
      expect(out).toEqual({ binary: '/explicit/path/ngc-rs', source: 'option' });
    } finally {
      ws.cleanup();
    }
  });

  it('resolves a relative option path against the workspace root', () => {
    const ws = makeWorkspace();
    try {
      const out = locateNgcRs(ws.dir, './bin/ngc-rs', {});
      expect(out.binary).toBe(path.join(ws.dir, 'bin', 'ngc-rs'));
      expect(out.source).toBe('option');
    } finally {
      ws.cleanup();
    }
  });

  it('reads NGC_RS_BINARY from env when no option is set', () => {
    const ws = makeWorkspace();
    try {
      const out = locateNgcRs(ws.dir, null, { NGC_RS_BINARY: '/from/env/ngc-rs' });
      expect(out).toEqual({ binary: '/from/env/ngc-rs', source: 'env' });
    } finally {
      ws.cleanup();
    }
  });

  it('falls through to PATH when no candidate exists', () => {
    const ws = makeWorkspace();
    try {
      const out = locateNgcRs(ws.dir, null, {});
      expect(out.source).toBe('path');
      expect(out.binary).toMatch(/ngc-rs/);
    } finally {
      ws.cleanup();
    }
  });

  it('finds an executable under <workspace>/target/release', () => {
    const ws = makeWorkspace();
    try {
      const dir = path.join(ws.dir, 'target', 'release');
      fs.mkdirSync(dir, { recursive: true });
      const bin = path.join(dir, process.platform === 'win32' ? 'ngc-rs.exe' : 'ngc-rs');
      fs.writeFileSync(bin, '#!/bin/sh\necho hi\n', { mode: 0o755 });
      const out = locateNgcRs(ws.dir, null, {});
      expect(out.source).toBe('workspace-target');
      expect(out.binary).toBe(bin);
    } finally {
      ws.cleanup();
    }
  });

  it('finds the npm-distributed platform package binary in node_modules', () => {
    const ws = makeWorkspace();
    try {
      // Materialize a fake `@ngc-rs/cli-{platform}-{arch}` package layout
      // under <workspace>/node_modules so `require.resolve` finds it.
      const platformPkg = `@ngc-rs/cli-${process.platform}-${process.arch}`;
      const pkgDir = path.join(ws.dir, 'node_modules', ...platformPkg.split('/'));
      fs.mkdirSync(path.join(pkgDir, 'bin'), { recursive: true });
      fs.writeFileSync(
        path.join(pkgDir, 'package.json'),
        JSON.stringify({ name: platformPkg, version: '1.0.0' }),
      );
      const binName = process.platform === 'win32' ? 'ngc-rs.exe' : 'ngc-rs';
      const bin = path.join(pkgDir, 'bin', binName);
      fs.writeFileSync(bin, '#!/bin/sh\necho hi\n', { mode: 0o755 });

      const out = locateNgcRs(ws.dir, null, {});
      expect(out.source).toBe('node-modules');
      // realpath both sides because `require.resolve` returns the realpath
      // (collapsing macOS' `/var` → `/private/var` symlink) while
      // `path.join` keeps the symlink form.
      expect(fs.realpathSync(out.binary)).toBe(fs.realpathSync(bin));
    } finally {
      ws.cleanup();
    }
  });

  it('prefers node_modules over <workspace>/target/release when both exist', () => {
    const ws = makeWorkspace();
    try {
      // Materialize both candidates.
      const targetDir = path.join(ws.dir, 'target', 'release');
      fs.mkdirSync(targetDir, { recursive: true });
      const binName = process.platform === 'win32' ? 'ngc-rs.exe' : 'ngc-rs';
      fs.writeFileSync(path.join(targetDir, binName), '#!/bin/sh\n', { mode: 0o755 });

      const platformPkg = `@ngc-rs/cli-${process.platform}-${process.arch}`;
      const pkgDir = path.join(ws.dir, 'node_modules', ...platformPkg.split('/'));
      fs.mkdirSync(path.join(pkgDir, 'bin'), { recursive: true });
      fs.writeFileSync(
        path.join(pkgDir, 'package.json'),
        JSON.stringify({ name: platformPkg, version: '1.0.0' }),
      );
      const pkgBin = path.join(pkgDir, 'bin', binName);
      fs.writeFileSync(pkgBin, '#!/bin/sh\n', { mode: 0o755 });

      const out = locateNgcRs(ws.dir, null, {});
      expect(out.source).toBe('node-modules');
      expect(fs.realpathSync(out.binary)).toBe(fs.realpathSync(pkgBin));
    } finally {
      ws.cleanup();
    }
  });
});
