// ── Queue tab ───────────────────────────────────────────────────────────

function formatSize(bytes) {
    if (bytes <= 0) return '0 B';
    const units = ['B', 'KB', 'MB', 'GB', 'TB'];
    const i = Math.floor(Math.log(bytes) / Math.log(1024));
    return (bytes / Math.pow(1024, i)).toFixed(i > 0 ? 1 : 0) + ' ' + units[i];
}

function formatSpeed(bps) {
    if (bps <= 0) return '';
    return formatSize(bps) + '/s';
}

function formatEta(seconds) {
    if (seconds <= 0 || seconds >= 8640000) return '';
    const h = Math.floor(seconds / 3600);
    const m = Math.floor((seconds % 3600) / 60);
    const s = seconds % 60;
    if (h > 0) return h + 'h ' + m + 'm';
    if (m > 0) return m + 'm ' + s + 's';
    return s + 's';
}

// Keyed off the kebab-case `state_kind` slug from DownloadItemState,
// so the same label/badge vocabulary renders consistently across
// qBit, Deluge, Transmission, and rTorrent.
//
// A new Rust-side enum variant will serialize to a slug this map
// doesn't know about. The server-side tests lock the slug contract
// in place, but nothing forces the JS to stay in sync — so the
// fallback logs a devtools warning instead of silently shipping
// a raw kebab slug in the badge.
function stateLabel(kind) {
    const map = {
        'downloading': 'Downloading',
        'downloading-stalled': 'Stalled',
        'downloading-queued': 'Queued',
        'checking-download': 'Checking',
        'seeding': 'Seeding',
        'seeding-stalled': 'Seeding',
        'seeding-queued': 'Queued',
        'checking-seed': 'Checking',
        'paused': 'Paused',
        'paused-complete': 'Paused',
        'errored': 'Error',
    };
    if (map[kind] === undefined) {
        console.warn('[downloads] unmapped state_kind slug:', kind);
        return kind;
    }
    return map[kind];
}

function stateBadgeClass(kind) {
    if (['seeding', 'seeding-stalled', 'paused-complete'].includes(kind)) return 'log-badge-info';
    if (kind === 'downloading') return 'log-badge-debug';
    if (['downloading-stalled', 'downloading-queued', 'seeding-queued', 'paused'].includes(kind)) return 'log-badge-warn';
    if (kind === 'errored') return 'log-badge-error';
    return '';
}

function escapeHtml(s) {
    const d = document.createElement('div');
    d.textContent = s;
    return d.innerHTML;
}

function renderQueue(torrents) {
    const container = document.getElementById('queue-container');
    if (!container) return;
    // Capture scroll before the wholesale innerHTML replace below.
    // Without this, the 5s queue poll yanks the user back to the top
    // of the page mid-scroll because replacing the container's
    // children temporarily collapses its height to 0, scroll-anchoring
    // can't keep the prior anchor, and the document scroll position
    // resets. Restore after the swap.
    const prevScrollX = window.scrollX;
    const prevScrollY = window.scrollY;
    if (!torrents || torrents.length === 0) {
        container.innerHTML = '<div class="logs-empty">No active downloads.</div>';
        return;
    }
    const isDownloadingKind = (k) => k === 'downloading' || k === 'downloading-stalled'
        || k === 'downloading-queued' || k === 'checking-download';
    torrents.sort((a, b) => {
        const aDown = isDownloadingKind(a.state_kind) ? 0 : 1;
        const bDown = isDownloadingKind(b.state_kind) ? 0 : 1;
        if (aDown !== bDown) return aDown - bDown;
        return b.progress - a.progress;
    });
    let html = '<div class="rss-table-wrap"><table class="rss-table"><thead><tr>';
    html += '<th>Name</th><th>Size</th><th>Progress</th><th>Speed</th><th>ETA</th><th>Status</th><th>Actions</th>';
    html += '</tr></thead><tbody>';
    for (const t of torrents) {
        const pct = (t.progress * 100).toFixed(1);
        const isPaused = t.state_kind === 'paused' || t.state_kind === 'paused-complete';
        html += `<tr data-hash="${escapeHtml(t.hash)}">`;
        html += `<td><div class="dl-torrent-name">${escapeHtml(t.name)}</div></td>`;
        html += `<td>${formatSize(t.size)}</td>`;
        html += `<td><div class="dl-progress-wrap"><div class="dl-progress-bar"><div class="dl-progress-fill" style="width:${pct}%"></div></div><span class="dl-progress-text">${pct}%</span></div></td>`;
        html += `<td>${formatSpeed(t.dlspeed)}</td>`;
        html += `<td>${formatEta(t.eta)}</td>`;
        html += `<td><span class="log-badge ${stateBadgeClass(t.state_kind)}">${stateLabel(t.state_kind)}</span></td>`;
        html += `<td class="dl-actions">`;
        if (isPaused) {
            html += `<button class="btn btn-ghost btn-sm" onclick="resumeTorrent('${escapeHtml(t.hash)}')" title="Resume"><svg width="14" height="14" viewBox="0 0 24 24" fill="none" stroke="currentColor" stroke-width="2"><polygon points="5 3 19 12 5 21 5 3"/></svg></button>`;
        } else {
            html += `<button class="btn btn-ghost btn-sm" onclick="pauseTorrent('${escapeHtml(t.hash)}')" title="Pause"><svg width="14" height="14" viewBox="0 0 24 24" fill="none" stroke="currentColor" stroke-width="2"><rect x="6" y="4" width="4" height="16"/><rect x="14" y="4" width="4" height="16"/></svg></button>`;
        }
        // Copy infohash button — keep in sync with the template's
        // queue-row rendering in downloads.html. Without this, the
        // post-fetch JS render dropped the button so it visibly
        // flashed away on page load when loadQueue() ran immediately.
        html += `<button class="btn btn-ghost btn-sm" onclick="ryokanCopy('${escapeHtml(t.hash)}', this)" title="Copy infohash"><svg width="14" height="14" viewBox="0 0 24 24" fill="none" stroke="currentColor" stroke-width="2"><rect x="9" y="9" width="13" height="13" rx="2" ry="2"/><path d="M5 15H4a2 2 0 01-2-2V4a2 2 0 012-2h9a2 2 0 012 2v1"/></svg></button>`;
        html += `<button class="btn btn-ghost btn-sm" onclick="deleteTorrent('${escapeHtml(t.hash)}')" title="Remove"><svg width="14" height="14" viewBox="0 0 24 24" fill="none" stroke="currentColor" stroke-width="2"><polyline points="3 6 5 6 21 6"/><path d="M19 6l-1 14a2 2 0 01-2 2H8a2 2 0 01-2-2L5 6"/><path d="M10 11v6"/><path d="M14 11v6"/></svg></button>`;
        html += `</td></tr>`;
    }
    html += '</tbody></table></div>';
    container.innerHTML = html;
    // Restore the pre-swap scroll position. window.scrollTo with
    // {behavior: 'instant'} skips the smooth-scroll animation so a
    // poll tick doesn't visibly jiggle the page.
    window.scrollTo({left: prevScrollX, top: prevScrollY, behavior: 'instant'});
}

