// ── localStorage search options persistence (always-present block) ─────

(function () {
    const fields = ['search-category', 'search-filter', 'search-user'];
    const KEY = 'nyaa_search_opts';
    const saved = JSON.parse(localStorage.getItem(KEY) || '{}');
    for (const id of fields) {
        if (saved[id] !== undefined && saved[id] !== '') {
            const el = document.getElementById(id);
            if (el) el.value = saved[id];
        }
    }
    const form = document.getElementById('search-form');
    if (form) {
        form.addEventListener('submit', function () {
            const opts = {};
            for (const id of fields) {
                const el = document.getElementById(id);
                if (el) opts[id] = el.value;
            }
            localStorage.setItem(KEY, JSON.stringify(opts));
        });
    }
})();

// ── Results-present block (load-more + grab) ────────────────────────────
//
// The original template rendered this block only when {% if searched %},
// and initialized `hasMore` / `totalResults` via Askama-templated number
// literals. The extracted version is always loaded; it reads the same
// server state from `window.searchState` (set inline in the template
// right before this file loads) and gates execution on the presence of
// the `#results-body` element.

var searchState = window.searchState || { hasMore: false, totalResults: 0, searched: false };
var nextPage = 2;
var hasMore = !!searchState.hasMore;
var totalResults = Number(searchState.totalResults) || 0;

// Handle prefill from library "Search Nyaa" button.
(function () {
    const params = new URLSearchParams(window.location.search);
    const prefill = params.get('prefill');
    if (prefill) {
        const input = document.getElementById('search-query');
        if (input) input.value = prefill;
        if (!searchState.searched) {
            // Auto-submit the form so results load immediately.
            const form = document.getElementById('search-form');
            if (form) form.submit();
        }
    }
})();

function getSearchParams() {
    return new URLSearchParams({
        query: document.getElementById('search-query').value,
        category: document.getElementById('search-category').value,
        filter: document.getElementById('search-filter').value,
        user: document.getElementById('search-user').value,
    });
}

