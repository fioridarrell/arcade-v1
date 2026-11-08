// Chains & Coins: install, follow, pause and remove chains by hand (arcaded chains.* API).
(() => {
  const $ = (id) => document.getElementById(id);
  const invoke = window.__TAURI__.core.invoke;
  const api = async (cmd, args = {}) => {
    const r = await invoke("api", { req: { cmd, ...args } });
    if (!r.ok) throw new Error(r.error || "failed");
    return r.result;
  };
  const COLORS = { test: "#2e8b44", main: "#3f9c55", dgb: "#0066cc", dgbtest: "#3388dd", kot: "#7a4cc2", flop: "#d0602a", ltc: "#345d9d", ltctest: "#5b7fb8", btc: "#f7931a" };
  const MODES = [
    ["full", "Full", "Blocks, every address, and your chosen indexes. Serves the mesh.", true],
    ["chain", "Blockchain only", "Blocks and every address and balance. No protocol indexes.", true],
    ["index", "Index only", "Coming: chosen indexes, blocks borrowed from the mesh.", false],
    ["light", "Light", "Coming: headers only; everything else borrowed.", false],
  ];
  // Every index is opt-in. Names, maps and tokens ride on the ord index (ticking one ticks ord).
  const INDEXES = [
    ["ledger", "Arcade ledger", "arcade tokens, exchange, arcade NFTs, @tags", true],
    ["profiles", "Profiles", "published keys, names and linked wallets", true],
    ["ord", "Ord index", "inscriptions (Pepinals, Doginals, ...), content served from the chain", true, "ord"],
    ["names", "Names", ".pepe / .doge names and \"p\":\"dns\" claims (needs ord)", true, "ord"],
    ["maps", "Maps", "block maps: <block>.pepemap, .dogemap, .bitmap (needs ord)", true, "ord"],
    ["tokens", "Tokens", "PRC-20 / DRC-20 deploys, mints, transfers, balances (needs ord)", true, "ord"],
    ["runes", "Runes", "coming: Bitcoin's fungible tokens", false, "runes"],
    ["dunes", "Dunes", "coming: Dogecoin's runes-style tokens", false, "dunes"],
  ];
  const RIDERS = ["names", "maps", "tokens"];
  let chains = [], open = null, mode = "full", watching = null, poll = null;

  function msg(text, bad) { const m = $("c-msg"); m.textContent = text; m.className = "msg " + (bad ? "bad" : "ok"); }

  function card(c) {
    const el = document.createElement("div");
    el.className = "chain";
    el.innerHTML = `<div class="top"><div class="logo"></div><div><div class="nm"></div><div class="net"></div></div></div>
      <div class="bar" hidden><i></i></div><div class="state"></div>`;
    el.querySelector(".logo").style.background = COLORS[c.key] || "#777";
    el.querySelector(".logo").textContent = c.name[0];
    el.querySelector(".nm").textContent = c.name;
    el.querySelector(".net").textContent = c.key;
    const st = el.querySelector(".state");
    if (c.installed) {
      st.className = "state on";
      const pct = Math.floor(c.overall || 0);
      st.textContent = c.home ? `Home chain · ${c.mode} · ${(c.indexes || []).join(", ")}` : `${pct}% · ${c.state} · ${c.mode}`;
      if (!c.home && pct < 100) { const b = el.querySelector(".bar"); b.hidden = false; b.querySelector("i").style.width = pct + "%"; }
      const b = document.createElement("button"); b.textContent = "Manage…"; b.onclick = () => manage(c.key); el.append(b);
    } else {
      st.textContent = "Not installed";
      const b = document.createElement("button"); b.textContent = "Install…"; b.onclick = () => choose(c); el.append(b);
    }
    return el;
  }

  async function refresh() {
    try { chains = await api("chains.list"); } catch (e) { return; }
    $("c-list").replaceChildren(...chains.map(card));
    if (watching) {
      try { showProgress(await api("chains.status", { key: watching })); } catch (e) { msg(e.message, true); }
    }
  }

  function choose(c) {
    open = c; watching = null;
    $("c-installer").hidden = false; $("c-choose").hidden = false; $("c-progress").hidden = true; msg("");
    $("c-title").textContent = `Install ${c.name}`;
    $("c-modes").replaceChildren(...MODES.map(([k, t, d, ok]) => {
      const l = document.createElement("label");
      l.className = "mode" + (k === mode ? " sel" : "") + (ok ? "" : " off");
      l.innerHTML = `<input type="radio" name="mode" ${k === mode ? "checked" : ""} ${ok ? "" : "disabled"}><b></b><small></small>`;
      l.querySelector("b").textContent = t; l.querySelector("small").textContent = d;
      if (ok) l.onclick = () => { mode = k; choose(c); };
      return l;
    }));
    const noIdx = mode === "chain";
    // Runes only on Bitcoin, dunes only on Dogecoin: other chains don't list them.
    $("c-idx").replaceChildren(...INDEXES.filter(([, , , , needs]) => (needs !== "runes" || c.runes) && (needs !== "dunes" || c.dunes)).map(([k, t, d, ok, needs]) => {
      const can = ok && !noIdx && (needs !== "ord" || c.ord);
      const l = document.createElement("label");
      l.className = can ? "" : "off";
      l.innerHTML = `<input type="checkbox" data-k="${k}" ${can ? "" : "disabled"}> <b></b> <small></small>`;
      l.querySelector("b").textContent = k === "tokens" && c.tokens ? `${c.tokens} tokens` : t;
      l.querySelector("small").textContent = can || !ok ? d : "not available on this chain yet";
      const box = l.querySelector("input");
      box.onchange = () => {
        const all = $("c-idx");
        if (box.checked && RIDERS.includes(k)) all.querySelector('input[data-k="ord"]').checked = true;
        if (!box.checked && k === "ord") RIDERS.forEach((r) => { const x = all.querySelector(`input[data-k="${r}"]`); if (x) x.checked = false; });
      };
      return l;
    }));
    $("c-est").textContent = noIdx ? "Blocks and every address; no indexes." : "Nothing is ticked for you: choose the indexes you want. Each is built in the same commit as each block and can be removed later.";
  }

  async function install() {
    const withIdx = [...$("c-idx").querySelectorAll("input:checked")].map((i) => i.dataset.k);
    try {
      const st = await api("chains.add", { key: open.key, mode, with: withIdx });
      watching = open.key; showProgress(st); refresh();
    } catch (e) { msg(e.message, true); }
  }

  async function manage(key) {
    watching = key; $("c-installer").hidden = false; $("c-choose").hidden = true; msg("");
    try { showProgress(await api("chains.status", { key })); } catch (e) { msg(e.message, true); }
  }

  function showProgress(st) {
    $("c-progress").hidden = false;
    $("c-title").textContent = `${st.name} · ${st.mode} · ${st.state}`;
    $("c-stages").replaceChildren(...st.stages.map((s) => {
      const li = document.createElement("li");
      const p = s.pct || 0;
      li.className = p >= 100 ? "done" : p > 0 ? "run" : "wait";
      li.innerHTML = `<div class="sh"><span class="sn"></span><span class="sd"></span></div><div class="bar"><i></i></div>`;
      li.querySelector(".sn").textContent = s.name;
      li.querySelector(".sd").textContent = p >= 100 ? "done" : `${p.toFixed(1)}% · ${(s.at ?? 0).toLocaleString()} of ${(s.of ?? 0).toLocaleString()} · ${s.detail}`;
      li.querySelector(".bar > i").style.width = p + "%";
      // Remove-index buttons on index stages.
      const ix = s.index;
      if (ix && !(st.home && ix === "ledger")) {
        const b = document.createElement("button"); b.textContent = "Remove index"; b.className = "small";
        b.onclick = async () => { try { await api("chains.index_remove", { key: st.key, index: ix }); msg(`Removing the ${s.name} index…`); refresh(); } catch (e) { msg(e.message, true); } };
        li.querySelector(".sh").append(b);
      }
      return li;
    }));
    // Add an index the chain doesn't have yet (opt-in, one at a time).
    let add = document.getElementById("c-addix");
    if (!add) { add = document.createElement("div"); add.id = "c-addix"; add.className = "addix"; $("c-stages").after(add); }
    const info = chains.find((x) => x.key === st.key) || {};
    const have = st.indexes || [];
    const can = st.mode === "full" ? INDEXES.filter(([k, , , ok, needs]) => ok && !have.includes(k) && (needs !== "ord" || info.ord)) : [];
    add.replaceChildren(...(can.length ? [Object.assign(document.createElement("span"), { textContent: "Add an index: " })] : []),
      ...can.map(([k, t]) => {
        const b = document.createElement("button"); b.className = "small";
        b.textContent = k === "tokens" && info.tokens ? `${info.tokens} tokens` : t;
        b.onclick = async () => {
          try {
            const r = await api("chains.index_add", { key: st.key, index: k });
            msg(r.rebuilding ? `Adding ${t}: the ord index is rebuilt from its start with it.` : `Adding ${t}…`);
            refresh();
          } catch (e) { msg(e.message, true); }
        };
        return b;
      }));
    $("c-pause").hidden = !!st.home; $("c-stop").hidden = !!st.home;
    $("c-pause").textContent = st.state === "paused" ? "Resume" : "Pause";
  }

  $("c-close").onclick = $("c-cancel").onclick = () => { $("c-installer").hidden = true; watching = null; };
  $("c-go").onclick = install;
  $("c-pause").onclick = async () => {
    if (!watching) return;
    const paused = $("c-pause").textContent === "Resume";
    try { showProgress(await api(paused ? "chains.resume" : "chains.pause", { key: watching })); } catch (e) { msg(e.message, true); }
  };
  let armed = false;
  $("c-stop").onclick = async () => {
    if (!watching) return;
    if (!armed) { armed = true; $("c-stop").textContent = "Click again to remove the chain and its data"; setTimeout(() => { armed = false; $("c-stop").textContent = "Stop and remove"; }, 4000); return; }
    armed = false;
    try { await api("chains.remove", { key: watching }); msg("Removing the chain and its data…"); watching = null; $("c-progress").hidden = true; refresh(); } catch (e) { msg(e.message, true); }
  };
  document.querySelector('nav button[data-tab="chains"]').addEventListener("click", refresh);
  clearInterval(poll); poll = setInterval(() => { if (!document.getElementById("chains").hidden && document.getElementById("chains").classList.contains("on")) refresh(); }, 2500);
  refresh();
})();
