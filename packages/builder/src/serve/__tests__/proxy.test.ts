import * as fs from 'node:fs';
import * as http from 'node:http';
import * as os from 'node:os';
import * as path from 'node:path';
import { afterEach, describe, expect, it } from 'vitest';

import {
  loadProxyConfig,
  matchRule,
  normalizeProxyConfig,
  ProxyConfigError,
  rewritePath,
  startProxyServer,
} from '../proxy';

const tmpFiles: string[] = [];

afterEach(() => {
  while (tmpFiles.length) {
    const f = tmpFiles.pop();
    if (f) {
      try {
        fs.rmSync(f, { recursive: true, force: true });
      } catch {
        /* ignore */
      }
    }
  }
});

function writeTmp(content: string, ext = '.json'): string {
  const dir = fs.mkdtempSync(path.join(os.tmpdir(), 'ngcrs-proxy-'));
  tmpFiles.push(dir);
  const file = path.join(dir, `proxy${ext}`);
  fs.writeFileSync(file, content, 'utf8');
  return file;
}

describe('normalizeProxyConfig', () => {
  it('accepts the webpack-style map form', () => {
    const rules = normalizeProxyConfig(
      { '/api': { target: 'http://example.test', changeOrigin: true } },
      'inline',
    );
    expect(rules).toHaveLength(1);
    const first = rules[0];
    expect(first).toBeDefined();
    expect(first?.contexts).toEqual(['/api']);
    expect(first?.target).toBe('http://example.test');
    expect(first?.changeOrigin).toBe(true);
  });

  it('accepts the array form with context arrays', () => {
    const rules = normalizeProxyConfig(
      [{ context: ['/a', '/b'], target: 'http://t' }],
      'inline',
    );
    expect(rules).toHaveLength(1);
    expect(rules[0]?.contexts).toEqual(['/a', '/b']);
  });

  it('strips trailing wildcards from contexts', () => {
    const rules = normalizeProxyConfig(
      { '/api/*': { target: 'http://t' } },
      'inline',
    );
    expect(rules[0]?.contexts).toEqual(['/api']);
  });

  it('throws when target is missing', () => {
    expect(() =>
      normalizeProxyConfig({ '/api': { changeOrigin: true } }, 'inline'),
    ).toThrow(ProxyConfigError);
  });

  it('compiles pathRewrite into RegExp objects', () => {
    const rules = normalizeProxyConfig(
      { '/api': { target: 'http://t', pathRewrite: { '^/api': '' } } },
      'inline',
    );
    const rewritten = rewritePath(rules[0]!, '/api/foo');
    expect(rewritten).toBe('/foo');
  });
});

describe('loadProxyConfig', () => {
  it('reads a JSON file from disk', () => {
    const file = writeTmp(
      JSON.stringify({ '/api': { target: 'http://example.test' } }),
    );
    const rules = loadProxyConfig(file);
    expect(rules[0]?.target).toBe('http://example.test');
  });

  it('strips JSON comments before parsing', () => {
    const file = writeTmp(
      '// header comment\n{ "/api": { "target": "http://t" /* inline */ } }',
    );
    const rules = loadProxyConfig(file);
    expect(rules[0]?.target).toBe('http://t');
  });
});

describe('matchRule', () => {
  it('matches an exact context prefix', () => {
    const rules = normalizeProxyConfig(
      { '/api': { target: 'http://t' } },
      'inline',
    );
    expect(matchRule(rules, '/api/foo')).not.toBeNull();
    expect(matchRule(rules, '/static/main.js')).toBeNull();
  });
});

describe('startProxyServer', () => {
  it('forwards a request to the upstream when no rule matches', async () => {
    const upstream = http.createServer((req, res) => {
      res.statusCode = 200;
      res.setHeader('Content-Type', 'text/plain');
      res.end(`upstream:${req.url}`);
    });
    await new Promise<void>((r) => upstream.listen(0, '127.0.0.1', () => r()));
    const upstreamPort = (upstream.address() as { port: number }).port;

    const handle = await startProxyServer({
      rules: [],
      upstream: { host: '127.0.0.1', port: upstreamPort },
      listen: { host: '127.0.0.1', port: 0 },
    });
    try {
      const addr = handle.address();
      const body = await fetchText(`http://${addr.host}:${addr.port}/hello`);
      expect(body).toBe('upstream:/hello');
    } finally {
      await handle.close();
      upstream.close();
    }
  });

  it('forwards to a proxy target when a rule matches', async () => {
    const target = http.createServer((req, res) => {
      res.statusCode = 200;
      res.setHeader('Content-Type', 'text/plain');
      res.end(`target:${req.url}`);
    });
    await new Promise<void>((r) => target.listen(0, '127.0.0.1', () => r()));
    const targetPort = (target.address() as { port: number }).port;

    const upstream = http.createServer((_req, res) => {
      res.end('upstream-fallback');
    });
    await new Promise<void>((r) => upstream.listen(0, '127.0.0.1', () => r()));
    const upstreamPort = (upstream.address() as { port: number }).port;

    const rules = normalizeProxyConfig(
      { '/api': { target: `http://127.0.0.1:${targetPort}`, pathRewrite: { '^/api': '' } } },
      'inline',
    );
    const handle = await startProxyServer({
      rules,
      upstream: { host: '127.0.0.1', port: upstreamPort },
      listen: { host: '127.0.0.1', port: 0 },
    });
    try {
      const addr = handle.address();
      const body = await fetchText(`http://${addr.host}:${addr.port}/api/foo`);
      expect(body).toBe('target:/foo');

      const fallback = await fetchText(`http://${addr.host}:${addr.port}/static/x`);
      expect(fallback).toBe('upstream-fallback');
    } finally {
      await handle.close();
      target.close();
      upstream.close();
    }
  });
});

function fetchText(url: string): Promise<string> {
  return new Promise((resolve, reject) => {
    http
      .get(url, (res) => {
        const chunks: Buffer[] = [];
        res.on('data', (c: Buffer) => chunks.push(c));
        res.on('end', () => resolve(Buffer.concat(chunks).toString('utf8')));
        res.on('error', reject);
      })
      .on('error', reject);
  });
}
