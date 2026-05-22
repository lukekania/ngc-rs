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
  if (raw.ssl === true) {
    throw new OptionTranslationError(
      'ssl=true is not yet supported by ngc-rs serve. Remove the option or run a separate TLS-terminating proxy in front of ngc-rs.',
    );
  }
  if (raw.sslKey || raw.sslCert) {
    throw new OptionTranslationError(
      'sslKey/sslCert are not yet supported by ngc-rs serve.',
    );
  }

  const userPort = raw.port ?? DEFAULT_PORT;
  const userHost = raw.host ?? DEFAULT_HOST;
  const open = raw.open === true;

  const configuration = parseConfigurationFromBuildTarget(raw.buildTarget);
  const project = raw.project ?? 'tsconfig.json';
  const servePath = normalizeServePath(raw.servePath ?? null);

  const proxyConfigPath = raw.proxyConfig
    ? path.resolve(workspaceRoot, raw.proxyConfig)
    : null;
  const proxyEnabled = proxyConfigPath !== null;

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
    url: formatUrl(userHost, userPort, servePath),
  };
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
): string {
  const isLoopbackName = host === 'localhost' || host === '0.0.0.0';
  const display = isLoopbackName ? 'localhost' : host;
  const suffix = servePath ?? '/';
  return `http://${display}:${port}${suffix}`;
}
