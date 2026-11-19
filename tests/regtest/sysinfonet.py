#!/usr/bin/env python3
"""Shared system info and ping per arcade (sysinfo.rs, linkrtt.rs) on a closed network: three arcades, B linked to
A and C. A shares its system info, C doesn't.

- Off by default: A's sysinfo.status is off, with the exact record it would send.
- A switches on: B's mesh.sysinfo shows A's machine and counts 1 of 2 sharing; C (not linked to A) has it too,
  passed on by B. B's mesh.overview has a ping_ms for A (linked) and a machine for A only, none for C.
- The switch survives a restart of A.
- A switches off: B and C drop A's machine at once; B's mesh.overview keeps A's ping, without a machine.

Run inside: unshare -Urn sh -c "ip link set lo up; python3 sysinfonet.py ARCADED WORKDIR"
"""
import json, os, shutil, socket, subprocess, sys, time

EXE, WORK = sys.argv[1], sys.argv[2]
PORTS = {"A": (18761, 18771), "B": (18762, 18772), "C": (18763, 18773)}
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
    if not r.get("ok"):
        raise RuntimeError(f"{n} {cmd}: {r.get('error')}")
    return r["result"]


def start(n, peer=None):
    http, mesh = PORTS[n]
    args = ["nice", "-n", "19", EXE, "--home", home(n), "--http-port", str(http), "--mesh-port", str(mesh), "--stun-port", "0", "--no-seeds"]
    if peer:
        args += ["--mesh-peer", f"127.0.0.1:{PORTS[peer][1]}"]
    log = open(os.path.join(WORK, f"{n}.log"), "a")
    procs[n] = subprocess.Popen(args, stdout=log, stderr=subprocess.STDOUT)
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
            print("   ", what, ":", e)
        time.sleep(1)
    print(f"  timed out: {what}")
    return False


def row(n, node):
    return next((a for a in call(n, "mesh.overview")["arcades"] if a["node"] == node), None)


def main():
    shutil.rmtree(WORK, ignore_errors=True)
    os.makedirs(WORK)
    fails = []

    def check(name, cond):
        print(("  ok   " if cond else "  FAIL ") + name)
        if not cond:
            fails.append(name)

    start("A")
    start("B", peer="A")
    start("C", peer="B")
    ida, idb, idc = (call(n, "status")["node_id"] for n in "ABC")

    print("1. linked, and off by default")
    check("B links to A and C", wait("B's links", lambda: {ida, idc} <= {a["node"] for a in call("B", "mesh.overview")["arcades"] if a["linked"]}, 120))
    st = call("A", "sysinfo.status")
    print("    A's preview:", json.dumps(st["record"]))
    check("A: off by default", st["on"] is False)
    check("A: the preview is A's own record", st["record"]["node"] == ida and st["record"]["t"] == "si" and "sig" not in st["record"])
    text = json.dumps(st["record"]).lower()
    check("A: no host name, home path or address in it", socket.gethostname().lower() not in text and WORK.lower() not in text and "127.0.0.1" not in text)
    check("A: the record is at most 1 KB with its signature", st["bytes"] + 137 <= 1024)
    check("B: nobody shares yet", call("B", "mesh.sysinfo")["sharing"] == 0)
    check("B: ping_ms for A once linked", wait("ping", lambda: isinstance(row("B", ida)["ping_ms"], (int, float)), 60))
    r = row("B", ida)
    print(f"    B -> A: {r['ping_ms']} ms via {r['ping_via']}")
    check("B: ping_via link for A", r["ping_via"] == "link")
    check("B: no machine for A while it doesn't share", r["machine"] is None)

    print("2. A shares, C doesn't")
    st = call("A", "sysinfo.set", on=True)
    check("A: on", st["on"] is True and st["sent_at"])
    check("B: sees A's machine, 1 of 2 sharing", wait("B sysinfo", lambda: (lambda s: s["sharing"] == 1 and s["heard"] == 2 and s["arcades"][0]["node"] == ida)(call("B", "mesh.sysinfo")), 60))
    si = call("B", "mesh.sysinfo")
    print("    B's mesh.sysinfo:", json.dumps({k: si[k] for k in ("sharing", "heard", "totals")}))
    check("B: A's record as A signed it", si["arcades"][0]["sig"] == st["record"]["sig"] and si["arcades"][0]["linked"] is True)
    check("B: totals from A's record", si["totals"]["ram_gb"] == st["record"].get("ram_gb", 0))
    check("C: has A's machine too, passed on by B", wait("C sysinfo", lambda: any(a["node"] == ida for a in call("C", "mesh.sysinfo")["arcades"]), 60))
    check("B: explorer.arc_machines answers the same", call("B", "explorer.arc_machines")["sharing"] == 1)
    ra, rc = row("B", ida), row("B", idc)
    print("    B's machine for A:", json.dumps(ra["machine"]))
    check("B: mesh.overview machine for A", ra["machine"] is not None and ra["machine"]["at"] == st["record"]["at"])
    check("B: no machine for C", rc["machine"] is None)
    check("B: ping_ms for C too", wait("ping C", lambda: isinstance(row("B", idc)["ping_ms"], (int, float)), 60))
    check("C: off", call("C", "sysinfo.status")["on"] is False)

    print("3. the switch survives a restart")
    stop("A")
    start("A")
    check("A: still on after a restart", call("A", "sysinfo.status")["on"] is True)
    check("B: links to A again", wait("relink", lambda: (row("B", ida) or {}).get("linked") is True, 120))

    print("4. A switches off: dropped at once")
    call("A", "sysinfo.set", on=False)
    t = time.time()
    check("B: drops A", wait("B drops", lambda: call("B", "mesh.sysinfo")["sharing"] == 0, 15))
    print(f"    dropped in {time.time() - t:.1f} s")
    check("C: drops A", wait("C drops", lambda: not any(a["node"] == ida for a in call("C", "mesh.sysinfo")["arcades"]), 15))
    check("B: mesh.overview has no machine for A, still a ping", wait("overview", lambda: (lambda r: r["machine"] is None and isinstance(r["ping_ms"], (int, float)))(row("B", ida)), 60))
    check("A: off", call("A", "sysinfo.status")["on"] is False)

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
