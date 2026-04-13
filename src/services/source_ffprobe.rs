//! Layer 5 — ffprobe container / stream analysis.
//!
//! The strongest single post-download signal. Shells out to `ffprobe` with
//! `-show_streams -show_format -show_chapters -of json`, then walks the
//! parsed JSON looking for fingerprints that only appear in one source
//! type. A file with FLAC audio and PGS subtitles is, in practice,
//! always a BluRay rip; a file with AAC as its only audio track is
//! overwhelmingly a Web release; native DVD dimensions (720×480, 704×480,
//! 720×576) are diagnostic of a DVDRip.
//!
//! | Signal                                                  | Confidence |
//! |---------------------------------------------------------|------------|
//! | FLAC / TrueHD / DTS-HD MA / PCM audio                   | 0.90       |
//! | PGS (S_HDMV/PGS) subtitle track                         | 0.90       |
//! | Native DVD dimensions (720×480 / 704×480 / 720×576)     | 0.90       |
//! | Commentary audio track (detected via stream title)      | 0.90       |
//! | HEVC + FLAC + 10-bit fingerprint                        | 0.90       |
//! | AAC as the sole audio codec                             | 0.85       |
//! | H.264 + AAC + 8-bit fingerprint                         | 0.85       |
//! | E-AC-3 / DD+ audio                                      | 0.80       |
//!
//! **Intentionally useless signals** (no evidence produced): Opus, AV1,
//! x265/HEVC alone, AC-3 alone, stream track count. These appear in
//! both BluRay and Web releases and would only add noise.
//!
//! The module exposes a pure `scan_ffprobe_json` for unit testability and
//! an async `classify_ffprobe` wrapper that owns the ffprobe shell-out
//! plus `(path, mtime, size)`-keyed caching. Missing `ffprobe` binary,
//! malformed JSON, or probe failures return an empty evidence vec — the
//! aggregator will simply fall back on the other layers.
//!
//! This module does NOT fold evidence into a final decision. It emits
//! a bag of [`SourceEvidence`] plus an observed [`Resolution`] (if any)
//! for the caller to hand to [`crate::services::source::aggregate`].

use std::path::Path;
use std::process::Stdio;
use std::time::SystemTime;

use serde_json::Value;
use sqlx::SqlitePool;
use tokio::process::Command;

use crate::models::media_probe_cache;
use crate::services::source::{Resolution, Source, SourceEvidence};

const ORIGIN: &str = "ffprobe";

/// Public output of Layer 5.
#[derive(Debug, Clone, Default)]
pub struct FfprobeClassification {
    /// Zero or more pieces of source evidence extracted from the probe JSON.
    pub evidence: Vec<SourceEvidence>,
    /// Observed display resolution if the probe output included a video
    /// stream with usable dimensions. Takes precedence over filename-parsed
    /// resolution at the orchestrator level since it's a direct observation.
    pub resolution: Option<Resolution>,
}

/// Run ffprobe against `path` (or return a cached result), parse the JSON,
/// and emit Layer 5 evidence. Returns an empty classification on any error —
/// missing binary, missing file, cache miss that then fails to spawn, probe
/// timeout, malformed JSON — so the caller can always safely aggregate the
/// result without null-checking.
pub async fn classify_ffprobe(db: &SqlitePool, path: &Path) -> FfprobeClassification {
    let Some(path_str) = path.to_str() else {
        return FfprobeClassification::default();
    };

    // Snapshot mtime + size to key the cache. An rmdir-then-recreate (same
    // path, new file) invalidates on either mtime or size. If stat fails,
    // treat it as "can't probe" rather than blindly going to the network.
    let meta = match std::fs::metadata(path) {
        Ok(m) => m,
        Err(_) => return FfprobeClassification::default(),
    };
    let size = meta.len() as i64;
    let mtime = meta
        .modified()
        .ok()
        .and_then(|m| m.duration_since(SystemTime::UNIX_EPOCH).ok())
        .map(|d| d.as_secs() as i64)
        .unwrap_or(0);

    let cached = media_probe_cache::get(db, path_str, mtime, size).await;
    let probe_json = match cached {
        Some(j) => j,
        None => {
            let Some(j) = run_ffprobe(path).await else {
                return FfprobeClassification::default();
            };
            media_probe_cache::upsert(db, path_str, mtime, size, &j).await;
            j
        }
    };

    scan_ffprobe_json(&probe_json)
}

