/* Arcade V1.0: arcade.folder for app pages (2026-11-21): one folder on this computer that the person gave the app,
   to read and write inside and nowhere else. Off until the person picks a folder on the app window's card.

     arcade.folder.granted()               -> {granted, name}      (name: the folder's own name; never its path)
     arcade.folder.ask(why)                -> {granted, name}      the window shows "<app> asks for a folder to work
                                                                   in: <why>" with a folder picker; Cancel rejects
     arcade.folder.list(path)              -> {path, entries: [{name, kind: file|dir|link, dir, size, modified}], more}
     arcade.folder.stat(path)              -> {exists, name, kind, dir, size, modified}
     arcade.folder.read(path, {as})        -> text, or base64 with {as: 'base64'} (at most 8 MB)
     arcade.folder.write(path, data, {as}) -> {path, size}  data: text, base64 with {as: 'base64'}, or an ArrayBuffer,
                                              typed array or Blob (sent as base64); at most 8 MB, 200 MB in the folder
     arcade.folder.mkdir(path)             -> {path, made}
     arcade.folder.remove(path)            -> {path, removed}      a file, or an empty folder

   Paths are relative to the folder ("" is the folder itself); absolute paths and ".." are refused. */
(function () {
  'use strict';
  window.arcade = window.arcade || {};
  if (window.arcade.folder) return;
  var seq = 0, waiting = {};

  // The card is the window's: asked by postMessage, like arcade.svc's door.
  function door(message) {
    return new Promise(function (resolve, reject) {
      message.arcade = 'folder';
      message.seq = ++seq;
      waiting[message.seq] = { resolve: resolve, reject: reject };
      window.parent.postMessage(message, '*');
    });
  }
  window.addEventListener('message', function (e) {
    if (e.source !== window.parent) return;
    var m = e.data || {};
    if (m.arcade !== 'folder' || !waiting[m.seq]) return;
    var w = waiting[m.seq];
    delete waiting[m.seq];
    if (m.ok) w.resolve({ granted: !!m.granted, name: m.name || null }); else w.reject(new Error(m.error || 'refused'));
  });

  // The rest are the page API's (/r/folder/*), which the page's pass goes with.
  function answer(r) {
    return r.json().catch(function () { return {}; }).then(function (j) {
      if (!r.ok) throw new Error(j.error || ('folder: ' + r.status));
      return j;
    });
  }
  function get(op, path, as) {
    var q = '?path=' + encodeURIComponent(path == null ? '' : String(path)) + (as ? '&as=' + encodeURIComponent(as) : '');
    return fetch('/r/folder/' + op + q, { cache: 'no-store' }).then(answer);
  }
  function post(op, body) {
    return fetch('/r/folder/' + op, { method: 'POST', headers: { 'Content-Type': 'application/json' }, body: JSON.stringify(body) }).then(answer);
  }
  function b64(buf) {
    var b = new Uint8Array(buf), out = '';
    for (var i = 0; i < b.length; i += 0x8000) out += String.fromCharCode.apply(null, b.subarray(i, i + 0x8000));
    return btoa(out);
  }
  function asBase64(data) {
    if (typeof Blob !== 'undefined' && data instanceof Blob) return data.arrayBuffer().then(b64);
    if (data instanceof ArrayBuffer) return Promise.resolve(b64(data));
    if (ArrayBuffer.isView(data)) return Promise.resolve(b64(data.buffer.slice(data.byteOffset, data.byteOffset + data.byteLength)));
    return null;
  }

  window.arcade.folder = {
    granted: function () { return fetch('/r/folder/granted', { cache: 'no-store' }).then(answer); },
    ask: function (why) { return door({ op: 'ask', why: String(why == null ? '' : why).slice(0, 200) }); },
    list: function (path) { return get('list', path); },
    stat: function (path) { return get('stat', path); },
    read: function (path, o) {
      var as = (o && o.as) === 'base64' ? 'base64' : 'text';
      return get('read', path, as).then(function (j) { return j.data; });
    },
    write: function (path, data, o) {
      var bin = asBase64(data);
      if (bin) return bin.then(function (d) { return post('write', { path: String(path), data: d, as: 'base64' }); });
      var as = (o && o.as) === 'base64' ? 'base64' : 'text';
      return post('write', { path: String(path), data: String(data == null ? '' : data), as: as });
    },
    mkdir: function (path) { return post('mkdir', { path: String(path) }); },
    remove: function (path) { return post('remove', { path: String(path) }); }
  };
})();
