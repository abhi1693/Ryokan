const SD = document.getElementById('series-data').dataset;

// Title-language switching is handled entirely by CSS via the
// `html[data-title-language]` attribute set by the inline head script in
// base.html. No DOM walking here — doing it post-parse caused a visible
// flash of the english titles before they were swapped to romaji.

function toggleMonitorAll(dbId, currentlyAllMonitored) {
    if (!dbId) return;
    const btn = document.getElementById('btn-monitor-all');
    const summary = document.getElementById('monitor-summary');
    const newMode = currentlyAllMonitored ? 'none' : 'all';
    if (btn) btn.disabled = true;
    if (summary) summary.textContent = 'Updating…';
    fetch('/api/library/monitoring', {
        method: 'POST',
        headers: {'Content-Type': 'application/json'},
        body: JSON.stringify({ series_id: dbId, monitor_mode: newMode })
    })
    .then(async r => {
        let data = {};
        try { data = await r.json(); } catch (_) {}
        if (!r.ok) throw new Error(data.message || 'Failed');
        const newState = newMode === 'all';
        // Update all monitor buttons in the table
        document.querySelectorAll('.ep-mon-btn').forEach(monBtn => {
            monBtn.textContent = newState ? 'Yes' : 'No';
            monBtn.className = 'ep-mon-btn ' + (newState ? 'ep-mon-yes' : 'ep-mon-no');
            monBtn.title = newState ? 'Monitored — click to unmonitor' : 'Not monitored — click to monitor';
        });
        // Update the monitor-all button
        if (btn) {
            btn.disabled = false;
            btn.classList.toggle('is-active', newState);
            btn.onclick = function() { toggleMonitorAll(dbId, newState); };
            btn.title = newState ? 'All monitored — click to unmonitor all' : 'Click to monitor all episodes';
            btn.innerHTML = newState
                ? '<svg width="14" height="14" viewBox="0 0 24 24" fill="currentColor" stroke="none"><path d="M19 21l-7-5-7 5V5a2 2 0 012-2h10a2 2 0 012 2z"/></svg>'
                : '<svg width="14" height="14" viewBox="0 0 24 24" fill="none" stroke="currentColor" stroke-width="2"><path d="M19 21l-7-5-7 5V5a2 2 0 012-2h10a2 2 0 012 2z"/></svg>';
        }
        if (summary) summary.textContent = `${data.monitor_mode_label || newMode} · ${data.monitored_count || 0} monitored`;
        // Update the select dropdown
        const select = document.getElementById('monitor-mode');
        if (select) select.value = newMode;
    })
    .catch(err => {
        if (summary) summary.textContent = err.message || 'Failed to update monitoring';
        if (btn) btn.disabled = false;
    });
}

// Legacy alias used in older code paths.
function monitorAll(dbId) { toggleMonitorAll(dbId, false); }

// HTMX migration (issue #129) — toggleEpisodeMonitor() removed; the
// per-episode monitor button now uses `hx-post` directly. The handler
// at `/api/library/episode-monitoring` returns the swapped button HTML
// for HX-Request, JSON otherwise (preserving the API contract).

let _currentEpNum = null;

// Restore a footer button (Delete File / Cancel Pending) to its
// ready-to-click shape on every modal open. The fetch success path
// intentionally leaves the button with `disabled = true` + loading
// text so a double-click can't fire a second request in flight; but
// the modal closes immediately and the button lives on in the DOM
// (it's a singleton footer element, not re-rendered per episode),
// so without this reset the next modal opens with a stuck
// "Deleting…" / "Cancelling…" label and disabled state.
//
// The initial HTML snapshot is captured on first use via
// `dataset.defaultHtml` — avoids having to re-declare the SVG +
// label inline in the JS.
function resetFooterButton(btn) {
    if (!btn) return;
    if (!btn.dataset.defaultHtml) {
        btn.dataset.defaultHtml = btn.innerHTML;
    }
    btn.innerHTML = btn.dataset.defaultHtml;
    btn.disabled = false;
}

function showEpisodeDetail(epNum, btn) {
    const filename = btn.dataset.filename || '';
    const size = btn.dataset.size || '';
    const onDisk = btn.dataset.onDisk === 'true';
    const modal = document.getElementById('ep-detail-modal');
    const titleEl = document.getElementById('ep-detail-title');
    const body = document.getElementById('ep-detail-body');
    const deleteBtn = document.getElementById('btn-delete-file');
    const cancelBtn = document.getElementById('btn-cancel-pending');
    _currentEpNum = epNum;
    titleEl.textContent = 'Episode ' + epNum;

    // Reset both footer buttons before toggling visibility so a button
    // that was mid-request when its modal was closed shows up clean in
    // the next episode's modal.
    resetFooterButton(deleteBtn);
    resetFooterButton(cancelBtn);

    // Show/hide delete button
    if (deleteBtn) deleteBtn.style.display = onDisk ? '' : 'none';
    // Cancel-pending button: visible when the episode row is in the
    // 'grabbed' state (torrent sent but post-processing hasn't landed
    // yet). Detect via the `ep-row-queued` class that updateEpisodeRow
    // toggles on the row when a grab lands; hidden otherwise so it
    // doesn't clutter on-disk / missing rows where it would be a no-op.
    if (cancelBtn) {
        const rows = document.querySelectorAll('.episode-table tbody tr');
        let isPending = false;
        for (const row of rows) {
            const numCell = row.querySelector('.ep-col-num');
            if (!numCell || parseInt(numCell.textContent.trim()) !== epNum) continue;
            isPending = row.classList.contains('ep-row-queued');
            break;
        }
        cancelBtn.style.display = isPending ? '' : 'none';
    }

    // Two stable slots the grab-history loader patches in place once
    // data arrives: the library-side file path (media_root-relative,
    // rendered here when on_disk) and the download-client-side
    // content_path (rendered by renderGrabHistory if the current grab
    // has a client_content_path). Both can coexist when post-processing
    // uses hardlinks — the user wants to see both; the Sonarr dual-path
    // split (#14 follow-up) makes this possible.
    const mediaRoot = document.getElementById('series-data').dataset.mediaRoot || '';
    const folderName = document.getElementById('series-data').dataset.folderName || '';
    const libraryPath = (onDisk && filename)
        ? [mediaRoot, folderName, filename].filter(Boolean).join('/')
        : '';
    body.innerHTML =
        '<div class="ep-detail-two-col">' +
        (libraryPath
            ? '<div class="ep-detail-row ep-detail-full"><span class="ep-detail-label">Library path</span><span class="ep-detail-value ep-detail-path">' + escHtml(libraryPath) + '</span></div>'
            : '<div class="ep-detail-row ep-detail-full" id="ep-detail-library-placeholder"><span class="ep-detail-label">Library path</span><span class="ep-detail-value" style="color:var(--text-dim)">Not in library root</span></div>') +
        // Client path row is always rendered as a placeholder;
        // renderGrabHistory fills it in when the current grab has a
        // client_content_path. Hidden until then so empty state doesn't
        // render for rows whose torrent hasn't finished downloading yet.
        '<div class="ep-detail-row ep-detail-full" id="ep-detail-client-path" style="display:none"><span class="ep-detail-label">Output path</span><span class="ep-detail-value ep-detail-path" id="ep-detail-client-path-value"></span></div>' +
        // The Size row starts with the on-disk file size and is
        // patched in place by `renderGrabHistory` once the grab
        // history loads: if the latest grab was a batch, the row is
        // rewritten to show the whole-torrent total with a
        // "(batch total)" hint. Always rendered (even when size is
        // empty) so the batch-patch has a stable target to find.
        '<div class="ep-detail-row"><span class="ep-detail-label">Size</span><span class="ep-detail-value ep-detail-size-value">' + escHtml(size || '—') + '</span></div>' +
        '</div>' +
        '<div class="ep-detail-row" id="grab-history-section" style="margin-top:16px"><span class="ep-detail-label">Grab History</span><div id="grab-history-body" style="margin-top:6px;color:var(--text-dim);font-size:12px">Loading…</div></div>';
    modal.style.display = 'flex';

    // Load grab history
    if (SD.dbId) {
        fetch(`/api/series/${SD.id}/grab-history/${epNum}`)
            .then(r => r.json())
            .then(entries => renderGrabHistory(entries, epNum))
            .catch(() => {
                const el = document.getElementById('grab-history-body');
                if (el) el.textContent = 'No history.';
            });
    }
}