/// Spawn `ffprobe` and capture its JSON output. Returns `None` on any
/// failure including a missing binary. We pass `-v quiet` to suppress
/// ffprobe's banner so cache hits compare byte-for-byte.
async fn run_ffprobe(path: &Path) -> Option<String> {
    let output = Command::new("ffprobe")
        .arg("-v")
        .arg("quiet")
        .arg("-show_streams")
        .arg("-show_format")
        .arg("-show_chapters")
        .arg("-of")
        .arg("json")
        .arg(path)
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::null())
        .output()
        .await
        .ok()?;
    if !output.status.success() {
        return None;
    }
    String::from_utf8(output.stdout).ok()
}

/// Pure scanner: takes a ffprobe JSON document as a string and emits
/// classification evidence. Kept free of I/O and fs access so the unit
/// tests can feed in canned fixtures with no shell-out.
pub fn scan_ffprobe_json(json: &str) -> FfprobeClassification {
    let Ok(root) = serde_json::from_str::<Value>(json) else {
        return FfprobeClassification::default();
    };
    let Some(streams) = root.get("streams").and_then(|s| s.as_array()) else {
        return FfprobeClassification::default();
    };

    // First pass: collect the facts we need for the rule evaluation.
    let mut facts = ProbeFacts::default();
    for s in streams {
        let codec_type = s
            .get("codec_type")
            .and_then(|v| v.as_str())
            .unwrap_or("")
            .to_ascii_lowercase();
        let codec_name = s
            .get("codec_name")
            .and_then(|v| v.as_str())
            .unwrap_or("")
            .to_ascii_lowercase();

        match codec_type.as_str() {
            "video" => {
                facts.has_video = true;
                facts.video_codec = codec_name.clone();
                facts.width = s.get("width").and_then(|v| v.as_u64()).unwrap_or(0) as u32;
                facts.height = s.get("height").and_then(|v| v.as_u64()).unwrap_or(0) as u32;
                // Bit depth comes from pix_fmt (yuv420p10le → 10-bit, etc.)
                // or bits_per_raw_sample when present.
                let pix_fmt = s
                    .get("pix_fmt")
                    .and_then(|v| v.as_str())
                    .unwrap_or("")
                    .to_ascii_lowercase();
                if pix_fmt.contains("10") {
                    facts.bit_depth = 10;
                } else if pix_fmt.contains("12") {
                    facts.bit_depth = 12;
                } else if !pix_fmt.is_empty() {
                    facts.bit_depth = 8;
                }
                if let Some(bps) = s.get("bits_per_raw_sample").and_then(|v| v.as_str()) {
                    if let Ok(n) = bps.parse::<u8>() {
                        if n > 0 {
                            facts.bit_depth = n;
                        }
                    }
                }
            }
            "audio" => {
                facts.audio_codecs.push(codec_name.clone());
                // Detect commentary tracks via stream title metadata. Title
                // typically lives under tags.title.
                let title = s
                    .get("tags")
                    .and_then(|t| t.get("title"))
                    .and_then(|v| v.as_str())
                    .unwrap_or("")
                    .to_ascii_lowercase();
                if !title.is_empty()
                    && (title.contains("commentary")
                        || title.contains("audio commentary")
                        || title.contains("director"))
                {
                    facts.has_commentary = true;
                }
            }
            "subtitle" => {
                // PGS (Blu-ray) subtitles come through as "hdmv_pgs_subtitle"
                // under codec_name. Also accept the older S_HDMV/PGS form that
                // shows up in some mkvtoolnix-produced files.
                if codec_name.contains("pgs") || codec_name.contains("hdmv_pgs") {
                    facts.has_pgs_subs = true;
                }
            }
            _ => {}
        }
    }

    let mut out = FfprobeClassification::default();
    if !facts.has_video {
        return out;
    }

    // Observed resolution — always set if we have a video stream with
    // readable dimensions, even when no source evidence fires.
    if facts.width > 0 && facts.height > 0 {
        let res = Resolution::from_dimensions(facts.width, facts.height);
        if res != Resolution::Unknown {
            out.resolution = Some(res);
        }
    }

    // Rule: DVD-native dimensions. 720×480, 704×480 (NTSC), 720×576 (PAL).
    // This is diagnostic regardless of codec — any file with these exact
    // dimensions is a DVD rip.
    if matches!(
        (facts.width, facts.height),
        (720, 480) | (704, 480) | (720, 576)
    ) {
        out.evidence.push(SourceEvidence::new(
            Source::Dvd,
            0.90,
            ORIGIN,
            format!("native DVD dimensions {}x{}", facts.width, facts.height),
        ));
    }

    // Rule: PGS subtitles → BluRay. PGS is the bitmap subtitle format used
    // on retail Blu-ray discs. Web releases never ship PGS.
    if facts.has_pgs_subs {
        out.evidence.push(SourceEvidence::new(
            Source::BluRay,
            0.90,
            ORIGIN,
            "PGS subtitle track present",
        ));
    }

    // Rule: commentary track → BluRay. Only retail BD releases ship
    // director / cast commentary tracks.
    if facts.has_commentary {
        out.evidence.push(SourceEvidence::new(
            Source::BluRay,
            0.90,
            ORIGIN,
            "commentary audio track",
        ));
    }

    // Rule: high-fidelity audio codecs → BluRay. FLAC, TrueHD, DTS-HD MA,
    // and PCM are the four lossless / master-audio formats that streaming
    // services don't ship.
    let has_flac = facts.audio_codecs.iter().any(|c| c == "flac");
    let has_truehd = facts.audio_codecs.iter().any(|c| c == "truehd");
    // ffprobe reports DTS-HD MA as `dts_hd_ma` and DTS-HD HRA as
    // `dts_hd_hra`. Match those codec IDs explicitly rather than doing a
    // loose "contains dts && contains hd" substring scan, which could
    // false-positive on e.g. a hypothetical "dts_hdcam" codec and misses
    // the point of being a strict BD-exclusive signal.
    let has_dts_hd = facts
        .audio_codecs
        .iter()
        .any(|c| c == "dts_hd_ma" || c == "dts_hd_hra");
    // PCM on ffprobe comes through as e.g. "pcm_s16le" / "pcm_s24le".
    let has_pcm = facts.audio_codecs.iter().any(|c| c.starts_with("pcm_"));
    if has_flac || has_truehd || has_dts_hd || has_pcm {
        let codec = if has_flac {
            "FLAC"
        } else if has_truehd {
            "TrueHD"
        } else if has_dts_hd {
            "DTS-HD MA"
        } else {
            "PCM"
        };
        out.evidence.push(SourceEvidence::new(
            Source::BluRay,
            0.90,
            ORIGIN,
            format!("{} audio codec", codec),
        ));
    }

    // Rule: E-AC-3 / DD+ → Web. Streaming services (Amazon, Netflix,
    // Disney+) have standardized on DDP for their HD releases. Not
    // diagnostic on its own — but good enough to lean Web.
    let has_ddp = facts
        .audio_codecs
        .iter()
        .any(|c| c == "eac3" || c == "ec-3" || c == "e-ac-3");
    if has_ddp {
        out.evidence.push(SourceEvidence::new(
            Source::Web,
            0.80,
            ORIGIN,
            "E-AC-3 / DD+ audio codec",
        ));
    }

    // Rule: AAC as the sole audio codec → Web. AAC is uncommon on BD
    // releases — BDs ship lossless. An AAC-only file is almost always a
    // streaming rip. The check is "contains aac and no high-fidelity
    // codec" rather than "only aac" because the streamable-set vs.
    // BluRay distinction is the whole point.
    let has_aac = facts.audio_codecs.iter().any(|c| c == "aac");
    if has_aac && !has_flac && !has_truehd && !has_dts_hd && !has_pcm {
        out.evidence.push(SourceEvidence::new(
            Source::Web,
            0.85,
            ORIGIN,
            "AAC audio without lossless track",
        ));
    }

    // Combo fingerprint: H.264 + AAC + 8-bit → Web encode. Catches the
    // typical streaming profile so files that would otherwise pull only
    // the weaker AAC rule get a dedicated high-confidence signal.
    let is_h264 = facts.video_codec == "h264" || facts.video_codec == "avc1";
    if is_h264 && has_aac && facts.bit_depth == 8 && !has_flac && !has_truehd && !has_dts_hd {
        out.evidence.push(SourceEvidence::new(
            Source::Web,
            0.85,
            ORIGIN,
            "H.264 + AAC + 8-bit streaming fingerprint",
        ));
    }

    // Combo fingerprint: HEVC + FLAC + 10-bit → BluRay encode. This is
    // the signature of community BD re-encodes (VCB, Beatrice-Raws, etc.).
    let is_hevc = facts.video_codec == "hevc" || facts.video_codec == "h265";
    if is_hevc && has_flac && facts.bit_depth == 10 {
        out.evidence.push(SourceEvidence::new(
            Source::BluRay,
            0.90,
            ORIGIN,
            "HEVC + FLAC + 10-bit BD encode fingerprint",
        ));
    }

    out
}

