/* Trades between two players of one room (arcade.swap.trade), for app windows: the original arcade's
   protocol, message for message, so a player here trades with a player on any arcade.

   The proposal, the yes and the receipt travel as hidden messages in the room both players are in
   ("\u001farcade-trade:" + JSON, never shown to the page); the signed leg travels sealed to the other
   player's messaging key. Each player says yes in their own window, and their own arcade signs
   (trade.leg / trade.fill): the page only hears statuses. One transaction settles it.

   Who signs the leg: the side giving an inscription (both: the proposer); with none, the proposer. That
   side gives an inscription or a token, never coins. Bundles (several things a side) are not here yet. */
(function () {
  "use strict";
  const TRADE = "\u001farcade-trade:", PART = 360, MOST_PARTS = 64;

  window.ArcadeTrade = function (env) {
    const { api, rooms, toPage, card, oneCard, walletGate } = env;
    const trades = {};
    const proposals = {};
    const nft = (l) => !!(l && l.inscription);
    const iSign = (t) => (nft(t.give) ? t.role === "proposer" || !nft(t.get) : !nft(t.get) && t.role === "proposer");
    const pause = (ms) => new Promise((r) => setTimeout(r, ms));
    const roomWith = (id) => { for (const k in rooms) if (rooms[k].members && rooms[k].members[id]) return k; return ""; };
    const who = (p) => (p && p.tag ? "@" + p.tag : "the other player");
    function words(l) {
      if (!l) return "nothing";
      if (l.inscription) return "inscription " + String(l.inscription).slice(0, 12);
      if (l.token) return String(l.amount) + " of token #" + l.token;
      if (l.coins) return String(l.coins) + " coins";
      return "something";
    }
    function say(t, status, extra) {
      t.status = status;
      toPage({ arcade: "swap", event: { type: "trade", id: t.id, status, with: t.peer, give: t.give, get: t.get, role: t.role, ...(extra || {}) } });
    }
    async function send(t, msg) {
      const s = rooms[t.room];
      if (!s) throw new Error("you left the room this trade was in");
      msg.id = t.id; msg.to = t.peer.id;
      await api("realtime.send", { token: s.token, data: TRADE + JSON.stringify(msg) });
    }
    function fail(t, err, quiet) {
      const why = String((err && err.message) || err || "failed");
      say(t, "failed", { why });
      if (!quiet) send(t, { t: "failed", why }).catch(() => {});
    }
    const ask = (t, title, button, more) => oneCard(() => card(title, [
      `You give ${words(t.give)} and get ${words(t.get)}, with ${who(t.peer)}. One transaction: both move, or neither does.`, ...(more || [])], button));

    async function sign(t) {
      try {
        if (!(await walletGate("trade with another player"))) throw new Error("cancelled");
        const r = await api("trade.leg", { give: t.give, take: t.get, buyer: t.peer.address });
        const parts = r.sealed.match(new RegExp(`.{1,${PART}}`, "g")) || [];
        for (let i = 0; i < parts.length; i++) {
          await send(t, { t: "leg", n: i, of: parts.length, part: parts[i] });
          await pause(250);                              // the room's pace
        }
        say(t, "signed");
      } catch (err) { fail(t, err); }
    }
    async function finish(t) {
      try {
        if (!(await walletGate("trade with another player"))) throw new Error("cancelled");
        const got = await api("trade.open", { sealed: t.parts.join("") });
        const leg = got && got.leg;
        if (!leg || leg.seller !== t.peer.address) throw new Error("that leg is not from the player you are trading with");
        const expect = { give: t.get, take: t.give };      // the leg's own view: the signer gives what this player gets
        const shown = await api("trade.fill", { leg, expect, peer: t.peer.address });
        if (!(await ask(t, "Finish the trade?", "Finish", [`The network fee is ${shown.fee_text}.`]))) throw new Error("cancelled");
        const done = await api("trade.fill", { leg, expect, peer: t.peer.address, send: true });
        await send(t, { t: "done", txid: done.txid }).catch(() => {});
        say(t, "broadcast", { txid: done.txid });
        watch(t, done.txid);
      } catch (err) { fail(t, err); }
    }
    function watch(t, txid) {
      const until = Date.now() + 30 * 60 * 1000;
      const piece = nft(t.give) ? t.give.inscription : nft(t.get) ? t.get.inscription : "";
      const me = rooms[t.room] && rooms[t.room].me;
      const owner = nft(t.give) ? t.peer.address : me && me.address;
      (async function tick() {
        let r = null;
        try { r = await api("trade.watch", { txid, inscription: piece, owner: piece ? owner : "" }); } catch (err) {}
        if (r && r.state === "settled") return say(t, "settled", { txid });
        if (r && r.state === "failed") return fail(t, new Error(r.why), true);
        if (Date.now() < until) setTimeout(tick, 10000);
      })();
    }

    // A hidden trade message heard in a room.
    function heard(room, from, text) {
      let msg; try { msg = JSON.parse(text); } catch (err) { return; }
      const s = rooms[room];
      if (!msg || !from || !s || !s.me || msg.to !== s.me.id || typeof msg.id !== "string") return;
      if (msg.t === "propose-part") {
        const of = msg.of | 0, n = msg.n | 0;
        if (trades[msg.id] || !(of > 0 && of <= MOST_PARTS && n >= 0 && n < of) || from.guest || s.me.guest) return;
        const got = proposals[msg.id] || (proposals[msg.id] = []);
        got[n] = String(msg.part || "");
        for (let i = 0; i < of; i++) if (typeof got[i] !== "string") return;
        delete proposals[msg.id];
        let whole; try { whole = JSON.parse(got.join("")); } catch (err) { return; }
        msg = { ...whole, id: msg.id, t: "propose" };
      }
      let t = trades[msg.id];
      if (msg.t === "propose") {
        if (t || from.guest || s.me.guest) return;
        if (Array.isArray(msg.give) || Array.isArray(msg.get)) return;   // bundles: not here yet
        t = trades[msg.id] = { id: msg.id, role: "responder", room, peer: from, give: msg.get, get: msg.give, parts: [] };
        say(t, "incoming");
        return;
      }
      if (!t || from.id !== t.peer.id) return;
      if (msg.t === "accept" && t.role === "proposer" && t.status === "proposed") {
        say(t, "accepted");
        if (iSign(t)) sign(t);
      } else if (msg.t === "decline") say(t, "declined", { why: msg.why || "" });
      else if (msg.t === "cancel") say(t, "cancelled");
      else if (msg.t === "failed") say(t, "failed", { why: msg.why || "" });
      else if (msg.t === "leg" && !iSign(t) && !t.finishing) {
        const of = msg.of | 0, n = msg.n | 0;
        if (!(of > 0 && of <= MOST_PARTS && n >= 0 && n < of)) return;
        t.parts[n] = String(msg.part || "");
        let have = 0; for (let i = 0; i < of; i++) if (typeof t.parts[i] === "string") have++;
        if (have === of) { t.finishing = true; t.parts.length = of; finish(t); }
      } else if (msg.t === "done" && iSign(t) && typeof msg.txid === "string") {
        say(t, "broadcast", { txid: msg.txid });
        watch(t, msg.txid);
      }
    }

    async function door(m) {
      if (m.op === "items") return await api("trade.items");
      if (m.op === "trade") {
        const peerId = String(m.with || ""), room = roomWith(peerId), s = rooms[room];
        if (!s) throw new Error("trade with a player in a room you are in");
        const peer = s.members[peerId];
        if (!s.me || s.me.guest || !s.me.address) throw new Error("sign in to trade");
        if (!peer || peer.guest || !peer.address) throw new Error("that player is a guest and cannot trade");
        if (!m.give || !m.get) throw new Error("a trade needs something on each side");
        if (Array.isArray(m.give) || Array.isArray(m.get)) throw new Error("trades of several things a side are not on this arcade yet");
        if (m.give.coins && m.get.coins) throw new Error("coins for coins is no trade");
        const t = { id: Math.random().toString(36).slice(2) + Date.now().toString(36), role: "proposer", room, peer, give: m.give, get: m.get, parts: [] };
        if (iSign(t) && m.give.coins) throw new Error("the side that signs gives an inscription or a token: put the coins on the other side");
        const reader = iSign(t) ? peer : s.me;          // who opens the sealed leg needs a published key
        await api("trade.who", { address: reader.address }).catch(() => { throw new Error(reader === peer ? "that player has not published a messaging key yet" : "publish your messaging key first (Messages tab)"); });
        if (!(await ask(t, "Offer this trade?", "Offer it"))) throw new Error("cancelled");
        trades[t.id] = t;
        const whole = JSON.stringify({ give: m.give, get: m.get });
        if (whole.length <= 300) await send(t, { t: "propose", give: m.give, get: m.get });
        else {
          const parts = whole.match(/.{1,300}/g);
          for (let i = 0; i < parts.length; i++) { await send(t, { t: "propose-part", n: i, of: parts.length, part: parts[i] }); await pause(250); }
        }
        say(t, "proposed");
        return { id: t.id };
      }
      const t = trades[String(m.id || "")];
      if (!t) throw new Error("no such trade");
      if (m.op === "answer") {
        if (t.role !== "responder" || t.status !== "incoming") throw new Error("that trade is not waiting for your answer");
        if (!m.accept) { await send(t, { t: "decline", why: String(m.why || "") }); say(t, "declined", { why: String(m.why || "") }); return {}; }
        if (!(await ask(t, "Accept this trade?", "Accept"))) { await send(t, { t: "decline", why: "" }); say(t, "declined", { why: "" }); return {}; }
        await send(t, { t: "accept" });
        say(t, "accepted");
        if (iSign(t)) sign(t);
        return {};
      }
      if (m.op === "cancel") {
        if (["broadcast", "settled"].includes(t.status)) throw new Error("that trade has gone out already");
        await send(t, { t: "cancel" }).catch(() => {});
        say(t, "cancelled");
        return {};
      }
      throw new Error("swap ops are trade, answer, cancel and items (shops are not on this arcade yet)");
    }

    return { door, heard };
  };
})();
