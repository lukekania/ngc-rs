import { json } from '@angular-devkit/core';
import * as path from 'node:path';

/// Application builder options accepted by `@ngc-rs/builder:application`.
/// Mirrors the most-used subset of `@angular/build:application` so projects
/// can swap one line in `angular.json`. See `schemas/application.json` for
/// per-field documentation.
export interface ApplicationOptions extends json.JsonObject {
  tsConfig: string;
  outputPath: string | { base: string; browser?: string } | null;
  browser: string | null;
  main: string | null;
  polyfills: string | string[] | null;
  index: string | { input: string; output?: string } | boolean | null;
  assets: json.JsonArray | null;
  styles: json.JsonArray | null;
  scripts: json.JsonArray | null;
  fileReplacements: json.JsonArray | null;
  sourceMap: boolean | json.JsonObject | null;
  optimization: boolean | json.JsonObject | null;
  outputHashing: 'none' | 'all' | 'media' | 'bundles' | null;
  namedChunks: boolean | null;
  vendorChunk: boolean | null;
  aot: boolean | null;
  baseHref: string | null;
  deployUrl: string | null;
  crossOrigin: 'none' | 'anonymous' | 'use-credentials' | null;
  subresourceIntegrity: boolean | null;
  serviceWorker: boolean | string | null;
  ngswConfigPath: string | null;
  preserveSymlinks: boolean | null;
  statsJson: boolean | null;
  budgets: json.JsonArray | null;
  externalDependencies: string[] | null;
  allowedCommonJsDependencies: string[] | null;
  extractLicenses: boolean | null;
  verbose: boolean | null;
  progress: boolean | null;
  watch: boolean | null;
  prerender: boolean | json.JsonObject | null;
  ssr: boolean | json.JsonObject | null;
  server: string | null;
  outputMode: 'static' | 'server' | null;
  ngcRsBinary: string | null;
  localize: boolean | string[] | null;
  inlineStyleLanguage: 'css' | 'less' | 'sass' | 'scss' | null;
  stylePreprocessorOptions: json.JsonObject | null;
  define: { [key: string]: string } | null;
  conditions: string[] | null;
  loader: { [ext: string]: string } | null;
  deleteOutputPath: boolean | null;
  clearScreen: boolean | null;
  i18nDuplicateTranslation: 'warning' | 'error' | 'ignore' | null;
  i18nMissingTranslation: 'warning' | 'error' | 'ignore' | null;
  poll: number | null;
  security: json.JsonObject | null;
  appShell: boolean | json.JsonObject | null;
  webWorkerTsConfig: string | null;
  strictTemplates: boolean | null;
}

/// Result of translating raw [`ApplicationOptions`] into a CLI invocation.
export interface TranslatedBuildArgs {
  /// Argv to pass to `ngc-rs` (always begins with `'build'`).
  args: string[];
  /// Configuration name resolved from the architect target's `configuration`
  /// field, when one was supplied. Used for warning attribution.
  configuration: string | null;
  /// Per-option warnings to surface via `BuilderContext.logger.warn`.
  /// Compatibility hints — not fatal.
  warnings: string[];
}

/// Thrown when an option is set in a way ngc-rs cannot honor (e.g. SSR
/// fields, watch=true). The builder turns this into a `BuilderOutput`
/// with `success: false` and surfaces the message.
export class OptionTranslationError extends Error {
  constructor(message: string) {
    super(message);
    this.name = 'OptionTranslationError';
  }
}

