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
| `ns7ii/` | Rust workspace (see below) |
| `tools/` | Python scripts used to unpack the vendor packages (DMG, pkg, InstallShield, Mach-O disassembly) |
| `udev/` | udev rule so the driver runs without root |
| `tmp/` | The vendor downloads (not tracked) |

## Rust workspace

```
ns7ii/crates/ns7ii-proto   protocol: control requests, PCM codecs, MIDI framing   (no unsafe, no I/O, 18 tests)
ns7ii/crates/ns7ii-usb     transport: sysfs discovery, descriptors, usbfs (bulk + isochronous), URB reactor
ns7ii/crates/ns7ii-alsa    libasound loaded at run time: sequencer MIDI ports, S24_3LE PCM
ns7ii/crates/ns7iid        the CLI / daemon
```

`unsafe` lives in exactly two files: `ns7ii-usb/src/usbfs.rs` (Linux usbfs
ioctls, because no pure-Rust USB crate offers isochronous transfers yet) and
`ns7ii-alsa/src/lib.rs` (FFI into libasound). Everything else is `#![forbid(unsafe_code)]`
or free of it.

Build and test (stable Rust, no C headers needed):

```
cd ns7ii
cargo test --workspace
cargo build --release
sudo install -m644 ../udev/99-numark-ns7ii.rules /etc/udev/rules.d/
sudo udevadm control --reload && sudo udevadm trigger
```

Then, with the NS7II plugged in:

```
./target/release/ns7iid probe        # descriptors + endpoint plan: send this output back
./target/release/ns7iid handshake    # firmware version, channel counts, sample rate
./target/release/ns7iid midi-dump    # raw MIDI from the control surface
./target/release/ns7iid record out.wav --seconds 10
./target/release/ns7iid play --seconds 5          # 440 Hz test tone on every output
./target/release/ns7iid run --pcm-out hw:Loopback,1,0 --pcm-in hw:Loopback,1,1
```

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
it. Mixxx reads the folder only at start-up, so restart Mixxx afterwards,
make sure the driver is running (the controller only appears in Mixxx
while `ns7iid` runs), open Preferences, Controllers, click "Numark NS7II" in
the list on the left, choose "Numark NS7II" under Load Mapping, tick
Enabled and Apply.

The mapping covers all four decks (DECK buttons switch layers), platters,
strip search, the hot cue row, pads in hot cue / auto loop / manual loop /
slicer / sampler modes, beat grid buttons, FX, mixer with FX and crossfader
assign, library, microphone, front panel switches and LEDs. Settings such
as pitch fader direction and platter resolution are at the top of
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
either a native driver (this repo, Linux) or by the bridge box, which hides
the proprietary protocol behind a standard USB Audio Class 2 + USB-MIDI
device that every OS, including iPadOS, drives out of the box.
