#!/usr/bin/env python3
"""Borrowed = installed, batch 3 (feed and messages), on a closed local ARCADE chain. A writes it, claims @alice and
posts with a picture; C never installs ARCADE (it borrows it from A, so its posts and profiles are lent rows). On C:
feed.file of A's post returns the picture's exact bytes (explorer.msg_file from A); msg.stats counts the lent rows;
and once C's own messaging key is on chain (known to C only through lent profile rows), msg.announce sends nothing
more across two of its re-announce waits (ARCADE_ANNOUNCE_AGAIN_SECS shortened), where it used to pay again each time.

Run inside: unshare -Urn sh -c "ip link set lo up; python3 borrowfeed.py ARCADED WORKDIR"
"""
import hashlib, json, os, shutil, socket, subprocess, sys, time

EXE, WORK = sys.argv[1], sys.argv[2]
PORTS = {"A": (18661, 18671), "C": (18662, 18672)}
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


def tag_status(n, tag):
    for t in call(n, "tag.list"):
        if t["tag"] == tag:
            return t["status"]
    return None


PICTURE = bytes(range(256)) * 12 + b"borrowfeed picture end"


def main():
    import base64
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
    print("1. A writes until it has ARCADE to spare, then claims @alice")
    if not wait("A has 3 ARCADE", lambda: call("A", "wallet.send", chain="arctest", to=a_arc, amount=300_000_000) is not None, 1500, show=lambda: f"height A {h('A')}"):
        return 1
    call("A", "tag.claim", tag="alice", chain="arctest")
    if not wait("@alice confirmed on A", lambda: tag_status("A", "alice") == "confirmed", 600, show=lambda: f"A claim {tag_status('A', 'alice')}"):
        return 1
    print("2. A posts with a picture")
    post = call("A", "feed.post", text="a picture from alice", channel="borrowfeed",
                file={"name": "pic.bin", "type": "image/png", "data": base64.b64encode(PICTURE).decode()})
    print("   feed.post answered", json.dumps(post)[:300])
    # The post is filed under its first chunk's txid once every chunk is in a block (a picture post is several
    # transactions on ARCADE): read its id back from A's own feed.
    ids = lambda: [p.get("txid") for p in call("A", "feed", order="new", channel="borrowfeed").get("posts", []) if (p.get("h") or p.get("height") or 0)]
    if not wait("A's post in a block", lambda: ids(), 300):
        fails.append("A's post didn't confirm")
        return 1
    post_tx = ids()[0]
    print("   A's feed files it as", post_tx, "(feed.post said", post.get("txid"), ")")
    start("C", peer="A", settings=sp, env={"ARCADE_ANNOUNCE_AGAIN_SECS": "20"})
    call("C", "wallet.create")
    print("3. C (ARCADE not installed) borrows it, gets ARCADE and claims @carol (lent rows need a handle)")
    if not wait("C borrows arctest", lambda: borrowed("C")[0] and (borrowed("C")[1].get("tip") or 0) > 0, 300):
        fails.append("C doesn't borrow")
        return 1
    c_addr = [k["address"] for k in call("C", "wallet.keys", chain="arctest")][0]
    call("A", "wallet.send", chain="arctest", to=c_addr, amount=200_000_000, send=True)
    if not wait("C sees 2 ARCADE", lambda: arc_balance("C") >= 200_000_000, 300):
        fails.append("C doesn't see its ARCADE")
        return 1
    call("C", "tag.claim", tag="carol", chain="arctest")
    if not wait("@carol confirmed on C", lambda: tag_status("C", "carol") == "confirmed", 600, show=lambda: f"C claim {tag_status('C', 'carol')}"):
        fails.append("C's claim of @carol didn't confirm")
        return 1
    print("4. On C: feed.file of A's post (from A, through explorer.msg_file)")
    def got_file():
        r = call("C", "feed.file", txid=post_tx)
        return base64.b64decode(r["data"]) == PICTURE and r.get("type") == "image/png"
    def lent_view():
        try:
            return json.dumps(call("C", "ledger.lent").get("arctest"))[:240] + " | feed: " + json.dumps([p.get("txid") for p in call("C", "feed", order="new", channel="borrowfeed").get("posts", [])])
        except Exception as e:
            return str(e)
    if not wait("C's feed.file returns the picture", got_file, 600, show=lent_view):
        fails.append("feed.file of a lent post didn't return the picture")
    print("5. On C: msg.stats counts lent rows")
    st = call("C", "msg.stats")
    print("   ", json.dumps(st))
    if not (st.get("lent") or {}).get("post"):
        fails.append(f"msg.stats on C lists no lent posts: {st}")
    print("6. On C: its messaging key goes on chain once; then no re-announce across two waits")
    try:
        first = call("C", "msg.announce").get("txid")
        print("   first announce:", first)
    except Exception as e:
        print("   msg.announce:", e)
    # Lent profiles are keyed by address, so a new one comes with the whole re-read (every 10 minutes, lentrows
    # PROFILES_AGAIN), well inside the 30-minute re-announce wait this guards.
    known = lambda: (call("C", "msg.who", name="@carol") or {}).get("key") is not None
    if not wait("C's key known through lent profiles", known, 900, show=lambda: lent_view()):
        fails.append("C's own profile isn't known on C")
    for i in (1, 2):
        time.sleep(25)  # past ARCADE_ANNOUNCE_AGAIN_SECS
        again = call("C", "msg.announce").get("txid")
        print(f"   after wait {i}: announce txid {again}")
        if again:
            fails.append(f"msg.announce sent again after wait {i} ({again})")
    print("RESULT:", "PASS" if not fails else "FAIL", fails)
    return 0 if not fails else 1


try:
    rc = main()
finally:
    for n in list(procs):
        stop(n)
sys.exit(rc)