function loadMore() {
    if (!hasMore) return;

    const btn = document.getElementById('load-more-btn');
    const status = document.getElementById('load-more-status');
    btn.disabled = true;
    btn.textContent = `Loading page ${nextPage}...`;

    const params = getSearchParams();
    params.set('p', nextPage);

    fetch(`/api/search/page?${params}`)
        .then(r => r.json())
        .then(data => {
            const results = data.results || [];
            hasMore = data.has_next;

            if (results.length === 0) {
                hasMore = false;
                document.getElementById('load-more-area').style.display = 'none';
                status.textContent = `All ${totalResults} results loaded`;
                return;
            }

            const tbody = document.getElementById('results-body');
            const cards = document.getElementById('results-cards');
            for (const r of results) {
                let rowClass = '';
                if (r.is_batch && r.is_trusted) rowClass = 'is-batch is-trusted';
                else if (r.is_batch) rowClass = 'is-batch';
                else if (r.is_trusted) rowClass = 'is-trusted';

                let scoreClass = r.score >= 60 ? 'score-high' : r.score >= 30 ? 'score-mid' : 'score-low';
                let tags = '';
                if (r.is_batch) tags += '<span class="tag tag-batch">BATCH</span>';
                if (r.is_trusted) tags += '<span class="tag tag-trusted">TRUSTED</span>';
                if (r.group) tags += `<span class="tag tag-group">${escHtml(r.group)}</span>`;
                if (r.resolution) tags += `<span class="tag tag-res">${escHtml(r.resolution)}p</span>`;

                const grabUrl = r.magnet || r.torrent || '';
                const grabBtn = grabUrl ? `<button class="btn btn-grab" onclick="grabRelease('${escAttr(grabUrl)}', this)">Grab</button>` : '';

                const scoreBreakdownHtml = renderScoreBreakdown(r);
                const dateCell = r.upload_date
                    ? `<span data-ts="${escAttr(r.upload_date)}">${escHtml(r.upload_date)}</span>`
                    : '—';

                // Table row (desktop).
                const tr = document.createElement('tr');
                if (rowClass) tr.className = rowClass;
                // data-* attrs mirror the server-rendered rows so the
                // client-side column sort picks up paginated rows too.
                tr.dataset.score = r.score;
                tr.dataset.name = r.title;
                tr.dataset.size = r.size_bytes;
                tr.dataset.sizeHuman = r.size;
                tr.dataset.date = r.upload_date || '';
                tr.dataset.seeders = r.seeders;
                tr.dataset.leechers = r.leechers;
                tr.dataset.downloads = r.downloads;
                tr.dataset.infoHash = r.info_hash || '';
                tr.dataset.group = r.group || '';
                if (r.indexer_id != null) tr.dataset.indexerId = r.indexer_id;
                tr.innerHTML = `
                    <td class="col-score">
                        <details class="score-details" name="score-breakdown">
                            <summary class="score-badge ${scoreClass}" title="Click to see breakdown">${r.score}</summary>
                            ${scoreBreakdownHtml}
                        </details>
                    </td>
                    <td class="col-name">
                        <a href="${escAttr(r.link)}" target="_blank" rel="noopener">${escHtml(r.title)}</a>
                        <div class="result-tags">${tags}</div>
                    </td>
                    <td class="col-size">${escHtml(r.size)}</td>
                    <td class="col-date">${dateCell}</td>
                    <td class="col-seed"><span class="seed-count">${r.seeders}</span></td>
                    <td class="col-leech"><span class="leech-count">${r.leechers}</span></td>
                    <td class="col-dl"><span class="dl-count">${r.downloads}</span></td>
                    <td class="col-actions">${grabBtn}</td>
                `;
                tbody.appendChild(tr);

                // Card (mobile). Same data, different shape. Hidden above
                // --bp-phone via CSS; loadMore keeps both in sync so a
                // viewport resize post-load still renders correctly.
                if (cards) {
                    const card = document.createElement('div');
                    card.className = `result-card${rowClass ? ' ' + rowClass : ''}`;
                    card.dataset.name = r.title;
                    card.dataset.sizeHuman = r.size;
                    card.dataset.seeders = r.seeders;
                    card.dataset.infoHash = r.info_hash || '';
                    card.dataset.group = r.group || '';
                    if (r.indexer_id != null) card.dataset.indexerId = r.indexer_id;
                    card.innerHTML = `
                        <div class="result-card-header">
                            <details class="score-details" name="score-breakdown">
                                <summary class="score-badge ${scoreClass}" title="Click to see breakdown">${r.score}</summary>
                                ${scoreBreakdownHtml}
                            </details>
                            <a class="result-card-title" href="${escAttr(r.link)}" target="_blank" rel="noopener">${escHtml(r.title)}</a>
                        </div>
                        <div class="result-card-tags">${tags}</div>
                        <div class="result-card-footer">
                            <span class="result-card-meta">${escHtml(r.size)}</span>
                            <span class="result-card-meta"><span class="seed-count">${r.seeders}</span> S</span>
                            <span class="result-card-meta"><span class="leech-count">${r.leechers}</span> L</span>
                            ${grabBtn}
                        </div>
                    `;
                    cards.appendChild(card);
                }
            }

            totalResults += results.length;
            nextPage++;
            document.getElementById('results-count').textContent = `${totalResults} results`;
            status.textContent = `${totalResults} results total`;

            if (hasMore) {
                btn.disabled = false;
                btn.textContent = `Load page ${nextPage}`;
            } else {
                document.getElementById('load-more-area').style.display = 'none';
                status.textContent = `All ${totalResults} results loaded`;
            }
        })
        .catch(err => {
            btn.disabled = false;
            btn.textContent = `Load page ${nextPage}`;
            console.error('Load more failed:', err);
        });
}

function grabRelease(url, btn) {
    // Pull the row's data-* attributes so the backend can link the
    // grab to a library series (#6d) and so we can feed the file-
    // picker modal a useful header when we route that way. Falls
    // back to a URL-only grab when the button wasn't mounted inside
    // a result row — e.g. a caller from a different template.
    const row = btn.closest('tr[data-score]') || btn.closest('.result-card');
    const isBatch = !!(row && row.classList.contains('is-batch'));

    // Issue #83 — batch releases open the interactive file picker
    // unless the user's `grab_preview_mode` config is `never`, in
    // which case the Grab button takes the direct /api/grab path
    // for 1.3.0-style one-click behavior. Single-file releases
    // always bypass the picker (nothing to pick). If the picker
    // isn't available on this page (no modal DOM, no info_hash
    // on the row), fall through to the direct grab so the button
    // keeps working.
    const previewMode = (window.searchState && window.searchState.grabPreviewMode) || 'batches_only';
    const canPicker = isBatch
        && previewMode !== 'never'
        && typeof window.openGrabPicker === 'function'
        && row && row.dataset.infoHash;
    if (canPicker) {
        window.openGrabPicker(url, {
            title: row.dataset.name || '',
            size: row.dataset.sizeHuman || '',
            seeders: Number(row.dataset.seeders) || 0,
            group: row.dataset.group || '',
            infoHash: row.dataset.infoHash || '',
            // Carry the search-hit's batch classification through to
            // the preview POST so the backend's grab-row write uses
            // the listing's flag instead of a file-count proxy (which
            // mis-flags .mkv+.ass+.srt single-episode releases).
            isBatch: isBatch,
        });
        return;
    }

    btn.disabled = true;
    btn.textContent = '...';
    const payload = {url: url};
    if (row) {
        if (row.dataset.name) payload.title = row.dataset.name;
        if (row.dataset.infoHash) payload.info_hash = row.dataset.infoHash;
        payload.is_batch = isBatch;
        // Multi-client routing — round-trip the result's indexer_id so
        // the backend can dispatch through the per-indexer pin. Falls
        // through to the Nyaa pin server-side when absent.
        if (row.dataset.indexerId) payload.indexer_id = Number(row.dataset.indexerId);
    }
    fetch('/api/grab', {
        method: 'POST',
        headers: {'Content-Type': 'application/json'},
        body: JSON.stringify(payload),
    })
    .then(resp => {
        if (resp.ok) {
            btn.textContent = 'Sent';
            btn.classList.add('btn-success');
        } else {
            btn.textContent = 'Error';
            btn.classList.add('btn-error');
        }
    })
    .catch(() => {
        btn.textContent = 'Error';
        btn.classList.add('btn-error');
    });
}

