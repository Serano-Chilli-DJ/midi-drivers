# Bring-up guide: verifying the driver on a real NS7II

Everything in this repo was derived from the vendor binaries. These steps
confirm each assumption on hardware and produce the evidence needed to fix
whatever differs. Each step takes a minute; stop at the first one that fails
and send its output.

Build first (any Linux, stable Rust):

```
cd ns7ii && cargo build --release
sudo install -m644 ../udev/99-numark-ns7ii.rules /etc/udev/rules.d/
sudo udevadm control --reload && sudo udevadm trigger
export RUST_LOG=debug     # the daemon logs every control transfer
```

## Step 0: what the kernel sees

```
lsusb -d 15e4:002c -v 2>/dev/null | tee lsusb.txt
dmesg | tail -30
aseqdump -l                 # does a "Numark NS7 II" MIDI port already exist?
arecord -l; aplay -l        # does an ALSA card appear (it should not)
```

If `aseqdump -l` lists the NS7II, the MIDI interface is class compliant and
Linux already provides MIDI; only audio needs the driver.

## Step 1: `ns7iid probe`

Prints descriptors, kernel drivers per interface and the endpoint plan. Check:

* Is there an isochronous OUT endpoint? That is playback.
* Is there a tiny isochronous IN endpoint (wMaxPacketSize 3 or 4)? Feedback.
* Which bulk/interrupt IN endpoints exist, and on which interface? Capture
  and raw MIDI in. If the plan assigns them the wrong way round, pass
  `--capture-ep 0x8n --midi-in-ep 0x8m` to every later command.

## Step 2: `ns7iid handshake`

Expected: firmware version, `in N ch / out M ch` from the device, status
bytes, `GET_CUR` returning 44100 after `SET_CUR`. Failures to note:

* `topology request failed`: the 5-byte 0x56 probe is not supported; channel
  counts fall back to 12/4.
* `status read failed`: the 0x49 register is not on this chip; harmless.
* `SET_CUR ... failed` with EPIPE: the endpoint address is wrong (see step 1).

## Step 3: MIDI

```
ns7iid midi-dump            # move a fader; expect lines like "raw fd fd b0 07 40  midi b0 07 40"
ns7iid midi-send 90 3c 7f   # should light an LED/pad; then 80 3c 00
```

If `midi-dump` only ever prints filler bytes, the MIDI IN endpoint is wrong
(try the other bulk IN). If nothing arrives at all but `aseqdump` showed a
port in step 0, use `aseqdump -p <port>` instead; the kernel owns MIDI.

## Step 4: capture

```
ns7iid record test.wav --seconds 10
```

Play something into the NS7II's inputs or scratch a platter (deck audio is
sent to the computer). Check the printed byte rate: it must be about
`44100 × channels × 3` bytes per second (1.59 MB/s for 12 channels). Then
open `test.wav`: clean audio on some channels means `plain24` is right; white
noise at the right level means the wire format is the junction bit-plane
codec: retry with `--codec junction`. Silence with the right byte rate means
the channel count is wrong (try `--channels 4`, `8`, `12`, `16`).

For the definitive answer capture the USB traffic too:

```
sudo modprobe usbmon
sudo tcpdump -i usbmon<bus> -w ns7ii-record.pcap &      # bus number from `ns7iid list`
ns7iid record test.wav --seconds 5
```

## Step 5: playback

```
ns7iid play --seconds 5      # 440 Hz tone on every output at -10 dBFS
```

Listen on Master OUT. Watch the statistics line: `pkt errors` must stay 0
and `underrun` must stay 0. If the tone is there but pitched wrong or
crackling, the pacing is off: the feedback value format needs decoding (the
daemon logs raw feedback bytes at debug level, send them). If there is no
sound and no errors, try `--codec junction`, then a different `--play-ep`.

## Step 6: the daemon

```
sudo modprobe snd-aloop
ns7iid run --pcm-out plughw:Loopback,1,0 --pcm-in plughw:Loopback,1,0
```

(If `install.sh` already ran, the service owns the device: stop it first
with `sudo systemctl stop ns7iid`.)

Both bridges use subdevice 0 of loopback device 1, so apps play into and
record from the same `plughw:CARD=Loopback,DEV=0` (subdevice 0 when none is
given). The two directions are separate cables, so playback can be 4
channels and capture 12.

snd-aloop only passes audio when both ends of a cable use the same format,
channel count and rate. ns7iid asks for 4 ch (playback) / 12 ch (capture)
S24_3LE at 44100 Hz; an app that opens raw `hw:` with, say, 32 channels of
F32_LE makes the kernel fail ns7iid's side with EIO. `plughw:` converts, so
use it on ns7iid's side (as above) and, preferably, in the app too. If it
still happens, ns7iid logs `playback bridge ...: I/O failed: ... Input/output error` once
per second with the likely cause and keeps reopening the PCM until the
formats agree.