#[derive(Default)]
struct ProbeFacts {
    has_video: bool,
    video_codec: String,
    width: u32,
    height: u32,
    bit_depth: u8,
    audio_codecs: Vec<String>,
    has_pgs_subs: bool,
    has_commentary: bool,
}

// ───────────────────────────────────────────────────────────────────────────
// Tests
// ───────────────────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;

    /// Minimal ffprobe-shaped JSON builder for tests. Composes a root with
    /// a single video stream and an arbitrary list of audio/subtitle streams.
    fn probe_json(streams: Vec<Value>) -> String {
        serde_json::to_string(&serde_json::json!({
            "streams": streams,
            "format": {},
        }))
        .unwrap()
    }

    fn video_stream(codec: &str, width: u32, height: u32, pix_fmt: &str) -> Value {
        serde_json::json!({
            "codec_type": "video",
            "codec_name": codec,
            "width": width,
            "height": height,
            "pix_fmt": pix_fmt,
        })
    }

    fn audio_stream(codec: &str) -> Value {
        serde_json::json!({
            "codec_type": "audio",
            "codec_name": codec,
        })
    }

    fn audio_stream_with_title(codec: &str, title: &str) -> Value {
        serde_json::json!({
            "codec_type": "audio",
            "codec_name": codec,
            "tags": { "title": title },
        })
    }

    fn subtitle_stream(codec: &str) -> Value {
        serde_json::json!({
            "codec_type": "subtitle",
            "codec_name": codec,
        })
    }

    #[test]
    fn malformed_json_is_empty() {
        let out = scan_ffprobe_json("not json");
        assert!(out.evidence.is_empty());
        assert!(out.resolution.is_none());
    }

    #[test]
    fn empty_streams_is_empty() {
        let out = scan_ffprobe_json(&probe_json(vec![]));
        assert!(out.evidence.is_empty());
        assert!(out.resolution.is_none());
    }

    #[test]
    fn video_only_sets_resolution() {
        let out = scan_ffprobe_json(&probe_json(vec![video_stream(
            "h264", 1920, 1080, "yuv420p",
        )]));
        assert_eq!(out.resolution, Some(Resolution::R1080p));
    }

    #[test]
    fn dvd_dimensions_fire_dvd_rule() {
        let out = scan_ffprobe_json(&probe_json(vec![video_stream(
            "mpeg2video",
            720,
            480,
            "yuv420p",
        )]));
        assert!(out.evidence.iter().any(|e| e.source == Source::Dvd));
        assert_eq!(out.resolution, Some(Resolution::R480p));
    }

    #[test]
    fn pal_dvd_dimensions_also_fire() {
        let out = scan_ffprobe_json(&probe_json(vec![video_stream(
            "mpeg2video",
            720,
            576,
            "yuv420p",
        )]));
        assert!(out.evidence.iter().any(|e| e.source == Source::Dvd));
    }

    #[test]
    fn flac_audio_is_bluray() {
        let out = scan_ffprobe_json(&probe_json(vec![
            video_stream("hevc", 1920, 1080, "yuv420p10le"),
            audio_stream("flac"),
        ]));
        assert!(out.evidence.iter().any(|e| e.source == Source::BluRay));
    }

    #[test]
    fn hevc_flac_10bit_combo_fires_bd_encode_rule() {
        let out = scan_ffprobe_json(&probe_json(vec![
            video_stream("hevc", 1920, 1080, "yuv420p10le"),
            audio_stream("flac"),
        ]));
        // Should have both the FLAC rule AND the combo-fingerprint rule.
        let bd_count = out
            .evidence
            .iter()
            .filter(|e| e.source == Source::BluRay)
            .count();
        assert!(bd_count >= 2);
    }

    #[test]
    fn truehd_audio_is_bluray() {
        let out = scan_ffprobe_json(&probe_json(vec![
            video_stream("hevc", 1920, 1080, "yuv420p"),
            audio_stream("truehd"),
        ]));
        assert!(out.evidence.iter().any(|e| e.source == Source::BluRay));
    }

    #[test]
    fn dts_hd_ma_is_bluray() {
        let out = scan_ffprobe_json(&probe_json(vec![
            video_stream("hevc", 1920, 1080, "yuv420p"),
            audio_stream("dts"), // fake — dts_hd_ma shows up as "dts" with a profile
        ]));
        // Plain DTS doesn't fire — we need "dts" + "hd" in the name.
        assert!(!out.evidence.iter().any(|e| {
            e.source == Source::BluRay
                && e.detail.contains("DTS")
        }));
        let out2 = scan_ffprobe_json(&probe_json(vec![
            video_stream("hevc", 1920, 1080, "yuv420p"),
            audio_stream("dts_hd_ma"),
        ]));
        assert!(out2.evidence.iter().any(|e| e.source == Source::BluRay));
    }

    #[test]
    fn pcm_audio_is_bluray() {
        let out = scan_ffprobe_json(&probe_json(vec![
            video_stream("mpeg2video", 1920, 1080, "yuv420p"),
            audio_stream("pcm_s16le"),
        ]));
        assert!(out.evidence.iter().any(|e| e.source == Source::BluRay));
    }

    #[test]
    fn aac_only_is_web() {
        let out = scan_ffprobe_json(&probe_json(vec![
            video_stream("h264", 1920, 1080, "yuv420p"),
            audio_stream("aac"),
        ]));
        assert!(out.evidence.iter().any(|e| e.source == Source::Web));
    }

    #[test]
    fn aac_with_flac_is_not_web() {
        // Dual audio BDs sometimes carry both an AAC downmix and a FLAC
        // master. FLAC wins.
        let out = scan_ffprobe_json(&probe_json(vec![
            video_stream("hevc", 1920, 1080, "yuv420p10le"),
            audio_stream("aac"),
            audio_stream("flac"),
        ]));
        assert!(!out.evidence.iter().any(|e| e.source == Source::Web));
        assert!(out.evidence.iter().any(|e| e.source == Source::BluRay));
    }

    #[test]
    fn h264_aac_8bit_fires_streaming_fingerprint() {
        let out = scan_ffprobe_json(&probe_json(vec![
            video_stream("h264", 1920, 1080, "yuv420p"),
            audio_stream("aac"),
        ]));
        // AAC-only rule + combo fingerprint = at least 2 Web hits.
        let web_count = out
            .evidence
            .iter()
            .filter(|e| e.source == Source::Web)
            .count();
        assert!(web_count >= 2);
    }

    #[test]
    fn eac3_is_web() {
        let out = scan_ffprobe_json(&probe_json(vec![
            video_stream("hevc", 1920, 1080, "yuv420p"),
            audio_stream("eac3"),
        ]));
        assert!(out.evidence.iter().any(|e| e.source == Source::Web));
    }

    #[test]
    fn pgs_subtitles_are_bluray() {
        let out = scan_ffprobe_json(&probe_json(vec![
            video_stream("hevc", 1920, 1080, "yuv420p"),
            subtitle_stream("hdmv_pgs_subtitle"),
        ]));
        assert!(out.evidence.iter().any(|e| e.source == Source::BluRay));
    }

    #[test]
    fn commentary_track_is_bluray() {
        let out = scan_ffprobe_json(&probe_json(vec![
            video_stream("hevc", 1920, 1080, "yuv420p"),
            audio_stream_with_title("ac3", "Director Commentary"),
        ]));
        assert!(out.evidence.iter().any(|e| e.source == Source::BluRay));
    }

    #[test]
    fn opus_audio_produces_no_evidence() {
        // Opus is explicitly on the "useless signal" list.
        let out = scan_ffprobe_json(&probe_json(vec![
            video_stream("av1", 1920, 1080, "yuv420p"),
            audio_stream("opus"),
        ]));
        assert!(out.evidence.is_empty());
    }

    #[test]
    fn bare_ac3_produces_no_evidence() {
        // AC-3 alone is too ambiguous (both BD and Web ship it for
        // backwards-compatible tracks).
        let out = scan_ffprobe_json(&probe_json(vec![
            video_stream("h264", 1920, 1080, "yuv420p"),
            audio_stream("ac3"),
        ]));
        assert!(out.evidence.is_empty());
    }

    #[test]
    fn hevc_alone_produces_no_evidence() {
        let out = scan_ffprobe_json(&probe_json(vec![video_stream(
            "hevc", 1920, 1080, "yuv420p",
        )]));
        assert!(out.evidence.is_empty());
    }

    #[test]
    fn resolution_set_even_when_no_source_evidence() {
        // Useless-signal file — we should still know it's 1080p.
        let out = scan_ffprobe_json(&probe_json(vec![
            video_stream("av1", 1920, 1080, "yuv420p"),
            audio_stream("opus"),
        ]));
        assert_eq!(out.resolution, Some(Resolution::R1080p));
    }

    #[test]
    fn missing_video_stream_emits_nothing() {
        let out = scan_ffprobe_json(&probe_json(vec![audio_stream("flac")]));
        assert!(out.evidence.is_empty());
        assert!(out.resolution.is_none());
    }
}
