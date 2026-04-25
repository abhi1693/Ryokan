// Wire any form carrying data-ryokan-confirm-title/body through the shared
// ryokanConfirm modal. The form submits natively on "Yes"; a flag keeps
// the handler from re-prompting after the programmatic submit() call.
(function() {
    document.querySelectorAll('form[data-ryokan-confirm-title]').forEach(function(form) {
        form.addEventListener('submit', function(ev) {
            if (form.dataset.ryokanConfirmed === '1') return;
            ev.preventDefault();
            window.ryokanConfirm({
                title: form.getAttribute('data-ryokan-confirm-title') || 'Confirm',
                body: form.getAttribute('data-ryokan-confirm-body') || 'Are you sure?',
                yesLabel: form.getAttribute('data-ryokan-confirm-label') || 'Yes',
            }).then(function(result) {
                if (result.ok) {
                    form.dataset.ryokanConfirmed = '1';
                    form.submit();
                }
            });
        });
    });
})();

// #11.1 — Client-side filter for the CF card grid. Matches against
// `data-cf-*` attributes on each card (name is pre-lowercased
// server-side, score/trash_id/origin match case-insensitively). Empty
// query = show all. Updates the `#cf-visible-count` pill and toggles
// the "no matches" placeholder so the grid doesn't render as a blank
// void when every card is filtered out.
function filterCfList(query) {
    const q = (query || '').trim().toLowerCase();
    const grid = document.getElementById('cf-list-tbody');
    if (!grid) return;
    const cards = grid.querySelectorAll('.cf-card:not(.cf-card-add)');
    let visible = 0;
    cards.forEach(function(card) {
        if (!q) {
            card.style.display = '';
            visible++;
            return;
        }
        const name = card.dataset.cfName || '';
        const score = (card.dataset.cfScore || '').toLowerCase();
        const trashId = card.dataset.cfTrashId || '';
        const origin = (card.dataset.cfOrigin || '').toLowerCase();
        const hit = name.includes(q)
            || score.includes(q)
            || trashId.includes(q)
            || origin.includes(q);
        card.style.display = hit ? '' : 'none';
        if (hit) visible++;
    });
    const countEl = document.getElementById('cf-visible-count');
    if (countEl) countEl.textContent = visible;
    const emptyEl = document.getElementById('cf-list-empty-filter');
    if (emptyEl) emptyEl.style.display = (visible === 0 && cards.length > 0) ? '' : 'none';
    // Hide the trailing "+ Add" tile while the user is filtering —
    // mixing it into search results reads as noise.
    const addCard = grid.querySelector('.cf-card-add');
    if (addCard) addCard.style.display = q ? 'none' : '';
}

