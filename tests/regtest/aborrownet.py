#!/usr/bin/env python3
"""A:// writes from an arcade that only borrows ARCADE (Martin 2026-11-18: "It's just a transaction that needs to be
sent"), on a closed local ARCADE chain. A writes the chain; C never installs it. C publishes a profile, follows A's
@tag, saves a contact, likes, replies to and tips A's post, all signed at C and sent by its lender, and every one
confirms on A and is counted there. A blob (a long body) is refused with a clear reason.

Run inside: unshare -Urn sh -c "ip link set lo up; python3 aborrownet.py ARCADED WORKDIR"
"""
import hashlib, json, os, shutil, socket, subprocess, sys, time

EXE, WORK = sys.argv[1], sys.argv[2]
PORTS = {"A": (18641, 18651), "C": (18642, 18652)}
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



def borrowed(n):
    for c in call(n, "borrow.status")["chains"]:
        if c["key"] == "arctest":
            return c.get("borrowed") is True, c
    return False, None


def arc_balance(n):
    ks = call(n, "wallet.keys", chain="arctest")
    return sum((k.get("balance") or 0) for k in ks)


def confirmed(n, txid):
    return (call(n, "explorer.tx", chain="arctest", txid=txid, local=True).get("height") or 0) > 0


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

    print("1. A writes until it has ARCADE to spare, claims @alpha and posts (the target)")
    if not wait("A has 3 ARCADE", lambda: call("A", "wallet.send", chain="arctest", to=a_arc, amount=300_000_000) is not None, 1500, show=lambda: f"height A {h('A')}"):
        return 1
    call("A", "tag.claim", tag="alpha", chain="arctest")
    if not wait("@alpha confirmed", lambda: call("A", "explorer.tag", chain="arctest", tag="alpha", local=True).get("home_address"), 600):
        return 1
    post = call("A", "a.post", text="react to me")["txid"]
    wait("A's post in a block", lambda: confirmed("A", post), 300)
    T = {"chain": "arctest", "txid": post}

    print("2. C (never installs ARCADE) borrows it and gets 3 ARCADE from A")
    start("C", peer="A", settings=sp)
    call("C", "wallet.create")
    if not wait("C borrows arctest", lambda: borrowed("C")[0] and (borrowed("C")[1].get("tip") or 0) > 0, 300, show=lambda: json.dumps(borrowed("C")[1])[:300]):
        return 1
    installed = [c["installed"] for c in call("C", "borrow.status")["chains"] if c["key"] == "arctest"]
    print("   C installed?", installed)
    if installed != [False]:
        fails.append("C has ARCADE installed: the test proves nothing")
    c_arc = call("C", "wallet.keys", chain="arctest")[0]["address"]
    call("A", "wallet.send", chain="arctest", to=c_arc, amount=300_000_000, send=True)
    if not wait("C sees 3 ARCADE", lambda: arc_balance("C") >= 300_000_000, 300, show=lambda: f"C balance {arc_balance('C')}"):
        return 1

    print("3. C writes A:// records through its lender, one block each")
    done = {}
    def do(label, cmd, **p):
        try:
            r = call("C", cmd, **p)
        except RuntimeError as e:
            fails.append(f"{label}: {e}")
            print("  ", label, "REFUSED:", e)
            return None
        tx = r.get("txid")
        print("  ", label, "->", (tx or "")[:16], r.get("to", ""))
        if not tx or not wait(f"{label} in a block on A", lambda: confirmed("A", tx), 300):
            fails.append(f"{label} didn't confirm on A")
        done[label] = tx
        return r
    do("profile", "a.profile_set", name="Borrowed C", bio="writes without installing ARCADE")
    do("follow", "a.follow", tag="alpha")
    do("contact", "a.contact_save", name="Alpha", tag="alpha", addresses={"arcade": a_arc})
    do("like", "a.react", kind="like", **T)
    do("reply", "a.reply", text="from a borrowed ARCADE", **T)
    tip = do("tip", "a.tip", amount=5_000_000, memo="thanks", **T)
    if tip and tip.get("to") != a_arc:
        fails.append(f"the tip didn't go to the author: {tip.get('to')} vs {a_arc}")
    time.sleep(4)

    print("4. A counts them")
    t = call("A", "a.reactions", targets=[T])["targets"][0]
    print("   A:", json.dumps({k: t[k] for k in ("likes", "replies", "tips")})[:300])
    if (t["likes"], t["replies"], t["tips"]["to_author"]) != (1, 1, 5_000_000):
        fails.append(f"A's counts {t}")
    tc = call("C", "a.reactions", targets=[T])["targets"][0]
    if (tc["likes"], tc["replies"], tc["tips"]["total"]) != (t["likes"], t["replies"], t["tips"]["total"]):
        fails.append(f"C (borrowed) reads other counts than A: {tc} vs {t}")
    rp = call("A", "a.replies", **T)
    if [x["author"]["address"] for x in rp["replies"]] != [c_arc]:
        fails.append(f"the reply's author isn't C: {rp}")
    prof = call("A", "a.profile", address=c_arc)
    print("   A sees C's profile:", prof.get("profile"))
    if (prof.get("profile") or {}).get("n") != "Borrowed C":
        fails.append(f"A doesn't have C's profile: {prof.get('profile')}")
    fol = call("A", "a.follows", tag="alpha")
    names = json.dumps(fol)
    if c_arc not in names:
        fails.append(f"C isn't among @alpha's followers on A: {names[:300]}")

    print("5. A blob (long body) from C is refused, saying why")
    try:
        call("C", "arc.blob_publish", hex=("7b2278223a22" + "79" * 2000 + "227d"), type="application/json", send=True)
        fails.append("a blob was taken on a borrowed ARCADE")
    except RuntimeError as e:
        print("   refused:", str(e)[:160])
        if "need ARCADE installed" not in str(e):
            fails.append(f"the refusal doesn't say why: {e}")
    print("RESULT:", "PASS" if not fails else "FAIL", fails)
    return 0 if not fails else 1


try:
    rc = main()
finally:
    for n in list(procs):
        stop(n)
sys.exit(rc)
