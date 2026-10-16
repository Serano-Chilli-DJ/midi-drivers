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
ns7iid run --pcm-out hw:Loopback,1,0 --pcm-in hw:Loopback,1,1
```

Point Mixxx at `hw:Loopback,0,0` (output) and `hw:Loopback,0,1` (input) and
at the "Numark NS7II" MIDI ports. PipeWire users can instead use
`pw-loopback` or just leave PipeWire on the loopback card.

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
