#!/usr/bin/env python3
"""Make a release of the arcade's own code for every arcade to keep (crates/arcaded/src/arccode.rs).

  code-publish.py --new-key KEYFILE                       make a release key (prints its public key)
  code-publish.py --repo DIR --ref main --version v1.0.0 --key KEYFILE --out DIR \\
                  [--name @arcade/arcade-v1] --program PLATFORM=ARCHIVE ... [--allow-leaks] [--source-only]

A release carries the program archives the GitHub release workflow (.github/workflows/release.yml) built from the
same commit, one per system, so every arcade (Linux, Windows, and later macOS) updates to the same code:

  gh release download vX.Y.Z -R martinseeger2002/arcade-v1 -p 'arcade-v1-*' -D rel
  code-publish.py --repo . --ref vX.Y.Z --version vX.Y.Z --key KEYFILE --out out \\
      --program linux-x86_64=rel/arcade-v1-linux-x86_64.tar.gz \\
      --program linux-aarch64=rel/arcade-v1-linux-aarch64.tar.gz \\
      --program windows-x86_64=rel/arcade-v1-windows-x86_64.zip \\
      [--program macos-aarch64=rel/arcade-v1-macos-aarch64.tar.gz --program macos-x86_64=rel/arcade-v1-macos-x86_64.tar.gz]

It refuses unless every archive's COMMIT file is the release's commit (--ref) and every required platform
(linux-x86_64, linux-aarch64, windows-x86_64) is given; an ARCHIVE.sha256 beside an archive is checked too. Without
program-windows-x86_64 a Windows arcade finds no update in the release (issue #187). --source-only (test networks)
publishes the code with no programs at all.

It writes into --out: arcade-v1.bundle (the repo's history up to the commit: `git clone arcade-v1.bundle`),
vendor.tar.zst (every dependency, so `cargo build --offline` works: see BUILD.txt), program-<platform> files, and
release.json ({record, sig, files}), which `code.add` on an arcade takes as it is.

Everything published is public forever, so the whole history up to the commit is scanned for things that must never be
published (private keys, wallet phrases, home paths, private addresses) first; any hit stops it unless --allow-leaks.
"""
import argparse, hashlib, json, os, re, subprocess, sys, tarfile, tempfile, time, zipfile

def sh(*a, cwd=None, capture=True):
    r = subprocess.run(a, cwd=cwd, capture_output=capture, text=capture)
    if r.returncode != 0:
        sys.exit(f"{' '.join(a)}: {r.stderr if capture else ''}")
    return r.stdout.strip() if capture else None

def sha256(path):
    h = hashlib.sha256()
    with open(path, "rb") as f:
        for b in iter(lambda: f.read(1 << 20), b""):
            h.update(b)
    return h.hexdigest()

LEAKS = [
    (r"-----BEGIN [A-Z ]*PRIVATE KEY-----", "a private key"),
    (r"\b[5KL][1-9A-HJ-NP-Za-km-z]{50,51}\b", "something shaped like a WIF private key"),
    (r"/home/[a-z][a-z0-9_-]*/", "a home folder path"),
    (r"\b192\.168\.\d{1,3}\.\d{1,3}\b", "a private network address"),
    (r"\b(xprv|tprv)[1-9A-HJ-NP-Za-km-z]{100,}", "an extended private key"),
]

# The systems a release carries a program archive for (`program-<platform>`, crates/arcaded/src/updater.rs asset_for).
REQUIRED_PLATFORMS = ["linux-x86_64", "linux-aarch64", "windows-x86_64"]
OPTIONAL_PLATFORMS = ["macos-aarch64", "macos-x86_64"]

def archive_name(plat):
    return f"arcade-v1-{plat}.zip" if plat.startswith("windows-") else f"arcade-v1-{plat}.tar.gz"

