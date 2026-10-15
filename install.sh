#!/bin/sh
# Install a pre-built ns7iid binary, the udev rule and the systemd service. Run with sudo.
# Usage: sudo ./install.sh [path/to/ns7iid-binary]   (default: dist/ns7iid-$(uname -m))
set -e
here=$(cd "$(dirname "$0")" && pwd)
bin=${1:-"$here/dist/ns7iid-$(uname -m)"}
[ -f "$bin" ] || { echo "no binary at $bin"; exit 1; }
systemctl stop ns7iid.service 2>/dev/null || true
install -m755 "$bin" /usr/local/bin/ns7iid
install -m644 "$here/udev/99-numark-ns7ii.rules" /etc/udev/rules.d/
install -m644 "$here/systemd/ns7iid.service" /etc/systemd/system/
systemctl daemon-reload
udevadm control --reload && udevadm trigger --subsystem-match=usb --attr-match=idVendor=15e4
if ! lsmod | grep -q '^snd_aloop'; then modprobe snd-aloop 2>/dev/null || true; fi
echo "installed. The driver now starts automatically when the NS7II is plugged in:"
echo "  systemctl status ns7iid     # is it running?"
echo "  journalctl -u ns7iid -f     # its log"
echo "To run it by hand instead: systemctl stop ns7iid && ns7iid run"
