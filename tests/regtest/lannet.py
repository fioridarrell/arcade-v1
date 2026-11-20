#!/usr/bin/env python3
"""Two arcades behind one router link directly at home, and through a relay otherwise (samerouter.rs, relaysvc.rs),
on a closed network: a relay R, and A and B that each link only to R.

The network has one machine, so "one router" is played: lo carries a home address (192.168.77.2) as the address the
default route leaves from, and ARCADE_TEST_PUBLIC_IP says which address an arcade's router has outside. The relay
sees A and B at 127.0.0.1, and that is what it tells each about the other.

1. A and B behind the same router (both say 127.0.0.1): they link through R, tell each other their home address, A or
   B dials the other there, and the relayed link ends ("a direct link took over").
2. A and B restart, B now behind another router (203.0.113.50): neither dials the other's home address (A tells it,
   B refuses it); they stay linked through R, and that link holds.

Run inside: unshare -Urn sh -c "ip link set lo up; ip addr add 192.168.77.2/24 dev lo; ip route add default dev lo src 192.168.77.2; python3 lannet.py ARCADED WORKDIR"
"""
import json, os, shutil, socket, subprocess, sys, time

EXE, WORK = sys.argv[1], sys.argv[2]
PORTS = {"R": (18881, 18891), "A": (18882, 18892), "B": (18883, 18893)}
HOME_IP = "192.168.77.2"
procs = {}


def home(n):
    return os.path.join(WORK, n)


def call(n, cmd, timeout=60, **p):
    p["cmd"] = cmd
    s = socket.socket(socket.AF_UNIX)
    s.settimeout(timeout)
    s.connect(os.path.join(home(n), "arcaded.sock"))
    s.sendall((json.dumps(p) + "\n").encode())
    r = json.loads(s.makefile().readline())
    s.close()
    if not r.get("ok"):
        raise RuntimeError(f"{n} {cmd}: {r.get('error')}")
    return r["result"]


def start(n, public=None, peer=None):
    http, mesh = PORTS[n]
    args = ["nice", "-n", "19", EXE, "--home", home(n), "--http-port", str(http), "--mesh-port", str(mesh), "--stun-port", "0", "--no-seeds"]
    if peer:
        args += ["--mesh-peer", f"127.0.0.1:{PORTS[peer][1]}"]
    env = dict(os.environ)
    env.pop("ARCADE_TEST_PUBLIC_IP", None)
    if public:
        env["ARCADE_TEST_PUBLIC_IP"] = public
    log = open(os.path.join(WORK, f"{n}.log"), "a")
    procs[n] = subprocess.Popen(args, stdout=log, stderr=subprocess.STDOUT, env=env)
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


def entry(a, node):
    return next((x for x in call(a, "mesh.overview")["arcades"] if x["node"] == node and x["linked"]), None)


def relayed(a):
    return call(a, "mesh.relay")["through_relays"]


def at_home(a, node):
    """A's link to `node` at the home address (several links may be listed: any at home)."""
    return any(x["node"] == node and x["linked"] and x["addr"].startswith(HOME_IP + ":") for x in call(a, "mesh.overview")["arcades"])


def mesh_events(n):
    out, cur = [], "0"
    while True:
        r = call(n, "events.since", cursor=cur, kinds="mesh", limit=1000)
        out += r.get("events", [])
        if not r.get("events") or not r.get("next") or r["next"] == cur:
            return out
        cur = r["next"]


def main():
    shutil.rmtree(WORK, ignore_errors=True)
    os.makedirs(WORK)
    fails = []

    def check(name, cond):
        print(("  ok   " if cond else "  FAIL ") + name)
        if not cond:
            fails.append(name)

    start("R")
    call("R", "mesh.relay_set", on=True)
    idr = call("R", "status")["node_id"]

    print("1. A and B behind one router")
    start("A", public="127.0.0.1", peer="R")
    start("B", public="127.0.0.1", peer="R")
    ida, idb = (call(n, "status")["node_id"] for n in "AB")
    check("A and B linked to R", wait("links to R", lambda: entry("A", idr) and entry("B", idr), 60))
    t = time.time()
    check("A links to B at home", wait("home link", lambda: at_home("A", idb) or at_home("B", ida), 150))
    print(f"    {time.time() - t:.1f} s")
    check("no relayed link left", wait("relayed link ends", lambda: not relayed("A") and not relayed("B"), 30))
    ev = [e["text"] for n in "AB" for e in mesh_events(n)]
    for line in ev:
        if "router" in line or "relayed link" in line or "took over" in line:
            print("    ", line)
    check("an arcade said the other is behind its router", any("is behind our router" in x for x in ev))
    check("A and B: still linked at home after 20 s", (time.sleep(20) or True) and (at_home("A", idb) or at_home("B", ida)))

    print("2. B behind another router")
    stop("A")
    stop("B")
    shutil.rmtree(home("A"))
    shutil.rmtree(home("B"))
    start("A", public="127.0.0.1", peer="R")
    start("B", public="203.0.113.50", peer="R")
    ida, idb = (call(n, "status")["node_id"] for n in "AB")
    check("A and B link through R", wait("relayed", lambda: bool(relayed("A")) and entry("A", idb), 150))
    time.sleep(45)
    check("A: B is linked, through R only", entry("A", idb) is not None and not at_home("A", idb) and bool(relayed("A")))
    check("B: never at A's home", not at_home("B", ida))
    ev = [e["text"] for n in "AB" for e in mesh_events(n)]
    check("nobody said the other is behind its router", not any("is behind our router" in x for x in ev))
    dials = json.dumps(call("B", "mesh.dials"))
    check("B never dialed the home address", HOME_IP not in dials)

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
