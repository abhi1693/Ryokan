//! Release group → source classification map.
//!
//! Backs Layer 3 of the classification pipeline: when Layer 1 (filename
//! parsing) can't determine a source because the release group's naming
//! convention doesn't carry source keywords (SubsPlease, HorribleSubs,
//! VCB-Studio, …), look up the group in this table and emit its known
//! source as evidence.
//!
//! The table is seeded on migration with well-known groups and is also
//! user-editable via the settings UI. User edits set `is_user_edit = 1` so
//! re-running `seed_defaults` doesn't clobber them.

use sqlx::{Row, SqlitePool};

use crate::services::source::Source;

/// A single group → source mapping.
#[derive(Debug, Clone)]
pub struct GroupSourceEntry {
    pub group_name: String,
    pub source: Source,
    pub confidence: f32,
    pub is_user_edit: bool,
    pub notes: String,
}

// ─────────────────────────────────────────────────────────────────────────
// Seed data
// ─────────────────────────────────────────────────────────────────────────

/// Built-in seed mappings applied on every migration.
///
/// Sourced from TRaSH Guides' Sonarr anime custom formats
/// (<https://github.com/TRaSH-Guides/Guides>), specifically the
/// `anime-bd-tier-*`, `anime-web-tier-*`, and `anime-raws` CF lists. A group
/// is only included here if it appears **exclusively** in the BD tiers or
/// **exclusively** in the WEB tiers — groups that show up in both (sam, FLE,
/// LYS1TH3A, LostYears, Arg0, Arid, Vodes, MTBB, Okay-Subs, Foxtrot, Pizza,
/// Reza, SCY, Baws, McBalls, Asakura, Commie, GJM, Chihiro, Dae, …) are
/// source-ambiguous and are left out so Layer 3 falls through to other
/// evidence instead of guessing. Groups from TRaSH's `anime-lq-groups`
/// blocklist (ASW, bonkai77, Trix, …) are intentionally omitted. That list
/// governs scoring, not source classification.
///
/// Confidence is 0.95 across the board: a group's tier ranking reflects
/// encoding quality, not how reliably the source can be inferred. Entries
/// are inserted with `INSERT OR IGNORE`, so user edits (which set
/// `is_user_edit = 1`) are never overwritten by the seed pass.
pub const SEED_DEFAULTS: &[(&str, Source, f32, &str)] = &[
    // ── Legacy BD encoders ────────────────────────────────────────────────
    // Well-known BD-only encoders that predate or sit outside the current
    // TRaSH custom formats but are still widely seeded.
    ("VCB-Studio", Source::BluRay, 0.95, "legacy BD encode specialist"),
    ("Coalgirls", Source::BluRay, 0.95, "legacy BD encoder"),

    // ── TRaSH BD Tier 01 ──────────────────────────────────────────────────
    ("DemiHuman", Source::BluRay, 0.95, "TRaSH BD tier 01"),
    ("Flugel", Source::BluRay, 0.95, "TRaSH BD tier 01"),
    ("Moxie", Source::BluRay, 0.95, "TRaSH BD tier 01"),
    ("NAN0", Source::BluRay, 0.95, "TRaSH BD tier 01"),

    // ── TRaSH BD Tier 02 ──────────────────────────────────────────────────
    ("Aergia", Source::BluRay, 0.95, "TRaSH BD tier 02"),
    ("FateSucks", Source::BluRay, 0.95, "TRaSH BD tier 02"),
    ("hchcsen", Source::BluRay, 0.95, "TRaSH BD tier 02"),
    ("hydes", Source::BluRay, 0.95, "TRaSH BD tier 02"),
    ("JOHNTiTOR", Source::BluRay, 0.95, "TRaSH BD tier 02"),
    ("JySzE", Source::BluRay, 0.95, "TRaSH BD tier 02"),
    ("koala", Source::BluRay, 0.95, "TRaSH BD tier 02"),
    ("Kulot", Source::BluRay, 0.95, "TRaSH BD tier 02"),
    ("Lulu", Source::BluRay, 0.95, "TRaSH BD tier 02"),
    ("Meakes", Source::BluRay, 0.95, "TRaSH BD tier 02"),
    ("Orphan", Source::BluRay, 0.95, "TRaSH BD tier 02"),
    ("PMR", Source::BluRay, 0.95, "TRaSH BD tier 02"),
    ("WAP", Source::BluRay, 0.95, "TRaSH BD tier 02"),
    ("YURI", Source::BluRay, 0.95, "TRaSH BD tier 02"),

    // ── TRaSH BD Tier 03 ──────────────────────────────────────────────────
    ("ARC", Source::BluRay, 0.95, "TRaSH BD tier 03"),
    ("BBT-RMX", Source::BluRay, 0.95, "TRaSH BD tier 03"),
    ("cappybara", Source::BluRay, 0.95, "TRaSH BD tier 03"),
    ("ChucksMux", Source::BluRay, 0.95, "TRaSH BD tier 03"),
    ("CRUCiBLE", Source::BluRay, 0.95, "TRaSH BD tier 03"),
    ("CUNNY", Source::BluRay, 0.95, "TRaSH BD tier 03"),
    ("Cunnysseur", Source::BluRay, 0.95, "TRaSH BD tier 03"),
    ("Doc", Source::BluRay, 0.95, "TRaSH BD tier 03"),
    ("fig", Source::BluRay, 0.95, "TRaSH BD tier 03"),
    ("Headpatter", Source::BluRay, 0.95, "TRaSH BD tier 03"),
    ("Inka-Subs", Source::BluRay, 0.95, "TRaSH BD tier 03"),
    ("LaCroiX", Source::BluRay, 0.95, "TRaSH BD tier 03"),
    ("Legion", Source::BluRay, 0.95, "TRaSH BD tier 03"),
    ("Mehul", Source::BluRay, 0.95, "TRaSH BD tier 03"),
    ("Mysteria", Source::BluRay, 0.95, "TRaSH BD tier 03"),
    ("Netaro", Source::BluRay, 0.95, "TRaSH BD tier 03"),
    ("Noiy", Source::BluRay, 0.95, "TRaSH BD tier 03"),
    ("npz", Source::BluRay, 0.95, "TRaSH BD tier 03"),
    ("NTRX", Source::BluRay, 0.95, "TRaSH BD tier 03"),
    ("P9", Source::BluRay, 0.95, "TRaSH BD tier 03"),
    ("RaiN", Source::BluRay, 0.95, "TRaSH BD tier 03"),
    ("RMX", Source::BluRay, 0.95, "TRaSH BD tier 03"),
    ("RUDY", Source::BluRay, 0.95, "TRaSH BD tier 03"),
    ("Sekkon", Source::BluRay, 0.95, "TRaSH BD tier 03"),
    ("Serendipity", Source::BluRay, 0.95, "TRaSH BD tier 03"),
    // sgt is listed in TRaSH BD tier 03 but has also released WEB on Nyaa,
    // so treat it as source-ambiguous and let other evidence decide.
    ("SubsMix", Source::BluRay, 0.95, "TRaSH BD tier 03"),
    ("uba", Source::BluRay, 0.95, "TRaSH BD tier 03"),

    // ── TRaSH BD Tier 04 ──────────────────────────────────────────────────
    ("ABdex", Source::BluRay, 0.95, "TRaSH BD tier 04"),
    ("Afro", Source::BluRay, 0.95, "TRaSH BD tier 04"),
    ("aRMX", Source::BluRay, 0.95, "TRaSH BD tier 04"),
    ("BiRJU", Source::BluRay, 0.95, "TRaSH BD tier 04"),
    ("BKC", Source::BluRay, 0.95, "TRaSH BD tier 04"),
    ("CBT", Source::BluRay, 0.95, "TRaSH BD tier 04"),
    ("Chimera", Source::BluRay, 0.95, "TRaSH BD tier 04"),
    ("derp", Source::BluRay, 0.95, "TRaSH BD tier 04"),
    ("DIY", Source::BluRay, 0.95, "TRaSH BD tier 04"),
    ("EXP", Source::BluRay, 0.95, "TRaSH BD tier 04"),
    ("grimf", Source::BluRay, 0.95, "TRaSH BD tier 04"),
    ("IK", Source::BluRay, 0.95, "TRaSH BD tier 04"),
    ("Iznjie_Biznjie", Source::BluRay, 0.95, "TRaSH BD tier 04"),
    ("Kaleido-subs", Source::BluRay, 0.95, "TRaSH BD tier 04"),
    ("Kametsu", Source::BluRay, 0.95, "TRaSH BD tier 04"),
    ("Kawatare", Source::BluRay, 0.95, "TRaSH BD tier 04"),
    ("KH", Source::BluRay, 0.95, "TRaSH BD tier 04"),
    ("LazyRemux", Source::BluRay, 0.95, "TRaSH BD tier 04"),
    ("Metal", Source::BluRay, 0.95, "TRaSH BD tier 04"),
    ("MK", Source::BluRay, 0.95, "TRaSH BD tier 04"),
    ("neko-kBaraka", Source::BluRay, 0.95, "TRaSH BD tier 04"),
    ("OZR", Source::BluRay, 0.95, "TRaSH BD tier 04"),
    ("pog42", Source::BluRay, 0.95, "TRaSH BD tier 04"),
    ("Quetzal", Source::BluRay, 0.95, "TRaSH BD tier 04"),
    ("Shimatta", Source::BluRay, 0.95, "TRaSH BD tier 04"),
    ("Smoke", Source::BluRay, 0.95, "TRaSH BD tier 04"),
    ("Spirale", Source::BluRay, 0.95, "TRaSH BD tier 04"),
    ("UDF", Source::BluRay, 0.95, "TRaSH BD tier 04"),
    ("UQW", Source::BluRay, 0.95, "TRaSH BD tier 04"),
    ("Vanilla", Source::BluRay, 0.95, "TRaSH BD tier 04"),
    ("Virtuality", Source::BluRay, 0.95, "TRaSH BD tier 04"),
    ("VULCAN", Source::BluRay, 0.95, "TRaSH BD tier 04"),

    // ── TRaSH BD Tier 05 ──────────────────────────────────────────────────
    ("Animorphs", Source::BluRay, 0.95, "TRaSH BD tier 05"),
    ("AOmundson", Source::BluRay, 0.95, "TRaSH BD tier 05"),
    ("ASC", Source::BluRay, 0.95, "TRaSH BD tier 05"),
    ("B00BA", Source::BluRay, 0.95, "TRaSH BD tier 05"),
    ("Beatrice-Raws", Source::BluRay, 0.95, "TRaSH BD tier 05"),
    ("Cait-Sidhe", Source::BluRay, 0.95, "TRaSH BD tier 05"),
    ("CsS", Source::BluRay, 0.95, "TRaSH BD tier 05"),
    ("CTR", Source::BluRay, 0.95, "TRaSH BD tier 05"),
    ("D4C", Source::BluRay, 0.95, "TRaSH BD tier 05"),
    ("deanzel", Source::BluRay, 0.95, "TRaSH BD tier 05"),
    ("Drag", Source::BluRay, 0.95, "TRaSH BD tier 05"),
    ("eldon", Source::BluRay, 0.95, "TRaSH BD tier 05"),
    ("Freehold", Source::BluRay, 0.95, "TRaSH BD tier 05"),
    ("GHS", Source::BluRay, 0.95, "TRaSH BD tier 05"),
    ("Hark0N", Source::BluRay, 0.95, "TRaSH BD tier 05"),
    ("Holomux", Source::BluRay, 0.95, "TRaSH BD tier 05"),
    ("Judgment", Source::BluRay, 0.95, "TRaSH BD tier 05"),
    ("MC", Source::BluRay, 0.95, "TRaSH BD tier 05"),
    ("mottoj", Source::BluRay, 0.95, "TRaSH BD tier 05"),
    ("NH", Source::BluRay, 0.95, "TRaSH BD tier 05"),
    ("NTRM", Source::BluRay, 0.95, "TRaSH BD tier 05"),
    ("o7", Source::BluRay, 0.95, "TRaSH BD tier 05"),
    ("QM", Source::BluRay, 0.95, "TRaSH BD tier 05"),
    ("Thighs", Source::BluRay, 0.95, "TRaSH BD tier 05"),
    ("TTGA", Source::BluRay, 0.95, "TRaSH BD tier 05"),
    ("UltraRemux", Source::BluRay, 0.95, "TRaSH BD tier 05"),
    ("WBDP", Source::BluRay, 0.95, "TRaSH BD tier 05"),
    ("WSE", Source::BluRay, 0.95, "TRaSH BD tier 05"),
    ("Yuki", Source::BluRay, 0.95, "TRaSH BD tier 05"),

    // ── TRaSH BD Tier 06 ──────────────────────────────────────────────────
    ("ANE", Source::BluRay, 0.95, "TRaSH BD tier 06"),
    ("Bunny-Apocalypse", Source::BluRay, 0.95, "TRaSH BD tier 06"),
    ("CyC", Source::BluRay, 0.95, "TRaSH BD tier 06"),
    ("Datte13", Source::BluRay, 0.95, "TRaSH BD tier 06"),
    ("EJF", Source::BluRay, 0.95, "TRaSH BD tier 06"),
    ("GetItTwisted", Source::BluRay, 0.95, "TRaSH BD tier 06"),
    ("iKaos", Source::BluRay, 0.95, "TRaSH BD tier 06"),
    ("karios", Source::BluRay, 0.95, "TRaSH BD tier 06"),
    ("Pookie", Source::BluRay, 0.95, "TRaSH BD tier 06"),
    ("RASETSU", Source::BluRay, 0.95, "TRaSH BD tier 06"),
    ("Starbez", Source::BluRay, 0.95, "TRaSH BD tier 06"),
    ("Yoghurt", Source::BluRay, 0.95, "TRaSH BD tier 06"),
    ("YURASUKA", Source::BluRay, 0.95, "TRaSH BD tier 06"),

    // ── TRaSH BD Tier 07 ──────────────────────────────────────────────────
    ("AC", Source::BluRay, 0.95, "TRaSH BD tier 07"),
    ("Almighty", Source::BluRay, 0.95, "TRaSH BD tier 07"),
    ("BlurayDesuYo", Source::BluRay, 0.95, "TRaSH BD tier 07"),
    ("Bolshevik", Source::BluRay, 0.95, "TRaSH BD tier 07"),
    ("Brrrrrrr", Source::BluRay, 0.95, "TRaSH BD tier 07"),
    ("Crow", Source::BluRay, 0.95, "TRaSH BD tier 07"),
    ("Dekinai", Source::BluRay, 0.95, "TRaSH BD tier 07"),
    ("Dragon-Releases", Source::BluRay, 0.95, "TRaSH BD tier 07"),
    ("DragsterPS", Source::BluRay, 0.95, "TRaSH BD tier 07"),
    ("E-D", Source::BluRay, 0.95, "TRaSH BD tier 07"),
    ("Exiled-Destiny", Source::BluRay, 0.95, "TRaSH BD tier 07"),
    ("FFF", Source::BluRay, 0.95, "TRaSH BD tier 07"),
    ("Final8", Source::BluRay, 0.95, "TRaSH BD tier 07"),
    ("Geonope", Source::BluRay, 0.95, "TRaSH BD tier 07"),
    ("iAHD", Source::BluRay, 0.95, "TRaSH BD tier 07"),
    ("inid4c", Source::BluRay, 0.95, "TRaSH BD tier 07"),
    ("Koten_Gars", Source::BluRay, 0.95, "TRaSH BD tier 07"),
    ("kuchikirukia", Source::BluRay, 0.95, "TRaSH BD tier 07"),
    ("LCE", Source::BluRay, 0.95, "TRaSH BD tier 07"),
    ("NTW", Source::BluRay, 0.95, "TRaSH BD tier 07"),
    ("orz", Source::BluRay, 0.95, "TRaSH BD tier 07"),
    ("RAI", Source::BluRay, 0.95, "TRaSH BD tier 07"),
    ("REVO", Source::BluRay, 0.95, "TRaSH BD tier 07"),
    ("SCP-2223", Source::BluRay, 0.95, "TRaSH BD tier 07"),
    ("Senjou", Source::BluRay, 0.95, "TRaSH BD tier 07"),
    ("SEV", Source::BluRay, 0.95, "TRaSH BD tier 07"),
    ("THORA", Source::BluRay, 0.95, "TRaSH BD tier 07"),
    ("Vivid", Source::BluRay, 0.95, "TRaSH BD tier 07"),

    // ── TRaSH BD Tier 08 ──────────────────────────────────────────────────
    ("AkihitoSubs", Source::BluRay, 0.95, "TRaSH BD tier 08"),
    ("Arukoru", Source::BluRay, 0.95, "TRaSH BD tier 08"),
    ("EDGE", Source::BluRay, 0.95, "TRaSH BD tier 08"),
    ("EMBER", Source::BluRay, 0.95, "TRaSH BD tier 08"),
    ("GHOST", Source::BluRay, 0.95, "TRaSH BD tier 08"),
    ("Judas", Source::BluRay, 0.95, "TRaSH BD tier 08"),
    ("naiyas", Source::BluRay, 0.95, "TRaSH BD tier 08"),
    ("Nep_Blanc", Source::BluRay, 0.95, "TRaSH BD tier 08"),
    ("Prof", Source::BluRay, 0.95, "TRaSH BD tier 08"),
    ("Shiro", Source::BluRay, 0.95, "TRaSH BD tier 08"),

    // ── TRaSH anime-raws (Japanese BD raw encoders) ───────────────────────
    // BD-only raw encoding specialists from the `anime-raws` CF. Confidence
    // is 0.95 because these groups exclusively work from Blu-ray sources.
    ("Asuka-Raws", Source::BluRay, 0.95, "TRaSH anime raws"),
    ("Daddy-Raws", Source::BluRay, 0.95, "TRaSH anime raws"),
    ("Fumi-Raws", Source::BluRay, 0.95, "TRaSH anime raws"),
    ("Iriza-Raws", Source::BluRay, 0.95, "TRaSH anime raws"),
    ("Kawaiika-Raws", Source::BluRay, 0.95, "TRaSH anime raws"),
    ("Koi-Raws", Source::BluRay, 0.95, "TRaSH anime raws"),
    ("Lilith-Raws", Source::BluRay, 0.95, "TRaSH anime raws"),
    ("LowPower-Raws", Source::BluRay, 0.95, "TRaSH anime raws"),
    ("Moozzi2", Source::BluRay, 0.95, "TRaSH anime raws"),
    ("Nanako-Raws", Source::BluRay, 0.95, "TRaSH anime raws"),
    ("NC-Raws", Source::BluRay, 0.95, "TRaSH anime raws"),
    ("neko-raws", Source::BluRay, 0.95, "TRaSH anime raws"),
    ("New-Raws", Source::BluRay, 0.95, "TRaSH anime raws"),
    ("Ohys-Raws", Source::BluRay, 0.95, "TRaSH anime raws"),
    ("Pandoratv-Raws", Source::BluRay, 0.95, "TRaSH anime raws"),
    ("Raws-Maji", Source::BluRay, 0.95, "TRaSH anime raws"),
    ("ReinForce", Source::BluRay, 0.95, "TRaSH anime raws"),
    ("Scryous-Raws", Source::BluRay, 0.95, "TRaSH anime raws"),
    ("Seicher-Raws", Source::BluRay, 0.95, "TRaSH anime raws"),
    ("Shiniori-Raws", Source::BluRay, 0.95, "TRaSH anime raws"),

    // ── TRaSH WEB Tier 01 ─────────────────────────────────────────────────
    ("Setsugen", Source::Web, 0.95, "TRaSH WEB tier 01"),
    ("Z4ST1N", Source::Web, 0.95, "TRaSH WEB tier 01"),

    // ── TRaSH WEB Tier 02 ─────────────────────────────────────────────────
    ("0x539", Source::Web, 0.95, "TRaSH WEB tier 02"),
    ("Cyan", Source::Web, 0.95, "TRaSH WEB tier 02"),
    ("Cytox", Source::Web, 0.95, "TRaSH WEB tier 02"),
    ("Gao", Source::Web, 0.95, "TRaSH WEB tier 02"),
    ("Half-Baked", Source::Web, 0.95, "TRaSH WEB tier 02"),
    ("HatSubs", Source::Web, 0.95, "TRaSH WEB tier 02"),
    ("MALD", Source::Web, 0.95, "TRaSH WEB tier 02"),
    ("Not-Vodes", Source::Web, 0.95, "TRaSH WEB tier 02"),
    ("Slyfox", Source::Web, 0.95, "TRaSH WEB tier 02"),
    ("SoLCE", Source::Web, 0.95, "TRaSH WEB tier 02"),
    ("tenshi", Source::Web, 0.95, "TRaSH WEB tier 02"),

    // ── TRaSH WEB Tier 03 ─────────────────────────────────────────────────
    ("AnoZu", Source::Web, 0.95, "TRaSH WEB tier 03"),
    ("Dooky", Source::Web, 0.95, "TRaSH WEB tier 03"),
    ("Kitsune", Source::Web, 0.95, "TRaSH WEB tier 03"),
    ("SubsPlus+", Source::Web, 0.95, "TRaSH WEB tier 03"),

    // ── TRaSH WEB Tier 04 ─────────────────────────────────────────────────
    ("Erai-raws", Source::Web, 0.95, "TRaSH WEB tier 04"),
    ("ToonsHub", Source::Web, 0.95, "TRaSH WEB tier 04"),
    ("VARYG", Source::Web, 0.95, "TRaSH WEB tier 04"),

    // ── TRaSH WEB Tier 05 ─────────────────────────────────────────────────
    ("BlueLobster", Source::Web, 0.95, "TRaSH WEB tier 05"),
    ("GST", Source::Web, 0.95, "TRaSH WEB tier 05"),
    ("HorribleRips", Source::Web, 0.95, "TRaSH WEB tier 05"),
    ("HorribleSubs", Source::Web, 0.95, "TRaSH WEB tier 05"),
    ("KAN3D2M", Source::Web, 0.95, "TRaSH WEB tier 05"),
    ("KiyoshiStar", Source::Web, 0.95, "TRaSH WEB tier 05"),
    ("Lia", Source::Web, 0.95, "TRaSH WEB tier 05"),
    ("NanDesuKa", Source::Web, 0.95, "TRaSH WEB tier 05"),
    ("PlayWeb", Source::Web, 0.95, "TRaSH WEB tier 05"),
    ("SobsPlease", Source::Web, 0.95, "TRaSH WEB tier 05"),
    ("Some-Stuffs", Source::Web, 0.95, "TRaSH WEB tier 05"),
    ("SubsPlease", Source::Web, 0.95, "TRaSH WEB tier 05"),
    ("URANIME", Source::Web, 0.95, "TRaSH WEB tier 05"),
    ("ZigZag", Source::Web, 0.95, "TRaSH WEB tier 05"),

    // ── TRaSH WEB Tier 06 ─────────────────────────────────────────────────
    ("DameDesuYo", Source::Web, 0.95, "TRaSH WEB tier 06"),
    ("Doki", Source::Web, 0.95, "TRaSH WEB tier 06"),
    ("Kaleido", Source::Web, 0.95, "TRaSH WEB tier 06"),
    ("Kantai", Source::Web, 0.95, "TRaSH WEB tier 06"),
    ("KawaSubs", Source::Web, 0.95, "TRaSH WEB tier 06"),
];

