// Manual import wizard (/library/import, issue #122).
//
// The only client-side behavior: while a preview is scanning, watch
// its progress job through the shared sticky toast and reload the page
// when the terminal event lands, so the user sees "Matching 12 of 50"
// tick up and then the finished preview without touching anything.
// Every override control on the preview is a plain form with hx-*
// attributes; htmx swaps the card, no script needed here.
//
// Registered through the page-lifecycle helper so a boosted nav away
// from the page tears the watcher down and a nav back re-arms it.

ryokanRegisterPageInit('import-scanning', {
    check: function () { return !!document.getElementById('import-scanning'); },
    mount: function () {
        var el = document.getElementById('import-scanning');
        var id = el ? el.getAttribute('data-session') : '';
        if (!id || !window.ryokanProgressToast) return;
        window.__importScanToast = window.ryokanProgressToast({
            progressId: id,
            title: 'Scanning library...',
            category: 'library',
            onTerminal: function (ev) {
                window.__importScanToast = null;
                // A real terminal event: the session is Ready or
                // Failed; re-render. An empty descriptor means the
                // job was already swept before we polled (very fast
                // scan, or a stale tab): reload once to pick up
                // whatever state the session is in, but never loop
                // on a session that stays Scanning with no job.
                var key = 'ryokan-import-reload-' + id;
                var stage = ev && ev.stage;
                var already = false;
                try { already = sessionStorage.getItem(key) === '1'; } catch (_) {}
                if (!stage && already) return;
                try { sessionStorage.setItem(key, '1'); } catch (_) {}
                window.location.href = '/library/import?session=' + encodeURIComponent(id);
            }
        });
    },
    unmount: function () {
        var t = window.__importScanToast;
        window.__importScanToast = null;
        if (t && typeof t.dismiss === 'function') {
            try { t.dismiss(); } catch (_) {}
        }
    }
});
