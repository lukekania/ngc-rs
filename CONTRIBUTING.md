# Contributing to ngc-rs

Thanks for your interest in contributing!

## Before opening a PR

For non-trivial changes (anything beyond a typo or single-line fix),
**please open a GitHub issue first** to discuss the approach. We require
this both to give us advance notice and to keep the contribution surface
area something we can review carefully — outside-contributor PRs do not
run CI automatically and require maintainer approval before any workflow
executes against them. See [SECURITY.md](SECURITY.md) for the
threat model behind this policy.

For one-line fixes (typos in comments, README, etc.), feel free to
open the PR directly.

## Getting started

1. Fork and clone the repo
2. Install Rust (stable) via [rustup](https://rustup.rs/)
3. Run `cargo test --workspace` to make sure everything works
4. For changes touching `packages/builder/`: `cd packages/builder && npm install && npm test`

## Development workflow

```sh
# Run all checks (same as CI)
cargo test --workspace && cargo clippy --workspace --all-targets -- -D warnings && cargo fmt --check
```

## Code conventions

- **No `.unwrap()`** in library crates — use `?` or explicit `match`
- **No `println!`** in library crates — use `tracing::info!` / `tracing::debug!`
- All errors flow through `crates/diagnostics::NgcError`
- Every `pub` item needs a `///` doc comment
- Parallelism via `rayon` only — no async unless forced by a dependency
- Run `cargo fmt` and `cargo clippy --workspace --all-targets -- -D warnings` before submitting

## Snapshot tests

We use [insta](https://insta.rs/) for snapshot tests. After adding or changing snapshots:

```sh
cargo insta review
```

## Commit messages

One-liner conventional-commit format:

```
<prefix>: <short description in lowercase>
```

Prefixes: `feat:`, `fix:`, `refactor:`, `chore:`, `docs:`, `test:`,
`style:`, `perf:`. No body, no `Co-Authored-By` trailer. Wrap any
`@`-prefixed token in backticks (e.g. `` `@for` ``, `` `@angular/core` ``)
to avoid pinging unrelated GitHub users.

## Verifying build-pipeline changes

When changing anything in the build pipeline (resolver, transform,
template compiler, linker, bundler, npm-resolver, dev-server), the
standing rule is to diff `dist/` output against vanilla `ng build` on a
real Angular 17+ project. The maintainer's reference project is
treasr-frontend (Angular 21, standalone, signals, zoneless). Any
non-trivial pipeline change should include a confirmation that this
diff is empty (or that any new differences are deliberate).

## Release & secret rotation

Releases are tag-triggered (`v*` → cargo-dist + npm publish workflow).
The required repository secrets are:

- `NPM_TOKEN` — granular access token scoped to the `@ngc-rs/*` org with
  publish + write permissions. Used by the `publish-npm` job in
  `.github/workflows/release.yml`. **npm caps granular access tokens at
  90 days**, so rotate quarterly.
- `CARGO_REGISTRY_TOKEN` — scoped to the `ngc-rs` crate. Used by the
  crates.io publish step. crates.io tokens have no forced expiry; rotate
  annually.

Rotate both immediately on any suspected exposure and document the
rotation in the release notes.

## License

ngc-rs is licensed under either of [Apache-2.0](LICENSE-APACHE) or
[MIT](LICENSE-MIT) at your option.

Unless you explicitly state otherwise, any contribution intentionally
submitted for inclusion in the work by you, as defined in the
Apache-2.0 license, shall be dual licensed as above, without any
additional terms or conditions.
