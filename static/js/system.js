// ── Logs tab ────────────────────────────────────────────────────────────

// Click-outside-to-close for the log-download dropdown. The .open
// toggle on the trigger button is enough to show the menu; this
// listener handles dismissal — clicking anywhere outside the menu
// (or on a menu item, which navigates) closes it.
document.addEventListener('click', function (ev) {
    const menu = document.getElementById('log-download-options');
    if (!menu || !menu.classList.contains('open')) return;
    // The trigger button is inside .log-download-menu — let its
    // own click open + immediately re-toggle (don't fight it). For
    // option clicks, the navigation closes the menu naturally; for
    // any other click, dismiss.
    if (ev.target.closest('.log-download-menu') && !ev.target.closest('.log-download-option')) {
        return;
    }
    menu.classList.remove('open');
});

let pollTimer = null;
// Initial "latest seen" log id is read from the first rendered row's
// data-id attribute (server-side Askama writes one on every <tr>) so the
// JS stays free of Askama templating. 0 when the logs tab isn't rendered.
let latestId = (function () {
    const firstRow = document.querySelector('#log-tbody tr[data-id]');
    return firstRow ? parseInt(firstRow.dataset.id, 10) || 0 : 0;
})();

function applyFilters() {
    const level = document.getElementById('filter-level').value;
    const category = document.getElementById('filter-category').value;
    const search = document.getElementById('filter-search').value;
    const params = new URLSearchParams({tab: 'logs', level});
    if (category) params.set('category', category);
    if (search) params.set('search', search);
    window.location.href = '/system?' + params.toString();
}

async function clearLogs() {
    const result = await window.ryokanConfirm({
        title: 'Clear logs',
        body: 'Clear all log entries?',
        yesLabel: 'Clear',
    });
    if (!result.ok) return;
    try {
        const r = await fetch('/api/logs/clear', {method: 'POST', headers: {'Content-Type': 'application/json'}});
        await r.json();
        location.reload();
    } catch (err) {
        console.error('Failed to clear logs:', err);
        window.ryokanToast({kind: 'error', title: 'Clear logs failed', body: err && err.message ? err.message : 'Unknown error'});
    }
}

function formatTimestamp(iso) {
    try {
        const d = new Date(iso + 'Z');
        const pad = n => String(n).padStart(2, '0');
        return `${d.getFullYear()}-${pad(d.getMonth()+1)}-${pad(d.getDate())} ${pad(d.getHours())}:${pad(d.getMinutes())}:${pad(d.getSeconds())}`;
    } catch (_) {
        return iso;
    }
}

function escapeHtml(s) {
    const d = document.createElement('div');
    d.textContent = s;
    return d.innerHTML;
}

function pollLogs() {
    const toggle = document.getElementById('poll-toggle');
    if (!toggle || !toggle.checked) return;

    const level = document.getElementById('filter-level').value;
    const category = document.getElementById('filter-category').value;
    const params = new URLSearchParams({after: latestId});
    if (level) params.set('level', level);
    if (category) params.set('category', category);

    fetch('/api/logs/poll?' + params.toString())
        .then(r => r.json())
        .then(entries => {
            if (!entries || !entries.length) return;
            const tbody = document.getElementById('log-tbody');
            const empty = document.querySelector('.logs-empty');
            if (empty) empty.remove();

            // Entries come newest-first; insert at top in order.
            for (let i = entries.length - 1; i >= 0; i--) {
                const e = entries[i];
                if (e.id > latestId) latestId = e.id;
                const tr = document.createElement('tr');
                tr.className = `log-row log-level-${e.level} log-row-new`;
                tr.dataset.id = e.id;
                tr.innerHTML = `
                    <td class="log-col-time" title="${escapeHtml(e.timestamp)}">${escapeHtml(e.timestamp)}</td>
                    <td class="log-col-level"><span class="log-badge log-badge-${e.level}">${escapeHtml(e.level)}</span></td>
                    <td class="log-col-cat">${escapeHtml(e.category)}</td>
                    <td class="log-col-msg">
                        <span class="log-message">${escapeHtml(e.message)}</span>
                        ${e.detail ? `<span class="log-detail" title="${escapeHtml(e.detail)}">${escapeHtml(e.detail)}</span>` : ''}
                    </td>`;
                tbody.insertBefore(tr, tbody.firstChild);
                // Remove new-row highlight after animation.
                setTimeout(() => tr.classList.remove('log-row-new'), 2000);
            }
        })
        .catch(() => {}); // Silently fail polling.
}

