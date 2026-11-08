// The app window: run one app sandboxed in a frame and answer what it asks (the original arcade's
// viewer "doors"): storage through this arcade, its window state, rooms (offline until the mesh
// carries them). Only messages from the app's own frame are answered; the page's identity is the
// page this window opened, never what a message says.
(() => {
  const q = new URLSearchParams(location.search);
  const frame = q.get("frame") || "";
  const id = q.get("id") || "";
  document.title = q.get("title") || "App";
  const el = document.getElementById("app");
  if (!/^http:\/\/127\.0\.0\.1:\d+\/app\/[a-z]+\/[0-9a-f]{64}$/.test(frame)) {
    const e = document.getElementById("err");
    e.textContent = "This app can't be opened.";
    e.style.display = "flex";
    return;
  }
  const here = frame.split("/").pop();
  const invoke = window.__TAURI__.core.invoke;
  async function api(cmd, args = {}) {
    const r = await invoke("api", { req: { cmd, ...args } });
    if (!r.ok) throw new Error(r.error || "failed");
    return r.result;
  }
  let appState = { state: "open", timeout: null, closesAt: null, inShell: false };

  const perms = async () => (id ? await api("apps.perms", { id }) : {});
  let lastSend = 0;
  const doors = {
    async groups(m) {
      const p = await perms();
      if (!p.groups) throw new Error("off: turn on Group chats on this app's card");
      switch (m.op) {
        case "list": return await api("group.app_list");
        case "read": return await api("group.app_read", { id: m.group, after: m.after || 0, limit: m.limit || 200 });
        case "send": {
          const text = String(m.text || "");
          if (!text.trim()) throw new Error("nothing to send");
          if (text.length > 2000) throw new Error("too long: at most 2000 characters");
          if (Date.now() - lastSend < 400) throw new Error("busy: wait a moment");
          lastSend = Date.now();
          const r = await api("group.say", { id: m.group, text });
          return { txid: r.id, fee: r.fee };
        }
        case "leave": { await api("group.leave", { id: m.group }); return { left: m.group }; }
        case "seal": return await api("group.seal", { id: m.group, text: String(m.text || "") });
        case "open": return await api("group.open", { id: m.group, sealed: String(m.sealed || "") });
        case "people": return { people: (await api("contacts.list")).map((c) => ({ address: c.address, tag: c.tag })) };
        case "add": return await api("group.add", { id: m.group, members: (m.members || []).slice(0, 50).map((x) => typeof x === "string" ? x : (x.tag ? "@" + x.tag : x.address)) });
        case "create": {
          const name = String(m.name || "").slice(0, 60);
          const members = (m.members || []).slice(0, 50).map((x) => typeof x === "string" ? x : (x.tag ? "@" + x.tag : x.address));
          if (!name) throw new Error("a group needs a name");
          const r = await api("group.create", { name, members });
          return { group: (await api("group.app_list")).groups.find((g) => g.id === r.id) || { id: r.id, name } };
        }
        case "save": throw new Error("saving files from an app is coming to Arcade V1.0");
        default: throw new Error("groups ops are list, read, send, leave, seal, open, people, add, create and save");
      }
    },
    async storage(m) {
      const r = await api("apps.storage", { page: here, op: m.op, key: m.key, value: m.value });
      return r;
    },
    async app(m) {
      if (m.op !== "state") throw new Error("app: op is state");
      return { ...appState };
    },
    async realtime(m) {
      if (m.op === "join") return { online: false, why: "rooms are coming to Arcade V1.0: playing alone for now", me: null, members: [] };
      if (m.op === "leave") return {};
      if (m.op === "ringed") return { ring: null };
      if (m.op === "ring") return { rung: false, why: "ringing is coming to Arcade V1.0" };
      throw new Error("not in a room: rooms are coming to Arcade V1.0");
    },
  };

  window.addEventListener("message", async (e) => {
    if (e.source !== el.contentWindow) return;
    const m = e.data;
    if (!m || typeof m !== "object" || typeof m.seq !== "number" || !m.arcade) return;
    if (m.arcade === "realtime" && m.op === "fast") return; // never answered
    const door = doors[m.arcade];
    let body;
    if (!door) body = { ok: false, error: `${m.arcade} is coming to Arcade V1.0` };
    else {
      try { body = { ok: true, ...(await door(m)) }; }
      catch (err) { body = { ok: false, error: String(err && err.message || err) }; }
    }
    body.arcade = m.arcade; body.seq = m.seq;
    el.contentWindow.postMessage(body, "*");
  });

  // Window state for arcade.app: minimized / restored / closing.
  const tell = (state) => { appState = { ...appState, state }; el.contentWindow?.postMessage({ arcade: "app", event: true, ...appState }, "*"); };
  document.addEventListener("visibilitychange", () => tell(document.hidden ? "minimized" : "restored"));
  window.addEventListener("beforeunload", () => tell("closing"));

  el.src = frame;
  window.arcadeApp = { id, frame, el };
})();
