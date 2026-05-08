// Server state handoff: window.initialTitleLanguage is set inline in
// index.html before this file loads. Fall back to empty string so
// `getTitleByLang` below still has something to compare against.
var initialTitleLanguage = window.initialTitleLanguage || '';

// Live client-side library search. The full library is in the DOM
// after page load, so substring-matching titles + toggling display
// is faster than a server round-trip and avoids the page reflow. We
// only stamp `?search=foo` into the URL via replaceState so a
// hard reload still lands the user on the same filtered view via
// the server-side handler (no-JS fallback). The dropdowns and sort
// still submit-on-change because list-membership and score-sort
// genuinely need DB work.
//
// 250ms debounce on `input` so a fast typist doesn't trigger a
// full grid re-walk + replaceState per keystroke. The DOM filter is
// cheap on a few hundred series but a power user with thousands
// would feel the per-keystroke layout thrash from the
// `style.display = 'none'` writes.
var _liveSearchTimer = null;
function liveLibrarySearch(input) {
    if (_liveSearchTimer) clearTimeout(_liveSearchTimer);
    _liveSearchTimer = setTimeout(() => {
        _liveSearchTimer = null;
        _liveLibrarySearchImmediate(input);
    }, 250);
}

function _liveLibrarySearchImmediate(input) {
    const q = (input.value || '').trim().toLowerCase();
    const grid = document.getElementById('library-grid');
    if (!grid) return;

    let visibleCount = 0;
    grid.querySelectorAll('.series-card').forEach((card) => {
        // Match against all three title fields. They're in the DOM
        // as .title-option spans (title-switcher controls which one
        // is visible via CSS, but all three are searchable).
        const titles = card.querySelectorAll('.title-option');
        let matches = q === '';
        if (!matches) {
            for (let i = 0; i < titles.length; i++) {
                if (titles[i].textContent.toLowerCase().includes(q)) {
                    matches = true;
                    break;
                }
            }
        }
        card.style.display = matches ? '' : 'none';
        if (matches) visibleCount++;
    });

    // URL update without navigation. Other params (list, sort) are
    // preserved so a manual reload picks up everything.
    const url = new URL(window.location.href);
    if (q) url.searchParams.set('search', q);
    else url.searchParams.delete('search');
    window.history.replaceState(null, '', url);

    // Clear button visibility: track whether *any* filter is active,
    // not just search. The list filter is reflected in the URL.
    const hasListFilter = !!url.searchParams.get('list');
    const clear = document.querySelector('.library-filter-clear');
    if (clear) {
        const anyActive = !!q || hasListFilter;
        clear.classList.toggle('library-filter-clear-hidden', !anyActive);
        if (anyActive) {
            clear.removeAttribute('aria-hidden');
            clear.removeAttribute('tabindex');
        } else {
            clear.setAttribute('aria-hidden', 'true');
            clear.setAttribute('tabindex', '-1');
        }
    }

    // No-matches inline empty state. Built once and toggled — avoids
    // navigation while still telling the user their query found
    // nothing.
    let empty = document.getElementById('library-search-empty');
    if (visibleCount === 0 && q) {
        if (!empty) {
            empty = document.createElement('div');
            empty.id = 'library-search-empty';
            empty.className = 'empty-state';
            const p = document.createElement('p');
            p.textContent = 'No series match your search.';
            empty.appendChild(p);
            grid.parentNode.insertBefore(empty, grid.nextSibling);
        }
        empty.style.display = '';
        grid.style.display = 'none';
    } else if (empty) {
        empty.style.display = 'none';
        grid.style.display = '';
    }
}

function openAddModal() {
    document.getElementById('add-modal').style.display = 'flex';
    document.getElementById('anilist-query').focus();
}

function closeAddModal(e) {
    if (e && e.target !== document.getElementById('add-modal')) return;
    document.getElementById('add-modal').style.display = 'none';
    document.getElementById('anilist-results').innerHTML = '';
    document.getElementById('anilist-query').value = '';
}

function getTitleByLang(entry, lang) {
    if (lang === 'native') return entry.title_native || entry.title_romaji || entry.title_english || '';
    if (lang === 'romaji') return entry.title_romaji || entry.title_english || entry.title_native || '';
    return entry.title_english || entry.title_romaji || entry.title_native || '';
}

// Title-language switching for `.title-switcher` elements is handled by
// CSS via the `html[data-title-language]` attribute that base.html sets
// pre-paint. `getTitleByLang` above is still used for search results and
// modal titles that render via innerHTML rather than title-switcher spans.

// Per-search provider override. Defaults to AniList; the user can flip
// to MAL via the toggle in the Add Series modal. The selection is
// remembered across modal opens within the page session, but resets on
// a hard reload (no localStorage — explicit-each-session by design).
var currentSearchSource = 'al';

