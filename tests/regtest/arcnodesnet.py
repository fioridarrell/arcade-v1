#!/usr/bin/env python3
"""explorer.arc_nodes (arcnodes.rs) on a closed network. A writes a closed ARCADE chain; B links to A, keeps Pepecoin testnet in full, shares
system info; C links to B only (A hears C through B's passed-on hold report), keeps Pepecoin testnet too.
Checks A's rows for itself, B and C (linked / heard, beta false, B's machine line), the chain rows and the page reads
/r/explorer/arc_nodes and /r/explorer/arc_machines. (Sizes stay unknown here: a closed net has no blocks to measure; the
hold report's sz part is unit-tested in shares.rs.)
Run inside: unshare -Urn sh -c "ip link set lo up; python3 arcnodesnet.py ARCADED WORKDIR" """
import hashlib, json, os, shutil, socket, subprocess, sys, time, urllib.request

EXE, WORK = sys.argv[1:3]
PORTS = {"A": (18781, 18791), "B": (18782, 18792), "C": (18783, 18793)}
procs = {}
B58 = "123456789ABCDEFGHJKLMNPQRSTUVWXYZabcdefghijkmnopqrstuvwxyz"
fails = []


def check(name, ok):
    print(("  ok   " if ok else "  FAIL ") + name, flush=True)
    if not ok:
        fails.append(name)


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
    s.settimeout(120)
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
    procs[n] = subprocess.Popen(args, stdout=open(os.path.join(WORK, f"{n}.log"), "a"), stderr=subprocess.STDOUT)
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


def wait(what, cond, secs):
    t = time.time()
    while time.time() - t < secs:
        try:
            if cond():
                return True
        except Exception as e:
            print("   ", what, ":", e, flush=True)
        time.sleep(2)
    print(f"  timed out: {what}", flush=True)
    return False


def main():
    shutil.rmtree(WORK, ignore_errors=True)
    os.makedirs(WORK)
    start("A")
    ida = call("A", "status")["node_id"]
    call("A", "wallet.create")
    a_arc = b58check(0x53, b58decode(call("A", "start.status")["address"]))
    stop("A")
    settings = {"net": "arctest", "genesis": {"time": int(time.time()) - 5, "closed_list": "01" * 32, "final_list": "02" * 32, "beacon_1": "03" * 32,
                "beacon_2": "04" * 32, "difficulty": "1099511627776"}, "holders": [{"key": ida, "bytes": 1 << 30, "pay": a_arc}],
                "steps_per_second": 1_000_000, "block_seconds": 2, "segment_bytes": 4096, "audit_period": 120}
    sp = os.path.join(WORK, "settings.json")
    json.dump(settings, open(sp, "w"))
    start("A", settings=sp)
    call("A", "chains.add", key="arctest")
    start("B", peer="A", settings=sp)
    start("C", peer="B", settings=sp)
    for n in "BC":
        call(n, "wallet.create")
        call(n, "chains.add", key="arctest")
        print(f"   {n} chains.add peptest:", json.dumps(call(n, "chains.add", key="peptest", mode="full"))[:200], flush=True)
    idb, idc = call("B", "status")["node_id"], call("C", "status")["node_id"]
    call("B", "sysinfo.set", on=True)
    time.sleep(5)
    for n in "BC":
        m = call(n, "chains.sizes")["measured"]
        print(f"   {n} measured:", json.dumps(m)[:300], flush=True)

    def row(id_):
        return next((a for a in call("A", "explorer.arc_nodes")["arcades"] if a["node"] == id_), None)

    print("1. A's arc_nodes lists itself, B (linked) and C (heard through B)")
    check("B and C in A's arc_nodes", wait("rows", lambda: row(idb) and row(idc) and any(c["key"] == "peptest" for c in row(idc)["chains"]), 300))
    v = call("A", "explorer.arc_nodes")
    print("   answer:", json.dumps({k: v[k] for k in ("chain", "height", "arcade", "floor", "totals", "truncated")}), flush=True)
    me = next(a for a in v["arcades"] if a["me"])
    check("A: its own row, this arcade", me["node"] == ida)
    rb, rc = row(idb), row(idc)
    check("B linked, V1.0, not beta", rb["linked"] is True and rb["beta"] is False)
    check("C not linked to A, still listed", rc["linked"] is False and rc["beta"] is False)
    check("B's machine line (shares system info)", wait("machine", lambda: row(idb)["machine"] is not None, 120))
    check("C has no machine line", row(idc)["machine"] is None)
    pc = lambda: next(c for c in row(idc)["chains"] if c["key"] == "peptest")
    print("   C's peptest on A:", json.dumps(pc()), flush=True)
    # C measured its own size and A isn't linked to C: the size came in C's hold report (the new `sz` part).
    print("   C size:", pc()["size"], "(nothing to measure: no blocks offline; the sz path is unit-tested)")
    check("peptest listed, kept in full: sealable", pc()["listed"] is True and pc()["sealable"] is True)
    pep = next((c for c in v["chains"] if c["key"] == "peptest"), None)
    check("Coins: peptest (the home chain here) kept in full by A, B and C", pep is not None and len(pep["full"]) == 3)
    r = json.loads(urllib.request.urlopen(f"http://127.0.0.1:{PORTS['A'][0]}/r/explorer/arc_nodes", timeout=30).read())
    check("GET /r/explorer/arc_nodes answers", r["ok"] is True and len(r["result"]["arcades"]) == 3)
    r = json.loads(urllib.request.urlopen(f"http://127.0.0.1:{PORTS['A'][0]}/r/explorer/arc_machines", timeout=30).read())
    check("GET /r/explorer/arc_machines answers", r["ok"] is True and r["result"]["sharing"] == 1)
    for n in list(procs):
        stop(n)
    print("RESULT:", "PASS" if not fails else "FAIL", fails, flush=True)
    return 1 if fails else 0


if __name__ == "__main__":
    try:
        sys.exit(main())
    finally:
        for n in list(procs):
            try:
                stop(n)
            except Exception:
                pass
