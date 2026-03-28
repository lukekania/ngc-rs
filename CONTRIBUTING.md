# Contributing to ngc-rs

Thanks for your interest in contributing!

## Getting started

1. Fork and clone the repo
2. Install Rust (stable) via [rustup](https://rustup.rs/)
3. Run `cargo test --workspace` to make sure everything works

## Development workflow

```sh
# Run all checks (same as CI)
cargo test --workspace && cargo clippy -- -D warnings && cargo fmt --check
```

## Code conventions

- **No `.unwrap()`** in library crates — use `?` or explicit `match`
- **No `println!`** in library crates — use `tracing::info!` / `tracing::debug!`
- All errors flow through `crates/diagnostics::NgcError`
- Every `pub` item needs a `///` doc comment
- Parallelism via `rayon` only — no async unless forced by a dependency
- Run `cargo fmt` and `cargo clippy -- -D warnings` before submitting

## Snapshot tests

We use [insta](https://insta.rs/) for snapshot tests. After adding or changing snapshots:

```sh
cargo insta review
```

## Commit messages

```
<prefix>: <short description in lowercase>
```

Prefixes: `feat:`, `fix:`, `refactor:`, `chore:`, `docs:`, `test:`, `style:`

## Reporting issues

Open an issue on GitHub. If you're reporting a bug, include:

- Your Rust version (`rustc --version`)
- Angular project structure that triggers the issue
- Full error output
