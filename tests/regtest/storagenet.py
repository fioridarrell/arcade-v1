#!/usr/bin/env python3
"""ARCADE storage analytics reads (crate::arcstats) on a closed local ARCADE chain with small segments, so sealing,
registration and payouts happen within minutes. A writes the chain (genesis holder) and seals; B runs its own copy
(installed) and earns; C never installs ARCADE and borrows it, so its reads are forwarded to A or B. Checks that
explorer.arc_storage_summary, arc_segments, arc_holders and arc_payouts answer with real numbers, agree on A and B,
and reach C through the mesh. Writes the answers to WORKDIR/answers.json (the app's mock can serve them).

Segments close only at the end of a 10,000-block range (params::RANGE, consensus), so no ARCADE segment seals in a
test this short: segment and replica contents are checked by the unit test arcstats::segments_and_holders_from_a_
genesis_list; here arc_segments must answer (empty).

Run inside: unshare -Urn sh -c "ip link set lo up; python3 storagenet.py ARCADED WORKDIR"
"""
import hashlib, json, os, shutil, socket, subprocess, sys, time

EXE, WORK = sys.argv[1], sys.argv[2]
PORTS = {"A": (18741, 18751), "B": (18742, 18752), "C": (18743, 18753)}
procs = {}
B58 = "123456789ABCDEFGHJKLMNPQRSTUVWXYZabcdefghijkmnopqrstuvwxyz"


def b58decode(s):
    n = 0
    for ch in s:
        n = n * 58 + B58.index(ch)
    raw = n.to_bytes(25, "big")
    return raw[1:21]


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
    s.settimeout(60)
    s.connect(os.path.join(home(n), "arcaded.sock"))
    s.sendall((json.dumps(p) + "\n").encode())
    r = json.loads(s.makefile().readline())
    s.close()
    if not r.get("ok"):
        raise RuntimeError(f"{n} {cmd}: {r.get('error')}")
    return r["result"]


def start(n, peer=None, settings=None, env=None):
    http, mesh = PORTS[n]
    args = ["nice", "-n", "19", EXE, "--home", home(n), "--http-port", str(http), "--mesh-port", str(mesh), "--stun-port", "0", "--no-seeds"]
    if peer:
        args += ["--mesh-peer", f"127.0.0.1:{PORTS[peer][1]}"]
    if settings:
        args += ["--arc-local", settings]
    log = open(os.path.join(WORK, f"{n}.log"), "a")
    procs[n] = subprocess.Popen(args, stdout=log, stderr=subprocess.STDOUT, env=dict(os.environ, **(env or {})))
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





