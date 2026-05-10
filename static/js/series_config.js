// Series-level configuration controls: monitor mode, manual
// classification override, allow-upgrades, allow-pt-upgrades,
// per-series search overrides. Each is a small fetch wrapper around
// the corresponding `/api/library/...` endpoint with optimistic UI
// state + revert-on-error.

function setMonitoring(mode) {
    const dbId = parseInt(SD.dbId);
    if (!dbId) return;
    const select = document.getElementById('monitor-mode');
    const summary = document.getElementById('monitor-summary');
    if (select) select.disabled = true;
    if (summary) summary.textContent = 'Updating monitoring…';

    fetch('/api/library/monitoring', {
        method: 'POST',
        headers: {'Content-Type': 'application/json'},
        body: JSON.stringify({ series_id: dbId, monitor_mode: mode })
    })
    .then(async r => {
        let data = {};
        try { data = await r.json(); } catch (_) {}
        if (!r.ok) throw new Error(data.message || 'Failed to update monitoring');
        return data;
    })
    .then(data => {
        if (summary) summary.textContent = `${data.monitor_mode_label || mode} · ${data.monitored_count || 0} monitored`;
        if (select) select.disabled = false;
        // Reload to reflect per-episode monitoring changes since they depend on server-side logic
        location.reload();
    })
    .catch(err => {
        if (summary) summary.textContent = err.message || 'Failed to update monitoring';
        if (select) select.disabled = false;
    });
}

var overrideTargetEpisode = null;

// Composite dropdown key ↔ backend quartet. Each entry maps the <select>'s
// value to the {source, is_remux, is_bdmv, web_kind} payload that the
// /api/library/manual-override handler expects. Centralising the mapping
// here means the HTML options and the POST body can't drift apart.
var OVERRIDE_SOURCE_MAP = {
    bluray_bdmv: { source: 'BluRay', is_remux: false, is_bdmv: true,  web_kind: '' },
    bluray_remux:{ source: 'BluRay', is_remux: true,  is_bdmv: false, web_kind: '' },
    bluray:      { source: 'BluRay', is_remux: false, is_bdmv: false, web_kind: '' },
    // Issue #48: WebDl and bare WEB collapse into a single user-
    // selectable option. The backend still tracks `web_kind` for CF
    // matching, but the manual override picker only offers WEB vs
    // WEBRip — if the user wants to pin a classification, they
    // shouldn't have to reason about the WebDl distinction.
    web:         { source: 'Web',    is_remux: false, is_bdmv: false, web_kind: '' },
    webrip:      { source: 'Web',    is_remux: false, is_bdmv: false, web_kind: 'WEBRip' },
    dvd:         { source: 'DVD',    is_remux: false, is_bdmv: false, web_kind: '' },
    hdtv:        { source: 'HDTV',   is_remux: false, is_bdmv: false, web_kind: '' },
    tv:          { source: 'TV',     is_remux: false, is_bdmv: false, web_kind: '' },
    // Note: no `unknown` entry. The <select> no longer offers Unknown
    // (the handler rejects Source::Unknown with 400), so a map entry
    // for it would be dead state — and worse, a footgun for any
    // `map[key] || map.unknown` fallback that outlives the dropdown
    // change. Fallbacks elsewhere now route to `map.bluray`.
};

// Reverse lookup: given the current classification quartet on the episode,
// pick the dropdown key that best represents it. Falls back to 'bluray' so
// a freshly-opened modal has a sensible default even when the tag is
// missing or unrecognized.
function overrideKeyFromClassification(source, isRemux, isBdmv, webKind) {
    const src = (source || '').toLowerCase();
    if (src === 'bluray' || src === 'blu-ray') {
        if (isBdmv) return 'bluray_bdmv';
        if (isRemux) return 'bluray_remux';
        return 'bluray';
    }
    if (src === 'web') {
        // WebRip gets its own key; everything else (bare WEB, WebDl,
        // Unknown sub-kind) maps to the unified 'web' key.
        const wk = (webKind || '').toLowerCase();
        if (wk === 'webrip') return 'webrip';
        return 'web';
    }
    if (src === 'dvd') return 'dvd';
    if (src === 'hdtv') return 'hdtv';
    if (src === 'tv') return 'tv';
    // Unknown-verdict rows fall back to 'bluray' as the pre-fill
    // default, matching reviewKeyFromClassification in
    // needs_review.html. Don't return 'unknown' — the dropdown no
    // longer offers that option (the handler rejects Source::Unknown
    // with 400), so `srcSelect.value = 'unknown'` would silently fail
    // and the <select> would render its first option (BD-RAW) as the
    // effective pre-fill for every Unknown-verdict episode.
    return 'bluray';
}

