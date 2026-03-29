# ngc-rs

## What this is

A Rust CLI tool that replaces `ng build` in Angular projects with a native,
significantly faster alternative. Angular developers should be able to swap
one line in angular.json and run `ngc-rs build` instead of `ng build` and
get identical dist/ output, 5-10x faster.

## Why it is faster

The Angular CLI build pipeline runs on Node.js and is largely single-threaded.
ngc-rs replaces it with a Rust binary that uses:

- oxc for native JS/TS parsing
- rayon for parallel file processing
- Rolldown-style bundling primitives
- tsc --noEmit as a subprocess for type-checking (we do not reimplement the TS type system)

## Milestones

### v0.1 — Project Resolver ✅

Goal: resolve an Angular project's file graph from tsconfig.json, fast.

- [x] Workspace scaffolded with crates: cli, project-resolver, diagnostics
- [x] tsconfig.json parsing with `extends` chain resolution
- [x] File dependency graph using petgraph
- [x] Parallel file scanning with rayon
- [x] `ngc-rs info` prints file count, entry points, graph summary
- [x] Benchmark harness comparing resolution time vs tsc --listFiles
- [x] Snapshot tests with insta against tests/fixtures/simple-app

### v0.2 — TS Transform ✅

Goal: parse and transform TypeScript files without tsc.

- [x] Integrate oxc for JS/TS parsing
- [x] Strip type annotations and decorators
- [x] Emit plain JS for each input file
- [x] ngc-rs build produces a raw JS file tree in out/

### v0.3 — Bundling ✅

Goal: produce a single bundle from the JS file tree.

- [x] Custom ESM concatenation bundler (Rolldown requires async, violates rayon-only rule)
- [x] ngc-rs build produces dist/main.js with all project code bundled
- [x] Integration + snapshot tests verify bundle structure and content

### v0.4 — Angular Template Compiler ✅

Goal: compile Angular component templates natively.

