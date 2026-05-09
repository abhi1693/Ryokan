// Interactive search: per-episode + per-series-batch flows. Both
// share the same `#isearch-modal` element so the user only sees one
// modal style; the difference is which endpoint feeds the table and
// where the Grab button posts. Also owns the score-breakdown
// expander positioning logic (the `.score-details` panel needs
// `position: fixed` lifting when its parent has `overflow:hidden`).

// Quick source detection from release title. The backend already carries
// the canonical ClassificationResult, but the interactive-search payload
// only exposes `resolution`. Per user preference the column here is a
// pure regex read of the title — no platform-tag tables, no group
// heuristics, no description scraping — so what you see in the UI is
// exactly what the filename says.
//
// Labels mirror the backend `ClassificationResult::label()` Sonarr-parity
// scheme so the value shown in interactive search equals the value
// persisted to `episode_tags` once the release is grabbed:
//     BDMV/BDISO/BD-Raw → `BD-1080p RAW`  (raw disc image)
//     Remux             → `BD-1080p Remux`
//     BluRay/BDRip/BD   → `BD-1080p`      (re-encode)
//     WEB-DL / WEB      → `WEB-1080p`     (unified — issue #48)
//     WEB-Rip           → `WEBRip-1080p`
// Note: BDRip is a re-encode and deliberately falls into the BluRay branch,
// NOT the BDMV branch.
function parseQualityFromTitle(title, resolution) {
    const t = String(title || '');
    let source = '';
    let suffix = ''; // ' RAW' | ' Remux' | '' — appended after resolution
    if (/\b(BDMV|BD-?Raw|BDISO)\b/i.test(t)) {
        source = 'BD'; suffix = ' RAW';
    } else if (/\bRemux\b/i.test(t)) {
        source = 'BD'; suffix = ' Remux';
    } else if (/\b(BluRay|Blu-?Ray|BDRip|BD|\.BD\.|\[BD\])\b/i.test(t)) {
        source = 'BD';
    } else if (/\bWEB-?Rip\b/i.test(t)) {
        // Check WebRip before the bare WEB branch — WebRip is the
        // lower-quality sub-tier and deserves its own label.
        source = 'WEBRip';
    } else if (/\bWEB(?:-?DL)?\b/i.test(t)) {
        // WebDl and bare WEB collapse into a single "WEB" label.
        source = 'WEB';
    } else if (/\bHDTV\b/i.test(t)) {
        source = 'HDTV';
    } else if (/\bDVD(?:Rip)?\b/i.test(t)) {
        source = 'DVD';
    }
    const res = String(resolution || '').trim();
    let base;
    if (source && res) base = `${source}-${res}`;
    else if (source)   base = source;
    else if (res)      base = res;
    else               return '—';
    return base + suffix;
}

// Seeders in green, leechers in red, separated by a dim slash.
// E.g., `32 / 5` where 32 is green and 5 is red.
function renderPeers(seeders, leechers) {
    const s = Number.isFinite(seeders) ? seeders : 0;
    const l = Number.isFinite(leechers) ? leechers : 0;
    return `<span class="seed-count">${s}</span><span class="peer-sep">/</span><span class="leech-count">${l}</span>`;
}

// #1.3.0 — score breakdown expander for the interactive search tables.
// Parallel to the server-rendered <details> in templates/search.html, so
// the UX is identical between the generic Nyaa search and the per-series
// interactive picker. Panel content lives in a named accordion group so
// opening a second breakdown auto-closes the first.
function renderScoreDetails(r, scoreClass) {
    const parts = r.score_breakdown || [];
    let inner;
    if (parts.length === 0) {
        inner = `<div class="form-hint">No components fired.</div>`;
    } else {
        const lis = parts.map(function (c) {
            const deltaClass = c.delta > 0 ? 'sc-delta-pos' : 'sc-delta-neg';
            const sign = c.delta > 0 ? '+' : '';
            const detail = c.detail
                ? `<span class="sc-detail">${escHtml(c.detail)}</span>`
                : '';
            return `<li>
                <span class="sc-delta ${deltaClass}">${sign}${c.delta}</span>
                <span class="sc-label">${escHtml(c.label)}</span>
                ${detail}
            </li>`;
        }).join('');
        inner = `<ul>${lis}</ul>`;
    }
    return `<details class="score-details" name="isearch-score-breakdown">
        <summary class="score-badge ${scoreClass}" title="Click to see breakdown">${r.score}</summary>
        <div class="score-components">
            <div class="score-components-title">Score breakdown</div>
            ${inner}
        </div>
    </details>`;
}