// #11.1 stage 2 — modal editor open/close. The modal element itself is
// server-rendered with the pre-filled form when ?edit_id=N is set, so
// auto-opening is just flipping display. "+ Add Custom Format" opens a
// fresh modal — but since the form markup is tied to server-side edit
// state, clicking + on a page that's rendering in edit mode would
// open the edit form, not a blank one. Fix by clearing edit_id via a
// GET navigation first whenever the user hits + from an edit-mode page.
function openCfEditorModal() {
    const modal = document.getElementById('cf-editor-modal');
    if (!modal) return;
    // Reset the form to "Add Custom Format" shape in-place. The form
    // was server-rendered with the previous edit's values if the page
    // load had ?edit_id=N, so just flipping display:flex would show
    // those stale fields. Navigate-to-reset (the previous fix) worked
    // but required two clicks to see the empty modal. Clearing in-place
    // keeps it a single click.
    const form = modal.querySelector('form');
    if (form) {
        const hiddenId = form.querySelector('input[type="hidden"][name="id"]');
        if (hiddenId) hiddenId.remove();
        const trashDesc = form.querySelector('.cf-trash-description');
        if (trashDesc) trashDesc.remove();
        const name = form.querySelector('#cf_name');
        if (name) name.value = '';
        const score = form.querySelector('#cf_score');
        if (score) score.value = '0';
        const trashId = form.querySelector('#cf_trash_id');
        if (trashId) trashId.value = '';
        const json = form.querySelector('#cf_json');
        if (json) json.value = '';
    }
    const title = document.getElementById('cf-editor-modal-title');
    if (title) title.textContent = 'Add Custom Format';
    const submit = document.getElementById('cf-upsert-submit');
    if (submit) submit.textContent = 'Create Custom Format';
    // Hide the Delete form — nothing to delete when we haven't saved
    // yet. Leaving it visible in Add mode would let the user click
    // Delete with the previously-edited CF's hidden id still in the
    // form (server-rendered). Hiding also collapses the footer to the
    // single-button layout on the right.
    const deleteForm = document.getElementById('cf-delete-form');
    if (deleteForm) deleteForm.style.display = 'none';

    modal.style.display = 'flex';
    const nameEl = document.getElementById('cf_name');
    if (nameEl) nameEl.focus();
}
function closeCfEditorModal() {
    const modal = document.getElementById('cf-editor-modal');
    if (modal) modal.style.display = 'none';
    // Drop ?edit_id=N from the URL without reloading so Cancel doesn't
    // leave the user on a URL that'd re-open the modal on refresh.
    const params = new URLSearchParams(window.location.search);
    if (params.has('edit_id')) {
        params.delete('edit_id');
        const newUrl = window.location.pathname + (params.toString() ? '?' + params.toString() : '');
        history.replaceState(null, '', newUrl);
    }
    // Drop the selected-card highlight. The class was server-rendered
    // based on ?edit_id=N, so closing without a full reload leaves the
    // highlight stuck on the originally-edited card until next refresh.
    document.querySelectorAll('.cf-card-selected').forEach(function(el) {
        el.classList.remove('cf-card-selected');
    });
}
// Auto-open on load when the URL carries ?edit_id=N — the server
// already rendered the pre-filled form inside the modal markup, so
// this is just a display flip. Also handle backdrop click + Escape
// to dismiss.
(function() {
    const modal = document.getElementById('cf-editor-modal');
    if (!modal) return;
    const params = new URLSearchParams(window.location.search);
    if (params.has('edit_id')) {
        modal.style.display = 'flex';
    }
    modal.addEventListener('click', function(ev) {
        if (ev.target === modal) closeCfEditorModal();
    });
    document.addEventListener('keydown', function(ev) {
        if (ev.key === 'Escape' && modal.style.display !== 'none') closeCfEditorModal();
    });
})();

// #11.4 — CF export selector. Radios pick the mode, checkboxes pick the
// ids, then two actions: download the file (via the existing GET endpoint)
// or copy the pretty-printed JSON to the clipboard (same endpoint, fetch
// then navigator.clipboard.writeText).

// #11.3 — When a JSON file is picked, read it and drop into the paste
// textarea so the user doesn't need a two-step ceremony. The server-side
// flow stays the same (POST payload → preview → resolve).
function cfImportFilePicked(input) {
    const file = input.files && input.files[0];
    if (!file) return;
    const reader = new FileReader();
    reader.onload = function(ev) {
        const target = document.getElementById('cf_import_payload');
        if (target && ev.target && typeof ev.target.result === 'string') {
            target.value = ev.target.result;
            target.focus();
        }
    };
    reader.onerror = function() {
        if (window.ryokanToast) {
            window.ryokanToast({ kind: 'error', title: 'Import', body: 'Could not read the file.' });
        }
    };
    reader.readAsText(file);
}

function cfExportSelectAll(state) {
    document.querySelectorAll('input[name="cf_export_ids"]').forEach(function(cb) {
        cb.checked = state;
    });
}

function cfExportBuildUrl() {
    const mode = document.querySelector('input[name="cf_export_mode"]:checked');
    const ids = Array.from(
        document.querySelectorAll('input[name="cf_export_ids"]:checked')
    ).map(function(cb) { return cb.value; });
    const params = new URLSearchParams();
    if (mode && mode.value) params.set('mode', mode.value);
    // Only attach `ids` when the user has deselected at least one; an
    // empty `ids` param would be interpreted by the server as "all" (by
    // design — keeps curl workflows unchanged), but sending the full list
    // as the default is also harmless.
    if (ids.length > 0) params.set('ids', ids.join(','));
    const qs = params.toString();
    return '/settings/custom-formats/export' + (qs ? '?' + qs : '');
}

function cfExportDownload() {
    // Simplest possible "download file" trigger: navigate to the URL —
    // the server sets Content-Disposition: attachment and the browser
    // handles the rest. Guards against "select none, click export" by
    // bailing with a toast.
    const ids = document.querySelectorAll('input[name="cf_export_ids"]:checked');
    if (ids.length === 0) {
        if (window.ryokanToast) {
            window.ryokanToast({ kind: 'warn', title: 'Export', body: 'Select at least one Custom Format to export.' });
        }
        return;
    }
    window.location.href = cfExportBuildUrl();
}