- [x] Parser for Angular template syntax (pest grammar)
- [x] Ivy codegen: emit ɵɵdefineComponent, ɵfac, and template function
- [x] Supports elements, text, interpolation, bindings, events, @if/@for/@switch, pipes
- [x] templateUrl resolution, two-way binding, ng-content, ng-container, ng-template
- [x] Structural directives (*ngIf, *ngFor), template reference variables (#ref)
- [x] Styles extraction and emission in defineComponent

### v0.5 — Build Output Completeness ✅

Goal: produce browser-loadable output from ngc-rs build.

- [x] angular.json parsing (styles, assets, polyfills, fileReplacements)
- [x] Index HTML generation with script/link injection
- [x] Global styles extraction (concatenate CSS files → dist/styles.css)
- [x] Asset copying (src/assets/ → dist/assets/)
- [x] Polyfills bundle (dist/polyfills.js)
- [x] fileReplacements (environment file swapping per configuration)
- [x] 3rdpartylicenses.txt generation
- [x] JSON output mode (--output-json for builder integration)

### v0.6 — Code Splitting & Lazy Routes

Goal: support lazy-loaded Angular routes with separate chunk files.

- [ ] Dynamic import() detection in bundler
- [ ] Chunk graph construction (main + lazy chunks + shared chunks)
- [ ] Multi-file bundle output (main.js + chunk-*.js)
- [ ] Import rewriting for chunk filenames

### v0.7 — Source Maps & Optimization

Goal: source maps and production-mode optimized builds.

- [ ] Source map generation through full pipeline (transform → bundle)
- [ ] Minification (oxc_minifier or equivalent)
- [ ] Tree shaking / dead code elimination
- [ ] Production vs development build modes (--configuration)

### v0.8 — Watch Mode & Dev Server

Goal: file watching, incremental rebuilds, and ng serve support.

- [ ] File watcher with incremental rebuilds (notify crate)
- [ ] HTTP dev server with live reload
- [ ] ngc-rs serve command
- [ ] ng serve integration via builder adapter

### v1.0 — Angular CLI Drop-in

Goal: zero-config swap for Angular developers.

- [ ] npm binary distribution (platform-specific packages)
- [ ] Angular builder adapter (speaks @angular-devkit builder protocol)
- [ ] angular.json integration: swap builder, run ng build as normal
- [ ] Works on Angular 17+ projects out of the box
- [ ] GitHub Actions cross-compile release workflow
- [ ] Published to crates.io and npm (@ngc-rs/cli wrapper)
- [ ] Documentation: README with install guide + performance GIF

## Architecture (planned)

tsconfig.json
│
▼
ProjectResolver ← crates/project-resolver [v0.1]
│
▼
TsParser (oxc) ← crates/ts-parser [v0.2]
│
▼
TemplateCompiler ← crates/template-compiler [v0.4]
│
▼
Bundler ← crates/bundler [v0.3]
│
▼
dist/

## Commit Conventions

- **One-liner messages only** — no body, no `Co-Authored-By`
- **Format:** `<prefix>: <short description in lowercase>`
- **Prefixes:**
  - `feat:` — new feature
  - `fix:` — bug fix
  - `refactor:` — code restructuring without behavior change
  - `chore:` — maintenance, config, dependencies
  - `docs:` — documentation changes
  - `test:` — adding or updating tests
  - `style:` — formatting, whitespace (no logic change)

## Coding rules

- NEVER use .unwrap() in library crates. Use ? or explicit match.
- NEVER use println! in library crates. Use tracing::info! / tracing::debug!
- ALWAYS run cargo fmt and cargo clippy -- -D warnings before finishing a task
- ALWAYS add a unit test when adding a public function
- Parallelism via rayon only. No async unless forced by an external dependency.
- All errors flow through crates/diagnostics::NgcError. No ad-hoc error strings.
- Every pub item needs a /// doc comment.

## Verification — run this to confirm work is correct

cargo test --workspace && cargo clippy -- -D warnings && cargo fmt --check

## Key dependencies and rationale

| Crate      | Purpose                        | Notes                        |
| ---------- | ------------------------------ | ---------------------------- |
| clap       | CLI parsing                    | Use derive macros            |
| serde_json | tsconfig parsing               |                              |
| petgraph   | File dependency graph          | DiGraph<PathBuf, ()>         |
| oxc        | JS/TS parsing and transforms   | Replaces Babel/SWC, v0.2+    |
| rayon      | Data parallelism               |                              |
| tracing    | Structured logging             |                              |
| insta      | Snapshot tests                 | Run cargo insta review after |
| colored    | Terminal colors in diagnostics |                              |

## Out of scope — do not implement, open a GitHub issue instead

- Full TypeScript type checking in Rust (delegate to tsc --noEmit)
- SSR / @angular/ssr support
- Angular < v17
- ng-packagr / library publishing
- Webpack compatibility layer

## Decisions log

### Delegate type-checking to tsc

Do not reimplement the TypeScript type system.
The check command runs tsc --noEmit as a subprocess.
Revisit when oxc-checker matures enough for production use.

### Use oxc over swc

Faster, cleaner Rust API, more actively maintained as of early 2026.

### Custom bundler over Rolldown

Rolldown requires tokio/async which violates the rayon-only rule. Its Rust crate API
is undocumented and targets JS consumers via napi-rs. We already own the dependency
graph via petgraph, making a custom ESM concatenation bundler simpler and lighter.

## Milestone workflow

When starting a new milestone:

1. **Branch:** `git checkout -b milestone/v<X.Y.Z>-<descriptive-name>` from main
2. **Implement incrementally:** after each logical step, verify with
   `cargo test --workspace && cargo clippy -- -D warnings && cargo fmt --check`
3. **Commit often:** one commit per logical change, using the commit conventions above
4. **PR:** when DoD is met, create a PR targeting main via `gh pr create`
5. **Version bump:** update `workspace.package.version` in root Cargo.toml
6. **Update CLAUDE.md:** mark completed milestone items with `[x]` and ✅
