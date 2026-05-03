<!--
Thanks for contributing to ngc-rs!

For non-trivial changes (anything beyond a typo or single-line fix), please
open a GitHub issue first to discuss the approach. CI does not run on
outside-contributor PRs until a maintainer approves it — that's expected.

See CONTRIBUTING.md for the full guidelines.
-->

## Summary

<!-- One or two sentences. What changed and why. -->

## Linked issue

<!-- For non-trivial changes: `Fixes #123` or `Refs #123`. Single-line typo
fixes can omit this. -->

## Checklist

- [ ] `cargo test --workspace` passes locally
- [ ] `cargo clippy --workspace --all-targets -- -D warnings` clean
- [ ] `cargo fmt --check` clean
- [ ] If touching `packages/builder/`: `npm test && npm run lint` clean
- [ ] If changing build pipeline behavior: diffed `dist/` output against
      vanilla `ng build` baseline on a real Angular 17+ project (see
      "Verifying build-pipeline changes" in CONTRIBUTING.md)
- [ ] Conventional-commit title prefix (`feat:`, `fix:`, `chore:`,
      `refactor:`, `docs:`, `test:`, `style:`, `perf:`)
- [ ] Public items have `///` doc comments