async function cfExportClipboard(btn) {
    const ids = document.querySelectorAll('input[name="cf_export_ids"]:checked');
    if (ids.length === 0) {
        if (window.ryokanToast) {
            window.ryokanToast({ kind: 'warn', title: 'Export', body: 'Select at least one Custom Format to copy.' });
        }
        return;
    }
    const originalText = btn.textContent;
    btn.disabled = true;
    btn.textContent = 'Copying…';
    try {
        const resp = await fetch(cfExportBuildUrl(), { credentials: 'same-origin' });
        if (!resp.ok) throw new Error('HTTP ' + resp.status);
        const text = await resp.text();
        await navigator.clipboard.writeText(text);
        if (window.ryokanToast) {
            window.ryokanToast({ kind: 'success', title: 'Copied', body: ids.length + ' Custom Format(s) copied to clipboard.' });
        }
    } catch (e) {
        if (window.ryokanToast) {
            window.ryokanToast({ kind: 'error', title: 'Copy failed', body: String(e && e.message ? e.message : e) });
        }
    } finally {
        btn.disabled = false;
        btn.textContent = originalText;
    }
}

// CF test box (#18). Posts the pasted release title to
// /api/custom-formats/test and renders matched/not-matched CFs with
// the summed score. Title-based specs only — Size and SeaDex specs
// always miss here, and the section copy on the page says so.
async function runCfTest() {
    const input = document.getElementById('cf-test-input');
    const out = document.getElementById('cf-test-results');
    if (!input || !out) return;
    const title = (input.value || '').trim();
    if (!title) {
        out.style.display = 'none';
        return;
    }
    // All user-controlled strings flowing into the rendered HTML below
    // (CF names, parsed fields derived from the title the user pasted,
    // error bodies from the server) must be HTML-escaped — CF names
    // persist across requests, so a malicious CF name would otherwise
    // self-execute for any admin who ran a test.
    const esc = window.ryokanEscapeHtml;
    out.style.display = 'block';
    out.innerHTML = '<p class="form-hint">Testing…</p>';
    try {
        const r = await fetch('/api/custom-formats/test', {
            method: 'POST',
            headers: {'Content-Type': 'application/json'},
            body: JSON.stringify({release_title: title}),
        });
        const data = await r.json();
        if (!r.ok || !data.ok) {
            out.innerHTML = '<p class="form-hint">Test failed: ' + esc(data.error || r.status) + '</p>';
            return;
        }
        const parsed = data.parsed || {};
        const rows = [];
        rows.push('<p class="form-hint" style="margin-bottom:8px">Parsed: source=<code>' + esc(parsed.source || 'Unknown') + '</code>, resolution=<code>' + esc(parsed.resolution || 'Unknown') + '</code>, group=<code>' + esc(parsed.group || '(none)') + '</code>' + (parsed.is_remux ? ', <code>remux</code>' : '') + (parsed.is_bdmv ? ', <code>BDMV</code>' : '') + '</p>');
        rows.push('<p><strong>Total score: ' + Number(data.total_score) + '</strong> — <span class="form-hint">' + Number(data.matched.length) + ' matched, ' + Number(data.not_matched.length) + ' not matched</span></p>');
        if (data.matched.length > 0) {
            rows.push('<div class="settings-subheading">Matched</div>');
            rows.push('<ul style="list-style:none;padding:0;margin:0 0 12px 0">');
            data.matched.forEach(cf => {
                const score = Number(cf.score);
                const sign = score > 0 ? '+' : '';
                const cls = score > 0 ? 'cf-score-positive' : (score < 0 ? 'cf-score-negative' : 'cf-score-zero');
                rows.push('<li style="padding:4px 0;display:flex;gap:10px;align-items:baseline"><span class="cf-score ' + cls + '" style="min-width:48px;text-align:right">' + sign + score + '</span><span>' + esc(cf.name) + '</span></li>');
            });
            rows.push('</ul>');
        }
        if (data.not_matched.length > 0) {
            rows.push('<details><summary class="form-hint">' + Number(data.not_matched.length) + ' CFs did not match</summary>');
            rows.push('<ul style="list-style:none;padding:0;margin:4px 0 0 0">');
            data.not_matched.forEach(cf => {
                rows.push('<li style="padding:2px 0;color:var(--text-dim);font-size:13px">' + esc(cf.name) + ' <span class="form-hint">(score ' + Number(cf.score) + ')</span></li>');
            });
            rows.push('</ul></details>');
        }
        out.innerHTML = rows.join('');
    } catch (e) {
        out.innerHTML = '<p class="form-hint">Test failed: ' + esc(e && e.message ? e.message : e) + '</p>';
    }
}

