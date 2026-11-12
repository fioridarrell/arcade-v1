/* Arcade V1.0: arcade.app for app pages (the original arcade's app.js API): open / minimized / restored / closing. */
(function () {
  'use strict';
  window.arcade = window.arcade || {};
  if (window.arcade.app) return;
  var now = { state: 'open', timeout: null, closesAt: null, inShell: false }, fns = [], seq = 0, waiting = {};
  function strip(m) { var o = {}; for (var k in m) if (k !== 'arcade' && k !== 'seq' && k !== 'event' && k !== 'ok') o[k] = m[k]; return o; }
  window.addEventListener('message', function (e) {
    if (e.source !== window.parent) return;          // only the arcade window that framed this page
    var m = e.data;
    if (!m || m.arcade !== 'app') return;
    if (m.event) {
      now = strip(m);
      fns.slice().forEach(function (fn) { try { fn(now); } catch (err) { console.error(err); } });
      window.dispatchEvent(new CustomEvent('arcade:app', { detail: now }));
      return;
    }
    var w = waiting[m.seq]; if (!w) return; delete waiting[m.seq]; clearTimeout(w.t);
    if (m.ok) { now = strip(m); w.resolve(now); } else w.reject(new Error(m.error || 'refused'));
  });
  window.arcade.app = {
    on: function (fn) { fns.push(fn); return fn; },
    off: function (fn) { fns = fns.filter(function (f) { return f !== fn; }); },
    state: function () {
      if (window.parent === window) return Promise.reject(new Error('no viewer: open this page in the arcade'));
      return new Promise(function (resolve, reject) {
        var s = ++seq;
        var t = setTimeout(function () { delete waiting[s]; reject(new Error('no viewer: nothing answered; open this page in the arcade')); }, 15000);
        waiting[s] = { resolve: resolve, reject: reject, t: t };
        window.parent.postMessage({ arcade: 'app', seq: s, op: 'state' }, '*');
      });
    }
  };
})();
