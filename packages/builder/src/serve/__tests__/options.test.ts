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

  it('forwards --ssl and uses an https url when ssl is true without key/cert', () => {
    const t = translateOptions({ ...base, ssl: true }, '/ws');
    expect(t.args).toContain('--ssl');
    expect(t.args).not.toContain('--ssl-key');
    expect(t.args).not.toContain('--ssl-cert');
    expect(t.url).toBe('https://localhost:4200/');
  });

  it('forwards resolved --ssl-key/--ssl-cert when both are provided', () => {
    const t = translateOptions(
      { ...base, ssl: true, sslKey: 'certs/dev.key', sslCert: 'certs/dev.crt' },
      '/ws',
    );
    expect(t.args).toContain('--ssl');
    const keyIdx = t.args.indexOf('--ssl-key');
    const certIdx = t.args.indexOf('--ssl-cert');
    expect(t.args[keyIdx + 1]).toBe('/ws/certs/dev.key');
    expect(t.args[certIdx + 1]).toBe('/ws/certs/dev.crt');
    expect(t.url).toBe('https://localhost:4200/');
  });

  it('throws when only one of sslKey/sslCert is provided', () => {
    expect(() =>
      translateOptions({ ...base, ssl: true, sslKey: '/k' }, '/ws'),
    ).toThrow(OptionTranslationError);
    expect(() =>
      translateOptions({ ...base, ssl: true, sslCert: '/c' }, '/ws'),
    ).toThrow(OptionTranslationError);
  });

  it('rejects ssl combined with proxyConfig', () => {
    expect(() =>
      translateOptions(
        { ...base, ssl: true, proxyConfig: 'proxy.conf.json' },
        '/ws',
      ),
    ).toThrow(OptionTranslationError);
  });

  it('ignores sslKey/sslCert and stays on http when ssl is not enabled', () => {
    const t = translateOptions(
      { ...base, sslKey: 'certs/dev.key', sslCert: 'certs/dev.crt' },
      '/ws',
    );
    expect(t.args).not.toContain('--ssl');
    expect(t.args).not.toContain('--ssl-key');
    expect(t.url).toBe('http://localhost:4200/');
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

  it('forwards a non-empty allowedHosts list as a comma-joined --allowed-hosts arg', () => {
    const t = translateOptions(
      { ...base, allowedHosts: ['my-app.ngrok.io', 'app.local'] },
      '/ws',
    );
    const idx = t.args.indexOf('--allowed-hosts');
    expect(idx).toBeGreaterThanOrEqual(0);
    expect(t.args[idx + 1]).toBe('my-app.ngrok.io,app.local');
  });

  it('passes through the "all" sentinel verbatim', () => {
    const t = translateOptions({ ...base, allowedHosts: ['all'] }, '/ws');
    const idx = t.args.indexOf('--allowed-hosts');
    expect(t.args[idx + 1]).toBe('all');
  });

  it('drops empty / whitespace-only allowedHosts entries and dedupes case-insensitively', () => {
    const t = translateOptions(
      {
        ...base,
        allowedHosts: ['', '   ', 'foo.example', 'Foo.Example', 'bar.example'],
      },
      '/ws',
    );
    const idx = t.args.indexOf('--allowed-hosts');
    expect(t.args[idx + 1]).toBe('foo.example,bar.example');
  });

  it('omits --allowed-hosts when the list is empty or unset', () => {
    expect(
      translateOptions({ ...base, allowedHosts: [] }, '/ws').args,
    ).not.toContain('--allowed-hosts');
    expect(translateOptions(base, '/ws').args).not.toContain('--allowed-hosts');
  });

  it('forwards a headers map as a JSON --headers arg', () => {
    const t = translateOptions(
      {
        ...base,
        headers: { 'Cross-Origin-Opener-Policy': 'same-origin' },
      },
      '/ws',
    );
    const idx = t.args.indexOf('--headers');
    expect(idx).toBeGreaterThanOrEqual(0);
    expect(JSON.parse(t.args[idx + 1])).toEqual({
      'Cross-Origin-Opener-Policy': 'same-origin',
    });
  });

  it('forwards multiple headers in a single --headers arg', () => {
    const t = translateOptions(
      {
        ...base,
        headers: { 'X-Frame-Options': 'DENY', 'X-Content-Type-Options': 'nosniff' },
      },
      '/ws',
    );
    const idx = t.args.indexOf('--headers');
    expect(JSON.parse(t.args[idx + 1])).toEqual({
      'X-Frame-Options': 'DENY',
      'X-Content-Type-Options': 'nosniff',
    });
  });

  it('trims header names and drops empty-name / non-string entries', () => {
    const t = translateOptions(
      {
        ...base,
        headers: {
          '  X-Trim  ': 'ok',
          '': 'dropped',
          // eslint-disable-next-line @typescript-eslint/no-explicit-any
          'X-Bad': 123 as any,
        },
      },
      '/ws',
    );
    const idx = t.args.indexOf('--headers');
    expect(JSON.parse(t.args[idx + 1])).toEqual({ 'X-Trim': 'ok' });
  });

  it('omits --headers when the map is empty or unset', () => {
    expect(
      translateOptions({ ...base, headers: {} }, '/ws').args,
    ).not.toContain('--headers');
    expect(translateOptions(base, '/ws').args).not.toContain('--headers');
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
  it('uses the https scheme when requested', () => {
    expect(formatUrl('localhost', 4200, null, 'https')).toBe(
      'https://localhost:4200/',
    );
    expect(formatUrl('app.local', 8080, '/admin/', 'https')).toBe(
      'https://app.local:8080/admin/',
    );
  });
});