function clearCfTest() {
    const input = document.getElementById('cf-test-input');
    const out = document.getElementById('cf-test-results');
    if (input) input.value = '';
    if (out) { out.innerHTML = ''; out.style.display = 'none'; }
}

function generateApiKey() {
    const chars = 'abcdefghijklmnopqrstuvwxyz0123456789';
    const buf = new Uint8Array(32);
    crypto.getRandomValues(buf);
    let key = '';
    for (let i = 0; i < 32; i++) key += chars[buf[i] % chars.length];
    document.getElementById('sonarr_api_key').value = key;
}

function generateRadarrApiKey() {
    const chars = 'abcdefghijklmnopqrstuvwxyz0123456789';
    const buf = new Uint8Array(32);
    crypto.getRandomValues(buf);
    let key = '';
    for (let i = 0; i < 32; i++) key += chars[buf[i] % chars.length];
    document.getElementById('radarr_api_key').value = key;
}

// #63 Phase 2 — show/hide the credential fieldset for the selected
// download client. Both fieldsets stay in the DOM so a user
// mid-edit doesn't lose form state when they toggle back.
function toggleClientFieldset(value) {
    const qbit = document.getElementById('qbit-fieldset');
    const deluge = document.getElementById('deluge-fieldset');
    const transmission = document.getElementById('transmission-fieldset');
    const rtorrent = document.getElementById('rtorrent-fieldset');
    if (qbit) qbit.style.display = value === 'qbittorrent' ? '' : 'none';
    if (deluge) deluge.style.display = value === 'deluge' ? '' : 'none';
    if (transmission) transmission.style.display = value === 'transmission' ? '' : 'none';
    if (rtorrent) rtorrent.style.display = value === 'rtorrent' ? '' : 'none';
}

// API-key inputs render as type="password" so the secret isn't visible
// to anyone glancing at the admin's screen. These two helpers restore
// the workflow that masking otherwise broke: Show toggles visibility
// for verification, Copy puts the value on the clipboard so the user
// can paste straight into Seerr without ever needing to read it.
function toggleApiKeyVisibility(inputId, btn) {
    const input = document.getElementById(inputId);
    if (!input) return;
    if (input.type === 'password') {
        input.type = 'text';
        btn.textContent = 'Hide';
    } else {
        input.type = 'password';
        btn.textContent = 'Show';
    }
}

async function copyApiKey(inputId, btn) {
    const input = document.getElementById(inputId);
    if (!input || !input.value) return;
    const original = btn.textContent;
    const flash = (label, ms) => {
        btn.textContent = label;
        setTimeout(() => { btn.textContent = original; }, ms);
    };
    // navigator.clipboard.writeText needs a secure context (HTTPS or
    // localhost). Self-hosted Ryokan often runs over plain HTTP on a
    // LAN address, so fall back to surfacing the value in a text input
    // and selecting it — the user can then Ctrl+C themselves.
    try {
        await navigator.clipboard.writeText(input.value);
        flash('Copied!', 1500);
    } catch (_e) {
        input.type = 'text';
        input.focus();
        input.select();
        flash('Select & copy', 2500);
    }
}

function syncTitleLanguagePreview(lang) {
    localStorage.setItem('titleLanguage', lang);
}

// Gather the per-collision radio selections and rename text fields
// into the two newline-delimited hidden inputs that the import-resolve
// handler expects. See plan §6.2 — each line is "<index>:<action>".
function buildCfImportResolvePayload(form) {
    const rows = form.querySelectorAll('tr[data-cf-collision-idx]');
    const decisions = [];
    const renames = [];
    for (const row of rows) {
        const idx = row.getAttribute('data-cf-collision-idx');
        const chosen = row.querySelector('input[name="cf_action_' + idx + '"]:checked');
        const action = chosen ? chosen.value : 'skip';
        decisions.push(idx + ':' + action);
        if (action === 'rename') {
            const input = row.querySelector('input[name="cf_rename_' + idx + '"]');
            const newName = input ? input.value.trim() : '';
            if (!newName) {
                window.ryokanAlert({
                    title: 'Rename required',
                    body: 'Collision ' + idx + ': pick a new name or choose a different action.',
                });
                return false;
            }
            renames.push(idx + ':' + newName);
        }
    }
    form.querySelector('#cf_import_resolve_decisions').value = decisions.join('\n');
    form.querySelector('#cf_import_resolve_renames').value = renames.join('\n');
    return true;
}

