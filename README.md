# ngc-rs

A native Rust replacement for `ng build` in Angular projects. Drop-in swap, **~34x faster**.

### Early benchmark

> `ngc-rs info` resolves a project's file graph **34.4 ± 4.0x faster** than `tsc --listFiles --noEmit`.

> **Status: v0.1 — Project Resolver**
> ngc-rs can resolve an Angular project's full file dependency graph from `tsconfig.json`.
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

### Benchmark comparison

Compare resolution time against `tsc --listFiles` (requires [hyperfine](https://github.com/sharkdp/hyperfine)):

```sh
./scripts/bench_compare.sh /path/to/angular-project
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

- **v0.1** — Project Resolver (current)
- **v0.2** — TS Transform (strip types with oxc, emit plain JS)
- **v0.3** — Bundling (produce `dist/` matching `ng build` output)
- **v0.4** — Angular Template Compiler (native template compilation)
- **v1.0** — Angular CLI Drop-in (swap one line in `angular.json`)

## Contributing

Contributions are welcome! Please see [CONTRIBUTING.md](CONTRIBUTING.md) for guidelines.

## License

[MIT](LICENSE)
