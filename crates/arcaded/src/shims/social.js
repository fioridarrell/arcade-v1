/* Arcade V1.0: arcade.social for app pages: likes, dislikes, replies and tips on ANY transaction of any chain (a
   post, an inscription, a game's record, an app's own item), kept on the ARCADE chain as A:// ops 8 to 10
   (docs/APROTO.md). Reading is open to any app; reacting, replying, tipping and following (fees and tips paid from
   this arcade's wallet) only when this app's "Feed" switch is on, and every tip is shown on its own card first.
   Answered by the arcade window that framed the page. A target is {chain, txid}: a chain key ("peptest",
   "arctest", "doge", ...) and a transaction id (64 hex). */
(function () {
  'use strict';
  window.arcade = window.arcade || {};
  if (window.arcade.social) return;
  var seq = 0, waiting = {};
  function ask(message, wait) {
    return new Promise(function (resolve, reject) {
      message.arcade = 'social'; message.seq = ++seq;
      var t = setTimeout(function () { delete waiting[message.seq]; reject(new Error('no viewer is listening, or the wallet is still locked')); }, wait || 120000);
      waiting[message.seq] = { resolve: resolve, reject: reject, t: t };
      window.parent.postMessage(message, '*');
    });
  }
  window.addEventListener('message', function (e) {
    if (e.source !== window.parent) return;          // only the arcade window that framed this page
    var m = e.data;
    if (!m || m.arcade !== 'social' || !waiting[m.seq]) return;
    var w = waiting[m.seq]; delete waiting[m.seq]; clearTimeout(w.t);
    if (m.ok) w.resolve(m.result); else w.reject(new Error(m.error || 'refused'));
  });
  window.arcade.social = {
    // targets: [{chain, txid}] (up to 100) -> {targets: [{chain, txid, likes, dislikes, replies,
    //   tips: {total, count, to_author, to: [{address, sats, count, author}]}, author: {address} | null,
    //   mine: {react: "like" | "dislike" | null, tipped}}]}
    reactions: function (targets) { return ask({ op: 'reactions', targets: targets || [] }); },
    // -> {replies: [{txid, author: {address, tag}, text, height}], total, offset}
    replies: function (target, opts) { opts = opts || {}; return ask({ op: 'replies', target: target || {}, offset: opts.offset || 0, limit: opts.limit || 50 }); },
    // kind: like, unlike, dislike, undislike -> {txid, fee}
    react: function (target, kind) { return ask({ op: 'react', target: target || {}, kind: kind }, 600000); },
    // text: 1 to 500 bytes -> {txid, fee}
    reply: function (target, text) { return ask({ op: 'reply', target: target || {}, text: text }, 600000); },
    // amount: sats of ARCADE; opts.to: an ARCADE address (default: the target's author, where one is found); opts.memo
    tip: function (target, amount, opts) { opts = opts || {}; return ask({ op: 'tip', target: target || {}, amount: amount, to: opts.to || '', memo: opts.memo || '' }, 600000); },
    follow: function (tag) { return ask({ op: 'follow', tag: tag, on: true }, 600000); },
    unfollow: function (tag) { return ask({ op: 'follow', tag: tag, on: false }, 600000); }
  };
})();