function testQbit(btn) {
    const result = document.getElementById('qbit-test-result');
    const payload = {
        qbit_url: document.getElementById('qbit_url').value,
        qbit_user: document.getElementById('qbit_user').value,
        qbit_pass: document.getElementById('qbit_pass').value,
        qbit_category: document.getElementById('qbit_category').value,
    };
    btn.disabled = true;
    result.textContent = 'Testing...';
    fetch('/api/qbit/test', {
        method: 'POST',
        headers: {'Content-Type': 'application/json'},
        body: JSON.stringify(payload)
    })
    .then(async r => {
        const data = await r.json();
        if (!r.ok) throw new Error(data.message || 'Connection failed');
        result.textContent = data.message;
    })
    .catch(err => {
        result.textContent = err.message;
    })
    .finally(() => {
        btn.disabled = false;
    });
}

function testJellyfin(btn) {
    const result = document.getElementById('jellyfin-test-result');
    const payload = {
        jellyfin_url: document.getElementById('jellyfin_url').value,
        jellyfin_api_key: document.getElementById('jellyfin_api_key').value,
    };
    btn.disabled = true;
    result.textContent = 'Testing...';
    fetch('/api/jellyfin/test', {
        method: 'POST',
        headers: {'Content-Type': 'application/json'},
        body: JSON.stringify(payload)
    })
    .then(async r => {
        const data = await r.json();
        if (!r.ok) throw new Error(data.message || 'Connection failed');
        result.textContent = data.message;
    })
    .catch(err => {
        result.textContent = err.message;
    })
    .finally(() => {
        btn.disabled = false;
    });
}

function refreshJellyfin(btn) {
    const result = document.getElementById('jellyfin-test-result');
    btn.disabled = true;
    result.textContent = 'Refreshing...';
    fetch('/api/jellyfin/refresh', {
        method: 'POST',
        headers: {'Content-Type': 'application/json'},
    })
    .then(async r => {
        const data = await r.json();
        if (!r.ok) throw new Error(data.message || 'Refresh failed');
        result.textContent = data.message || 'Library refresh requested.';
    })
    .catch(err => {
        result.textContent = err.message;
    })
    .finally(() => {
        btn.disabled = false;
    });
}

// Auto-check connection health on integrations tab load.
// The download-client status dispatches by `type` (sonarr_impl_name:
// "QBittorrent" | "Deluge" | "Transmission") so the badge lights up
// next to the correct fieldset legend regardless of which client is
// active. Only the active client's badge is populated — the others
// stay blank (no stale "Disconnected" on a client the user isn't
// even trying to use).
(function() {
    const badges = {
        QBittorrent: document.getElementById('qbit-health'),
        Deluge: document.getElementById('deluge-health'),
        Transmission: document.getElementById('transmission-health'),
        RTorrent: document.getElementById('rtorrent-health'),
    };
    const jellyfinHealth = document.getElementById('jellyfin-health');
    const anyClientBadge = Object.values(badges).some(b => b);
    if (!anyClientBadge && !jellyfinHealth) return;

    fetch('/api/health')
        .then(r => r.json())
        .then(data => {
            let activeType = null;
            if (data.download_client) {
                const dc = data.download_client;
                activeType = dc.type;
                const target = badges[dc.type];
                if (target) {
                    if (dc.ok) {
                        target.innerHTML = '<span class="log-badge log-badge-info">' + window.ryokanEscapeHtml(dc.message) + '</span>';
                    } else if (dc.message === 'Not configured') {
                        target.innerHTML = '<span class="log-badge log-badge-warn">Not configured</span>';
                    } else {
                        target.innerHTML = '<span class="log-badge log-badge-error">Disconnected</span>';
                    }
                }
            }
            // Fill non-active client badges with a neutral "Not active"
            // so the badge slot reads consistently across all four
            // fieldsets when a user toggles the dropdown to view
            // credentials for a client they haven't activated.
            Object.keys(badges).forEach(function (key) {
                const el = badges[key];
                if (!el) return;
                if (key === activeType) return;
                if (el.innerHTML.trim() !== '') return;
                el.innerHTML = '<span class="log-badge">Not active</span>';
            });
            if (jellyfinHealth && data.jellyfin) {
                if (data.jellyfin.ok) {
                    jellyfinHealth.innerHTML = '<span class="log-badge log-badge-info">' + window.ryokanEscapeHtml(data.jellyfin.message) + '</span>';
                } else if (data.jellyfin.message !== 'Not configured') {
                    jellyfinHealth.innerHTML = '<span class="log-badge log-badge-error">Disconnected</span>';
                }
            }
        })
        .catch(() => {});
})();