// Close any open <details class="score-details"> when the user clicks
// outside it or presses Escape. Registered once at module load; applies
// to both the interactive-search table and the batch table since they
// share the same markup shape.
//
// Also rewrites the panel's positioning to `fixed` on open when the
// expander lives inside an overflow-clipping ancestor (the interactive-
// search modal has `overflow:hidden` on `.modal` and `overflow-y:auto`
// on `.modal-body`, which would otherwise clip the absolutely-
// positioned `.score-components` panel out of sight). Without this the
// breakdown silently opened offscreen and looked like nothing happened
// when you clicked the score badge.
(function () {
    if (window.__ryokanScoreBreakdownInit) return;
    window.__ryokanScoreBreakdownInit = true;
    function closeAllOpenBreakdowns(except) {
        document.querySelectorAll('details.score-details[open]').forEach(function (d) {
            if (d !== except) d.removeAttribute('open');
            // Clear any inline fixed-position styles we applied on open.
            const panel = d.querySelector('.score-components');
            if (panel && d !== except) resetPanelPosition(panel);
        });
    }
    function resetPanelPosition(panel) {
        panel.style.position = '';
        panel.style.top = '';
        panel.style.left = '';
        panel.style.width = '';
        panel.style.minWidth = '';
        panel.style.maxWidth = '';
        panel.style.maxHeight = '';
        panel.style.overflowY = '';
    }
    function positionPanelIfClipped(details) {
        const panel = details.querySelector('.score-components');
        if (!panel) return;
        // Only lift to fixed-positioning when the details is inside an
        // overflow-clipping ancestor. Outside a modal the regular CSS
        // `position:absolute` works fine.
        let clipped = false;
        let node = details.parentElement;
        while (node && node !== document.body) {
            const cs = window.getComputedStyle(node);
            if (cs.overflow !== 'visible' || cs.overflowX !== 'visible' || cs.overflowY !== 'visible') {
                clipped = true;
                break;
            }
            node = node.parentElement;
        }
        if (!clipped) {
            resetPanelPosition(panel);
            return;
        }
        // Scrolling-only strategy — no flip-above fallback. The panel
        // always opens below the badge; vertical fit is handled by
        // `max-height` + internal scroll, horizontal fit by clamping
        // `left` and capping width to the viewport. Works the same on
        // desktop and mobile: narrow viewports just get a narrower
        // panel with more internal scroll.
        const GAP = 6;
        const MARGIN = 8;
        const rect = details.getBoundingClientRect();
        const vw = window.innerWidth;
        const vh = window.innerHeight;

        const top = rect.bottom + GAP;
        const maxHeight = Math.max(120, vh - top - MARGIN);
        const maxWidth = Math.max(240, vw - 2 * MARGIN);
        // Clamp left edge to stay within the viewport; on phones the
        // panel's full width often exceeds badge.left + panel.width,
        // so also cap the width when it would otherwise overflow.
        let left = rect.left;
        const desiredWidth = Math.min(360, maxWidth);
        if (left + desiredWidth + MARGIN > vw) {
            left = Math.max(MARGIN, vw - desiredWidth - MARGIN);
        }
        if (left < MARGIN) left = MARGIN;

        panel.style.position = 'fixed';
        panel.style.top = top + 'px';
        panel.style.left = left + 'px';
        panel.style.minWidth = '240px';
        panel.style.maxWidth = maxWidth + 'px';
        panel.style.maxHeight = maxHeight + 'px';
        panel.style.overflowY = 'auto';
    }
    document.addEventListener('click', function (evt) {
        const inside = evt.target.closest('details.score-details');
        closeAllOpenBreakdowns(inside);
    });
    document.addEventListener('keydown', function (evt) {
        if (evt.key === 'Escape') {
            closeAllOpenBreakdowns(null);
        }
    });
    // `toggle` doesn't bubble, so we capture it at the document level.
    document.addEventListener('toggle', function (evt) {
        const d = evt.target;
        if (!(d instanceof HTMLDetailsElement)) return;
        if (!d.classList.contains('score-details')) return;
        if (d.open) positionPanelIfClipped(d);
        else {
            const panel = d.querySelector('.score-components');
            if (panel) resetPanelPosition(panel);
        }
    }, true);
})();