function escHtml(s) {
    const d = document.createElement('div');
    d.textContent = s == null ? '' : s;
    return d.innerHTML;
}

function escAttr(s) {
    return String(s == null ? '' : s).replace(/'/g, "\\'").replace(/"/g, '&quot;');
}

// Build the <div class="score-components"> panel content for a result,
// matching the server-rendered shape in templates/search.html so a row
// appended via loadMore() behaves identically to a page-1 row. Keeping
// the two paths in lockstep is load-bearing for the sort+expand UX.
function renderScoreBreakdown(r) {
    const parts = r.score_breakdown || [];
    let inner;
    if (parts.length === 0) {
        inner = `<div class="form-hint">No components fired (score stayed at 0).</div>`;
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
        inner = `<ul>${lis}</ul>
            <div class="form-hint">CF contributions shown here are evaluated against the release's classification alone. SeaDex-based CFs need a tracked AniList series to resolve, so they never fire on the manual search page — open the series page's interactive search for the full breakdown.</div>`;
    }
    return `<div class="score-components">
        <div class="score-components-title">Base score breakdown</div>
        ${inner}
    </div>`;
}

// #1.3.0 — close any open <details class="score-details"> when the user
// clicks outside it or presses Escape. Without these, the only way to
// dismiss the expander is to click the score badge itself, which is a
// footgun on the mobile card layout where the score sits in a small
// target at the card's top-left corner.
//
// Scroll-only edge handling: when the panel opens near the viewport
// edge we apply `position: fixed` with a viewport-aware `max-height` +
// internal `overflow-y: auto` so long breakdowns scroll inside the
// panel instead of falling off-screen. Width is capped to the viewport
// so mobile layouts don't overflow horizontally either. No flip-above
// logic — one direction is easier to reason about and predictable for
// both keyboard and touch users.
(function () {
    if (window.__ryokanSearchScoreBreakdownInit) return;
    window.__ryokanSearchScoreBreakdownInit = true;
    function closeAllOpenBreakdowns(except) {
        document.querySelectorAll('details.score-details[open]').forEach(function (d) {
            if (d !== except) d.removeAttribute('open');
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
    function positionPanel(details) {
        const panel = details.querySelector('.score-components');
        if (!panel) return;
        // Only lift the panel to fixed-positioning when it lives
        // inside an overflow-clipping ancestor (e.g. the interactive-
        // search modal). On the plain /search page the results table
        // is a direct descendant of the viewport, so CSS
        // `position: absolute; top: calc(100% + 6px); left: 0` anchored
        // to the `.score-details` summary is the correct placement —
        // it rides scroll with the row and keeps the CSS max-width
        // cap intact. An earlier blanket fixed-positioning here
        // stretched the panel to the full viewport width and left it
        // floating in place after a scroll.
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

        const GAP = 6;
        const MARGIN = 8;
        const rect = details.getBoundingClientRect();
        const vw = window.innerWidth;
        const vh = window.innerHeight;

        const top = rect.bottom + GAP;
        const maxHeight = Math.max(120, vh - top - MARGIN);
        const maxWidth = Math.max(240, vw - 2 * MARGIN);
        const desiredWidth = Math.min(360, maxWidth);
        let left = rect.left;
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
    // `toggle` doesn't bubble, so we capture it.
    document.addEventListener('toggle', function (evt) {
        const d = evt.target;
        if (!(d instanceof HTMLDetailsElement)) return;
        if (!d.classList.contains('score-details')) return;
        if (d.open) positionPanel(d);
        else {
            const panel = d.querySelector('.score-components');
            if (panel) resetPanelPosition(panel);
        }
    }, true);
})();

// #6a — click-to-sort columns on the results table. Each row carries
// data-* attributes populated server-side (data-score, data-name,
// data-size, data-date, data-seeders, data-leechers, data-downloads).
// Clicking a sortable header toggles between asc/desc and re-orders
// the tbody rows in place. State is purely client-side — no URL
// params, no server round-trip.
(function () {
    function parseValue(raw, key) {
        if (raw == null) return null;
        // Numeric columns.
        if (key === 'score' || key === 'size' || key === 'seeders' || key === 'leechers' || key === 'downloads') {
            const n = parseFloat(raw);
            return isNaN(n) ? 0 : n;
        }
        // Date is sortable as a string in "YYYY-MM-DD HH:MM" shape;
        // empty → sort last.
        if (key === 'date') {
            return raw || '';
        }
        // Name — case-insensitive string compare.
        return String(raw).toLowerCase();
    }

    function sortRows(tbody, key, dir) {
        const rows = Array.from(tbody.children);
        const sign = dir === 'asc' ? 1 : -1;
        rows.sort(function (a, b) {
            const av = parseValue(a.dataset[key], key);
            const bv = parseValue(b.dataset[key], key);
            // Empty-date rows sort to the end regardless of direction.
            if (key === 'date') {
                if (!av && !bv) return 0;
                if (!av) return 1;
                if (!bv) return -1;
            }
            if (av < bv) return -1 * sign;
            if (av > bv) return 1 * sign;
            return 0;
        });
        const frag = document.createDocumentFragment();
        rows.forEach(function (r) { frag.appendChild(r); });
        tbody.appendChild(frag);
    }

    function bindSortHandlers() {
        const table = document.getElementById('results-table');
        if (!table) return;
        const tbody = document.getElementById('results-body');
        if (!tbody) return;
        table.querySelectorAll('th.sortable').forEach(function (th) {
            // Per-th `dataset.bound` guard: under hx-boost the IIFE
            // re-runs and we'd otherwise add another click listener
            // every visit.
            if (th.dataset.sortBound === '1') return;
            th.dataset.sortBound = '1';
            th.addEventListener('click', function () {
                const key = th.dataset.sortKey;
                const wasAsc = th.classList.contains('sort-asc');
                const wasDesc = th.classList.contains('sort-desc');
                // Clear other headers.
                table.querySelectorAll('th.sortable').forEach(function (other) {
                    other.classList.remove('sort-asc', 'sort-desc');
                });
                // Flip direction: no current sort → desc for numeric
                // columns (more is usually better), asc for name/date.
                let dir;
                if (wasAsc) dir = 'desc';
                else if (wasDesc) dir = 'asc';
                else dir = (key === 'name' || key === 'date') ? 'asc' : 'desc';
                th.classList.add(dir === 'asc' ? 'sort-asc' : 'sort-desc');
                sortRows(tbody, key, dir);
            });
        });

        // Template ships `sort-desc` on the Score column as the default
        // visual state, but the server hands us rows in Nyaa's natural
        // order (upload date descending) — not sorted by score. Without
        // this initial sort the arrow lies about the column state,
        // which read as "sort-by-score gives weird results" on page
        // load. Run the same sort the click handler would so the
        // rendered rows match whatever initial class the server set.
        const initial = table.querySelector('th.sortable.sort-asc, th.sortable.sort-desc');
        if (initial) {
            const key = initial.dataset.sortKey;
            const dir = initial.classList.contains('sort-asc') ? 'asc' : 'desc';
            sortRows(tbody, key, dir);
        }
    }
    // Use the page-lifecycle helper so the bind fires AFTER htmx
    // settles each body swap. Direct script-execution-time binding
    // was racy under boost: the script tag runs as part of htmx's
    // swap evaluation and the table elements aren't always queryable
    // yet by the time the script reaches the binding code. The
    // lifecycle helper wires through `htmx.onLoad`, which fires
    // *after* the swap completes and the DOM is settled.
    //
    // `dataset.sortBound` per-th guard inside `bindSortHandlers`
    // makes the mount idempotent, so re-firing on every htmx.onLoad
    // (including on this same page if the user does an in-place
    // refresh of just the results table) doesn't accumulate listeners.
    if (typeof window.ryokanRegisterPageInit === 'function') {
        window.ryokanRegisterPageInit('search-sort', {
            check: function () { return !!document.getElementById('results-table'); },
            mount: bindSortHandlers,
        });
    } else if (document.readyState === 'loading') {
        document.addEventListener('DOMContentLoaded', bindSortHandlers);
    } else {
        bindSortHandlers();
    }
})();
