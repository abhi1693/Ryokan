// /calendar page lifecycle (issue #116). HTMX-driven swap:
//
//   1. Range tabs (this week / next week / month) carry hx-get
//      against the same /calendar URL. The handler branches on
//      HxRequest and returns the partials/calendar/list.html
//      partial, which htmx swaps into #calendar-list. No JS
//      involvement needed here — this file only re-applies post-
//      swap hydrators (time, today-highlight, local filters).
//
//   2. "Monitored only" toggle uses htmx.ajax() to fetch the
//      same partial with the new query string, keeping the URL
//      bar honest via hx-push-url semantics.
//
//   3. iCal subscribe modal opens on the page-header button and
//      composes the subscription URL by fetching the picked
//      key's plaintext via /api/api-keys/{id}/reveal (cookie-auth
//      gated same as Settings).
//
// `var` at module scope per CLAUDE.md hx-boost re-execution rule.

(function () {
    if (!document.getElementById('calendar-list')) return;

    function activeRange() {
        var t = document.querySelector('.calendar-range-tab.active');
        return t ? t.dataset.range : 'this_week';
    }

    function monitoredOnly() {
        var cb = document.getElementById('calendar-monitored-toggle');
        return cb ? cb.checked : false;
    }

    function fmtTime(unixTs) {
        try {
            var d = new Date(unixTs * 1000);
            return d.toLocaleTimeString(undefined, {
                hour: 'numeric',
                minute: '2-digit',
            });
        } catch (_) {
            return '';
        }
    }

    function calendarUrl() {
        var url = '/calendar?range=' + encodeURIComponent(activeRange());
        if (monitoredOnly()) url += '&monitored=true';
        return url;
    }

    // Patch each tab's `href` + `hx-get` to carry the current
    // monitored-only state forward, so toggling monitored then
    // clicking a range doesn't reset it.
    function syncRangeTabHrefs() {
        document.querySelectorAll('.calendar-range-tab').forEach(function (a) {
            var url = '/calendar?range=' + encodeURIComponent(a.dataset.range);
            if (monitoredOnly()) url += '&monitored=true';
            a.setAttribute('href', url);
            a.setAttribute('hx-get', url);
        });
    }

    // ── Monitored-only toggle (HTMX swap) ───────────────────────
    var monToggle = document.getElementById('calendar-monitored-toggle');
    if (monToggle && window.htmx) {
        monToggle.addEventListener('change', function () {
            syncRangeTabHrefs();
            // htmx.ajax with target+swap mirrors what hx-get would
            // do declaratively, plus pushUrl for back/forward.
            window.htmx.ajax('GET', calendarUrl(), {
                target: '#calendar-list',
                swap: 'innerHTML',
                pushUrl: true,
            });
        });
    }

    // ── iCal subscribe modal ────────────────────────────────────

    var icalBtn = document.getElementById('calendar-ical-btn');
    var modal = document.getElementById('calendar-subscribe-modal');
    var keyPicker = document.getElementById('calendar-subscribe-key');
    var urlInput = document.getElementById('calendar-subscribe-url');
    var copyBtn = document.getElementById('calendar-subscribe-copy-btn');
    var copyConfirm = document.getElementById('calendar-subscribe-copy-confirm');

    function refreshSubscribeUrl() {
        if (!keyPicker || !urlInput) return;
        var keyId = keyPicker.value;
        if (!keyId) {
            urlInput.value = '';
            return;
        }
        urlInput.value = 'Loading...';
        fetch('/api/api-keys/' + keyId + '/reveal', { credentials: 'same-origin' })
            .then(function (r) {
                if (!r.ok) throw new Error('reveal failed');
                return r.json();
            })
            .then(function (data) {
                if (!data || !data.plaintext) {
                    urlInput.value = '(failed to load key)';
                    return;
                }
                urlInput.value = window.location.origin
                    + '/api/calendar.ics?apikey='
                    + encodeURIComponent(data.plaintext);
            })
            .catch(function () {
                urlInput.value = '(failed to load key)';
            });
    }

    if (icalBtn && modal) {
        icalBtn.addEventListener('click', function () {
            modal.style.display = 'flex';
            refreshSubscribeUrl();
        });
    }
    if (keyPicker) keyPicker.addEventListener('change', refreshSubscribeUrl);

    document.querySelectorAll('[data-modal-close="calendar-subscribe-modal"]').forEach(function (el) {
        el.addEventListener('click', function () {
            if (modal) modal.style.display = 'none';
        });
    });
    if (modal) {
        modal.addEventListener('click', function (ev) {
            if (ev.target === modal) modal.style.display = 'none';
        });
    }

    if (copyBtn && urlInput) {
        copyBtn.addEventListener('click', function () {
            if (!urlInput.value || urlInput.value.startsWith('(')) return;
            navigator.clipboard.writeText(urlInput.value).then(function () {
                if (copyConfirm) {
                    copyConfirm.hidden = false;
                    setTimeout(function () { copyConfirm.hidden = true; }, 2000);
                }
            }, function () {
                urlInput.select();
                urlInput.setSelectionRange(0, 99999);
            });
        });
    }

    // ── Per-cell time hydration ─────────────────────────────────
    // Server renders `·` as a placeholder; we hydrate to the
    // user's local-time format on first paint and after every
    // HTMX swap.
    function hydrateTimes(root) {
        (root || document).querySelectorAll('.calendar-episode-time[data-airing-at]').forEach(function (el) {
            var ts = parseInt(el.dataset.airingAt, 10);
            if (ts > 0) el.textContent = fmtTime(ts);
        });
    }

    // ── Today highlight + scroll-to-today ───────────────────────
    function applyTodayHighlight(root) {
        var now = Math.floor(Date.now() / 1000);
        var todayUtcMidnight = now - (((now % 86400) + 86400) % 86400);
        var todayEl = null;
        (root || document).querySelectorAll('.calendar-day').forEach(function (el) {
            var dk = parseInt(el.dataset.dayKey, 10);
            var isToday = dk === todayUtcMidnight;
            el.classList.toggle('calendar-day-today', isToday);
            if (isToday) todayEl = el;
        });
        return todayEl;
    }

    function maybeScrollToToday(todayEl) {
        if (!todayEl || typeof todayEl.scrollIntoView !== 'function') return;
        // Defer to next frame so layout has settled.
        requestAnimationFrame(function () {
            todayEl.scrollIntoView({ behavior: 'auto', block: 'start' });
            window.scrollBy({ top: -68, left: 0, behavior: 'auto' });
        });
    }

    // ── Local filters: series-name search + premieres-only ──────
    var seriesSearchInput = document.getElementById('calendar-series-search');
    var premieresToggle = document.getElementById('calendar-premieres-toggle');

    function applyLocalFilters(root) {
        var query = seriesSearchInput ? seriesSearchInput.value.trim().toLowerCase() : '';
        var premieresOnly = premieresToggle ? premieresToggle.checked : false;

        (root || document).querySelectorAll('.calendar-day').forEach(function (day) {
            var visibleCount = 0;
            day.querySelectorAll('.calendar-episode').forEach(function (ep) {
                var name = ep.dataset.seriesName || '';
                var epNum = parseInt(ep.dataset.episode, 10) || 0;
                var matches = (!query || name.indexOf(query) !== -1)
                    && (!premieresOnly || epNum === 1);
                ep.classList.toggle('calendar-episode-hidden', !matches);
                if (matches) visibleCount++;
            });
            day.classList.toggle('calendar-day-hidden', visibleCount === 0);
            var countEl = day.querySelector('.calendar-day-label-count');
            if (countEl) {
                var totalEps = day.querySelectorAll('.calendar-episode').length;
                if (visibleCount < totalEps) {
                    countEl.textContent = visibleCount + ' / ' + totalEps + ' eps';
                } else {
                    countEl.textContent = totalEps + (totalEps === 1 ? ' ep' : ' eps');
                }
            }
        });
    }

    if (seriesSearchInput) {
        seriesSearchInput.addEventListener('input', function () { applyLocalFilters(); });
    }
    if (premieresToggle) {
        premieresToggle.addEventListener('change', function () { applyLocalFilters(); });
    }

    // Initial paint hydrators + first scroll-to-today.
    hydrateTimes();
    var initialToday = applyTodayHighlight();
    applyLocalFilters();
    maybeScrollToToday(initialToday);

    // ── Re-hydrate after every HTMX swap of #calendar-list ──────
    // The handler returns the same partial used for the initial
    // render, but the DOM nodes are fresh — so the timestamps,
    // today-highlight, and local filters all need re-applying.
    // Listen on the list container; the event bubbles up but we
    // scope to swaps that actually replaced our region.
    var listEl = document.getElementById('calendar-list');
    if (listEl) {
        listEl.addEventListener('htmx:afterSwap', function (ev) {
            if (!ev || !ev.target) return;
            // The target on a swap settle is the swapped element
            // itself. Re-run the hydrators against the current
            // list root so we don't pick up stale handles.
            var root = document.getElementById('calendar-list') || document;
            hydrateTimes(root);
            applyTodayHighlight(root);
            applyLocalFilters(root);
            // Active-tab class follows hx-push-url'd state. htmx
            // doesn't update the `.active` class on swap, so we
            // resync from the URL.
            try {
                var url = new URL(window.location.href);
                var range = url.searchParams.get('range') || 'this_week';
                document.querySelectorAll('.calendar-range-tab').forEach(function (a) {
                    var on = a.dataset.range === range;
                    a.classList.toggle('active', on);
                    a.setAttribute('aria-selected', on ? 'true' : 'false');
                });
            } catch (_) {}
        });
    }
})();
