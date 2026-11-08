/* Arcade V1.0: arcade.groups for app pages (the original arcade's groups.js API): your private groups,
   answered by the arcade window only when this app's "Group chats" switch is on. */
(function () {
  'use strict';
  window.arcade = window.arcade || {};
  if (window.arcade.groups) return;
  var seq = 0, waiting = {};
  function ask(message, wait) {
    return new Promise(function (resolve, reject) {
      message.arcade = 'groups'; message.seq = ++seq;
      var t = setTimeout(function () { delete waiting[message.seq]; reject(new Error('no viewer is listening, or the wallet is still locked')); }, wait || 120000);
      waiting[message.seq] = { resolve: resolve, reject: reject, t: t };
      window.parent.postMessage(message, '*');
    });
  }
  window.addEventListener('message', function (e) {
    var m = e.data;
    if (!m || m.arcade !== 'groups' || !waiting[m.seq]) return;
    var w = waiting[m.seq]; delete waiting[m.seq]; clearTimeout(w.t);
    if (m.ok) w.resolve(m); else w.reject(new Error(m.error || 'refused'));
  });
  window.arcade.groups = {
    list: function () { return ask({ op: 'list' }); },
    read: function (id, o) { o = o || {}; return ask({ op: 'read', group: id, after: o.after || 0, limit: o.limit || 200 }).then(function (r) { return r.lines; }); },
    send: function (id, text) { return ask({ op: 'send', group: id, text: text }); },
    leave: function (id) { return ask({ op: 'leave', group: id }, 600000); },
    seal: function (id, text) { return ask({ op: 'seal', group: id, text: text }).then(function (r) { return r.sealed; }); },
    open: function (id, sealed) { return ask({ op: 'open', group: id, sealed: sealed }).then(function (r) { return r.text; }); },
    people: function () { return ask({ op: 'people' }).then(function (r) { return r.people; }); },
    add: function (id, members) { return ask({ op: 'add', group: id, members: members }, 600000).then(function (r) { return r.group; }); },
    create: function (name, members) { return ask({ op: 'create', name: name, members: members }, 600000).then(function (r) { return r.group; }); },
    save: function (name, bytes) { return ask({ op: 'save', name: name, data: bytes }); }
  };
})();
