#!/usr/bin/env python3
"""Pepecoin testnet borrowed until installed (Martin 2026-11-18), on a closed network: no coin peers, no Internet.

N is a new arcade: Pepecoin testnet is not installed (no sync, no mining), chains.list and chains.status say so.
Installed (chains.add), its own copy starts (looks for coin peers); removed (chains.remove), it is borrowed again,
and that survives a restart. O is an arcade that needs the chain from the start (ARCADE_HOME_INSTALLED=1): installed.

What this can't show on a closed network: balances and sends through a lender, since no closed-net arcade has
Pepecoin testnet blocks to lend (the chain's real history; the borrowed copy itself is the one fresh installs use
already while they catch up).

Run inside: unshare -Urn sh -c "ip link set lo up; python3 homenet.py ARCADED WORKDIR"
"""
import json, os, shutil, socket, subprocess, sys, time

EXE, WORK = sys.argv[1], sys.argv[2]
PORTS = {"N": (18721, 18731), "O": (18722, 18732)}
procs = {}


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
    return r


def ok(n, cmd, **p):
    r = call(n, cmd, **p)
    if not r.get("ok"):
        raise RuntimeError(f"{n} {cmd}: {r.get('error')}")
    return r["result"]


def start(n, env=None):
    http, mesh = PORTS[n]
    args = ["nice", "-n", "19", EXE, "--home", home(n), "--http-port", str(http), "--mesh-port", str(mesh), "--stun-port", "0", "--no-seeds"]
    log = open(os.path.join(WORK, f"{n}.log"), "a")
    procs[n] = subprocess.Popen(args, stdout=log, stderr=subprocess.STDOUT, env=dict(os.environ, **(env or {})))
    for _ in range(600):
        try:
            ok(n, "status")
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
            print("   ", what, ":", e)
        time.sleep(1)
    print(f"  timed out: {what}")
    return False


def row(n, key="peptest"):
    return next(c for c in ok(n, "chains.list") if c["key"] == key)


def main():
    shutil.rmtree(WORK, ignore_errors=True)
    os.makedirs(WORK)
    fails = []

    def check(name, cond):
        print(("  ok   " if cond else "  FAIL ") + name)
        if not cond:
            fails.append(name)

    print("1. a new arcade: Pepecoin testnet borrowed, not installed")
    start("N")
    ok("N", "wallet.create")
    check("chains.list: not installed", wait("list row", lambda: row("N")["installed"] is False, 30))
    check("status: borrowed, not syncing", wait("sync state", lambda: "borrowed" in ok("N", "status")["sync"], 30))
    check("chains.status: not installed, home", (lambda st: st.get("installed") is not True and st.get("home") is True)(ok("N", "chains.status", key="peptest")))
    r = call("N", "mine.start", threads="1")
    check("mining refused while borrowed", not r.get("ok") and "borrowed" in (r.get("error") or ""))
    check("no blocks taken", ok("N", "status")["tip"] is None)

    print("2. installed: its own copy starts")
    ok("N", "chains.add", key="peptest", mode="full")
    check("chains.list: installed", wait("installed", lambda: row("N")["installed"] is True, 30))
    check("its own copy looks for coin peers", wait("syncing", lambda: "borrowed" not in ok("N", "status")["sync"], 30))
    r = call("N", "chains.add", key="peptest", mode="full")
    check("installing again changes nothing", r.get("ok"))

    print("3. removed: borrowed again, and after a restart")
    ok("N", "chains.remove", key="peptest")
    check("chains.list: not installed", wait("removed", lambda: row("N")["installed"] is False, 30))
    check("status: borrowed again", wait("sync borrowed", lambda: "borrowed" in ok("N", "status")["sync"], 30))
    r = call("N", "chains.add", key="peptest", mode="full")
    check("installing while its blocks are deleted is refused", not r.get("ok") and "deleted" in (r.get("error") or ""))
    stop("N")
    start("N")
    check("after a restart: still not installed", wait("restart", lambda: row("N")["installed"] is False and "borrowed" in ok("N", "status")["sync"], 30))
    r = call("N", "chains.remove", key="peptest")
    check("removing what isn't installed is refused", not r.get("ok"))

    print("4. an arcade that needs the chain from the start (ARCADE_HOME_INSTALLED=1): installed")
    start("O", env={"ARCADE_HOME_INSTALLED": "1"})
    check("chains.list: installed", wait("O installed", lambda: row("O")["installed"] is True, 30))
    stop("O")
    start("O")
    check("after a restart without the setting: still installed", wait("O restart", lambda: row("O")["installed"] is True, 30))

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
