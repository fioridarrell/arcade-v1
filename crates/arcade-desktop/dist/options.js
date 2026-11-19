// Options: automatic updates (off unless the operator turns them on).
const OPT = { busy: false };

function agoText(t) {
  if (!t) return "not yet";
  const s = Math.max(0, Math.floor(Date.now() / 1000) - t);
  if (s < 90) return "just now";
  if (s < 5400) return `${Math.round(s / 60)} minutes ago`;
  return `${Math.round(s / 3600)} hours ago`;
}

async function refreshCache() {
  try {
    const c = await api("arc.cache");
    $("o-cache-mb").textContent = c.on ? `${c.mb} MB` : "off";
    $("o-cache-use").textContent = `${c.files} files, ${(c.bytes / 1048576).toFixed(1)} MB`;
    if (document.activeElement !== $("o-cache-in")) $("o-cache-in").value = c.mb;
  } catch (e) {
    $("o-cache-mb").textContent = "—";
    $("o-cache-use").textContent = "the ARCADE chain isn't running here";
  }
}
$("o-cache-set").addEventListener("click", async () => {
  const mb = Math.max(0, Math.floor(Number($("o-cache-in").value) || 0));
  try { await api("arc.cache", { mb }); say("o-cache-msg", mb ? `Blob cache set to ${mb} MB.` : "Blob cache off."); }
  catch (e) { say("o-cache-msg", e.message, true); }
  refreshCache();
});

// Relayed links (mesh.relay): the switch, the month's limit, the meter, and the links passed now.
async function refreshRelay() {
  try {
    const r = await api("mesh.relay");
    $("o-relay-state").textContent = r.on ? (r.over ? "on, limit reached" : "on") : "off";
    $("o-relay-month").textContent = `${r.used_gb} of ${r.limit_gb} GB`;
    $("o-relay-today").textContent = `${r.today_gb} GB`;
    $("o-relay-links").textContent = `${r.passing.length} of ${r.max_links}`;
    if (document.activeElement !== $("o-relay-on")) $("o-relay-on").checked = !!r.on;
    if (document.activeElement !== $("o-relay-gb")) $("o-relay-gb").value = r.limit_gb;
    if (document.activeElement !== $("o-relay-max")) $("o-relay-max").value = r.max_links;
    if (document.activeElement !== $("o-relay-price")) $("o-relay-price").value = r.price_per_gb;
    if (document.activeElement !== $("o-relay-free")) $("o-relay-free").value = r.free_mb;
    $("o-relay-earned").textContent = `${r.earned} ARCADE` + (r.waiting_payments ? ` (${r.waiting_payments} waiting for a block)` : "");
    $("o-relay-payto").textContent = r.pay_to || "no address: free until one is set";
    const list = $("o-relay-list");
    list.replaceChildren(
      ...r.passing.map((c) => h("li", {}, `Passing ${c.a} ⇄ ${c.b}: ${c.mb} MB`)),
      ...r.accounts.map((a) => h("li", {}, `${a.arcade}: sent ${a.used_mb} MB this month, charged ${a.month_charged}, paid ${a.paid}` + (a.owed > 0 ? `, owes ${a.owed}` : "") + " ARCADE")),
      ...r.through_relays.map((c) => h("li", {}, `This arcade reaches ${c.arcade} through ${c.relay}`)));
    if (document.activeElement !== $("o-pay-on")) $("o-pay-on").checked = !!r.pay_on;
    if (document.activeElement !== $("o-pay-cap")) $("o-pay-cap").value = r.pay_cap;
    $("o-pay-list").replaceChildren(
      h("li", {}, `Paid this month: ${r.paid_this_month} of ${r.pay_cap} ARCADE`),
      ...r.bills.map((b) => h("li", {}, `${b.relay}: ${b.price_per_gb} ARCADE a GB, ${b.used_mb} MB sent this month, balance ${b.balance.toFixed(4)} ARCADE`)));
  } catch (e) {
    $("o-relay-state").textContent = "—";
  }
}
$("o-relay-on").addEventListener("change", async (e) => {
  try { await api("mesh.relay_set", { on: e.target.checked }); say("o-relay-msg", e.target.checked ? "Relaying links for other arcades." : "Relay off."); }
  catch (err) { say("o-relay-msg", err.message, true); }
  refreshRelay();
});
$("o-relay-set").addEventListener("click", async () => {
  const limit_gb = Math.max(0, Number($("o-relay-gb").value) || 0);
  const max_links = Math.max(0, Math.floor(Number($("o-relay-max").value) || 0));
  try { await api("mesh.relay_set", { limit_gb, max_links }); say("o-relay-msg", `Limit set: ${limit_gb} GB a month, ${max_links} links at once.`); }
  catch (e) { say("o-relay-msg", e.message, true); }
  refreshRelay();
});

