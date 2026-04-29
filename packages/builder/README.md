# @ngc-rs/builder

Angular `@angular-devkit/architect` builders backed by [ngc-rs](https://github.com/lukekania/ngc-rs).

## Builders

| Name         | Status              |
| ------------ | ------------------- |
| `dev-server` | Implemented (#27)   |
| `application`| Reserved for #28    |

## Usage

Install:

```sh
npm install --save-dev @ngc-rs/builder
```

Edit `angular.json`, swap the `serve` target's builder:

```json
{
  "projects": {
    "my-app": {
      "architect": {
        "serve": {
          "builder": "@ngc-rs/builder:dev-server",
          "options": {
            "buildTarget": "my-app:build:development",
            "port": 4200,
            "host": "localhost",
            "proxyConfig": "proxy.conf.json"
          }
        }
      }
    }
  }
}
```

Then run `ng serve` as usual.

## Binary discovery

The builder spawns the `ngc-rs` binary in this priority order:

1. `ngcRsBinary` builder option (resolved relative to the workspace root).
2. `NGC_RS_BINARY` environment variable.
3. `<workspaceRoot>/target/release/ngc-rs` (and parent directories) — convenient
   for local development against a Cargo workspace.
4. `ngc-rs` from `PATH`.

## Proxy support

`proxyConfig` is implemented in the Node shim rather than in `ngc-rs serve`.
When set, the shim listens on the user-configured `host`/`port`, spawns
`ngc-rs serve` on an ephemeral loopback port, and forwards requests either to
the configured proxy target or back to `ngc-rs`. Both the webpack-style map
form and Angular's array form (`{ context, target, ... }`) are supported.
WebSocket upgrades are not yet forwarded.

## Unsupported options

`ssl`, `sslKey`, `sslCert`, `hmr`, `define`, `headers`, `liveReload`, `watch`,
`poll`, `inspect`, `prebundle`, `allowedHosts`, `servePath` and `verbose` are
not yet supported. `ssl=true` produces a hard error rather than silently
falling back to HTTP. The remaining options are accepted only in the schema
fields listed in `schemas/dev-server.json`.

## Development

```sh
cd packages/builder
npm install
npm run build
npm test
```
