// Server state handoff: window.initialTitleLanguage is set inline in
// index.html before this file loads. Fall back to empty string so
// `getTitleByLang` below still has something to compare against.
const initialTitleLanguage = window.initialTitleLanguage || '';

// Live client-side library search. The full library is in the DOM
// after page load, so substring-matching titles + toggling display
// is faster than a server round-trip and avoids the page reflow. We
// only stamp `?search=foo` into the URL via replaceState so a
// hard reload still lands the user on the same filtered view via
// the server-side handler (no-JS fallback). The dropdowns and sort
// still submit-on-change because list-membership and score-sort
// genuinely need DB work.
function liveLibrarySearch(input) {
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
let currentSearchSource = 'al';

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

let _pendingSeriesId = null;
let _selectedMonitorMode = 'all';

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
