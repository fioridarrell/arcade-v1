#!/usr/bin/env python3
"""Content screening, borrowed over the mesh (screening.rs) on a closed local ARCADE chain.

S sells four models with the Models helper (arcade-models) from a fake model server: cheap-screen (the cheapest, but
it calls everything "ok"), a-screen and b-screen (right answers; b dearer) and a chat model (not a screener). B runs
no helper at all: arcaded asks S's models service itself.

- A new arcade borrows by default (not chosen yet), sees the three screeners cheapest first and not the chat model.
- No credit at S: screening says so and names S for a top-up; screening.topup pays S with the marketplace's memo.
- With credit: cheap-screen fails the built-in test (wrong answers), a-screen passes and is used; credit and costs show.
- a-screen drops (its server stops listing it): the next look switches to b-screen.
- Own model: passes with a right-answering model, fails with the "always ok" one; off; the choice survives a restart.
- A daily cap of 1 base unit stops borrowed checks.

Run inside: unshare -Urn sh -c 'ip link set lo up; python3 screennet.py ARCADED WORKDIR HELPER'
(HELPER: an arcade-models binary.)
"""
import hashlib, json, os, shutil, socket, subprocess, sys, threading, time
from http.server import BaseHTTPRequestHandler, ThreadingHTTPServer

EXE, WORK, HELPER = sys.argv[1], sys.argv[2], sys.argv[3]
PORTS = {"S": (18766, 18776), "B": (18767, 18777)}
DOOR = {"S": 18870}
FAKE = 18799
B58 = "123456789ABCDEFGHJKLMNPQRSTUVWXYZabcdefghijkmnopqrstuvwxyz"
procs, helpers, IDS = {}, {}, {}
LISTED = ["cheap-screen", "a-screen", "b-screen", "chatty"]
ASKED = []


def b58decode(s):
    n = 0
    for ch in s:
        n = n * 58 + B58.index(ch)
    return n.to_bytes(25, "big")[1:21]


def b58check(ver, payload):
    raw = bytes([ver]) + payload
    raw += hashlib.sha256(hashlib.sha256(raw).digest()).digest()[:4]
    n, out = int.from_bytes(raw, "big"), ""
    while n:
        n, r = divmod(n, 58)
        out = B58[r] + out
    return "1" * (len(raw) - len(raw.lstrip(b"\0"))) + out


def home(n):
    return os.path.join(WORK, n)


def call(n, cmd, **p):
    p["cmd"] = cmd
    s = socket.socket(socket.AF_UNIX)
    s.settimeout(300)
    s.connect(os.path.join(home(n), "arcaded.sock"))
    s.sendall((json.dumps(p) + "\n").encode())
    r = json.loads(s.makefile().readline())
    s.close()
    if not r.get("ok"):
        raise RuntimeError(f"{n} {cmd}: {r.get('error')}")
    return r["result"]


def start(n, peer=None, settings=None):
    http, mesh = PORTS[n]
    args = ["nice", "-n", "19", EXE, "--home", home(n), "--http-port", str(http), "--mesh-port", str(mesh), "--stun-port", "0", "--no-seeds"]
    if peer:
        args += ["--mesh-peer", f"127.0.0.1:{PORTS[peer][1]}"]
    if settings:
        args += ["--arc-local", settings]
    log = open(os.path.join(WORK, f"{n}.log"), "a")
    procs[n] = subprocess.Popen(args, stdout=log, stderr=subprocess.STDOUT, env=dict(os.environ, ARCADE_MESH_UDP="0"))
    for _ in range(600):
        try:
            call(n, "status")
            return
        except Exception:
            time.sleep(0.2)
    raise RuntimeError(f"{n} didn't start")


def stop(n):
    p = procs.pop(n)
    p.terminate()
    try:
        p.wait(60)
    except subprocess.TimeoutExpired:
        p.kill()
        p.wait()


def wait(what, cond, secs, show=None):
    t, last = time.time(), 0
    while time.time() - t < secs:
        try:
            if cond():
                return True
        except Exception as e:
            print("   ", what, ":", e)
        if show and time.time() - last > 30:
            last = time.time()
            print("   ", show())
        time.sleep(2)
    print(f"  timed out: {what}")
    return False


# ---------------------------------------------------------------- the fake model server

def verdict_for(model, content):
    if model == "cheap-screen":
        return "ok"
    if isinstance(content, list):
        return "ok"                                     # the test picture
    t = str(content).lower()
    if "stolen credit card" in t:
        return "illegal"
    if "porn" in t:
        return "sensitive"
    return "ok"


