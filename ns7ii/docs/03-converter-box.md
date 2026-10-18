# A USB-to-USB bridge for the NS7II

Goal: a box with two USB ports. The NS7II plugs into the host port; the
computer (Windows, macOS, Linux, iPadOS, ChromeOS) plugs into the device
port and sees a **USB Audio Class 2.0 sound card plus a USB-MIDI class
port**, which every operating system drives with built-in drivers. The box
runs this repository's driver and translates in both directions.

## Why a Linux SBC and not a microcontroller

The translation needs a USB *host* stack with isochronous support for the
NS7II side and a USB *device* (gadget) stack with UAC2 + MIDI functions for
the computer side, and about 10 Mbit/s of audio moving between them with
low jitter. Linux has both stacks mature and in-tree (`usbfs`/`snd-usb` on
the host side, `configfs` gadgets `f_uac2` and `f_midi` on the device side).
A bare-metal MCU with dual USB (e.g. STM32H7 with HS + FS, or RP2350 with
PIO-USB) could do it eventually but would mean writing both stacks' audio
classes from scratch; that is a second project.

## Hardware candidates

| Board | USB host | USB gadget | Notes |
|-------|----------|------------|-------|
| Raspberry Pi 4 B | 4× USB-A (xHCI, high speed) | USB-C power port (dwc2, high speed) | Cheapest proven combo; the USB-C port must be switched to OTG/peripheral mode (`dtoverlay=dwc2,dr_mode=peripheral`), power then comes from the computer or a GPIO PSU |
| Raspberry Pi Zero 2 W | micro-USB OTG (one port) | same port | Only one USB port: needs a hub for host and loses gadget mode. Not suitable |
| Raspberry Pi 5 | 4× USB-A | USB-C (dwc2) | Same as Pi 4 with more CPU; audio ports draw little |
| Radxa Zero 3 / Rock 4 / Orange Pi 5 | USB-A/USB 3 host | USB-C OTG | All run mainline-ish Linux with dwc3 gadget; Orange Pi 5 is overkill but has two controllers |
| Compute Module 4 + carrier with two USB-C | yes | yes | For a product-like enclosure |

Recommendation: start on a **Raspberry Pi 4** (host: USB-A; gadget: USB-C),
move to a CM4 carrier if a compact enclosure matters. The Pi 4's dwc2 gadget
controller supports isochronous IN/OUT at high speed; UAC2 gadgets at 44.1 kHz
24-bit with 12 capture and 4 playback channels are within its bandwidth
(44 100 × 16 × 3 ≈ 2.1 MB/s each way).

## Software stack on the box

```
          NS7II (vendor protocol)                      Computer (class compliant)
   ┌──────────────┐  iso OUT / bulk IN / bulk MIDI   ┌───────────────────────────┐
   │ USB host port│ ───────────────────────────────► │                           │
   └──────┬───────┘                                  │  dwc2 in peripheral mode  │
          │ usbfs                                    │  configfs gadget:         │
   ┌──────▼───────────────┐    ALSA PCM + rawmidi    │   f_uac2  (UAC2 card)     │
   │ ns7iid run           │ ◄──────────────────────► │   f_midi  (USB-MIDI port) │
   │  --pcm-in  hw:UAC2Gadget,0   (box → computer)   │                           │
   │  --pcm-out hw:UAC2Gadget,0   (computer → box)   └───────────────────────────┘
   │  MIDI via ALSA seq ⇄ f_midi (hw:f_midi)
   └──────────────────────┘
```

1. **Gadget definition** (`/sys/kernel/config/usb_gadget/ns7ii/`): one
   configuration with `f_uac2` (`c_chmask`, `c_srate=44100`, `c_ssize=3`,
   `p_chmask`, `p_srate=44100`, `p_ssize=3`, channel masks matching what the
   NS7II reports) and `f_midi` (`in_ports=1`, `out_ports=1`, `id="Numark NS7II"`).
   A ready script is sketched below. The gadget's ALSA card appears on the
   box as `UAC2Gadget`; audio written to its playback device goes **to the
   computer** as capture, and audio the computer plays arrives on the
   gadget's capture device.
2. **ns7iid run** bridges: NS7II capture → `hw:UAC2Gadget` playback;
   `hw:UAC2Gadget` capture → NS7II playback; NS7II MIDI ⇄ `f_midi` through the
   ALSA sequencer (`aconnect` the ports, or let the daemon auto-connect by name).
   If the NS7II's MIDI turns out to be a class interface, the box must claim it
   itself (`--take-class-midi`) so the kernel does not expose it twice.
3. **Clocking**: the NS7II is the master clock; the UAC2 gadget runs
   asynchronous mode with explicit feedback (`f_uac2` supports `c_sync=async`
   and feedback endpoints since Linux 5.14), so the computer follows the
   NS7II's clock and no resampler is needed. Latency budget: NS7II USB
   (≈2 ms) + bridge buffers (ns7iid keeps ≈20 ms queued by default; tune
   to 5 ms once stable) + computer's UAC2 driver (3–10 ms). Expect 15–35 ms
   round trip; Serato/Mixxx monitoring goes through the NS7II's hardware
   mixer, so this latency only affects deck audio start and cue-point
   scratch feel, which is why a tuned 5 ms bridge buffer is worth it.
4. **Serato DJ** will not run on the bridged device (it checks for the NS7II
   by USB ID), but Mixxx, Traktor, VirtualDJ, Rekordbox and djay will treat
   it as a generic MIDI controller with a multichannel sound card. Mixxx
   already has an NS7II controller mapping for the MIDI side.

### Gadget script (sketch, run as root on the box)

```sh
modprobe libcomposite
cd /sys/kernel/config/usb_gadget && mkdir ns7ii && cd ns7ii
echo 0x1d6b > idVendor; echo 0x0104 > idProduct   # Linux Foundation composite
mkdir -p strings/0x409; echo "Numark NS7II bridge" > strings/0x409/product
mkdir -p configs/c.1/strings/0x409; echo "UAC2+MIDI" > configs/c.1/strings/0x409/configuration
mkdir -p functions/uac2.0
echo 44100 > functions/uac2.0/c_srate; echo 3 > functions/uac2.0/c_ssize; echo 0xfff > functions/uac2.0/c_chmask   # 12 ch to computer
echo 44100 > functions/uac2.0/p_srate; echo 3 > functions/uac2.0/p_ssize; echo 0xf   > functions/uac2.0/p_chmask   # 4 ch from computer
echo async > functions/uac2.0/c_sync 2>/dev/null || true
mkdir -p functions/midi.0; echo "Numark NS7II" > functions/midi.0/id
ln -s functions/uac2.0 configs/c.1/; ln -s functions/midi.0 configs/c.1/
ls /sys/class/udc | head -1 > UDC
```

Then `ns7iid run --pcm-in hw:UAC2Gadget,0 --pcm-out hw:UAC2Gadget,0` and
`aconnect` the sequencer ports to `f_midi`.

## Why not just a hub or a "USB 32→64 adapter"

There is no such thing as a 32-bit USB signal; the data on the cable is the
same whatever the host's word size. The box works because it moves the
proprietary driver into a computer that can still run it (a Linux SBC with
this driver) and presents a standard device on the other side.

## Alternative: no box at all

On Linux, `ns7iid run` with the `snd-aloop` loopback card gives apps the same
thing without extra hardware. On macOS, Ozzy shows that a userspace
CoreAudio/CoreMIDI driver for Ploytec hardware is feasible; porting
`ns7ii-proto` + a libusb/IOKit transport there would be the next project.
On Windows, a WinUSB + ASIO user-mode driver is the equivalent route.
