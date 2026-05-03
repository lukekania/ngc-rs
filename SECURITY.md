# Security Policy

## Reporting a Vulnerability

If you believe you've found a security issue in ngc-rs, please report it
**privately** rather than opening a public issue. The preferred channel is
GitHub's private vulnerability reporting:

1. Go to <https://github.com/lukekania/ngc-rs/security/advisories/new>
2. Fill in the form with reproduction steps, impact, and any proof of concept
3. We aim to acknowledge within 7 days and ship a fix within 30 days for
   confirmed issues

If you cannot use GitHub's private reporting, email the maintainer at the
address listed on their GitHub profile and prefix the subject with
`[ngc-rs security]`.

Please do not disclose the issue publicly until a fix is released.

## Scope

In scope:

- The `ngc-rs` Rust binary and every workspace crate (`crates/*`)
- The `@ngc-rs/cli`, `@ngc-rs/cli-*`, and `@ngc-rs/builder` npm packages
- The release pipeline (`.github/workflows/release.yml`) and the workflow
  that publishes to npm and crates.io

Out of scope:

- Vulnerabilities in third-party dependencies (please report those upstream)
- Issues that require unrealistic threat models (e.g. attacker has write
  access to the source tree)

## Supply-Chain & CI Security

ngc-rs is a build tool that runs against user source code. We take
supply-chain security seriously. The mitigations currently in place:

- **Pinned actions** — every third-party GitHub Action across `ci.yml`,
  `release.yml`, and `release-notes.yml` is pinned to a commit SHA, not a
  version tag. A force-push to a tag cannot inject code into our CI.
- **Tag-only release trigger** — the publish stages of `release.yml` only
  fire when a `v*` tag is pushed. PR runs of `release.yml` perform a dry
  run with no secret access.
- **Approval-gated workflows** — outside-contributor pull requests do
  **not** trigger any workflow run automatically. A maintainer must click
  "Approve and run" before any job executes against an outside PR. If you
  open a PR and CI doesn't start, that's expected — it's waiting for our
  review.
- **Tag protection** — `v*` tags can only be pushed by repository admins.
- **Branch protection on `main`** — direct pushes are blocked; every
  change lands via PR with required CI passing.
- **Minimal `permissions:` blocks** — every workflow declares the smallest
  token scope it needs (`contents: read` for CI; `contents: write` +
  `id-token: write` for the release).
- **OIDC provenance** — npm packages are published with `--provenance`,
  which links each release back to the workflow run that built it.
- **Token scoping & rotation** — `NPM_TOKEN` is a granular access token
  scoped to the `@ngc-rs/*` org with publish + write permissions only;
  npm caps granular access tokens at **90 days**, so this token is
  rotated quarterly (set a calendar reminder; the workflow fails
  cleanly with a 401 once the token expires). `CARGO_REGISTRY_TOKEN`
  is scoped to the `ngc-rs` crate; crates.io tokens have no forced
  expiry but are rotated annually. Both are rotated immediately on
  any suspected exposure.

## Contribution Policy

For non-trivial changes, please open a GitHub issue first. This both
gives us advance notice and lets us shape the change before you invest
time. See [CONTRIBUTING.md](CONTRIBUTING.md) for details.
