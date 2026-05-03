import { spawn } from 'node:child_process';

/// One entry in the JSON `errors` / `warnings` arrays emitted by
/// `ngc-rs build --output-json`. Mirrors the Rust `Diagnostic` struct in
/// `crates/cli/src/main.rs`.
export interface NgcDiagnostic {
  file: string | null;
  line: number | null;
  column: number | null;
  message: string;
  severity: 'error' | 'warning';
}

/// One entry in the JSON `output_files` array. Mirrors the Rust
/// `OutputFile` struct in `crates/cli/src/main.rs`.
export interface NgcOutputFile {
  path: string;
  size: number;
  kind: 'script' | 'style' | 'html' | 'source-map' | 'asset';
}

/// JSON payload produced by `ngc-rs build --output-json`. Mirrors the Rust
/// `BuildResult` struct in `crates/cli/src/main.rs`. The shape is
/// intentionally a superset of `BuilderOutput` so the builder shim can drop
/// the relevant fields straight onto the architect protocol's response.
export interface NgcBuildResult {
  success: boolean;
  error: string | null;
  errors: NgcDiagnostic[];
  warnings: NgcDiagnostic[];
  output_path: string;
  output_files: NgcOutputFile[];
  modules_bundled: number;
  total_size_bytes: number;
  duration_ms: number;
}

/// Outcome of running ngc-rs build once.
export interface BuildRunResult {
  /// Parsed JSON payload, when `--output-json` produced one. `null` when
  /// the binary failed to emit valid JSON (e.g. the spawn itself failed).
  result: NgcBuildResult | null;
  /// Process exit code. `null` if the process was killed by signal.
  exitCode: number | null;
  /// Captured stderr (already stripped of ANSI control codes upstream by
  /// caller if desired).
  stderr: string;
}

export interface RunNgcBuildOptions {
  /// Absolute path to the ngc-rs binary.
  binary: string;
  /// CLI args (typically begins with `'build'`).
  args: string[];
  /// Working directory passed to the spawn.
  cwd: string;
  /// Optional env override merged on top of `process.env`.
  env?: NodeJS.ProcessEnv;
  /// Hook for streaming stderr output. Called once per chunk; the chunk
  /// preserves whatever the child wrote (multi-line OK). Used by the
  /// builder to forward log lines to `BuilderContext.logger`.
  onStderr?: (chunk: string) => void;
}

/// Run `ngc-rs build --output-json` once and parse the JSON payload from
/// stdout. Resolves regardless of exit code — the architect builder maps
/// `result.success` onto `BuilderOutput.success`. Rejects only if the
/// child failed to spawn at all.
export function runNgcBuild(options: RunNgcBuildOptions): Promise<BuildRunResult> {
  return new Promise<BuildRunResult>((resolve, reject) => {
    const child = spawn(options.binary, options.args, {
      cwd: options.cwd,
      env: { ...process.env, ...(options.env ?? {}) },
      stdio: ['ignore', 'pipe', 'pipe'],
      windowsHide: true,
    });

    let stdout = '';
    let stderr = '';

    child.stdout?.setEncoding('utf8');
    child.stderr?.setEncoding('utf8');

    child.stdout?.on('data', (chunk: string) => {
      stdout += chunk;
    });
    child.stderr?.on('data', (chunk: string) => {
      stderr += chunk;
      options.onStderr?.(chunk);
    });

    child.on('error', (err) => {
      reject(new Error(`failed to spawn ngc-rs: ${err.message}`));
    });

    child.on('exit', (code) => {
      const result = parseJsonPayload(stdout);
      resolve({ result, exitCode: code, stderr });
    });
  });
}

function parseJsonPayload(stdout: string): NgcBuildResult | null {
  // ngc-rs emits exactly one JSON object on stdout when `--output-json` is
  // set. Be lenient about leading/trailing whitespace.
  const trimmed = stdout.trim();
  if (!trimmed) {
    return null;
  }
  try {
    const parsed = JSON.parse(trimmed) as NgcBuildResult;
    if (typeof parsed.success !== 'boolean') {
      return null;
    }
    return parsed;
  } catch {
    return null;
  }
}
