# Changelog

## 1.9.13 (unreleased)

- Refresh all 42 direct Rust dependencies and compatible transitive packages, including Utoipa 6, Swagger UI 10, Scraper 0.29, Nix 0.31, and rstest 0.28.
- Update vendored HTMX to 2.0.11 and Zensical to 0.0.71, with verified browser-asset provenance and its retained license.
- Update ffprobe to 9.0.2-1 and pin the Rust 1.99.1 builder, ffprobe source, and Debian runtime to registry-verified multi-platform image digests. Keep the declared Rust 1.95 minimum.
- Preserve the already shipped 1.9.9–1.9.12 episode-parsing and complete-series import fixes that are ahead of the fork's default branch.