function loadQueue() {
    if (!document.getElementById('queue-container')) return;
    fetch('/api/torrents')
        .then(r => { if (!r.ok) throw new Error('Failed to load queue'); return r.json(); })
        .then(data => renderQueue(data))
        .catch(err => {
            const c = document.getElementById('queue-container');
            if (c) c.innerHTML =
                '<div class="logs-empty">Could not load queue: ' + escapeHtml(err.message) + '</div>';
        });
}

function pauseTorrent(hash) {
    fetch('/api/downloads/pause', {method: 'POST', headers: {'Content-Type': 'application/json'}, body: JSON.stringify({hash})})
        .then(() => setTimeout(loadQueue, 500));
}

function resumeTorrent(hash) {
    fetch('/api/downloads/resume', {method: 'POST', headers: {'Content-Type': 'application/json'}, body: JSON.stringify({hash})})
        .then(() => setTimeout(loadQueue, 500));
}

function deleteTorrent(hash) {
    window.ryokanConfirm({
        title: 'Remove torrent',
        body: 'Remove this torrent from the download client?',
        yesLabel: 'Remove',
        noLabel: 'Cancel',
        extras: [{id: 'deleteFiles', label: 'Also delete downloaded files', default: false}],
    }).then(function(res) {
        if (!res.ok) return;
        fetch('/api/downloads/delete', {
            method: 'POST',
            headers: {'Content-Type': 'application/json'},
            body: JSON.stringify({hash: hash, delete_files: !!res.extras.deleteFiles}),
        }).then(function() { setTimeout(loadQueue, 500); });
    });
}

// Only start the queue poller when the queue tab is rendered.
// Skip the immediate loadQueue() — the server-rendered queue table
// is already correct on first paint, so an immediate JS render just
// causes a visible flash (any markup divergence between the template
// and `renderQueue()` flickers as the JS render overwrites the
// container). The 5s interval handles live updates from there.
if (document.getElementById('queue-container')) {
    setInterval(loadQueue, 5000);
}

// ── History tab ─────────────────────────────────────────────────────────

function filterHistory() {
    const search = (document.getElementById('history-filter')?.value || '').toLowerCase().trim();
    const state = document.getElementById('history-state-filter')?.value || 'all';
    const rows = document.querySelectorAll('#history-table tbody tr');
    rows.forEach(row => {
        const text = (row.dataset.text || '').toLowerCase();
        const rowState = row.dataset.state || '';
        const matchesState = state === 'all' || rowState === state;
        const matchesSearch = !search || text.includes(search);
        row.style.display = (matchesState && matchesSearch) ? '' : 'none';
    });
}

// ── Blocklist tab ───────────────────────────────────────────────────────
// HTMX migration (issue #129) — removeFromBlocklist() removed; the
// row form's `hx-post` + `hx-target="closest tr"` + `hx-swap="outerHTML"`
// fires the request and strips the row in one declarative shot. The
// `data-ryokan-confirm-*` attrs route through the htmx:confirm bridge
// in base.js so cancelling leaves the row alone (same pattern as
// Phase 1 settings deletes).
