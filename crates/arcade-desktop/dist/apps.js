// Apps tab: the apps installed on this arcade, and the App Store of published app cards
// (the original arcade's game cards). Each app opens in its own window, sandboxed.
(() => {
  const PEP = homeCoins;
  const ago = (t) => { if (!t) return "waiting"; const s = Date.now() / 1000 - t; return s < 3600 ? `${Math.max(1, Math.round(s / 60))}m ago` : s < 172800 ? `${Math.round(s / 3600)}h ago` : `${Math.round(s / 86400)}d ago`; };
  const PERMS = [["wallet", "Wallet access"], ["contacts", "Contacts"], ["groups", "Group chats"], ["feed", "Feed (posts in your name)"], ["media", "Camera & microphone"], ["pictures", "Pictures in the app"], ["notify", "Notifications"]];
  let view = "mine", sort = "popular", query = "", cat = "", base = "http://127.0.0.1:8420", detail = null;
  // Category labels by key, from the last App Store answer (apps.store "categories").
  let CATS = {};
  const catLabel = (k) => CATS[k] || k;

  const note = (t, bad) => say("ap2-msg", t, bad);

  async function open(a) {
    try {
      const l = await api("apps.launch", { chain: a.chain, txid: a.txid });
      await invoke("open_app", { id: l.id, title: l.title, frame: l.frame });
      // The version that opened is one arcade's word (apps on a chain whose ledger isn't kept here): it opens, said.
      if (l.caution) note(`⚠ ${a.card.name}: ${l.caution}`);
      // Withdrawn by its publisher (2026-11-21): it still opens (an installed copy is never taken away); said, with the
      // replacement looked up in the store when the publisher named one.
      if (l.withdrawn) withdrawnNote(a.card.name, l.withdrawn);
    } catch (e) { note(e.message, true); }
  }

  function withdrawnNote(name, w) {
    note(`${name} was withdrawn by its publisher${w.reason ? ": " + w.reason : "."}${w.replaced_by ? " Replaced by " + w.replaced_by + " (shown in the store)." : ""}`);
    if (w.replaced_by) {
      const q = $("ap2-q");
      if (q) { q.value = String(w.replaced_by).replace(/^[a-z0-9]+:/, ""); q.dispatchEvent(new Event("input")); }
    }
  }

  // A theme app (category "themes", theme.js): checked, installed if it isn't, then worn (the window reloads).
  async function useTheme(a) {
    try { note(`Checking ${a.card.name}…`); await THEME.use(a); } catch (e) { note(`${a.card.name} can't be worn: ${e.message}`, true); }
  }

  function cover(a) {
    return a.card.cover
      ? h("img", { class: "ap-cover", src: `${base}/app/${a.chain}/${a.card.cover}`, alt: "" })
      : h("div", { class: "ap-cover ap-tile" }, a.card.name.slice(0, 2));
  }

  function byline(a) {
    const s = a.social || {};
    return [a.withdrawn ? "withdrawn by its publisher" : null, a.creator_tag ? "@" + a.creator_tag : a.creator.slice(0, 10) + "…", ago(a.time), a.card.version ? "v" + a.card.version : null,
      a.card.players ? a.card.players + " players" : null, s.tips ? `tipped ${s.tips} time${s.tips === 1 ? "" : "s"}` : null].filter(Boolean).join(" · ");
  }

  // Star ratings live in reviews: a comment that starts with ★★★★☆ (appsvc::stars_of).
  const STARS = (n) => "★".repeat(n) + "☆".repeat(5 - n);
  function ratingLine(a) {
    const r = (a.social || {}).rating || {};
    return r.count ? h("span", { class: "ap-stars", title: `${r.count} rating${r.count === 1 ? "" : "s"}` }, `★ ${r.average.toFixed(1)} (${r.count})`) : h("span", { class: "ap-stars s" }, "no ratings yet");
  }
  function rateBox(a) {
    const r = (a.social || {}).rating || {};
    let pick = r.mine || 0;
    const stars = [1, 2, 3, 4, 5].map((n) => {
      const b = h("button", { class: "ap-star-pick", title: `${n} star${n === 1 ? "" : "s"}` }, "☆");
      b.onclick = () => { pick = n; paint(); };
      return b;
    });
    const paint = () => stars.forEach((b, i) => { b.textContent = i < pick ? "★" : "☆"; b.classList.toggle("on", i < pick); });
    paint();
    const text = h("input", { placeholder: "Your review (optional)", maxlength: "200", style: "flex:1;min-width:12em" });
    const send = h("button", { class: "primary small" }, r.mine ? "Change my rating" : "Rate");
    // Changing edits the review that counts (a new review only when there is none), so no older
    // one is left behind to count again; removing deletes every starred review of mine.
    send.onclick = () => {
      if (!pick) return note("Pick 1 to 5 stars first.", true);
      const body = `${STARS(pick)}${text.value.trim() ? " " + text.value.trim() : ""}`;
      if (r.my_review) act("edit", { ...a, txid: r.my_review }, { text: body });
      else act("reply", a, { text: body });
    };
    const kids = [h("div", { class: "row" }, ...stars, text, send)];
    if (r.my_review) {
      const take = h("button", { class: "small" }, "Remove my rating");
      take.onclick = async () => {
        take.disabled = true;
        const mine = r.my_reviews && r.my_reviews.length ? r.my_reviews : [r.my_review];
        for (const txid of mine) if (!(await act("delete", { ...a, txid }))) break;
      };
      kids.push(h("div", { class: "row s" }, `Your rating: ${STARS(r.mine)}`, take));
    }
    const dist = (r.dist || [0, 0, 0, 0, 0]).map((x, i) => [5 - i, (r.dist || [])[4 - i] || 0]);
    const most = Math.max(1, ...dist.map((d) => d[1]));
    return h("div", { class: "card ap-rate" },
      h("div", { class: "k" }, r.count ? `Rated ${r.average.toFixed(1)} of 5 by ${r.count}` : "Not rated yet"),
      ...dist.map(([n, x]) => h("div", { class: "ap-dist" }, h("span", {}, `${n}★`), h("div", { class: "bar" }, h("i", { style: `width:${(x / most) * 100}%` })), h("span", { class: "s" }, String(x)))),
      ...kids,
      h("div", { class: "s" }, `A rating is a review on ${HOME.name}: a small fee, like a comment. Your latest one counts; change it any time, or remove it.`));
  }

  // The chain an app is inscribed on (Martin 2026-11-15: a badge on every app in the lists): its coin and name.
  function chainBadge(a) {
    const b = h("span", { class: "ap-chain", title: `Inscribed on ${a.chain_name || a.chain}` });
    b.append(coinLogo(a.chain, a.chain_name, "span", "ap-chain-logo"), document.createTextNode(a.chain_name || a.chain));
    return b;
  }

  // How far a card from the mesh is vouched for (appsvc trust): a line when it is one arcade's word.
  function trustLine(a, always) {
    const t = a.trust;
    if (!t || (!always && t.level === "green")) return "";
    return vouchLine(t.level, t.note);
  }

  function pills(a) {
    return h("div", { class: "ap-pills" },
      a.card.multiplayer ? h("span", { class: "pill" }, "multiplayer") : "",
      a.card.category ? h("span", { class: "pill" }, catLabel(a.card.category)) : "",
      a.card.genre ? h("span", { class: "pill" }, a.card.genre) : "");
  }

  async function act(kind, a, extra = {}) {
    try {
      if (a.chain !== HOME.key) throw new Error(`likes, comments and tips work on ${HOME.name} apps for now`);
      const r = await api("feed.act", { kind, target: a.txid, ...extra });
      note(`${kind} sent (fee ${PEP(r.fee)}); it counts once it's in a block.`);
      return true;
    } catch (e) { note(e.message, true); return false; }
  }

  function storeCard(a) {
    const s = a.social || {};
    const star = h("button", { class: "ap-star" + (a.starred ? " on" : ""), title: "Star" }, a.starred ? "★" : "☆");
    star.onclick = async () => { await api("apps.star", { chain: a.chain, txid: a.txid, on: !a.starred }); draw(); };
    const inst = h("button", { class: a.installed ? "" : "primary" }, a.installed ? "Installed" : "Install");
    inst.disabled = a.installed;
    inst.onclick = async () => { await api("apps.install", { chain: a.chain, txid: a.txid }); note(`${a.card.name} is on Your apps.`); draw(); };
    const theme = a.card.category === "themes";
    const openB = h("button", {}, theme ? "Preview" : "Open"); openB.onclick = () => open(a);
    const wear = theme ? h("button", { class: "primary" }, THEME.current()?.id === a.id ? "Worn" : "Use theme") : "";
    if (wear) wear.onclick = () => useTheme(a);
    const details = h("button", { class: "small" }, "Details"); details.onclick = () => { detail = a; draw(); };
    const comment = h("div", { class: "fd-reply", hidden: true }, (() => { const t = h("input", { placeholder: "Comment…" }); const b = h("button", { class: "small primary" }, "Send"); b.onclick = () => t.value.trim() && act("reply", a, { text: t.value.trim() }); return [t, b]; })());
    const tip = h("div", { class: "fd-reply", hidden: true }, (() => { const t = h("input", { type: "number", min: "0.01", step: "0.01", placeholder: homeTicker(), style: "width:8em" }); const b = h("button", { class: "small primary" }, "Tip"); b.onclick = () => { const v = Math.round(Number(t.value) * 1e8); if (v > 0) act("tip", a, { amount: v }); }; return [t, b]; })());
    const btn = (l, f, on) => { const b = h("button", { class: "fd-btn" + (on ? " on" : "") }, l); b.onclick = f; return b; };
    return h("div", { class: "ap-card" }, cover(a),
      h("div", { class: "ap-body" },
        h("div", { class: "ap-title" }, h("b", {}, a.card.name), star),
        h("div", { class: "ap-pills" }, chainBadge(a), pills(a)), h("div", { class: "s" }, byline(a), " · ", ratingLine(a)),
        trustLine(a),
        a.card.description ? h("div", { class: "ap-desc" }, a.card.description) : "",
        h("div", { class: "row ap-row" }, openB, wear || inst, wear ? inst : "", details),
        h("div", { class: "fd-acts" },
          btn(`▲ ${s.likes || 0}`, () => act(s.my_opinion === "like" ? "unlike" : "like", a), s.my_opinion === "like"),
          btn(`▼ ${s.dislikes || 0}`, () => act(s.my_opinion === "dislike" ? "undislike" : "dislike", a), s.my_opinion === "dislike"),
          btn(`Comment${s.comments ? " · " + s.comments : ""}`, () => { comment.hidden = !comment.hidden; }),
          a.mine ? "" : btn("Tip", () => { tip.hidden = !tip.hidden; })),
        comment, tip,
        ...(s.last_comments || []).map((c) => h("div", { class: "ap-comment s" }, (c.from || "").slice(0, 8) + "…: " + c.text))));
  }

  function mineCard(a) {
    if (a.missing) {
      const un = h("button", { class: "small" }, "Uninstall"); un.onclick = async () => { await api("apps.uninstall", { id: a.id }); draw(); };
      return h("div", { class: "ap-mine" }, h("div", { class: "ap-cover ap-tile" }, "?"), h("b", {}, a.card?.name || a.id), h("div", { class: "s" }, "Not found on the chains here now"), un);
    }
    const tile = h("div", { class: "ap-mine" + (a.removed ? " gone" : "") }, cover(a), h("b", {}, a.card.name), chainBadge(a), h("div", { class: "s" }, a.removed ? "removed by its publisher" : (a.card.version ? "v" + a.card.version : "")));
    // A theme's tile wears it (its page is only a preview of the look; Preview in the store opens that).
    tile.onclick = (e) => { if (e.target.tagName !== "BUTTON" && !a.removed) (a.card.category === "themes" ? useTheme(a) : open(a)); };
    const un = h("button", { class: "small" }, "Uninstall");
    un.onclick = async () => { if (!un.dataset.sure) { un.dataset.sure = 1; un.textContent = "Sure?"; return; } await api("apps.uninstall", { id: a.id }); draw(); };
    const det = h("button", { class: "small" }, "Details"); det.onclick = () => { detail = a; draw(); };
    const wear = a.card.category === "themes" ? h("button", { class: "small primary" }, THEME.current()?.id === a.id ? "Worn" : "Use theme") : "";
    if (wear) wear.onclick = () => useTheme(a);
    // An installed app now at a version only one arcade lists: said on its tile, before it is opened.
    tile.append(trustLine(a) || "", h("div", { class: "row ap-row" }, wear, det, un));
    return tile;
  }

  async function drawDetail() {
    const a = await api("apps.card", { chain: detail.chain, txid: detail.txid });
    const back = h("button", { class: "small" }, "← Back"); back.onclick = () => { detail = null; draw(); };
    const claimed = PERMS.filter(([k]) => a.card[k]);
    const sw = claimed.map(([k, label]) => {
      const box = h("input", { type: "checkbox", checked: !!a.permissions[k] });
      box.onchange = async () => { await api("apps.perms", { id: a.id, set: { [k]: box.checked } }); note(`${label} ${box.checked ? "on" : "off"} for ${a.card.name}.`); };
      return h("label", { class: "ap-perm" }, box, " ", label);
    });
    const openB = h("button", { class: "primary" }, "Open"); openB.onclick = () => open(a);
    // Spending allowances (2026-11-21): what the person lets this app spend a day per service without a card, and
    // what it spent today.
    const allowRows = [];
    for (const [svc, sats] of Object.entries((a.permissions && a.permissions.allowances) || {})) {
      let st = null;
      try { st = await api("apps.allowance", { id: a.id, service: svc }); } catch (_) { /* shown without today's */ }
      const per = h("input", { type: "number", min: "0", step: "any", value: String(Number(sats) / 1e8), style: "width:7em" });
      const save = h("button", { class: "small" }, "Save");
      save.onclick = async () => {
        await api("apps.perms", { id: a.id, set: { allowances: { [svc]: Number(per.value) || 0 } } });
        note(`${a.card.name}: ${Number(per.value) > 0 ? "up to " + per.value + " ARCADE a day" : "no allowance"} for ${svc}.`); draw();
      };
      const what = svc === "models" ? "AI models" : svc;
      allowRows.push(h("div", { class: "ap-perm" }, `${what}: up to `, per, " ARCADE a day ", save,
        st ? h("div", { class: "s" }, `Spent today ${st.spent_today} ARCADE, ${st.left} left.`) : ""));
    }
    // The folder the person gave this app (arcade.folder, 2026-11-21): its name, the last few changes the app made in
    // it, and Revoke (the folder and its files stay).
    let folderBox = "";
    const fo = a.permissions && a.permissions.folder;
    if (fo && fo.granted) {
      let changes = [];
      try { changes = (await api("apps.folder_log", { id: a.id, limit: 5 })).changes || []; } catch (_) { /* shown without them */ }
      const revoke = h("button", { class: "small" }, "Revoke");
      revoke.onclick = async () => {
        await api("apps.perms", { id: a.id, set: { folder: false } });
        note(`${a.card.name} can no longer use the folder “${fo.name}”.`); draw();
      };
      const when = (t) => new Date(t * 1000).toLocaleString();
      folderBox = h("div", { class: "card" }, h("div", { class: "k" }, "Folder"),
        h("div", { class: "ap-perm" }, `It can read and write in “${fo.name}”. `, revoke),
        changes.length ? h("ul", { class: "ap-list" }, ...changes.map((c) => h("li", { class: "s" }, `${when(c.at)} · ${c.op} ${c.path}${c.op === "write" ? ` (${c.bytes} bytes)` : ""}`)))
          : h("div", { class: "s" }, "No changes yet."));
    }
    $("ap2-body").replaceChildren(h("div", { class: "ap-detail" }, back,
      h("div", { class: "ap-card" }, cover(a), h("div", { class: "ap-body" }, h("h2", {}, a.card.name), h("div", { class: "ap-pills" }, chainBadge(a), pills(a)), h("div", { class: "s" }, byline(a)),
        trustLine(a, true),
        h("div", { class: "ap-desc" }, a.card.description || ""), h("div", { class: "row ap-row" }, openB))),
      claimed.length ? h("div", { class: "card" }, h("div", { class: "k" }, "This app asks for"), ...sw, h("div", { class: "s" }, "Off: it asks you the first time it wants it.")) : "",
      allowRows.length ? h("div", { class: "card" }, h("div", { class: "k" }, "Allowances"), ...allowRows, h("div", { class: "s" }, "Payments within these go without asking; 0 takes it away.")) : "",
      folderBox,
      h("h2", {}, "Ratings"),
      rateBox(a),
      h("h2", {}, "Versions"),
      h("ul", { class: "ap-list" }, ...a.versions.map((v, i) => h("li", {}, v.trust ? vouchDot(v.trust) : "", `${v.version ? "v" + v.version : "version"} · block ${v.height}${i === 0 ? " · shown" : ""}`))),
      h("h2", {}, `Reviews and comments (${a.comments.length})`),
      h("ul", { class: "ap-list" }, ...(a.comments.length ? a.comments.map((c) => h("li", {}, h("b", {}, (c.from || "").slice(0, 10) + "… "), c.stars ? h("span", { class: "ap-stars" }, STARS(c.stars) + " ") : "", c.stars ? c.text.replace(/^[★☆]{5}\s*/, "") : c.text)) : [h("li", { class: "s" }, "None yet.")])),
      h("div", { class: "s" }, `Card ${a.txid} · page ${a.opens} on ${a.chain_name} · by ${a.creator}`)));
  }

  let drawSeq = 0;
  async function draw() {
    const mySeq = ++drawSeq;
    const body = $("ap2-body");
    // A slower, older draw must not replace a newer one.
    const put = (...kids) => { if (mySeq === drawSeq) body.replaceChildren(...kids); };
    document.querySelectorAll("#ap2-views button").forEach((b) => b.classList.toggle("on", b.dataset.v === view));
    $("ap2-storebar").hidden = view !== "store";
    $("ap2-cats").hidden = view !== "store";
    try {
      base = arcadeUrl((await api("rpc.info")).url.replace(/\/rpc$/, ""));
      if (detail) return drawDetail();
      if (view === "mine") {
        const list = await api("apps.installed");
        put(list.length ? h("div", { class: "ap-grid" }, ...list.map(mineCard)) : h("div", { class: "mx-empty" }, "No apps installed yet. Find some in the App Store."));
      } else if (view === "store") {
        const r = await api("apps.store", { sort, q: query, category: cat });
        drawCats(r.categories || []);
        put(r.apps.length ? h("div", { class: "ap-store" }, ...r.apps.map(storeCard)) : h("div", { class: "mx-empty" }, cat ? `No ${catLabel(cat)} found yet.` : "No apps found."));
      } else if (view === "publish") {
        drawPublish(put);
      } else {
        const r = await api("apps.store", { flagged: true });
        put(h("p", { class: "note" }, "These cards look like an earlier app by someone else, so the App Store hides them until you decide. Trusted publishers are never held back."),
          ...(r.apps.length ? r.apps.map((a) => {
            const ok = h("button", { class: "small primary" }, "Show it"); ok.onclick = async () => { await api("apps.review", { chain: a.chain, txid: a.txid, verdict: "clear" }); draw(); };
            const no = h("button", { class: "small" }, "Keep hidden"); no.onclick = async () => { await api("apps.review", { chain: a.chain, txid: a.txid, verdict: "confirm" }); draw(); };
            const tr = h("button", { class: "small" }, "Trust this publisher"); tr.onclick = async () => { await api("apps.trust", { address: a.creator, on: true }); draw(); };
            return h("div", { class: "ap-card" }, cover(a), h("div", { class: "ap-body" }, h("b", {}, a.card.name), chainBadge(a), h("div", { class: "s" }, byline(a)), h("div", { class: "s bad" }, a.flag), h("div", { class: "row ap-row" }, ok, no, tr)));
          }) : [h("div", { class: "mx-empty" }, "Nothing to review.")]));
      }
    } catch (e) { note(e.message, true); }
  }

  // Category chips: All, the known ones (even when empty), then any other a card uses.
  function drawCats(list) {
    CATS = Object.fromEntries(list.map((c) => [c.key, c.label]));
    const total = list.reduce((n, c) => n + c.count, 0);
    const chip = (key, label, n, about) => {
      const b = h("button", { class: key === cat ? "on" : "", title: about || "", "data-cat": key }, `${label} ${n}`);
      b.onclick = () => { cat = key; draw(); };
      return b;
    };
    $("ap2-cats").replaceChildren(chip("", "All", total), ...list.filter((c) => c.known || c.count).map((c) => chip(c.key, c.label, c.count, c.about)));
  }

  function drawPublish(put) {
    const f = (ph, attrs = {}) => h("input", { placeholder: ph, ...attrs });
    const name = f("Name (required, up to 60)"), version = f("Version, e.g. 1.0"), players = f("Players, e.g. 1-4"), genre = f("Genre, e.g. Puzzle"), family = f("Family (rooms shared across versions; letters, digits . _ : -)");
    const desc = h("textarea", { rows: 3, placeholder: "Description (up to 280)" });
    const FLAGS = [["multiplayer", "Multiplayer"], ["wallet", "Uses the wallet"], ["contacts", "Reads contacts"], ["groups", "Group chats"], ["feed", "Posts on the feed"], ["media", "Camera & microphone"], ["pictures", "Draws pictures"], ["notify", "Rings while closed"]];
    const boxes = FLAGS.map(([k, l]) => h("label", { class: "ap-perm" }, h("input", { type: "checkbox", value: k }), " ", l));
    const page = h("input", { type: "file", accept: ".html,.htm,text/html" }), cov = h("input", { type: "file", accept: "image/*" });
    const chain = h("select", { title: "Which chain this app is published on" });
    const est = h("div", { class: "s" });
    const resume = h("button", {}, "Resume");
    resume.hidden = true;
    // Category: a known one, or a new one typed in (it shows in the App Store once used).
    const catSel = h("select", {}, h("option", { value: "" }, "Category: Apps"));
    const catNew = f("or a new category", { maxlength: "24", style: "max-width:14em" });
    api("apps.categories").then((list) => { catSel.replaceChildren(...list.map((c) => h("option", { value: c.key }, `Category: ${c.label}`)), h("option", { value: "" }, "Category: a new one →")); catSel.value = "apps"; }).catch(() => {});
    const read = (input) => new Promise((ok) => { const file = input.files[0]; if (!file) return ok(null); const r = new FileReader(); r.onload = () => ok({ name: file.name, type: file.type, data: String(r.result).split(",")[1], size: file.size }); r.readAsDataURL(file); });
    const cardOf = () => {
      const c = { name: name.value.trim() };
      for (const [k, el] of [["version", version], ["players", players], ["genre", genre], ["family", family], ["description", desc]]) if (el.value.trim()) c[k] = el.value.trim();
      for (const b of boxes) { const i = b.querySelector("input"); if (i.checked) c[i.value] = true; }
      const cg = (catNew.value.trim() || catSel.value).toLowerCase().split(/\s+/).join("-");
      if (cg) c.category = cg;
      return c;
    };
    // A quote computed for the previous chain must not overwrite the one for the chain just chosen.
    let estSeq = 0;
    const money = (sats, unit) => coinNum(sats) + " " + (unit || (chain.selectedOptions[0] && chain.selectedOptions[0].dataset.unit) || homeTicker());
    const chainArg = () => chain.value || undefined;
    const showWait = (r) => {
      const sent = Number(r.sent) || 0, total = Number(r.total || r.transactions) || 0;
      resume.hidden = false;
      resume.dataset.id = r.id || "";
      est.textContent = `${sent} of ${total} sent, waiting for confirmations. The rest continues automatically.`;
      note(r.note || est.textContent, false);
    };
    const estimate = async () => {
      const seq = ++estSeq;
      const picked = chainArg();
      const pg = await read(page), cv = await read(cov);
      if (seq !== estSeq) return;
      if (!pg) { if (resume.hidden) est.textContent = ""; return; }
      const card = JSON.stringify({ app: cardOf() });
      const a = await api("inscribe.estimate", { data: pg.data, encoding: "base64", contenttype: "text/html", json: card, chain: picked });
      const b = cv ? await api("inscribe.estimate", { data: cv.data, encoding: "base64", contenttype: cv.type || "image/png", chain: picked }) : { fee: 0, locked: 0, transactions: 0, unit: a.unit };
      if (seq !== estSeq) return;
      const n = a.transactions + b.transactions;
      const unit = a.unit || b.unit;
      est.textContent = `${n} transaction${n === 1 ? "" : "s"} · network fee about ${money(a.fee + b.fee, unit)} · ${money(a.locked + b.locked, unit)} kept in data outputs (yours, spendable later)`;
    };
    page.onchange = estimate; cov.onchange = estimate;
    chain.onchange = () => { estimate(); };
    chainPicker(chain).then(() => estimate());
    const go = h("button", { class: "primary" }, "Publish");
    // While this view is open, nudge an unfinished inscription and keep the line up to date. The arcade also
    // continues it with the window closed, and after a restart.
    const watch = (id, after) => {
      clearInterval(window.__inscWatch);
      const tick = async () => {
        try {
          const s = await api("inscribe.resume", { id: id || undefined });
          const job = s.jobs ? (s.jobs.find((j) => j.state !== "sent") || s.jobs[0]) : s;
          if (!job) return;
          if (job.state === "sent") {
            clearInterval(window.__inscWatch);
            resume.hidden = true;
            if (after) after(job); else note(job.note || "Inscribed.");
          } else showWait(job);
        } catch (e) { clearInterval(window.__inscWatch); note(e.message, true); go.disabled = false; }
      };
      window.__inscWatch = setInterval(tick, 15000);
    };
    resume.onclick = async () => {
      resume.disabled = true;
      try {
        const s = await api("inscribe.resume", { id: resume.dataset.id || undefined });
        const job = s.jobs ? (s.jobs.find((j) => j.state !== "sent") || s.jobs[0]) : s;
        if (!job || job.state === "sent") { resume.hidden = true; note((job && job.note) || "Inscribed."); }
        else { showWait(job); watch(job.id); }
      } catch (e) { note(e.message, true); }
      resume.disabled = false;
    };
    const published = (name, txid) => note(`Published ${name}: card ${String(txid || "").slice(0, 12)}… It shows in the App Store once its transactions are in blocks.`);
    go.onclick = async () => {
      const c = cardOf();
      if (!c.name) return note("Give the app a name.", true);
      const pg = await read(page);
      if (!pg) return note("Pick the app's HTML file.", true);
      go.disabled = true;
      clearInterval(window.__inscWatch);
      try {
        const cv = await read(cov);
        if (cv) {
          const cover = await api("inscribe", { data: cv.data, encoding: "base64", contenttype: cv.type || "image/png", chain: chainArg() });
          if (cover.state === "waiting") {
            showWait(cover);
            watch(cover.id, async (done) => {
              try {
                c.cover = (done && done.txid) || cover.txid;
                const pageNow = await read(page);
                if (!pageNow) return;
                const r = await api("inscribe", { data: pageNow.data, encoding: "base64", contenttype: "text/html", json: JSON.stringify({ app: c }), chain: chainArg() });
                if (r.state === "waiting") { showWait(r); watch(r.id, (fin) => published(c.name, (fin && fin.txid) || r.txid)); }
                else published(c.name, r.txid);
              } catch (e) { note(e.message, true); }
              go.disabled = false;
            });
            return;
          }
          c.cover = cover.txid;
        }
        const r = await api("inscribe", { data: pg.data, encoding: "base64", contenttype: "text/html", json: JSON.stringify({ app: c }), chain: chainArg() });
        if (r.state === "waiting") { showWait(r); watch(r.id, (fin) => published(c.name, (fin && fin.txid) || r.txid)); }
        else published(c.name, r.txid);
      } catch (e) { note(e.message, true); }
      go.disabled = false;
    };
    api("inscribe.status").then((s) => {
      const j = (s.jobs || []).find((x) => x.state !== "sent");
      if (j) { showWait(j); watch(j.id); }
    }).catch(() => {});
    put(h("div", { class: "card ap-pub" },
      h("p", { class: "s" }, "An app is an HTML page inscribed with its card. Publishing again under the same name replaces your earlier version; the old one still plays at its own link."),
      name, h("div", { class: "row" }, version, players, genre), h("div", { class: "row" }, catSel, catNew), family, desc, h("div", { class: "ap-flags" }, ...boxes),
      h("label", {}, "App page (HTML) ", page), h("label", {}, "Cover image ", cov), h("label", {}, "Chain ", chain), est, h("div", { class: "row" }, go, resume)));
  }

  document.querySelectorAll("#ap2-views button").forEach((b) => b.onclick = () => { view = b.dataset.v; detail = null; draw(); });
  document.querySelectorAll("#ap2-sort button").forEach((b) => b.onclick = () => { sort = b.dataset.s; document.querySelectorAll("#ap2-sort button").forEach((x) => x.classList.toggle("on", x === b)); draw(); });
  $("ap2-q").addEventListener("input", (e) => { query = e.target.value; clearTimeout(window.__apq); window.__apq = setTimeout(draw, 300); });
  document.querySelector('nav button[data-tab="apps"]').addEventListener("click", () => { detail = null; draw(); });
})();