class Fake(BaseHTTPRequestHandler):
    def log_message(self, *a):
        pass

    def send(self, code, body, ctype="application/json"):
        b = body if isinstance(body, bytes) else json.dumps(body).encode()
        self.send_response(code); self.send_header("Content-Type", ctype); self.send_header("Content-Length", str(len(b))); self.end_headers(); self.wfile.write(b)

    def do_GET(self):
        if self.path.rstrip("/") == "/v1/models":
            return self.send(200, {"object": "list", "data": [{"id": m, "object": "model"} for m in LISTED]})
        self.send(404, {"error": {"message": "not found"}})

    def do_POST(self):
        q = json.loads(self.rfile.read(int(self.headers.get("Content-Length") or 0)) or b"{}")
        model = q.get("model")
        if model not in LISTED:
            return self.send(404, {"error": {"message": f"no model {model}"}})
        users = [m for m in q.get("messages", []) if m.get("role") == "user"]
        system = [m for m in q.get("messages", []) if m.get("role") == "system"]
        content = users[-1]["content"] if users else ""
        ASKED.append({"model": model, "system": bool(system), "picture": isinstance(content, list)})
        said = json.dumps({"verdict": verdict_for(model, content), "reason": "fake"})
        usage = {"prompt_tokens": 600, "completion_tokens": 12, "total_tokens": 612}
        if q.get("stream"):
            chunks = [{"object": "chat.completion.chunk", "model": model, "choices": [{"index": 0, "delta": {"role": "assistant", "content": said}, "finish_reason": None}]},
                      {"object": "chat.completion.chunk", "model": model, "choices": [{"index": 0, "delta": {}, "finish_reason": "stop"}]},
                      {"object": "chat.completion.chunk", "model": model, "choices": [], "usage": usage}]
            body = b"".join(b"data: " + json.dumps(c).encode() + b"\n\n" for c in chunks) + b"data: [DONE]\n\n"
            return self.send(200, body, "text/event-stream")
        self.send(200, {"object": "chat.completion", "model": model, "usage": usage,
                        "choices": [{"index": 0, "finish_reason": "stop", "message": {"role": "assistant", "content": said}}]})


def start_helper(n):
    log = open(os.path.join(WORK, f"H{n}.log"), "a")
    helpers[n] = subprocess.Popen(["nice", "-n", "19", HELPER, "--home", os.path.join(WORK, f"H{n}"), "--arcade-sock", os.path.join(home(n), "arcaded.sock"),
                                   "--port", str(DOOR[n])], stdout=log, stderr=subprocess.STDOUT)


def op(n, what, **b):
    b["what"] = what
    return call(n, "svc.ask", to=IDS[n], service="models", body=b, timeout=600)["last"]


def scr(n="B"):
    return call(n, "screening.status")