function startPolling() {
    if (pollTimer) clearInterval(pollTimer);
    pollTimer = setInterval(pollLogs, 3000);
}

function stopPolling() {
    if (pollTimer) { clearInterval(pollTimer); pollTimer = null; }
}

// Guarded: only wires up the poll toggle when the logs tab is rendered.
// On other tabs (#poll-toggle is absent) this is a silent no-op.
(function () {
    const pollToggle = document.getElementById('poll-toggle');
    if (!pollToggle) return;
    pollToggle.addEventListener('change', function () {
        if (this.checked) startPolling(); else stopPolling();
    });
    startPolling();
})();

// ── RSS tab ─────────────────────────────────────────────────────────────

function filterRssRows() {
    const search = (document.getElementById('rss-filter-search')?.value || '').toLowerCase().trim();
    const decision = document.getElementById('rss-filter-decision')?.value || 'all';
    const rows = document.querySelectorAll('#rss-decision-table tbody tr');
    rows.forEach(row => {
        const text = (row.dataset.rssText || '').toLowerCase();
        const rowDecision = row.dataset.rssDecision || '';
        const matchesDecision = decision === 'all' || rowDecision === decision;
        const matchesSearch = !search || text.includes(search);
        row.style.display = (matchesDecision && matchesSearch) ? '' : 'none';
    });
}

function runRssSync(btn) {
    const result = document.getElementById('rss-sync-result');
    btn.disabled = true;
    result.textContent = 'Syncing...';
    window.ryokanToast({kind: 'info', title: 'RSS sync running', body: 'Checking the feed for new episodes.'});
    fetch('/api/rss/sync', { method: 'POST', headers: {'Content-Type': 'application/json'} })
        .then(async r => {
            const data = await r.json();
            if (!r.ok) throw new Error(data.message || 'RSS sync failed');
            result.textContent = data.message || 'RSS sync finished.';
            // Queue across the reload so the toast survives the
            // navigation that re-renders the RSS decisions table.
            window.ryokanQueueToast({
                kind: 'success',
                title: 'RSS sync complete',
                body: data.message || 'Feed checked.',
            });
            setTimeout(() => window.location.reload(), 600);
        })
        .catch(err => {
            result.textContent = err.message;
            window.ryokanToast({
                kind: 'error',
                title: 'RSS sync failed',
                body: err && err.message ? err.message : 'Unknown error',
            });
        })
        .finally(() => { btn.disabled = false; });
}

// ── Scheduled tasks tab ─────────────────────────────────────────────────

function forceRunTask(btn, taskKey) {
    const endpoints = {
        rss_sync: '/api/rss/sync',
        metadata_refresh: '/api/tasks/metadata-refresh',
        cleanup: '/api/tasks/cleanup',
        post_processing: '/api/tasks/post-processing',
        library_classify: '/api/tasks/library-classify',
        upgrade_search: '/api/tasks/upgrade-search',
        anibridge_refresh: '/api/system/reload-anibridge',
        external_sync: '/api/tasks/external-sync',
    };
    const url = endpoints[taskKey];
    if (!url) {
        window.ryokanAlert({
            title: 'Unknown task',
            body: 'No run endpoint for task: ' + taskKey,
        });
        return;
    }
    btn.disabled = true;
    btn.textContent = 'Running...';
    fetch(url, { method: 'POST' })
        .then(r => r.json().then(data => ({ ok: r.ok, data })).catch(() => ({ ok: r.ok, data: null })))
        .then(({ ok, data }) => {
            // Queue across the reload — `location.reload()` below
            // tears down the DOM and a non-queued toast disappears
            // before the user can read it.
            if (data && data.message) {
                window.ryokanQueueToast({
                    kind: ok ? 'success' : 'error',
                    title: ok ? 'Task complete' : 'Task failed',
                    body: data.message,
                });
            } else if (!ok) {
                window.ryokanQueueToast({
                    kind: 'error',
                    title: 'Task failed',
                    body: 'The task did not report a reason.',
                });
            } else {
                window.ryokanQueueToast({
                    kind: 'success',
                    title: 'Task complete',
                    body: taskKey + ' finished.',
                });
            }
            location.reload();
        })
        .catch(err => {
            window.ryokanToast({
                kind: 'error',
                title: 'Task error',
                body: err && err.message ? err.message : String(err),
            });
        })
        .finally(() => { btn.disabled = false; btn.textContent = 'Run now'; });
}

// ── Debug tab ───────────────────────────────────────────────────────────

