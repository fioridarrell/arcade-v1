#!/usr/bin/env bash
# Detached signatures for the Linux release archives.
#
# Either secret is enough. Both may be set. Neither is set today: Martin has to
# add one before a release carries a signature. An empty secret skips that
# signature and says so. A secret that is set and then fails to sign fails the job.
#
#   LINUX_MINISIGN_SECRET_KEY   the minisign secret key file (unencrypted; create
#                               it with: minisign -G -W -s arcade-minisign.key -p arcade-minisign.pub)
#   LINUX_GPG_PRIVATE_KEY       ASCII-armored GPG private key
#   LINUX_GPG_PASSPHRASE        passphrase for that key, if it has one
#
# Writes, next to the archives:
#   arcade-v1-linux-<arch>.tar.gz.minisig
#   arcade-v1-minisign.pub
#   arcade-v1-linux-<arch>.tar.gz.asc
#   arcade-v1-gpg.pub.asc
# The archives themselves are not modified, so their .sha256 files stay valid.
set -euo pipefail
umask 077

have_mini=0
have_gpg=0
if [ -n "${LINUX_MINISIGN_SECRET_KEY:-}" ]; then have_mini=1; fi
if [ -n "${LINUX_GPG_PRIVATE_KEY:-}" ]; then have_gpg=1; fi

if [ "$have_mini" -eq 0 ] && [ "$have_gpg" -eq 0 ]; then
  echo "Linux detached signature skipped: neither LINUX_MINISIGN_SECRET_KEY nor LINUX_GPG_PRIVATE_KEY is set. The .sha256 files are still published. Martin needs to supply a minisign secret key (minisign -G -W) or an ASCII-armored GPG private key before releases carry a detached signature."
  exit 0
fi

shopt -s nullglob
files=(arcade-v1-linux-*.tar.gz)
if [ "${#files[@]}" -eq 0 ]; then
  echo "refusing to sign: no arcade-v1-linux-*.tar.gz archives are in $(pwd)" >&2
  exit 1
fi

cleanup() {
  if [ -n "${mini_key:-}" ]; then rm -f "$mini_key"; fi
  if [ -n "${gpg_home:-}" ]; then rm -rf "$gpg_home"; fi
}
trap cleanup EXIT

if [ "$have_mini" -eq 1 ]; then
  if ! command -v minisign >/dev/null 2>&1; then
    echo "refusing to sign: LINUX_MINISIGN_SECRET_KEY is set but minisign is not installed" >&2
    exit 1
  fi
  mini_key="$(mktemp)"
  printf '%s' "$LINUX_MINISIGN_SECRET_KEY" > "$mini_key"
  # The key file is line oriented. A secret pasted into CI often drops the last newline.
  if [ -n "$(tail -c 1 "$mini_key" || true)" ]; then
    printf '\n' >> "$mini_key"
  fi
  if ! minisign -R -s "$mini_key" -p arcade-v1-minisign.pub >/dev/null; then
    echo "refusing to sign: LINUX_MINISIGN_SECRET_KEY is not an unencrypted minisign secret key. Create it with: minisign -G -W -s arcade-minisign.key -p arcade-minisign.pub" >&2
    exit 1
  fi
  for f in "${files[@]}"; do
    rm -f "$f.minisig"
    minisign -S -W -s "$mini_key" -m "$f" -x "$f.minisig" -c "Arcade V1.0" -t "Arcade V1.0 release archive"
  done
  echo "minisign: signed ${files[*]} and wrote arcade-v1-minisign.pub"
fi

if [ "$have_gpg" -eq 1 ]; then
  if ! command -v gpg >/dev/null 2>&1; then
    echo "refusing to sign: LINUX_GPG_PRIVATE_KEY is set but gpg is not installed" >&2
    exit 1
  fi
  gpg_home="$(mktemp -d)"
  chmod 700 "$gpg_home"
  export GNUPGHOME="$gpg_home"
  key_file="$(mktemp)"
  printf '%s' "$LINUX_GPG_PRIVATE_KEY" > "$key_file"
  if [ -n "$(tail -c 1 "$key_file" || true)" ]; then
    printf '\n' >> "$key_file"
  fi
  gpg --batch --import "$key_file"
  rm -f "$key_file"
  gpg_args=(--batch --yes --pinentry-mode loopback --detach-sign --armor)
  if [ -n "${LINUX_GPG_PASSPHRASE:-}" ]; then
    gpg_args+=(--passphrase "$LINUX_GPG_PASSPHRASE")
  fi
  for f in "${files[@]}"; do
    rm -f "$f.asc"
    gpg "${gpg_args[@]}" -o "$f.asc" "$f"
  done
  gpg --batch --export --armor > arcade-v1-gpg.pub.asc
  echo "gpg: signed ${files[*]} and wrote arcade-v1-gpg.pub.asc"
fi
