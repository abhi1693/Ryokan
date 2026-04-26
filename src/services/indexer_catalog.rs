//! Seeded indexer catalog for the Settings → Indexers picker
//! (issue #28 PR D follow-up).
//!
//! Sonarr / autobrr both ship a curated indexer list so users
//! pick from named cards instead of typing into a blank form.
//! Ryokan does the same, but every indexer reaches Ryokan
//! through Prowlarr or Jackett (Ryokan doesn't speak any
//! tracker's native API directly), so the catalog entries
//! pre-fill *defaults* — the row's display name, the private-
//! tracker flag, sensible priority + min-seeders — and leave
//! the URL + API key for the user to paste from their Prowlarr
//! / Jackett instance. Per-tracker seed ratio / seed time stay
//! blank by default because the right value depends on each
//! tracker's specific HnR / ratio policy and the user's
//! buffer goals; baking a number in would be wrong as often
//! as it's right.
//!
//! The grid sits above the form on the Add path; clicking a
//! card sends `?tab=indexers&template=<slug>` and the handler
//! re-renders the form with `IndexerSeed` populated from the
//! matched entry. Generic Torznab / Generic Newznab serve as
//! the fall-throughs for anything not in the curated list.

/// One curated indexer template. The grid renders a card per
/// entry; the form pre-fills from whichever entry the user
/// clicked.
pub struct SeededIndexer {
    /// URL-safe identifier, used as the `template=<slug>` query
    /// param when the user picks this card. Must be unique.
    pub slug: &'static str,
    /// Card heading + default `name` field on the form.
    pub display_name: &'static str,
    /// One-line description shown under the heading on the
    /// card. Generic per-category text — keep it short enough
    /// to fit on one line at the picker grid's narrowest
    /// breakpoint.
    pub blurb: &'static str,
    /// Optional notes shown above the form once the user picks
    /// this template. Reserved for *real* gotchas (e.g. when
    /// two cards point at the same tracker via different
    /// protocols and the user needs to pick the right side).
    /// Empty string renders no panel — that's the default.
    pub notes: &'static str,
    /// Marks the indexer as a private tracker, which flips the
    /// per-series upgrade-opt-in default and may affect future
    /// seed-rule defaults.
    pub is_private_tracker: bool,
    /// Sonarr-convention priority floor. Lower = preferred.
    /// Range 1..=50, default 25.
    pub default_priority: i64,
    /// Floor for releases the indexer is allowed to surface;
    /// scoring runs only on releases above this seeder count.
    pub default_min_seeders: i64,
    /// Suggested seed ratio passed to the download client at
    /// add time. `None` is the right answer for almost every
    /// tracker — site rules vary widely and the user knows
    /// their own buffer goals better than the catalog does.
    pub default_seed_ratio: Option<f64>,
    /// Suggested seed time floor in minutes. Same `None`-by-
    /// default reasoning as `default_seed_ratio`.
    pub default_seed_time_minutes: Option<i64>,
    /// Default `kind` for the indexer (`torznab` or `newznab`).
    pub default_kind: &'static str,
    /// Hint shown in the URL field when the user picks this
    /// card. Should look like a real Prowlarr / Jackett URL so
    /// the user can pattern-match their own.
    pub url_placeholder: &'static str,
    /// `true` for the catch-all entries at the bottom of the
    /// grid (Generic Torznab / Generic Newznab). Renders with
    /// a different visual treatment so the picker doesn't
    /// suggest the user always belongs in the curated list.
    pub is_generic: bool,
}