// ─────────────────────────────────────────────────────────────────────────
// Migration + seeding
// ─────────────────────────────────────────────────────────────────────────

/// Create the `group_source_map` table if it doesn't exist, then refresh the
/// seed defaults. Idempotent — safe to call on every startup.
///
/// `COLLATE NOCASE` on the primary key lets lookups be case-insensitive
/// without us having to normalize strings at every call site.
///
/// Non-user rows (`is_user_edit = 0`) are cleared before re-seeding so
/// [`SEED_DEFAULTS`] stays the single source of truth — groups we remove or
/// re-classify in code automatically propagate on the next startup. User
/// edits made via the settings UI are untouched.
pub async fn migrate(db: &SqlitePool) -> Result<(), sqlx::Error> {
    sqlx::query(
        r#"
        CREATE TABLE IF NOT EXISTS group_source_map (
            group_name    TEXT PRIMARY KEY COLLATE NOCASE,
            source        TEXT NOT NULL,
            confidence    REAL NOT NULL,
            is_user_edit  INTEGER NOT NULL DEFAULT 0,
            notes         TEXT NOT NULL DEFAULT '',
            updated_at    DATETIME DEFAULT CURRENT_TIMESTAMP
        )
        "#,
    )
    .execute(db)
    .await?;

    sqlx::query("DELETE FROM group_source_map WHERE is_user_edit = 0")
        .execute(db)
        .await?;

    seed_defaults(db).await?;

    Ok(())
}