def archive_commit(path, plat):
    """The COMMIT file inside a release archive (arcade-v1-<platform>/COMMIT), or None."""
    want = f"arcade-v1-{plat}/COMMIT"
    try:
        if zipfile.is_zipfile(path):
            with zipfile.ZipFile(path) as z:
                for i in z.infolist():
                    # Compress-Archive may write Windows separators.
                    if i.filename.replace("\\", "/").lstrip("./") == want:
                        return z.read(i).decode("ascii", "replace").strip()
        else:
            with tarfile.open(path, "r:*") as t:
                for m in t:
                    if m.isfile() and m.name.lstrip("./") == want:
                        return t.extractfile(m).read().decode("ascii", "replace").strip()
    except (OSError, tarfile.TarError, zipfile.BadZipFile) as e:
        sys.exit(f"--program {plat}: {path} is not a readable release archive ({e})")
    return None

def check_programs(programs, commit, source_only):
    """Refuse a release whose programs are not all built from `commit`, or that leaves a required system out."""
    seen = {}
    for p in programs:
        if "=" not in p:
            sys.exit(f"--program {p}: give PLATFORM=ARCHIVE")
        plat, path = p.split("=", 1)
        if plat not in REQUIRED_PLATFORMS + OPTIONAL_PLATFORMS:
            sys.exit(f"--program {plat}: unknown platform (one of {', '.join(REQUIRED_PLATFORMS + OPTIONAL_PLATFORMS)})")
        if plat in seen:
            sys.exit(f"--program {plat} given twice")
        if os.path.basename(path) != archive_name(plat):
            sys.exit(f"--program {plat}: expected {archive_name(plat)}, got {os.path.basename(path)}")
        if not os.path.isfile(path):
            sys.exit(f"--program {plat}: {path} not found")
        if os.path.isfile(path + ".sha256"):
            want = open(path + ".sha256").read().split()[0].lower()
            if sha256(path) != want:
                sys.exit(f"--program {plat}: {path} does not match {path}.sha256")
        c = archive_commit(path, plat)
        if c != commit:
            sys.exit(f"--program {plat}: {path} was built from {c or 'an unknown commit (no COMMIT file)'}, not {commit}")
        seen[plat] = path
    if source_only:
        if seen:
            sys.exit("--source-only publishes no programs; leave out --program")
        return seen
    missing = [p for p in REQUIRED_PLATFORMS if p not in seen]
    if missing:
        sys.exit(f"missing --program for {', '.join(missing)}: every release carries every required system, built from "
                 f"{commit} (--source-only for a release with no programs)")
    return seen

def leak_scan(repo, ref):
    """Every line ever added in the history up to `ref` (what a bundle of it carries)."""
    out = subprocess.run(["git", "-C", repo, "log", "-p", "--no-color", "--format=commit %H", ref], capture_output=True, text=True, errors="replace").stdout
    hits, commit = [], "?"
    for line in out.splitlines():
        if line.startswith("commit "):
            commit = line[7:19]
        elif line.startswith("+") and not line.startswith("+++"):
            for pat, what in LEAKS:
                if re.search(pat, line):
                    hits.append((commit, what, line[1:120]))
    return hits

