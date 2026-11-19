#!/usr/bin/env python3
"""A dropped link is dialed again, and svc.ask dials an arcade's known address (meshsvc.rs link_dropped, known_dials;
svcmesh.rs reach), on a closed network: two arcades, B dials A, B answers a service "echo".

- B linked to A: A's svc.ask to B answers.
- B restarts without --mesh-peer (so B doesn't dial A; only A can link them again), down past A's first redial: A's
  svc.ask to B answers within 10 s of B being up, A not restarted. Before the fix A never dialed B's address: "can't reach that arcade yet".
- B restarts again, nobody asks: A links to B again by itself (the dropped link is redialed at 5 s, 30 s, 2 min).

Run inside: unshare -Urn sh -c "ip link set lo up; python3 reachnet.py ARCADED WORKDIR"
"""
import json, os, shutil, socket, subprocess, sys, threading, time

EXE, WORK = sys.argv[1], sys.argv[2]
PORTS = {"A": (18781, 18791), "B": (18782, 18792)}
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


def echo_helper(n):
    """Attach "echo" on arcade n: every request is answered with its body. Runs until n stops."""
    s = socket.socket(socket.AF_UNIX)
    s.connect(os.path.join(home(n), "arcaded.sock"))
    s.sendall((json.dumps({"cmd": "svc.attach", "service": "echo"}) + "\n").encode())
    f = s.makefile("rw")
    first = json.loads(f.readline())
    if not first.get("ok"):
        raise RuntimeError(f"attach: {first}")

    def serve():
        try:
            for line in f:
                m = json.loads(line)
                if "in" in m:
                    f.write(json.dumps({"out": {"id": m["in"]["id"], "ev": {"echo": m["in"]["body"]}, "end": True}}) + "\n")
                    f.flush()
        except Exception:
            pass

    threading.Thread(target=serve, daemon=True).start()


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


def linked(a, node):
    return any(x["node"] == node and x["linked"] for x in call(a, "mesh.overview")["arcades"])


def ask(idb, word):
    r = call("A", "svc.ask", timeout=90, to=idb, service="echo", body={"w": word})
    return json.dumps(r)


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
    echo_helper("B")
    ida, idb = (call(n, "status")["node_id"] for n in "AB")

    print("1. B linked to A")
    check("A has B's link", wait("A's link", lambda: linked("A", idb), 120))
    r = ask(idb, "one")
    print("    A asks B:", r)
    check("A: svc.ask to B answers", '"one"' in r)

    print("2. B restarts (and dials nobody): A's ask reaches it")
    stop("B")
    check("A: B's link is down", wait("down", lambda: not linked("A", idb), 30))
    # Past A's first redial of the dropped link (5 s, refused: B is down); its next is at about 30 s.
    time.sleep(10)
    start("B")
    echo_helper("B")
    t = time.time()
    check("A: not linked to B again yet (the ask must do it)", not linked("A", idb))
    try:
        r = ask(idb, "two")
    except Exception as e:
        r = str(e)
    took = time.time() - t
    print(f"    A asks B after its restart: {r} ({took:.1f} s)")
    check("A: svc.ask to B answers after B restarted", '"two"' in r)
    check("A: within 10 s of B being up", took <= 10)
    dials = call("A", "mesh.dials")
    print("    A's mesh.dials:", json.dumps(dials)[:400])

    print("3. B restarts again, nobody asks: A links to it by itself")
    stop("B")
    check("A: B's link is down", wait("down", lambda: not linked("A", idb), 30))
    start("B")
    t = time.time()
    check("A: links to B again", wait("relink", lambda: linked("A", idb), 60))
    print(f"    linked again {time.time() - t:.1f} s after B was up")
    check("B: linked to A (the link came from A)", wait("B's link", lambda: linked("B", ida), 5))

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
