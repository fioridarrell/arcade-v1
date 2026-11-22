/* Arcade V1.0: arcade.apps for app pages: the apps on this arcade, and opening one in its own window. Any app (an
   assistant, a launcher, a catalog) can list what the arcade has and offer to open it; the arcade names no app.

     arcade.apps.list(o?)  -> [{id, chain, chain_name, txid, name, version, category, about, by, creator, owner, icon,
                               trust, installed}]
       every app the App Store lists, by name (GET /r/apps: public card data, no pass needed). o = {q, offset, limit}:
       q matches name, about, category and the publisher's @tag; at most 500 rows a call (100 when no limit is given).
       by: the publisher's @tag or null. icon: the app's cover on this arcade (/app/<chain>/<txid>, an image to show) or
       null. trust: green (this arcade's own ledger, or arcades on the mesh agree) or yellow (one arcade's word).
       Cards carry no screenshots: a row has none.
     arcade.apps.page(o?)  -> {apps, total, more}: the same, with how many match and whether there are more after these.
     arcade.apps.open(id)  -> {opened, title}
       asks to open the app with that id (a row's id) in its own window. The window shows a card ("<this app> wants to
       open <that app>", Allow / No) the page can't see or press; Allow opens it as Apps does. Refused, with the reason
       as the error: the page's own app, an id the App Store doesn't list, a No ("declined"), or another question still
       open on this page. */
(function () {
  'use strict';
  window.arcade = window.arcade || {};
  if (window.arcade.apps) return;
  var seq = 0, waiting = {};

  function ask(message) {
    return new Promise(function (resolve, reject) {
      message.arcade = 'apps';
      message.seq = ++seq;
      waiting[message.seq] = { resolve: resolve, reject: reject };
      window.parent.postMessage(message, '*');
    });
  }

  window.addEventListener('message', function (e) {
    if (e.source !== window.parent) return;
    var m = e.data || {};
    if (m.arcade !== 'apps' || !waiting[m.seq]) return;
    var w = waiting[m.seq];
    delete waiting[m.seq];
    if (m.ok) w.resolve(m); else w.reject(new Error(m.error || 'refused'));
  });

  function page(o) {
    o = o || {};
    var q = [];
    if (o.q) q.push('q=' + encodeURIComponent(String(o.q)));
    if (o.offset) q.push('offset=' + Math.max(0, Math.floor(Number(o.offset) || 0)));
    if (o.limit) q.push('limit=' + Math.max(1, Math.floor(Number(o.limit) || 1)));
    return fetch('/r/apps' + (q.length ? '?' + q.join('&') : ''), { cache: 'no-store' })
      .then(function (r) { return r.json().then(function (v) { if (!r.ok) throw new Error(v.error || ('the arcade answered ' + r.status)); return v; }); });
  }

  window.arcade.apps = {
    list: function (o) { return page(o).then(function (v) { return v.apps || []; }); },
    page: page,
    open: function (id) { return ask({ op: 'open', id: String(id || '') }).then(function (m) { return { opened: m.opened, title: m.title }; }); }
  };
})();
