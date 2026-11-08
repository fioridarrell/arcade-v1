/* Arcade V1.0: arcade.storage for app pages (the original arcade's storage.js API). Saves are kept by the
   arcade the app runs in, per app page; the page talks to its window by postMessage. */
(function () {
  'use strict';
  window.arcade = window.arcade || {};
  if (window.arcade.storage) return;
  var items = {}, seq = 0, waiting = {};
  function ask(message, raw) {
    return new Promise(function (resolve, reject) {
      message.arcade = 'storage';
      message.seq = ++seq;
      var t = setTimeout(function () { delete waiting[message.seq]; reject(new Error('no wallet is listening: open this page in a DogecoinArcade viewer')); }, 5000);
      waiting[message.seq] = { resolve: resolve, reject: reject, raw: raw, t: t };
      window.parent.postMessage(message, '*');
    });
  }
  window.addEventListener('message', function (e) {
    var m = e.data;
    if (!m || m.arcade !== 'storage' || !waiting[m.seq]) return;
    var w = waiting[m.seq]; delete waiting[m.seq]; clearTimeout(w.t);
    if (m.ok) {
      if (w.raw) return w.resolve(m.history || {});
      if (m.items) items = m.items;
      w.resolve(storage);
    } else w.reject(new Error(m.error || 'refused'));
  });
  var storage = {
    getItem: function (key) { key = String(key); return Object.prototype.hasOwnProperty.call(items, key) ? items[key] : null; },
    setItem: function (key, value) {
      key = String(key); value = String(value);
      var had = Object.prototype.hasOwnProperty.call(items, key), old = items[key];
      items[key] = value;
      return ask({ op: 'set', key: key, value: value }).catch(function (e) { if (had) items[key] = old; else delete items[key]; throw e; });
    },
    removeItem: function (key) { key = String(key); delete items[key]; return ask({ op: 'remove', key: key }); },
    clear: function () { items = {}; return ask({ op: 'clear' }); },
    history: function (key) { return ask({ op: 'history', key: key === undefined ? '' : String(key) }, true); },
    key: function (i) { return Object.keys(items)[i] || null; },
    get length() { return Object.keys(items).length; },
    all: function () { var o = {}; for (var k in items) o[k] = items[k]; return o; }
  };
  storage.ready = ask({ op: 'load' });
  window.arcade.storage = storage;
})();