$("o-relay-fee-set").addEventListener("click", async () => {
  const price_per_gb = Math.max(0, Number($("o-relay-price").value) || 0);
  const free_mb = Math.max(0, Math.floor(Number($("o-relay-free").value) || 0));
  try { await api("mesh.relay_set", { price_per_gb, free_mb }); say("o-relay-msg", price_per_gb ? `Price set: ${price_per_gb} ARCADE a GB, ${free_mb} MB free a month.` : "Relaying is free."); }
  catch (e) { say("o-relay-msg", e.message, true); }
  refreshRelay();
});
$("o-relay-payto-set").addEventListener("click", async () => {
  const pay_to = $("o-relay-payto-in").value.trim();
  try { await api("mesh.relay_set", { pay_to }); say("o-relay-msg", pay_to ? `Paid to ${pay_to}.` : "Paid to this arcade's wallet."); }
  catch (e) { say("o-relay-msg", e.message, true); }
  refreshRelay();
});
$("o-pay-on").addEventListener("change", async (e) => {
  try { await api("mesh.relay_set", { pay_on: e.target.checked }); say("o-pay-msg", e.target.checked ? "Paying relays that charge, within the cap." : "Not paying relays."); }
  catch (err) { say("o-pay-msg", err.message, true); }
  refreshRelay();
});
$("o-pay-set").addEventListener("click", async () => {
  const pay_cap = Math.max(0, Number($("o-pay-cap").value) || 0);
  try { await api("mesh.relay_set", { pay_cap }); say("o-pay-msg", `Cap set: ${pay_cap} ARCADE a month.`); }
  catch (e) { say("o-pay-msg", e.message, true); }
  refreshRelay();
});

// Share system info (sysinfo.status / sysinfo.set): the switch, and the exact record it sends, field by field.
const SI_FIELDS = [["cpu", "CPU"], ["cores", "Cores"], ["ram_gb", "RAM"], ["gpus", "GPUs"], ["disk_gb", "Disk (this arcade's data)"],
  ["os", "System"], ["arch", "Architecture"], ["version", "Arcade version"], ["seal", "Seal speed"]];
