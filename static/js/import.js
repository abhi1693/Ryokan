// Manual import wizard (/system/import, issue #122).
//
// The only client-side behavior: while a preview is scanning or an
// import is running, watch its progress job through the shared sticky
// toast and reload the page when the terminal event lands, so the user
// sees "Matching 12 of 50" / "Importing S01E07" tick up and then the
// finished preview or report without touching anything.
// Every override control on the preview is a plain form with hx-*
// attributes; htmx swaps the card, no script needed here.
//
// Registered through the page-lifecycle helper so a boosted nav away
// from the page tears the watcher down and a nav back re-arms it.

// Both long-running states (scanning, importing) render a block with
// `data-import-progress` (the job's progress id) and `data-session`
// (the wizard URL to come back to).
ryokanRegisterPageInit('import-progress', {
    check: function () { return !!document.querySelector('[data-import-progress]'); },
    mount: function () {
        var el = document.querySelector('[data-import-progress]');
        var progressId = el ? el.getAttribute('data-import-progress') : '';
        var id = el ? el.getAttribute('data-session') : '';
        if (!id || !progressId || !window.ryokanProgressToast) return;
        var importing = el.id === 'import-importing';
        window.__importScanToast = window.ryokanProgressToast({
            progressId: progressId,
            title: importing ? 'Importing...' : 'Scanning library...',
            category: 'library',
            onTerminal: function (ev) {
                window.__importScanToast = null;
                // A real terminal event: the session moved on;
                // re-render. An empty descriptor means the job was
                // already swept before we polled (very fast job, or a
                // stale tab): reload once to pick up whatever state
                // the session is in, but never loop on a session
                // that stays put with no job behind it.
                var key = 'ryokan-import-reload-' + progressId;
                var stage = ev && ev.stage;
                var already = false;
                try { already = sessionStorage.getItem(key) === '1'; } catch (_) {}
                if (!stage && already) return;
                try { sessionStorage.setItem(key, '1'); } catch (_) {}
                window.location.href = '/system/import?session=' + encodeURIComponent(id);
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

// Focus the picker's search box when a card's <details> opens. `toggle`
// doesn't bubble, so listen in the capture phase on the document; the
// handler is idempotent across boost re-executions because
// document-level listeners are replaced with the page's script.
if (!window.__importPickerFocusBound) {
    window.__importPickerFocusBound = true;
    document.addEventListener('toggle', function (ev) {
        var d = ev.target;
        if (!d || !d.matches || !d.matches('details.import-picker') || !d.open) return;
        var input = d.querySelector('input[type="search"]');
        if (input) { try { input.focus(); input.select(); } catch (_) {} }
    }, true);
}
