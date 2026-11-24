# The release key

Arcade V1 installs a mesh update only when that release is signed by a key pinned in the binary. This page is how that key is kept, who can sign with it, and what to do if it is stolen or needs to be replaced. The check itself is `crates/arcaded/src/arccode.rs` (`RELEASE_KEYS`, `Release::verify`) and `crates/arcaded/src/updater.rs`. Nothing in the program turns the check off: there is no flag, setting, or environment variable that skips it. `--code-key` adds one extra verifying key on a closed test network. It does not accept an unsigned release.

## What is pinned

One ed25519 verifying key, hex:

```
9c955cd5c945d72775f2fa8592b13f5098285d1205edd0be2cca11591b2a008a
```

It is the constant `RELEASE_KEYS` in `crates/arcaded/src/arccode.rs`. Martin named it on 2026-11-14: Strawberry holds the release signing key, the same public key the original arcade already pinned. It signs code releases and nothing else. The private half is not in this repository, not in the GitHub release workflow, and not in the binary.

A signature is ed25519 over the bytes `ARCADE code release` followed by a zero byte, followed by the hex SHA-256 of the release record's exact bytes (`scripts/code-publish.py` builds that record). The record names `@arcade/arcade-v1`, the version, the git commit, and each file's SHA-256.

## Who can sign

Whoever holds the private half of that key. Today that is Strawberry's release key. Signing is a separate step from the GitHub build: `.github/workflows/release.yml` builds the archives, and `scripts/code-publish.py --key KEYFILE` signs the record.

Make a key file (32 raw bytes, hex, mode 600, created only if the path does not exist yet):

```sh
python3 scripts/code-publish.py --new-key /path/outside/the/repo/release.key
```

The command prints the public key. Keep the file where the repo is not, and where CI cannot read it. Pass it only as `--key` when publishing. Do not commit it, and do not put it in a workflow secret that the release job can use to sign on its own: the person who holds the key signs, after the archives exist.

Publishing checks that every required archive (Linux x86_64, Linux aarch64, Windows x86_64) was built from the same commit, then writes `release.json` (`record`, `sig`, and the files). An arcade takes that in with `code.add`. The mesh then carries the record. An arcade with automatic updates on installs it only after checking the signature, the archive hash, and the version again, and only if the version is not older than the one running. Automatic updates are off until the operator turns them on.

## Rotating the key

Do this while the current key is still trusted, and while you still hold it. Arcades trust a key only when it is pinned in the binary they are already running. A new key cannot sign an update that old binaries will accept until those binaries have been taught the new key, by a release the old key signed.

1. Generate the new key with `--new-key`, as above. Leave the old key in place.
2. Add the new public key to `RELEASE_KEYS` (keep the old one in the list).
3. Ship that change as a release signed by the **old** key, with the usual archives. Arcades that update install a binary that trusts both keys.
4. After that binary is what you are willing to require, later releases may be signed by the new key. A signature is accepted when any pinned key verifies it.
5. Remove the old public key from `RELEASE_KEYS` in a later release signed by the **new** key. Arcades install that only if they already trust the new key (step 3). Until a machine runs a build that no longer lists the old key, the old key still verifies on that machine.

## If the key is compromised

There is no online list that can cancel a key. A stolen key can sign a higher version, and any arcade that still pins that key and has automatic updates on will treat it as genuine.

1. Turn automatic updates off. That is the default on a fresh install. On a machine that opted in: Options, uncheck automatic updates, or `update.set` with `on` false. That stops the install. It does not stop the mesh from carrying a bad record.
2. Do not ship the revocation as an update signed by the stolen key. The holder of the stolen key can sign a higher version that old binaries still trust, so auto-update cannot be the path that drops the key.
3. Publish a build whose `RELEASE_KEYS` no longer contains the stolen key and does contain a new key you hold. Operators install that build by hand (the installer, or a build they check themselves), not through the mesh update.
4. Tell operators to install that build before turning automatic updates on again. Until they do, a machine with updates on will still accept the stolen key.

If the honest holder still has the key and the attacker does not, the rotation steps above are enough: sign the two-key release with the current key, then drop the old key in a release signed by the new one. If you cannot be sure the attacker lacks the key, treat it as stolen and use the hand install.
