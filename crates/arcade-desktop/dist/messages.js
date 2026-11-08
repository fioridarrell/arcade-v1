// Messages tab: private messages on the home chain (compatible with the original arcade).
(() => {
  const $ = (id) => document.getElementById(id);
  const invoke = window.__TAURI__.core.invoke;
  async function api(cmd, args = {}) {
    const r = await invoke("api", { req: { cmd, ...args } });
    if (!r.ok) throw new Error(r.error || "failed");
    return r.result;
  }
  const h = (tag, attrs = {}, ...kids) => {
    const e = document.createElement(tag);
    for (const [k, v] of Object.entries(attrs || {})) {
      if (v === undefined || v === null || v === false) continue;
      if (k === "class") e.className = v; else if (k.startsWith("on")) e[k] = v; else e.setAttribute(k, v === true ? "" : v);
    }
    for (const k of kids.flat()) if (k !== null && k !== undefined && k !== false) e.append(k.nodeType ? k : document.createTextNode(String(k)));
    return e;
  };
  const when = (t) => {
    if (!t) return "";
    const d = new Date(t * 1000), now = new Date();
    return d.toDateString() === now.toDateString() ? d.toLocaleTimeString([], { hour: "2-digit", minute: "2-digit" }) : d.toLocaleDateString([], { month: "short", day: "numeric" });
  };
  const nameOf = (w) => (w.name ? w.name : w.tag ? "@" + w.tag : w.address ? w.address.slice(0, 10) + "…" : w.fingerprint || "someone");
  const sub = (w) => [w.tag ? "@" + w.tag : null, w.fingerprint].filter(Boolean).join(" · ");
  const PEP = (sats) => (sats / 1e8).toLocaleString(undefined, { maximumFractionDigits: 8 }) + " PEPtest";

  let view = "chats", open = null, composing = false, lastUnread = null, notified = new Set();

  function note(text, bad) { const m = $("mx-msg"); m.textContent = text; m.className = "msg " + (bad ? "bad" : "ok"); }

  async function refreshMe() {
    try {
      const me = await api("msg.me");
      $("mx-me").replaceChildren(
        h("div", { class: "k" }, "Your messaging key"),
        h("div", { class: "mono" }, me.fingerprint),
        h("div", { class: "s" }, me.announced ? "published: people can message you" : "not published yet: it goes out with your @tag"));
    } catch (e) {
      $("mx-me").replaceChildren(h("div", { class: "s" }, e.message.includes("wallet") ? "Create or restore a wallet first (Wallet tab)." : e.message));
    }
  }

  async function refreshList() {
    const items = view === "contacts" ? await api("contacts.list") : await api("msg.conversations", { requests: view === "requests" });
    const list = $("mx-list");
    if (!items.length) {
      list.replaceChildren(h("li", { class: "mx-empty" }, view === "contacts" ? "No contacts yet. Add someone from a conversation, or with New message." : view === "requests" ? "No message requests." : "No conversations yet."));
      return;
    }
    list.replaceChildren(...items.map((c) => {
      const w = view === "contacts" ? c : c.who;
      const peer = view === "contacts" ? c.key : c.peer;
      const li = h("li", { class: "mx-item" + (open === peer ? " on" : "") },
        h("div", { class: "mx-row" }, h("b", {}, nameOf(w)), view === "contacts" ? "" : h("time", {}, when(c.last.at))),
        h("div", { class: "s" }, view === "contacts" ? sub(w) : (c.last.dir === "out" ? "You: " : "") + (c.last.text || (c.last.file ? "📎 " + c.last.file.name : ""))),
        c.unread ? h("span", { class: "badge" }, String(c.unread)) : "");
      li.onclick = () => { open = peer; composing = false; draw(); };
      return li;
    }));
  }

  async function fileView(m) {
    const f = m.file;
    if (!f) return null;
    const box = h("div", { class: "mx-file" }, "📎 ", f.name, h("span", { class: "s" }, ` · ${(f.size / 1024).toFixed(1)} KB`));
    if ((f.type || "").startsWith("image/") && f.size < 4 << 20) {
      try {
        const d = await api("msg.file", { id: m.id });
        box.prepend(h("img", { src: `data:${d.type};base64,${d.data}`, alt: f.name }), h("br"));
      } catch (_) { /* shown as a name */ }
    } else {
      const b = h("button", { class: "small" }, "Save");
      b.onclick = async () => {
        const d = await api("msg.file", { id: m.id });
        const a = h("a", { href: `data:${d.type};base64,${d.data}`, download: d.name });
        document.body.append(a); a.click(); a.remove();
      };
      box.append(" ", b);
    }
    return box;
  }

  async function drawThread() {
    const head = $("mx-head"), body = $("mx-thread");
    if (composing || !open) {
      head.replaceChildren(h("b", {}, composing ? "New message" : "Messages"), composing ? "" : h("span", { class: "s" }, " · pick a conversation or start a new one"));
      body.replaceChildren();
      $("mx-to-row").hidden = !composing;
      $("mx-compose").hidden = !composing;
      return;
    }
    const t = await api("msg.thread", { peer: open });
    const w = t.who;
    const add = h("button", { class: "small" }, w.contact ? "In contacts" : "Add to contacts");
    add.disabled = w.contact;
    add.onclick = async () => { await api("contacts.save", { key: open, name: w.name || (w.tag ? "@" + w.tag : ""), address: w.address }); note("Added to contacts."); draw(); };
    const mute = h("button", { class: "small" }, "Mute");
    mute.disabled = !w.address;
    mute.onclick = async () => { await api("mute.add", { address: w.address }); note(`Muted ${nameOf(w)}. Unmute from Contacts.`); open = null; draw(); };
    head.replaceChildren(h("div", {}, h("b", {}, nameOf(w)), h("div", { class: "s" }, sub(w) + (w.address ? " · " + w.address : ""))), h("div", { class: "mx-acts" }, add, mute));
    const rows = [];
    for (const m of t.messages) {
      const mine = m.dir !== "in";
      const f = await fileView(m);
      rows.push(h("div", { class: "mx-msg " + (mine ? "mine" : "theirs") },
        m.text ? h("div", { class: "mx-text" }, m.text) : "", f,
        h("div", { class: "mx-meta" }, when(m.at), m.height ? "" : " · waiting for a block", m.fee ? ` · fee ${PEP(m.fee)}` : "")));
    }
    body.replaceChildren(...(rows.length ? rows : [h("div", { class: "mx-empty" }, "No messages yet.")]));
    body.scrollTop = body.scrollHeight;
    $("mx-to-row").hidden = true;
    $("mx-compose").hidden = false;
    if (t.messages.some((m) => m.unread)) { await api("msg.read", { peer: open }); refreshBadge(); }
  }

  async function draw() {
    document.querySelectorAll("#mx-views button").forEach((b) => b.classList.toggle("on", b.dataset.v === view));
    try { await Promise.all([refreshMe(), refreshList(), drawThread()]); } catch (e) { note(e.message, true); }
  }

  // Unread badge on the tab and a notification (sender only, never the text).
  async function refreshBadge() {
    try {
      const u = await api("msg.unread");
      const tab = document.querySelector('nav button[data-tab="messages"]');
      tab.querySelector(".badge")?.remove();
      if (u.unread) tab.append(h("span", { class: "badge" }, String(u.unread)));
      $("mx-req").textContent = u.requests ? ` (${u.requests})` : "";
      if (lastUnread !== null && u.unread + u.requests > lastUnread) {
        const cs = [...await api("msg.conversations", { requests: false }), ...await api("msg.conversations", { requests: true })];
        for (const c of cs.filter((c) => c.unread && !notified.has(c.peer + c.last.at))) {
          notified.add(c.peer + c.last.at);
          notify(`New message from ${nameOf(c.who)}`);
        }
        if (document.querySelector(".tab.on")?.id === "messages") draw();
      }
      lastUnread = u.unread + u.requests;
    } catch (_) { /* no wallet yet */ }
  }
  function notify(text) {
    try {
      if (!("Notification" in window)) return;
      if (Notification.permission === "granted") new Notification("Arcade", { body: text });
      else if (Notification.permission !== "denied") Notification.requestPermission().then((p) => p === "granted" && new Notification("Arcade", { body: text }));
    } catch (_) { /* not available here */ }
  }

  // Sending.
  let attached = null;
  $("mx-file").addEventListener("change", (e) => {
    const f = e.target.files[0];
    if (!f) { attached = null; $("mx-fileinfo").textContent = ""; return; }
    const r = new FileReader();
    r.onload = () => {
      attached = { name: f.name, type: f.type || "application/octet-stream", data: String(r.result).split(",")[1] };
      const txs = Math.max(1, Math.ceil((f.size + 300) / 7600));
      $("mx-fileinfo").textContent = `${f.name} · ${(f.size / 1024).toFixed(1)} KB · about ${txs} transaction${txs === 1 ? "" : "s"} each way`;
    };
    r.readAsDataURL(f);
  });
  $("mx-compose").addEventListener("submit", async (e) => {
    e.preventDefault();
    const text = $("mx-text").value.trim();
    if (!text && !attached) return;
    const to = composing ? $("mx-to").value.trim() : open;
    if (!to) { note("Who to? A @tag, an address or a messaging key.", true); return; }
    const btn = $("mx-send"); btn.disabled = true; note("Sending…");
    try {
      const r = await api("msg.send", { to, text, file: attached || undefined });
      $("mx-text").value = ""; $("mx-file").value = ""; attached = null; $("mx-fileinfo").textContent = "";
      note(`Sent in ${r.txids.length} transaction${r.txids.length === 1 ? "" : "s"} (fee ${PEP(r.fee)}). It shows on their side once it's in a block.`);
      open = r.to.key; composing = false; draw();
    } catch (err) { note(err.message, true); }
    btn.disabled = false;
  });
  $("mx-to").addEventListener("change", async () => {
    const v = $("mx-to").value.trim();
    if (!v) return;
    try { const w = await api("msg.who", { name: v }); $("mx-towho").textContent = `${nameOf(w)} · ${sub(w)}`; $("mx-towho").className = "s"; }
    catch (e) { $("mx-towho").textContent = e.message; $("mx-towho").className = "s bad"; }
  });
  $("mx-new").onclick = () => { composing = true; open = null; $("mx-to").value = ""; $("mx-towho").textContent = ""; draw(); $("mx-to").focus(); };
  document.querySelectorAll("#mx-views button").forEach((b) => b.addEventListener("click", () => { view = b.dataset.v; draw(); }));
  document.querySelector('nav button[data-tab="messages"]').addEventListener("click", () => draw());

  window.arcadeMessages = { draw };
  refreshBadge();
  setInterval(refreshBadge, 15000);
  setInterval(() => { if (document.querySelector(".tab.on")?.id === "messages") refreshList().catch(() => {}); }, 15000);
})();
