import { json } from '@angular-devkit/core';
import * as path from 'node:path';

export interface DevServerOptions extends json.JsonObject {
  buildTarget: string;
  port: number;
  host: string;
  open: boolean;
  ssl: boolean;
  sslKey: string | null;
  sslCert: string | null;
  proxyConfig: string | null;
  project: string;
  ngcRsBinary: string | null;
  define: { [key: string]: string } | null;
  watch: boolean | null;
  servePath: string | null;
  allowedHosts: string[] | null;
  headers: { [key: string]: string } | null;
  hmr: boolean | null;
}

export interface TranslatedServeArgs {
  args: string[];
  configuration: string | null;
  spawnHost: string;
  spawnPort: number;
  proxyEnabled: boolean;
  proxyHost: string;
  proxyPort: number;
  proxyConfigPath: string | null;
  open: boolean;
  url: string;
}

export class OptionTranslationError extends Error {
  constructor(message: string) {
    super(message);
    this.name = 'OptionTranslationError';
  }
}

const DEFAULT_PORT = 4200;
const DEFAULT_HOST = 'localhost';

export function translateOptions(
  raw: Partial<DevServerOptions>,
  workspaceRoot: string,
): TranslatedServeArgs {
  const userPort = raw.port ?? DEFAULT_PORT;
  const userHost = raw.host ?? DEFAULT_HOST;
  const open = raw.open === true;
  const ssl = raw.ssl === true;

  const configuration = parseConfigurationFromBuildTarget(raw.buildTarget);
  const project = raw.project ?? 'tsconfig.json';
  const servePath = normalizeServePath(raw.servePath ?? null);

  const proxyConfigPath = raw.proxyConfig
    ? path.resolve(workspaceRoot, raw.proxyConfig)
    : null;
  const proxyEnabled = proxyConfigPath !== null;

  // The proxy is the browser-facing endpoint, so HTTPS would have to be
  // terminated there rather than at the spawned ngc-rs serve. That's not
  // wired up, so reject the combination with an actionable message rather
  // than silently serving plain HTTP behind the proxy.
  if (ssl && proxyEnabled) {
    throw new OptionTranslationError(
      'ssl cannot be combined with proxyConfig in ngc-rs serve. Remove proxyConfig to serve HTTPS directly, or terminate TLS at a proxy in front of the (plain-HTTP) dev server.',
    );
  }

  // Resolve the SSL flags up-front so a bad key/cert combination fails the
  // build before the server is spawned. `ssl` is the master switch:
  // sslKey/sslCert are honored only when ssl is true (matching
  // `@angular/build:dev-server`).
  const sslArgs = buildSslArgs(raw, workspaceRoot, ssl);

  const spawnHost = proxyEnabled ? '127.0.0.1' : userHost;
  const spawnPort = proxyEnabled ? 0 : userPort;

  const args: string[] = ['serve', '--project', project];
  if (configuration) {
    args.push('--configuration', configuration);
  }
  args.push('--host', spawnHost, '--port', String(spawnPort));
  if (servePath) {
    args.push('--serve-path', servePath);
  }
  const allowedHosts = normalizeAllowedHosts(raw.allowedHosts);
  if (allowedHosts.length > 0) {
    args.push('--allowed-hosts', allowedHosts.join(','));
  }
  const headers = normalizeHeaders(raw.headers);
  if (headers !== null) {
    args.push('--headers', headers);
  }
  // `hmr` is tri-state: an explicit true/false becomes `--hmr`/`--no-hmr`
  // (the CLI override flags), while unset forwards nothing so the binary
  // falls back to `architect.serve.options.hmr` in angular.json.
  if (raw.hmr === true) {
    args.push('--hmr');
  } else if (raw.hmr === false) {
    args.push('--no-hmr');
  }
  args.push(...sslArgs);

  return {
    args,
    configuration,
    spawnHost,
    spawnPort,
    proxyEnabled,
    proxyHost: userHost,
    proxyPort: userPort,
    proxyConfigPath,
    open,
    url: formatUrl(userHost, userPort, servePath, ssl ? 'https' : 'http'),
  };
}

