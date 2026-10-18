# Numark NS7II on Linux (and everywhere else)

Reverse engineering of the Numark NS7II's USB protocol, a user-space Linux
driver written in Rust, and a design for a small bridge box that turns the
NS7II into a class-compliant USB audio/MIDI device for any host.

Status on 2026-10-15: built from the vendor packages only, **not yet run
against a real NS7II**. The driver is staged so each layer can be verified
on hardware in minutes; see `docs/04-bring-up-guide.md`.

## Layout

| Path | What |
|------|------|
| `docs/01-vendor-package-analysis.md` | What is inside the Windows driver, macOS driver and firmware updater |
| `docs/02-usb-protocol.md` | The protocol as reconstructed, with a confidence tag on every item |
| `docs/03-converter-box.md` | USB-to-USB bridge: hardware choice, software stack, latency budget |
| `docs/04-bring-up-guide.md` | Step-by-step hardware verification and what to send back |
| `docs/05-midi-chart.md` | The NS7II MIDI chart (every button, knob, fader, LED and motor message) |
| `Cargo.toml`, `crates/` | Rust workspace (see below) |
| `tools/` | Python scripts used to unpack the vendor packages (DMG, pkg, InstallShield, Mach-O disassembly) |
| `udev/` | udev rule so the driver runs without root |
| `systemd/`, `modules/`, `alsa/`, `install.sh` | the service, its settings, the snd-aloop card config, the `ns7ii` ALSA device name and the installer |
| `tmp/` | The vendor downloads (not tracked) |

## Rust workspace

```
crates/ns7ii-proto   protocol: control requests, PCM codecs, MIDI framing   (no unsafe, no I/O, 18 tests)
crates/ns7ii-usb     transport: sysfs discovery, descriptors, usbfs (bulk + isochronous), URB reactor
crates/ns7ii-alsa    libasound loaded at run time: sequencer MIDI ports, S24_3LE PCM
crates/ns7iid        the CLI / daemon
```

`unsafe` lives in exactly two files: `ns7ii-usb/src/usbfs.rs` (Linux usbfs
ioctls, because no pure-Rust USB crate offers isochronous transfers yet) and
`ns7ii-alsa/src/lib.rs` (FFI into libasound). Everything else is `#![forbid(unsafe_code)]`
or free of it.

Build and test (stable Rust, no C headers needed), from this directory:

```
cargo test --workspace
cargo build --release
sudo install -m644 udev/99-numark-ns7ii.rules /etc/udev/rules.d/
sudo udevadm control --reload && sudo udevadm trigger
```

Then, with the NS7II plugged in:

```
./target/release/ns7iid probe        # descriptors + endpoint plan: send this output back
./target/release/ns7iid handshake    # firmware version, channel counts, sample rate
./target/release/ns7iid midi-dump    # raw MIDI from the control surface
./target/release/ns7iid record out.wav --seconds 10
./target/release/ns7iid play --seconds 5          # 440 Hz test tone on every output
./target/release/ns7iid run --pcm-out plughw:Loopback,1,0 --pcm-in plughw:Loopback,1,0
```

## Install

`install.sh` sets up everything a DJ app needs, with no systemd editing:

```
cargo build --release
sudo ./install.sh target/release/ns7iid     # or a pre-built dist/ns7iid-$(uname -m)
```

It installs `/usr/local/bin/ns7iid`, the udev rule (the service starts when
the NS7II is plugged in and exits when it is unplugged), the
`ns7iid.service` unit with its `/usr/local/libexec/ns7iid-start` helper,
`/etc/modules-load.d/ns7ii.conf` and `/etc/modprobe.d/ns7ii-aloop.conf`
(the `snd-aloop` card, loaded at boot with the id `Loopback`, never card 0),
`/etc/alsa/conf.d/50-ns7ii.conf` (the ALSA device `ns7ii`, see below),
and `/etc/default/ns7iid` (only if absent; your edits survive reinstalls).
Running it again updates everything in place.

The service bridges audio by default: the NS7II's 4 outputs and 12 inputs
appear on the other end of the loopback. In the DJ app choose

| | |
|---|---|
| audio output and input | `ns7ii` "Numark NS7II Audio Controller" (ALSA) |
| MIDI controller | "Numark NS7II MIDI 1" |

`ns7ii` is a named plug PCM on `hw:CARD=Loopback,DEV=0,SUBDEV=0`, defined in
`alsa/50-ns7ii.conf`; `aplay -L`, `arecord -L` and apps that list ALSA
devices show it as "Numark NS7II Audio Controller". alsa-lib 1.2.x (1.2.6 on
Ubuntu / Pop!_OS 22.04) reads `/etc/alsa/conf.d/*.conf` by default, so
nothing else needs editing. Apps that only take a raw device string can use
the equivalent `plughw:CARD=Loopback,DEV=0`.
Like `plughw:`, it lets the app use any format, channel count and rate, and
passes the channel count through: 4 channels give master on 1-2 and cue on
3-4, plain stereo apps get the master. For playback
ns7iid opens its end of the loopback only once an app has opened the other
end, takes the app's rate as it is and converts it to the NS7II's 44.1 kHz
itself with a band-limited resampler that also absorbs the clock difference
between the loopback (kernel timer) and the unit (its own crystal, followed
through its feedback endpoint). While no app plays, the unit gets silence and
the journal says so once. For capture, raw `hw:CARD=Loopback,DEV=0` works
only if the app opens exactly 12 ch S24_3LE at 44100 Hz.

Check it:

```
systemctl status ns7iid
journalctl -u ns7iid -f            # "playback bridge: reading plughw:CARD=Loopback,DEV=1,SUBDEV=0 ..."
aplay -L | grep -A1 ns7ii          # "ns7ii" / "Numark NS7II Audio Controller"
aplay -D ns7ii -r 44100 -c 2 some.wav   # plays from the NS7II master out
```

