#!/bin/sh
# Install a pre-built ns7iid binary and the udev rule. Run with sudo.
# Usage: sudo ./install.sh [path/to/ns7iid-binary]   (default: dist/ns7iid-$(uname -m))
set -e
here=$(cd "$(dirname "$0")" && pwd)
bin=${1:-"$here/dist/ns7iid-$(uname -m)"}
[ -f "$bin" ] || { echo "no binary at $bin"; exit 1; }
install -m755 "$bin" /usr/local/bin/ns7iid
install -m644 "$here/udev/99-numark-ns7ii.rules" /etc/udev/rules.d/
udevadm control --reload && udevadm trigger
if ! lsmod | grep -q '^snd_aloop'; then modprobe snd-aloop 2>/dev/null || true; fi
echo "installed /usr/local/bin/ns7iid; replug the NS7II, then run: ns7iid probe"
