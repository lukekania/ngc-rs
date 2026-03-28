# ngc-rs

A native Rust replacement for `ng build` in Angular projects. Drop-in swap, **~19x faster**.

### Benchmarks

| Command | vs tsc equivalent | Ratio |
|---------|-------------------|-------|
| `ngc-rs info` (file graph resolution) | `tsc --listFiles --noEmit` | **~34x faster** |
| `ngc-rs build` (full pipeline: resolve + compile + bundle) | `tsc --outDir` | **~19x faster** |

Measured with [hyperfine](https://github.com/sharkdp/hyperfine) on a real-world 77-module Angular project. ngc-rs completes the full build pipeline in **~20ms** vs **~370ms** for tsc.

> **Status: v0.4 — Angular Template Compiler**
> ngc-rs can resolve, transform, compile Angular templates to Ivy, and bundle into a single `dist/main.js`.
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

Compile templates, transform TypeScript, and bundle into a single file:

```sh
ngc-rs build --project tsconfig.app.json --out-dir dist
```

Produces `dist/main.js` — a single ESM bundle with Ivy-compiled templates, hoisted external imports, and all project-local code concatenated in dependency order.

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
cargo test --workspace && cargo clippy -- -D warnings && cargo fmt --check
```

## Roadmap

See the [GitHub milestones](https://github.com/lukekania/ngc-rs/milestones) for the full plan:

- **v0.1** — Project Resolver ✅
- **v0.2** — TS Transform ✅ (strip types with oxc, emit plain JS)
- **v0.3** — Bundling ✅ (ESM concatenation with dependency ordering)
- **v0.4** — Angular Template Compiler ✅ (Ivy codegen, pest parser)
- **v1.0** — Angular CLI Drop-in (swap one line in `angular.json`)

## Contributing

Contributions are welcome! Please see [CONTRIBUTING.md](CONTRIBUTING.md) for guidelines.

## License

[MIT](LICENSE)