function formatBytes(bytes) {
    if (!bytes || bytes <= 0) return '';
    const gb = bytes / (1024 * 1024 * 1024);
    if (gb >= 1) return gb.toFixed(1) + ' GiB';
    const mb = bytes / (1024 * 1024);
    return Math.round(mb) + ' MiB';
}

function renderGrabHistory(entries, epNum) {
    const el = document.getElementById('grab-history-body');
    if (!el) return;
    if (!entries || !entries.length) {
        el.textContent = 'No grab history.';
        return;
    }
    // Table lives inside a scroll container so history past 10 entries
    // scrolls within the modal rather than blowing out modal height.
    let html = '<div class="grab-history-scroll"><table class="grab-history-table"><thead><tr><th>Quality</th><th>Release</th><th>File Name</th><th>Group</th><th>Size</th><th>Date</th><th>State</th><th></th></tr></thead><tbody>';
    for (const e of entries) {
        const stateClass = e.state === 'failed' ? 'grab-state-failed'
            : e.state === 'removed' ? 'grab-state-removed'
            : e.state === 'replaced' ? 'grab-state-replaced'
            : e.state === 'completed' ? 'grab-state-completed'
            : 'grab-state-grabbed';
        // Only active 'grabbed' rows can be manually failed — once
        // post-processing flips to 'completed' the user should delete
        // the file or trigger an upgrade instead.
        const canFail = e.state === 'grabbed';
        // File name column: shows the post-processed on-disk basename
        // once post-processing lands the file. Before that it's still
        // seeded with the release title, so hide the duplicate — the
        // Release column already carries that.
        const fileName = e.file_name && e.file_name.length ? e.file_name : e.release_title;
        const sameAsRelease = fileName === e.release_title;
        const fileCell = sameAsRelease
            ? '<span style="color:var(--text-dim)">—</span>'
            : escHtml(fileName);
        // Size column: for batch grabs it's the whole-torrent total
        // (suffixed with a dim " (batch)" hint) so the user can tell
        // it's a pack size rather than a per-episode size.
        const sizeText = formatBytes(e.size_bytes);
        const sizeCell = sizeText
            ? (e.is_batch
                ? escHtml(sizeText) + ' <span style="color:var(--text-dim);font-size:10px">(batch)</span>'
                : escHtml(sizeText))
            : '';
        html += `<tr>
            <td>${escHtml(e.quality_tag)}</td>
            <td class="grab-history-ellipsis" title="${escHtml(e.release_title)}">${escHtml(e.release_title)}</td>
            <td class="grab-history-ellipsis" title="${escHtml(fileName)}">${fileCell}</td>
            <td>${escHtml(e.release_group)}</td>
            <td style="white-space:nowrap;color:var(--text-dim)">${sizeCell}</td>
            <td style="white-space:nowrap;color:var(--text-dim)">${escHtml(e.grabbed_at)}</td>
            <td class="${stateClass}">${escHtml(e.state)}</td>
            <td>${canFail ? `<button class="btn-mark-failed" onclick="markEpisodeFailed(${e.id}, ${epNum}, this)">Mark Failed</button>` : ''}</td>
        </tr>`;
    }
    html += '</tbody></table></div>';
    el.innerHTML = html;

    // Task 24: the episode detail "Size" row above the grab history
    // should reflect the batch total when the latest grab for this
    // episode was a batch, not the per-file on-disk size. We only
    // know this once history loads, so patch the row in-place after
    // the table renders. Find the newest non-failed entry as the
    // current source of truth — a 'completed' row wins over an older
    // 'grabbed' row sitting behind a failed upgrade attempt.
    const current = entries.find(function(e) { return e.state === 'completed' || e.state === 'grabbed'; });
    if (current && current.is_batch && current.size_bytes > 0) {
        const sizeValueEl = document.querySelector('#ep-detail-body .ep-detail-size-value');
        if (sizeValueEl) {
            sizeValueEl.innerHTML = escHtml(formatBytes(current.size_bytes))
                + ' <span style="color:var(--text-dim);font-size:11px">(batch total)</span>';
        }
    }

    // Dual-path display: if the current grab has a client content path
    // (populated by post-processing when the torrent reports complete),
    // reveal the client path row in the detail header. Shown whenever
    // present — with post-proc on + hardlink mode both paths point at
    // the same bytes but are still worth surfacing so the operator can
    // find the torrent in the download client without guessing.
    if (current && current.client_content_path) {
        const clientRow = document.getElementById('ep-detail-client-path');
        const clientValue = document.getElementById('ep-detail-client-path-value');
        if (clientRow && clientValue) {
            clientValue.textContent = current.client_content_path;
            clientRow.style.display = '';
        }
    }
}

