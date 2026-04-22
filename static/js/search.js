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

const searchState = window.searchState || { hasMore: false, totalResults: 0, searched: false };
let nextPage = 2;
let hasMore = !!searchState.hasMore;
let totalResults = Number(searchState.totalResults) || 0;

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

                // Table row (desktop).
                const tr = document.createElement('tr');
                if (rowClass) tr.className = rowClass;
                tr.innerHTML = `
                    <td class="col-score"><span class="score-badge ${scoreClass}">${r.score}</span></td>
                    <td class="col-name">
                        <a href="${escAttr(r.link)}" target="_blank" rel="noopener">${escHtml(r.title)}</a>
                        <div class="result-tags">${tags}</div>
                    </td>
                    <td class="col-size">${escHtml(r.size)}</td>
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
                    card.innerHTML = `
                        <div class="result-card-header">
                            <span class="score-badge ${scoreClass}">${r.score}</span>
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
    btn.disabled = true;
    btn.textContent = '...';
    fetch('/api/grab', {
        method: 'POST',
        headers: {'Content-Type': 'application/json'},
        body: JSON.stringify({url: url})
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
    d.textContent = s;
    return d.innerHTML;
}

function escAttr(s) {
    return s.replace(/'/g, "\\'").replace(/"/g, '&quot;');
}