/// Translate raw architect options into ngc-rs CLI args.
///
/// `configuration` is the architect target's configuration name (e.g.
/// `"production"`), passed through as `--configuration`. ngc-rs then re-reads
/// `angular.json` to pick up the per-configuration overrides — option-level
/// overrides set in architect don't need to be re-passed.
export function translateOptions(
  raw: Partial<ApplicationOptions>,
  workspaceRoot: string,
  configuration: string | null,
): TranslatedBuildArgs {
  const warnings: string[] = [];

  // Hard rejections — anything ngc-rs cannot do at all.
  if (raw.watch === true) {
    throw new OptionTranslationError(
      '`watch: true` on the application builder is not supported. Use the `@ngc-rs/builder:dev-server` builder for incremental rebuilds.',
    );
  }
  if (raw.prerender) {
    throw new OptionTranslationError(
      'Prerendering is not supported by ngc-rs (SSR is out of scope for v1). Remove the `prerender` option or use `@angular/build:application` for prerender targets.',
    );
  }
  if (raw.ssr) {
    throw new OptionTranslationError(
      'Server-side rendering is not supported by ngc-rs (SSR is out of scope for v1). Remove the `ssr` option.',
    );
  }
  if (raw.server) {
    throw new OptionTranslationError(
      'The `server` option (SSR entry point) is not supported by ngc-rs. Remove it.',
    );
  }
  if (raw.outputMode === 'server') {
    throw new OptionTranslationError(
      '`outputMode: "server"` is not supported by ngc-rs. Use `"static"` or omit the option.',
    );
  }
  if (raw.appShell) {
    throw new OptionTranslationError(
      'The `appShell` option is SSR-related and is out of scope for ngc-rs v1. Remove the option.',
    );
  }
  if (raw.webWorkerTsConfig) {
    throw new OptionTranslationError(
      'The `webWorkerTsConfig` option is not supported by ngc-rs (web workers are not yet detected by the bundler). Remove the option.',
    );
  }
  if (raw.loader && Object.keys(raw.loader).length > 0) {
    throw new OptionTranslationError(
      'The `loader` option (per-extension file loaders) is not yet supported by ngc-rs. Remove the option.',
    );
  }

  // Compatibility warnings — accepted, but ngc-rs's behaviour is hardcoded.
  if (raw.aot === false) {
    warnings.push(
      'aot=false is ignored by ngc-rs; the build always runs ahead-of-time compilation.',
    );
  }
  if (raw.statsJson === true) {
    warnings.push('statsJson=true is ignored by ngc-rs (no webpack stats output).');
  }
  if (raw.namedChunks === true) {
    warnings.push(
      'namedChunks=true is ignored by ngc-rs; chunk filenames are derived from the lazy-route module path with a content hash in production.',
    );
  }
  if (raw.vendorChunk === true) {
    warnings.push(
      'vendorChunk=true is ignored by ngc-rs (no separate vendor bundle).',
    );
  }
  if (raw.preserveSymlinks === true) {
    warnings.push(
      'preserveSymlinks=true is ignored by ngc-rs; paths are always canonicalised.',
    );
  }
  if (raw.optimization !== undefined && raw.optimization !== null) {
    warnings.push(
      'The `optimization` option is hardcoded by ngc-rs per `--configuration` (development = off, production = on); the option value is ignored.',
    );
  }
  if (raw.sourceMap !== undefined && raw.sourceMap !== null) {
    warnings.push(
      'The `sourceMap` option is hardcoded by ngc-rs per `--configuration` (development = inline, production = external); the option value is ignored.',
    );
  }
  if (raw.outputHashing !== undefined && raw.outputHashing !== null) {
    warnings.push(
      'The `outputHashing` option is hardcoded by ngc-rs per `--configuration` (production hashes bundles, development does not); the option value is ignored.',
    );
  }
  if (raw.externalDependencies && raw.externalDependencies.length > 0) {
    warnings.push(
      '`externalDependencies` is currently ignored by ngc-rs; all imports are bundled.',
    );
  }
  if (Array.isArray(raw.localize)) {
    warnings.push(
      'Selecting a locale subset via `localize` array is not yet honoured by ngc-rs; all locales declared in `angular.json` `i18n.locales` are emitted.',
    );
  }
  if (raw.stylePreprocessorOptions) {
    const opts = raw.stylePreprocessorOptions as json.JsonObject;
    const includePaths = opts['includePaths'];
    if (Array.isArray(includePaths) && includePaths.length > 0) {
      warnings.push(
        '`stylePreprocessorOptions.includePaths` is not yet honoured by ngc-rs; SCSS/Sass `@use`/`@import` resolves against the file directory and node_modules only.',
      );
    }
  }
  if (raw.conditions && raw.conditions.length > 0) {
    warnings.push(
      'The `conditions` option (custom package.json export conditions) is not yet honoured by ngc-rs; the resolver uses a fixed condition set.',
    );
  }
  if (raw.deleteOutputPath === false) {
    warnings.push(
      '`deleteOutputPath: false` is not honoured by ngc-rs; the output directory is always cleaned before each build.',
    );
  }
  if (raw.clearScreen === true) {
    warnings.push('`clearScreen: true` is not honoured by ngc-rs; the screen is not cleared between rebuilds.');
  }
  if (raw.i18nDuplicateTranslation && raw.i18nDuplicateTranslation !== 'warning') {
    warnings.push(
      `\`i18nDuplicateTranslation: "${raw.i18nDuplicateTranslation}"\` is not honoured by ngc-rs; duplicate ids are reported as warnings regardless.`,
    );
  }
  if (raw.i18nMissingTranslation && raw.i18nMissingTranslation !== 'warning') {
    warnings.push(
      `\`i18nMissingTranslation: "${raw.i18nMissingTranslation}"\` is not honoured by ngc-rs; missing translations are reported as warnings regardless.`,
    );
  }
  if (raw.security) {
    warnings.push('The `security` option (CSP auto-emission etc.) is not yet supported by ngc-rs.');
  }
  if (raw.poll !== null && raw.poll !== undefined) {
    warnings.push(
      'The `poll` option only applies in watch mode, which the application builder does not support — use the `dev-server` builder if you need polling.',
    );
  }

  const tsConfig = raw.tsConfig ?? 'tsconfig.json';
  const outDir = resolveOutDir(raw.outputPath, workspaceRoot);
  const localize = raw.localize === true || Array.isArray(raw.localize);

  const args: string[] = ['build', '--project', tsConfig, '--output-json'];
  if (configuration) {
    args.push('--configuration', configuration);
  }
  if (outDir) {
    args.push('--out-dir', outDir);
  }
  if (localize) {
    args.push('--localize');
  }
  if (raw.strictTemplates === true) {
    args.push('--strict-templates');
  }

  return { args, configuration, warnings };
}

function resolveOutDir(
  outputPath: ApplicationOptions['outputPath'] | undefined,
  workspaceRoot: string,
): string | null {
  if (!outputPath) {
    return null;
  }
  if (typeof outputPath === 'string') {
    return path.resolve(workspaceRoot, outputPath);
  }
  if (typeof outputPath === 'object' && typeof outputPath.base === 'string') {
    return path.resolve(workspaceRoot, outputPath.base);
  }
  return null;
}
