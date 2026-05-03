# `@ngc-rs/cli`

Native Rust replacement for `ng build`. Drop-in for Angular 17+ projects, ~10x faster on real apps.

## Install

```sh
npm i -D @ngc-rs/cli @ngc-rs/builder
```

`@ngc-rs/cli` ships a small Node wrapper plus a platform-specific binary delivered via `optionalDependencies` (the esbuild/biome/swc pattern — no postinstall, no network calls during install). Supported targets: `darwin-arm64`, `darwin-x64`, `linux-arm64`, `linux-x64`, `win32-x64`.

## Use it as a drop-in `ng build`

In your `angular.json`, change one line:

```diff
 "build": {
-  "builder": "@angular/build:application",
+  "builder": "@ngc-rs/builder:application",
   "options": { ... }
 }
```

Run `ng build` as normal — the builder shells out to the `ngc-rs` binary while continuing to speak the `@angular-devkit/architect` protocol.

## Use the binary directly

```sh
npx ngc-rs build --project tsconfig.app.json
npx ngc-rs build --project tsconfig.app.json -c production
```

See the [main README](https://github.com/lukekania/ngc-rs#readme) for full subcommand documentation.

## License

Licensed under either of [Apache-2.0](https://github.com/lukekania/ngc-rs/blob/main/LICENSE-APACHE) or [MIT](https://github.com/lukekania/ngc-rs/blob/main/LICENSE-MIT) at your option.
