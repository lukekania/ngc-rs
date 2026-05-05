import { describe, expect, it } from 'vitest';

import {
  DevServerOptions,
  OptionTranslationError,
  formatUrl,
  translateOptions,
} from '../options';

const base: Partial<DevServerOptions> = {
  buildTarget: 'app:build:development',
  port: 4200,
  host: 'localhost',
};

describe('translateOptions', () => {
  it('produces serve args with project, configuration, host, and port', () => {
    const t = translateOptions(base, '/ws');
    expect(t.args).toEqual([
      'serve',
      '--project',
      'tsconfig.json',
      '--configuration',
      'development',
      '--host',
      'localhost',
      '--port',
      '4200',
    ]);
    expect(t.proxyEnabled).toBe(false);
    expect(t.url).toBe('http://localhost:4200/');
  });

  it('drops configuration when buildTarget has only two segments', () => {
    const t = translateOptions({ ...base, buildTarget: 'app:build' }, '/ws');
    expect(t.args).not.toContain('--configuration');
  });

  it('uses 127.0.0.1:0 for the spawn target when proxyConfig is set', () => {
    const t = translateOptions(
      { ...base, proxyConfig: 'proxy.conf.json' },
      '/ws',
    );
    expect(t.proxyEnabled).toBe(true);
    expect(t.proxyConfigPath).toBe('/ws/proxy.conf.json');
    expect(t.spawnHost).toBe('127.0.0.1');
    expect(t.spawnPort).toBe(0);
    expect(t.proxyHost).toBe('localhost');
    expect(t.proxyPort).toBe(4200);
    const portIdx = t.args.indexOf('--port');
    expect(t.args[portIdx + 1]).toBe('0');
  });

  it('rejects ssl=true with a clear error', () => {
    expect(() =>
      translateOptions({ ...base, ssl: true }, '/ws'),
    ).toThrow(OptionTranslationError);
  });

  it('rejects sslKey/sslCert', () => {
    expect(() =>
      translateOptions({ ...base, sslKey: '/k' }, '/ws'),
    ).toThrow(OptionTranslationError);
  });

  it('honors a custom project tsconfig', () => {
    const t = translateOptions(
      { ...base, project: 'tsconfig.app.json' },
      '/ws',
    );
    const projectIdx = t.args.indexOf('--project');
    expect(t.args[projectIdx + 1]).toBe('tsconfig.app.json');
  });

  it('handles missing buildTarget by emitting no --configuration flag', () => {
    const t = translateOptions({ port: 4200, host: 'localhost' }, '/ws');
    expect(t.args).not.toContain('--configuration');
  });

  it('forwards a normalized servePath as --serve-path', () => {
    const t = translateOptions({ ...base, servePath: 'admin' }, '/ws');
    const idx = t.args.indexOf('--serve-path');
    expect(idx).toBeGreaterThanOrEqual(0);
    expect(t.args[idx + 1]).toBe('/admin/');
    expect(t.url).toBe('http://localhost:4200/admin/');
  });

  it('preserves a fully-qualified servePath unchanged', () => {
    const t = translateOptions({ ...base, servePath: '/app/' }, '/ws');
    const idx = t.args.indexOf('--serve-path');
    expect(t.args[idx + 1]).toBe('/app/');
    expect(t.url).toBe('http://localhost:4200/app/');
  });

  it('drops --serve-path when servePath is empty or just a slash', () => {
    expect(
      translateOptions({ ...base, servePath: '/' }, '/ws').args,
    ).not.toContain('--serve-path');
    expect(
      translateOptions({ ...base, servePath: '' }, '/ws').args,
    ).not.toContain('--serve-path');
  });
});

describe('formatUrl', () => {
  it('replaces 0.0.0.0 with localhost for display', () => {
    expect(formatUrl('0.0.0.0', 4200)).toBe('http://localhost:4200/');
  });
  it('keeps custom hostnames untouched', () => {
    expect(formatUrl('app.local', 8080)).toBe('http://app.local:8080/');
  });
  it('appends a servePath when provided', () => {
    expect(formatUrl('localhost', 4200, '/admin/')).toBe(
      'http://localhost:4200/admin/',
    );
  });
});
