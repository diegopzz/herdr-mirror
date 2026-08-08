#!/usr/bin/env bash
# Fetch the prebuilt herdr-mirror binary for this platform from GitHub
# Releases, verified against SHA256SUMS. Run by the herdr plugin [[build]]
# step with cwd = plugin root. No cargo fallback: dev installs (herdr plugin
# link) build with `cargo build --release` themselves.
set -euo pipefail

cd "$(dirname "$0")/.."
DEST="target/release/herdr-mirror"

fail() {
  echo "herdr-mirror fetch failed: $1" >&2
  echo "to build from source instead: cargo build --release" >&2
  exit 1
}

VERSION="$(sed -n 's/^version = "\(.*\)"/\1/p' Cargo.toml | head -1)"
[ -n "$VERSION" ] || fail "cannot read version from Cargo.toml"

# owner/repo from the git remote (plugins are installed by git clone)
SLUG="$(git config --get remote.origin.url 2>/dev/null |
  sed -n 's#.*[:/]\([^/]*/[^/]*\)\.git$#\1#p; s#.*[:/]\([^/]*/[^/]*\)$#\1#p' | head -1)"
[ -n "$SLUG" ] || fail "cannot derive owner/repo from the git remote"

case "$(uname -s)" in
  Darwin) OS="darwin" ;;
  Linux) OS="linux" ;;
  *) fail "unsupported OS: $(uname -s)" ;;
esac
case "$(uname -m)" in
  arm64 | aarch64) ARCH="aarch64" ;;
  x86_64 | amd64) ARCH="x86_64" ;;
  *) fail "unsupported architecture: $(uname -m)" ;;
esac
ASSET="herdr-mirror-${OS}-${ARCH}"
BASE="https://github.com/${SLUG}/releases/download/v${VERSION}"

# SHA-256 verifier. coreutils `sha256sum` on Linux and recent macOS; `shasum`
# (perl Digest::SHA) on older macOS and Debian-family. Neither is universal:
# Arch's perl ships no /usr/bin/shasum, so requiring it made install fail there.
if command -v sha256sum >/dev/null 2>&1; then
  sha256_check() { sha256sum -c -; }
elif command -v shasum >/dev/null 2>&1; then
  sha256_check() { shasum -a 256 -c -; }
else
  fail "no SHA-256 tool found: install coreutils (sha256sum) or perl (shasum)"
fi

TMP="$(mktemp -d)"
trap 'rm -rf "$TMP"' EXIT
echo "fetching ${BASE}/${ASSET}"
curl -fsSL --retry 2 -o "${TMP}/${ASSET}" "${BASE}/${ASSET}" || fail "download failed: ${BASE}/${ASSET}"
curl -fsSL --retry 2 -o "${TMP}/SHA256SUMS" "${BASE}/SHA256SUMS" || fail "download failed: ${BASE}/SHA256SUMS"

# Look the hash up first, so a release missing this asset can't be reported as
# a corrupt download: under `pipefail` a no-match grep fails the whole pipeline
# and would otherwise land on the mismatch message below.
EXPECTED="$(grep " ${ASSET}\$" "${TMP}/SHA256SUMS")" ||
  fail "${ASSET} is not listed in SHA256SUMS — the v${VERSION} release looks incomplete"
(cd "$TMP" && printf '%s\n' "$EXPECTED" | sha256_check) ||
  fail "checksum MISMATCH for ${ASSET} — the download is corrupt or tampered with; do not use it"

mkdir -p "$(dirname "$DEST")"
install -m 755 "${TMP}/${ASSET}" "$DEST"
echo "installed ${ASSET} v${VERSION} at ${DEST}"

# Link the CLI at the stable path the README documents. Keybindings must use
# the absolute ~/.local/bin/herdr-mirror (herdr runs shell bindings through a
# login sh that never reads ~/.zshrc, so PATH can't be trusted there), and
# `herdr-mirror <cmd>` should work from a shell. Refreshed on every update;
# anything at that path we don't manage is left alone.
LINK="${HOME}/.local/bin/herdr-mirror"
TARGET="$(pwd)/${DEST}"

# A link is foreign when it's live, isn't our target, and points outside
# herdr's plugin dirs — e.g. a dev checkout the user linked deliberately.
is_foreign_link() {
  [ -L "$LINK" ] && [ -e "$LINK" ] || return 1
  CUR="$(readlink "$LINK")"
  [ "$CUR" = "$TARGET" ] && return 1
  case "$CUR" in
    "${HOME}/.config/herdr/plugins/"*) return 1 ;; # an older install of ours
  esac
  return 0
}

if [ -e "$LINK" ] && [ ! -L "$LINK" ]; then
  echo "note: ${LINK} exists and is not a symlink; left untouched (README keybindings expect herdr-mirror there)"
elif is_foreign_link; then
  echo "note: ${LINK} -> ${CUR} left untouched; not managed by this install"
else
  mkdir -p "${HOME}/.local/bin"
  ln -sf "$TARGET" "$LINK"
  echo "linked ${LINK} -> ${TARGET}"
fi
