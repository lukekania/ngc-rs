import { describe, expect, it } from 'vitest';

import {
  ApplicationOptions,
  OptionTranslationError,
  translateOptions,
} from '../options';

const minimal: Partial<ApplicationOptions> = {
  tsConfig: 'tsconfig.app.json',
};

describe('translateOptions (build)', () => {
  it('emits build args with project, output-json, and the architect configuration', () => {
    const t = translateOptions(minimal, '/ws', 'production');
    expect(t.args).toEqual([
      'build',
      '--project',
      'tsconfig.app.json',
      '--output-json',
      '--configuration',
      'production',
    ]);
    expect(t.warnings).toEqual([]);
  });

  it('omits --configuration when the architect target has no configuration', () => {
    const t = translateOptions(minimal, '/ws', null);
    expect(t.args).not.toContain('--configuration');
  });

  it('forwards string outputPath as a workspace-relative --out-dir', () => {
    const t = translateOptions(
      { ...minimal, outputPath: 'dist/my-app' },
      '/ws',
      null,
    );
    const i = t.args.indexOf('--out-dir');
    expect(i).toBeGreaterThanOrEqual(0);
    expect(t.args[i + 1]).toBe('/ws/dist/my-app');
  });

  it('uses the `base` field when outputPath is the {base, browser} object form', () => {
    const t = translateOptions(
      { ...minimal, outputPath: { base: 'dist/app', browser: 'browser' } },
      '/ws',
      null,
    );
    const i = t.args.indexOf('--out-dir');
    expect(t.args[i + 1]).toBe('/ws/dist/app');
  });

  it('appends --localize when localize is true', () => {
    const t = translateOptions({ ...minimal, localize: true }, '/ws', null);
    expect(t.args).toContain('--localize');
  });

  it('appends --localize and warns when localize is an array (subset not yet honoured)', () => {
    const t = translateOptions(
      { ...minimal, localize: ['en', 'de'] },
      '/ws',
      null,
    );
    expect(t.args).toContain('--localize');
    expect(t.warnings.some((w) => w.includes('locale subset'))).toBe(true);
  });

  it('rejects non-empty scripts arrays', () => {
    expect(() =>
      translateOptions(
        { ...minimal, scripts: ['some.js'] as never },
        '/ws',
        null,
      ),
    ).toThrow(OptionTranslationError);
  });

  it('accepts empty scripts arrays without error', () => {
    expect(() =>
      translateOptions(
        { ...minimal, scripts: [] as never },
        '/ws',
        null,
      ),
    ).not.toThrow();
  });

  it('rejects watch=true, prerender, ssr, server, outputMode=server', () => {
    expect(() =>
      translateOptions({ ...minimal, watch: true }, '/ws', null),
    ).toThrow(OptionTranslationError);
    expect(() =>
      translateOptions({ ...minimal, prerender: true }, '/ws', null),
    ).toThrow(OptionTranslationError);
    expect(() =>
      translateOptions({ ...minimal, ssr: true }, '/ws', null),
    ).toThrow(OptionTranslationError);
    expect(() =>
      translateOptions({ ...minimal, server: 'src/server.ts' }, '/ws', null),
    ).toThrow(OptionTranslationError);
    expect(() =>
      translateOptions(
        { ...minimal, outputMode: 'server' as never },
        '/ws',
        null,
      ),
    ).toThrow(OptionTranslationError);
  });

  it('warns when sourceMap, optimization, or outputHashing is set (hardcoded by ngc-rs per configuration)', () => {
    const t = translateOptions(
      {
        ...minimal,
        sourceMap: false,
        optimization: true,
        outputHashing: 'all',
      },
      '/ws',
      'production',
    );
    expect(t.warnings.some((w) => w.includes('sourceMap'))).toBe(true);
    expect(t.warnings.some((w) => w.includes('optimization'))).toBe(true);
    expect(t.warnings.some((w) => w.includes('outputHashing'))).toBe(true);
  });

  it('warns on aot=false, statsJson, namedChunks, vendorChunk, preserveSymlinks', () => {
    const t = translateOptions(
      {
        ...minimal,
        aot: false,
        statsJson: true,
        namedChunks: true,
        vendorChunk: true,
        preserveSymlinks: true,
      },
      '/ws',
      null,
    );
    expect(t.warnings.length).toBeGreaterThanOrEqual(5);
  });

  it('defaults tsConfig to "tsconfig.json" when omitted', () => {
    const t = translateOptions({}, '/ws', null);
    const i = t.args.indexOf('--project');
    expect(t.args[i + 1]).toBe('tsconfig.json');
  });
});
