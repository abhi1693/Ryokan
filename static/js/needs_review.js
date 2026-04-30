// Bulk source-key → wire-shape map. Mirrors OVERRIDE_SOURCE_MAP in
// series.js. Kept duplicated rather than hoisted into base.js because
// most pages have no need for the override vocabulary; the duplication
// is short and grep-friendly.
//
// `var` (not `const`) at module scope is deliberate across every per-
// page JS file: htmx body-swap re-executes the inserted `<script>` tag
// when the user navigates back to a page they previously visited, but
// the original declarations still occupy the global scope. A `let` /
// `const` redeclaration is a *parser-stage* SyntaxError — the whole
// file is rejected, taking every event listener and
// `ryokanRegisterPageInit` call with it. `var` redeclares silently so
// the file evaluates fine on the second pass. The lifecycle helper
// handles per-page mount / unmount of pollers + listeners.
var REVIEW_OVERRIDE_SOURCE_MAP = {
    bluray_bdmv: { source: 'BluRay', is_remux: false, is_bdmv: true,  web_kind: '' },
    bluray_remux:{ source: 'BluRay', is_remux: true,  is_bdmv: false, web_kind: '' },
    bluray:      { source: 'BluRay', is_remux: false, is_bdmv: false, web_kind: '' },
    web:         { source: 'Web',    is_remux: false, is_bdmv: false, web_kind: '' },
    webrip:      { source: 'Web',    is_remux: false, is_bdmv: false, web_kind: 'WEBRip' },
    dvd:         { source: 'DVD',    is_remux: false, is_bdmv: false, web_kind: '' },
    hdtv:        { source: 'HDTV',   is_remux: false, is_bdmv: false, web_kind: '' },
    tv:          { source: 'TV',     is_remux: false, is_bdmv: false, web_kind: '' },
};

// ── Bulk actions ────────────────────────────────────────────────────
// Row checkbox + header "select all" + a sticky action bar that applies
// a chosen source/resolution to every selected row in one request via
// /api/library/bulk-manual-override. Rows fade and self-remove on
// success, matching the single-row flow.
(function () {
    const bar = document.getElementById('review-bulk-bar');
    const countEl = document.getElementById('review-bulk-count-n');
    const selectAll = document.getElementById('review-select-all');
    const applyBtn = document.getElementById('review-bulk-apply');
    const clearBtn = document.getElementById('review-bulk-clear');
    const bulkSource = document.getElementById('review-bulk-source');
    const bulkResolution = document.getElementById('review-bulk-resolution');
    if (!bar || !selectAll || !applyBtn) return;
    // Element-bound listeners (selectAll/clearBtn/applyBtn) attach to
    // these specific DOM nodes; on a hx-boost nav-back, these elements
    // are fresh nodes that need fresh listeners. The document-scoped
    // listeners (`change` / `keydown` event delegation) are gated by a
    // separate singleton guard further down — `document` persists
    // across body swaps so re-attaching there would accumulate.

    function rowChecks() {
        return Array.from(document.querySelectorAll('.review-row-check'));
    }
    function selectedRows() {
        return rowChecks().filter(cb => cb.checked).map(cb => cb.closest('tr'));
    }
    function refresh() {
        const n = selectedRows().length;
        countEl.textContent = n;
        bar.hidden = n === 0;
        const total = rowChecks().length;
        selectAll.checked = total > 0 && n === total;
        selectAll.indeterminate = n > 0 && n < total;
    }

    document.addEventListener('change', function (ev) {
        if (ev.target && ev.target.classList && ev.target.classList.contains('review-row-check')) {
            refresh();
        }
    });
    selectAll.addEventListener('change', function () {
        const on = selectAll.checked;
        rowChecks().forEach(cb => { cb.checked = on; });
        refresh();
    });
    clearBtn.addEventListener('click', function () {
        rowChecks().forEach(cb => { cb.checked = false; });
        refresh();
    });
    document.addEventListener('keydown', function (ev) {
        if (ev.key === 'Escape' && !bar.hidden) {
            rowChecks().forEach(cb => { cb.checked = false; });
            refresh();
        }
    });

    applyBtn.addEventListener('click', function () {
        const rows = selectedRows();
        if (rows.length === 0) return;
        const key = bulkSource.value;
        const mapped = REVIEW_OVERRIDE_SOURCE_MAP[key] || REVIEW_OVERRIDE_SOURCE_MAP.bluray;
        const resolution = bulkResolution.value;
        const items = rows.map(function (row) {
            return {
                series_id: parseInt(row.dataset.seriesId, 10),
                episode_number: parseInt(row.dataset.episode, 10),
                source: mapped.source,
                resolution: resolution,
                is_remux: mapped.is_remux,
                is_bdmv: mapped.is_bdmv,
                web_kind: mapped.web_kind,
            };
        });
        applyBtn.disabled = true;
        fetch('/api/library/bulk-manual-override', {
            method: 'POST',
            headers: {'Content-Type': 'application/json'},
            body: JSON.stringify({items: items}),
        })
        .then(async function (r) {
            let data = {};
            try { data = await r.json(); } catch (_) {}
            if (!r.ok) throw new Error(data.message || 'Bulk apply failed');
            return data;
        })
        .then(function (data) {
            const appliedIds = new Set();
            (data.failed || []).forEach(function (f) {
                appliedIds.add(f.series_id + ':' + f.episode_number);
            });
            // Fade out the rows that succeeded. Failed rows stay visible.
            rows.forEach(function (row) {
                const key = row.dataset.seriesId + ':' + row.dataset.episode;
                if (appliedIds.has(key)) return;
                row.style.transition = 'opacity 0.2s';
                row.style.opacity = '0';
                setTimeout(function () {
                    if (row.parentNode) row.parentNode.removeChild(row);
                    refresh();
                    const tbody = document.querySelector('.review-table tbody');
                    if (tbody && tbody.children.length === 0) {
                        // Swap to the pre-rendered empty-state placeholder
                        // instead of reloading — a reload would clobber the
                        // success toast that fired moments ago.
                        const list = document.querySelector('.review-list');
                        const emptyState = document.getElementById('review-empty-state');
                        if (list) list.hidden = true;
                        bar.hidden = true;
                        if (emptyState) emptyState.hidden = false;
                    }
                }, 200);
            });
            const kind = (data.failed && data.failed.length > 0) ? 'warn' : 'success';
            const title = data.applied + ' of ' + data.requested + ' applied';
            window.ryokanToast({
                kind: kind,
                title: title,
                body: data.failed && data.failed.length > 0 ? (data.failed.length + ' failed') : '',
                category: 'library',
            });
        })
        .catch(function (err) {
            window.ryokanToast({
                kind: 'error',
                title: 'Bulk apply failed',
                body: err.message || String(err),
                category: 'library',
            });
        })
        .finally(function () {
            applyBtn.disabled = false;
        });
    });

    refresh();
})();
