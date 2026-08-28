# Nyaa filename corpus

Real-world video filenames scraped from Nyaa torrent file lists on
2026-08-28, one JSON per torrent, used by `tests/nyaa_filename_corpus.rs`
as a regression corpus for `services::media::parse_episode_number`.

Each fixture records the torrent's category bucket, release title, source
URL, and every contained video filename alongside the `(season, episode)`
the parser returned at capture time (`null` episode = parser returned
`None`). Fixtures titled "Hand-supplied" were contributed directly rather
than scraped and carry `url: null`; the [SoM] DBox and Bleach entries
specifically exercise the dot-delimited episode branch and its codec-token
guard (PR #198).

**These are characterization snapshots, not ground truth.** Some captured
expectations are known mis-parses of extras (e.g. `Part 2 - SP` parsing as
episode 2, `07_5` recap parsing as episode 7) and NC/PV/CM files
deliberately parse to `None`. The test pins today's behavior so any parser
change that shifts a real-world outcome shows up explicitly in review;
updating an expectation alongside a deliberate parser fix is normal and
expected.

Category buckets:

- `seasonal-single` — modern per-episode releases (SubsPlease / Erai-raws).
- `batch-modern` — modern season batch packs.
- `bd-pack-extras` — BD packs shipping NCOP/NCED/SP/PV/CM extras (Moozzi2).
- `dvd-underscore` — underscore-delimited old-school DVD rips (Exiled-Destiny).
- `dvd-dot` — DVD complete-series era releases.
- `movie` — movie releases and a movies+OVA+seasons ultimate collection.
- `numeric-title` — numeric series titles (86 Eighty-Six).
- `webdl-dotted-codec` — hand-supplied WEB-DL scene names carrying dotted
  `AAC2.0.H.264` codec tokens (regression guard for dot-token episode
  parsing: `SxxEyy` must keep outranking any bare dot-delimited number).

Regenerate by re-scraping (throttle >= 1s between requests; Nyaa tarpits
scrape-looking bursts) and re-running the parser over the collected names.
