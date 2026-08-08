// Per-episode action buttons (monitor toggle, auto-search) + the
// shared button-state helpers they all use. Split out from series.js
// 2026-05-08 to address the "find one of 51 functions" complaint
// (PR #164 frontend review): each per-feature file is small enough
// to grep through. Cross-file references resolve at invocation time
// against globals (function declarations hoist), so the split is
// behavior-preserving.

function toggleMonitorAll(dbId, currentlyAllMonitored) {
    if (!dbId) return;
    const btn = document.getElementById('btn-monitor-all');
    const summary = document.getElementById('monitor-summary');
    const newMode = currentlyAllMonitored ? 'none' : 'all';
    if (btn) btn.disabled = true;
    if (summary) summary.textContent = 'Updating…';
    // Issue #166 — `/api/library/monitoring` switched from a Json<>
    // extractor to Form<> when the dropdown + add-modal callers
    // migrated to declarative HTMX. This call site keeps imperative
    // DOM updates (the bookmark toggle changes many .ep-mon-btn
    // elements at once, which doesn't fit HTMX's per-element swap),
    // so we send URL-encoded form data and read JSON back from the
    // handler's non-HTMX path. No `HX-Request` header → server picks
    // the JSON branch unchanged.
    fetch('/api/library/monitoring', {
        method: 'POST',
        headers: {'Content-Type': 'application/x-www-form-urlencoded'},
        body: new URLSearchParams({ series_id: dbId, monitor_mode: newMode }),
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
            monBtn.title = newState ? 'Monitored; click to unmonitor' : 'Not monitored; click to monitor';
        });
        // Update the monitor-all button
        if (btn) {
            btn.disabled = false;
            btn.classList.toggle('is-active', newState);
            btn.onclick = function() { toggleMonitorAll(dbId, newState); };
            btn.title = newState ? 'All monitored; click to unmonitor all' : 'Click to monitor all episodes';
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

// HTMX migration (issue #129) — toggleEpisodeMonitor() removed; the
// per-episode monitor button now uses `hx-post` directly. The handler
// at `/api/library/episode-monitoring` returns the swapped button HTML
// for HX-Request, JSON otherwise (preserving the API contract).

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

var SEARCH_ICON_SVG = '<svg width="12" height="12" viewBox="0 0 24 24" fill="none" stroke="currentColor" stroke-width="2.5"><circle cx="11" cy="11" r="8"/><path d="M21 21l-4.35-4.35"/></svg>';
var SUCCESS_ICON_SVG = '<svg width="12" height="12" viewBox="0 0 24 24" fill="none" stroke="currentColor" stroke-width="2.5"><polyline points="20 6 9 17 4 12"/></svg>';
var ERROR_ICON_SVG = '<svg width="12" height="12" viewBox="0 0 24 24" fill="none" stroke="currentColor" stroke-width="2"><circle cx="12" cy="12" r="10"/><line x1="15" y1="9" x2="9" y2="15"/><line x1="9" y1="9" x2="15" y2="15"/></svg>';

// How long to leave the success/error icon up before reverting to
// the default search icon. Picked to roughly match the toast
// auto-dismiss feel — long enough for the user to register the
// outcome, short enough that the button doesn't look "stuck" on a
// long-lived series page where they grabbed something an hour ago.
// Errors get a longer window so the user can read the tooltip.
var EPISODE_BTN_REVERT_SUCCESS_MS = 2500;
var EPISODE_BTN_REVERT_ERROR_MS = 4000;

function setEpisodeButtonState(btn, state, title) {
    if (!btn) return;
    // Cancel any pending auto-revert from a prior terminal state —
    // the new state takes over the button, and a leftover revert
    // would otherwise stomp it mid-display (e.g. user grabbed,
    // success → revert-pending; user grabs again 1s later → loading
    // briefly, then the original revert fires and resets back to
    // default while the new request is still in flight).
    if (btn._ryokanRevertTimer) {
        clearTimeout(btn._ryokanRevertTimer);
        btn._ryokanRevertTimer = null;
    }
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
        // Auto-revert: without this, the success checkmark sat on
        // the button forever (until F5). The persistence looked
        // intentional in a brief test session but felt stuck on a
        // page kept open across multiple grab cycles — user report
        // 2026-05-02. Stash the timer id on the element so the
        // top-of-function clear can cancel it on the next state
        // change.
        btn._ryokanRevertTimer = setTimeout(() => {
            btn._ryokanRevertTimer = null;
            setEpisodeButtonState(btn, 'default');
        }, EPISODE_BTN_REVERT_SUCCESS_MS);
    } else if (state === 'error') {
        btn.classList.add('is-error');
        if (inner) inner.innerHTML = ERROR_ICON_SVG;
        btn.title = title || 'Search failed';
        setBusyButton(btn, false);
        // Same auto-revert as success but with a longer window so
        // the user has time to read the tooltip explaining what
        // went wrong before the icon flips back to "search".
        btn._ryokanRevertTimer = setTimeout(() => {
            btn._ryokanRevertTimer = null;
            setEpisodeButtonState(btn, 'default');
        }, EPISODE_BTN_REVERT_ERROR_MS);
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
        title: 'Searching monitored episodes',
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