// Translate the `ssl`/`sslKey`/`sslCert` options into CLI flags for the
// spawned `ngc-rs serve`. Returns an empty array when ssl is off. When ssl
// is on:
//   * both sslKey and sslCert set → forward `--ssl --ssl-key <p> --ssl-cert
//     <p>` with the paths resolved against the workspace root;
//   * exactly one set → throw, since both halves are required;
//   * neither set → forward just `--ssl` and let the binary mint a
//     self-signed certificate.
function buildSslArgs(
  raw: Partial<DevServerOptions>,
  workspaceRoot: string,
  ssl: boolean,
): string[] {
  if (!ssl) {
    return [];
  }
  const key = raw.sslKey ?? null;
  const cert = raw.sslCert ?? null;
  if ((key && !cert) || (!key && cert)) {
    throw new OptionTranslationError(
      'ssl requires both sslKey and sslCert, or neither (to auto-generate a self-signed certificate).',
    );
  }
  const args = ['--ssl'];
  if (key && cert) {
    args.push('--ssl-key', path.resolve(workspaceRoot, key));
    args.push('--ssl-cert', path.resolve(workspaceRoot, cert));
  }
  return args;
}

// Normalize a user-supplied servePath into the canonical `/foo/` form, or
// return null when the value is empty / a bare `/` (i.e. no prefix). The
// rust side runs the same normalization (`ngc_dev_server::normalize_serve_path`),
// but normalizing on this side too lets the printed URL and any downstream
// proxy rewriting agree without depending on the spawned process.
function normalizeServePath(raw: string | null | undefined): string | null {
  if (!raw) {
    return null;
  }
  const trimmed = raw.trim();
  if (!trimmed || trimmed === '/') {
    return null;
  }
  let out = trimmed.startsWith('/') ? trimmed : `/${trimmed}`;
  if (!out.endsWith('/')) {
    out = `${out}/`;
  }
  return out;
}

// Strip empty/whitespace-only entries and dedupe (case-insensitive on the
// host portion) so the downstream CLI receives a clean comma-joined list.
// Order of distinct entries is preserved, since order shouldn't matter for
// a set-membership check but stable args make `--help` traces easier to
// diff between runs.
function normalizeAllowedHosts(raw: string[] | null | undefined): string[] {
  if (!raw || raw.length === 0) {
    return [];
  }
  const seen = new Set<string>();
  const out: string[] = [];
  for (const entry of raw) {
    if (typeof entry !== 'string') {
      continue;
    }
    const trimmed = entry.trim();
    if (!trimmed) {
      continue;
    }
    const key = trimmed.toLowerCase();
    if (seen.has(key)) {
      continue;
    }
    seen.add(key);
    out.push(trimmed);
  }
  return out;
}

// Serialize the dev-server `headers` map into a compact JSON object string
// for the `--headers` CLI flag (the shape the Rust side parses). Header
// names are trimmed; entries with an empty name or a non-string value are
// dropped — the Rust side would reject the latter anyway, and dropping
// here keeps a stray null/number in angular.json from failing the build.
// Returns null when nothing survives so the caller can omit the flag.
function normalizeHeaders(
  raw: { [key: string]: string } | null | undefined,
): string | null {
  if (!raw || typeof raw !== 'object') {
    return null;
  }
  const out: { [key: string]: string } = {};
  let count = 0;
  for (const [key, value] of Object.entries(raw)) {
    if (typeof value !== 'string') {
      continue;
    }
    const name = key.trim();
    if (!name) {
      continue;
    }
    out[name] = value;
    count++;
  }
  return count > 0 ? JSON.stringify(out) : null;
}

function parseConfigurationFromBuildTarget(buildTarget?: string): string | null {
  if (!buildTarget) {
    return null;
  }
  const parts = buildTarget.split(':');
  if (parts.length >= 3 && parts[2]) {
    return parts[2];
  }
  return null;
}

export function formatUrl(
  host: string,
  port: number,
  servePath: string | null = null,
  scheme: 'http' | 'https' = 'http',
): string {
  const isLoopbackName = host === 'localhost' || host === '0.0.0.0';
  const display = isLoopbackName ? 'localhost' : host;
  const suffix = servePath ?? '/';
  return `${scheme}://${display}:${port}${suffix}`;
}
