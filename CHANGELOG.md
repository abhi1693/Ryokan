# Changelog

## 1.9.13 (2026-10-23)

- Update ctutils to 0.4.4 for RUSTSEC-2026-0385, including upstream constant-time mask and `u128` comparison fixes; retain the Rust 1.95 minimum and normal audit gate.
- Refresh all 42 direct Rust dependencies and compatible transitive packages, including Utoipa 6, Swagger UI 10, Scraper 0.29, Nix 0.31, and rstest 0.28.
- Update vendored HTMX to 2.0.11 and Zensical to 0.0.71, with verified browser-asset provenance and its retained license.
- Update ffprobe to 9.0.2-1 and pin the Rust 1.99.1 builder, ffprobe source, and Ubuntu 26.04 LTS runtime to registry-verified multi-platform image digests. Keep the declared Rust 1.95 minimum.
- Rebuild the unchanged upstream gosu 1.19 source with Go 1.27.3, retain its third-party notices, and explicitly install the native C++ parser's runtime library.
- Require native AMD64/ARM64 image scans, SBOMs, source/release version parity and real isolated HTTP/runtime checks before publication, including unfixed HIGH/CRITICAL vulnerabilities and secrets.
- Preserve the already shipped 1.9.9–1.9.12 episode-parsing and complete-series import fixes that are ahead of the fork's default branch.
- Add a read-only locked-dependency audit to the active PR workflow, covering forks where the inherited audit workflow is disabled.
