// Explorer tab + the chain selector in the header. Everything is read from this arcade's own
// chain.mdbx through arcaded's explorer.* API: no outside service.
(() => {
  const $ = (id) => document.getElementById(id);
  const invoke = window.__TAURI__.core.invoke;
  const api = async (cmd, args = {}) => {
    const r = await invoke("api", { req: { cmd, ...args } });
    if (!r.ok) throw new Error(r.error || "failed");
    return r.result;
  };
  const COLORS = { peptest: "#2e8b44", pep: "#3f9c55", doge: "#c2a633", dgb: "#0066cc", dgbtest: "#3388dd", kot: "#7a4cc2", flop: "#d0602a", ltc: "#345d9d", ltctest: "#5b7fb8", btc: "#f7931a" };
  // What people call each coin (the keys underneath stay as the mesh knows them).
  const TICKERS = { peptest: "PEPtest", pep: "PEP", doge: "DOGE", dgb: "DGB", dgbtest: "DGBtest", kot: "KOT", flop: "FLOP", ltc: "LTC", ltctest: "LTCtest", btc: "BTC", wjk: "WJK" };
  const COIN_ICONS = ["peptest", "pep", "doge", "dgb", "dgbtest", "kot", "flop", "ltc", "ltctest", "btc", "wjk"];
  const ICON_FILE = (k) => `coins/${k}.${k === "btc" ? "svg" : "png"}`;
  // A chain's coin: the original arcade's icon, or a letter on the chain's colour.
  function coinEl(c) {
    const e = document.createElement("span"); e.className = "coin";
    if (COIN_ICONS.includes(c.key)) { const i = document.createElement("img"); i.src = ICON_FILE(c.key); i.alt = ""; e.append(i); e.classList.add("img"); }
    else { e.style.background = COLORS[c.key] || "#777"; e.textContent = c.name[0]; }
    return e;
  }
  const SECTIONS = [
    ["home", "Overview"], ["blocks", "Blocks"], ["mempool", "Mempool"], ["inscriptions", "Inscriptions", "ord"],
    ["tokens", "Tokens", "tokens"], ["names", "Names", "names"], ["maps", "Maps", "maps"], ["ledger", "Arcade ledger", "ledger"],
    // Runes and dunes join here (with their index keys) when those indexes exist.
  ];
  // Pages and the index each needs; a page comes online only when its index is on the chain.
  const NEEDS = { inscriptions: "ord", inscription: "ord", tokens: "tokens", token: "tokens", names: "names", name: "names", maps: "maps", map: "maps", ledger: "ledger" };
  const DETAIL_PARENT = { block: "blocks", tx: "blocks", address: "home", inscription: "inscriptions", token: "tokens", name: "names", map: "maps" };

  let chains = [], chain = null, sum = null, page = "home", arg = null, seq = 0;

  // ---------- helpers ----------
  const h = (tag, attrs = {}, ...kids) => {
    const e = document.createElement(tag);
    for (const [k, v] of Object.entries(attrs || {})) {
      if (v === undefined || v === null || v === false) continue;
      if (k === "class") e.className = v; else if (k.startsWith("on")) e[k] = v; else e.setAttribute(k, v === true ? "" : v);
    }
    for (const k of kids.flat()) if (k !== null && k !== undefined && k !== false) e.append(k.nodeType ? k : document.createTextNode(String(k)));
    return e;
  };
  const fmt = (n) => (n ?? 0).toLocaleString("en-US");
  const short = (s, a = 10, b = 8) => (s && s.length > a + b + 1 ? s.slice(0, a) + "…" + s.slice(-b) : s || "");
  const coins = (sats) => (sats / 1e8).toLocaleString("en-US", { maximumFractionDigits: 8 }) + " " + (sum?.unit || "");
  const ago = (t) => {
    if (!t) return "";
    const s = Math.max(0, Math.floor(Date.now() / 1000 - t));
    if (s < 90) return `${s}s ago`; if (s < 5400) return `${Math.floor(s / 60)}m ago`; if (s < 172800) return `${Math.floor(s / 3600)}h ago`;
    return new Date(t * 1000).toLocaleDateString();
  };
  const units = (v, dec) => { // base-unit decimal string -> display
    try {
      const b = BigInt(v || "0"), d = 10n ** BigInt(dec ?? 18), w = b / d, f = (b % d).toString().padStart(Number(dec ?? 18), "0").replace(/0+$/, "");
      return w.toLocaleString("en-US") + (f ? "." + f.slice(0, 8) : "");
    } catch { return String(v); }
  };
  const has = (ix) => !!sum && sum.indexes.includes(ix);
  const link = (text, p, a, extra) => h("span", { class: "link" + (extra ? " " + extra : ""), onclick: (ev) => { ev.stopPropagation(); go(p, a); } }, text);
  const addrLink = (a, tag) => a ? h("span", {}, link(short(a, 10, 6), "address", a, "mono"), tag ? h("span", { class: "chip arc" }, "@" + tag) : "") : h("span", { class: "faint" }, "no address");

  // ---------- chain selector ----------
  async function loadChains() {
    try { chains = await api("chains.list"); } catch { return; }
    const inst = chains.filter((c) => c.installed);
    const first = !chain;
    // ?chain=<key> picks the starting chain (links, shortcuts).
    const want = first ? new URLSearchParams(location.search).get("chain") : null;
    if (want && inst.find((c) => c.key === want.toLowerCase())) chain = inst.find((c) => c.key === want.toLowerCase());
    else if (!chain || !inst.find((c) => c.key === chain.key)) chain = inst.find((c) => c.home) || inst[0];
    else chain = inst.find((c) => c.key === chain.key);
    paintSelector();
    if (first) announce();
  }
  function sub(c) {
    if (c.home) return "home";
    if (c.mode === "chain") return "blockchain only";
    return `${c.state || ""}${c.indexes?.length ? " · " + c.indexes.join(", ") : ""}`;
  }
  function paintSelector() {
    if (!chain) return;
    $("cs-coin").replaceWith(Object.assign(coinEl(chain), { id: "cs-coin" }));
    $("cs-name").textContent = chain.name; $("cs-sub").textContent = "· " + sub(chain);
  }
  function menu() {
    const m = $("cs-menu");
    const row = (c, on) => h("button", { class: c.key === chain?.key ? "on" : "", disabled: !on, onclick: on ? () => pick(c) : null },
      coinEl(c), h("span", {}, c.name), h("span", { class: "meta" }, on ? sub(c) : "—"));
    const inst = chains.filter((c) => c.installed), rest = chains.filter((c) => !c.installed);
    m.replaceChildren(h("div", { class: "hd" }, "Installed on this arcade"), ...inst.map((c) => row(c, true)),
      rest.length ? h("div", { class: "hd" }, "Not installed") : "", ...rest.map((c) => row(c, false)),
      h("div", { class: "foot" }, h("span", { class: "link", onclick: () => { close(); document.querySelector('nav button[data-tab="chains"]').click(); } }, "Install a chain in Chains & Coins →")));
  }
  function pick(c) { chain = c; close(); paintSelector(); sum = null; go("home"); announce(); }
  // Every tab follows the picked chain.
  function announce() { window.dispatchEvent(new CustomEvent("arcade:chain", { detail: chain })); }
  function close() { $("cs-menu").hidden = true; $("cs-btn").setAttribute("aria-expanded", "false"); }
  $("cs-btn").onclick = async (ev) => {
    ev.stopPropagation();
    const m = $("cs-menu");
    if (m.hidden) { await loadChains(); menu(); }
    m.hidden = !m.hidden; $("cs-btn").setAttribute("aria-expanded", String(!m.hidden));
  };
  document.addEventListener("click", (ev) => { if (!ev.target.closest(".chainsel")) close(); });
  document.addEventListener("keydown", (ev) => { if (ev.key === "Escape") close(); });

  // ---------- pieces ----------
  function preview(i, big) {
    const t = h("div", { class: big ? "x-big" : "x-thumb" });
    const c = i.content || {};
    if (c.kind === "image") t.append(h("img", { src: c.data, alt: `inscription ${i.n}`, loading: "lazy" }));
    else if (c.kind === "text") t.append(h("pre", {}, c.text.slice(0, big ? 20000 : 400)));
    else t.append(h("div", { class: "x-ph" }, c.kind === "html" ? "HTML (not run here)" : c.kind === "missing" ? "content not stored" : (c.type || "binary")));
    if (!big) t.append(h("span", { class: "tag" }, (i.ct || "").split(";")[0] || "no type"));
    return t;
  }
  function blockRows(list) {
    return h("div", { class: "x-list" }, list.map((b) => h("div", { class: "x-blk", onclick: () => go("block", String(b.height)) },
      h("div", { class: "x-hbox" }, h("b", {}, fmt(b.height)), h("small", {}, ago(b.time))),
      h("div", { class: "x-mid" }, h("div", { class: "x-hash" }, b.hash),
        h("div", { class: "chips" }, h("span", { class: "chip" }, `${b.txs} tx`), b.inscriptions ? h("span", { class: "chip ins" }, `${b.inscriptions} inscription${b.inscriptions > 1 ? "s" : ""}`) : "")))));
  }
  function insGrid(list) {
    if (!list.length) return h("div", { class: "x-empty" }, "No inscriptions yet.");
    return h("div", { class: "x-grid" }, list.map((i) => h("div", { class: "x-ins", onclick: () => go("inscription", i.id) }, preview(i),
      h("div", { class: "cap" }, h("b", {}, `#${fmt(i.n)}`), h("span", { class: "faint" }, fmt(i.h))))));
  }
  function table(head, rows) {
    return h("div", { class: "x-tbl" }, h("table", {}, h("thead", {}, h("tr", {}, head.map(([t, num]) => h("th", { class: num ? "num" : "" }, t)))),
      h("tbody", {}, rows.map((r) => h("tr", { class: r.go ? "click" : "", onclick: r.go || null }, r.cells.map(([v, num]) => h("td", { class: num ? "num" : "" }, v)))))));
  }
  const sect = (title, more, body) => h("section", { class: "x-sect" }, h("header", {}, h("h2", {}, title), more ? link("View all →", more) : ""), body);
  const kv = (rows) => h("div", { class: "x-kv" }, rows.filter(Boolean).flatMap(([k, v]) => [h("div", { class: "k" }, k), h("div", {}, v)]));
  const crumbs = (...parts) => h("div", { class: "x-crumbs" }, parts.flatMap((p, i) => (i ? [" / ", p] : [p])));
  const head = (title, ...pills) => h("div", { class: "x-dhead" }, h("h1", {}, title), ...pills);
  const pill = (t, ok) => h("span", { class: "pill" + (ok ? " ok" : "") }, t);

  // ---------- views ----------
  async function vHome() {
    sum = await api("explorer.summary", { chain: chain.key });
    const tok = sum.protocol ? sum.protocol.toUpperCase() : "Token";
    const st = [
      ["Height", fmt(sum.tip), sum.tip_time ? `last block ${ago(sum.tip_time)}` : ""],
      ["Mempool", `${fmt(sum.mempool.count)} tx`, `${fmt(sum.mempool.bytes)} bytes`],
      has("ord") && ["Inscriptions", fmt(sum.inscriptions), "ord index"],
      has("tokens") && [`${tok} tokens`, fmt(sum.ticks), "deployed ticks"],
      has("names") && ["Names", fmt(sum.names?.[0] ?? 0), sum.names ? `json ${fmt(sum.names[1])} · doge2023 ${fmt(sum.names[2])}` : ""],
      has("maps") && ["Maps", fmt(sum.maps), "first claim wins"],
    ].filter(Boolean);
    const lower = [
      has("tokens") && sect(`${tok} tokens`, "tokens", tokTable(sum.tokens || [])),
      has("names") && sect("Names", "names", (sum.name_rows || []).length ? table([["Name"], ["Inscription", 1]], sum.name_rows.map((r) => ({ go: () => go("name", r.name), cells: [[h("span", { class: "mono" }, r.name)], [`#${fmt(r.number)}`, 1]] }))) : h("div", { class: "x-empty" }, "No names claimed yet.")),
      has("maps") && sect("Maps", "maps", (sum.map_rows || []).length ? table([["Claim"], ["Inscription", 1]], sum.map_rows.map((r) => ({ go: () => go("map", r.claim), cells: [[h("span", { class: "mono" }, r.claim)], [`#${fmt(r.number)}`, 1]] }))) : h("div", { class: "x-empty" }, "No block maps claimed yet.")),
    ].filter(Boolean);
    return h("div", {},
      h("div", { class: "x-stats" }, st.map(([k, v, s]) => h("div", { class: "x-stat" }, h("div", { class: "k" }, k), h("div", { class: "v" }, v), h("div", { class: "s" }, s)))),
      h("div", { class: has("ord") ? "x-cols" : "x-cols one" },
        sect("Latest blocks", "blocks", blockRows(sum.blocks)),
        has("ord") ? sect("Latest inscriptions", "inscriptions", insGrid(sum.latest || [])) : ""),
      lower.length ? h("div", { class: `x-cols3 n${lower.length}` }, lower) : "");
  }
  function tokTable(list) {
    if (!list.length) return h("div", { class: "x-empty" }, "No tokens deployed yet.");
    return table([["Tick"], ["Minted"], ["Supply", 1]], list.map((t) => {
      const pct = Number(BigInt(t.minted || "0") * 1000n / (BigInt(t.supply || "1") || 1n)) / 10;
      return { go: () => go("token", t.tick), cells: [[h("b", {}, t.shown || t.tick)], [h("div", { class: "x-meter", title: `${pct}%` }, h("i", { style: `width:${Math.max(1, pct)}%` }))], [units(t.supply, t.dec), 1]] };
    }));
  }
  async function vBlocks() {
    const before = arg ? Number(arg) : undefined;
    const list = await api("explorer.blocks", { chain: chain.key, before, n: 25 });
    const last = list[list.length - 1];
    return sect("Blocks", null, h("div", {}, blockRows(list), last && last.height > 0 ? h("div", { class: "row" }, h("button", { onclick: () => go("blocks", String(last.height)) }, "Older blocks")) : ""));
  }
  async function vMempool() {
    const m = await api("explorer.mempool", { chain: chain.key });
    const body = m.txs.length ? table([["Txid"], ["Out", 1], ["Size", 1], ["Kind"]], m.txs.map((t) => ({ go: () => go("tx", t.txid),
      cells: [[h("span", { class: "mono" }, short(t.txid, 16, 8))], [coins(t.out), 1], [`${fmt(t.size)} B`, 1], [h("span", { class: "chip" + (t.kind === "inscription" ? " ins" : "") }, t.kind + (t.rbf ? " · RBF" : ""))]] })))
      : h("div", { class: "x-empty" }, "No unconfirmed transactions right now.");
    return h("div", {}, sect("Mempool", null, body),
      m.pending_tags.length ? h("p", { class: "x-note" }, `Tag claims waiting for a block: ${m.pending_tags.map((t) => "@" + (t.tag || t)).join(", ")}`) : "",
      h("p", { class: "x-note" }, "Read from this arcade's mempool. Spending with zero confirmations is fine on chains without RBF."));
  }
  async function vInscriptions() {
    const r = await api("explorer.inscriptions", { chain: chain.key, before: arg ? Number(arg) : undefined, n: 36 });
    const last = r.items[r.items.length - 1];
    return sect(`Inscriptions · ${fmt(r.total)}`, null, h("div", {}, insGrid(r.items), last && last.n > 0 ? h("div", { class: "row" }, h("button", { onclick: () => go("inscriptions", String(last.n)) }, "Older inscriptions")) : ""));
  }
  async function vTokens() {
    const r = await api("explorer.tokens", { chain: chain.key, after: arg || undefined, n: 50 });
    const last = r.items[r.items.length - 1];
    return sect(`${r.protocol.toUpperCase()} tokens`, null, h("div", {}, tokTable(r.items), r.items.length === 50 && last ? h("div", { class: "row" }, h("button", { onclick: () => go("tokens", last.tick) }, "More")) : ""));
  }
  async function vNames() {
    const r = await api("explorer.names", { chain: chain.key, after: arg || undefined, n: 50 });
    const last = r.items[r.items.length - 1];
    return sect(`Names · ${fmt(r.counts[0])}`, null, h("div", {}, r.items.length ? table([["Name"], ["Winning inscription", 1]], r.items.map((x) => ({ go: () => go("name", x.name), cells: [[h("span", { class: "mono" }, x.name)], [`#${fmt(x.number)}`, 1]] })))
      : h("div", { class: "x-empty" }, "No names claimed yet."), r.items.length === 50 && last ? h("div", { class: "row" }, h("button", { onclick: () => go("names", last.name) }, "More")) : ""));
  }
  async function vMaps() {
    const r = await api("explorer.maps", { chain: chain.key, n: 50 });
    return sect("Maps", null, h("div", {}, r.items.length ? table([["Claim"], ["Winning inscription", 1]], r.items.map((x) => ({ go: () => go("map", x.claim), cells: [[h("span", { class: "mono" }, x.claim)], [`#${fmt(x.number)}`, 1]] })))
      : h("div", { class: "x-empty" }, "No block maps claimed yet."),
      h("p", { class: "x-note" }, "A map claim is an inscription whose whole text is <block>.<name>map (pepemap, dogemap, bitmap, …). The block can't be above the one it was inscribed in, and the first claim wins.")));
  }
  async function vLedger() {
    if (!chain.home) return sect("Arcade ledger", null, h("div", { class: "x-empty" }, "The arcade ledger view is on the home chain for now."));
    const l = await api("ledger");
    const same = (l.peers || []).filter((p) => p.same || p.match).length;
    return h("div", {}, sect("Arcade ledger", null, kv([["Indexed to", fmt(l.indexed_to)], ["Checkpoint", fmt(l.checkpoint)],
      ["Consensus hash", h("span", { class: "mono" }, l.consensus_hash)], ["Names digest", h("span", { class: "mono" }, l.names_digest)],
      ["Other arcades", `${(l.peers || []).length} reporting${same ? ` · ${same} agree` : ""}`]])));
  }
  async function vBlock() {
    const b = await api("explorer.block", { chain: chain.key, id: arg });
    return h("div", {}, crumbs(link("Blocks", "blocks"), fmt(b.height)),
      head(`Block ${fmt(b.height)}`, pill(b.best ? `${fmt(b.confirmations)} confirmations` : "not on the best chain", b.best)),
      kv([["Hash", h("span", { class: "mono" }, b.hash)], ["Time", `${new Date(b.time * 1000).toLocaleString()} · ${ago(b.time)}`],
        ["Transactions", fmt(b.tx_count)], ["Size", `${fmt(b.size)} bytes`], b.inscriptions != null && ["New inscription pieces", fmt(b.inscriptions)],
        ["Previous", link(short(b.prev, 16, 8), "block", b.prev, "mono")], b.next && ["Next", link(short(b.next, 16, 8), "block", b.next, "mono")],
        ["Bits · nonce · version", h("span", { class: "mono" }, `${b.bits} · ${b.nonce} · ${b.version}`)], ["Merkle root", h("span", { class: "mono" }, b.merkle_root)]]),
      h("h2", { class: "x-h2" }, "Transactions"),
      table([["Txid"], [""], ["In · out"], ["Output", 1]], b.txs.map((t) => ({ go: () => go("tx", t.txid), cells: [[h("span", { class: "mono" }, short(t.txid, 18, 8))],
        [t.coinbase ? h("span", { class: "chip" }, "coinbase") : t.inscription ? h("span", { class: "chip ins" }, "inscription") : ""], [`${t.inputs} · ${t.outputs}`], [coins(t.out), 1]] }))),
      b.tx_count > b.txs.length ? h("p", { class: "x-note" }, `Showing the first ${b.txs.length} of ${fmt(b.tx_count)} transactions.`) : "");
  }
  async function vTx() {
    const t = await api("explorer.tx", { chain: chain.key, txid: arg });
    const io = (rows) => h("div", { class: "x-io" }, rows);
    return h("div", {}, crumbs(link("Blocks", "blocks"), t.height != null ? link(fmt(t.height), "block", String(t.height)) : "mempool", "transaction"),
      head("Transaction", pill(t.height != null ? `${fmt(t.confirmations)} confirmation${t.confirmations === 1 ? "" : "s"}` : "unconfirmed", t.height != null), pill(t.rbf ? "signals RBF" : "no RBF", !t.rbf),
        t.genesis ? h("span", { class: "chip ins" }, `inscribes ${t.genesis.split(";")[0]}`) : ""),
      kv([["Txid", h("span", { class: "mono" }, t.txid)], ["Size", `${fmt(t.size)} bytes`], t.fee != null && ["Fee", coins(t.fee)], ["Total out", coins(t.out)]]),
      h("div", { class: "x-flow" },
        io(t.inputs.map((i) => i.coinbase ? h("div", {}, h("span", { class: "chip" }, "new coins (coinbase)")) :
          h("div", {}, h("span", { class: "a" }, addrLink(i.address, i.tag)), h("span", {}, link(i.value != null ? coins(i.value) : "?", "tx", i.txid))))),
        h("div", { class: "arrow" }, "→"),
        io(t.outputs.map((o) => h("div", {}, h("span", { class: "a" }, o.op_return ? h("span", { class: "chip" }, "OP_RETURN data") : addrLink(o.address, o.tag)),
          h("span", { class: "r" }, o.inscriptions.map((id) => link("◆", "inscription", id, "ins-on")), coins(o.value), " ",
            o.spent_by ? link("spent", "tx", o.spent_by.txid, "faint") : h("span", { class: "faint" }, o.op_return ? "" : "unspent")))))));
  }
  async function vAddress() {
    const a = await api("explorer.address", { chain: chain.key, address: arg });
    return h("div", {}, crumbs("Address"),
      head(short(a.address, 14, 8), a.tag ? h("span", { class: "chip arc" }, "@" + a.tag) : ""),
      kv([["Address", h("span", { class: "mono" }, a.address)], ["Balance", coins(a.balance)], ["Transactions", fmt(a.tx_count)],
        a.inscription_count != null && ["Inscriptions held", fmt(a.inscription_count)]]),
      a.tokens && a.tokens.length ? h("div", {}, h("h2", { class: "x-h2" }, "Tokens"), table([["Tick"], ["Balance", 1], ["Transferable", 1]],
        a.tokens.map((t) => ({ go: () => go("token", t.tick), cells: [[h("b", {}, t.tick)], [units(t.overall, t.dec), 1], [units(t.transferable, t.dec), 1]] })))) : "",
      a.inscriptions && a.inscriptions.length ? h("div", {}, h("h2", { class: "x-h2" }, "Inscriptions"), insGrid(a.inscriptions)) : "",
      h("h2", { class: "x-h2" }, "History"),
      a.history.length ? table([["Block"], ["Txid"], ["Received", 1], ["Sent", 1]], a.history.map((r) => ({ go: r.txid ? () => go("tx", r.txid) : null,
        cells: [[fmt(r.height)], [h("span", { class: "mono" }, short(r.txid || "", 16, 8))], [r.received ? coins(r.received) : "", 1], [r.sent ? coins(r.sent) : "", 1]] })))
        : h("div", { class: "x-empty" }, "No transactions."));
  }
  async function vInscription() {
    const i = await api("explorer.inscription", { chain: chain.key, id: arg });
    const ct = (i.ct || "").split(";")[0];
    return h("div", {}, crumbs(link("Inscriptions", "inscriptions"), `#${fmt(i.n)}`),
      head(`Inscription #${fmt(i.n)}`, h("span", { class: "chip ins" }, ct || "no type"), i.burned != null ? pill("burned", false) : ""),
      h("div", { class: "x-split" }, preview(i, true),
        kv([["Id", h("span", { class: "mono" }, i.id)], ["Holder", i.owner ? addrLink(i.owner, i.owner_tag) : h("span", { class: "faint" }, i.burned != null ? `burned at ${fmt(i.burned)}` : "—")],
          i.at && ["Location", link(short(i.at, 12, 10), "tx", i.at.split(":")[0], "mono")], ["Inscribed", link(`block ${fmt(i.h)}`, "block", String(i.h))],
          ["Content type", i.ct || "—"], ["Size", `${fmt(i.size)} bytes · ${i.pieces} piece${i.pieces === 1 ? "" : "s"}`], i.proto && ["Protocol", i.proto],
          i.delegate && ["Delegate", link(short(i.delegate, 12, 6), "inscription", i.delegate, "mono")], i.parent && ["Parent", link(short(i.parent, 12, 6), "inscription", i.parent, "mono")],
          i.re && ["Reinscription", "yes"], ["Reveal", link(short(i.txs?.[i.txs.length - 1] || "", 12, 8), "tx", i.txs?.[i.txs.length - 1], "mono")]])));
  }
  async function vToken() {
    const t = await api("explorer.token", { chain: chain.key, tick: arg });
    const pct = Number(BigInt(t.minted || "0") * 1000n / (BigInt(t.supply || "1") || 1n)) / 10;
    return h("div", {}, crumbs(link("Tokens", "tokens"), t.shown || t.tick),
      head(t.shown || t.tick, pct >= 100 ? pill("minted out") : pill(`${pct}% minted`, true)),
      kv([["Supply", units(t.supply, t.dec)], ["Minted", units(t.minted, t.dec)], ["Mint limit", units(t.lim, t.dec)], ["Decimals", String(t.dec)],
        ["Deploy", link(`inscription #${fmt(t.n)}`, "inscription", t.id)], ["Deployer", addrLink(t.deployer)], ["Holders", t.holders]]),
      h("h2", { class: "x-h2" }, "Top holders"),
      t.top.length ? table([["Holder"], ["Balance", 1], ["Transferable", 1]], t.top.map((x) => ({ go: () => go("address", x.address), cells: [[addrLink(x.address, x.tag)], [units(x.overall, t.dec), 1], [units(x.transferable, t.dec), 1]] })))
        : h("div", { class: "x-empty" }, "Nobody holds any yet."));
  }
  async function vName() {
    const n = await api("explorer.name", { chain: chain.key, name: arg });
    return h("div", {}, crumbs(link("Names", "names"), n.name), head(n.name),
      table([["Reading"], ["Winning inscription"], ["Holder"]], n.readings.map((r) => ({ cells: [[r.rule], [link(`#${fmt(r.number)}`, "inscription", r.id)], [r.owner ? addrLink(r.owner, r.owner_tag) : "—"]] }))),
      h("p", { class: "x-note" }, "Each reading has its own winner: the lowest inscription number with that name. The holder is whoever holds the claim inscription now."));
  }
  async function vMap() {
    const m = await api("explorer.map", { chain: chain.key, claim: arg });
    return h("div", {}, crumbs(link("Maps", "maps"), m.claim), head(m.claim),
      kv([["Winning inscription", link(`#${fmt(m.number)}`, "inscription", m.id)], ["Holder", m.owner ? addrLink(m.owner, m.owner_tag) : "—"], ["Block", link(m.claim.split(".")[0], "block", m.claim.split(".")[0])]]));
  }
  const VIEWS = { home: vHome, blocks: vBlocks, mempool: vMempool, inscriptions: vInscriptions, tokens: vTokens, names: vNames, maps: vMaps, ledger: vLedger,
    block: vBlock, tx: vTx, address: vAddress, inscription: vInscription, token: vToken, name: vName, map: vMap };

  // ---------- navigation ----------
  function paintNav() {
    const cur = DETAIL_PARENT[page] || page;
    $("x-nav").replaceChildren(...SECTIONS.filter(([, , ix]) => !ix || has(ix)).map(([k, t]) => h("button", { class: cur === k ? "on" : "", onclick: () => go(k) }, t)));
    const hint = ["block height or hash", "txid", "address", "@tag"];
    if (has("ord")) hint.push("inscription #");
    if (has("names")) hint.push("name");
    if (has("maps")) hint.push("map");
    if (has("tokens")) hint.push("tick");
    $("x-q").placeholder = hint.join(", ") + "…";
  }
  async function render() {
    if (!chain) await loadChains();
    if (!chain) { $("x-view").replaceChildren(h("div", { class: "x-empty" }, "Waiting for arcaded…")); return; }
    const my = ++seq;
    paintNav();
    try {
      if (!sum || sum.chain !== chain.key) sum = await api("explorer.summary", { chain: chain.key });
      // A page whose index is off on this chain isn't available: back to the overview.
      if (NEEDS[page] && !has(NEEDS[page])) { page = "home"; arg = null; }
      const v = await VIEWS[page]();
      if (my !== seq) return;
      $("x-view").replaceChildren(v);
      paintNav();
    } catch (e) {
      if (my !== seq) return;
      $("x-view").replaceChildren(h("div", { class: "x-empty bad" }, e.message), h("div", { class: "row" }, h("button", { onclick: () => go("home") }, "Back to the overview")));
    }
  }
  function go(p, a = null) { page = p; arg = a; render(); window.scrollTo({ top: 0 }); }

  $("x-q").addEventListener("keydown", async (ev) => {
    if (ev.key !== "Enter") return;
    const q = ev.target.value.trim();
    if (!q || !chain) return;
    try {
      const r = await api("explorer.search", { chain: chain.key, q });
      if (r.chain && r.chain !== chain.key) { const c = chains.find((x) => x.key === r.chain); if (c) { chain = c; paintSelector(); sum = null; } }
      if (NEEDS[r.kind] && !has(NEEDS[r.kind])) throw new Error(`Turn on the ${NEEDS[r.kind]} index for ${chain.name} in Chains & Coins to look up ${q}.`);
      go(r.kind, r.id);
    } catch (e) { $("x-view").replaceChildren(h("div", { class: "x-empty" }, e.message)); }
  });

  document.querySelector('nav button[data-tab="explorer"]').addEventListener("click", () => { sum = null; render(); });
  // Keep the overview fresh while it is open.
  setInterval(() => { if ($("explorer").classList.contains("on") && page === "home" && !document.hidden) { sum = null; render(); } }, 30000);
  setInterval(loadChains, 15000);
  loadChains();
  // #explorer/<page>/<arg> opens that page.
  const hp = (location.hash || "").slice(1).split("/");
  if (hp[0] === "explorer") { if (hp[1]) { page = hp[1]; arg = hp.slice(2).join("/") || null; } render(); }
  window.arcadeExplorer = { go, chain: () => chain };
  window.arcadeChain = () => chain;
})();