function markEpisodeFailed(historyId, epNum, btn) {
    window.ryokanConfirm({
        title: `Mark Episode ${epNum} as Failed`,
        body: 'Mark this grab as failed and re-search for the episode?',
        yesLabel: 'Mark Failed',
        noLabel: 'Cancel',
        extras: [{id: 'blocklist', label: 'Also add this release to the blocklist', default: false}],
    }).then(function(res) {
        if (!res.ok) return;
        const addToBlocklist = !!res.extras.blocklist;
        btn.disabled = true;
        btn.textContent = 'Searching…';
        fetch(`/api/series/${SD.id}/mark-failed/${epNum}`, {
            method: 'POST',
            headers: {'Content-Type': 'application/json'},
            body: JSON.stringify({ history_id: historyId, blocklist: addToBlocklist })
        })
        .then(async r => {
            let data = {};
            try { data = await r.json(); } catch (_) {}
            if (!r.ok) throw new Error(data.message || 'Failed');
            const grabbed = Array.isArray(data.grabbed) ? data.grabbed.length : 0;
            btn.textContent = grabbed > 0 ? 'Re-grabbed' : 'No result';
            if (grabbed > 0) {
                const first = data.grabbed[0];
                updateEpisodeRow(epNum, 'grabbed', first.release_group);
                ensureDlPollRunning();
                refreshEpisodeRows({ force: true });
                window.ryokanToast({
                    kind: 'success',
                    category: 'auto_search',
                    title: `Episode ${epNum} re-grabbed`,
                    body: first.release_title + (first.release_group ? ' · ' + first.release_group : ''),
                });
            } else {
                window.ryokanToast({
                    kind: 'warn',
                    category: 'auto_search',
                    title: `No replacement for episode ${epNum}`,
                    body: 'Nothing on Nyaa matched after marking the current grab as failed.',
                });
            }
            // Refresh the grab history in the modal
            if (SD.dbId) {
                fetch(`/api/series/${SD.id}/grab-history/${epNum}`)
                    .then(r => r.json())
                    .then(entries => renderGrabHistory(entries, epNum))
                    .catch(() => {});
            }
        })
        .catch(err => {
            btn.disabled = false;
            btn.textContent = 'Mark Failed';
            window.ryokanToast({
                kind: 'error',
                category: 'auto_search',
                title: `Mark-failed error for episode ${epNum}`,
                body: err && err.message ? err.message : 'Unknown error',
            });
        });
    });
}

async function deleteEpisodeFile() {
    const epNum = _currentEpNum;
    if (!epNum) return;
    const confirmed = await window.ryokanConfirm({
        title: 'Delete episode file',
        body: `Delete the file for Episode ${epNum} from disk? This cannot be undone.`,
        yesLabel: 'Delete',
    });
    if (!confirmed.ok) return;
    const btn = document.getElementById('btn-delete-file');
    if (btn) { btn.disabled = true; btn.textContent = 'Deleting…'; }
    fetch(`/api/series/${SD.id}/delete-file/${epNum}`, { method: 'POST', headers: {'Content-Type': 'application/json'} })
        .then(async r => {
            let data = {};
            try { data = await r.json(); } catch (_) {}
            if (!r.ok) throw new Error(data.message || 'Delete failed');
            document.getElementById('ep-detail-modal').style.display = 'none';
            updateEpisodeRow(epNum, 'deleted');
            refreshEpisodeRows({ force: true });
            window.ryokanToast({
                kind: 'success',
                category: 'library',
                title: `Episode ${epNum} deleted`,
                body: 'File removed from disk.',
            });
        })
        .catch(err => {
            if (btn) { btn.disabled = false; btn.textContent = 'Delete File'; }
            window.ryokanToast({
                kind: 'error',
                category: 'library',
                title: `Delete failed for episode ${epNum}`,
                body: err && err.message ? err.message : 'Unknown error',
            });
        });
}

// Cancel an in-flight grab: removes the torrent from qBit (with its
// partial/complete data), marks the grab 'removed' in the DB, clears
// the episode's quality tag. Does NOT trigger a re-search — the user
// wanted to drop this one, not find a replacement. Mirrors
// `deleteEpisodeFile` for the pending-grab state.
async function cancelPendingEpisode() {
    const epNum = _currentEpNum;
    if (!epNum) return;
    const confirmed = await window.ryokanConfirm({
        title: 'Cancel pending grab',
        body: `Remove the in-flight torrent for Episode ${epNum} from the download client and mark it cancelled? This will delete any downloaded data and will not trigger a re-search.`,
        yesLabel: 'Cancel grab',
        noLabel: 'Keep',
    });
    if (!confirmed.ok) return;
    const btn = document.getElementById('btn-cancel-pending');
    if (btn) { btn.disabled = true; btn.textContent = 'Cancelling…'; }
    fetch(`/api/series/${SD.id}/cancel-pending/${epNum}`, { method: 'POST', headers: {'Content-Type': 'application/json'} })
        .then(async r => {
            let data = {};
            try { data = await r.json(); } catch (_) {}
            if (!r.ok) throw new Error(data.message || 'Cancel failed');
            document.getElementById('ep-detail-modal').style.display = 'none';
            updateEpisodeRow(epNum, 'deleted');
            refreshEpisodeRows({ force: true });
            window.ryokanToast({
                kind: 'success',
                category: 'library',
                title: `Episode ${epNum} cancelled`,
                body: `${data.cancelled || 0} pending grab(s) removed.`,
            });
        })
        .catch(err => {
            if (btn) { btn.disabled = false; btn.textContent = 'Cancel Pending'; }
            window.ryokanToast({
                kind: 'error',
                category: 'library',
                title: `Cancel failed for episode ${epNum}`,
                body: err && err.message ? err.message : 'Unknown error',
            });
        });
}

function closeEpisodeDetail(e) {
    const modal = document.getElementById('ep-detail-modal');
    if (e && e.target !== modal) return;
    modal.style.display = 'none';
}

function escHtml(s) {
    const d = document.createElement('div');
    d.textContent = String(s);
    return d.innerHTML;
}

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

