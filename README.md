# ngc-rs

A native Rust replacement for `ng build` in Angular projects. Drop-in swap, **~72x faster**.

### Benchmarks

| Command | Time | Ratio |
|---------|------|-------|
| `ngc-rs build -c production` | **~52ms** | **~72x faster** |
| `ng build --configuration production` | ~3,810ms | baseline |

Measured with [hyperfine](https://github.com/sharkdp/hyperfine) on a real-world 77-module Angular v21 project. Production mode includes source maps, minification, tree shaking, and content-hashed filenames.

> **Status: v0.7 — Source Maps & Optimization**
> ngc-rs reads `angular.json`, compiles templates to Ivy, bundles with code splitting, and emits production-ready output with source maps, minified code, tree-shaken exports, and content-hashed filenames.
> See the [milestones](https://github.com/lukekania/ngc-rs/milestones) for the roadmap toward a full `ng build` replacement.

## Why is it faster?

The Angular CLI build pipeline runs on Node.js and is largely single-threaded. ngc-rs replaces it with a Rust binary that uses:

- **[oxc](https://oxc.rs/)** for native JS/TS parsing (v0.2+)
- **[rayon](https://github.com/rayon-rs/rayon)** for parallel file processing
- **[petgraph](https://github.com/petgraph/petgraph)** for the file dependency graph

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

### Benchmark comparison

Compare against `tsc` (requires [hyperfine](https://github.com/sharkdp/hyperfine)):

```sh
# Resolution
hyperfine --warmup 3 -i -N \
  './target/release/ngc-rs info --project /path/to/tsconfig.app.json' \
  'npx tsc --project /path/to/tsconfig.app.json --listFiles --noEmit'

# Transform
hyperfine --warmup 3 -i -N \
  './target/release/ngc-rs build --project /path/to/tsconfig.app.json --out-dir /tmp/ngc-rs-out' \
  'npx tsc --project /path/to/tsconfig.app.json --outDir /tmp/tsc-out --noCheck'
```

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
- **v0.8** — Watch Mode & Dev Server
- **v1.0** — Angular CLI Drop-in (swap one line in `angular.json`)

## Contributing

Contributions are welcome! Please see [CONTRIBUTING.md](CONTRIBUTING.md) for guidelines.

## License

[MIT](LICENSE)