/// Insert any missing built-in seed rows. Existing rows (including
/// user-edited ones) are left untouched thanks to `INSERT OR IGNORE`.
pub async fn seed_defaults(db: &SqlitePool) -> Result<(), sqlx::Error> {
    for (name, source, confidence, notes) in SEED_DEFAULTS {
        sqlx::query(
            "INSERT OR IGNORE INTO group_source_map (group_name, source, confidence, is_user_edit, notes)
             VALUES (?, ?, ?, 0, ?)",
        )
        .bind(name)
        .bind(source.as_str())
        .bind(*confidence)
        .bind(notes)
        .execute(db)
        .await?;
    }
    Ok(())
}

// ─────────────────────────────────────────────────────────────────────────
// CRUD
// ─────────────────────────────────────────────────────────────────────────

/// Look up a single group by name. Case-insensitive thanks to the table's
/// `COLLATE NOCASE` primary key.
pub async fn get(
    db: &SqlitePool,
    group_name: &str,
) -> Result<Option<GroupSourceEntry>, sqlx::Error> {
    let row = sqlx::query(
        "SELECT group_name, source, confidence, is_user_edit, notes
         FROM group_source_map WHERE group_name = ?",
    )
    .bind(group_name)
    .fetch_optional(db)
    .await?;

    Ok(row.map(row_to_entry))
}

