/* Arcade V1.0: arcade.files for app pages: save a file the app made (a video, a picture, an export) to this
   computer's Downloads folder. An app's frame cannot save files itself, so the arcade window does it. The first
   file in a window asks once ("Let <app> save files?"); after a yes, the app saves until the window closes.
   Needs no switch on the app's card. Outside the arcade window (a plain tab) it rejects. */
(function () {
  'use strict';
  window.arcade = window.arcade || {};
  if (window.arcade.files) return;
  var seq = 0, waiting = {};
  function ask(message, wait) {
    return new Promise(function (resolve, reject) {
      message.arcade = 'files'; message.seq = ++seq;
      var t = setTimeout(function () { delete waiting[message.seq]; reject(new Error('no arcade window is listening')); }, wait || 600000);
      waiting[message.seq] = { resolve: resolve, reject: reject, t: t };
      window.parent.postMessage(message, '*');
    });
  }
  window.addEventListener('message', function (e) {
    if (e.source !== window.parent) return;          // only the arcade window that framed this page
    var m = e.data;
    if (!m || m.arcade !== 'files' || !waiting[m.seq]) return;
    var w = waiting[m.seq]; delete waiting[m.seq]; clearTimeout(w.t);
    if (m.ok) w.resolve(m); else w.reject(new Error(m.error || 'refused'));
  });
  window.arcade.files = {
    // bytes: an ArrayBuffer, a typed array or a Blob. Resolves {saved: name as kept} (a number is added when a file
    // of that name is already there); rejects 'declined', 'too big: ...' or 'nothing to save'.
    save: function (name, bytes) {
      var go = function (buf) { return ask({ op: 'save', name: name, data: buf }); };
      if (bytes && typeof Blob !== 'undefined' && bytes instanceof Blob) return bytes.arrayBuffer().then(go);
      var data = bytes && bytes.buffer ? bytes.buffer.slice(bytes.byteOffset, bytes.byteOffset + bytes.byteLength) : bytes;
      return go(data);
    }
  };
})();