function setSearchSource(btn) {
    currentSearchSource = btn.dataset.source;
    document.querySelectorAll('.search-source-toggle .btn-pill').forEach(b => {
        b.classList.toggle('active', b === btn);
    });
}

function searchAnilist() {
    const q = document.getElementById('anilist-query').value.trim();
    if (!q) return;

    const container = document.getElementById('anilist-results');
    container.innerHTML = '<p class="loading-text">Searching titles...</p>';

    fetch(`/api/anilist/search?q=${encodeURIComponent(q)}&source=${currentSearchSource}`)
        .then(r => {
            if (!r.ok) {
                return r.text().then(msg => {
                    const trimmed = (msg || '').trim();
                    throw new Error(trimmed || `Search failed (HTTP ${r.status})`);
                });
            }
            return r.json();
        })
        .then(results => {
            if (!results.length) {
                container.innerHTML = '<p class="loading-text">No results found.</p>';
                return;
            }

            const lang = localStorage.getItem('titleLanguage') || initialTitleLanguage || 'english';
            container.innerHTML = results.map(r => {
                const title = getTitleByLang(r, lang);
                const subtitle = lang === 'english'
                    ? (r.title_romaji || r.title_native || '')
                    : (r.title_english || r.title_romaji || r.title_native || '');
                const eps = r.episodes ? `${r.episodes} eps` : '?';
                const isMal = r.source === 'mal';
                const sourceLabel = isMal ? 'MAL' : 'AniList';
                // External link to the provider page matching whichever
                // source served the row. AL rows use the AniList id.
                // MAL rows need `id_mal` — if a MAL-served row somehow
                // arrives without one (shouldn't happen from Jikan, but
                // defensively), fall back to rendering the cover/title
                // as plain non-link markup rather than an href pointing
                // at myanimelist.net with an AL id, which 404s.
                const externalHref = isMal
                    ? (r.id_mal ? `https://myanimelist.net/anime/${r.id_mal}` : null)
                    : `https://anilist.co/anime/${r.id}`;
                const coverMarkup = externalHref
                    ? `<a href="${escAttr(externalHref)}" target="_blank" rel="noopener" class="anilist-cover-link" title="Open on ${escAttr(sourceLabel)}">
                            <img src="${escAttr(r.cover_url)}" alt="" class="anilist-cover" loading="lazy">
                        </a>`
                    : `<img src="${escAttr(r.cover_url)}" alt="" class="anilist-cover" loading="lazy">`;
                const titleMarkup = externalHref
                    ? `<a href="${escAttr(externalHref)}" target="_blank" rel="noopener" class="anilist-title-link" title="Open on ${escAttr(sourceLabel)}">${escHtml(title)}</a>`
                    : escHtml(title);
                return `
                    <div class="anilist-result">
                        ${coverMarkup}
                        <div class="anilist-info">
                            <p class="anilist-title">${titleMarkup}</p>
                            <p class="anilist-subtitle">${escHtml(subtitle)}</p>
                            <div class="anilist-meta">
                                <span class="tag tag-res">${escHtml((r.format || 'TBA').replace(/_/g, ' '))}</span>
                                <span class="tag tag-res">${eps}</span>
                                <span class="tag tag-status-${escAttr((r.status || "").toLowerCase())}">${escHtml((r.status_display || r.status || "").replace(/_/g, " "))}</span>
                                <span>${escHtml(sourceLabel)}</span>
                            </div>
                        </div>
                        <button class="btn btn-primary btn-add" onclick="addSeries(${r.id}, this)" data-entry='${escAttr(JSON.stringify(r))}'>Add</button>
                    </div>
                `;
            }).join('');
        })
        .catch(err => {
            container.innerHTML = `<p class="loading-text">Error: ${escHtml(err.message)}</p>`;
        });
}

var _pendingSeriesId = null;
var _selectedMonitorMode = 'all';

