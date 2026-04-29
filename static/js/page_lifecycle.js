// Page lifecycle helper for the hx-boost rollout (Phase B per
// /home/john/Documents/ryokan-roadmap/hx_boost_rollout_plan.md).
//
// Why this exists: per-page scripts that started a `setInterval` at
// module scope (downloads.js, system.js) work fine on a fresh
// document load but leak when the user navigates AWAY via boost
// (interval keeps firing in the background, polling for elements
// that no longer exist) and double-leak when they navigate BACK
// (a second interval starts on top of the first, never the same
// reference). One `setInterval` per nav, never cleared.
//
// Pattern this exposes:
//
//   ryokanRegisterPageInit('downloads-queue', {
//       check: () => !!document.getElementById('queue-container'),
//       mount: () => { window.__downloadsQueuePoller = setInterval(loadQueue, 5000); },
//       unmount: () => {
//           clearInterval(window.__downloadsQueuePoller);
//           window.__downloadsQueuePoller = null;
//       },
//   });
//
// Each registration runs `check()` on every htmx.onLoad firing
// (initial document load + every boosted swap):
//   - was-active && !is-active  → call unmount() (page left)
//   - !was-active && is-active  → call mount() (page entered)
//   - is-active && was-active   → no-op (re-render of same page;
//                                  caller's mount must be idempotent
//                                  if it ever needs to run twice)
//
// The boot order is important: this script must load AFTER htmx
// (so `window.htmx.onLoad` exists) and BEFORE the per-page scripts
// that call `ryokanRegisterPageInit`. base.html loads it between
// htmx + extensions and `base.js`.

(function () {
    const registry = [];

    window.ryokanRegisterPageInit = function (name, options) {
        const reg = {
            name: name,
            wasActive: false,
            check: options.check,
            mount: options.mount || function () {},
            unmount: options.unmount || function () {},
        };
        registry.push(reg);
        // Immediate-reconcile: per-page scripts load AFTER
        // page_lifecycle.js (defer ordering). htmx.onLoad already
        // fired its initial-document pass before this registration
        // landed, so without an immediate check the page's mount
        // wouldn't fire until the FIRST boosted swap. That breaks
        // direct-URL loads where the page IS active right now —
        // the poller never starts. Run the check + mount inline.
        if (document.readyState !== 'loading') {
            try {
                if (reg.check()) {
                    reg.mount();
                    reg.wasActive = true;
                }
            } catch (e) {
                if (window.console && console.error) {
                    console.error(
                        'ryokanRegisterPageInit immediate-mount failed for',
                        reg.name, e
                    );
                }
            }
        }
    };

    function applyLifecycle() {
        for (const reg of registry) {
            const isActive = !!reg.check();
            if (isActive && !reg.wasActive) {
                try { reg.mount(); } catch (e) {
                    // A page's mount throwing must not prevent
                    // sibling registrations from being processed.
                    // Log and continue — same posture as
                    // `htmx.onLoad`'s own internal error handling.
                    if (window.console && console.error) {
                        console.error('ryokanRegisterPageInit mount failed for', reg.name, e);
                    }
                }
            } else if (!isActive && reg.wasActive) {
                try { reg.unmount(); } catch (e) {
                    if (window.console && console.error) {
                        console.error('ryokanRegisterPageInit unmount failed for', reg.name, e);
                    }
                }
            }
            reg.wasActive = isActive;
        }
    }

    // Wire to htmx.onLoad when available; falls back to a single
    // DOMContentLoaded firing for the rare case htmx never loads
    // (vendored asset 404, etc.) so the initial page still gets
    // its mount() call.
    if (window.htmx && typeof window.htmx.onLoad === 'function') {
        window.htmx.onLoad(applyLifecycle);
    } else {
        document.addEventListener('DOMContentLoaded', applyLifecycle);
    }
})();
