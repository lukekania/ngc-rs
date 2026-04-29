import * as fs from 'node:fs';
import * as http from 'node:http';
import * as path from 'node:path';
import { URL } from 'node:url';

export interface ProxyRule {
  contexts: string[];
  target: string;
  secure: boolean;
  changeOrigin: boolean;
  pathRewrite: Array<{ from: RegExp; to: string }>;
  ws: boolean;
}

export interface ProxyConfigLoadError {
  message: string;
  cause?: unknown;
}

export class ProxyConfigError extends Error {
  constructor(message: string) {
    super(message);
    this.name = 'ProxyConfigError';
  }
}

export function loadProxyConfig(configPath: string): ProxyRule[] {
  const ext = path.extname(configPath).toLowerCase();
  let raw: unknown;
  if (ext === '.json' || ext === '') {
    const text = fs.readFileSync(configPath, 'utf8');
    try {
      raw = JSON.parse(stripJsonComments(text));
    } catch (err) {
      throw new ProxyConfigError(
        `failed to parse proxy config JSON at ${configPath}: ${(err as Error).message}`,
      );
    }
  } else if (ext === '.js' || ext === '.cjs' || ext === '.mjs') {
    try {
      // eslint-disable-next-line @typescript-eslint/no-require-imports
      raw = require(configPath);
      if (raw && typeof raw === 'object' && 'default' in (raw as Record<string, unknown>)) {
        const def = (raw as Record<string, unknown>)['default'];
        if (def !== undefined) {
          raw = def;
        }
      }
    } catch (err) {
      throw new ProxyConfigError(
        `failed to load proxy config module at ${configPath}: ${(err as Error).message}`,
      );
    }
  } else {
    throw new ProxyConfigError(
      `unsupported proxy config extension "${ext}" at ${configPath}`,
    );
  }
  return normalizeProxyConfig(raw, configPath);
}

export function normalizeProxyConfig(raw: unknown, source: string): ProxyRule[] {
  if (!raw || typeof raw !== 'object') {
    throw new ProxyConfigError(`proxy config at ${source} must be an object or array`);
  }
  if (Array.isArray(raw)) {
    return raw.map((entry, idx) => normalizeArrayEntry(entry, source, idx));
  }
  const out: ProxyRule[] = [];
  for (const [key, value] of Object.entries(raw as Record<string, unknown>)) {
    out.push(normalizeMapEntry(key, value, source));
  }
  return out;
}

function normalizeArrayEntry(entry: unknown, source: string, idx: number): ProxyRule {
  if (!entry || typeof entry !== 'object') {
    throw new ProxyConfigError(`proxy config entry [${idx}] in ${source} is not an object`);
  }
  const e = entry as Record<string, unknown>;
  const ctx = e['context'];
  let contexts: string[];
  if (typeof ctx === 'string') {
    contexts = [ctx];
  } else if (Array.isArray(ctx) && ctx.every((c) => typeof c === 'string')) {
    contexts = ctx as string[];
  } else {
    throw new ProxyConfigError(
      `proxy config entry [${idx}] in ${source} is missing a "context" string or string[]`,
    );
  }
  return buildRule(contexts, e, `entry [${idx}]`, source);
}

function normalizeMapEntry(key: string, value: unknown, source: string): ProxyRule {
  if (!value || typeof value !== 'object') {
    throw new ProxyConfigError(`proxy config entry "${key}" in ${source} is not an object`);
  }
  return buildRule([key], value as Record<string, unknown>, `entry "${key}"`, source);
}

function buildRule(
  contexts: string[],
  e: Record<string, unknown>,
  label: string,
  source: string,
): ProxyRule {
  const target = e['target'];
  if (typeof target !== 'string' || target.length === 0) {
    throw new ProxyConfigError(`proxy ${label} in ${source} requires a "target" string`);
  }
  const pathRewrite: Array<{ from: RegExp; to: string }> = [];
  const rawRewrite = e['pathRewrite'];
  if (rawRewrite && typeof rawRewrite === 'object' && !Array.isArray(rawRewrite)) {
    for (const [from, to] of Object.entries(rawRewrite as Record<string, unknown>)) {
      if (typeof to !== 'string') {
        throw new ProxyConfigError(
          `proxy ${label} in ${source} pathRewrite["${from}"] must be a string`,
        );
      }
      pathRewrite.push({ from: new RegExp(from), to });
    }
  }
  return {
    contexts: contexts.map(stripGlobSuffix),
    target,
    secure: e['secure'] !== false,
    changeOrigin: e['changeOrigin'] === true,
    pathRewrite,
    ws: e['ws'] === true,
  };
}

function stripGlobSuffix(ctx: string): string {
  if (ctx.endsWith('/*')) {
    return ctx.slice(0, -2);
  }
  if (ctx.endsWith('*')) {
    return ctx.slice(0, -1);
  }
  return ctx;
}

export function matchRule(rules: ProxyRule[], pathname: string): ProxyRule | null {
  for (const rule of rules) {
    for (const ctx of rule.contexts) {
      if (pathname === ctx || pathname.startsWith(ctx + '/') || pathname.startsWith(ctx)) {
        if (ctx === '' || pathname.startsWith(ctx)) {
          return rule;
        }
      }
    }
  }
  return null;
}

export function rewritePath(rule: ProxyRule, pathname: string): string {
  let out = pathname;
  for (const { from, to } of rule.pathRewrite) {
    out = out.replace(from, to);
  }
  return out;
}

