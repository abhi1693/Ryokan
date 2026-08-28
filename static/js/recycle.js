// Recycle Bin page (/library/recycle, issue #123).
//
// Three actions, all plain fetch POSTs against the JSON endpoints in
// handlers/library/recycle.rs. Rows are removed client-side on success
// and the summary line is recomputed from the surviving rows'
// `data-bytes`, so the page never needs a full reload (which would wipe
// the outcome toast). `var` at module scope: this file re-executes on
// every hx-boost body swap and `let`/`const` would throw a
// redeclaration SyntaxError.

var recycleInFlight = false;

function recycleHumanBytes(n) {
    var units = ['B', 'KiB', 'MiB', 'GiB', 'TiB'];
    var v = Number(n) || 0;
    var i = 0;
    while (v >= 1024 && i < units.length - 1) {
        v /= 1024;
        i += 1;
    }
    return i === 0 ? Math.round(v) + ' B' : v.toFixed(1) + ' ' + units[i];
}

function recyclePost(url) {
    return fetch(url, {
        method: 'POST',
        headers: { 'Content-Type': 'application/json' }
    }).then(function (r) {
        return r.json().catch(function () {
            return { ok: false, message: 'HTTP ' + r.status };
        }).then(function (body) {
            return { status: r.status, body: body };
        });
    });
}

function recycleToast(kind, title, body) {
    if (!window.ryokanToast) return;
    window.ryokanToast({ kind: kind, title: title, body: body, log: kind === 'error' });
}

function recycleSetRowBusy(id, busy) {
    var row = document.getElementById('recycle-' + id);
    if (!row) return;
    row.classList.toggle('recycle-busy', busy);
    row.querySelectorAll('button').forEach(function (b) { b.disabled = busy; });
}

// Drop a row, collapse its date section when empty, and refresh the
// summary counts from whatever rows remain.
function recycleRemoveRow(id) {
    var row = document.getElementById('recycle-' + id);
    if (row) {
        var section = row.closest('.recycle-group');
        row.remove();
        // Drop the date group once its last entry is gone.
        if (section && !section.querySelector('tbody tr')) section.remove();
    }
    recycleRefreshSummary();
}

function recycleRefreshSummary() {
    var rows = document.querySelectorAll('#recycle-groups tbody tr');
    var count = rows.length;
    var bytes = 0;
    rows.forEach(function (r) { bytes += Number(r.getAttribute('data-bytes')) || 0; });
    var countEl = document.getElementById('recycle-total-entries');
    var sizeEl = document.getElementById('recycle-total-size');
    if (countEl) countEl.textContent = String(count);
    if (sizeEl) sizeEl.textContent = recycleHumanBytes(bytes);
    var emptyBtn = document.getElementById('recycle-empty-btn');
    if (emptyBtn) emptyBtn.disabled = count === 0;
    if (count === 0) {
        var groups = document.getElementById('recycle-groups');
        if (!document.getElementById('recycle-empty-state') && groups) {
            var empty = document.createElement('div');
            empty.className = 'empty-state';
            empty.id = 'recycle-empty-state';
            empty.innerHTML = '<p>The recycle bin is empty.</p>';
            groups.parentNode.insertBefore(empty, groups);
        }
    }
}

function recycleRestore(id) {
    if (recycleInFlight) return;
    recycleInFlight = true;
    recycleSetRowBusy(id, true);
    recyclePost('/api/library/recycle/' + encodeURIComponent(id) + '/restore')
        .then(function (res) {
            if (res.body && res.body.ok) {
                recycleToast('success', 'Restored', res.body.message || 'Restored');
                recycleRemoveRow(id);
            } else {
                recycleToast('error', 'Restore failed', (res.body && res.body.message) || ('HTTP ' + res.status));
                recycleSetRowBusy(id, false);
            }
        })
        .catch(function (e) {
            recycleToast('error', 'Restore failed', (e && e.message) || 'Network error');
            recycleSetRowBusy(id, false);
        })
        .then(function () { recycleInFlight = false; });
}

function recycleDeleteNow(id) {
    if (recycleInFlight || !window.ryokanConfirm) return;
    var row = document.getElementById('recycle-' + id);
    var label = row ? (row.getAttribute('data-bytes-label') || '') : '';
    window.ryokanConfirm({
        title: 'Delete permanently?',
        body: 'Delete this item from the recycle bin now' + (label ? ' (' + label + ')' : '') + '. This cannot be undone.',
        yesLabel: 'Delete now',
        noLabel: 'Cancel',
        danger: true
    }).then(function (result) {
        if (!result || !result.ok) return;
        recycleInFlight = true;
        recycleSetRowBusy(id, true);
        return recyclePost('/api/library/recycle/' + encodeURIComponent(id) + '/purge')
            .then(function (res) {
                if (res.body && res.body.ok) {
                    recycleToast('success', 'Deleted', res.body.message || 'Deleted permanently');
                    recycleRemoveRow(id);
                } else {
                    recycleToast('error', 'Delete failed', (res.body && res.body.message) || ('HTTP ' + res.status));
                    recycleSetRowBusy(id, false);
                }
            })
            .catch(function (e) {
                recycleToast('error', 'Delete failed', (e && e.message) || 'Network error');
                recycleSetRowBusy(id, false);
            })
            .then(function () { recycleInFlight = false; });
    });
}

function recycleEmpty(btn) {
    if (recycleInFlight || !window.ryokanConfirm) return;
    var countEl = document.getElementById('recycle-total-entries');
    var sizeEl = document.getElementById('recycle-total-size');
    var count = countEl ? countEl.textContent : '';
    var size = sizeEl ? sizeEl.textContent : '';
    window.ryokanConfirm({
        title: 'Empty recycle bin?',
        body: 'Permanently delete every recycled item (' + count + ' item' + (count === '1' ? '' : 's') + ', ' + size + '). This cannot be undone.',
        yesLabel: 'Empty recycle bin',
        noLabel: 'Cancel',
        danger: true
    }).then(function (result) {
        if (!result || !result.ok) return;
        recycleInFlight = true;
        if (btn) { btn.disabled = true; btn.textContent = 'Emptying...'; }
        return recyclePost('/api/library/recycle/empty')
            .then(function (res) {
                if (res.body && res.body.ok) {
                    recycleToast('success', 'Recycle bin emptied', res.body.message || '');
                    document.querySelectorAll('#recycle-groups .recycle-group').forEach(function (s) { s.remove(); });
                    recycleRefreshSummary();
                } else {
                    recycleToast('error', 'Empty failed', (res.body && res.body.message) || ('HTTP ' + res.status));
                }
            })
            .catch(function (e) {
                recycleToast('error', 'Empty failed', (e && e.message) || 'Network error');
            })
            .then(function () {
                recycleInFlight = false;
                if (btn) { btn.textContent = 'Empty recycle bin'; btn.disabled = false; recycleRefreshSummary(); }
            });
    });
}
