#!/usr/bin/env python3
"""A:// ops 8 to 10, reacts, replies and tips on any transaction (docs/APROTO.md), on a closed local ARCADE chain.
A writes the chain and posts; B (its own copy) likes, unlikes, replies and tips; A reacts too. Below the ops' height
they're refused; above it both arcades count the same: likes (newest react per author), replies in order, tips per
recipient with the author marked, and a tip on a transaction whose author can't be found.

Run inside: unshare -Urn sh -c "ip link set lo up; python3 socialnet.py ARCADED WORKDIR"
"""
import hashlib, json, os, shutil, socket, subprocess, sys, time

EXE, WORK = sys.argv[1], sys.argv[2]
PORTS = {"A": (18641, 18651), "B": (18642, 18652)}
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




def arc_balance(n):
    ks = call(n, "wallet.keys", chain="arctest")
    return sum((k.get("balance") or 0) for k in ks)


def confirmed(n, txid):
    return (call(n, "explorer.tx", chain="arctest", txid=txid, local=True).get("height") or 0) > 0


def counts(n, targets, me=False):
    r = call(n, "a.reactions", targets=targets, me=me)["targets"]
    if not me:
        for t in r:
            t.pop("mine", None)
    return r


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
    }
    sp = os.path.join(WORK, "settings.json")
    json.dump(settings, open(sp, "w"))
    fails = []
    start("A", settings=sp)
    call("A", "chains.add", key="arctest")
    h = lambda n: call(n, "arc.status")["height"]

    print("1. A writes until it has ARCADE to spare (its writer share)")
    if not wait("A has spendable ARCADE", lambda: call("A", "wallet.send", chain="arctest", to=a_arc, amount=300_000_000) is not None, 1500, show=lambda: f"height A {h('A')}"):
        return 1
    # Ops 8 to 10 from a height a little ahead, on both arcades (as a release names one).
    at = h("A") + 45
    env = {"ARCADE_A_SOCIAL_FROM": str(at)}
    print("   A at height", h("A"), "- ops 8 to 10 known from", at)
    stop("A")
    start("A", settings=sp, env=env)
    start("B", peer="A", settings=sp, env=env)
    call("B", "wallet.create")
    call("B", "chains.add", key="arctest")
    if not wait("B's own ARCADE copy catches up", lambda: h("B") + 2 >= h("A") and h("B") > 0, 600, show=lambda: f"B {h('B')} A {h('A')}"):
        return 1
    b_arc = call("B", "wallet.keys", chain="arctest")[0]["address"]
    r = call("A", "wallet.send", chain="arctest", to=b_arc, amount=500_000_000, send=True)
    print("   A pays B 5 ARCADE:", (r.get("broadcast") or {}).get("txid"))
    if not wait("B has 5 ARCADE", lambda: arc_balance("B") >= 500_000_000, 300, show=lambda: f"B balance {arc_balance('B')}"):
        return 1

    print("2. A posts on ARCADE (A:// op 6): the target")
    post = call("A", "a.post", text="a thing to react to")["txid"]
    wait("A's post in a block", lambda: confirmed("A", post), 300)
    T = {"chain": "arctest", "txid": post}
    # A transaction no arcade here can read: its author is unknown, tips still count.
    U = {"chain": "doge", "txid": "11" * 32}
    third = b58check(0x53, bytes([0x22] * 20))

    print("3. Below block", at, "the ops are refused")
    if h("B") < at - 3:
        try:
            call("B", "a.react", kind="like", **T)
            fails.append("a react was taken below the ops' height")
        except RuntimeError as e:
            print("   refused:", e)
            if "known from block" not in str(e):
                fails.append(f"refusal doesn't name the height: {e}")
    else:
        fails.append("the chain passed the ops' height before the test could try")
    if not wait(f"the chain reaches {at}", lambda: h("B") >= at and h("A") >= at, 600, show=lambda: f"B {h('B')} A {h('A')}"):
        return 1

    print("4. B likes, A likes, B unlikes and likes again, A dislikes then undislikes; B replies twice; B tips")
    sent = []
    def do(n, cmd, **p):
        r = call(n, cmd, **p)
        sent.append((n, r["txid"]))
        print("  ", n, cmd, p.get("kind") or p.get("text") or p.get("amount"), "->", r["txid"][:16], r.get("to", ""))
        # One record a block for each sender: the newest-wins order is the chain's.
        wait(f"{cmd} in a block", lambda: confirmed(n, r["txid"]), 300)
        return r
    do("B", "a.react", kind="like", **T)
    do("A", "a.react", kind="like", **T)
    do("B", "a.react", kind="unlike", **T)
    do("B", "a.react", kind="like", **T)
    do("A", "a.react", kind="dislike", **T)
    do("A", "a.react", kind="undislike", **T)
    do("B", "a.reply", text="first reply", **T)
    do("B", "a.reply", text="second reply ☀", **T)
    tip1 = do("B", "a.tip", amount=10_000_000, memo="thanks", **T)
    do("B", "a.tip", amount=3_000_000, to=third, **T)
    try:
        call("B", "a.tip", amount=1_000_000, **U)
        fails.append("a tip on an unknown author's transaction went somewhere without a to")
    except RuntimeError as e:
        print("   no author, no to: refused:", str(e)[:100])
    do("B", "a.tip", amount=2_000_000, to=third, **U)
    if tip1.get("to") != a_arc:
        fails.append(f"the tip didn't default to the author: {tip1.get('to')} vs {a_arc}")
    wait("A has B's last records", lambda: all(confirmed("A", t) for _, t in sent), 300)
    time.sleep(4)

    print("5. Both arcades count the same")
    ca, cb = counts("A", [T, U]), counts("B", [T, U])
    print("   A:", json.dumps(ca)[:600])
    if ca != cb:
        fails.append(f"A and B differ: {json.dumps(ca)} vs {json.dumps(cb)}")
    t = ca[0]
    # B likes; A's dislike replaced its like and its undislike cleared it: one like, no dislike.
    want = {"likes": 1, "dislikes": 0, "replies": 2}
    if {k: t[k] for k in want} != want:
        fails.append(f"counts {({k: t[k] for k in want})}, want {want}")
    if (t["tips"]["total"], t["tips"]["count"], t["tips"]["to_author"]) != (13_000_000, 2, 10_000_000):
        fails.append(f"tips {t['tips']}")
    if (t.get("author") or {}).get("address") != a_arc:
        fails.append(f"author {t.get('author')}, want {a_arc}")
    if not any(x["address"] == third and x["sats"] == 3_000_000 and x["author"] is False for x in t["tips"]["to"]):
        fails.append("the tip to a third address isn't listed as not the author's")
    u = ca[1]
    if u.get("author") is not None or u["tips"]["total"] != 2_000_000 or u["tips"]["to_author"] != 0:
        fails.append(f"the unknown author's target: {u}")
    mine = counts("B", [T], me=True)[0]["mine"]
    print("   B's own:", mine)
    if mine != {"react": "like", "tipped": 13_000_000}:
        fails.append(f"B's mine {mine}")
    ra, rb = call("A", "a.replies", **T), call("B", "a.replies", **T)
    print("   replies:", [(x["text"], x["author"]["address"][:8], x["height"]) for x in ra["replies"]])
    if ra != rb or [x["text"] for x in ra["replies"]] != ["first reply", "second reply ☀"] or ra["total"] != 2:
        fails.append(f"replies differ or wrong: {ra} vs {rb}")
    if any(x["author"]["address"] != b_arc for x in ra["replies"]):
        fails.append("a reply's author isn't B")
    page = call("A", "a.replies", offset=1, limit=1, **T)
    if [x["text"] for x in page["replies"]] != ["second reply ☀"]:
        fails.append(f"paging {page}")
    print("RESULT:", "PASS" if not fails else "FAIL", fails)
    return 0 if not fails else 1


try:
    rc = main()
finally:
    for n in list(procs):
        stop(n)
sys.exit(rc)