Point Mixxx at `plughw:CARD=Loopback,DEV=0` for both output and input and
at the "Numark NS7II" MIDI ports. PipeWire users can instead use
`pw-loopback` or just leave PipeWire on the loopback card.

### As an installed service

Once `ns7iid run` works by hand, `sudo ./install.sh ns7ii/target/release/ns7iid`
(from the repo root, path to the binary you built) makes it permanent: the
`Loopback` card loads at boot, and the service starts on plug-in with the
plughw bridge above. Its settings are in `/etc/default/ns7iid`.

```
systemctl status ns7iid
journalctl -u ns7iid -f
aplay -D plughw:CARD=Loopback,DEV=0 -r 44100 -c 2 some.wav    # sound on Master OUT
cat /proc/asound/cards                                        # a "Loopback" card
```

Apps should open `plughw:CARD=Loopback,DEV=0` for output and input. If the
journal repeats `I/O failed ... Input/output error`, an app holds the
loopback in a format ns7iid cannot convert to; switch the app to plughw.

* MIDI only: set `NS7IID_AUDIO=0` in `/etc/default/ns7iid`, then
  `sudo systemctl restart ns7iid`.
* Run by hand for debugging: `sudo systemctl stop ns7iid`, then
  `RUST_LOG=debug ns7iid run --pcm-out plughw:Loopback,1,0 --pcm-in plughw:Loopback,1,0`.
* Roll back: `sudo ./install.sh --uninstall` removes the binary, unit,
  helper, udev rule and module config (and `/etc/default/ns7iid` unless
  edited); reboot or `sudo modprobe -r snd-aloop` to drop the card.

### Audio quality, drift and recovery checks (need the hardware)

These parts were built and unit-tested without a unit (simulated clocks,
fake PCMs and a fake MIDI endpoint); confirm them on the NS7II:

1. **Feedback format.** `journalctl -u ns7iid -b | grep -E "feedback"`
   should show `first packet [2c ...]` or `[2d ...]` (44 or 45 frames,
   3 bytes) and `feedback locked` within a second. Also run
   `sudo systemctl stop ns7iid; ns7iid play --seconds 20` and check the
   `feedback ... locked true, 0 out of range, 0 bad length` part of the
   statistics line. Anything else: send the lines.
2. **No drift clicks.** Play a steady sine for 30 minutes from the DJ app (or
   `speaker-test -D plughw:CARD=Loopback,DEV=0 -c 2 -r 48000 -t sine -f 1000`)
   and listen on Master OUT; the minute lines must show a settled
   `clock correction` and `0 underruns`:
   `journalctl -u ns7iid -f | grep -E "queue|underrun|overflowed"`.
3. **Idle is quiet.** With no app playing, `journalctl -u ns7iid -f` shows
   one `no app playing into the loopback` and nothing else.
4. **MIDI OUT under load.** Run the DJ app with meters and pad colours for
   an hour; `journalctl -u ns7iid -b | grep -c "MIDI OUT"` should stay near 0,
   and `systemctl status ns7iid` should show memory staying around its
   start-up value (a few MB, not hundreds).
5. **Recovery.** `sudo usbreset 15e4:002c` (package `usbutils`) while the
   app runs: the journal should show `left the USB bus`, `NS7II reopened and
   handshaken`, `NS7II streaming`, and the app keeps its MIDI connection.

## Step 7: Mixxx mapping

```
ns7iid mixxx-map                  # built-in chart -> ~/.mixxx/controllers (or the Flatpak / Snap folder)
```

Then start the driver, restart Mixxx, open Preferences, Controllers, click
"Numark NS7II" on the left and pick the "Numark NS7II" mapping under Load
Mapping. The chart is in `docs/05-midi-chart.md`.

If a control misbehaves, capture it from the unit and let the capture
override the chart:

```
systemctl stop ns7iid
ns7iid midi-learn                 # Enter skips, r redoes, s skips the section, q saves and quits
ns7iid mixxx-map --capture ns7ii-learn.json
systemctl start ns7iid
```

`mixxx-map` prints what it mapped and what a capture still lacks. For the
deck 3 and 4 prompts press the DECK button first; when a deck 3/4 capture
equals deck 1/2 the DECK button had no effect and that deck is left
unmapped rather than driving two Mixxx decks at once.

## What to send back

`lsusb.txt`, the full output of `probe` and `handshake`, any pcap, and for
audio problems a 5-second `record` WAV. With those the remaining
**assumed** items in `02-usb-protocol.md` can be settled in one pass.