// Dirty-state guard on the Settings form. Flips a flag on any input
// change, prompts the user on nav-away (topbar click, browser back, tab
// close). Clears the flag on submit so the save itself doesn't trigger
// the prompt.
(function () {
    const form = document.querySelector('form.settings-form[action="/settings"]');
    if (!form) return;
    let dirty = false;
    const markDirty = () => { dirty = true; };
    form.addEventListener('input', markDirty);
    form.addEventListener('change', markDirty);
    form.addEventListener('submit', () => { dirty = false; });
    window.addEventListener('beforeunload', (ev) => {
        if (!dirty) return;
        ev.preventDefault();
        ev.returnValue = '';
    });
})();

// ── External Accounts (AL / MAL, issue #62 PR A) ──────────────────────
//
// Three interactions on the Settings → Integrations → External
// Accounts card:
//
//   1. `startExternalAccountLink(provider)` — opens the provider's
//      OAuth authorize page in a new tab via Ryokan's /start endpoint
//      (which redirects), then shows a paste-modal for the user to
//      return to once they have a token/code from the broker page.
//   2. `saveExternalAccountPrefs()` — fires on any checkbox change
//      in the linked-state panel; POSTs the whole preference set so
//      the sync task's next tick picks it up without a full form save.
//   3. `unlinkExternalAccount()` — confirmation + POST /settings/
//      oauth/unlink + reload.
//
// The paste-modal is built inline to avoid yet another templates/
// partials/ file for what's essentially a single-field prompt.

// Origin of the gh-pages-hosted broker page that AL/MAL redirect to
// after user approval. The postMessage receiver below validates
// `event.origin` against this value before reading any data.
const EXT_BROKER_ORIGIN = 'https://johnthreekay.github.io';

// Single in-flight link attempt at module scope. Holds the
// {handler, timer, provider} for the active OAuth flow so a second
// click on Link AL / Link MAL aborts the prior listener and the
// prior 10-minute cleanup timer. Without this, a user clicking
// Link AL then Link MAL before the AL flow completes would leave
// both listeners alive — and since both modals share fixed input
// IDs, the AL postMessage would auto-fill the MAL modal and
// trigger an AL submit while the user was looking at MAL.
let _extLinkAttempt = null;

function clearExtLinkAttempt() {
    if (!_extLinkAttempt) return;
    window.removeEventListener('message', _extLinkAttempt.handler);
    if (_extLinkAttempt.timer) clearTimeout(_extLinkAttempt.timer);
    _extLinkAttempt = null;
}

function startExternalAccountLink(provider) {
    // Abort any prior in-flight attempt — the user clicked Link
    // again, so the previous flow's broker postback should not be
    // accepted into the now-different modal.
    clearExtLinkAttempt();

    // Set up a one-shot postMessage listener BEFORE opening the
    // popup so a fast-completing flow (already-authenticated user,
    // already-approved app) can't deliver before we're listening.
    // Receiver validates origin + message shape; the broker page
    // parses token/state from the URL fragment/query and posts back
    // here as soon as it loads, skipping the copy-paste step.
    const expectedType = `ryokan-oauth-${provider}`;
    const handler = (event) => {
        if (event.origin !== EXT_BROKER_ORIGIN) return;
        const data = event.data || {};
        if (data.type !== expectedType) return;
        // Belt-and-suspenders: the attempt may already have been
        // cleared (timeout fired, second click came in) by the time
        // a duplicate emit lands. Only act on the still-active one.
        if (!_extLinkAttempt || _extLinkAttempt.handler !== handler) return;
        clearExtLinkAttempt();
        autoSubmitExternalAccount(provider, data);
    };
    // Auto-clean after the OAuth-state TTL (10 min) so a forgotten
    // flow doesn't leave a stale listener / timer attached for the
    // rest of the session.
    const timer = setTimeout(clearExtLinkAttempt, 10 * 60 * 1000);
    _extLinkAttempt = { handler, timer, provider };
    window.addEventListener('message', handler);

    // Open the OAuth authorize flow in a new tab so the Settings
    // page stays loaded behind it. NOT passing 'noopener' is
    // deliberate — the broker page needs `window.opener` to be set
    // so it can post values back to this tab via postMessage. The
    // popup navigates only to URLs we control (`/start` → AL/MAL
    // authorize → our gh-pages broker), so the standard tabnabbing
    // protections noopener provides aren't load-bearing here.
    window.open(`/settings/oauth/${provider}/start`, '_blank');
    openExternalAccountPasteModal(provider);
}

