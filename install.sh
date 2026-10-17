#!/bin/sh
# Install (or remove) the ns7iid driver: binary, udev rule, systemd service,
# its start helper and settings, and the snd-aloop "Loopback" card that DJ
# apps play into. Safe to run again (an update keeps /etc/default/ns7iid).
# Usage: sudo ./install.sh [path/to/ns7iid-binary]   (default: dist/ns7iid-$(uname -m))
#        sudo ./install.sh --uninstall
set -e
here=$(cd "$(dirname "$0")" && pwd)

BIN=/usr/local/bin/ns7iid
HELPER=/usr/local/libexec/ns7iid-start
UNIT=/etc/systemd/system/ns7iid.service
RULE=/etc/udev/rules.d/99-numark-ns7ii.rules
DEFAULTS=/etc/default/ns7iid
MODLOAD=/etc/modules-load.d/ns7ii.conf
MODPROBE=/etc/modprobe.d/ns7ii-aloop.conf

[ "$(id -u)" = 0 ] || { echo "run with sudo"; exit 1; }

if [ "$1" = --uninstall ]; then
    systemctl stop ns7iid.service 2>/dev/null || true
    rm -f "$UNIT" "$RULE" "$HELPER" "$BIN" "$MODLOAD" "$MODPROBE"
    if [ -f "$DEFAULTS" ]; then
        if cmp -s "$DEFAULTS" "$here/systemd/ns7iid.default"; then
            rm -f "$DEFAULTS"
        else
            echo "kept $DEFAULTS (edited); remove it by hand if you no longer need it"
        fi
    fi
    systemctl daemon-reload
    udevadm control --reload
    echo "uninstalled. The snd-aloop module stays loaded until reboot"
    echo "(or: sudo modprobe -r snd-aloop, once no app uses the Loopback card)."
    exit 0
fi

bin=${1:-"$here/dist/ns7iid-$(uname -m)"}
[ -f "$bin" ] || { echo "no binary at $bin"; exit 1; }
systemctl stop ns7iid.service 2>/dev/null || true
install -m755 "$bin" "$BIN"
install -D -m755 "$here/systemd/ns7iid-start" "$HELPER"
install -m644 "$here/udev/99-numark-ns7ii.rules" "$RULE"
install -m644 "$here/systemd/ns7iid.service" "$UNIT"
install -D -m644 "$here/modules/ns7ii-modules-load.conf" "$MODLOAD"
install -D -m644 "$here/modules/ns7ii-aloop.conf" "$MODPROBE"
if [ -e "$DEFAULTS" ]; then
    echo "keeping existing $DEFAULTS (new template: $here/systemd/ns7iid.default)"
else
    install -D -m644 "$here/systemd/ns7iid.default" "$DEFAULTS"
fi
# Load the loopback card now (at boot, modules-load.d does it). A copy loaded
# earlier without these options keeps its old card index until it is reloaded.
if [ ! -e /proc/asound/Loopback ]; then
    modprobe snd-aloop || echo "warning: could not load snd-aloop; audio bridging stays off until it loads"
fi
systemctl daemon-reload
udevadm control --reload && udevadm trigger --subsystem-match=usb --attr-match=idVendor=15e4
# Start (or restart, after the stop above) the service if the NS7II is plugged in now.
for d in /sys/bus/usb/devices/*; do
    if [ "$(cat "$d/idVendor" 2>/dev/null)" = 15e4 ] && [ "$(cat "$d/idProduct" 2>/dev/null)" = 002c ]; then
        systemctl start --no-block ns7iid.service
    fi
done

echo "installed. The driver starts automatically when the NS7II is plugged in,"
echo "bridging audio to the ALSA card 'Loopback'. Point the DJ app at"
echo "  plughw:CARD=Loopback,DEV=0   (output and input)"
echo "and the 'Numark NS7II' MIDI port."
echo "  systemctl status ns7iid     # is it running?"
echo "  journalctl -u ns7iid -f     # its log"
echo "Settings (NS7IID_AUDIO=0 for MIDI only): $DEFAULTS, then: systemctl restart ns7iid"
echo "To run it by hand instead: systemctl stop ns7iid && ns7iid run"
echo "Remove everything: sudo $here/install.sh --uninstall"
