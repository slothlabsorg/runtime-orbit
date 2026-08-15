#!/bin/sh
# runtime-orbit installer — macOS & Linux.
#
#   curl -fsSL https://slothlabs.org/install/runtime-orbit | sh
#   curl -fsSL https://raw.githubusercontent.com/slothlabsorg/runtime-orbit/main/dist/install.sh | sh
#
# Downloads the latest release binary for your OS/arch and installs
# `runtime-orbit`, plus the `r-orbit` and `orbit` shortcuts.
#
# Env:
#   ORBIT_INSTALL_DIR   install location (default: /usr/local/bin, else ~/.local/bin)
#   ORBIT_VERSION       version tag to install (default: latest)
set -eu

REPO="slothlabsorg/runtime-orbit"
BIN="runtime-orbit"
ALIASES="r-orbit orbit"

say()  { printf '\033[36m▸\033[0m %s\n' "$1"; }
ok()   { printf '\033[32m✓\033[0m %s\n' "$1"; }
warn() { printf '\033[33m!\033[0m %s\n' "$1"; }
die()  { printf '\033[31merror:\033[0m %s\n' "$1" >&2; exit 1; }

os=$(uname -s)
arch=$(uname -m)

case "$os" in
  Darwin) plat="apple-darwin" ;;
  Linux)  plat="unknown-linux-gnu" ;;
  *) die "unsupported OS: $os (use install.ps1 on Windows)" ;;
esac
case "$arch" in
  arm64|aarch64) cpu="aarch64" ;;
  x86_64|amd64)  cpu="x86_64" ;;
  *) die "unsupported architecture: $arch" ;;
esac
target="${cpu}-${plat}"

command -v curl >/dev/null 2>&1 || die "curl is required"
command -v tar  >/dev/null 2>&1 || die "tar is required"

if [ "${ORBIT_VERSION:-latest}" = "latest" ]; then
  base="https://github.com/${REPO}/releases/latest/download"
else
  base="https://github.com/${REPO}/releases/download/${ORBIT_VERSION}"
fi
asset="${BIN}-${target}.tar.gz"
url="${base}/${asset}"

# Choose an install dir we can actually write to.
dir="${ORBIT_INSTALL_DIR:-/usr/local/bin}"
if [ ! -d "$dir" ] || [ ! -w "$dir" ]; then
  dir="$HOME/.local/bin"
fi
mkdir -p "$dir"

tmp=$(mktemp -d)
trap 'rm -rf "$tmp"' EXIT

say "Downloading $asset…"
curl -fsSL "$url" -o "$tmp/$asset" || die "download failed: $url"
tar -xzf "$tmp/$asset" -C "$tmp"
[ -f "$tmp/$BIN" ] || die "archive did not contain '$BIN'"
install -m 0755 "$tmp/$BIN" "$dir/$BIN"
ok "installed $BIN to $dir/$BIN"

# Short aliases. A symlink is enough — the CLI doesn't branch on argv[0].
for a in $ALIASES; do
  # Don't clobber an unrelated binary that happens to share the name.
  if [ -e "$dir/$a" ] && [ ! -L "$dir/$a" ]; then
    warn "left $dir/$a alone (not a symlink — something else owns that name)"
    continue
  fi
  ln -sf "$dir/$BIN" "$dir/$a" && ok "linked $a → $BIN"
done

case ":$PATH:" in
  *":$dir:"*) : ;;
  *) warn "add $dir to your PATH:  export PATH=\"$dir:\$PATH\"" ;;
esac

echo
ok "Next — on the machine that needs the RAM:"
printf '      \033[1mruntime-orbit setup --ip <donor-ip>\033[0m\n'
echo
printf '   …and on the machine lending its runtime:\n'
printf '      \033[1mruntime-orbit donor setup\033[0m\n'
