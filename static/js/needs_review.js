// Mirrors OVERRIDE_SOURCE_MAP in series.js. Kept duplicated here rather
// than hoisted into base.js because base.js is shared across every page
// and most pages have no need for the override vocabulary — the
// duplication is short and grep-friendly.
const REVIEW_OVERRIDE_SOURCE_MAP = {
    bluray_bdmv: { source: 'BluRay', is_remux: false, is_bdmv: true,  web_kind: '' },
    bluray_remux:{ source: 'BluRay', is_remux: true,  is_bdmv: false, web_kind: '' },
    bluray:      { source: 'BluRay', is_remux: false, is_bdmv: false, web_kind: '' },
    web:         { source: 'Web',    is_remux: false, is_bdmv: false, web_kind: '' },
    webrip:      { source: 'Web',    is_remux: false, is_bdmv: false, web_kind: 'WEBRip' },
    dvd:         { source: 'DVD',    is_remux: false, is_bdmv: false, web_kind: '' },
    hdtv:        { source: 'HDTV',   is_remux: false, is_bdmv: false, web_kind: '' },
    tv:          { source: 'TV',     is_remux: false, is_bdmv: false, web_kind: '' },
    // See series.js: no `unknown` entry; fallbacks use `bluray`.
};

// Resolve the right dropdown key for the current verdict, honoring the
// Sonarr-parity BD variant flags and the Web sub-tier so a row that was
// originally classified as BD-Remux / BD-RAW / WEBRip pre-fills the
// specific variant instead of collapsing to plain `bluray` / `web`. The
// quintet (source + is_remux + is_bdmv + web_kind) is the same space
// OVERRIDE_SOURCE_MAP in series.js canonicalizes.
function reviewKeyFromClassification(source, isRemux, isBdmv, webKind) {
    const src = (source || '').toLowerCase();
    if (src === 'bluray' || src === 'blu-ray') {
        if (isBdmv) return 'bluray_bdmv';
        if (isRemux) return 'bluray_remux';
        return 'bluray';
    }
    if (src === 'web') {
        return ((webKind || '').toLowerCase() === 'webrip') ? 'webrip' : 'web';
    }
    if (src === 'dvd') return 'dvd';
    if (src === 'hdtv') return 'hdtv';
    if (src === 'tv') return 'tv';
    return 'bluray'; // sensible default for an uncertain row
}

// `data-*` attributes come out as strings; a missing flag renders as
// the literal string "false" (askama's Display on bool), so parse with
// a strict true-only check to avoid treating "false" as truthy.
function boolAttr(value) {
    return String(value || '').toLowerCase() === 'true';
}

// Pre-fill the dropdowns with the current (uncertain) verdict so the
// user only has to flip the field that's wrong instead of building the
// classification from scratch.
document.addEventListener('DOMContentLoaded', function() {
    document.querySelectorAll('tr[data-series-id]').forEach(function(row) {
        const src = row.dataset.currentSource || '';
        const res = row.dataset.currentResolution || '';
        const isRemux = boolAttr(row.dataset.currentIsRemux);
        const isBdmv = boolAttr(row.dataset.currentIsBdmv);
        const webKind = row.dataset.currentWebKind || '';
        const srcSel = row.querySelector('.review-source');
        const resSel = row.querySelector('.review-resolution');
        if (srcSel) {
            const key = reviewKeyFromClassification(src, isRemux, isBdmv, webKind);
            srcSel.value = REVIEW_OVERRIDE_SOURCE_MAP[key] ? key : 'bluray';
        }
        if (resSel && res) {
            const match = Array.from(resSel.options).find(o => o.value.toLowerCase() === res.toLowerCase());
            if (match) resSel.value = match.value;
        }
    });
});

function applyReviewOverride(btn) {
    const row = btn.closest('tr');
    if (!row) return;
    const seriesId = parseInt(row.dataset.seriesId, 10);
    const episodeNumber = parseInt(row.dataset.episode, 10);
    const key = row.querySelector('.review-source').value;
    // Defensive fallback to `bluray` instead of `unknown` — the dropdown
    // no longer offers Unknown (the handler 400s on Source::Unknown), so
    // falling back to the now-gone `unknown` map entry would have
    // produced `undefined.source` at runtime. Mirror series.js.
    const mapped = REVIEW_OVERRIDE_SOURCE_MAP[key] || REVIEW_OVERRIDE_SOURCE_MAP.bluray;
    const resolution = row.querySelector('.review-resolution').value;
    const status = row.querySelector('.review-status');
    if (status) {
        status.textContent = 'Saving…';
        status.style.display = 'block';
    }
    btn.disabled = true;
    fetch('/api/library/manual-override', {
        method: 'POST',
        headers: {'Content-Type': 'application/json'},
        body: JSON.stringify({
            series_id: seriesId,
            episode_number: episodeNumber,
            source: mapped.source,
            resolution: resolution,
            is_remux: mapped.is_remux,
            is_bdmv: mapped.is_bdmv,
            web_kind: mapped.web_kind,
        })
    })
    .then(async r => {
        let data = {};
        try { data = await r.json(); } catch (_) {}
        if (!r.ok) throw new Error(data.message || 'Failed to apply override');
        return data;
    })
    .then(_ => {
        // Manual override clears needs_review, so the row no longer
        // belongs in this list. Fade it out and remove instead of doing
        // a full page reload — keeps any other in-flight overrides
        // running uninterrupted.
        row.style.transition = 'opacity 0.2s';
        row.style.opacity = '0';
        setTimeout(function() {
            const tbody = row.parentNode;
            if (tbody) tbody.removeChild(row);
            if (tbody && tbody.children.length === 0) location.reload();
        }, 200);
    })
    .catch(err => {
        if (status) status.textContent = err.message || 'Failed';
        btn.disabled = false;
    });
}
