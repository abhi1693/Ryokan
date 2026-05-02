# templates/CLAUDE.md — Askama + HTMX

Askama 0.16 (Jinja2-like, compiled into the binary at build time via proc-macro). Templates live here; handlers call `template.render()` and wrap in `Html(...)` themselves — there's no `axum`-integration crate (Askama 0.13 merged with rinja and dropped per-framework wrappers).

HTMX 2.x is vendored under `static/vendor/`. **Body-wide `hx-boost="true"` is active** — every plain `<a>` and `<form>` runs through htmx by default.

Pinned to htmx **2.x** deliberately. htmx 4 is in beta with substantial breaking changes (event renames, attribute-inheritance flip, error-response swap policy, SSE rewrite); upgrade tracked separately for late-2026 / early-2027.

## Boot order

`templates/base.html` loads htmx scripts as `defer` *before* `static/js/page_lifecycle.js` and `static/js/base.js` so any code referencing the `htmx.*` global sees it on first paint.

`htmx.config.historyEnableCache = false` is pinned via a `<meta name="htmx-config" content='{"historyEnableCache":false}'>` tag in `<head>` (htmx 2.x reads this meta during init). Back/forward refetches dynamic pages (Downloads queue, System logs) instead of restoring stale snapshots. Pinned by `tests/htmx_foundation.rs::base_pins_history_cache_off_via_meta`.

`htmx-ext-head-support` diff-merges `<head>` so per-page `{% block page_css %}` swaps cleanly between pages.

