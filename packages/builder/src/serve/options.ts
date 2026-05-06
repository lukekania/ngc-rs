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
    url: formatUrl(userHost, userPort),
  };
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

export function formatUrl(host: string, port: number): string {
  const isLoopbackName = host === 'localhost' || host === '0.0.0.0';
  const display = isLoopbackName ? 'localhost' : host;
  return `http://${display}:${port}/`;
}
