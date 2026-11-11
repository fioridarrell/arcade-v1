/* Arcade V1.0: arcade.swap for app pages: trades between two players of one room, the original arcade's
   calls (templates/swap.js). Shops are not on this arcade yet: those calls say so instead of waiting.

     arcade.swap.trade({with, give, get})  -> {id}       offer `give` for `get` to the player `with` (a room member id)
     arcade.swap.answer(id, accept, why)                  yes or no to a trade offered to you
     arcade.swap.cancel(id)
     arcade.swap.items()  -> {address, inscriptions, balances}
     arcade.swap.onTrade(fn)  fn({id, status, with, give, get, role, why, txid})
       status: proposed, incoming, accepted, signed, broadcast, settled, declined, cancelled, failed

   A side is {inscription: id}, {token: id, amount: "25"} or {coins: "1.5"}. Each player says yes in their own
   window, which signs with their own key; the page only hears statuses. One transaction settles it: both
   move or neither does. */
(function () {
  'use strict';
  window.arcade = window.arcade || {};
  if (window.arcade.swap) return;
  var seq = 0, waiting = {}, onTrade = [];

  function ask(message, wait) {
    return new Promise(function (resolve, reject) {
      message.arcade = 'swap';
      message.seq = ++seq;
      waiting[message.seq] = { resolve: resolve, reject: reject };
      setTimeout(function () {
        if (waiting[message.seq]) {
          delete waiting[message.seq];
          reject(new Error('no wallet is listening: open this page in an arcade'));
        }
      }, wait || 60000);
      window.parent.postMessage(message, '*');
    });
  }

  window.addEventListener('message', function (e) {
    var m = e.data || {};
    if (m.arcade === 'swap' && m.event && m.event.type === 'trade') {
      onTrade.forEach(function (fn) { try { fn(m.event); } catch (err) { setTimeout(function () { throw err; }); } });
      return;
    }
    if (m.arcade !== 'swap' || !waiting[m.seq]) return;
    var w = waiting[m.seq];
    delete waiting[m.seq];
    if (m.ok) w.resolve(m); else w.reject(new Error(m.error || 'refused'));
  });

  function describe(l) {
    if (!l) return '';
    if (l.kind === 'coins' || l.coins) return (l.amount || l.coins) + ' coins';
    if (l.kind === 'token' || l.token) return (l.amount || '') + ' ' + (l.name || ('token ' + (l.propertyid || l.token)));
    if (l.kind === 'random' || l.pick) return 'a random ' + (l.collection || 'piece');
    if (l.kind === 'inscription' || l.inscription) return 'inscription #' + (l.number || l.inscription);
    return 'something';
  }
  var no = function () { return Promise.reject(new Error('shops are not on this arcade yet')); };
  var api = {
    describe: describe,
    trade: function (t) { t = t || {}; return ask({ op: 'trade', with: String(t.with || ''), give: t.give, get: t.get }, 600000); },
    answer: function (id, accept, why) { return ask({ op: 'answer', id: id, accept: !!accept, why: why || '' }, 600000); },
    cancel: function (id) { return ask({ op: 'cancel', id: id }); },
    items: function () { return ask({ op: 'items' }); },
    onTrade: function (fn) { onTrade.push(fn); return api; },
  };
  ['shop', 'offer', 'awaitOffer', 'accept', 'status', 'awaitDecision', 'awaitSwap', 'buy'].forEach(function (k) { api[k] = no; });
  window.arcade.swap = api;
})();
