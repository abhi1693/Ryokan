# Third-party license notices

Dependency changes targeting `main` must include matching notices in
`THIRD_PARTY_LICENSES.html`. Regenerate with
`cargo about generate -c about.toml about.hbs -o licenses/THIRD_PARTY_LICENSES.html`.
The existing `license-notices.yml` regeneration on `dev` is preserved; it does
not repair a pull request targeting `main`, and no auto-commit workflow is added
on `main`.

The Rust version job runs `python3 scripts/check_license_notices.py` and its
stdlib tests before installing build tools. It checks that every listed
crate/version pair exists exactly in `Cargo.lock`, rejects empty or malformed
notice lists, and supports multiple locked versions and repeated attribution
under different licenses. This is a notice membership guard, not a claim that
every locked package is listed: cargo-about's default selection can omit
platform-specific and development dependencies. License text remains generated
by cargo-about.