// Auto-fill the paste modal from a postMessage payload, then
// submit. Falls through to the manual paste UI if something looks
// off — value or state empty, network error on submit, etc.
function autoSubmitExternalAccount(provider, data) {
    const value = provider === 'anilist' ? data.access_token : data.code;
    const stateValue = data.state || '';
    if (!value || !stateValue) {
        console.warn('[ext-accounts] postMessage missing fields, falling back to manual paste');
        return;
    }
    const valueInput = document.getElementById('ext-accounts-paste-value');
    const stateInput = document.getElementById('ext-accounts-paste-state');
    if (valueInput) valueInput.value = value;
    if (stateInput) stateInput.value = stateValue;
    // Tiny delay so the user catches the auto-fill visually before
    // the modal closes on success — better feedback than an instant
    // disappear.
    setTimeout(() => submitExternalAccountPaste(provider), 200);
}

function openExternalAccountPasteModal(provider) {
    const isAnilist = provider === 'anilist';
    const providerLabel = isAnilist ? 'AniList' : 'MyAnimeList';
    const fieldLabel = isAnilist ? 'Access token' : 'Authorization code';
    const hint = isAnilist
        ? 'Approve in the AniList tab. The token + state will fill in here automatically once the broker page loads — no copy-paste needed in the common case. If your popup blocker prevented the tab from opening, copy the values from the broker page manually and paste them below.'
        : 'Approve in the MyAnimeList tab. The code + state will fill in here automatically once the broker page loads — no copy-paste needed in the common case. If your popup blocker prevented the tab from opening, copy the values from the broker page manually and paste them below.';

    let modal = document.getElementById('ext-accounts-paste-modal');
    if (modal) modal.remove();
    modal = document.createElement('div');
    modal.id = 'ext-accounts-paste-modal';
    modal.className = 'modal-backdrop';
    modal.style.display = 'flex';
    modal.innerHTML = `
        <div class="modal" role="dialog" aria-modal="true" style="max-width:480px">
            <div class="modal-header">
                <div style="font-weight:600;font-size:15px">Link ${providerLabel}</div>
                <button type="button" class="btn-icon" aria-label="Close" onclick="closeExternalAccountPasteModal()">
                    <svg width="20" height="20" viewBox="0 0 24 24" fill="none" stroke="currentColor" stroke-width="2"><path d="M18 6L6 18M6 6l12 12"/></svg>
                </button>
            </div>
            <div class="modal-body" style="padding:18px">
                <p class="form-hint" style="margin-top:0">${hint}</p>
                <div class="form-group">
                    <label for="ext-accounts-paste-value">${fieldLabel}</label>
                    <textarea id="ext-accounts-paste-value" rows="3" style="width:100%;font-family:monospace;font-size:12px"></textarea>
                </div>
                <div class="form-group">
                    <label for="ext-accounts-paste-state">State</label>
                    <input id="ext-accounts-paste-state" type="text" style="width:100%;font-family:monospace;font-size:12px">
                    <span class="form-hint">CSRF nonce — required. Both fields appear on the callback page.</span>
                </div>
                <div id="ext-accounts-paste-error" class="form-hint" style="color:var(--red);display:none"></div>
                <div style="display:flex;gap:8px;justify-content:flex-end;margin-top:12px">
                    <button type="button" class="btn btn-secondary" onclick="closeExternalAccountPasteModal()">Cancel</button>
                    <button type="button" class="btn btn-primary" id="ext-accounts-paste-submit"
                        onclick="submitExternalAccountPaste('${provider}')">Link</button>
                </div>
            </div>
        </div>`;
    document.body.appendChild(modal);
    setTimeout(() => {
        const input = document.getElementById('ext-accounts-paste-value');
        if (input) input.focus();
    }, 0);
}

function closeExternalAccountPasteModal() {
    const modal = document.getElementById('ext-accounts-paste-modal');
    if (modal) modal.remove();
}

