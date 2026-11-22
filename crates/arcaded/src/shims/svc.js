/* Arcade V1.0: arcade.svc for app pages: services over the arcade mesh (docs/SVC.md), such as the AI model
   marketplace's helper (arcade-models). The arcade carries them; it knows nothing about what a service does.

     arcade.svc.offers(service)            -> [{service, arcade, here, tag, name, pay_to, linked, at, offer}]
     arcade.svc.ask(to, service, body, o?) -> {last, answers:[{ev, blob?}]}   (o = {timeout}: seconds)
       asks the service at arcade `to` (a node id) in the player's name (its ARCADE address); the player says
       yes once per window and service.
     arcade.svc.local(service, body)       -> the answer of this computer's own helper for that service (its owner's
       operations: settings, buying, keys...). The player says yes once per window and service; each payment and each
       new key also asks on its own card. A payment ({what: "buy", amount}) within the allowance the person gave this app
       for the service goes without a card.
     arcade.svc.allowance(service)         -> {per_day, spent_today, left, payments} (ARCADE, today UTC)
     arcade.svc.askAllowance(service, perDay, reason?) -> the same, after the person says yes on a card
       ("<app> asks to spend up to N ARCADE a day on AI models"). */
(function () {
  'use strict';
  window.arcade = window.arcade || {};
  if (window.arcade.svc) return;
  var seq = 0, waiting = {};

  function ask(message) {
    return new Promise(function (resolve, reject) {
      message.arcade = 'svc';
      message.seq = ++seq;
      waiting[message.seq] = { resolve: resolve, reject: reject };
      window.parent.postMessage(message, '*');
    });
  }

  window.addEventListener('message', function (e) {
    if (e.source !== window.parent) return;
    var m = e.data || {};
    if (m.arcade !== 'svc' || !waiting[m.seq]) return;
    var w = waiting[m.seq];
    delete waiting[m.seq];
    if (m.ok) w.resolve(m); else w.reject(new Error(m.error || 'refused'));
  });

  window.arcade.svc = {
    offers: function (service) { return ask({ op: 'offers', service: service }).then(function (m) { return m.offers || []; }); },
    ask: function (to, service, body, o) {
      return ask({ op: 'ask', to: to, service: service, body: body || {}, timeout: (o && o.timeout) || 600 })
        .then(function (m) { return { last: m.last, answers: m.answers || [] }; });
    },
    local: function (service, body) { return ask({ op: 'local', service: service, body: body || {} }).then(function (m) { return m.result; }); },
    allowance: function (service) { return ask({ op: 'allowance', service: service }).then(function (m) { return m.allowance; }); },
    askAllowance: function (service, perDay, reason) {
      return ask({ op: 'ask_allowance', service: service, per_day: perDay, reason: reason || '' }).then(function (m) { return m.allowance; });
    }
  };
})();
