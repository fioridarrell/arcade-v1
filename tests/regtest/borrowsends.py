#!/usr/bin/env python3
"""Borrowed = installed, batch 1 (sends, coins, fees, history, pool, profile), on a closed local ARCADE chain. A writes
it; C never installs it. The site API (tx.coins, tx.fee, tx.activity, tx.broadcast), mempool and profile answer on C
for its borrowed ARCADE, from A; C's own send shows as waiting, then in its history.

Run inside: unshare -Urn sh -c "ip link set lo up; python3 borrowsends.py ARCADED WORKDIR"
"""
import hashlib, json, os, shutil, socket, subprocess, sys, time

EXE, WORK = sys.argv[1], sys.argv[2]
PORTS = {"A": (18621, 18631), "C": (18622, 18632), "D": (18623, 18633)}
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
    print("1. A writes until it has ARCADE to spare")
    if not wait("A has 3 ARCADE", lambda: call("A", "wallet.send", chain="arctest", to=a_arc, amount=300_000_000) is not None, 1500, show=lambda: f"height A {h('A')}"):
        return 1
    start("C", peer="A", settings=sp)
    call("C", "wallet.create")
    print("2. C (ARCADE not installed) borrows it")
    if not wait("C borrows arctest", lambda: borrowed("C")[0] and (borrowed("C")[1].get("tip") or 0) > 0, 300, show=lambda: json.dumps(borrowed("C")[1])[:200]):
        fails.append("C doesn't borrow")
        return 1
    c_addr = [k["address"] for k in call("C", "wallet.keys", chain="arctest")][0]
    print("   C's ARCADE address", c_addr)
    print("3. A pays C 2 ARCADE")
    pay = call("A", "wallet.send", chain="arctest", to=c_addr, amount=200_000_000, send=True)
    pay_tx = (pay.get("broadcast") or {}).get("txid") or pay.get("txid")
    if not wait("C sees 2 ARCADE", lambda: arc_balance("C") >= 200_000_000, 300, show=lambda: f"C balance {arc_balance('C')}"):
        fails.append("C doesn't see its ARCADE")
    print("4. The site API on C, for its borrowed ARCADE")
    try:
        coins = call("C", "tx.coins", chain="arctest", addresses=[c_addr])
        print("   tx.coins:", json.dumps(coins)[:200])
        if "is not installed" in json.dumps(coins) or not json.dumps(coins).count(pay_tx or "x"):
            fails.append("tx.coins doesn't list A's payment")
    except Exception as e:
        fails.append(f"tx.coins: {e}")
    try:
        fee = call("C", "tx.fee", chain="arctest")
        print("   tx.fee:", fee)
        if fee.get("chain") != "arctest":
            fails.append("tx.fee isn't ARCADE's")
    except Exception as e:
        fails.append(f"tx.fee: {e}")
    def act():
        return call("C", "tx.activity", chain="arctest", addresses=[c_addr])
    if not wait("tx.activity lists A's payment", lambda: any(r.get("txid") == pay_tx for r in act()["addresses"][c_addr]["confirmed"]), 120, show=lambda: json.dumps(act())[:300]):
        fails.append("tx.activity doesn't list A's payment")
    print("5. C sends 0.5 back; it shows in mempool and as waiting, then in history")
    r = call("C", "wallet.send", chain="arctest", to=a_arc, amount=50_000_000, send=True)
    back = r.get("txid") or (r.get("broadcast") or {}).get("txid")
    print("   sent", back)
    try:
        mp = call("C", "mempool", chain="arctest")
        print("   mempool:", json.dumps(mp)[:200])
        if not any(t["txid"] == back for t in mp["txs"]) and (call("A", "explorer.tx", chain="arctest", txid=back).get("height") or 0) == 0:
            fails.append("mempool doesn't show C's waiting send")
    except Exception as e:
        fails.append(f"mempool: {e}")
    a1 = act()["addresses"][c_addr]
    print("   waiting rows:", a1["waiting"])
    if not wait("A has C's send in a block", lambda: (call("A", "explorer.tx", chain="arctest", txid=back).get("height") or 0) > 0, 300):
        fails.append("C's send didn't confirm")
    elif not wait("tx.activity lists C's send", lambda: any(r.get("txid") == back for r in act()["addresses"][c_addr]["confirmed"]), 120):
        fails.append("tx.activity doesn't list C's send once it is in a block")
    print("6. profile on C for its borrowed ARCADE")
    try:
        pr = call("C", "profile", chain="arctest", who=c_addr)
        print("   profile:", {k: pr.get(k) for k in ("chain", "address", "balance", "history_rows")})
        if not pr.get("balance"):
            fails.append("profile shows no balance")
    except Exception as e:
        fails.append(f"profile: {e}")
    print("RESULT:", "PASS" if not fails else "FAIL", fails)
    return 0 if not fails else 1


try:
    rc = main()
finally:
    for n in list(procs):
        stop(n)
sys.exit(rc)