// Debug-tab fetch helper: all four buttons share the same toast shape —
// info toast on start, success/error toast on completion — with the
// disabled button as the only in-flight indicator. No inline result span.
//
// The long-running actions (metadata rebuild, library classify, …) run
// detached on the server via `tokio::spawn`, so their server-side work
// continues even if the client navigates away. The browser, however,
// still aborts its own in-flight fetch on navigation, which used to
// fire a misleading "Rebuild failed / NetworkError" toast that then
// got persisted back to the server logs via `/api/logs/client`.
//
// Wire up an AbortController to the fetch and trip it on
// `beforeunload` / `pagehide`: when the catch fires, `signal.aborted`
// tells us the abort was ours (navigation) vs. a real network/server
// failure, and we skip the toast on the navigation case.
function runDebugAction(btn, opts) {
    btn.disabled = true;
    window.ryokanToast({
        kind: 'info',
        title: opts.startTitle,
        body: opts.startBody || '',
    });
    const controller = new AbortController();
    const onLeaving = () => controller.abort();
    // `pagehide` is the reliable signal across browsers — `beforeunload`
    // is deliberately skipped by some (iOS Safari) and blocked by BFCache.
    // Register both; whichever fires first wins.
    window.addEventListener('beforeunload', onLeaving, { once: true });
    window.addEventListener('pagehide', onLeaving, { once: true });
    fetch(opts.url, {
        method: 'POST',
        headers: {'Content-Type': 'application/json'},
        signal: controller.signal,
    })
    .then(async r => {
        const data = await r.json();
        if (!r.ok) throw new Error(data.message || opts.failureTitle);
        window.ryokanToast({
            kind: 'success',
            title: opts.successTitle,
            body: data.message || opts.successBody || '',
        });
    })
    .catch(err => {
        if (controller.signal.aborted) {
            // Browser cancelled our own fetch because the user is
            // navigating away. The server-side work continues
            // detached; don't show a misleading failure toast.
            return;
        }
        window.ryokanToast({
            kind: 'error',
            title: opts.failureTitle,
            body: err && err.message ? err.message : 'Unknown error',
        });
    })
    .finally(() => {
        window.removeEventListener('beforeunload', onLeaving);
        window.removeEventListener('pagehide', onLeaving);
        btn.disabled = false;
    });
}

function reconcileFallbacks(btn) {
    runDebugAction(btn, {
        url: '/api/library/reconcile-fallbacks',
        startTitle: 'Reconciling fallback entries',
        successTitle: 'Reconciliation complete',
        successBody: 'Fallback reconciliation complete.',
        failureTitle: 'Reconciliation failed',
    });
}

async function rebuildAniListCache(btn) {
    const confirmed = await window.ryokanConfirm({
        title: 'Rebuild metadata cache',
        body: 'Rebuild cached metadata, relations, episode data, and artwork for tracked series using the best currently available provider data? This can use MAL/Jikan fallback when AniList is unavailable.',
        yesLabel: 'Rebuild',
    });
    if (!confirmed.ok) return;
    runDebugAction(btn, {
        url: '/api/system/rebuild-anilist-cache',
        startTitle: 'Rebuilding metadata cache',
        startBody: 'This can take a while for large libraries.',
        successTitle: 'Metadata cache rebuilt',
        successBody: 'Metadata cache rebuild complete.',
        failureTitle: 'Rebuild failed',
    });
}

async function classifyLibrary(btn) {
    const confirmed = await window.ryokanConfirm({
        title: 'Classify library',
        body: 'Run the source/resolution classifier on every tracked series folder? Files that already have a structured classification row are skipped. This can take a while for large libraries because it runs ffprobe on each unclassified file.',
        yesLabel: 'Classify',
    });
    if (!confirmed.ok) return;
    runDebugAction(btn, {
        url: '/api/tasks/library-classify',
        startTitle: 'Classifying imported files',
        startBody: 'Running ffprobe on unclassified files.',
        successTitle: 'Library classify complete',
        successBody: 'Library classify complete.',
        failureTitle: 'Library classify failed',
    });
}

async function clearRssHistory(btn) {
    const confirmed = await window.ryokanConfirm({
        title: 'Clear RSS history',
        body: 'Clear all RSS grab history? Previously grabbed episodes will be re-evaluated on the next RSS sync.',
        yesLabel: 'Clear',
    });
    if (!confirmed.ok) return;
    runDebugAction(btn, {
        url: '/api/rss/clear-history',
        startTitle: 'Clearing grab history',
        successTitle: 'Grab history cleared',
        successBody: 'Grab history cleared.',
        failureTitle: 'Clear failed',
    });
}
