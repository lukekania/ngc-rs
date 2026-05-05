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

  it('appends --strict-templates when strictTemplates is true', () => {
    const t = translateOptions(
      { ...minimal, strictTemplates: true },
      '/ws',
      null,
    );
    expect(t.args).toContain('--strict-templates');
  });

  it('omits --strict-templates when strictTemplates is false or unset', () => {
    const off = translateOptions(
      { ...minimal, strictTemplates: false },
      '/ws',
      null,
    );
    expect(off.args).not.toContain('--strict-templates');

    const unset = translateOptions(minimal, '/ws', null);
    expect(unset.args).not.toContain('--strict-templates');
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

  it('accepts non-empty scripts arrays without error', () => {
    // ngc-rs reads `scripts` directly from angular.json, so the builder
    // does not need to forward it as a CLI flag — but it must no longer
    // reject the option (issue #138).
    expect(() =>
      translateOptions(
        { ...minimal, scripts: ['some.js'] as never },
        '/ws',
        null,
      ),
    ).not.toThrow();
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

  it('accepts a `define` map without warnings (passed through via angular.json)', () => {
    const t = translateOptions(
      {
        ...minimal,
        define: {
          __APP_API_URL__: '"https://api.example.com"',
          __BUILD_VERSION__: '"1.0.0"',
        },
      },
      '/ws',
      null,
    );
    expect(t.warnings.some((w) => w.toLowerCase().includes('define'))).toBe(
      false,
    );
  });
});
