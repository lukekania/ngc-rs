import { BuilderContext, BuilderOutput, createBuilder } from '@angular-devkit/architect';
import { Observable, Subject } from 'rxjs';

import {
  DevServerOptions,
  OptionTranslationError,
  TranslatedServeArgs,
  translateOptions,
} from './options';
import { locateNgcRs } from './locate';
import {
  loadProxyConfig,
  ProxyConfigError,
  ProxyRule,
  ProxyServerHandle,
  startProxyServer,
} from './proxy';
import { createNgcRsRunner, NgcEvent, summarizeDiagnostics } from './process';

interface DevServerBuilderOutput extends BuilderOutput {
  baseUrl: string;
  port?: number;
  address?: string;
}

export function execute(
  options: DevServerOptions,
  context: BuilderContext,
): Observable<BuilderOutput> {
  const subject = new Subject<DevServerBuilderOutput>();

  startServer(options, context, subject).catch((err) => {
    const message = (err as Error).message;
    context.logger.error(`ngc-rs builder failed: ${message}`);
    subject.next({ success: false, baseUrl: '', error: message });
    subject.complete();
  });

  return subject.asObservable();
}

async function startServer(
  options: DevServerOptions,
  context: BuilderContext,
  subject: Subject<DevServerBuilderOutput>,
): Promise<void> {
  let translated: TranslatedServeArgs;
  try {
    translated = translateOptions(options, context.workspaceRoot);
  } catch (err) {
    if (err instanceof OptionTranslationError) {
      throw err;
    }
    throw err;
  }

  const located = locateNgcRs(
    context.workspaceRoot,
    options.ngcRsBinary ?? null,
    process.env,
  );
  context.logger.info(
    `ngc-rs binary: ${located.binary} (resolved from ${located.source})`,
  );

  let proxyRules: ProxyRule[] | null = null;
  if (translated.proxyEnabled && translated.proxyConfigPath) {
    try {
      proxyRules = loadProxyConfig(translated.proxyConfigPath);
    } catch (err) {
      if (err instanceof ProxyConfigError) {
        throw err;
      }
      throw new ProxyConfigError(
        `unexpected error loading proxy config: ${(err as Error).message}`,
      );
    }
    context.logger.info(
      `loaded ${proxyRules.length} proxy rule(s) from ${translated.proxyConfigPath}`,
    );
  }

  const runner = createNgcRsRunner({
    binary: located.binary,
    args: translated.args,
    cwd: context.workspaceRoot,
  });

  let proxyHandle: ProxyServerHandle | null = null;

  context.addTeardown(async () => {
    await runner.stop();
    if (proxyHandle) {
      try {
        await proxyHandle.close();
      } catch (err) {
        context.logger.warn(`proxy shutdown error: ${(err as Error).message}`);
      }
    }
    subject.complete();
  });

  context.reportRunning();

  runner.events.on('event', (raw: NgcEvent) => {
    handleEvent(raw);
  });

  runner.start();

  function handleEvent(ev: NgcEvent): void {
    switch (ev.type) {
      case 'stdout':
        context.logger.info(ev.line);
        return;
      case 'stderr':
        context.logger.warn(ev.line);
        return;
      case 'ready':
        onReady(ev.address);
        return;
      case 'rebuild-success': {
        const baseUrl = currentBaseUrl();
        subject.next({ success: true, baseUrl });
        return;
      }
      case 'rebuild-failure': {
        const summary = summarizeDiagnostics(ev.diagnostics);
        context.logger.error(summary);
        subject.next({ success: false, baseUrl: currentBaseUrl(), error: summary });
        return;
      }
      case 'exit':
        if (ev.code !== 0 && ev.code !== null) {
          context.logger.error(`ngc-rs serve exited with code ${ev.code}`);
        }
        subject.complete();
        return;
    }
  }

  function currentBaseUrl(): string {
    if (proxyHandle) {
      const a = proxyHandle.address();
      return `http://${a.host}:${a.port}/`;
    }
    return translated.url;
  }

  function onReady(address: string): void {
    const upstreamPort = parsePort(address);
    if (proxyRules && upstreamPort !== null) {
      startProxyServer({
        rules: proxyRules,
        upstream: { host: '127.0.0.1', port: upstreamPort },
        listen: { host: translated.proxyHost, port: translated.proxyPort },
        log: (line) => context.logger.info(line),
      })
        .then((handle) => {
          proxyHandle = handle;
          const addr = handle.address();
          const url = `http://${addr.host}:${addr.port}/`;
          context.logger.info(`ngc-rs builder proxy listening on ${url}`);
          subject.next({
            success: true,
            baseUrl: url,
            port: addr.port,
            address: addr.host,
          });
          maybeOpenBrowser(translated.open, url, context);
        })
        .catch((err: Error) => {
          context.logger.error(`could not start proxy: ${err.message}`);
          subject.next({
            success: false,
            baseUrl: translated.url,
            error: `proxy startup failed: ${err.message}`,
          });
        });
      return;
    }

    const url = translated.url;
    context.logger.info(`ngc-rs serve ready at ${url}`);
    subject.next({
      success: true,
      baseUrl: url,
      port: translated.proxyPort,
      address: translated.proxyHost,
    });
    maybeOpenBrowser(translated.open, url, context);
  }
}

function parsePort(address: string): number | null {
  const match = /:(\d+)/.exec(address);
  if (!match || !match[1]) {
    return null;
  }
  const n = Number(match[1]);
  return Number.isFinite(n) ? n : null;
}

function maybeOpenBrowser(open: boolean, url: string, context: BuilderContext): void {
  if (!open) {
    return;
  }
  const cmd =
    process.platform === 'darwin' ? 'open'
      : process.platform === 'win32' ? 'cmd'
        : 'xdg-open';
  const args = process.platform === 'win32' ? ['/c', 'start', '', url] : [url];
  try {
    // eslint-disable-next-line @typescript-eslint/no-require-imports
    const { spawn } = require('node:child_process') as typeof import('node:child_process');
    const child = spawn(cmd, args, { detached: true, stdio: 'ignore' });
    child.unref();
  } catch (err) {
    context.logger.warn(`could not open browser: ${(err as Error).message}`);
  }
}

export default createBuilder<DevServerOptions>(execute);
