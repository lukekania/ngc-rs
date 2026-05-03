import { describe, expect, it } from 'vitest';
import * as fs from 'node:fs';
import * as os from 'node:os';
import * as path from 'node:path';

import { runNgcBuild } from '../process';

/// Build a tiny shell script that pretends to be `ngc-rs`. The script
/// writes a fixed JSON payload to stdout and exits with the requested code,
/// letting us exercise `runNgcBuild`'s parse + return-shape paths without
/// depending on the real Rust binary.
function makeFakeNgcRs(
  payload: string,
  exitCode: number,
  stderr = '',
): string {
  const dir = fs.mkdtempSync(path.join(os.tmpdir(), 'ngc-rs-fake-'));
  const script = path.join(dir, 'ngc-rs');
  const stderrLine = stderr ? `printf '${stderr.replace(/'/g, "'\\''")}' >&2\n` : '';
  fs.writeFileSync(
    script,
    `#!/bin/sh\n${stderrLine}cat <<'EOF'\n${payload}\nEOF\nexit ${exitCode}\n`,
    { mode: 0o755 },
  );
  return script;
}

describe('runNgcBuild', () => {
  it('parses a successful BuildResult JSON payload', async () => {
    const payload = JSON.stringify({
      success: true,
      error: null,
      errors: [],
      warnings: [],
      output_path: '/tmp/dist',
      output_files: [
        { path: '/tmp/dist/main.js', size: 100, kind: 'script' },
      ],
      modules_bundled: 1,
      total_size_bytes: 100,
      duration_ms: 12,
    });
    const bin = makeFakeNgcRs(payload, 0);
    const run = await runNgcBuild({ binary: bin, args: ['build'], cwd: '/' });
    expect(run.exitCode).toBe(0);
    expect(run.result?.success).toBe(true);
    expect(run.result?.modules_bundled).toBe(1);
    expect(run.result?.output_files[0]?.kind).toBe('script');
  });

  it('parses a failure BuildResult even when the binary exits non-zero', async () => {
    const payload = JSON.stringify({
      success: false,
      error: 'parse error in /a/b.ts:3:7: missing semicolon',
      errors: [
        {
          file: '/a/b.ts',
          line: 3,
          column: 7,
          message: 'missing semicolon',
          severity: 'error',
        },
      ],
      warnings: [],
      output_path: '/tmp/dist',
      output_files: [],
      modules_bundled: 0,
      total_size_bytes: 0,
      duration_ms: 5,
    });
    const bin = makeFakeNgcRs(payload, 1);
    const run = await runNgcBuild({ binary: bin, args: ['build'], cwd: '/' });
    expect(run.exitCode).toBe(1);
    expect(run.result?.success).toBe(false);
    expect(run.result?.errors[0]?.line).toBe(3);
  });

  it('returns null result when stdout is empty (no JSON emitted)', async () => {
    const bin = makeFakeNgcRs('', 1, 'thread main panicked');
    const run = await runNgcBuild({ binary: bin, args: ['build'], cwd: '/' });
    expect(run.result).toBeNull();
    expect(run.exitCode).toBe(1);
    expect(run.stderr).toContain('thread main panicked');
  });

  it('forwards stderr chunks via the onStderr callback', async () => {
    const bin = makeFakeNgcRs('{"success":true,"error":null,"errors":[],"warnings":[],"output_path":"/x","output_files":[],"modules_bundled":0,"total_size_bytes":0,"duration_ms":1}', 0, 'log line\n');
    const chunks: string[] = [];
    await runNgcBuild({
      binary: bin,
      args: ['build'],
      cwd: '/',
      onStderr: (c) => chunks.push(c),
    });
    expect(chunks.join('')).toContain('log line');
  });

  it('rejects when the binary cannot be spawned', async () => {
    await expect(
      runNgcBuild({
        binary: '/nonexistent/path/ngc-rs',
        args: ['build'],
        cwd: '/',
      }),
    ).rejects.toThrow(/failed to spawn/);
  });
});