/// Curated seed list. Order is the render order on the picker
/// grid; the two `is_generic` entries always go last per the
/// Sonarr convention.
pub const SEEDED: &[SeededIndexer] = &[
    SeededIndexer {
        slug: "animebytes",
        display_name: "AnimeBytes",
        blurb: "Anime private tracker",
        notes: "",
        is_private_tracker: true,
        default_priority: 15,
        default_min_seeders: 1,
        default_seed_ratio: None,
        default_seed_time_minutes: None,
        default_kind: "torznab",
        url_placeholder: "https://prowlarr.local/{N}/api",
        is_generic: false,
    },
    SeededIndexer {
        slug: "bakabt",
        display_name: "BakaBT",
        blurb: "Anime private tracker",
        notes: "",
        is_private_tracker: true,
        default_priority: 20,
        default_min_seeders: 1,
        default_seed_ratio: None,
        default_seed_time_minutes: None,
        default_kind: "torznab",
        url_placeholder: "https://prowlarr.local/{N}/api",
        is_generic: false,
    },
    SeededIndexer {
        slug: "u2",
        display_name: "U2",
        blurb: "Anime private tracker",
        notes: "",
        is_private_tracker: true,
        default_priority: 25,
        default_min_seeders: 1,
        default_seed_ratio: None,
        default_seed_time_minutes: None,
        default_kind: "torznab",
        url_placeholder: "https://prowlarr.local/{N}/api",
        is_generic: false,
    },
    SeededIndexer {
        slug: "nekobt",
        display_name: "nekoBT",
        blurb: "Public anime tracker",
        notes: "",
        is_private_tracker: false,
        default_priority: 30,
        default_min_seeders: 2,
        default_seed_ratio: None,
        default_seed_time_minutes: None,
        default_kind: "torznab",
        url_placeholder: "https://prowlarr.local/{N}/api",
        is_generic: false,
    },
    SeededIndexer {
        slug: "animetosho",
        display_name: "AnimeTosho (Torznab)",
        blurb: "Public anime indexer",
        notes: "",
        is_private_tracker: false,
        default_priority: 35,
        default_min_seeders: 2,
        default_seed_ratio: None,
        default_seed_time_minutes: None,
        default_kind: "torznab",
        url_placeholder: "https://prowlarr.local/{N}/api",
        is_generic: false,
    },
    SeededIndexer {
        slug: "animetosho-newznab",
        display_name: "AnimeTosho (Newznab)",
        blurb: "Public anime indexer (Usenet mirror)",
        notes: "",
        is_private_tracker: false,
        default_priority: 35,
        default_min_seeders: 0,
        default_seed_ratio: None,
        default_seed_time_minutes: None,
        default_kind: "newznab",
        url_placeholder: "https://feed.animetosho.org/api",
        is_generic: false,
    },
    SeededIndexer {
        slug: "tokyotosho",
        display_name: "Tokyo Toshokan",
        blurb: "Public anime tracker",
        notes: "",
        is_private_tracker: false,
        default_priority: 40,
        default_min_seeders: 2,
        default_seed_ratio: None,
        default_seed_time_minutes: None,
        default_kind: "torznab",
        url_placeholder: "https://prowlarr.local/{N}/api",
        is_generic: false,
    },
    SeededIndexer {
        slug: "anidex",
        display_name: "AniDex",
        blurb: "Public anime tracker",
        notes: "",
        is_private_tracker: false,
        default_priority: 35,
        default_min_seeders: 2,
        default_seed_ratio: None,
        default_seed_time_minutes: None,
        default_kind: "torznab",
        url_placeholder: "https://prowlarr.local/{N}/api",
        is_generic: false,
    },
    SeededIndexer {
        slug: "generic-torznab",
        display_name: "Generic Torznab",
        blurb: "Any torznab-compatible indexer",
        notes: "",
        is_private_tracker: false,
        default_priority: 25,
        default_min_seeders: 1,
        default_seed_ratio: None,
        default_seed_time_minutes: None,
        default_kind: "torznab",
        url_placeholder: "https://prowlarr.local/{N}/api",
        is_generic: true,
    },
    SeededIndexer {
        slug: "generic-newznab",
        display_name: "Generic Newznab",
        blurb: "Any newznab-compatible Usenet indexer",
        notes: "",
        is_private_tracker: false,
        default_priority: 25,
        default_min_seeders: 0,
        default_seed_ratio: None,
        default_seed_time_minutes: None,
        default_kind: "newznab",
        url_placeholder: "https://nzb.indexer.example/api",
        is_generic: true,
    },
];

/// Look up a seed by slug. `None` for unknown slugs; the
/// caller treats that the same as no template selected.
pub fn find_seed(slug: &str) -> Option<&'static SeededIndexer> {
    SEEDED.iter().find(|s| s.slug == slug)
}
