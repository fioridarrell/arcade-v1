#!/usr/bin/env python3
"""Fail the release if a published hash or a required signature does not match.

The release job runs this after the artifacts are downloaded and, when the
signing secrets are set, after Linux signatures are written. Hash checks always
run. Authenticode runs when WINDOWS_SIGNING_REQUIRED is true. A detached
minisign or GPG signature is required when its own flag is true.

Those flags are set from the CI secrets. When a secret is empty the matching
check prints why it was skipped and does not fail the release.
"""

from __future__ import annotations

import hashlib
import os
import re
import shutil
import subprocess
import sys
import tempfile
import zipfile
from pathlib import Path

sys.path.insert(0, str(Path(__file__).resolve().parent))
import payload_trailer  # noqa: E402

WINDOWS = "arcade-v1-windows-x86_64"
LINUX = ("arcade-v1-linux-x86_64.tar.gz", "arcade-v1-linux-aarch64.tar.gz")
WIN_BINARIES = ("arcaded.exe", "arcade.exe", "arcade-desktop.exe", "Install.exe")


def env_flag(name: str) -> bool:
    return os.environ.get(name, "").strip().lower() in {"1", "true", "yes"}


def sha256_file(path: Path) -> str:
    digest = hashlib.sha256()
    with path.open("rb") as fh:
        for chunk in iter(lambda: fh.read(1024 * 1024), b""):
            digest.update(chunk)
    return digest.hexdigest()


def parse_line(line: str) -> tuple[str, str] | None:
    line = line.strip()
    if not line or line.startswith("#"):
        return None
    match = re.match(r"^([0-9a-fA-F]{64})\s+\*?(\S.*)$", line)
    if not match:
        return ("", line)
    return match.group(1).lower(), match.group(2).strip()


def add(errors: list[str], message: str) -> None:
    errors.append(message)


def check_hash(label: str, actual: str, expected: str, errors: list[str]) -> None:
    if actual != expected:
        add(errors, f"{label} sha256 mismatch: file {actual}, published {expected}")


def member_bytes(zip_path: Path, base: str) -> bytes | None:
    with zipfile.ZipFile(zip_path) as archive:
        for info in archive.infolist():
            if info.is_dir():
                continue
            name = info.filename.replace("\\", "/").rsplit("/", 1)[-1]
            if name == base:
                return archive.read(info)
    return None


def check_payload(setup: Path, published: str, errors: list[str]) -> None:
    if not setup.is_file():
        add(errors, f"missing {setup.name}, so the embedded payload hash cannot be checked")
        return
    image = setup.read_bytes()
    try:
        embedded = payload_trailer.embedded_hash(image)
        _start, payload = payload_trailer.split_payload(image)
    except payload_trailer.PayloadError as err:
        add(errors, f"{setup.name}: {err}")
        return
    actual = hashlib.sha256(payload).hexdigest()
    if actual != published or embedded != published:
        add(
            errors,
            f"{setup.name} payload SHA-256 is {actual}, embedded hash is {embedded}, "
            f"published hash is {published}",
        )


def check_named(root: Path, name: str, digest: str, errors: list[str]) -> None:
    if name.endswith("-payload"):
        setup_name = name[: -len("-payload")] + "-setup.exe"
        check_payload(root / setup_name, digest, errors)
        return
    direct = root / name
    if direct.is_file():
        check_hash(name, sha256_file(direct), digest, errors)
        return
    found = False
    for zip_path in sorted(root.glob("arcade-v1-windows-*.zip")):
        data = member_bytes(zip_path, name)
        if data is None:
            continue
        found = True
        check_hash(f"{zip_path.name}:{name}", hashlib.sha256(data).hexdigest(), digest, errors)
    if not found:
        add(errors, f"no file for published hash of {name}")


def check_sha256_files(root: Path, errors: list[str]) -> None:
    files = sorted(root.glob("*.sha256"))
    if not files:
        add(errors, "no .sha256 files were published")
        return
    for sidecar in files:
        text = sidecar.read_text(encoding="utf-8", errors="replace")
        parsed = [parse_line(line) for line in text.splitlines()]
        parsed = [item for item in parsed if item is not None]
        if not parsed:
            add(errors, f"{sidecar.name} has no hashes")
            continue
        for item in parsed:
            digest, name = item
            if not digest:
                add(errors, f"{sidecar.name} has an unreadable line: {name}")
                continue
            check_named(root, name, digest, errors)


def authenticode_status(path: Path) -> str:
    tool = shutil.which("osslsigncode")
    if tool is None:
        return "osslsigncode-missing"
    proc = subprocess.run(
        [tool, "verify", "-in", str(path)],
        capture_output=True,
        text=True,
    )
    text = proc.stdout + "\n" + proc.stderr
    if "No signature found" in text or "Unable to extract existing signature" in text:
        return "unsigned"
    current = re.search(r"Current message digest\s*:\s*([0-9A-Fa-f]+)", text)
    calculated = re.search(r"Calculated message digest\s*:\s*([0-9A-Fa-f]+)", text)
    if current and calculated:
        if current.group(1).lower() == calculated.group(1).lower():
            return "signed"
        return "broken"
    if proc.returncode == 0:
        return "signed"
    return "broken"


