// Rings: somebody in the address book rings from an app whose Notifications switch is on
// (arcade.realtime.ring). Asking the arcade what is ringing also tells it this window is open,
// which is when it lets itself be rung. A ring shows as a card here, and as a desktop
// notification; Open starts the app, which can then ask what rang (arcade.realtime.ringed).
(() => {
  const box = h("div", { id: "rings" });
  document.body.append(box);
  const shown = new Map();      // ring id -> its card
  const closed = new Set();     // rings the person dismissed or opened

  function notify(r) {
    const text = `${r.from} is ringing you in ${r.app || "an app"}${r.line ? ": " + r.line : ""}`;
    try {
      if (!("Notification" in window)) return;
      if (Notification.permission === "granted") new Notification("Arcade", { body: text });
      else if (Notification.permission !== "denied") Notification.requestPermission().then((p) => p === "granted" && new Notification("Arcade", { body: text }));
    } catch (_) { /* not available here */ }
  }

  async function open(r, card) {
    try {
      const l = await api("apps.launch", { chain: r.chain, txid: r.txid });
      await invoke("open_app", { id: l.id, title: l.title, frame: l.frame, ring: r.id });
      done(r.id);
    } catch (e) { card.querySelector(".ring-line").textContent = e.message; }
  }

  function done(id) {
    closed.add(id);
    shown.get(id)?.remove();
    shown.delete(id);
  }

  function draw(r) {
    const card = h("div", { class: "ring" },
      h("div", { class: "ring-who" }, h("b", {}, r.from), " is ringing you"),
      h("div", { class: "ring-app" }, r.app || "an app"),
      h("div", { class: "ring-line" }, r.line || ""),
      h("div", { class: "ring-btns" },
        h("button", { onclick: () => done(r.id) }, "Not now"),
        h("button", { class: "primary", onclick: () => open(r, card) }, "Open")));
    return card;
  }

  async function look() {
    let rings = [];
    try { rings = (await api("ring.waiting")).rings || []; } catch (_) { return; }
    const now = new Set(rings.map((r) => r.id));
    for (const [id, card] of shown) if (!now.has(id)) { card.remove(); shown.delete(id); }
    for (const r of rings) {
      if (shown.has(r.id) || closed.has(r.id)) continue;
      const card = draw(r);
      shown.set(r.id, card);
      box.append(card);
      notify(r);
    }
  }
  look();
  setInterval(look, 5000);
})();