function siValue(k, v, st) {
  if (k === "cores") return [v.physical != null ? `${v.physical} physical` : null, v.logical != null ? `${v.logical} logical` : null].filter(Boolean).join(", ");
  if (k === "ram_gb") return `${v} GB`;
  if (k === "gpus") return v.map((g) => g.unified ? `${g.name}: memory shared with the RAM` : g.vram_gb != null ? `${g.name}: ${g.vram_gb} GB` : g.name).join("; ");
  if (k === "disk_gb") return `${v.total} GB, ${v.free} GB free`;
  if (k === "seal") return `${v} core-seconds per GiB, measured`;
  return String(v);
}
async function refreshSysinfo() {
  let st;
  try { st = await api("sysinfo.status"); } catch (e) { say("o-si-msg", e.message, true); return; }
  if (document.activeElement !== $("o-si-on")) $("o-si-on").checked = !!st.on;
  const r = st.record || {};
  const rows = SI_FIELDS.map(([k, label]) => {
    let val = r[k] != null ? siValue(k, r[k], st) : null;
    if (k === "seal" && val == null) val = `not measured yet: ${(st.seal.sealed_bytes / 1048576).toFixed(0)} of ${(st.seal.needs_bytes / 1048576).toFixed(0)} MiB sealed here`;
    if (val == null) val = "can't be read here: left out";
    return h("tr", {}, h("td", {}, label), h("td", { translate: ["cpu", "version", "os", "arch"].includes(k) ? "no" : null }, val),
      h("td", { class: "muted" }, k === "seal" ? "measured" : "reported"));
  });
  $("o-si-rows").replaceChildren(...rows);
  const sig = JSON.stringify(st.record);
  if ($("o-si-json").dataset.sig !== sig) { $("o-si-json").dataset.sig = sig; $("o-si-json").textContent = JSON.stringify(st.record, null, 1); }
  if (!$("o-si-msg").textContent) say("o-si-msg", st.on ? (st.sent_at ? `Shared: last sent ${agoText(st.sent_at)}.` : "Shared.") : `Not shared. This is what would go out (${st.bytes} bytes, plus its signature).`);
}
$("o-si-on").addEventListener("change", async (e) => {
  try {
    await api("sysinfo.set", { on: e.target.checked });
    say("o-si-msg", e.target.checked ? "Sharing this arcade's system info with the mesh." : "Not shared any more: every arcade was told to drop it.");
  } catch (err) { say("o-si-msg", err.message, true); }
  refreshSysinfo();
});

async function refreshOptions() {
  refreshCache();
  refreshRelay();
  refreshSysinfo();
  const u = await api("update.status");
  $("o-ver").textContent = u.version || "local build";
  $("o-new").textContent = u.newest ? (u.newer ? `${u.newest} (newer)` : `${u.newest} (this one)`) : "—";
  $("o-last").textContent = agoText(u.last_check);
  const auto = $("o-auto");
  if (document.activeElement !== auto) auto.checked = !!u.on;
  auto.disabled = !u.supported || OPT.busy;
  $("o-check").disabled = !u.supported || OPT.busy || u.doing !== "idle";
  $("o-now").disabled = !u.supported || !u.newer || OPT.busy || u.doing !== "idle";
  if (OPT.busy) return;
  if (!u.supported) say("o-msg", `Updates can't be installed by this arcade: ${u.why_not}.`, true);
  else if (u.doing === "restarting") say("o-msg", `Updated to ${u.installed}: restarting…`);
  else if (u.doing !== "idle") say("o-msg", `${u.doing[0].toUpperCase()}${u.doing.slice(1)}…`);
  else if (u.error) say("o-msg", u.error, true);
  else $("o-msg").textContent = "";
}

async function optionsDo(what, f) {
  OPT.busy = true;
  say("o-msg", what);
  try { await f(); OPT.busy = false; await refreshOptions(); }
  catch (e) { OPT.busy = false; say("o-msg", e.message, true); }
}

$("o-auto").addEventListener("change", (e) => optionsDo(e.target.checked ? "Turning automatic updates on…" : "Turning automatic updates off…",
  () => api("update.set", { on: e.target.checked })));
