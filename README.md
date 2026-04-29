# ngc-rs

A native Rust replacement for `ng build` in Angular projects. Drop-in swap, **~10× faster on real-world apps**.

### Benchmarks

| Project | `ngc-rs` | `ng build` | Speedup |
|---------|----------|------------|---------|
| Production Angular v21 app (~1,200 modules, 14 lazy chunks) | **~380 ms** | ~3,800 ms | **~10×** |

Measured with [hyperfine](https://github.com/sharkdp/hyperfine) on Apple Silicon, `-c production` (source maps, minification, tree shaking, content-hashed filenames).

> **Status: v0.8.4 — watch mode & dev server**
> ngc-rs reads `angular.json`, compiles templates to Ivy, bundles with code splitting, and emits production-ready output with source maps, minified code, tree-shaken exports, and content-hashed filenames. Every hot stage — project resolution, template compilation, TS transform, npm dependency crawl, Angular linker, tree-shaking, bundling, and minification — runs across rayon worker threads. `ngc-rs serve` provides a watch-mode dev server with live reload and an in-page error overlay; `@ngc-rs/builder` plugs into Angular's `ng serve` via the architect builder protocol.
> See the [milestones](https://github.com/lukekania/ngc-rs/milestones) for the roadmap toward a full `ng build` replacement.

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

## Installation

```sh
cargo install --git https://github.com/lukekania/ngc-rs
```

Or build from source:

```sh
git clone https://github.com/lukekania/ngc-rs.git
cd ngc-rs
cargo build --release
```

The binary will be at `target/release/ngc-rs`.

## Usage

### `ngc-rs info`

Resolve the project file graph and print a summary:

```sh
ngc-rs info --project tsconfig.json
```

```
ngc-rs project info
  Files:          1247
  Entry points:   3
  Edges:          4891
  Unresolved:     12
```

### `ngc-rs build`

Compile templates, transform TypeScript, and produce browser-ready output:

```sh
ngc-rs build --project tsconfig.app.json
```

When an `angular.json` is found, ngc-rs reads styles, assets, polyfills, and file replacements from it automatically. Output includes:

- `dist/main.{hash}.js` — ESM bundle with Ivy-compiled templates (content-hashed in production)
- `dist/chunk-*.{hash}.js` — lazy-loaded route chunks
- `dist/main.{hash}.js.map` — source maps (production: external files, development: inline)
- `dist/index.html` — with injected script/style tags
- `dist/styles.css` — concatenated global stylesheets
- `dist/polyfills.js` — polyfill imports
- `dist/assets/` — copied static assets
- `dist/3rdpartylicenses.txt` — third-party license texts

Additional flags:

```sh
# Production build (minification, source maps, content hashes, npm bundling)
ngc-rs build --project tsconfig.app.json -c production

# Development build (no optimizations, fast iteration)
ngc-rs build --project tsconfig.app.json

# Machine-readable JSON output
ngc-rs build --project tsconfig.app.json --output-json
```

### `ngc-rs serve`

Build the project, watch for source changes, and host `dist/` over HTTP with live reload — the `ng serve` equivalent for everyday Angular development:

```sh
ngc-rs serve --project tsconfig.app.json
```

The first build runs to completion, then the dev server starts listening (default `http://localhost:4200`) and the watcher takes over. Edits to `.ts`, `.html`, `.css`, and `angular.json` trigger an incremental rebuild; connected browsers reload via Server-Sent Events. Build failures are surfaced as an in-page error overlay with file/line/column.

Additional flags:

```sh
# Custom host/port
ngc-rs serve --project tsconfig.app.json --host 0.0.0.0 --port 4300

# Open the default browser once the server is listening
ngc-rs serve --project tsconfig.app.json --open

# Pick a non-default angular.json configuration
ngc-rs serve --project tsconfig.app.json -c development
```

#### `ng serve` integration

Projects already using Angular's `ng serve` can swap in ngc-rs without abandoning the CLI by installing the [`@ngc-rs/builder`](packages/builder) package and pointing the `serve` target at it in `angular.json`:

```json
"serve": {
  "builder": "@ngc-rs/builder:dev-server",
  "options": {
    "buildTarget": "my-app:build"
  }
}
```

Then run `ng serve` as normal — the builder shells out to the `ngc-rs` binary while continuing to speak the `@angular-devkit/architect` protocol so proxy config and editor integrations keep working.

### Benchmark comparison

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
- **v0.2** — TS Transform ✅ (strip types with oxc, emit plain JS)
- **v0.3** — Bundling ✅ (ESM concatenation with dependency ordering)
- **v0.4** — Angular Template Compiler ✅ (Ivy codegen, pest parser)
- **v0.5** — Build Output Completeness ✅ (angular.json, index.html, styles, assets, polyfills, fileReplacements)
- **v0.6** — Code Splitting & Lazy Routes ✅ (dynamic import detection, chunk graph, multi-file output)
- **v0.7** — Source Maps & Optimization ✅ (source maps, minification, content hashing, npm bundling)
- **v0.7.x** — Angular 21 & Performance ✅ (full-pipeline rayon parallelism, overlapped PostCSS, canonicalize cache — 4.2× → ~10× vs `ng build`)
- **v0.8** — Watch Mode & Dev Server ✅ (`ngc-rs serve`, file watcher, dev server with live reload + error overlay, `@ngc-rs/builder` for `ng serve`)
- **v1.0** — Angular CLI Drop-in (swap one line in `angular.json`). Angular linker for partially-compiled npm packages already landed.

## Contributing

Contributions are welcome! Please see [CONTRIBUTING.md](CONTRIBUTING.md) for guidelines.

## License

[MIT](LICENSE)
