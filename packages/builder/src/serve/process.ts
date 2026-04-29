import { ChildProcess, spawn } from 'node:child_process';
import { EventEmitter } from 'node:events';

import { parseDiagnostics, ParsedDiagnostic, stripAnsi, summarizeDiagnostics } from './errors';

const READY_PATTERN = /listening on\s+http(?:s)?:\/\/([^\s]+)/i;
const REBUILD_OK_PATTERN = /ngc-rs rebuild\b/i;
const BUILD_FAILED_PATTERN = /ngc-rs (?:rebuild )?(?:build )?(?:failed|error)/i;

export interface NgcRsRunnerOptions {
  binary: string;
  args: string[];
  cwd: string;
  env?: NodeJS.ProcessEnv;
}

export type NgcEvent =
  | { type: 'ready'; address: string }
  | { type: 'rebuild-success' }
  | { type: 'rebuild-failure'; diagnostics: ParsedDiagnostic[]; raw: string }
  | { type: 'stdout'; line: string }
  | { type: 'stderr'; line: string }
  | { type: 'exit'; code: number | null; signal: NodeJS.Signals | null };

export interface NgcRsRunner {
  events: EventEmitter;
  start(): void;
  stop(): Promise<void>;
  isRunning(): boolean;
}

export function createNgcRsRunner(options: NgcRsRunnerOptions): NgcRsRunner {
  const events = new EventEmitter();
  let child: ChildProcess | null = null;
  let stopRequested = false;
  let stopResolve: (() => void) | null = null;
  let stderrBuffer = '';
  let stdoutBuffer = '';

  const start = (): void => {
    if (child) {
      throw new Error('ngc-rs runner already started');
    }
    child = spawn(options.binary, options.args, {
      cwd: options.cwd,
      env: { ...process.env, ...(options.env ?? {}) },
      stdio: ['ignore', 'pipe', 'pipe'],
      windowsHide: true,
    });

    child.stdout?.setEncoding('utf8');
    child.stderr?.setEncoding('utf8');

    child.stdout?.on('data', (chunk: string) => {
      stdoutBuffer = drainLines(stdoutBuffer + chunk, (line) => {
        events.emit('event', { type: 'stdout', line } satisfies NgcEvent);
        inspectLine(line, /*fromStderr*/ false);
      });
    });

    child.stderr?.on('data', (chunk: string) => {
      stderrBuffer = drainLines(stderrBuffer + chunk, (line) => {
        events.emit('event', { type: 'stderr', line } satisfies NgcEvent);
        inspectLine(line, /*fromStderr*/ true);
      });
    });

    child.on('exit', (code, signal) => {
      flushBuffers();
      events.emit('event', { type: 'exit', code, signal } satisfies NgcEvent);
      child = null;
      if (stopResolve) {
        stopResolve();
        stopResolve = null;
      }
    });

    child.on('error', (err) => {
      events.emit('event', {
        type: 'stderr',
        line: `failed to spawn ngc-rs: ${err.message}`,
      } satisfies NgcEvent);
    });
  };

  const inspectLine = (line: string, fromStderr: boolean): void => {
    const cleaned = stripAnsi(line);
    const ready = READY_PATTERN.exec(cleaned);
    if (ready && ready[1]) {
      events.emit('event', { type: 'ready', address: ready[1] } satisfies NgcEvent);
      return;
    }
    if (REBUILD_OK_PATTERN.test(cleaned)) {
      events.emit('event', { type: 'rebuild-success' } satisfies NgcEvent);
      return;
    }
    if (fromStderr && BUILD_FAILED_PATTERN.test(cleaned)) {
      const diagnostics = parseDiagnostics(line);
      events.emit('event', {
        type: 'rebuild-failure',
        diagnostics,
        raw: cleaned,
      } satisfies NgcEvent);
    }
  };

  const flushBuffers = (): void => {
    if (stdoutBuffer.length > 0) {
      events.emit('event', { type: 'stdout', line: stdoutBuffer } satisfies NgcEvent);
      inspectLine(stdoutBuffer, false);
      stdoutBuffer = '';
    }
    if (stderrBuffer.length > 0) {
      events.emit('event', { type: 'stderr', line: stderrBuffer } satisfies NgcEvent);
      inspectLine(stderrBuffer, true);
      stderrBuffer = '';
    }
  };

  const stop = (): Promise<void> => {
    if (!child) {
      return Promise.resolve();
    }
    stopRequested = true;
    const proc = child;
    return new Promise<void>((resolve) => {
      stopResolve = resolve;
      try {
        proc.kill('SIGINT');
      } catch {
        /* already exited */
      }
      const killTimer = setTimeout(() => {
        if (proc.exitCode === null && proc.signalCode === null) {
          try {
            proc.kill('SIGKILL');
          } catch {
            /* ignore */
          }
        }
      }, 3_000);
      killTimer.unref();
    });
  };

  return {
    events,
    start,
    stop,
    isRunning: () => child !== null && !stopRequested,
  };
}

export { summarizeDiagnostics };

function drainLines(buffer: string, onLine: (line: string) => void): string {
  let remaining = buffer;
  let newlineIndex = remaining.indexOf('\n');
  while (newlineIndex !== -1) {
    const line = remaining.slice(0, newlineIndex).replace(/\r$/, '');
    if (line.length > 0) {
      onLine(line);
    }
    remaining = remaining.slice(newlineIndex + 1);
    newlineIndex = remaining.indexOf('\n');
  }
  return remaining;
}