function openManualOverride(epNumber, currentSource, currentResolution, isRemux, isBdmv, webKind) {
    overrideTargetEpisode = epNumber;
    const modal = document.getElementById('override-modal');
    const title = document.getElementById('override-title');
    const srcSelect = document.getElementById('override-source');
    const resSelect = document.getElementById('override-resolution');
    const status = document.getElementById('override-status');
    if (title) title.textContent = `Override classification — Episode ${epNumber}`;
    if (srcSelect) {
        const key = overrideKeyFromClassification(currentSource, isRemux, isBdmv, webKind);
        srcSelect.value = OVERRIDE_SOURCE_MAP[key] ? key : 'bluray';
    }
    // Guard against currentResolution='Unknown': the dropdown no longer
    // offers that option, so a direct `.value = 'Unknown'` would fail
    // silently and the <select> would render its first option (2160p).
    // Fall through to 1080p, matching the default we use elsewhere.
    if (resSelect) {
        const resCandidate = currentResolution || '1080p';
        resSelect.value = resCandidate.toLowerCase() === 'unknown' ? '1080p' : resCandidate;
    }
    if (status) status.textContent = '';
    if (modal) modal.style.display = 'flex';
}

function closeManualOverride(event) {
    if (event && event.target !== event.currentTarget) return;
    const modal = document.getElementById('override-modal');
    if (modal) modal.style.display = 'none';
}

// Issue #166 — applyManualOverride / clearManualOverride / reclassifyEpisode
// moved to declarative `hx-post` on the modal-footer buttons in
// `templates/series.html`. The three helpers below build the form-
// values payload at click time via `hx-vals='js:'`. The composite
// dropdown key (`bluray_remux`, etc.) is unpacked here via
// OVERRIDE_SOURCE_MAP rather than server-side so the existing source-
// map (which doubles as the dropdown ↔ wire-shape mapping) doesn't
// need a parallel Rust port.
//
// `overrideTargetEpisode` is set when the modal opens via
// `openManualOverride` above. The helpers read it from module scope —
// the modal closes before the request lands, so a stale value can't
// race a fresh open: each open assigns the new value before the new
// click could fire.
function manualOverrideApplyVals() {
    const key = document.getElementById('override-source').value;
    // Defensive fallback: the dropdown can only produce keys that
    // exist in OVERRIDE_SOURCE_MAP, but a future refactor could pass
    // a stale/garbage key through. Use `bluray` (matches the dropdown
    // default that actually exists) instead of `unknown` — the latter
    // would build `{source: 'Unknown'}` which the handler 400s on.
    const mapped = OVERRIDE_SOURCE_MAP[key] || OVERRIDE_SOURCE_MAP.bluray;
    return {
        series_id: parseInt(SD.dbId),
        episode_number: overrideTargetEpisode,
        source: mapped.source,
        resolution: document.getElementById('override-resolution').value,
        is_remux: mapped.is_remux,
        is_bdmv: mapped.is_bdmv,
        web_kind: mapped.web_kind,
    };
}

function manualOverrideClearVals() {
    return {
        series_id: parseInt(SD.dbId),
        episode_number: overrideTargetEpisode,
        source: '',
        resolution: '',
        is_remux: false,
        is_bdmv: false,
        web_kind: '',
    };
}

function manualOverrideReclassifyVals() {
    return {
        series_id: parseInt(SD.dbId),
        episode_number: overrideTargetEpisode,
    };
}

// Issue #166 — `setAllowUpgrades`, `setAllowPtUpgrades`, and
// `saveSeriesSearchOverrides` moved to declarative HTMX attributes
// on the inputs/form in `templates/series.html`. The checkbox toggles
// post against `/api/library/allow-upgrades` and
// `/api/library/allow-pt-upgrades` with `hx-swap="none"`; an
// `hx-on::response-error` handler reverts the checkbox and fires a
// toast on 4xx/5xx. The search-overrides form posts to
// `/api/library/search-overrides` and swaps the rendered
// `partials/series/save_status_pill.html` into
// `#series-search-overrides-status`.
