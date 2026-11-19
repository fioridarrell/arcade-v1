#!/usr/bin/env python3
"""@tags per chain (Martin 2026-11-18), on a closed local ARCADE chain. A writes it and claims @alice by its full
name (alice.arctest); C never installs ARCADE (it borrows it from A). Checks: tag.check and tag.resolve answer full names
and labels; a dot naming no tag chain is refused; on C, explorer.search @alice gives alice.arctest, wallet.send to
@alice.arctest pays alice's address, profile @alice.arctest lists her tags each with its chain, follow @alice.arctest
works, and the feed shows A's post with alice's label (@alice · ARCADEtest).

(Two people holding one name on two chains is the unit test hometag::tests::one_name_two_people_on_two_chains: a closed
net here can't run a second chain carrying tags.)

Run inside: unshare -Urn sh -c "ip link set lo up; python3 tagsnet.py ARCADED WORKDIR"
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
    print("1. A writes until it has ARCADE to spare, then claims @alice by its full name, alice.arctest")
    if not wait("A has 3 ARCADE", lambda: call("A", "wallet.send", chain="arctest", to=a_arc, amount=300_000_000) is not None, 1500, show=lambda: f"height A {h('A')}"):
        return 1
    ck = call("A", "tag.check", tag="alice.arctest")
    print("   tag.check alice.arctest:", ck)
    if not (ck.get("available") and ck.get("full") == "alice.arctest" and ck.get("label") == "@alice · ARCADEtest" and ck.get("chain") == "arctest"):
        fails.append(f"tag.check alice.arctest before the claim: {ck}")
    try:
        call("A", "tag.check", tag="alice.nochain")
        fails.append("tag.check alice.nochain was accepted")
    except RuntimeError as e:
        print("   alice.nochain refused:", e)
    call("A", "tag.claim", tag="alice.arctest")
    if not wait("@alice confirmed on A", lambda: tag_status("A", "alice") == "confirmed", 600, show=lambda: f"A height {h('A')} claim {tag_status('A', 'alice')}"):
        fails.append("A's claim of @alice.arctest didn't confirm")
        return 1
    ck = call("A", "tag.check", tag="alice.arctest")
    if ck.get("available") or "taken" not in (ck.get("reason") or ""):
        fails.append(f"tag.check after the claim: {ck}")
    rs = call("A", "tag.resolve", tag="@alice")
    print("   tag.resolve @alice on A:", rs)
    if rs.get("found") != "one" or rs["choices"][0]["tag"] != "alice.arctest" or rs["choices"][0]["label"] != "@alice · ARCADEtest":
        fails.append(f"tag.resolve @alice on A: {rs}")
    if call("A", "tag.resolve", tag="@nobody_here").get("found") != "none":
        fails.append("tag.resolve of an unclaimed name isn't none")
    et = call("A", "explorer.tag", chain="arctest", tag="alice.arctest")
    alice_addr = et["home_address"]
    if et.get("full") != "alice.arctest" or et.get("chain") != "arctest":
        fails.append(f"explorer.tag alice.arctest: {json.dumps(et)[:200]}")
    print("   on A, @alice.arctest =", alice_addr)
    print("2. A posts on the feed")
    post = call("A", "feed.post", text="hello from alice", channel="tagsnet")
    post_tx = post.get("txid")
    start("C", peer="A", settings=sp)
    call("C", "wallet.create")
    print("3. C (ARCADE not installed) borrows it, gets coins, claims @carol.arctest")
    if not wait("C borrows arctest", lambda: borrowed("C")[0] and (borrowed("C")[1].get("tip") or 0) > 0, 300, show=lambda: json.dumps(borrowed("C")[1])[:200]):
        fails.append("C doesn't borrow")
        return 1
    c_addr = [k["address"] for k in call("C", "wallet.keys", chain="arctest")][0]
    call("A", "wallet.send", chain="arctest", to=c_addr, amount=200_000_000, send=True)
    if not wait("C sees 2 ARCADE", lambda: arc_balance("C") >= 200_000_000, 300, show=lambda: f"C balance {arc_balance('C')}"):
        fails.append("C doesn't see its ARCADE")
        return 1
    call("C", "tag.claim", tag="carol.arctest")
    if not wait("@carol confirmed on C", lambda: tag_status("C", "carol") == "confirmed", 600, show=lambda: f"C claim {tag_status('C', 'carol')}"):
        fails.append("C's claim of @carol.arctest didn't confirm")
    same = lambda a: a and b58decode(a) == b58decode(alice_addr)
    print("4. On C: tag.resolve @alice and explorer.search @alice give alice.arctest")
    if not wait("tag.resolve @alice on C", lambda: (call("C", "tag.resolve", tag="@alice").get("choices") or [{}])[0].get("tag") == "alice.arctest", 180,
                show=lambda: json.dumps(call("C", "tag.resolve", tag="@alice"))[:200]):
        fails.append("tag.resolve @alice on C")
    try:
        sr = call("C", "explorer.search", chain="arctest", q="@alice")
        print("   search:", json.dumps(sr)[:200])
        if sr.get("kind") != "tag" or sr.get("id") != "alice.arctest":
            fails.append(f"explorer.search @alice on C: {sr}")
    except Exception as e:
        fails.append(f"explorer.search @alice: {e}")
    print("5. On C: wallet.send to @alice.arctest pays alice's address")
    try:
        r = call("C", "wallet.send", chain="arctest", to="@alice.arctest", amount=10_000_000, send=True)
        tx = r.get("txid") or (r.get("broadcast") or {}).get("txid")
        print("   sent", tx)
        if not wait("the send to @alice.arctest is in a block on A", lambda: (call("A", "explorer.tx", chain="arctest", txid=tx).get("height") or 0) > 0, 300):
            fails.append("the send to @alice.arctest didn't confirm")
        else:
            outs = call("A", "explorer.tx", chain="arctest", txid=tx).get("outputs") or []
            if not any(same(o.get("address")) and o.get("value") == 10_000_000 for o in outs):
                fails.append(f"the send to @alice.arctest didn't pay alice's address: {outs}")
    except Exception as e:
        fails.append(f"wallet.send @alice.arctest: {e}")
    print("6. profile @alice.arctest lists her tags, each with its chain")
    try:
        pr = call("A", "profile", who="@alice.arctest", chain="arctest")
        print("   tags:", pr.get("tags"))
        if not any(t.get("tag") == "alice.arctest" and t.get("label") == "@alice · ARCADEtest" for t in pr.get("tags") or []):
            fails.append(f"profile tags: {pr.get('tags')}")
    except Exception as e:
        fails.append(f"profile @alice.arctest: {e}")
    print("7. On C: follow @alice.arctest")
    try:
        call("C", "follow", who="@alice.arctest")
        if not wait("@alice has C as a follower", lambda: (call("C", "follows", who="@alice.arctest").get("followers") or 0) >= 1, 300,
                    show=lambda: json.dumps(call("C", "follows", who="@alice.arctest"))[:200]):
            fails.append("follow @alice.arctest from C doesn't show in follows")
    except Exception as e:
        fails.append(f"follow @alice.arctest: {e}")
    print("8. The feed on A shows the post with alice's label")
    def post_author():
        for p in call("A", "feed", order="new", channel="tagsnet").get("posts", []):
            if p.get("txid") == post_tx:
                return p.get("author") or {}
        return {}
    if not wait("A's post carries @alice · ARCADEtest", lambda: post_author().get("label") == "@alice · ARCADEtest", 180, show=lambda: post_author()):
        fails.append(f"the post's author label: {post_author()}")
    print("RESULT:", "PASS" if not fails else "FAIL", fails)
    return 0 if not fails else 1


try:
    rc = main()
finally:
    for n in list(procs):
        stop(n)
sys.exit(rc)
