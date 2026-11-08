// MOCKUP of the Chains & Coins tab: simulated chains and install progress (no API calls).
(() => {
  const $ = (id) => document.getElementById(id);
  const CHAINS = [
    { key: "test", name: "Pepecoin", net: "testnet", color: "#2e8b44", blocks: "1.56 M blocks", size: "3.5 GB", installed: true, ord: "Pepinals" },
    { key: "main", name: "Pepecoin", net: "mainnet", color: "#3f9c55", blocks: "1.28 M blocks", size: "≈ 22 GB", ord: "Pepinals, PRC-20" },
    { key: "doge", name: "Dogecoin", net: "mainnet", color: "#c2a633", blocks: "5.9 M blocks", size: "≈ 230 GB", ord: "Doginals, DRC-20" },
    { key: "dgb", name: "DigiByte", net: "mainnet · DigiDollar", color: "#0066cc", blocks: "24.5 M blocks", size: "≈ 60 GB", ord: "DigiByte ordinals" },
    { key: "ltc", name: "Litecoin", net: "mainnet · MWEB", color: "#345d9d", blocks: "3.2 M blocks", size: "≈ 180 GB", ord: "LTC ord, ltc-20" },
    { key: "kot", name: "Kotia", net: "mainnet · proof of stake", color: "#7a4cc2", blocks: "1.54 M blocks", size: "≈ 2 GB", ord: null },
    { key: "flop", name: "Flopcoin", net: "mainnet", color: "#d0602a", blocks: "0.92 M blocks", size: "≈ 9 GB", ord: "Flopinals" },
  ];
  const MODES = [
    ["light", "Light", "Headers only; everything else borrowed from the mesh."],
    ["chain", "Blockchain only", "Blocks and every address and balance. No protocol indexes."],
    ["index", "Index only", "Your chosen indexes; blocks borrowed from the mesh as needed."],
    ["full", "Full", "Blocks, every address, and your chosen indexes. Serves the mesh."],
  ];
  let current = null, mode = "full", timer = null;

  function card(c) {
    const el = document.createElement("div");
    el.className = "chain";
    el.innerHTML = `
      <div class="top"><div class="logo" style="background:${c.color}">${c.name.slice(0, 1)}</div>
        <div><div class="nm"></div><div class="net"></div></div></div>
      <div class="meta"></div>
      <div class="bar" ${c.progress === undefined ? "hidden" : ""}><i style="width:${c.progress || 0}%"></i></div>
      <div class="state ${c.installed ? "on" : ""}"></div>`;
    el.querySelector(".nm").textContent = c.name;
    el.querySelector(".net").textContent = c.net;
    el.querySelector(".meta").textContent = `${c.blocks} · ${c.size}` + (c.ord ? ` · ord: ${c.ord}` : "");
    el.querySelector(".state").textContent = c.installed ? "Installed · Full · arcade ledger" : c.progress !== undefined ? `Installing… ${Math.floor(c.progress)}%` : "Not installed";
    if (!c.installed && c.progress === undefined) {
      const b = document.createElement("button");
      b.textContent = "Install…";
      b.onclick = () => open(c);
      el.append(b);
    } else if (c.installed) {
      const b = document.createElement("button");
      b.textContent = "Manage…";
      b.disabled = true;
      el.append(b);
    }
    return el;
  }
  function render() { $("c-list").replaceChildren(...CHAINS.map(card)); }

  function open(c) {
    current = c;
    $("c-title").textContent = `Install ${c.name} ${c.net.split(" ·")[0]}`;
    $("c-installer").hidden = false; $("c-choose").hidden = false; $("c-progress").hidden = true;
    $("c-modes").replaceChildren(...MODES.map(([k, t, d]) => {
      const l = document.createElement("label");
      l.className = "mode" + (k === mode ? " sel" : "");
      l.innerHTML = `<input type="radio" name="mode" ${k === mode ? "checked" : ""}><b></b><small></small>`;
      l.querySelector("b").textContent = t; l.querySelector("small").textContent = d;
      l.onclick = () => { mode = k; open(c); };
      return l;
    }));
    const noIdx = mode === "light" || mode === "chain";
    const idx = [
      ["ledger", "Arcade ledger", "tokens, exchange, arcade NFTs, @tags", true],
      ["ord", "Ord index", c.ord ? `inscriptions${c.ord ? " — " + c.ord : ""}` : "this chain has no inscriptions", !!c.ord],
      ["names", "Names", "\"p\":\"dns\" names", !!c.ord],
      ["profiles", "Profiles", "published keys, names and linked wallets", true],
    ];
    $("c-idx").replaceChildren(...idx.map(([k, t, d, ok]) => {
      const l = document.createElement("label");
      const can = ok && !noIdx;
      l.className = can ? "" : "off";
      l.innerHTML = `<input type="checkbox" data-k="${k}" ${can && (k === "ledger" || k === "ord") ? "checked" : ""} ${can ? "" : "disabled"}> <b></b><small></small>`;
      l.querySelector("b").textContent = t; l.querySelector("small").textContent = d;
      l.querySelector("input").onchange = est;
      return l;
    }));
    est();
  }
  function est() {
    const ord = $("c-idx").querySelector('[data-k="ord"]')?.checked;
    $("c-est").textContent = `Estimated: ${current.size} on disk${ord ? " + ord index" : ""}; ` +
      (mode === "light" ? "a few minutes." : $("c-snap").checked && ord ? "about 1–3 hours with mesh snapshots." : "several hours; the ord index is the slow part without a snapshot.");
  }

  function start() {
    const ord = $("c-idx").querySelector('[data-k="ord"]')?.checked && mode !== "light" && mode !== "chain";
    const stages = [
      ["Headers", "checked by proof of work", 6],
      ...(mode === "light" ? [] : [["Blocks", "from 3 arcades on the mesh + P2P", 1.2]]),
      ...(mode === "light" ? [] : [["Coins and addresses", "every address and balance", 2]]),
      ...(mode === "full" || mode === "index" ? [["Arcade ledger", "tokens, exchange, NFTs, @tags", 3]] : []),
      ...(ord ? [[`Ord index (${current.ord.split(",")[0]})`, $("c-snap").checked ? "verified snapshot, then live" : "from the ord start height", $("c-snap").checked ? 2.5 : 0.8]] : []),
      ["Fingerprints", "compared with arcades on the mesh", 8],
    ];
    $("c-choose").hidden = true; $("c-progress").hidden = false;
    $("c-stages").replaceChildren(...stages.map(([n, d]) => {
      const li = document.createElement("li");
      li.className = "wait";
      li.innerHTML = `<div class="sh"><span class="sn"></span><span class="sd"></span></div><div class="bar"><i></i></div>`;
      li.querySelector(".sn").textContent = n; li.querySelector(".sd").textContent = d;
      return li;
    }));
    let i = 0, pct = 0;
    current.progress = 0;
    clearInterval(timer);
    timer = setInterval(() => {
      const lis = $("c-stages").children;
      if (i >= lis.length) {
        clearInterval(timer); current.installed = true; delete current.progress; render();
        $("c-stages").insertAdjacentHTML("afterend", "");
        return;
      }
      const li = lis[i];
      li.className = "run";
      pct = Math.min(100, pct + stages[i][2]);
      li.querySelector(".bar > i").style.width = pct + "%";
      const left = Math.max(0, Math.round((100 - pct) / stages[i][2] * 0.25));
      li.querySelector(".sd").textContent = pct < 100 ? `${Math.floor(pct)}% · ${stages[i][1]} · about ${left}s left` : "done";
      if (pct >= 100) { li.className = "done"; i++; pct = 0; }
      current.progress = ((i + pct / 100) / lis.length) * 100;
      render();
    }, 250);
  }

  $("c-close").onclick = $("c-cancel").onclick = () => { $("c-installer").hidden = true; };
  $("c-go").onclick = start;
  $("c-snap").onchange = est;
  $("c-pause").onclick = () => { clearInterval(timer); };
  $("c-stop").onclick = () => { clearInterval(timer); delete current.progress; $("c-installer").hidden = true; render(); };
  render();
})();
