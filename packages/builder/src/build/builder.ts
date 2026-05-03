import { BuilderContext, BuilderOutput, createBuilder } from '@angular-devkit/architect';
import { Observable, Subject } from 'rxjs';

import { locateNgcRs } from '../serve/locate';
import {
  ApplicationOptions,
  OptionTranslationError,
  TranslatedBuildArgs,
  translateOptions,
} from './options';
import { NgcBuildResult, NgcDiagnostic, runNgcBuild } from './process';

interface ApplicationBuilderOutput extends BuilderOutput {
  outputPath?: string;
}

export function execute(
  options: ApplicationOptions,
  context: BuilderContext,
): Observable<BuilderOutput> {
  const subject = new Subject<ApplicationBuilderOutput>();

  void runOnce(options, context, subject)
    .catch((err) => {
      const message = (err as Error).message;
      context.logger.error(`ngc-rs builder failed: ${message}`);
      subject.next({ success: false, error: message });
    })
    .finally(() => {
      subject.complete();
    });

  return subject.asObservable();
}

async function runOnce(
  options: ApplicationOptions,
  context: BuilderContext,
  subject: Subject<ApplicationBuilderOutput>,
): Promise<void> {
  const configuration = context.target?.configuration ?? null;

  let translated: TranslatedBuildArgs;
  try {
    translated = translateOptions(options, context.workspaceRoot, configuration);
  } catch (err) {
    if (err instanceof OptionTranslationError) {
      context.logger.error(err.message);
      subject.next({ success: false, error: err.message });
      return;
    }
    throw err;
  }

  for (const w of translated.warnings) {
    context.logger.warn(w);
  }

  const located = locateNgcRs(
    context.workspaceRoot,
    options.ngcRsBinary ?? null,
    process.env,
  );
  context.logger.info(
    `ngc-rs binary: ${located.binary} (resolved from ${located.source})`,
  );

  context.reportRunning();

  const run = await runNgcBuild({
    binary: located.binary,
    args: translated.args,
    cwd: context.workspaceRoot,
    onStderr: (chunk) => {
      // Forward unbuffered for visibility; the architect logger handles its
      // own batching. Trim trailing newline so the logger doesn't emit a
      // blank follow-up line.
      const text = chunk.replace(/\n$/, '');
      if (text.length > 0) {
        context.logger.info(text);
      }
    },
  });

  if (!run.result) {
    const tail = run.stderr.trim().split('\n').slice(-5).join('\n');
    const message =
      `ngc-rs exited with code ${run.exitCode ?? 'null'} without emitting valid JSON output.` +
      (tail ? ` Last stderr lines:\n${tail}` : '');
    context.logger.error(message);
    subject.next({ success: false, error: message });
    return;
  }

  reportDiagnostics(run.result, context);

  if (!run.result.success) {
    const message = run.result.error ?? 'ngc-rs build failed';
    subject.next({
      success: false,
      error: message,
      outputPath: run.result.output_path,
    });
    return;
  }

  context.logger.info(
    `ngc-rs build complete: ${run.result.modules_bundled} module(s), ` +
      `${run.result.output_files.length} file(s), ` +
      `${formatBytes(run.result.total_size_bytes)}, ` +
      `${run.result.duration_ms} ms`,
  );

  subject.next({
    success: true,
    outputPath: run.result.output_path,
  });
}

function reportDiagnostics(result: NgcBuildResult, context: BuilderContext): void {
  for (const w of result.warnings) {
    context.logger.warn(formatDiagnostic(w));
  }
  for (const e of result.errors) {
    context.logger.error(formatDiagnostic(e));
  }
}

function formatDiagnostic(d: NgcDiagnostic): string {
  const loc = d.file
    ? d.line !== null
      ? d.column !== null
        ? `${d.file}:${d.line}:${d.column}`
        : `${d.file}:${d.line}`
      : d.file
    : null;
  return loc ? `${loc}: ${d.message}` : d.message;
}

function formatBytes(bytes: number): string {
  if (bytes < 1024) {
    return `${bytes} B`;
  }
  if (bytes < 1024 * 1024) {
    return `${(bytes / 1024).toFixed(1)} KiB`;
  }
  return `${(bytes / (1024 * 1024)).toFixed(2)} MiB`;
}

export default createBuilder<ApplicationOptions>(execute);
