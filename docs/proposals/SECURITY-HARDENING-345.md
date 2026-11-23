# Security hardening proposal (issue #345)


## Description
Issue #345 lists five risks in how Arcade V1 is installed and run. Some are already handled in the code: the API binds to 127.0.0.1 by default (`--http-bind`, main.rs), and the updater only accepts releases signed by the release key, with every file hash-checked and the old binaries kept as `.prev` (updater.rs). This PR closes the gaps that remain and makes the safe behavior something you can enforce and test, not just a default.

This is a proposal for discussion. Each part can land as its own PR if that's easier to review.

## Proposed changes

### 1. Signed Windows installer as the main install path (replaces the remote PowerShell one-liner)
- Make `arcade-v1-windows-x86_64-setup.exe` the install method the README and install page lead with. Move the `iex (DownloadString ...)` one-liner to an "advanced" section, or remove it.
- Authenticode-sign `Install.exe`, `setup.exe`, `arcaded.exe`, `arcade.exe` and `arcade-desktop.exe` in CI, using a code-signing certificate kept in a protected CI secret or a hardware or cloud HSM.
- Ship the release's SHA-256 inside the signed installer, and have the installer verify the payload before it extracts anything. If the check fails, nothing is installed and the error says why.
- Linux: do the same with a signed `.deb` or tarball plus a `.sha256` and a detached signature (minisign or GPG), and lead the docs with download-verify-run instead of `curl | bash`.
- CI check: the release job fails if any Windows binary is unsigned or doesn't match its published hash.

### 2. Wallet signing moved out of the always-on service
- New small process, `arcade-signer` (built on the existing `arcade-wallet` crate), that alone holds the decrypted seed. arcaded keeps only public keys and addresses.
- arcaded builds unsigned transactions and sends signing requests to `arcade-signer` over a local pipe or socket that only the current user can open.
- The signer starts locked. It unlocks only after the user presses Approve in the desktop window (with optional password or Windows Hello), signs only the exact transaction shown on that card, and locks again after a short timeout (default 5 minutes) or right after signing.
- Nothing on the mesh or HTTP side can reach the signer directly. The seed file stays readable by the owner only (700 or 600 on Linux, a user-only ACL on Windows).
- Tests: arcaded can't sign while the signer is locked; a changed transaction (amount, address or fee different from the approved card) is refused.

### 3. Mesh self-update: signed releases only, off by default, enforced
- Keep auto-update off on a fresh install. Show it as an explicit opt-in in Options, with a sentence about what it does.
- Fail closed: unsigned, wrong-key, hash-mismatched, or version-mismatched releases are rejected and logged, and never staged. There's no setting or flag that skips the signature check.
- Rollback protection: never install a version lower than the running one.
- Document the release-key procedure: where the key is kept, who can sign, and how a key would be rotated or revoked if it's ever compromised.
- Tests for each rejection case.
- Docs fix: the #345 reply says Windows never updates itself, but Windows builds do update over the mesh with the same signed-release checks. The docs should say that accurately.

### 4. API listens on localhost only
- 127.0.0.1 is already the default. Additionally:
  - refuse a non-loopback `--http-bind` unless `--i-understand-remote-api` is also given, and print a warning at startup when it is;
  - check the Host header, so a DNS-rebinding page in a browser can't drive the local API;
  - add a test that a default start listens only on loopback.
- Mesh (8421) and STUN (3478) stay public, but document exactly what they serve (blocks and signed records only, no wallet or API).

### 5. Clear security warning on the install page and in the installer
- Add a prominent notice at the top of the install page and README, and as a screen in the Windows installer that the user must click through:
  > **Arcade V1 is beta software with no independent security audit yet. Do not use funds you can't afford to lose. Do not restore a seed phrase that holds real savings. It installs a background service that starts at login and connects to other arcades.**
- Link to a SECURITY.md that covers what the service can access, the ports it opens, how updates are checked, and how to report security problems (team@arcade-nodes.com).

## Not in this PR
- An independent security audit. That's recommended, but it's outside what a PR can do.

## Testing plan
- Fresh Windows 11 install from the signed setup.exe; tampered payload refused.
- Release-mode `cargo test` passes, plus new tests for the signer lock, update rejections and the loopback bind.
- An existing install upgrades without losing its data folder or wallet.

Refs #345