def main():
    shutil.rmtree(WORK, ignore_errors=True)
    os.makedirs(WORK)
    start("A")
    ida = call("A", "status")["node_id"]
    call("A", "wallet.create")
    a_home = call("A", "start.status")["address"]
    stop("A")
    a_arc = b58check(0x53, b58decode(a_home))
    settings = {
        "net": "arctest",
        "genesis": {"time": int(time.time()) - 5, "closed_list": "01" * 32, "final_list": "02" * 32, "beacon_1": "03" * 32, "beacon_2": "04" * 32, "difficulty": "1099511627776"},
        "holders": [{"key": ida, "bytes": 1 << 30, "pay": a_arc}],
        "steps_per_second": 1_000_000,
        "block_seconds": 2,
        "segment_bytes": 4096,
        "audit_period": 120,
    }
    sp = os.path.join(WORK, "settings.json")
    json.dump(settings, open(sp, "w"))
    fails = []
    start("A", settings=sp)
    call("A", "chains.add", key="arctest")
    start("B", peer="A", settings=sp)
    call("B", "wallet.create")
    call("B", "chains.add", key="arctest")
    h = lambda n: call(n, "arc.status")["height"]
    summ = lambda n: call(n, "explorer.arc_storage_summary")
    show = lambda: f"height A {h('A')} B {h('B')}; summary A segments {summ('A')['segments']} replicas {summ('A')['replicas']} owners {summ('A')['owners']}"

    print("1. A writes; the pot fills and the genesis earner is paid at its turn")
    if not wait("a payout to an earner", lambda: any(e.get("paid", 0) > 0 for e in call("A", "explorer.arc_payouts", blocks=1152)["earners"]), 900, show=show):
        fails.append("no payout to an earner")
    s_a = summ("A")
    print("   summary A:", json.dumps({k: s_a[k] for k in ("height", "segments", "segment_bytes", "replicas", "average_copies", "owners", "genesis_earners", "pot", "issuance_per_month")}))
    if s_a["genesis_earners"] != 1 or not s_a["pot"] or s_a["issuance_per_month"] <= 0 or s_a["height"] < 100:
        fails.append("summary: missing numbers")
    segs = call("A", "explorer.arc_segments", limit=50)
    print("   segments A:", segs["total"], "first:", json.dumps(segs["segments"][0] if segs["segments"] else None)[:400])
    if "segments" not in segs or segs["total"] != len(segs["segments"]):
        fails.append("segments: doesn't answer")
    fam = call("A", "explorer.arc_segments", family="blocks", limit=5)
    if any(x["family"] != "blocks" for x in fam["segments"]):
        fails.append("family filter")
    hold = call("A", "explorer.arc_holders")
    print("   holders A:", json.dumps([{k: x.get(k) for k in ("kind", "node", "address", "replicas", "bytes", "pay_weight", "share", "accrued")} for x in hold["holders"]]))
    if not any(x["kind"] == "genesis" for x in hold["holders"]):
        fails.append("holders: no genesis earner")
    if abs(sum(x["share"] for x in hold["holders"]) - 1.0) > 0.01:
        fails.append(f"holders: shares add to {sum(x['share'] for x in hold['holders'])}")
    pay = call("A", "explorer.arc_payouts", blocks=300)
    print("   payouts A: epochs", len(pay["epochs"]), "earners", json.dumps(pay["earners"])[:400])
    gen = next((x for x in hold["holders"] if x["kind"] == "genesis"), {})
    if not any(e.get("address") == gen.get("address") and e["paid"] > 0 for e in call("A", "explorer.arc_payouts", blocks=1152)["earners"]):
        fails.append("payouts: the genesis earner wasn't named")
    if not pay["epochs"] or not all(e["minted"] == e["blocks"] * 50 * 100_000_000 for e in pay["epochs"]):
        fails.append("payouts: minted doesn't match blocks")
    if any(e["coinbase"] != e["writer"] + e["paid"] for e in pay["epochs"]):
        fails.append("payouts: coinbase != writer + paid")
    print("3. B agrees (same chain state)")
    wait("B at A's height", lambda: h("B") >= h("A") - 1, 300)
    hb, ha = call("B", "explorer.arc_segments", limit=500), call("A", "explorer.arc_segments", limit=500)
    if sorted(x["id"] for x in hb["segments"]) != sorted(x["id"] for x in ha["segments"]) and abs(h("A") - h("B")) <= 1:
        fails.append("A and B list different segments")
    ga, gb = call("A", "explorer.arc_holders")["holders"], call("B", "explorer.arc_holders")["holders"]
    if [(x["address"], x["pay_weight"]) for x in ga] != [(x["address"], x["pay_weight"]) for x in gb]:
        fails.append("A and B weigh holders differently")
    print("4. C borrows ARCADE: reads come from A or B")
    start("C", peer="A", settings=sp)  # knows the chain, never installs it: borrowed
    call("C", "wallet.create")
    got = {}
    def c_reads():
        got["s"] = call("C", "explorer.arc_storage_summary")
        got["h"] = call("C", "explorer.arc_holders")
        return got["s"]["height"] > 0 and got["h"]["holders"] and got["s"].get("_mesh")
    if not wait("C's forwarded reads", c_reads, 600, show=lambda: str(got.get("s", ""))[:200]):
        fails.append("C (borrowed) got no answers")
    else:
        print("   C summary from", got["s"].get("_mesh"), "segments", got["s"]["segments"])
    json.dump({"summary": summ("A"), "segments": call("A", "explorer.arc_segments", limit=500), "holders": call("A", "explorer.arc_holders"),
               "payouts": call("A", "explorer.arc_payouts", blocks=1152)}, open(os.path.join(WORK, "answers.json"), "w"), indent=1)
    for n in list(procs):
        stop(n)
    print("RESULT:", "PASS" if not fails else "FAIL", fails)
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