function searchBatchReleases(btn) {
    setBusyButton(btn, true, 'Searching…');
    const pid = window.ryokanNewProgressId();
    const toast = window.ryokanProgressToast({
        progressId: pid,
        kind: 'info',
        category: 'auto_search',
        title: 'Searching for batch releases',
        body: SD.titleEnglish || SD.titleRomaji || '',
    });
    fetch(`/api/series/${SD.id}/search-batch?progress_id=${encodeURIComponent(pid)}`, {
        method: 'POST',
        headers: {'Content-Type': 'application/json'}
    })
    .then(async resp => {
        let data = {};
        try { data = await resp.json(); } catch (_) {}
        if (!resp.ok) throw new Error(data.message || (resp.status === 404 ? 'No batch release found' : 'Batch search failed'));
        const grabbed = Array.isArray(data.grabbed) ? data.grabbed.length : 0;
        setBusyButton(btn, false);
        if (grabbed > 0) {
            ensureDlPollRunning();
            refreshEpisodeRows({ force: true });
        }
    })
    .catch(err => {
        setBusyButton(btn, false);
        toast.finalize({
            kind: 'error',
            title: 'Batch search failed',
            body: err && err.message ? err.message : 'Unknown error',
        });
    });
}

var _isearchEpNum = null;
var _isearchResults = [];

function openInteractiveSearch(epNum, btn) {
    _isearchEpNum = epNum;
    _isearchResults = [];
    const modal = document.getElementById('isearch-modal');
    const titleEl = document.getElementById('isearch-title');
    const body = document.getElementById('isearch-body');
    titleEl.textContent = `Interactive Search — Episode ${epNum}`;
    body.innerHTML = '<div style="text-align:center;color:var(--text-dim);padding:32px">Searching…</div>';
    modal.style.display = 'flex';

    fetch(`/api/series/${SD.id}/interactive-search/${epNum}`)
        .then(r => r.json())
        .then(results => {
            _isearchResults = results || [];
            renderInteractiveResults(_isearchResults, epNum);
        })
        .catch(err => {
            body.innerHTML = `<div style="text-align:center;color:var(--red);padding:32px">${escHtml(err.message || 'Search failed')}</div>`;
        });
}

function renderInteractiveResults(results, epNum) {
    const body = document.getElementById('isearch-body');
    if (!body) return;
    if (!results || !results.length) {
        body.innerHTML = '<div style="text-align:center;color:var(--text-dim);padding:32px">No results found.</div>';
        return;
    }
    let html = '<table class="interactive-search-table"><thead><tr><th class="col-score">Score</th><th>Release</th><th class="col-indexer">Indexer</th><th>Group</th><th class="col-quality">Quality</th><th class="col-size">Size</th><th class="col-seeds">Seeds</th><th class="col-grab">Grab</th></tr></thead><tbody>';
    results.forEach((r, idx) => {
        const batchTag = r.is_batch ? '<span class="tag tag-batch" style="margin-left:4px">batch</span>' : '';
        const trustedTag = r.is_trusted ? '<span class="tag tag-trusted" style="margin-left:4px">trusted</span>' : '';
        const scoreClass = r.score >= 80 ? 'score-high' : r.score >= 40 ? 'score-mid' : 'score-low';
        // Empty `indexer_name` falls back to "Nyaa" — Nyaa-direct
        // results don't carry a name (Nyaa isn't a row in the indexers
        // table per plan decision #1) but the column should still
        // attribute every row so the user can tell where a hit came
        // from at a glance.
        const indexer = escHtml(r.indexer_name || 'Nyaa');
        html += `<tr>
            <td class="col-score">${renderScoreDetails(r, scoreClass)}</td>
            <td><a class="isearch-release-link" href="${escHtml(r.link)}" target="_blank" rel="noopener">${escHtml(r.title)}</a>${batchTag}${trustedTag}</td>
            <td class="col-indexer" style="color:var(--text-dim)">${indexer}</td>
            <td style="color:var(--text-dim)">${escHtml(r.group)}</td>
            <td class="col-quality">${escHtml(r.quality_label || parseQualityFromTitle(r.title, r.resolution))}</td>
            <td class="col-size" style="color:var(--text-dim)">${escHtml(r.size)}</td>
            <td class="col-seeds">${renderPeers(r.seeders, r.leechers)}</td>
            <td class="col-grab"><button class="btn-grab" data-idx="${idx}" onclick="grabInteractiveResult(${epNum}, ${idx}, this)">Grab</button></td>
        </tr>`;
    });
    html += '</tbody></table>';
    body.innerHTML = html;
}