function submitExternalAccountPaste(provider) {
    const input = document.getElementById('ext-accounts-paste-value');
    const stateInput = document.getElementById('ext-accounts-paste-state');
    const err = document.getElementById('ext-accounts-paste-error');
    const btn = document.getElementById('ext-accounts-paste-submit');
    const value = (input && input.value || '').trim();
    const stateValue = (stateInput && stateInput.value || '').trim();
    if (!value || !stateValue) {
        if (err) {
            err.textContent = 'Paste both the value and the state from the callback page.';
            err.style.display = '';
        }
        return;
    }
    if (err) err.style.display = 'none';
    if (btn) { btn.disabled = true; btn.textContent = 'Linking…'; }

    const body = provider === 'anilist'
        ? { access_token: value, state: stateValue }
        : { code: value, state: stateValue };
    fetch(`/settings/oauth/${provider}/submit`, {
        method: 'POST',
        headers: { 'Content-Type': 'application/json' },
        body: JSON.stringify(body),
    })
    .then(async (r) => {
        const data = await r.json().catch(() => ({}));
        if (!r.ok) throw new Error(data.error || data.message || `Link failed (${r.status})`);
        return data;
    })
    .then(() => {
        closeExternalAccountPasteModal();
        window.location.reload();
    })
    .catch((e) => {
        if (err) {
            err.textContent = e && e.message ? e.message : 'Link failed';
            err.style.display = '';
        }
        if (btn) { btn.disabled = false; btn.textContent = 'Link'; }
    });
}

function unlinkExternalAccount() {
    if (!window.ryokanConfirm) {
        // Fallback to native confirm if the shared helper isn't loaded.
        if (!confirm('Unlink this external account? Imported series stay in your library; user scores and custom-list memberships are cleared.')) {
            return;
        }
        return unlinkExternalAccountConfirmed();
    }
    window.ryokanConfirm({
        title: 'Unlink external account',
        body: 'Imported series stay in your library. User scores and custom-list memberships are cleared. Re-link to restore them.',
        yesLabel: 'Unlink',
        noLabel: 'Cancel',
    }).then((res) => {
        if (res && res.ok) unlinkExternalAccountConfirmed();
    });
}

function unlinkExternalAccountConfirmed() {
    fetch('/settings/oauth/unlink', { method: 'POST' })
        .then((r) => r.json().catch(() => ({})))
        .then(() => window.location.reload())
        .catch((e) => console.error('[ext-accounts] unlink failed:', e));
}

function syncWatchListNow() {
    if (typeof window.ryokanNewProgressId !== 'function' || typeof window.ryokanProgressToast !== 'function') {
        // Sticky-toast helpers come from base.js; if they're missing
        // it's a load-order bug, not a user-facing failure mode.
        console.error('[ext-accounts] progress toast helpers not loaded');
        return;
    }
    const progressId = window.ryokanNewProgressId();
    const toast = window.ryokanProgressToast({
        progressId,
        title: 'Watch-list sync starting…',
        category: 'external_sync',
    });
    fetch('/settings/oauth/sync-now', {
        method: 'POST',
        headers: { 'Content-Type': 'application/json' },
        body: JSON.stringify({ progress_id: progressId }),
    })
        .then((r) => r.json().catch(() => ({})))
        .then((data) => {
            // The sync runs in the background; the toast finalizes off
            // the progress feed. A bad-state response (e.g. account
            // unlinked between page load and click) gets surfaced here.
            if (data && data.ok === false) {
                toast.finalize({
                    kind: 'error',
                    title: 'Sync could not start',
                    body: data.error || 'Try reloading the Settings page.',
                });
            }
        })
        .catch((err) => {
            toast.finalize({ kind: 'error', title: 'Sync request failed', body: String(err) });
        });
}

let _extPrefsSaveTimer = null;
function saveExternalAccountPrefs() {
    // Debounce so the user toggling three checkboxes in a row doesn't
    // fire three POSTs back-to-back.
    if (_extPrefsSaveTimer) clearTimeout(_extPrefsSaveTimer);
    _extPrefsSaveTimer = setTimeout(() => {
        const read = (key) => {
            const el = document.querySelector(`[data-ext-pref="${key}"]`);
            return el ? el.checked : false;
        };
        fetch('/settings/oauth/preferences', {
            method: 'POST',
            headers: { 'Content-Type': 'application/json' },
            body: JSON.stringify({
                import_watching: read('import_watching'),
                import_planning: read('import_planning'),
                import_paused: read('import_paused'),
                import_dropped: read('import_dropped'),
                import_completed: read('import_completed'),
                skip_already_watched: read('skip_already_watched'),
            }),
        })
        .then((r) => { if (!r.ok) console.error('[ext-accounts] prefs save failed:', r.status); });
    }, 250);
}