// The code of the release an update would install (Arcade V1.0's releases are the read-only repository
// @arcade/arcade-v1, relrepo.rs), in Arcade Git: Strawberry's app, installed from the App Store first if it isn't here.
const CODE_REPO = "arcade/arcade-v1";
async function openCode() {
  const git = (l) => (l || []).filter((a) => a.card && a.card.family === "arcade-git" && a.creator_tag === "strawberry")
    .sort((x, y) => (y.height || 0) - (x.height || 0))[0];
  let a = git(await api("apps.installed"));
  if (!a) {
    a = git((await api("apps.store", { q: "arcade git" })).apps);
    if (!a) throw new Error("Arcade Git isn't in the App Store on this arcade yet: the code is also at /git/@arcade/arcade-v1 for git clone");
    if (!confirm("Arcade Git (by @strawberry) isn't installed here. Install it from the App Store to read the code?")) return;
    await api("apps.install", { chain: a.chain, txid: a.txid });
  }
  // The repository's main is the newest release this arcade holds: the one Update now would install.
  const l = await api("apps.launch", { chain: a.chain, txid: a.txid });
  await invoke("open_app", { id: l.id, title: l.title, frame: l.frame, at: `#/@${CODE_REPO}` });
}
$("o-code").addEventListener("click", () => openCode().catch((e) => say("o-msg", e.message || String(e), true)));
$("o-check").addEventListener("click", () => optionsDo("Looking for a new release on the network…", () => api("update.check")));
$("o-now").addEventListener("click", () => {
  if (!confirm("Install the newest release now? The service restarts; your wallet and data stay as they are.")) return;
  optionsDo("Installing the new release…", () => api("update.now"));
});

// Use this arcade from a browser (ui.link): the tunnel command and the link, in the app and in a
// browser alike (docs/WEBUI.md).
const WEBUI = { link: null, shown: false };
async function refreshWebUi() {
  try { WEBUI.link = await api("ui.link"); } catch (e) { say("o-web-msg", e.message, true); return; }
  const l = WEBUI.link;
  $("o-tunnel").textContent = l.tunnel;
  $("o-plain").textContent = `http://127.0.0.1:${l.port}/ui/`;
  $("o-link").textContent = WEBUI.shown ? l.url : "hidden";
  $("o-link-show").textContent = WEBUI.shown ? "Hide" : "Show";
}
async function copyText(text, what) {
  try { await navigator.clipboard.writeText(text); say("o-web-msg", `${what} copied.`); }
  catch { say("o-web-msg", `Couldn't copy here: select the ${what.toLowerCase()} and copy it by hand.`, true); WEBUI.shown = true; refreshWebUi(); }
}
$("o-tunnel-copy").addEventListener("click", () => WEBUI.link && copyText(WEBUI.link.tunnel, "Tunnel command"));
$("o-link-copy").addEventListener("click", () => WEBUI.link && copyText(WEBUI.link.url, "Link"));
$("o-link-show").addEventListener("click", () => { WEBUI.shown = !WEBUI.shown; refreshWebUi(); });

// Look: the default, or an installed theme app (theme.js).
(async () => {
  const sel = document.getElementById("o-look"); if (!sel) return;
  const msg = document.getElementById("o-look-msg");
  const fill = async () => {
    let list = [];
    try { list = await THEME.installedThemes(); } catch {}
    const cur = THEME.current();
    sel.replaceChildren(h("option", { value: "" }, "Default look"), ...list.map((a) => h("option", { value: a.id }, a.card.name)));
    sel.value = cur && list.some((a) => a.id === cur.id) ? cur.id : "";
    sel._list = list;
  };
  sel.onchange = async () => {
    msg.className = "msg"; msg.textContent = "";
    if (!sel.value) return THEME.clear();
    const a = sel._list.find((x) => x.id === sel.value);
    try { await THEME.use(a); } catch (e) { msg.className = "msg bad"; msg.textContent = `${a.card.name} can't be worn: ${e.message}`; await fill(); }
  };
  document.getElementById("o-look-store").onclick = () => {
    goTab("apps"); document.querySelector('#ap2-views button[data-v="store"]')?.click();
    let n = 0; const t = setInterval(() => { const c = document.querySelector('#ap2-cats [data-cat="themes"]'); if (c || ++n > 20) { clearInterval(t); c?.click(); } }, 150);
  };
  document.querySelector('nav button[data-tab="options"]')?.addEventListener("click", fill);
  fill();
})();