let _isearchEpNum = null;
let _isearchResults = [];

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
    let html = '<table class="interactive-search-table"><thead><tr><th class="col-score">Score</th><th>Release</th><th>Group</th><th class="col-quality">Quality</th><th class="col-size">Size</th><th class="col-seeds">Seeds</th><th class="col-grab">Grab</th></tr></thead><tbody>';
    results.forEach((r, idx) => {
        const batchTag = r.is_batch ? '<span class="tag tag-batch" style="margin-left:4px">batch</span>' : '';
        const trustedTag = r.is_trusted ? '<span class="tag tag-trusted" style="margin-left:4px">trusted</span>' : '';
        const scoreClass = r.score >= 80 ? 'score-high' : r.score >= 40 ? 'score-mid' : 'score-low';
        html += `<tr>
            <td class="col-score">${renderScoreDetails(r, scoreClass)}</td>
            <td><a class="isearch-release-link" href="${escHtml(r.link)}" target="_blank" rel="noopener">${escHtml(r.title)}</a>${batchTag}${trustedTag}</td>
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
        let data = {};
        try { data = await r.json(); } catch (_) {}
        if (!r.ok) throw new Error(data.message || 'Grab failed');
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

// ── Interactive batch search ───────────────────────────────────────────────
// Parallel flow to openInteractiveSearch but for batch releases. Shares the
// isearch-modal element so the UI only has one modal to style. The results
// render is nearly identical but routes its Grab action to /grab-batch
// instead of the per-episode /grab endpoint.
let _ibatchResults = [];

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
    let html = '<table class="interactive-search-table"><thead><tr><th class="col-score">Score</th><th>Release</th><th>Group</th><th class="col-quality">Quality</th><th class="col-size">Size</th><th class="col-seeds">Seeds</th><th class="col-grab">Grab</th></tr></thead><tbody>';
    results.forEach((r, idx) => {
        const batchTag = r.is_batch ? '<span class="tag tag-batch" style="margin-left:4px">batch</span>' : '';
        const trustedTag = r.is_trusted ? '<span class="tag tag-trusted" style="margin-left:4px">trusted</span>' : '';
        const scoreClass = r.score >= 80 ? 'score-high' : r.score >= 40 ? 'score-mid' : 'score-low';
        html += `<tr>
            <td class="col-score">${renderScoreDetails(r, scoreClass)}</td>
            <td><a class="isearch-release-link" href="${escHtml(r.link)}" target="_blank" rel="noopener">${escHtml(r.title)}</a>${batchTag}${trustedTag}</td>
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
        let data = {};
        try { data = await r.json(); } catch (_) {}
        if (!r.ok) throw new Error(data.message || 'Grab failed');
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

function setBusyButton(btn, busy, busyLabel) {
    if (!btn) return;
    btn.disabled = busy;
    btn.classList.toggle('is-loading', busy);
    const label = btn.querySelector('.btn-label');
    if (label) {
        if (!btn.dataset.originalLabel) btn.dataset.originalLabel = label.textContent;
        label.textContent = busy ? busyLabel : btn.dataset.originalLabel;
    }
}

const SEARCH_ICON_SVG = '<svg width="12" height="12" viewBox="0 0 24 24" fill="none" stroke="currentColor" stroke-width="2.5"><circle cx="11" cy="11" r="8"/><path d="M21 21l-4.35-4.35"/></svg>';
const SUCCESS_ICON_SVG = '<svg width="12" height="12" viewBox="0 0 24 24" fill="none" stroke="currentColor" stroke-width="2.5"><polyline points="20 6 9 17 4 12"/></svg>';
const ERROR_ICON_SVG = '<svg width="12" height="12" viewBox="0 0 24 24" fill="none" stroke="currentColor" stroke-width="2"><circle cx="12" cy="12" r="10"/><line x1="15" y1="9" x2="9" y2="15"/><line x1="9" y1="9" x2="15" y2="15"/></svg>';

function setEpisodeButtonState(btn, state, title) {
    if (!btn) return;
    btn.disabled = state === 'loading';
    btn.classList.remove('is-loading', 'is-success', 'is-error');
    const inner = btn.querySelector('.icon-btn-inner');
    if (state === 'loading') {
        btn.classList.add('is-loading');
        if (inner) inner.innerHTML = '<span class="ep-search-spinner"></span>';
        btn.title = title || 'Searching...';
    } else if (state === 'success') {
        btn.classList.add('is-success');
        if (inner) inner.innerHTML = SUCCESS_ICON_SVG;
        btn.title = title || 'Queued';
    } else if (state === 'error') {
        btn.classList.add('is-error');
        if (inner) inner.innerHTML = ERROR_ICON_SVG;
        btn.title = title || 'Search failed';
        setBusyButton(btn, false);
    } else {
        if (inner) inner.innerHTML = SEARCH_ICON_SVG;
        btn.title = title || btn.title;
        setBusyButton(btn, false);
    }
}

function autoSearchEpisode(episodeNumber, btn) {
    const seriesTitle = SD.titleEnglish || SD.titleRomaji || SD.titleNative || '';
    setEpisodeButtonState(btn, 'loading', `Searching episode ${episodeNumber}...`);
    const pid = window.ryokanNewProgressId();
    const toast = window.ryokanProgressToast({
        progressId: pid,
        kind: 'info',
        category: 'auto_search',
        title: `Searching episode ${episodeNumber}`,
        body: seriesTitle,
    });

    fetch(`/api/series/${SD.id}/auto-search/${episodeNumber}?progress_id=${encodeURIComponent(pid)}`, {
        method: 'POST',
        headers: {'Content-Type': 'application/json'}
    })
    .then(async resp => {
        let data = {};
        try { data = await resp.json(); } catch (_) {}
        if (!resp.ok) {
            throw new Error(data.message || 'Episode search failed');
        }
        const first = Array.isArray(data.grabbed) ? data.grabbed[0] : null;
        if (first) {
            setEpisodeButtonState(btn, 'success', `Queued: ${first.release_title}`);
            updateEpisodeRow(episodeNumber, 'grabbed', first.release_group);
            ensureDlPollRunning();
            refreshEpisodeRows({ force: true });
        } else {
            setEpisodeButtonState(btn, 'error', 'No matching release found');
        }
    })
    .catch(err => {
        setEpisodeButtonState(btn, 'error', err.message || 'Episode search failed');
        toast.finalize({
            kind: 'error',
            title: `Episode ${episodeNumber} search failed`,
            body: err && err.message ? err.message : 'Unknown error',
        });
    });
}

function autoSearchSeries(btn) {
    const seriesTitle = SD.titleEnglish || SD.titleRomaji || SD.titleNative || '';
    setBusyButton(btn, true, 'Searching…');
    const pid = window.ryokanNewProgressId();
    const toast = window.ryokanProgressToast({
        progressId: pid,
        kind: 'info',
        category: 'auto_search',
        title: 'Searching missing episodes',
        body: seriesTitle,
    });

    fetch(`/api/series/${SD.id}/auto-search?progress_id=${encodeURIComponent(pid)}`, {
        method: 'POST',
        headers: {'Content-Type': 'application/json'}
    })
    .then(async resp => {
        let data = {};
        try { data = await resp.json(); } catch (_) {}
        if (!resp.ok) {
            throw new Error(data.message || 'Auto search failed');
        }
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
            title: 'Auto search failed',
            body: err && err.message ? err.message : 'Unknown error',
        });
    });
}

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

let overrideTargetEpisode = null;