def check_authenticode(root: Path, errors: list[str]) -> None:
    if not env_flag("WINDOWS_SIGNING_REQUIRED"):
        print(
            "Authenticode check skipped: the CI secret WINDOWS_CODESIGN_PFX is not set, "
            "so unsigned Windows binaries are allowed. Hash checks still run. "
            "Martin needs to put a code-signing certificate in that secret before "
            "Install.exe, setup.exe, arcaded.exe, arcade.exe and arcade-desktop.exe are signed."
        )
        return
    setup = root / f"{WINDOWS}-setup.exe"
    targets: list[Path] = []
    if setup.is_file():
        targets.append(setup)
    else:
        add(errors, f"missing {setup.name}")
    zip_path = root / f"{WINDOWS}.zip"
    if not zip_path.is_file():
        add(errors, f"missing {zip_path.name}")
    else:
        with tempfile.TemporaryDirectory() as tmp:
            folder = Path(tmp)
            for name in WIN_BINARIES:
                data = member_bytes(zip_path, name)
                if data is None:
                    add(errors, f"{zip_path.name} does not contain {name}")
                    continue
                path = folder / name
                path.write_bytes(data)
                targets.append(path)
            for path in targets:
                status = authenticode_status(path)
                if status != "signed":
                    add(errors, f"{path.name} is not Authenticode-signed ({status})")
            return
    for path in targets:
        status = authenticode_status(path)
        if status != "signed":
            add(errors, f"{path.name} is not Authenticode-signed ({status})")


def check_minisign(root: Path, errors: list[str]) -> None:
    tars = [root / name for name in LINUX if (root / name).is_file()]
    required = env_flag("LINUX_MINISIGN_REQUIRED")
    pub = root / "arcade-v1-minisign.pub"
    signed = [tar for tar in tars if Path(str(tar) + ".minisig").is_file()]
    if not required and not signed:
        print(
            "minisign check skipped: LINUX_MINISIGN_SECRET_KEY is not set. "
            "The .sha256 files are still checked. Martin needs to supply an unencrypted "
            "minisign secret key (minisign -G -W) before releases carry a detached signature."
        )
        return
    if shutil.which("minisign") is None:
        add(errors, "minisign is not installed, so the detached signature cannot be checked")
        return
    if not pub.is_file():
        add(errors, "arcade-v1-minisign.pub was not published with the detached signatures")
        return
    expected = tars if required else signed
    for tar in expected:
        sig = Path(str(tar) + ".minisig")
        if not sig.is_file():
            add(errors, f"missing detached minisign signature {sig.name}")
            continue
        proc = subprocess.run(
            ["minisign", "-V", "-p", str(pub), "-x", str(sig), "-m", str(tar)],
            capture_output=True,
            text=True,
        )
        if proc.returncode != 0:
            detail = (proc.stderr or proc.stdout).strip()
            add(errors, f"minisign rejected {tar.name}: {detail}")


def check_gpg(root: Path, errors: list[str]) -> None:
    tars = [root / name for name in LINUX if (root / name).is_file()]
    required = env_flag("LINUX_GPG_REQUIRED")
    pub = root / "arcade-v1-gpg.pub.asc"
    signed = [tar for tar in tars if Path(str(tar) + ".asc").is_file()]
    if not required and not signed:
        print(
            "GPG check skipped: LINUX_GPG_PRIVATE_KEY is not set. "
            "The .sha256 files are still checked. Martin needs to supply an ASCII-armored "
            "GPG private key before releases carry a detached GPG signature."
        )
        return
    if shutil.which("gpg") is None:
        add(errors, "gpg is not installed, so the detached signature cannot be checked")
        return
    if not pub.is_file():
        add(errors, "arcade-v1-gpg.pub.asc was not published with the detached signatures")
        return
    home = tempfile.mkdtemp(prefix="arcade-gpg-")
    os.chmod(home, 0o700)
    env = dict(os.environ)
    env["GNUPGHOME"] = home
    try:
        imported = subprocess.run(
            ["gpg", "--batch", "--import", str(pub)],
            capture_output=True,
            text=True,
            env=env,
        )
        if imported.returncode != 0:
            add(errors, f"could not import arcade-v1-gpg.pub.asc: {imported.stderr.strip()}")
            return
        expected = tars if required else signed
        for tar in expected:
            sig = Path(str(tar) + ".asc")
            if not sig.is_file():
                add(errors, f"missing detached GPG signature {sig.name}")
                continue
            proc = subprocess.run(
                ["gpg", "--batch", "--verify", str(sig), str(tar)],
                capture_output=True,
                text=True,
                env=env,
            )
            if proc.returncode != 0:
                detail = (proc.stderr or proc.stdout).strip()
                add(errors, f"gpg rejected {tar.name}: {detail}")
    finally:
        shutil.rmtree(home, ignore_errors=True)


def require_present(root: Path, names: list[str], errors: list[str]) -> None:
    for name in names:
        if not (root / name).is_file():
            add(errors, f"missing {name}")


def main(argv: list[str]) -> int:
    root = Path(argv[0]).resolve() if argv else Path(".").resolve()
    errors: list[str] = []
    require_present(
        root,
        [
            f"{WINDOWS}-setup.exe",
            f"{WINDOWS}-setup.exe.sha256",
            f"{WINDOWS}.zip",
            f"{WINDOWS}.zip.sha256",
            f"{WINDOWS}-binaries.sha256",
            f"{WINDOWS}-payload.sha256",
            *LINUX,
            *(name + ".sha256" for name in LINUX),
        ],
        errors,
    )
    for tar in sorted(root.glob("arcade-v1-macos-*.tar.gz")):
        if not Path(str(tar) + ".sha256").is_file():
            add(errors, f"missing {tar.name}.sha256")
    check_sha256_files(root, errors)
    check_authenticode(root, errors)
    check_minisign(root, errors)
    check_gpg(root, errors)
    if errors:
        print("refusing to publish:", file=sys.stderr)
        for message in errors:
            print(f"  {message}", file=sys.stderr)
        return 1
    print(f"release assets checked in {root}")
    return 0


if __name__ == "__main__":
    sys.exit(main(sys.argv[1:]))
