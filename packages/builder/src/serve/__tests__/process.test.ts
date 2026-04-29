import * as fs from 'node:fs';
import * as os from 'node:os';
import * as path from 'node:path';
import { describe, expect, it } from 'vitest';

import { createNgcRsRunner, NgcEvent } from '../process';

function writeFakeBin(script: string): { dir: string; bin: string } {
  const dir = fs.mkdtempSync(path.join(os.tmpdir(), 'ngcrs-fake-'));
  const bin = path.join(dir, 'fake');
  fs.writeFileSync(bin, `#!/bin/sh\n${script}\n`, { mode: 0o755 });
  return { dir, bin };
}

function collect(runner: ReturnType<typeof createNgcRsRunner>): {
  events: NgcEvent[];
  done: Promise<void>;
} {
  const events: NgcEvent[] = [];
  const done = new Promise<void>((resolve) => {
    runner.events.on('event', (ev: NgcEvent) => {
      events.push(ev);
      if (ev.type === 'exit') {
        resolve();
      }
    });
  });
  return { events, done };
}

describe('createNgcRsRunner', () => {
  it('detects the listening line and emits a ready event', async () => {
    const { dir, bin } = writeFakeBin(
      'echo "ngc-rs serve listening on http://127.0.0.1:54321" >&2; exit 0',
    );
    try {
      const runner = createNgcRsRunner({ binary: bin, args: [], cwd: dir });
      const { events, done } = collect(runner);
      runner.start();
      await done;
      const ready = events.find((e) => e.type === 'ready');
      expect(ready).toBeDefined();
      if (ready && ready.type === 'ready') {
        expect(ready.address).toBe('127.0.0.1:54321');
      }
    } finally {
      fs.rmSync(dir, { recursive: true, force: true });
    }
  });

  it('parses build failures from stderr', async () => {
    const { dir, bin } = writeFakeBin(
      'echo "ngc-rs build failed" >&2; echo "src/a.ts:5:3 - error TS123: nope" >&2; exit 1',
    );
    try {
      const runner = createNgcRsRunner({ binary: bin, args: [], cwd: dir });
      const { events, done } = collect(runner);
      runner.start();
      await done;
      const failure = events.find((e) => e.type === 'rebuild-failure');
      expect(failure).toBeDefined();
    } finally {
      fs.rmSync(dir, { recursive: true, force: true });
    }
  });

  it('stop() terminates the child cleanly', async () => {
    const { dir, bin } = writeFakeBin(
      'trap "exit 0" INT; echo "ngc-rs serve listening on http://127.0.0.1:0"; while true; do sleep 0.1; done',
    );
    try {
      const runner = createNgcRsRunner({ binary: bin, args: [], cwd: dir });
      const { done } = collect(runner);
      runner.start();
      await new Promise((r) => setTimeout(r, 200));
      const stopP = runner.stop();
      await stopP;
      await done;
      expect(runner.isRunning()).toBe(false);
    } finally {
      fs.rmSync(dir, { recursive: true, force: true });
    }
  });

  it('forwards stdout lines as stdout events', async () => {
    const { dir, bin } = writeFakeBin('echo first; echo second; exit 0');
    try {
      const runner = createNgcRsRunner({ binary: bin, args: [], cwd: dir });
      const { events, done } = collect(runner);
      runner.start();
      await done;
      const lines = events.filter((e) => e.type === 'stdout').map((e) => (e as { line: string }).line);
      expect(lines).toContain('first');
      expect(lines).toContain('second');
    } finally {
      fs.rmSync(dir, { recursive: true, force: true });
    }
  });
});