def main():
    threading.Thread(target=ThreadingHTTPServer(("127.0.0.1", FAKE), Fake).serve_forever, daemon=True).start()
    shutil.rmtree(WORK, ignore_errors=True)
    os.makedirs(WORK)
    homes = {}
    for n in "SB":
        start(n); call(n, "wallet.create"); IDS[n] = call(n, "status")["node_id"]; homes[n] = call(n, "start.status")["address"]; stop(n)
    arc = {n: b58check(0x53, b58decode(homes[n])) for n in "SB"}
    settings = {
        "net": "arctest",
        "genesis": {"time": int(time.time()) - 5, "closed_list": "01" * 32, "final_list": "02" * 32, "beacon_1": "03" * 32, "beacon_2": "04" * 32, "difficulty": "1099511627776"},
        "holders": [{"key": IDS[n], "bytes": 1 << 30, "pay": arc[n]} for n in "SB"],
        "steps_per_second": 1_000_000, "block_seconds": 2,
    }
    sp = os.path.join(WORK, "settings.json"); json.dump(settings, open(sp, "w"))
    start("S", settings=sp); start("B", peer="S", settings=sp)
    for n in "SB":
        call(n, "chains.add", key="arctest")
    S = IDS["S"]
    print("ids S", S[:8], "B", IDS["B"][:8])
    start_helper("S")
    res = {}
    res["helper attached at S"] = wait("helper attached", lambda: any(x["service"] == "models" for x in call("S", "svc.services")), 60)

    print("1. a new arcade borrows by default, not chosen yet")
    st = scr()
    print("   ", {k: st[k] for k in ("mode", "chosen", "working", "why", "unchecked", "day_cap")})
    res["default: borrow, not chosen, show_here, 1 ARCADE a day"] = st["mode"] == "borrow" and st["chosen"] is False and st["unchecked"] == "show_here" and st["day_cap"] == 100_000_000

    print("2. S sells three screeners and a chat model; B sees the screeners cheapest first")
    url = f"http://127.0.0.1:{FAKE}/v1"
    op("S", "set", on=True, servers=[{"url": url}], models=LISTED,
       prices={"cheap-screen": {"in": 0.05, "out": 0.05}, "a-screen": {"in": 0.1, "out": 0.1}, "b-screen": {"in": 0.5, "out": 0.5}, "chatty": {"in": 0.01, "out": 0.01}})
    res["B sees three screening offers"] = wait("offers at B", lambda: len(call("B", "screening.test")["borrow"]["offers"]) == 3, 180)
    st = scr()
    names = [o["model"] for o in st["borrow"]["offers"]]
    print("   ", names, "|", st["why"])
    res["cheapest first, chat model left out"] = names == ["cheap-screen", "a-screen", "b-screen"]

    print("3. no credit at S yet: says so, names S for a top-up")
    res["needs credit, top-up at S"] = wait("needs credit", lambda: "credit" in call("B", "screening.test")["why"], 60) and (scr()["borrow"]["top_up"] or {}).get("arcade") == S
    print("   ", scr()["why"], scr()["borrow"]["top_up"])

    print("4. B tops up 1 ARCADE at S (screening.topup, the marketplace's memo)")
    paid = None

    def pay():
        nonlocal paid
        paid = call("B", "screening.topup", amount=100_000_000)
        return True
    if not wait("B can pay S", pay, 1500, show=lambda: f"height {call('S', 'arc.status')['height']}"):
        return 1
    print("   ", paid)
    res["paid S with the memo, S told"] = paid["paid"]["txid"] and paid["told"] and paid["model"] in ("cheap-screen", "a-screen", "b-screen")
    res["S credits B"] = wait("credit at S", lambda: any(float(b.get("credit") or 0) >= 0.99 for b in [call("B", "svc.ask", to=S, service="models", body={"what": "balance"})["last"]]), 400)

    print("5. cheap-screen fails the test (wrong answers), a-screen passes and is used")
    res["a-screen in use"] = wait("a-screen chosen", lambda: (call("B", "screening.test")["borrow"]["current"] or {}).get("model") == "a-screen", 300)
    st = scr()
    cheap = next(o for o in st["borrow"]["offers"] if o["model"] == "cheap-screen")
    cur = st["borrow"]["current"] or {"test": {}}
    print("   ", "cheap:", cheap["test"], "| current:", {k: cur.get(k) for k in ("model", "per_text", "per_picture", "credit", "test")}, "| spent", st["spent_today"])
    res["cheap-screen failed on wrong answers"] = cheap["test"] and cheap["test"]["ok"] is False and cheap["test"]["wrong"] is True and "where" in cheap["test"]["error"]
    res["a-screen passed 5/5"] = cur["test"].get("ok") and cur["test"].get("passed") == 5
    res["working, costs and credit shown"] = st["working"] and cur.get("per_text", 0) > 0 and cur.get("per_picture", 0) > cur.get("per_text", 0) and cur.get("credit") is not None and st["spent_today"] > 0
    res["system prompt + a picture reached the server"] = any(a["system"] for a in ASKED) and any(a["picture"] for a in ASKED)

    print("6. a-screen drops (its server stops listing it): the next look switches to b-screen")
    LISTED.remove("a-screen")
    res["switched to b-screen"] = wait("b-screen chosen", lambda: (call("B", "screening.test")["borrow"]["current"] or {}).get("model") == "b-screen", 300)
    print("   ", (scr()["borrow"]["current"] or {}).get("model"), [(o["model"], o["state"]) for o in scr()["borrow"]["offers"]])
    res["still working"] = scr()["working"] is True

    print("7. own model: right answers pass, an always-ok model fails")
    call("B", "screening.set", mode="own", url=url, model="b-screen")
    st = call("B", "screening.test")
    print("   ", st["own"], st["working"])
    res["own model passes, used, key not shown"] = st["own"]["test"]["ok"] and st["working"] and "key" not in st["own"] and st["chosen"] is True
    call("B", "screening.set", mode="own", url=url, model="cheap-screen", key="sekrit")
    st = call("B", "screening.test")
    print("   ", st["why"])
    res["always-ok own model fails"] = st["working"] is False and "your model failed" in st["why"] and st["own"]["key_set"] is True
    e = None
    try:
        call("B", "screening.set", mode="own", url="ftp://x")
    except RuntimeError as x:
        e = str(x)
    res["bad url refused"] = e is not None and "url:" in e

    print("8. off, kept across a restart")
    call("B", "screening.set", mode="off")
    stop("B"); start("B", peer="S", settings=sp)
    st = scr()
    res["off and chosen after restart"] = st["mode"] == "off" and st["chosen"] is True and st["why"] == "screening is off"

    print("9. a daily cap of 1 base unit stops borrowed checks")
    call("B", "screening.set", mode="borrow", day_cap=1)
    res["cap reached"] = wait("cap", lambda: "budget" in call("B", "screening.test")["why"], 300)
    print("   ", scr()["why"], scr()["spent_today"])

    ok = sum(1 for v in res.values() if v)
    for k, v in res.items():
        print("PASS" if v else "FAIL", k)
    print(f"{ok}/{len(res)}", "ALL PASS" if ok == len(res) else "SOME FAILED")
    return 0 if ok == len(res) else 1


if __name__ == "__main__":
    try:
        code = main()
    finally:
        for n in list(procs):
            stop(n)
        for h in helpers.values():
            h.terminate()
    sys.exit(code)
