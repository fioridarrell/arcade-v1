/* Arcade V1.0: arcade.llm for app pages (the original arcade's llm.js API, docs/compat/APP_API.md B4): shared AI
   models on the mesh, paid from the player's ARCADE credit at each seller. Since the marketplace moved into the Models
   app, the window answers it over arcade.svc (docs/SVC.md): offers from the mesh, chats through this computer's
   arcade-models helper (credit isn't listed here: the Models app shows it).

     arcade.llm.offers()  -> [{seller, here, free, name, tag, models:[{id}], prices:[{leg, label, model, in, out}], max_out, credit}]
     arcade.llm.chat(req) -> {content, reasoning, usage:{tokens_in, tokens_out}, cost, left, seller, model, leg}
       req = OpenAI chat fields (messages, max_tokens, temperature, ...) + seller? (a node id or @tag) + model?
       (a bare model: a seller with credit, else free, else the cheapest). The player says yes once per window.
     arcade.llm.image()   -> rejects: pictures aren't shared yet. */
(function () {
  'use strict';
  window.arcade = window.arcade || {};
  if (window.arcade.llm) return;
  var seq = 0, waiting = {};

  function ask(message) {
    return new Promise(function (resolve, reject) {
      message.arcade = 'llm';
      message.seq = ++seq;
      waiting[message.seq] = { resolve: resolve, reject: reject };
      window.parent.postMessage(message, '*');
    });
  }

  window.addEventListener('message', function (e) {
    if (e.source !== window.parent) return;
    var m = e.data || {};
    if (m.arcade !== 'llm' || !waiting[m.seq]) return;
    var w = waiting[m.seq];
    delete waiting[m.seq];
    if (m.ok) w.resolve(m); else w.reject(new Error(m.error || 'refused'));
  });

  window.arcade.llm = {
    offers: function () { return ask({ op: 'offers' }).then(function (m) { return m.offers || []; }); },
    chat: function (req) {
      return ask({ op: 'chat', req: req || {} }).then(function (m) {
        return { content: m.content, reasoning: m.reasoning, usage: m.usage, cost: m.cost, left: m.left, seller: m.seller, model: m.model, leg: m.leg };
      });
    },
    image: function () { return Promise.reject(new Error("pictures aren't shared on this arcade yet")); }
  };
})();