export interface ProxyServerOptions {
  rules: ProxyRule[];
  upstream: { host: string; port: number };
  listen: { host: string; port: number };
  log?: (line: string) => void;
}

export interface ProxyServerHandle {
  server: http.Server;
  close(): Promise<void>;
  address(): { host: string; port: number };
}

export function startProxyServer(options: ProxyServerOptions): Promise<ProxyServerHandle> {
  const log = options.log ?? (() => {});
  const server = http.createServer((req, res) => {
    handleRequest(req, res, options, log).catch((err) => {
      log(`proxy request failed: ${(err as Error).message}`);
      if (!res.headersSent) {
        res.statusCode = 502;
        res.setHeader('Content-Type', 'text/plain; charset=utf-8');
        res.end(`bad gateway: ${(err as Error).message}`);
      } else {
        res.end();
      }
    });
  });
  // Disable response timeouts for SSE.
  server.requestTimeout = 0;
  server.keepAliveTimeout = 0;

  return new Promise((resolve, reject) => {
    const onError = (err: Error): void => {
      server.removeListener('listening', onListening);
      reject(err);
    };
    const onListening = (): void => {
      server.removeListener('error', onError);
      const addr = server.address();
      const host = options.listen.host;
      const port = typeof addr === 'object' && addr ? addr.port : options.listen.port;
      resolve({
        server,
        address: () => ({ host, port }),
        close: () =>
          new Promise<void>((res, rej) => {
            server.close((err) => (err ? rej(err) : res()));
            server.closeAllConnections();
          }),
      });
    };
    server.once('error', onError);
    server.once('listening', onListening);
    server.listen(options.listen.port, options.listen.host);
  });
}

async function handleRequest(
  req: http.IncomingMessage,
  res: http.ServerResponse,
  options: ProxyServerOptions,
  log: (line: string) => void,
): Promise<void> {
  const url = req.url ?? '/';
  const pathOnly = url.split('?')[0] ?? url;
  const rule = matchRule(options.rules, pathOnly);
  if (rule) {
    return forwardToTarget(req, res, rule, url, log);
  }
  return forwardToUpstream(req, res, options.upstream, url);
}

function forwardToUpstream(
  req: http.IncomingMessage,
  res: http.ServerResponse,
  upstream: { host: string; port: number },
  url: string,
): Promise<void> {
  return new Promise((resolve, reject) => {
    const headers = { ...req.headers };
    delete headers['host'];
    const proxyReq = http.request(
      {
        host: upstream.host,
        port: upstream.port,
        method: req.method,
        path: url,
        headers,
      },
      (proxyRes) => {
        res.writeHead(proxyRes.statusCode ?? 502, proxyRes.headers);
        proxyRes.pipe(res);
        proxyRes.on('end', () => resolve());
        proxyRes.on('error', reject);
      },
    );
    proxyReq.on('error', reject);
    req.pipe(proxyReq);
    req.on('aborted', () => proxyReq.destroy());
  });
}

function forwardToTarget(
  req: http.IncomingMessage,
  res: http.ServerResponse,
  rule: ProxyRule,
  url: string,
  log: (line: string) => void,
): Promise<void> {
  const target = new URL(rule.target);
  const [pathOnly, query] = splitPathQuery(url);
  const rewritten = rewritePath(rule, pathOnly);
  const targetPath = joinTargetPath(target.pathname, rewritten) + (query ? `?${query}` : '');

  const isHttps = target.protocol === 'https:';
  const headers = { ...req.headers };
  if (rule.changeOrigin) {
    headers['host'] = target.host;
  } else {
    delete headers['host'];
  }

  const requestModule = isHttps ? requireHttps() : http;
  log(`proxy ${req.method} ${url} -> ${target.protocol}//${target.host}${targetPath}`);

  return new Promise((resolve, reject) => {
    const proxyReq = requestModule.request(
      {
        protocol: target.protocol,
        host: target.hostname,
        port: target.port || (isHttps ? 443 : 80),
        method: req.method,
        path: targetPath,
        headers,
        rejectUnauthorized: rule.secure,
      } as http.RequestOptions,
      (proxyRes) => {
        res.writeHead(proxyRes.statusCode ?? 502, proxyRes.headers);
        proxyRes.pipe(res);
        proxyRes.on('end', () => resolve());
        proxyRes.on('error', reject);
      },
    );
    proxyReq.on('error', reject);
    req.pipe(proxyReq);
    req.on('aborted', () => proxyReq.destroy());
  });
}

function splitPathQuery(url: string): [string, string | null] {
  const idx = url.indexOf('?');
  if (idx === -1) {
    return [url, null];
  }
  return [url.slice(0, idx), url.slice(idx + 1)];
}

function joinTargetPath(base: string, rewritten: string): string {
  const trimmedBase = base.endsWith('/') ? base.slice(0, -1) : base;
  if (!trimmedBase) {
    return rewritten;
  }
  if (rewritten.startsWith('/')) {
    return trimmedBase + rewritten;
  }
  return trimmedBase + '/' + rewritten;
}

function requireHttps(): typeof http {
  // eslint-disable-next-line @typescript-eslint/no-require-imports
  return require('node:https') as unknown as typeof http;
}

function stripJsonComments(s: string): string {
  return s.replace(/\/\*[\s\S]*?\*\//g, '').replace(/(^|[^:])\/\/.*$/gm, '$1');
}