function grabInteractiveResult(epNum, idx, btn) {
    const result = _isearchResults[idx];
    if (!result) return;
    const url = result.magnet || result.torrent || '';

    // Issue #83 — batch releases open the file-picker modal so the
    // user can narrow to the episodes they actually want. Single-file
    // releases always take the direct /api/series/.../grab path
    // (nothing to pick). `grab_preview_mode = 'never'` opts out
    // globally and keeps 1.3.0-style one-click behavior.
    const previewMode = window.GRAB_PREVIEW_MODE || 'batches_only';
    if (result.is_batch
        && previewMode !== 'never'
        && typeof window.openGrabPicker === 'function'
        && result.info_hash) {
        window.openGrabPicker(url, {
            title: result.title || '',
            size: result.size || '',
            seeders: Number(result.seeders) || 0,
            group: result.group || '',
            infoHash: result.info_hash || '',
            seriesId: SD.dbId || null,
            isBatch: true,
            onConfirm: function () {
                updateEpisodeRow(epNum, 'grabbed', result.group);
                ensureDlPollRunning();
                refreshEpisodeRows({ force: true });
                const ismodal = document.getElementById('isearch-modal');
                if (ismodal) ismodal.style.display = 'none';
            },
        });
        return;
    }

    btn.disabled = true;
    btn.textContent = 'Grabbing…';
    fetch(`/api/series/${SD.id}/grab/${epNum}`, {
        method: 'POST',
        headers: {'Content-Type': 'application/json'},
        body: JSON.stringify({
            url,
            title: result.title,
            group: result.group,
            resolution: result.resolution,
            info_hash: result.info_hash,
            size_bytes: result.size_bytes || 0,
            indexer_id: result.indexer_id ?? null
        })
    })
    .then(async r => {
        // The server returns errors as `(StatusCode, String)`, which axum
        // serializes as plain-text bodies — NOT JSON. Reading r.json()
        // first returns `{}`, dropping the actual server error and leaving
        // the user with an unhelpful "Grab failed" toast (the JS-side
        // fallback). Read text first, parse JSON only on success
        // responses where the handler returns a JSON envelope.
        const text = await r.text();
        if (!r.ok) {
            throw new Error(text && text.trim().length > 0 ? text : 'Grab failed');
        }
        let data = {};
        try { data = JSON.parse(text); } catch (_) {}
        btn.textContent = 'Sent';
        btn.classList.add('btn-success');
        // Update the episode row to show grabbed state
        updateEpisodeRow(epNum, 'grabbed', result.group);
        ensureDlPollRunning();
        refreshEpisodeRows({ force: true });
        window.ryokanToast({
            kind: 'success',
            category: 'grab',
            title: `Episode ${epNum} queued`,
            body: result.title + (result.group ? ' · ' + result.group : ''),
        });
        // Close the modal after a short delay
        setTimeout(() => {
            document.getElementById('isearch-modal').style.display = 'none';
        }, 600);
    })
    .catch(err => {
        btn.textContent = 'Error';
        btn.classList.add('btn-error');
        btn.disabled = false;
        window.ryokanToast({
            kind: 'error',
            category: 'grab',
            title: `Grab failed for episode ${epNum}`,
            body: err && err.message ? err.message : 'Unknown error',
        });
    });
}

function closeInteractiveSearch(e) {
    const modal = document.getElementById('isearch-modal');
    if (e && e.target !== modal) return;
    modal.style.display = 'none';
}

// ── Interactive batch search ───────────────────────────────────────
// Parallel flow to openInteractiveSearch but for batch releases.
// Shares the isearch-modal element so the UI only has one modal to
// style. The results render is nearly identical but routes its Grab
// action to /grab-batch instead of the per-episode /grab endpoint.
var _ibatchResults = [];

function openInteractiveBatchSearch(btn) {
    _ibatchResults = [];
    const modal = document.getElementById('isearch-modal');
    const titleEl = document.getElementById('isearch-title');
    const body = document.getElementById('isearch-body');
    titleEl.textContent = 'Interactive Batch Search';
    body.innerHTML = '<div style="text-align:center;color:var(--text-dim);padding:32px">Searching batch releases…</div>';
    modal.style.display = 'flex';

    fetch(`/api/series/${SD.id}/interactive-search-batch`)
        .then(r => r.json())
        .then(results => {
            _ibatchResults = results || [];
            renderInteractiveBatchResults(_ibatchResults);
        })
        .catch(err => {
            body.innerHTML = `<div style="text-align:center;color:var(--red);padding:32px">${escHtml(err.message || 'Search failed')}</div>`;
        });
}

