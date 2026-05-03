# ngc-rs

A native Rust replacement for `ng build` in Angular projects. Drop-in swap, **~10× faster on real-world apps**.

### Benchmarks

| Project | `ngc-rs` | `ng build` | Speedup |
|---------|----------|------------|---------|
| Production Angular v21 app (~1,200 modules, 14 lazy chunks) | **~380 ms** | ~3,800 ms | **~10×** |

Measured with [hyperfine](https://github.com/sharkdp/hyperfine) on Apple Silicon, `-c production` (source maps, minification, tree shaking, content-hashed filenames).

> **Status: v1.0.0 — Angular CLI drop-in.** Add two npm packages, change one line in `angular.json`, run `ng build` as normal — your Angular project gets the Rust pipeline with no other code changes.

## Install (recommended: npm)

```sh
npm i -D @ngc-rs/cli @ngc-rs/builder
```

`@ngc-rs/cli` ships a small Node wrapper plus the right binary for your platform via `optionalDependencies` (the esbuild/biome/swc pattern — no postinstall, no network call during install). Supported targets: `darwin-arm64`, `darwin-x64`, `linux-arm64`, `linux-x64`, `win32-x64`.

In `angular.json`, change the builder line on your `build` (and optionally `serve`) target:

```diff
 "build": {
-  "builder": "@angular/build:application",
+  "builder": "@ngc-rs/builder:application",
   "options": { ... }
 },
 "serve": {
-  "builder": "@angular/build:dev-server",
+  "builder": "@ngc-rs/builder:dev-server",
   "options": {
     "buildTarget": "my-app:build"
   }
 }
```

Then run `ng build` (or `ng serve`) as normal. The builder shells out to the `ngc-rs` binary while continuing to speak the `@angular-devkit/architect` protocol, so editor integrations, proxy configs, and `--configuration` overrides keep working.

## Install (Rust users)

```sh
cargo install ngc-rs
```

Or build from source:

```sh
git clone https://github.com/lukekania/ngc-rs.git
cd ngc-rs
cargo build --release
```

The binary will be at `target/release/ngc-rs`.

## Why is it faster?

The Angular CLI build pipeline runs on Node.js and is largely single-threaded. ngc-rs replaces it with a Rust binary that is multi-threaded end-to-end:

- **[oxc](https://oxc.rs/)** for native JS/TS parsing, codegen, and minification
- **[rayon](https://github.com/rayon-rs/rayon)** for parallel per-file work at every stage
- **[petgraph](https://github.com/petgraph/petgraph)** for the file dependency graph
- **[dashmap](https://github.com/xacrimon/dashmap)** for a shared `canonicalize()` cache across worker threads — collapses duplicate filesystem `stat` syscalls

Additional wins on the critical path:
- **PostCSS/Tailwind subprocess overlaps with bundling** rather than running after it — ~200 ms of wallclock absorbed
- **Per-chunk bundling, minification, and tree-shake** all fan out to worker threads
- **npm dependency BFS** resolves each frontier level in parallel
- **Linker** (`ɵɵngDeclare*` → `ɵɵdefine*` for partially-compiled npm packages) processes all three of its passes in parallel

Type-checking is delegated to `tsc --noEmit` as a subprocess — we don't reimplement the TypeScript type system.

## CLI usage

When invoked directly (the Node wrapper or `cargo install`-ed binary), the same subcommands are available.

### `ngc-rs info`

Resolve the project file graph and print a summary:

```sh
ngc-rs info --project tsconfig.json
```

### `ngc-rs build`

Compile templates, transform TypeScript, and produce browser-ready output:

```sh
# Production build (minification, source maps, content hashes, npm bundling)
ngc-rs build --project tsconfig.app.json -c production

# Development build (no optimizations, fast iteration)
ngc-rs build --project tsconfig.app.json

# Machine-readable JSON output (consumed by @ngc-rs/builder)
ngc-rs build --project tsconfig.app.json --output-json
```

When an `angular.json` is found, ngc-rs reads styles, assets, polyfills, and file replacements from it automatically. Output includes:

- `dist/main.{hash}.js` — ESM bundle with Ivy-compiled templates
- `dist/chunk-*.{hash}.js` — lazy-loaded route chunks
- `dist/main.{hash}.js.map` — source maps (production: external, development: inline)
- `dist/index.html` — with injected script/style tags
- `dist/styles.css` — concatenated global stylesheets
- `dist/polyfills.js` — polyfill imports
- `dist/assets/` — copied static assets
- `dist/3rdpartylicenses.txt` — third-party license texts

### `ngc-rs serve`

Build the project, watch for source changes, and host `dist/` over HTTP with live reload — the `ng serve` equivalent for everyday Angular development:

```sh
ngc-rs serve --project tsconfig.app.json
ngc-rs serve --project tsconfig.app.json --host 0.0.0.0 --port 4300 --open
```

## Benchmark comparison

Reproduce the headline number against `ng build` with [hyperfine](https://github.com/sharkdp/hyperfine):

```sh
cargo build --release

hyperfine --warmup 3 \
  "./target/release/ngc-rs build --project /path/to/tsconfig.app.json --out-dir /tmp/ngc-rs-out -c production" \
  "npx ng build --configuration production"
```

Run the `ng build` invocation from inside the Angular project directory, or pass a `cwd` flag. Both commands include full production optimizations.

## Development

```sh
# Run tests
cargo test --workspace

# Lint
cargo clippy --workspace --all-targets -- -D warnings

# Format
cargo fmt --all

# All checks (CI runs this)
cargo test --workspace && cargo clippy --workspace --all-targets -- -D warnings && cargo fmt --check
```

## Roadmap

See the [GitHub milestones](https://github.com/lukekania/ngc-rs/milestones) for the full plan:

- **v0.1** — Project Resolver ✅
- **v0.2** — TS Transform ✅
- **v0.3** — Bundling ✅
- **v0.4** — Angular Template Compiler ✅
- **v0.5** — Build Output Completeness ✅
- **v0.6** — Code Splitting & Lazy Routes ✅
- **v0.7** — Source Maps & Optimization ✅
- **v0.8** — Watch Mode & Dev Server ✅
- **v1.0** — Angular CLI Drop-in ✅ (npm distribution, `application` builder, cross-compile release pipeline)

## Contributing

Contributions are welcome — please read [CONTRIBUTING.md](CONTRIBUTING.md) first. For non-trivial changes, open an issue before opening a PR. Outside-contributor PRs do not run CI automatically; a maintainer will approve and run the workflow.

For security reports, see [SECURITY.md](SECURITY.md).

## License

Licensed under either of

- Apache License, Version 2.0 ([LICENSE-APACHE](LICENSE-APACHE) or <http://www.apache.org/licenses/LICENSE-2.0>)
- MIT license ([LICENSE-MIT](LICENSE-MIT) or <http://opensource.org/licenses/MIT>)

at your option.

### Contribution

Unless you explicitly state otherwise, any contribution intentionally submitted for inclusion in the work by you, as defined in the Apache-2.0 license, shall be dual licensed as above, without any additional terms or conditions.