// Composite dropdown key ↔ backend quartet. Each entry maps the <select>'s
// value to the {source, is_remux, is_bdmv, web_kind} payload that the
// /api/library/manual-override handler expects. Centralising the mapping
// here means the HTML options and the POST body can't drift apart.
const OVERRIDE_SOURCE_MAP = {
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

function applyManualOverride() {
    if (overrideTargetEpisode == null) return;
    const dbId = parseInt(SD.dbId);
    if (!dbId) return;
    const key = document.getElementById('override-source').value;
    // Defensive fallback: the dropdown can only produce keys that
    // exist in OVERRIDE_SOURCE_MAP, but a future refactor could pass
    // a stale/garbage key through. Use `bluray` (matches the dropdown
    // default that actually exists) instead of `unknown` — the latter
    // would build `{source: 'Unknown'}` which the handler 400s on.
    const mapped = OVERRIDE_SOURCE_MAP[key] || OVERRIDE_SOURCE_MAP.bluray;
    const resolution = document.getElementById('override-resolution').value;
    const status = document.getElementById('override-status');
    if (status) status.textContent = 'Saving…';
    fetch('/api/library/manual-override', {
        method: 'POST',
        headers: {'Content-Type': 'application/json'},
        body: JSON.stringify({
            series_id: dbId,
            episode_number: overrideTargetEpisode,
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
    .then(_ => { location.reload(); })
    .catch(err => { if (status) status.textContent = err.message || 'Failed to apply override'; });
}

function clearManualOverride() {
    if (overrideTargetEpisode == null) return;
    const dbId = parseInt(SD.dbId);
    if (!dbId) return;
    const status = document.getElementById('override-status');
    if (status) status.textContent = 'Clearing…';
    fetch('/api/library/manual-override', {
        method: 'POST',
        headers: {'Content-Type': 'application/json'},
        body: JSON.stringify({
            series_id: dbId,
            episode_number: overrideTargetEpisode,
            source: '',
            resolution: '',
            is_remux: false,
            is_bdmv: false,
            web_kind: '',
        })
    })
    .then(async r => {
        let data = {};
        try { data = await r.json(); } catch (_) {}
        if (!r.ok) throw new Error(data.message || 'Failed to clear override');
        return data;
    })
    .then(_ => { location.reload(); })
    .catch(err => { if (status) status.textContent = err.message || 'Failed to clear override'; });
}

// Ad-hoc trigger for the per-episode full-pipeline classifier.
// Targets the same episode the modal is currently editing. Useful
// after the user edits Release Groups or a custom format and wants
// to see the new verdict without waiting up to 6h for the next
// library-classify sweep.
function reclassifyEpisode(btn) {
    if (overrideTargetEpisode == null) return;
    const dbId = parseInt(SD.dbId);
    if (!dbId) return;
    const status = document.getElementById('override-status');
    if (status) status.textContent = 'Re-classifying…';
    if (btn) btn.disabled = true;
    fetch('/api/library/reclassify-episode', {
        method: 'POST',
        headers: {'Content-Type': 'application/json'},
        body: JSON.stringify({
            series_id: dbId,
            episode_number: overrideTargetEpisode,
        })
    })
    .then(async r => {
        let text = await r.text();
        // The endpoint returns plain-text error bodies from Axum's
        // `Err((StatusCode, String))` shape, so surface the body
        // verbatim rather than trying to JSON-parse a 409.
        let data = {};
        try { data = JSON.parse(text); } catch (_) {}
        if (!r.ok) throw new Error(data.message || text || `Re-classify failed (HTTP ${r.status})`);
        return data;
    })
    .then(data => {
        if (status) {
            status.textContent = `→ ${data.quality_tag || 'unknown'} (conf ${Number(data.confidence || 0).toFixed(2)}${data.needs_review ? ', needs review' : ''})`;
        }
        // Reload so the episode row picks up the new tag everywhere it
        // renders (quality badge, override modal pre-selection, etc.).
        setTimeout(() => { location.reload(); }, 600);
    })
    .catch(err => {
        if (status) status.textContent = err.message || 'Re-classify failed';
        if (btn) btn.disabled = false;
    });
}

function setAllowUpgrades(allow) {
    const dbId = parseInt(SD.dbId);
    if (!dbId) return;
    const checkbox = document.getElementById('allow-upgrades');
    const status = document.getElementById('allow-upgrades-status');
    if (checkbox) checkbox.disabled = true;
    if (status) status.textContent = 'Saving…';

    fetch('/api/library/allow-upgrades', {
        method: 'POST',
        headers: {'Content-Type': 'application/json'},
        body: JSON.stringify({ series_id: dbId, allow: allow })
    })
    .then(async r => {
        let data = {};
        try { data = await r.json(); } catch (_) {}
        if (!r.ok) throw new Error(data.message || 'Failed to update upgrades toggle');
        return data;
    })
    .then(_ => {
        if (status) status.textContent = allow ? 'Upgrades enabled' : 'Upgrades paused for this series';
        if (checkbox) checkbox.disabled = false;
    })
    .catch(err => {
        if (status) status.textContent = err.message || 'Failed to update upgrades toggle';
        if (checkbox) {
            checkbox.checked = !allow;
            checkbox.disabled = false;
        }
    });
}

// Issue #28 PR E — toggle the per-series PT upgrade opt-in.
// Mirror of setAllowUpgrades; lives on the same page, hits the
// parallel /api/library/allow-pt-upgrades endpoint, reverts the
// checkbox state on failure so the UI never lies about what's
// persisted.
function setAllowPtUpgrades(allow) {
    const dbId = parseInt(SD.dbId);
    if (!dbId) return;
    const checkbox = document.getElementById('allow-pt-upgrades');
    const status = document.getElementById('allow-pt-upgrades-status');
    if (checkbox) checkbox.disabled = true;
    const originalHint = status ? status.textContent : '';
    if (status) status.textContent = 'Saving…';

    fetch('/api/library/allow-pt-upgrades', {
        method: 'POST',
        headers: {'Content-Type': 'application/json'},
        body: JSON.stringify({ series_id: dbId, allow: allow })
    })
    .then(async r => {
        let data = {};
        try { data = await r.json(); } catch (_) {}
        if (!r.ok) throw new Error(data.message || 'Failed to update PT-upgrades toggle');
        return data;
    })
    .then(_ => {
        if (status) status.textContent = allow
            ? 'PT-sourced upgrades enabled for this series.'
            : 'PT-sourced upgrades disabled — sweep will skip private-tracker candidates.';
        if (checkbox) checkbox.disabled = false;
    })
    .catch(err => {
        if (status) status.textContent = err.message || 'Failed to update PT-upgrades toggle';
        if (checkbox) {
            checkbox.checked = !allow;
            checkbox.disabled = false;
        }
        // Restore original hint after a beat so a transient error
        // doesn't permanently mask the default copy.
        setTimeout(() => { if (status && originalHint) status.textContent = originalHint; }, 4000);
    });
}

// #23 — Save per-series search overrides (Nyaa uploader + custom tokens).
// Empty inputs clear the override server-side so the series falls back
// to the global default in Settings → Quality.
function saveSeriesSearchOverrides(btn) {
    const dbId = parseInt(SD.dbId);
    if (!dbId) return;
    const tokens = (document.getElementById('series-custom-query-tokens')?.value || '').trim();
    const restrict = (document.getElementById('series-restrict-to-group')?.value || '').trim();
    const status = document.getElementById('series-search-overrides-status');
    btn.disabled = true;
    if (status) status.textContent = 'Saving…';

    fetch('/api/library/search-overrides', {
        method: 'POST',
        headers: {'Content-Type': 'application/json'},
        body: JSON.stringify({
            series_id: dbId,
            custom_query_tokens: tokens,
            restrict_to_uploader: restrict,
        }),
    })
    .then(async r => {
        let data = {};
        try { data = await r.json(); } catch (_) {}
        if (!r.ok) throw new Error(data.message || 'Failed to save overrides');
        return data;
    })
    .then(_ => {
        if (status) status.textContent = 'Saved';
        btn.disabled = false;
    })
    .catch(err => {
        if (status) status.textContent = err.message || 'Failed to save overrides';
        btn.disabled = false;
    });
}

function addSeries() {
    const btn = document.getElementById('btn-track');
    btn.disabled = true;
    btn.textContent = '...';

    fetch('/api/library/add', {
        method: 'POST',
        headers: {'Content-Type': 'application/json'},
        body: JSON.stringify({
            anilist_id: parseInt(SD.providerId),
            mal_id: SD.malId ? parseInt(SD.malId) : null,
            title: SD.titleEnglish || SD.titleRomaji || SD.titleNative,
            title_romaji: SD.titleRomaji,
            title_english: SD.titleEnglish,
            title_native: SD.titleNative,
            cover_url: SD.coverUrl,
            format: SD.format,
            status: SD.status,
            episodes: SD.episodes ? parseInt(SD.episodes) : null,
        })
    })
    .then(async r => {
        let data = {};
        try { data = await r.json(); } catch (_) {}
        if (!r.ok) throw new Error(data.message || 'Add failed');
        return data;
    })
    .then(data => {
        btn.textContent = 'Added';
        btn.className = 'btn btn-success';
        if (data.hydrating) {
            window.ryokanToast({
                kind: 'info',
                category: 'library',
                title: 'Added to library',
                body: 'Fetching metadata…',
            });
            setTimeout(() => location.reload(), 4000);
        } else {
            setTimeout(() => location.reload(), 400);
        }
    })
    .catch(err => {
        btn.textContent = 'Error';
        btn.className = 'btn btn-danger';
        window.ryokanToast({
            kind: 'error',
            category: 'library',
            title: 'Failed to add series',
            body: err && err.message ? err.message : 'Unknown error',
        });
    });
}

function removeSeries(dbId) {
    // Open the dedicated destructive modal. This is a one-off rather than
    // a ryokanConfirm call because library-remove needs to render concrete
    // stakes (path, file count, size) that the generic confirm primitive
    // can't represent. See the #remove-series-modal block above.
    const modal = document.getElementById('remove-series-modal');
    const cancelBtn = document.getElementById('remove-series-cancel');
    const closeBtn = document.getElementById('remove-series-close');
    const confirmBtn = document.getElementById('remove-series-confirm');

    function close() {
        modal.style.display = 'none';
        cancelBtn.removeEventListener('click', close);
        closeBtn.removeEventListener('click', close);
        confirmBtn.removeEventListener('click', doRemove);
        modal.removeEventListener('click', onBackdrop);
        document.removeEventListener('keydown', onKey);
    }
    function onBackdrop(ev) { if (ev.target === modal) close(); }
    function onKey(ev) { if (ev.key === 'Escape') close(); }
    function doRemove() {
        close();
        performRemoveSeries(dbId);
    }

    cancelBtn.addEventListener('click', close);
    closeBtn.addEventListener('click', close);
    confirmBtn.addEventListener('click', doRemove);
    modal.addEventListener('click', onBackdrop);
    document.addEventListener('keydown', onKey);
    modal.style.display = 'flex';
    // Focus Cancel by default so a stray Enter keypress can't
    // surprise-trigger the destructive action.
    setTimeout(function() { cancelBtn.focus(); }, 10);
}

function performRemoveSeries(dbId) {
    const btn = document.getElementById('btn-track');
    const originalText = btn.textContent;
    btn.disabled = true;
    btn.textContent = '...';

    fetch('/api/library/remove', {
        method: 'POST',
        headers: {'Content-Type': 'application/json'},
        body: JSON.stringify({id: dbId, delete_files: true})
    })
    .then(async r => {
        // Backend always replies with JSON now — success body is the
        // cleanup summary, error body is {ok:false, stage, message}.
        // Best-effort parse so a malformed body still shows *something*
        // useful instead of silently collapsing to a generic 'Error'.
        let data = {};
        try { data = await r.json(); } catch (_) {}
        if (!r.ok) {
            const stage = data.stage ? ' [' + data.stage + ']' : '';
            throw new Error((data.message || 'Remove failed') + stage);
        }
        return data;
    })
    .then(() => {
        window.location.href = '/';
    })
    .catch(err => {
        // Surface the real reason instead of the old generic 'Error'
        // button state — this was the bug where rss_seen's missing
        // ON DELETE CASCADE silently blocked every remove with
        // "FOREIGN KEY constraint failed" and the user had no way to
        // see it without devtools.
        window.ryokanAlert({
            title: 'Remove failed',
            body: err && err.message ? err.message : 'unknown error',
        });
        btn.disabled = false;
        btn.textContent = originalText;
    });
}
function updateEpisodeRow(epNum, state, group) {
    const rows = document.querySelectorAll('.episode-table tbody tr');
    for (const row of rows) {
        const numCell = row.querySelector('.ep-col-num');
        if (!numCell || parseInt(numCell.textContent.trim()) !== epNum) continue;

        if (state === 'grabbed') {
            row.classList.add('ep-row-queued');
            const qualityCell = row.querySelector('.ep-col-quality');
            if (qualityCell) {
                // Show a 0% progress bar immediately; the download poller will update it
                if (!qualityCell.dataset.originalHtml) {
                    qualityCell.dataset.originalHtml = qualityCell.innerHTML;
                }
                qualityCell.innerHTML = '<div class="dl-progress-wrap"><div class="dl-progress-bar"><div class="dl-progress-fill" style="width:0%"></div></div><span class="dl-progress-text">0.0%</span></div>';
            }
        } else if (state === 'deleted') {
            // Strip both have/queued so a cancelled pending row doesn't
            // stay visually styled as queued (grey progress-bar row)
            // after the text content switches to "Missing". The prior
            // version only removed `ep-row-have`, which left a cancelled
            // pending row wearing both `ep-row-queued` and
            // `ep-row-missing` simultaneously until the next force-
            // refresh arrived — hence the "cancel one visually cancels
            // all" / "stuck queued" marker.
            row.classList.remove('ep-row-have', 'ep-row-queued');
            row.classList.add('ep-row-missing');
            const statusCell = row.querySelector('.ep-col-status');
            if (statusCell) {
                statusCell.innerHTML = '<span class="ep-status-icon ep-missing" title="Missing"><svg width="14" height="14" viewBox="0 0 24 24" fill="none" stroke="currentColor" stroke-width="2"><circle cx="12" cy="12" r="10"/><line x1="15" y1="9" x2="9" y2="15"/><line x1="9" y1="9" x2="15" y2="15"/></svg></span>';
            }
            const qualityCell = row.querySelector('.ep-col-quality');
            if (qualityCell) {
                qualityCell.innerHTML = '<span class="ep-missing-label">Missing</span>';
                // Drop the cached `originalHtml` stash so future
                // showings of a progress bar on this row don't restore
                // the stale queued HTML.
                delete qualityCell.dataset.originalHtml;
            }
        }
        break;
    }
}

// --- Relations carousel scroll buttons ---
// The relations list is a horizontal-scroll flexbox. The native scrollbar is
// styled thin and is easy to miss, so we flank the cards with chevron buttons
// when the content actually overflows. At the scroll edges the buttons are
// kept in layout (via visibility:hidden in CSS) so the cards don't jitter as
// the user crosses the first or last position. For series whose relations
// already fit in view, the `.no-scroll` class removes the buttons entirely so
// short lists aren't padded with dead space.
(function initRelationsCarousel() {
    const EDGE_SLOP = 2; // pixels of tolerance for "reached the edge"
    document.querySelectorAll('.relations-section').forEach(section => {
        const row = section.querySelector('.relations-row');
        const track = section.querySelector('.relation-cards');
        const btnLeft = section.querySelector('.relation-scroll-btn-left');
        const btnRight = section.querySelector('.relation-scroll-btn-right');
        if (!row || !track || !btnLeft || !btnRight) return;

        const updateButtons = () => {
            const scrollable = track.scrollWidth > track.clientWidth + EDGE_SLOP;
            if (!scrollable) {
                row.classList.add('no-scroll');
                btnLeft.hidden = true;
                btnRight.hidden = true;
                return;
            }
            row.classList.remove('no-scroll');
            btnLeft.hidden = track.scrollLeft <= EDGE_SLOP;
            btnRight.hidden = track.scrollLeft >= track.scrollWidth - track.clientWidth - EDGE_SLOP;
        };

        const scrollByViewport = direction => {
            // Scroll by roughly one viewport minus a card, so the card at the
            // current edge remains visible as an anchor.
            const delta = Math.max(track.clientWidth - 152, 200) * direction;
            track.scrollBy({ left: delta, behavior: 'smooth' });
        };

        btnLeft.addEventListener('click', () => scrollByViewport(-1));
        btnRight.addEventListener('click', () => scrollByViewport(1));
        track.addEventListener('scroll', updateButtons, { passive: true });
        window.addEventListener('resize', updateButtons);

        updateButtons();
    });
})();

// --- Episode download progress polling ---
let dlPollTimer = null;
let dlPollActive = false;
let dlRefreshing = false;

function formatDlSpeed(bps) {
    if (bps <= 0) return '';
    const units = ['B/s', 'KB/s', 'MB/s', 'GB/s'];
    const i = Math.floor(Math.log(bps) / Math.log(1024));
    return (bps / Math.pow(1024, i)).toFixed(i > 0 ? 1 : 0) + ' ' + units[i];
}

function escapeHtml(s) {
    const div = document.createElement('div');
    div.textContent = (s == null ? '' : String(s));
    return div.innerHTML;
}

const STATUS_ICON_HAVE = '<span class="ep-status-icon ep-have" title="On disk"><svg width="14" height="14" viewBox="0 0 24 24" fill="none" stroke="currentColor" stroke-width="2.5"><polyline points="20 6 9 17 4 12"/></svg></span>';
const STATUS_ICON_MISSING = '<span class="ep-status-icon ep-missing" title="Missing"><svg width="14" height="14" viewBox="0 0 24 24" fill="none" stroke="currentColor" stroke-width="2"><circle cx="12" cy="12" r="10"/><line x1="15" y1="9" x2="9" y2="15"/><line x1="9" y1="9" x2="15" y2="15"/></svg></span>';
const DL_PROGRESS_HTML_ZERO = '<div class="dl-progress-wrap"><div class="dl-progress-bar"><div class="dl-progress-fill" style="width:0%"></div></div><span class="dl-progress-text">0.0%</span></div>';

// Sync the episode table with the server's authoritative state.
//
// By default this only touches rows that are currently showing a progress
// bar — the poll-path use case: when a download disappears from the
// progress response (post-processing finished moving the file, or the grab
// failed), we need to replace the bar with the real row state.
//
// Pass `{ force: true }` from mutation handlers (grab, delete, batch
// search) to patch every row, including rows that weren't previously
// showing a progress bar. That path also recomputes the season summary
// badge.
function refreshEpisodeRows(options) {
    const opts = options || {};
    const force = !!opts.force;
    if (!SD.dbId || dlRefreshing) return;
    dlRefreshing = true;
    fetch(`/api/series/${SD.id}/episodes`)
        .then(r => r.ok ? r.json() : null)
        .then(episodes => {
            if (!episodes) return;
            patchEpisodeRows(episodes, force);
            if (force) updateSeasonSummary(episodes);
        })
        .catch(() => {})
        .finally(() => { dlRefreshing = false; });
}

function patchEpisodeRows(episodes, force) {
    const byNum = {};
    for (const ep of episodes) byNum[ep.number] = ep;
    const rows = document.querySelectorAll('.episode-table tbody tr');
    for (const row of rows) {
        const numCell = row.querySelector('.ep-col-num');
        if (!numCell) continue;
        const n = parseInt(numCell.textContent.trim());
        const ep = byNum[n];
        if (!ep) continue;
        const qualityCell = row.querySelector('.ep-col-quality');
        if (!qualityCell) continue;

        const showingProgress = qualityCell.querySelector('.dl-progress-wrap') !== null;
        // Poll-path: only touch rows currently showing a progress bar.
        // Force-path: touch everything.
        if (!force && !showingProgress) continue;

        const statusCell = row.querySelector('.ep-col-status');

        if (ep.on_disk) {
            row.classList.remove('ep-row-missing', 'ep-row-queued');
            row.classList.add('ep-row-have');
            if (statusCell) statusCell.innerHTML = STATUS_ICON_HAVE;
            const quality = ep.quality || 'UNKNOWN';
            qualityCell.innerHTML = `<span class="tag tag-quality">${escapeHtml(quality)}</span>`;
            delete qualityCell.dataset.originalHtml;
        } else if (ep.quality_state === 'grabbed') {
            // Episode was just grabbed (or is still queued). Show a 0%
            // progress bar — the poller will update it once the
            // download client reports real progress.
            row.classList.remove('ep-row-have', 'ep-row-missing');
            row.classList.add('ep-row-queued');
            if (!showingProgress) {
                if (!qualityCell.dataset.originalHtml) {
                    qualityCell.dataset.originalHtml = qualityCell.innerHTML;
                }
                qualityCell.innerHTML = DL_PROGRESS_HTML_ZERO;
            }
        } else if (ep.quality_state === 'failed') {
            row.classList.remove('ep-row-queued', 'ep-row-have');
            row.classList.add('ep-row-missing');
            if (statusCell) statusCell.innerHTML = STATUS_ICON_MISSING;
            qualityCell.innerHTML = `<span class="tag tag-quality-failed">${escapeHtml(ep.quality || '')} ✗</span>`;
            delete qualityCell.dataset.originalHtml;
        } else {
            // Neither on disk, grabbed, nor failed — missing.
            row.classList.remove('ep-row-have', 'ep-row-queued');
            row.classList.add('ep-row-missing');
            if (statusCell) statusCell.innerHTML = STATUS_ICON_MISSING;
            // Don't blow away a stale progress bar unless force is set:
            // post-processing may simply not have run yet.
            if (force) {
                qualityCell.innerHTML = '<span class="ep-missing-label">Missing</span>';
                delete qualityCell.dataset.originalHtml;
            }
        }
    }
}

// Recompute the "N / total" season badge at the top of the episodes table
// from the server's episode list.
function updateSeasonSummary(episodes) {
    const onDisk = episodes.filter(ep => ep.on_disk).length;
    const total = episodes.length;
    const badge = document.querySelector('.season-header-left .season-badge');
    if (!badge) return;
    if (total > 0) {
        badge.textContent = `${onDisk} / ${total}`;
    } else {
        badge.textContent = `${onDisk} files`;
    }
    badge.classList.remove('season-badge-complete', 'season-badge-partial', 'season-badge-missing');
    if (total > 0 && onDisk >= total) {
        badge.classList.add('season-badge-complete');
    } else if (onDisk > 0) {
        badge.classList.add('season-badge-partial');
    } else {
        badge.classList.add('season-badge-missing');
    }
}

// Start the download-progress poller if it isn't already running. Called
// after any manual grab so newly-queued progress bars start ticking without
// waiting for a page reload.
function ensureDlPollRunning() {
    if (!SD.dbId || dlPollActive) return;
    dlPollActive = true;
    pollDownloadProgress();
    dlPollTimer = setInterval(pollDownloadProgress, 5000);
}

function pollDownloadProgress() {
    if (!SD.dbId) return;
    fetch(`/api/series/${SD.id}/download-progress`)
        .then(r => r.ok ? r.json() : [])
        .then(items => {
            items = items || [];
            const epMap = {};
            for (const item of items) {
                epMap[item.episode] = item;
            }

            const rows = document.querySelectorAll('.episode-table tbody tr');
            let needsRefresh = false;

            for (const row of rows) {
                const numCell = row.querySelector('.ep-col-num');
                if (!numCell) continue;
                const ep = parseInt(numCell.textContent.trim());
                const qualityCell = row.querySelector('.ep-col-quality');
                if (!qualityCell) continue;

                const item = epMap[ep];
                const showingProgress = qualityCell.querySelector('.dl-progress-wrap') !== null;

                if (item) {
                    if (!qualityCell.dataset.originalHtml) {
                        qualityCell.dataset.originalHtml = qualityCell.innerHTML;
                    }
                    const kind = item.state_kind || '';
                    const isComplete = kind.startsWith('seeding') || kind === 'paused-complete'
                        || kind === 'checking-seed' || item.progress >= 1.0;
                    if (isComplete) {
                        if (window.POST_PROCESSING_ENABLED) {
                            // Torrent finished but post-processing hasn't imported
                            // the release into the library yet. Show a full bar with
                            // an "Importing..." label so the user isn't staring at a
                            // stale 0.0% until the next post-processing tick.
                            qualityCell.innerHTML = '<div class="dl-progress-wrap"><div class="dl-progress-bar"><div class="dl-progress-fill" style="width:100%"></div></div><span class="dl-progress-text">Importing…</span></div>';
                        } else {
                            // Post-processing is off — there is no import step to
                            // wait on. The lightweight post-proc sweep
                            // (advance_state_without_import) flips the tag state
                            // to 'completed' on the next tick; trigger a refresh
                            // so the checkmark appears without flashing a
                            // spurious "Importing…" that'll never advance.
                            needsRefresh = true;
                        }
                    } else {
                        const pct = (item.progress * 100).toFixed(1);
                        qualityCell.innerHTML = `<div class="dl-progress-wrap"><div class="dl-progress-bar"><div class="dl-progress-fill" style="width:${pct}%"></div></div><span class="dl-progress-text">${pct}%</span></div>`;
                    }
                } else if (showingProgress) {
                    // Row was showing a progress bar but the download is no
                    // longer in the response — fetch fresh episode state.
                    needsRefresh = true;
                }
            }

            if (needsRefresh) {
                refreshEpisodeRows();
            }

            // Stop polling once everything is idle: nothing actively
            // downloading, nothing waiting to be patched, and no stale
            // progress bars left on the page.
            const stillShowingProgress = document.querySelector('.episode-table tbody .ep-col-quality .dl-progress-wrap') !== null;
            if (items.length === 0 && !needsRefresh && !stillShowingProgress) {
                if (dlPollActive) {
                    dlPollActive = false;
                    clearInterval(dlPollTimer);
                    dlPollTimer = null;
                }
            }
        })
        .catch(() => {});
}

// Start polling if the series is tracked
if (SD.dbId) {
    pollDownloadProgress();
    dlPollActive = true;
    dlPollTimer = setInterval(pollDownloadProgress, 5000);
}