function renderInteractiveBatchResults(results) {
    const body = document.getElementById('isearch-body');
    if (!body) return;
    if (!results || !results.length) {
        body.innerHTML = '<div style="text-align:center;color:var(--text-dim);padding:32px">No batch releases found.</div>';
        return;
    }
    let html = '<table class="interactive-search-table"><thead><tr><th class="col-score">Score</th><th>Release</th><th class="col-indexer">Indexer</th><th>Group</th><th class="col-quality">Quality</th><th class="col-size">Size</th><th class="col-seeds">Seeds</th><th class="col-grab">Grab</th></tr></thead><tbody>';
    results.forEach((r, idx) => {
        const batchTag = r.is_batch ? '<span class="tag tag-batch" style="margin-left:4px">batch</span>' : '';
        const trustedTag = r.is_trusted ? '<span class="tag tag-trusted" style="margin-left:4px">trusted</span>' : '';
        const scoreClass = r.score >= 80 ? 'score-high' : r.score >= 40 ? 'score-mid' : 'score-low';
        const indexer = escHtml(r.indexer_name || 'Nyaa');
        html += `<tr>
            <td class="col-score">${renderScoreDetails(r, scoreClass)}</td>
            <td><a class="isearch-release-link" href="${escHtml(r.link)}" target="_blank" rel="noopener">${escHtml(r.title)}</a>${batchTag}${trustedTag}</td>
            <td class="col-indexer" style="color:var(--text-dim)">${indexer}</td>
            <td style="color:var(--text-dim)">${escHtml(r.group)}</td>
            <td class="col-quality">${escHtml(r.quality_label || parseQualityFromTitle(r.title, r.resolution))}</td>
            <td class="col-size" style="color:var(--text-dim)">${escHtml(r.size)}</td>
            <td class="col-seeds">${renderPeers(r.seeders, r.leechers)}</td>
            <td class="col-grab"><button class="btn-grab" data-idx="${idx}" onclick="grabInteractiveBatchResult(${idx}, this)">Grab</button></td>
        </tr>`;
    });
    html += '</tbody></table>';
    body.innerHTML = html;
}

function grabInteractiveBatchResult(idx, btn) {
    const result = _ibatchResults[idx];
    if (!result) return;
    const url = result.magnet || result.torrent || '';

    // Issue #83 — every result in the interactive batch search is a
    // batch by definition, so the file-picker modal opens unless the
    // user has opted out globally via `grab_preview_mode = 'never'`.
    const previewMode = window.GRAB_PREVIEW_MODE || 'batches_only';
    if (previewMode !== 'never'
        && typeof window.openGrabPicker === 'function'
        && result.info_hash) {
        window.openGrabPicker(url, {
            title: result.title || '',
            size: result.size || '',
            seeders: Number(result.seeders) || 0,
            group: result.group || '',
            infoHash: result.info_hash || '',
            seriesId: SD.dbId || null,
            isBatch: true,
            onConfirm: function () {
                ensureDlPollRunning();
                refreshEpisodeRows({ force: true });
                const ismodal = document.getElementById('isearch-modal');
                if (ismodal) ismodal.style.display = 'none';
            },
        });
        return;
    }

    btn.disabled = true;
    btn.textContent = 'Grabbing…';
    fetch(`/api/series/${SD.id}/grab-batch`, {
        method: 'POST',
        headers: {'Content-Type': 'application/json'},
        body: JSON.stringify({
            url,
            title: result.title,
            group: result.group,
            resolution: result.resolution,
            info_hash: result.info_hash,
            size_bytes: result.size_bytes || 0,
            indexer_id: result.indexer_id ?? null
        })
    })
    .then(async r => {
        // Same error-surfacing pattern as the single-episode grab —
        // axum's `(StatusCode, String)` errors come back as plain text,
        // not JSON, so reading r.json() first drops the actual server
        // message.
        const text = await r.text();
        if (!r.ok) {
            throw new Error(text && text.trim().length > 0 ? text : 'Grab failed');
        }
        btn.textContent = 'Sent';
        btn.classList.add('btn-success');
        ensureDlPollRunning();
        refreshEpisodeRows({ force: true });
        window.ryokanToast({
            kind: 'success',
            category: 'grab',
            title: 'Batch queued',
            body: result.title + (result.group ? ' · ' + result.group : ''),
        });
        setTimeout(() => {
            document.getElementById('isearch-modal').style.display = 'none';
        }, 600);
    })
    .catch(err => {
        btn.textContent = 'Error';
        btn.classList.add('btn-error');
        btn.disabled = false;
        window.ryokanToast({
            kind: 'error',
            category: 'grab',
            title: 'Batch grab failed',
            body: err && err.message ? err.message : 'Unknown error',
        });
    });
}
