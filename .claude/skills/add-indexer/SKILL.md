---
name: add-indexer
description: Step-by-step workflow for adding a new Indexer impl beyond torznab/newznab (e.g., a private-tracker REST shape, a hypothetical anime-specific protocol). Walks the trait impl → caps fetcher → IndexerCache wiring → settings UI → CLAUDE.md notes. Skip this skill for adding a new torznab/newznab *deployment* (e.g., a new Prowlarr/Jackett indexer row) — that's just a Settings UI add. This skill is for adding a new *protocol*.
---

# Adding a new Indexer protocol impl

The `Indexer` trait + `IndexerCache` machinery is what runs **alongside** the direct Nyaa scraper. Nyaa stays out-of-band per plan decision #1 — never refactor Nyaa into this trait. The search pipeline dispatches to Nyaa-direct + fans out to `Indexer` impls in parallel and merges.

Currently one impl: `torznab` (which doubles as `newznab` since the wire format is identical — they only differ in download URL semantics and category mapping). If you're adding a fundamentally new wire shape (e.g., a JSON-API private tracker, a SOAP interface), this is your skill.

## When this skill does NOT apply

- **Adding a new Prowlarr/Jackett indexer row**: that's a Settings → Indexers UI add, no code change. The torznab impl handles the wire.
- **Adding a category alias / search-mode tweak to torznab**: edit `src/services/indexers/torznab/`, not a new protocol.
- **Adding a generic RSS feed source (e.g., SubsPlease)**: that's `direct_rss_feeds`, separate from indexers. See `src/handlers/settings/direct_rss_feeds.rs`.

## Step 1 — Implement the trait

Create `src/services/indexers/<kind>/mod.rs`. Read `src/services/indexers/mod.rs` for the trait surface — the `Indexer` trait, `Release` / `SearchQuery` / `IndexerCaps` data model.

Required methods:

```rust
#[async_trait]
impl Indexer for FooIndexer {
    fn id(&self) -> i64;
    fn kind(&self) -> &str;             // "foo"
    fn name(&self) -> &str;
    fn priority(&self) -> i32;
    fn download_client_id(&self) -> Option<i64>;  // per-indexer pin

    async fn search(&self, query: &SearchQuery) -> Result<Vec<Release>, String>;
    async fn fetch_caps(&self) -> Result<IndexerCaps, String>;
}
```

### Release identity contract

Snapshot `indexer_priority` and `indexer_name` at search time onto each `Release` — DO NOT read them live. A later DB edit (rename, priority change) shouldn't retroactively rewrite past `Release` records callers may have kept around. The dedup pass attributes each `(infohash, indexer)` pair to the lowest-priority-number indexer based on the snapshot.

### Source-classification degradation

The Nyaa-description-body signal is unavailable for non-Nyaa sources; classification degrades to four layers (filename + ffprobe + temporal + group-map). Ensure your `Release.title` is rich enough that the filename layer can do its job — anitomy parses the title for resolution, group, episode number, etc. If your wire format strips brackets or normalizes the title, the filename layer goes blind.

### Protocol-specific notes (any non-torznab impl)

- **URL opacity**: store the user-pasted base URL verbatim. Don't parse, don't reconstruct. The torznab wire-shape research notes in `src/services/indexers/CLAUDE.md` are the canonical reference for why.
- **Error-as-200**: many indexer protocols return 200 + `<error>` body for failures. Parse the body before trusting the status code.
- **Categories**: the standard torznab anime category is `5070`. If your protocol has its own category space, document the mapping in the impl's docstring + `services/indexers/CLAUDE.md`.
- **Rate limits**: per-indexer rate limits live inside the indexer (not Ryokan). They typically surface as `429 Retry-After`. Currently no process-wide cooldown is applied; every fan-out re-issues regardless of prior 429. If your impl needs a cooldown machine, mirror `services::anilist::rate_limit`'s shape.

## Step 2 — Caps fetcher

`fetch_caps` returns the indexer's reported categories, search modes, and limits. Cached as JSON in `indexers.caps_json` per a 7-day TTL (`CAPS_TTL_SECONDS` in `src/services/indexers/mod.rs`). The Settings UI exposes a "Refresh caps" button for out-of-band edits.

The settings UI renders `categories` as a multi-select on the per-indexer config form. If your protocol doesn't expose categories, return an empty `Vec<CategoryCap>` and add a hint in the form template that the multi-select is disabled.

