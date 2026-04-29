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
});