/// List all entries, alphabetically sorted.
pub async fn list_all(db: &SqlitePool) -> Result<Vec<GroupSourceEntry>, sqlx::Error> {
    let rows = sqlx::query(
        "SELECT group_name, source, confidence, is_user_edit, notes
         FROM group_source_map ORDER BY group_name COLLATE NOCASE ASC",
    )
    .fetch_all(db)
    .await?;

    Ok(rows.into_iter().map(row_to_entry).collect())
}

/// Insert or update a user-edited entry. Always marks the row as
/// `is_user_edit = 1` so subsequent `seed_defaults` calls won't revert it.
pub async fn upsert_user_edit(
    db: &SqlitePool,
    group_name: &str,
    source: Source,
    confidence: f32,
    notes: &str,
) -> Result<(), sqlx::Error> {
    sqlx::query(
        "INSERT INTO group_source_map (group_name, source, confidence, is_user_edit, notes, updated_at)
         VALUES (?, ?, ?, 1, ?, CURRENT_TIMESTAMP)
         ON CONFLICT(group_name) DO UPDATE SET
             source = excluded.source,
             confidence = excluded.confidence,
             is_user_edit = 1,
             notes = excluded.notes,
             updated_at = CURRENT_TIMESTAMP",
    )
    .bind(group_name)
    .bind(source.as_str())
    .bind(confidence.clamp(0.0, 1.0))
    .bind(notes)
    .execute(db)
    .await?;
    Ok(())
}

/// Delete an entry by group name.
pub async fn delete(db: &SqlitePool, group_name: &str) -> Result<(), sqlx::Error> {
    sqlx::query("DELETE FROM group_source_map WHERE group_name = ?")
        .bind(group_name)
        .execute(db)
        .await?;
    Ok(())
}

fn row_to_entry(row: sqlx::sqlite::SqliteRow) -> GroupSourceEntry {
    let source_str: String = row.get("source");
    let is_user_edit_int: i64 = row.get("is_user_edit");
    GroupSourceEntry {
        group_name: row.get("group_name"),
        source: Source::from_str(&source_str),
        confidence: row.get::<f64, _>("confidence") as f32,
        is_user_edit: is_user_edit_int != 0,
        notes: row.get("notes"),
    }
}
