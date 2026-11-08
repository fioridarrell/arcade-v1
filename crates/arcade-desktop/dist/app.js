// Arcade V1.0 desktop: everything goes through arcaded's local API (one JSON request each).
const $ = (id) => document.getElementById(id);
const invoke = window.__TAURI__.core.invoke;

async function api(cmd, args = {}) {
  const r = await invoke("api", { req: { cmd, ...args } });
  if (!r.ok) throw new Error(r.error || "failed");
  return r.result;
}

const pep = (sats) => (sats / 1e8).toLocaleString(undefined, { maximumFractionDigits: 8 }) + " PEP";
const short = (s, n = 12) => (s && s.length > n ? s.slice(0, n) + "…" : s || "");
function msg(el, text, kind = "") { el.textContent = text; el.className = "msg " + kind; }
function rows(tbody, items, cells) {
  tbody.replaceChildren(...items.map((it) => {
    const tr = document.createElement("tr");
    cells(it).forEach(([text, cls]) => { const td = document.createElement("td"); td.textContent = text; if (cls) td.className = cls; tr.append(td); });
    return tr;
  }));
}

// Tabs
document.querySelectorAll("nav button").forEach((b) => b.addEventListener("click", () => {
  document.querySelectorAll("nav button").forEach((x) => x.classList.toggle("on", x === b));
  document.querySelectorAll(".tab").forEach((t) => t.classList.toggle("on", t.id === b.dataset.tab));
  refresh();
}));

let state = { wallet: false, keys: [], claims: [] };

async function refreshHome() {
  const s = await api("status");
  state.wallet = s.wallet;
  $("conn").textContent = "connected"; $("conn").className = "pill ok";
  $("h-sync").textContent = s.sync;
  $("h-tip").textContent = s.tip ? `block ${s.tip.height.toLocaleString()} · ${short(s.tip.hash, 16)}` : "";
  $("h-mesh").textContent = `${s.mesh.links} arcade${s.mesh.links === 1 ? "" : "s"} linked`;
  $("h-node").textContent = "node " + short(s.node_id, 16);
  $("h-tags").textContent = `${s.tags.count} tags`;
  const same = s.mesh.tips.filter((t) => t.names_digest === s.tags.names_digest).length;
  const told = s.mesh.tips.filter((t) => t.names_digest).length;
  $("h-match").textContent = told ? `matches ${same} of ${told} arcades` : "no arcade has reported yet";
  $("h-wallet").textContent = s.wallet ? "ready" : "not created";

  let bal = 0, tag = null;
  if (s.wallet) {
    state.keys = await api("wallet.keys");
    bal = state.keys.reduce((a, k) => a + k.balance, 0);
    tag = state.keys.find((k) => k.tag)?.tag;
    $("h-bal").textContent = pep(bal) + (tag ? ` · @${tag}` : "");
  } else { $("h-bal").textContent = ""; }
  state.claims = await api("tag.list");

}

async function refreshWallet() {
  $("w-none").hidden = state.wallet; $("w-keys").hidden = !state.wallet;
  if (!state.wallet) return;
  if (!state.keys.length) state.keys = await api("wallet.keys");
  rows($("w-rows"), state.keys, (k) => [[String(k.index)], [k.address], [k.tag ? "@" + k.tag : "—"], [pep(k.balance), "num"]]);
}

async function refreshHandle() {
  rows($("t-rows"), state.claims.slice().reverse(), (c) => [["@" + c.tag], [c.status], [c.note]]);
}

async function refreshMine() {
  const m = await api("mine.status");
  $("m-state").textContent = m.running ? `mining (${m.threads} threads)` : "stopped";
  $("m-rate").textContent = m.hash_rate ? Math.round(m.hash_rate).toLocaleString() + " H/s" : "—";
  $("m-found").textContent = m.found;
}

async function refreshActivity() {
  const ev = await api("events");
  $("a-list").replaceChildren(...ev.map((e) => {
    const li = document.createElement("li");
    const t = document.createElement("time"); t.textContent = new Date(e.at * 1000).toLocaleTimeString();
    const k = document.createElement("span"); k.className = "kind"; k.textContent = e.kind;
    li.append(t, k, document.createTextNode(e.text));
    return li;
  }));
}

async function refresh() {
  try {
    await refreshHome();
    const tab = document.querySelector(".tab.on").id;
    if (tab === "wallet") await refreshWallet();
    if (tab === "handle") await refreshHandle();
    if (tab === "mine") await refreshMine();
    if (tab === "activity") await refreshActivity();
  } catch (e) {
    $("conn").textContent = "service not running"; $("conn").className = "pill bad";
  }
}

// Wallet actions
$("w-create").addEventListener("click", async () => {
  try {
    const r = await api("wallet.create");
    $("w-phrase-words").textContent = r.phrase;
    $("w-none").hidden = true; $("w-phrase").hidden = false;
  } catch (e) { msg($("w-msg"), e.message, "bad"); }
});
$("w-saved").addEventListener("change", (e) => { $("w-done").disabled = !e.target.checked; });
$("w-done").addEventListener("click", () => { $("w-phrase-words").textContent = ""; $("w-phrase").hidden = true; refresh(); });
$("w-restore").addEventListener("click", async () => {
  try { await api("wallet.create", { phrase: $("w-words").value }); $("w-words").value = ""; msg($("w-msg"), "Wallet restored.", "ok"); refresh(); }
  catch (e) { msg($("w-msg"), e.message, "bad"); }
});
$("w-faucet").addEventListener("click", async () => {
  try { await api("faucet.ask"); msg($("w-msg"), "Asked the mesh for test coins; any arcade with spare coins can send them.", "ok"); }
  catch (e) { msg($("w-msg"), e.message, "bad"); }
});

// Handle actions
$("t-tag").addEventListener("input", () => { $("t-claim").disabled = true; msg($("t-msg"), ""); });
$("t-check").addEventListener("click", async () => {
  try {
    const r = await api("tag.check", { tag: $("t-tag").value });
    $("t-claim").disabled = !r.available;
    msg($("t-msg"), r.available ? `@${r.tag} is free.` : `@${r.tag} is not available: ${r.reason}`, r.available ? "ok" : "bad");
  } catch (e) { msg($("t-msg"), e.message, "bad"); }
});
$("t-claim").addEventListener("click", async () => {
  try {
    const c = await api("tag.claim", { tag: $("t-tag").value });
    $("t-claim").disabled = true;
    msg($("t-msg"), `Claim for @${c.tag}: ${c.note}`, "ok");
    refresh();
  } catch (e) { msg($("t-msg"), e.message, "bad"); }
});

// Mining
$("m-start").addEventListener("click", async () => { try { await api("mine.start"); refresh(); } catch (e) { alert(e.message); } });
$("m-stop").addEventListener("click", async () => { await api("mine.stop"); refresh(); });

refresh();
setInterval(refresh, 4000);

// Open a tab from the address (#explorer, #chains, ...), e.g. from a link or a shortcut.
(() => {
  const t = (location.hash || "").slice(1).split("/")[0];
  const b = t && document.querySelector(`nav button[data-tab="${t}"]`);
  if (b) b.click();
})();