function addSeries(anilistId, btn) {
    const entry = JSON.parse(btn.dataset.entry);
    btn.disabled = true;
    btn.textContent = '...';

    fetch('/api/library/add', {
        method: 'POST',
        headers: {'Content-Type': 'application/json'},
        body: JSON.stringify({
            anilist_id: entry.id,
            mal_id: entry.id_mal || null,
            title: entry.title_english || entry.title_romaji || entry.title_native,
            title_romaji: entry.title_romaji || '',
            title_english: entry.title_english || '',
            title_native: entry.title_native || '',
            cover_url: entry.cover_url,
            format: entry.format,
            status: entry.status,
            episodes: entry.episodes,
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
        btn.classList.add('btn-success');
        _pendingSeriesId = data.id;
        _selectedMonitorMode = 'all';
        const lang = localStorage.getItem('titleLanguage') || initialTitleLanguage || 'english';
        const title = getTitleByLang(entry, lang) || entry.title_romaji || entry.title_english || 'this series';
        document.getElementById('monitor-series-title').textContent = title;
        // Reset mode buttons
        document.querySelectorAll('.monitor-option-btn').forEach(b => b.classList.remove('active'));
        const allBtn = document.querySelector('.monitor-option-btn[data-mode="all"]');
        if (allBtn) allBtn.classList.add('active');
        // Close add modal and show monitoring modal
        document.getElementById('add-modal').style.display = 'none';
        document.getElementById('monitor-modal').style.display = 'flex';
    })
    .catch(() => {
        btn.textContent = 'Error';
        btn.classList.add('btn-error');
        btn.disabled = false;
    });
}

function selectMonitorMode(btn) {
    document.querySelectorAll('.monitor-option-btn').forEach(b => b.classList.remove('active'));
    btn.classList.add('active');
    _selectedMonitorMode = btn.dataset.mode;
}

function confirmMonitoring() {
    if (!_pendingSeriesId) { location.reload(); return; }
    const confirmBtn = document.getElementById('monitor-confirm-btn');
    confirmBtn.disabled = true;
    confirmBtn.textContent = '...';
    fetch('/api/library/monitoring', {
        method: 'POST',
        headers: {'Content-Type': 'application/json'},
        body: JSON.stringify({ series_id: _pendingSeriesId, monitor_mode: _selectedMonitorMode, auto_grab: true })
    })
    .then(() => location.reload())
    .catch(() => location.reload());
}

function escHtml(s) {
    if (!s) return '';
    const d = document.createElement('div');
    d.textContent = s;
    return d.innerHTML;
}

function escAttr(s) {
    if (!s) return '';
    return s.replace(/&/g,'&amp;').replace(/'/g,'&#39;').replace(/"/g,'&quot;').replace(/</g,'&lt;').replace(/>/g,'&gt;');
}

// ── Bulk select (issue #125) ────────────────────────────────────────
//
// Selection state lives client-side; refresh clears it (matches the
// user's mental model — "I selected some cards on this page", not "I
// have a persistent selection that survives reloads"). A `Set` keyed
// by integer series id; the card's `.selected` class is the visual
// projection of membership.

var bulkSelectedIds = new Set();
var bulkPendingMode = null;

function toggleSeriesSelect(event, seriesId) {
    // Stop propagation so the parent <a> (whose href is /series/<id>)
    // doesn't navigate when the user clicks the checkbox itself.
    event.stopPropagation();
    var card = document.getElementById('series-' + seriesId);
    if (!card) return;
    var checkbox = card.querySelector('.series-card-select');
    var checked = checkbox ? checkbox.checked : false;
    if (checked) {
        bulkSelectedIds.add(seriesId);
        card.classList.add('selected');
    } else {
        bulkSelectedIds.delete(seriesId);
        card.classList.remove('selected');
    }
    renderBulkToolbar();
}

function renderBulkToolbar() {
    var bar = document.getElementById('bulk-action-toolbar');
    var num = document.getElementById('bulk-action-count-num');
    if (!bar || !num) return;
    num.textContent = String(bulkSelectedIds.size);
    if (bulkSelectedIds.size > 0) {
        bar.hidden = false;
        bar.classList.add('visible');
    } else {
        bar.hidden = true;
        bar.classList.remove('visible');
    }
}

function clearBulkSelection() {
    bulkSelectedIds.forEach(function (id) {
        var card = document.getElementById('series-' + id);
        if (card) {
            card.classList.remove('selected');
            var cb = card.querySelector('.series-card-select');
            if (cb) cb.checked = false;
        }
    });
    bulkSelectedIds.clear();
    renderBulkToolbar();
}

function openBulkMonitorModal() {
    if (bulkSelectedIds.size === 0) return;
    var modal = document.getElementById('bulk-monitor-modal');
    var count = document.getElementById('bulk-monitor-count');
    var confirmBtn = document.getElementById('bulk-monitor-confirm-btn');
    if (count) count.textContent = String(bulkSelectedIds.size);
    bulkPendingMode = null;
    if (confirmBtn) confirmBtn.disabled = true;
    document.querySelectorAll('#bulk-monitor-options .monitor-option-btn').forEach(function (btn) {
        btn.classList.remove('active');
        btn.onclick = function () { selectBulkMonitorMode(btn); };
    });
    if (modal) modal.style.display = 'flex';
}

function closeBulkMonitorModal(event) {
    // Allow the close button + backdrop click + explicit programmatic
    // close to all hit this. The backdrop click sets event.target ===
    // event.currentTarget; the close button uses .btn-icon. Anything
    // else (e.g. clicks bubbling up from inside the panel) is ignored.
    if (event && event.target && event.currentTarget && event.target !== event.currentTarget) {
        if (!event.target.closest('.btn-icon') && !event.target.classList.contains('btn-secondary')) {
            return;
        }
    }
    var modal = document.getElementById('bulk-monitor-modal');
    if (modal) modal.style.display = 'none';
}

function selectBulkMonitorMode(btn) {
    document.querySelectorAll('#bulk-monitor-options .monitor-option-btn').forEach(function (b) {
        b.classList.remove('active');
    });
    btn.classList.add('active');
    bulkPendingMode = btn.dataset.mode;
    var confirmBtn = document.getElementById('bulk-monitor-confirm-btn');
    if (confirmBtn) confirmBtn.disabled = false;
}

function confirmBulkMonitor() {
    if (!bulkPendingMode || bulkSelectedIds.size === 0) return;
    var ids = Array.from(bulkSelectedIds);
    var mode = bulkPendingMode;
    var confirmBtn = document.getElementById('bulk-monitor-confirm-btn');
    if (confirmBtn) {
        confirmBtn.disabled = true;
        confirmBtn.textContent = 'Applying…';
    }
    fetch('/api/library/bulk/monitor', {
        method: 'POST',
        headers: { 'Content-Type': 'application/json' },
        body: JSON.stringify({ series_ids: ids, mode: mode })
    })
    .then(function (r) { return r.json(); })
    .then(function (outcome) {
        renderBulkOutcome(outcome, 'Monitor mode updated');
        // Page reload after the toast briefly displays. Per-episode
        // monitor flags are recomputed server-side; the index page
        // doesn't render those per-card so a partial DOM update would
        // be a wash. Future bulk actions that affect visible badges
        // (delete, upgrades) will refresh in place.
        setTimeout(function () { location.reload(); }, 600);
    })
    .catch(function (e) {
        if (confirmBtn) {
            confirmBtn.disabled = false;
            confirmBtn.textContent = 'Apply';
        }
        if (window.ryokanToast) {
            window.ryokanToast({ kind: 'error', title: 'Bulk update failed', body: (e && e.message) || 'Network error', log: true });
        }
    });
}

// Render a BulkOutcome ({ succeeded: [], failed: [{series_id, reason}] })
// as a toast. All-success → success toast. All-failure → error toast,
// selection preserved so the user can retry without re-selecting.
// Partial → warn toast; succeeded IDs cleared from selection,
// failed IDs remain selected so the user sees which ones to address.
//
// Reused across all bulk actions; the `verb` argument is the user-
// facing summary noun ("Monitor mode updated" / "Series deleted" / etc.).
function renderBulkOutcome(outcome, verb) {
    var ok = (outcome.succeeded || []).length;
    var bad = (outcome.failed || []).length;
    if (!window.ryokanToast) return;
    if (bad === 0) {
        window.ryokanToast({ kind: 'success', title: verb, body: ok + ' succeeded', log: false, duration: 3000 });
        var modal = document.getElementById('bulk-monitor-modal');
        if (modal) modal.style.display = 'none';
        clearBulkSelection();
    } else if (ok === 0) {
        var first = outcome.failed[0];
        var summary = bad + ' failed';
        if (first) summary += '; ' + (first.reason || 'unknown error');
        window.ryokanToast({ kind: 'error', title: verb + ' failed', body: summary, log: true });
    } else {
        var failedSet = new Set(outcome.failed.map(function (f) { return f.series_id; }));
        var stillSelected = new Set();
        bulkSelectedIds.forEach(function (id) {
            if (failedSet.has(id)) {
                stillSelected.add(id);
            } else {
                var c = document.getElementById('series-' + id);
                if (c) {
                    c.classList.remove('selected');
                    var cb = c.querySelector('.series-card-select');
                    if (cb) cb.checked = false;
                }
            }
        });
        bulkSelectedIds = stillSelected;
        renderBulkToolbar();
        window.ryokanToast({
            kind: 'warn',
            title: verb,
            body: ok + ' succeeded, ' + bad + ' failed (selection still shows the failures)',
            log: true,
            duration: 6000
        });
        var modal2 = document.getElementById('bulk-monitor-modal');
        if (modal2) modal2.style.display = 'none';
    }
}

// One-shot Esc-to-clear listener. The `__ryokan*` boot guard pattern
// keeps hx-boost re-execs of this file from accumulating duplicate
// handlers on every nav-back into /.
if (!window.__ryokanBulkSelectInit) {
    window.__ryokanBulkSelectInit = true;
    document.addEventListener('keydown', function (ev) {
        if (ev.key !== 'Escape') return;
        var modal = document.getElementById('bulk-monitor-modal');
        if (modal && modal.style.display !== 'none' && modal.style.display !== '') {
            modal.style.display = 'none';
            return;
        }
        if (bulkSelectedIds.size > 0) {
            clearBulkSelection();
        }
    });
}