What the journal shows, and how to watch a problem:

```
journalctl -u ns7iid -b -o short-precise                 # this boot
journalctl -u ns7iid -f | grep -E "status:|queue|underrun|feedback|MIDI OUT|fault|left the USB|reset|reopen"
journalctl -u ns7iid -b | grep -cE "underrun|overflowed"  # audio dropouts since boot
journalctl -u ns7iid -b | grep -E "fault|left the USB|reset|reopen|keeps failing|panic"   # USB recoveries
```

* `feedback EP 81: first packet [2c, ..]` and `feedback locked` at start-up:
  playback follows the unit's clock. `no usable feedback` or `lost sync`:
  send that line back.
* `playback bridge: queue 20.1 ms (target 20 ms), clock correction +85 ppm,
  0 underruns`: once a minute while an app plays. The correction is the
  loopback-vs-NS7II clock difference (typically within ±300 ppm) and should
  settle; underruns should stay 0. `playback underrun` lines name the queue
  level when it happened.
* `status: playback ... feedback locked ... capture ...`: once a minute per
  USB session.
* `MIDI OUT to the NS7II timed out`: the unit was busy; the writer slowed
  down. Lower `--midi-out-rate` (in `NS7IID_ARGS`) if it repeats.
* `... stream fault ...; resetting the NS7II's USB port` / `left the USB bus
  ...; waiting up to 30 s`: ns7iid recovers by itself (the MIDI port stays,
  apps stay connected). Exit status 69 means the unit stayed away; udev starts
  the service again on the next plug-in. Any other exit is restarted by
  systemd after 3 s.

Settings live in `/etc/default/ns7iid` (`NS7IID_AUDIO=0` for MIDI only,
`NS7IID_PCM_OUT` / `NS7IID_PCM_IN`, extra `NS7IID_ARGS`); apply them with
`sudo systemctl restart ns7iid`. `sudo ./install.sh --uninstall` removes
everything the installer added (an edited `/etc/default/ns7iid` is kept);
`snd-aloop` stays loaded until reboot or `sudo modprobe -r snd-aloop`.

## Mixxx mapping

Mixxx has no NS7II mapping and Numark never published the MIDI chart. The
chart is now reconstructed (`docs/05-midi-chart.md`) and built into the
driver, so a complete mapping is one command away:

```
ns7iid mixxx-map          # writes the mapping into Mixxx's controller folder
```

It writes `Numark NS7II.midi.xml` and `Numark-NS7II-scripts.js` into the
folder Mixxx scans for user mappings:

| Mixxx install | folder |
|---------------|--------|
| native package / AppImage | `~/.mixxx/controllers/` |
| Flatpak | `~/.var/app/org.mixxx.Mixxx/.mixxx/controllers/` |
| Snap | `~/snap/mixxx/current/.mixxx/controllers/` |

It picks whichever of these settings folders exists; `--out-dir` overrides
it. The mapping loads Mixxx's own `lodash.mixxx.js` and
`midi-components-0.0.js` libraries and works on Mixxx 2.3 (whose QtScript
engine only understands ES5) and newer; `mixxx-map` warns if a Mixxx
install on the machine lacks either file.
Mixxx reads the folder only at start-up, so restart Mixxx afterwards,
make sure the driver is running (the controller only appears in Mixxx
while `ns7iid` runs), open Preferences, Controllers, click "Numark NS7II" in
the list on the left (the controller appears as "Numark NS7II MIDI 1"), choose "Numark NS7II" under Load Mapping, tick
Enabled and Apply.

The NS7II is a four-deck controller. Its mixer strips are, from left to
right, channels 3, 1, 2 and 4, and each side's DECK buttons switch that
side between decks 1/3 or 2/4; every message carries its deck number, so
the outer strips and the DECK 3 / DECK 4 layers land on Mixxx decks 3 and
4. Most Mixxx skins hide those until you switch the skin to four decks, so
either do that or press DECK 1 and DECK 2 on the unit.

The mapping covers all four decks (DECK buttons switch layers), platters,
strip search, the hot cue row, pads in hot cue / auto loop / manual loop /
slicer / sampler modes, beat grid buttons, FX, mixer with FX and crossfader
assign, library, microphone, front panel switches and LEDs. The platter
motors start and stop with the deck and follow the pitch fader (the motor
commands were decoded from the firmware; the pitch scale is a setting until
confirmed on hardware). SHIFT + SLIP MODE turns a deck's motor off, which
makes its platter a plain jog wheel. Settings such as pitch fader direction,
platter resolution and motor behaviour are at the top of
`Numark-NS7II-scripts.js`.

If a control on your unit does not match the chart, `midi-learn` still
exists: it asks for each control in turn, saves to `ns7ii-learn.json`, and
`mixxx-map --capture ns7ii-learn.json` lets the captured messages override
the chart (`--capture-only` ignores the chart entirely). The service holds
the device, so stop it first: `systemctl stop ns7iid`.

## One correction to the premise

USB itself has no 32-bit or 64-bit flavour. What broke the NS7II on modern
machines is that Numark's drivers are Ploytec kernel drivers from 2013 to 2016:
macOS stopped loading third-party kernel extensions that are not notarized
and re-signed (and Apple Silicon Macs need a reduced-security boot to load
any kext at all), and the Windows driver carries a pre-2015 cross-signature
that Windows 10/11 refuse on fresh installs. The device also is not USB Audio
Class compliant, so no generic driver picks it up. Both points are solved by
either a native driver (this directory, Linux) or by the bridge box, which hides
the proprietary protocol behind a standard USB Audio Class 2 + USB-MIDI
device that every OS, including iPadOS, drives out of the box.
