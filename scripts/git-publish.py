#!/usr/bin/env python3
"""Push a git repository onto the ARCADE chain as blobs (crates/arcaded/src/arcgit.rs).

  git-publish.py --sock <arcade home>/arcaded.sock --repo DIR [--ref main] [--name arcade-v1] [--prev MANIFEST_ID]
                 [--to @owner/name] [--as @tag] [--force]

Packs every object reachable from --ref that the manifest --prev doesn't reach yet (git pack-objects, pieces under
3 MB so a pack and its index fit one block's 4 MB of blobs), publishes each pack and its index from the arcade's
wallet (one transaction each, Permanent), then a manifest blob naming the refs and every pack so far. Prints the
manifest's id: `git clone http://<arcade>/git/<id>` clones it from any arcade.

With --to, the push goes to a repository under an @tag (§15 step 9): it carries on from the repository's manifest now
(unless --prev says otherwise), and once the new manifest is in a block a signed push record points the repository at
it, so `git clone http://<arcade>/git/@owner/name` follows it. Only the owner and its maintainers can push.
The push record names the manifest it was built on (`base`: the one read here, "" for a new repository); from ARCADE
rules version 4 the ledger refuses it if the repository has moved on since, unless --force (replace whatever is there).

With --as, everything is signed and paid by the key that holds that @tag on the ARCADE chain: the arcade wallet's own,
or one imported for ARCADE (wallet.import), such as a publisher's key.
"""
import argparse, glob, json, os, socket, subprocess, sys, tempfile, time


def call(sock, req, t=300):
    s = socket.socket(socket.AF_UNIX); s.settimeout(t); s.connect(sock)
    s.sendall((json.dumps(req) + "\n").encode())
    r = json.loads(s.makefile().readline())
    if not r.get("ok"):
        raise RuntimeError(r.get("error"))
    return r["result"]


AS = {}


def publish(sock, files):
    """Publish files in one transaction, waiting while the wallet's coins are tied up in transactions not yet in a block."""
    for _ in range(360):
        try:
            return call(sock, {"cmd": "arc.blob_publish_many", "files": files, "send": True, **AS})
        except RuntimeError as e:
            if "not enough ARCADE" not in str(e):
                raise
            time.sleep(10)
    raise SystemExit("the wallet never had enough ARCADE")


def main():
    ap = argparse.ArgumentParser()
    ap.add_argument("--sock", required=True); ap.add_argument("--repo", required=True)
    ap.add_argument("--ref", default="main"); ap.add_argument("--name", default="")
    ap.add_argument("--prev", default=""); ap.add_argument("--to", default=""); ap.add_argument("--as", dest="as_tag", default="")
    ap.add_argument("--force", action="store_true", help="replace the repository's manifest even if it moved on since --prev was read")
    a = ap.parse_args()
    if a.as_tag:
        AS["as"] = a.as_tag
    if a.to and not a.prev:
        try:
            a.prev = call(a.sock, {"cmd": "ledger.repo", "name": a.to})["manifest"]
        except RuntimeError as e:
            if "no repository" not in str(e):
                raise
    if a.to and not a.name:
        a.name = a.to.lstrip("@")
    git = lambda *x, **k: subprocess.run(["git", "-C", a.repo, *x], check=True, capture_output=True, **k)
    head = git("rev-parse", f"{a.ref}^{{commit}}").stdout.decode().strip()
    prev = {"refs": {}, "packs": []}
    if a.prev:
        prev = json.loads(bytes.fromhex(call(a.sock, {"cmd": "arc.blob", "id": a.prev})["hex"]))
    have = [sha for sha in prev["refs"].values()]
    with tempfile.TemporaryDirectory() as tmp:
        revs = "\n".join([head] + [f"^{h}" for h in have]) + "\n"
        git("pack-objects", "--revs", "--max-pack-size=3m", "-q", os.path.join(tmp, "pack"), input=revs.encode())
        packs = []
        for pack in sorted(glob.glob(os.path.join(tmp, "pack-*.pack"))):
            name = os.path.basename(pack)[len("pack-"):-len(".pack")]
            idx = pack[:-len(".pack")] + ".idx"
            r = publish(a.sock, [{"path": pack, "type": "application/x-git-packed-objects", "class": 0},
                                 {"path": idx, "type": "application/x-git-packed-objects-toc", "class": 0}])
            packs.append({"name": name, "pack": r["blobs"][0]["blob"], "idx": r["blobs"][1]["blob"]})
            print(f"pack {name}: {os.path.getsize(pack)} bytes, fee {r['fee'] / 1e8} ARCADE", file=sys.stderr)
        manifest = {"git": "manifest", "name": a.name or os.path.basename(os.path.abspath(a.repo)), "head": f"refs/heads/{a.ref}",
                    "refs": {**prev["refs"], f"refs/heads/{a.ref}": head}, "packs": prev["packs"] + packs, "prev": a.prev or None,
                    "time": int(time.time())}
        mpath = os.path.join(tmp, "manifest.json")
        json.dump(manifest, open(mpath, "w"), separators=(",", ":"))
        r = publish(a.sock, [{"path": mpath, "type": "application/json", "class": 0}])
    out = {"manifest": r["blobs"][0]["blob"], "head": head, "new_packs": len(packs), "packs": len(manifest["packs"])}
    if a.to:
        for _ in range(360):
            if call(a.sock, {"cmd": "arc.blob", "id": out["manifest"]})["on_chain"]:
                break
            time.sleep(5)
        for _ in range(360):
            try:
                out["push"] = call(a.sock, {"cmd": "ledger.repo_push", "name": a.to, "manifest": out["manifest"],
                                          "base": a.prev, "force": a.force, **AS}); break
            except RuntimeError as e:
                if "not enough" not in str(e) and "could not build" not in str(e):
                    raise
                time.sleep(10)
    print(json.dumps(out))


main()