Per-element `hx-boost="false"` opt-outs in `base.html`:
- `/logout` (avoids swap-then-redirect race against session-clear)
- `target="_blank"` and in-page `href="#anchor"` (defensive — htmx defaults already do the right thing, but explicit doesn't hurt)

## Partial-rendering convention

Handlers needing both full-page and fragment responses branch on `axum_htmx::HxRequest(is_htmx)`. Fragments live under:

- `templates/partials/<page>/<area>.html` (per-area, e.g. settings tab body)
- `templates/partials/<page>/<thing>_row.html` (single repeating row, e.g. an indexer row swapped after upsert)

Pure-fragment routes (no full-page equivalent — e.g. conditional-field swap on a select change) are new handlers only hit by `hx-*` requests.

**Progressive enhancement is preserved**: every form-POST handler keeps its non-HTMX path (form data → write → redirect) so the page works without JS. The HxRequest branch is the optimization, not the only path.

## Patterns load-bearing for new migrations

Tested via `tests/htmx_browser_e2e*.rs`.

- **`Form<T>`, not `Json<T>`**, on handler extractors. `hx-vals` + `hx-include="closest form"` form-encode by default. New handlers take `Form<T>` with `#[serde(default)]` on every field if `hx-include` may pull extras the handler doesn't care about — serde silently drops unknown fields.
- **Always-200 for inline-result swaps.** htmx 2.x's default error policy *skips the swap on 4xx/5xx* — a handler returning 502 on connection-test failure leaves the spinner up forever. Pattern: `templates/partials/settings/connection_test_result.html` + `ConnectionTestResultPartial::into_html_ok()` — render success/failure into the same partial (different inline color), always 200. Inverse for row-removal swaps: 5xx is the right signal so htmx skips the swap and the row stays put for the user to retry.
- **`htmx:confirm` bridge** in `static/js/base.js` wires `data-ryokan-confirm-*` attrs to the in-app confirm modal *for forms with hx-\* attrs*. Load-bearing because htmx's submit listener fires before any per-form `submit` listener could (registration order — htmx loads first), so a custom listener calling `preventDefault()` runs after the AJAX is already in flight. **Pure form-POST forms (no `hx-*`) keep using the per-form submit-intercept pattern.** Adding a confirm modal to a new HTMX form is just adding the `data-ryokan-confirm-*` attrs.
- **`HX-Refresh: true`** for full-page reload after a state change a per-row swap can't represent. CF delete returns this when the table goes empty so the empty-state CTA renders (lives outside the `{% for %}` loop, can't be swapped in by per-row `outerHTML`). Don't overuse — full reload is heavy; per-row swap is default.
- **Wire legacy form fields through hidden inputs** even after they no longer drive runtime behavior. A user with a stale tab will blank them on save otherwise. Pattern in `handlers/settings/mod.rs::settings_submit` for `qbit_url` / `qbit_user` / `qbit_pass`.
- **`htmx_aware_redirect` for any `Redirect::to`.** Under hx-boost, htmx 2.x follows 3xx via `fetch` and inline-swaps the destination's HTML into the source page's `hx-target` — producing nested-page renders (a Settings response inside the prior page's body). Helper at `src/handlers/responses.rs` returns `200 OK` + `HX-Redirect` for HTMX callers (htmx triggers a real `window.location` nav) and a standard 303 for plain callers. `htmx_aware_redirect_from_req(req, url)` is the middleware-friendly variant. **`tests/htmx_redirect_audit.rs` is a CI-enforced lint** — every `Redirect::to` must route through the helper, sit inside `if !is_htmx { ... }`, or be in the documented exceptions table.

## Per-page JS lifecycle (`static/js/page_lifecycle.js`)

Module-scope `setInterval` started once at initial document load runs forever and accumulates copies on every boosted re-entry. The lifecycle helper exposes:

```js
ryokanRegisterPageInit(name, { check, mount, unmount });
```

Each registration runs `check()` on every `htmx.onLoad` firing, calls `mount()` when the page becomes active and `unmount()` when it leaves. Try/catch wraps each so a throwing registration can't break siblings.

**Use this for any new page that starts a poller or registers global listeners.** The legacy `if (document.getElementById('foo')) setInterval(...)` shape leaks under boost.

## Per-page `<script>` placement

**Per-page `<script>` tags belong in `{% block page_js %}`, not `{% block content %}`.**

Per-page scripts call `window.ryokanRegisterPageInit` / `ryokanProgressToast` / `ryokanToast` defined in `page_lifecycle.js` + `base.js`. With `defer`, scripts execute in DOM-tree order — scripts inside `{% block content %}` run *before* base.html's bottom-of-body scripts, so the per-page script runs before its dependencies are defined → TypeError → silent script abort.

Boost-nav users don't notice because the helpers stay loaded from a prior page; only direct-URL loads hit the bug. The `{% block page_js %}` placeholder is at end-of-body after `base.js`, so per-page scripts render LAST.

## Per-page JS quirks under hx-boost

- **Use `var` at module scope, not `let` / `const`.** Top-level `let` / `const` throws "redeclaration" SyntaxError on body-swap re-execute.
- **Module-scope DOM snapshots go stale** after a body swap. Cache via a Proxy or re-query inside the handler.

## CSS gotcha: `background:` shorthand on `<select>`

`background:` shorthand resets every `background-*` longhand it doesn't mention, including `background-image`. **Never use it on `<select>` or anything that inherits select-styling.**

`forms.css` has a global `select { appearance: none; background-image: <SVG chevron>; }` rule that paints a CSS chevron in place of the native one (Firefox under hx-boost drops the native chevron after a body-swap; CSS-painted survives). Per-element rules like `.folder-select { background: var(--bg) }` silently clobber the chevron.

Use `background-color: var(--bg)` (longhand) instead. The global `select` rule has `!important` on its `background-*` properties as defense-in-depth, but that's a guard, not a license.

## HX-Trigger payloads must be ASCII

Non-ASCII bytes mojibake into Latin-1 (em-dash → `â\u{80}\u{94}`). Use ASCII punctuation in HX-Trigger JSON envelopes.

## Toast helpers (defined in `static/js/base.js`)

- `ryokanToast(msg, kind)` — kind is `info` | `success` | `error`.
- `ryokanProgressToast(jobId, opts)` — long-job progress polling.

## XSS surface

Anything rendered with `|safe` must have been round-tripped through `services::html::{escape, sanitize}` (built on `ammonia`) or it's an XSS vector. AniList descriptions, Nyaa description bodies, and any user-controlled string go through sanitize.