def main():
    ap = argparse.ArgumentParser()
    ap.add_argument("--new-key")
    ap.add_argument("--repo"); ap.add_argument("--ref", default="main"); ap.add_argument("--version")
    ap.add_argument("--name", default="@arcade/arcade-v1"); ap.add_argument("--key"); ap.add_argument("--out")
    ap.add_argument("--program", action="append", default=[]); ap.add_argument("--allow-leaks", action="store_true")
    ap.add_argument("--no-vendor", action="store_true", help="tests only: leave the dependencies out")
    ap.add_argument("--source-only", action="store_true", help="test networks: the code with no program archives")
    a = ap.parse_args()
    from cryptography.hazmat.primitives.asymmetric.ed25519 import Ed25519PrivateKey
    from cryptography.hazmat.primitives import serialization as ser
    if a.new_key:
        k = Ed25519PrivateKey.generate()
        seed = k.private_bytes(ser.Encoding.Raw, ser.PrivateFormat.Raw, ser.NoEncryption())
        fd = os.open(a.new_key, os.O_WRONLY | os.O_CREAT | os.O_EXCL, 0o600)
        os.write(fd, seed.hex().encode()); os.close(fd)
        print(k.public_key().public_bytes(ser.Encoding.Raw, ser.PublicFormat.Raw).hex()); return
    for need in ("repo", "version", "key", "out"):
        if not getattr(a, need): sys.exit(f"--{need} is needed")
    commit = sh("git", "-C", a.repo, "rev-parse", f"{a.ref}^{{commit}}")
    programs = check_programs(a.program, commit, a.source_only)
    hits = leak_scan(a.repo, commit)
    if hits:
        for c, what, line in hits[:40]: print(f"LEAK? {c} {what}: {line}", file=sys.stderr)
        print(f"{len(hits)} possible leaks in the history up to {commit[:12]}", file=sys.stderr)
        if not a.allow_leaks: sys.exit("stopped: everything published is public forever (--allow-leaks to go on anyway)")
    os.makedirs(a.out, exist_ok=True)
    files = {}
    # The commit as branch main with HEAD on it, in a bare repo of its own, so `git clone arcade-v1.bundle` checks it out
    # (a bundle of a remote-tracking ref clones empty).
    bundle = os.path.join(a.out, "arcade-v1.bundle")
    with tempfile.TemporaryDirectory() as bare:
        sh("git", "init", "-q", "--bare", bare)
        sh("git", "-C", a.repo, "push", "-q", bare, f"{commit}:refs/heads/main")
        sh("git", "-C", bare, "symbolic-ref", "HEAD", "refs/heads/main")
        sh("git", "-C", bare, "bundle", "create", os.path.abspath(bundle), "HEAD", "main")
    files["arcade-v1.bundle"] = bundle
    if not a.no_vendor:
        with tempfile.TemporaryDirectory() as tmp:
            src = os.path.join(tmp, "src")
            os.makedirs(src)
            subprocess.run(f"git -C '{a.repo}' archive {commit} | tar -x -C '{src}'", shell=True, check=True)
            # Its own folder name: the repo keeps a vendor/ of its own (a patched crate), which must stay as it is.
            sh("cargo", "vendor", "--offline", "--versioned-dirs", os.path.join(tmp, "cargo-deps"), cwd=src)
            vend = os.path.join(a.out, "vendor.tar.zst")
            subprocess.run(f"tar --sort=name --mtime=@0 --owner=0 --group=0 --numeric-owner -C '{tmp}' -cf - cargo-deps | zstd -19 -T0 -q -f -o '{vend}'", shell=True, check=True)
            files["vendor.tar.zst"] = vend
    for plat, path in programs.items():
        files[f"program-{plat}"] = path
    build = os.path.join(a.out, "BUILD.txt")
    with open(build, "w") as f:
        f.write(f"""Rebuild {a.name} {a.version} (commit {commit}) with no internet:
  git clone arcade-v1.bundle arcade-v1 && cd arcade-v1 && git checkout {commit}
  tar --zstd -xf ../vendor.tar.zst
  mkdir -p .cargo && printf '[source.crates-io]\\nreplace-with = "vendored-sources"\\n[source.vendored-sources]\\ndirectory = "cargo-deps"\\n' >> .cargo/config.toml
  cargo build --release --offline
""")
    files["BUILD.txt"] = build
    record = json.dumps({"name": a.name, "version": a.version, "commit": commit, "time": int(time.time()),
                         "files": {n: {"sha256": sha256(p), "bytes": os.path.getsize(p)} for n, p in sorted(files.items())}},
                        sort_keys=True, separators=(",", ":"))
    rid = hashlib.sha256(record.encode()).hexdigest()
    seed = bytes.fromhex(open(a.key).read().strip())
    sig = Ed25519PrivateKey.from_private_bytes(seed).sign(b"ARCADE code release\0" + rid.encode()).hex()
    out = {"record": record, "sig": sig, "files": {n: os.path.abspath(p) for n, p in files.items()}}
    json.dump(out, open(os.path.join(a.out, "release.json"), "w"))
    print(json.dumps({"id": rid, "commit": commit, "files": {n: os.path.getsize(p) for n, p in files.items()}}))

main()
