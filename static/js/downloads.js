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

function stateLabel(state) {
    const map = {
        'uploading': 'Seeding', 'stalledUP': 'Seeding', 'forcedUP': 'Seeding',
        'downloading': 'Downloading', 'forcedDL': 'Downloading',
        'stalledDL': 'Stalled',
        'pausedDL': 'Paused', 'pausedUP': 'Paused',
        'queuedDL': 'Queued', 'queuedUP': 'Queued',
        'checkingDL': 'Checking', 'checkingUP': 'Checking',
        'error': 'Error', 'missingFiles': 'Missing Files',
        'moving': 'Moving', 'metaDL': 'Fetching metadata', 'allocating': 'Allocating',
    };
    return map[state] || state;
}

function stateBadgeClass(state) {
    if (['uploading', 'stalledUP', 'forcedUP', 'pausedUP'].includes(state)) return 'log-badge-info';
    if (['downloading', 'forcedDL'].includes(state)) return 'log-badge-debug';
    if (['pausedDL', 'queuedDL', 'queuedUP', 'stalledDL'].includes(state)) return 'log-badge-warn';
    if (['error', 'missingFiles'].includes(state)) return 'log-badge-error';
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
    if (!torrents || torrents.length === 0) {
        container.innerHTML = '<div class="logs-empty">No active downloads.</div>';
        return;
    }
    torrents.sort((a, b) => {
        const aDown = a.state.includes('DL') || a.state === 'downloading' ? 0 : 1;
        const bDown = b.state.includes('DL') || b.state === 'downloading' ? 0 : 1;
        if (aDown !== bDown) return aDown - bDown;
        return b.progress - a.progress;
    });
    let html = '<div class="rss-table-wrap"><table class="rss-table"><thead><tr>';
    html += '<th>Name</th><th>Size</th><th>Progress</th><th>Speed</th><th>ETA</th><th>Status</th><th>Actions</th>';
    html += '</tr></thead><tbody>';
    for (const t of torrents) {
        const pct = (t.progress * 100).toFixed(1);
        const isPaused = t.state.startsWith('paused');
        html += `<tr data-hash="${escapeHtml(t.hash)}">`;
        html += `<td><div class="dl-torrent-name">${escapeHtml(t.name)}</div></td>`;
        html += `<td>${formatSize(t.size)}</td>`;
        html += `<td><div class="dl-progress-wrap"><div class="dl-progress-bar"><div class="dl-progress-fill" style="width:${pct}%"></div></div><span class="dl-progress-text">${pct}%</span></div></td>`;
        html += `<td>${formatSpeed(t.dlspeed)}</td>`;
        html += `<td>${formatEta(t.eta)}</td>`;
        html += `<td><span class="log-badge ${stateBadgeClass(t.state)}">${stateLabel(t.state)}</span></td>`;
        html += `<td class="dl-actions">`;
        if (isPaused) {
            html += `<button class="btn btn-ghost btn-sm" onclick="resumeTorrent('${escapeHtml(t.hash)}')" title="Resume"><svg width="14" height="14" viewBox="0 0 24 24" fill="none" stroke="currentColor" stroke-width="2"><polygon points="5 3 19 12 5 21 5 3"/></svg></button>`;
        } else {
            html += `<button class="btn btn-ghost btn-sm" onclick="pauseTorrent('${escapeHtml(t.hash)}')" title="Pause"><svg width="14" height="14" viewBox="0 0 24 24" fill="none" stroke="currentColor" stroke-width="2"><rect x="6" y="4" width="4" height="16"/><rect x="14" y="4" width="4" height="16"/></svg></button>`;
        }
        html += `<button class="btn btn-ghost btn-sm" onclick="deleteTorrent('${escapeHtml(t.hash)}')" title="Remove"><svg width="14" height="14" viewBox="0 0 24 24" fill="none" stroke="currentColor" stroke-width="2"><polyline points="3 6 5 6 21 6"/><path d="M19 6l-1 14a2 2 0 01-2 2H8a2 2 0 01-2-2L5 6"/><path d="M10 11v6"/><path d="M14 11v6"/></svg></button>`;
        html += `</td></tr>`;
    }
    html += '</tbody></table></div>';
    container.innerHTML = html;
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
        body: 'Remove this torrent from qBittorrent?',
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
if (document.getElementById('queue-container')) {
    setInterval(loadQueue, 5000);
    loadQueue();
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

function removeFromBlocklist(id) {
    fetch('/api/downloads/blocklist/remove', {method: 'POST', headers: {'Content-Type': 'application/json'}, body: JSON.stringify({id})})
        .then(r => r.json())
        .then(data => {
            if (data.ok) {
                const row = document.getElementById('blocklist-row-' + id);
                if (row) row.remove();
            }
        })
        .catch(err => console.error('Failed to remove:', err));
}
