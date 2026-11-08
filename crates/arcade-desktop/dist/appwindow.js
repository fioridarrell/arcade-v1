// The app window: run one app sandboxed in a frame; answer what it asks through the arcade
// (permission switches decide; the bridge for the app scripts plugs in here).
(() => {
  const q = new URLSearchParams(location.search);
  const frame = q.get("frame") || "";
  const id = q.get("id") || "";
  document.title = q.get("title") || "App";
  const el = document.getElementById("app");
  if (!frame.startsWith("http://127.0.0.1:")) {
    const e = document.getElementById("err");
    e.textContent = "This app can't be opened.";
    e.style.display = "flex";
    return;
  }
  el.src = frame;
  window.arcadeApp = { id, frame, el };
})();