## Step 3 — Tests

Same wiremock pattern as download clients. Create `src/services/indexers/<kind>/wiremock_tests/`:

```
wiremock_tests/
├── fixture.rs    # MockServer setup + response-shape helpers
├── search.rs     # search() against various query shapes
├── caps.rs       # fetch_caps() round-trip
└── error.rs      # 200-with-error-body, 401, 429+Retry-After, 5xx
```

No `live_smoke` for indexers (no `RYOKAN_<KIND>_E2E` gate exists). Wiremock coverage is the only verification — point it at recorded fixtures from a real Prowlarr/Jackett response if the impl is for a known service.

## Step 4 — Register the kind

Edit `src/services/indexers/mod.rs::rebuild_cache` and add an arm to the kind dispatch:

```rust
"foo" if !row.url.is_empty() => Some(Arc::new(foo::FooIndexer::new(
    row.id, row.name, row.priority, row.url, row.api_key, row.download_client_id,
))),
```

Then update `protocol_for_indexer_kind(kind: &str) -> Option<&'static str>` in `src/services/download_client/mod.rs`:

```rust
"foo" => Some("torrent"),  // or "usenet" if your protocol delivers NZBs
```

This is what routes new-kind grabs to the right per-protocol download client default when no per-indexer pin is set.

## Step 5 — Settings UI

Edit `src/handlers/settings/indexers.rs`:

1. Add the kind to the form's allowed-kind validator.
2. Add a `<option>` to the kind `<select>` in the indexer add/edit template.
3. If your protocol has unique fields (auth shape, custom URL parameters), add per-kind form hints like the SAB API-key labeling hint in `download_clients.rs`.

## Step 6 — DB schema

The `indexers` table is generic. New kinds usually fit without a schema change. If you need a new column (rare), add an idempotent migration in `src/models/migrations/mod.rs`:

```rust
sqlx::query("ALTER TABLE indexers ADD COLUMN foo_specific TEXT NOT NULL DEFAULT ''")
    .execute(&pool)
    .await
    .ok();
```

## Step 7 — Documentation

1. **`src/services/indexers/CLAUDE.md`**: add a section under "Wire shape" describing the new protocol's quirks. Mirror the torznab section's bullet shape: URL opacity, error-as-200 behavior (or whatever your protocol does), categories, rate-limit handling, query-mode mapping.
2. **Root `CLAUDE.md`**: update the Project Overview's "torznab/newznab indexer system" mention if the new protocol is broad enough to deserve a name change. Most additions don't qualify.

## Step 8 — Verify

```bash
cargo fmt --all -- --check
cargo clippy --workspace --all-targets --features test-support -- -D warnings
cargo nextest run --workspace --features test-support
```

Run `code-reviewer` against the diff for convention checks (Result tag prefixes, hardcoded UA on the new HTTP client, log category for indexer-search events).

## Files you'll touch (summary)

```
src/services/indexers/<kind>/mod.rs                  [NEW]
src/services/indexers/<kind>/wiremock_tests/*.rs     [NEW]
src/services/indexers/CLAUDE.md                      [edit — add protocol section]
src/services/indexers/mod.rs                         [edit — rebuild_cache match arm]
src/services/download_client/mod.rs                  [edit — protocol_for_indexer_kind]
src/handlers/settings/indexers.rs                    [edit — kind validator]
templates/settings.html (or partial)                 [edit — kind <select> option, per-kind hints]
src/models/migrations/mod.rs                         [edit — only if new column needed]
CLAUDE.md (root)                                     [edit — Project Overview if applicable]
```

## Reminder: Nyaa stays out-of-band

This is the most important rule and easiest to forget mid-impl. The search pipeline dispatches Nyaa + fans out `Indexer` impls **in parallel** and merges. Don't:

- Refactor Nyaa to implement `Indexer`. The trait was deliberately scoped to NOT include Nyaa-specific shapes (description-body URL, the scraped HTML quirks).
- Add Nyaa-only fields to `Release` (`nyaa_description: Option<String>`).
- Make the merge step "if Nyaa fails, treat as Indexer error" — Nyaa is the protected hot path.

If you find yourself tempted to add Nyaa to `Indexer`, stop and re-read the "Why Nyaa stays out-of-band" section in `services/indexers/CLAUDE.md`.
