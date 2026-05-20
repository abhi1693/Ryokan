<!-- Thanks for the PR! A few quick checks before you submit. -->

## What this changes

<!-- One or two sentences on the why, not just the what. Link the issue if there is one. -->

## Checklist

- [ ] Targets the `dev` branch (not `main`).
- [ ] `cargo fmt --all -- --check` is clean.
- [ ] `cargo clippy --workspace --all-targets --features test-support -- -D warnings` is clean.
- [ ] `cargo t` passes.
- [ ] Added or updated tests for the change (or it's a docs/chore-only PR).
- [ ] If this is an HTMX migration: a browser-e2e test was written against the old behavior first, then mutation-tested (see `tests/CLAUDE.md` for the migration discipline).
