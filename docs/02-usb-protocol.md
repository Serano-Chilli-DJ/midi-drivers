# NS7II USB protocol, as reconstructed

Confidence tags: **confirmed** = read from the NS7II driver's own code or
data; **family** = established for other Ploytec devices on code paths the
NS7II also runs; **assumed** = default to be checked on hardware. Nothing
here has been verified against a live NS7II yet.

## Identity

* VID `0x15E4` (Numark), PID `0x002C`. **confirmed**
* The kext's `start()` refuses every other VID/PID ("DEVICE_SUPPORTED_4"). **confirmed**
* The driver places the NS7II in `isPloytecBulkInIsocOutDevice` together
  with Numark PIDs 0x26, 0x30, 0x40, 0x70..0x79 and Alesis 0x13B2:0x0030: **confirmed**
  * playback = isochronous OUT, with an isochronous IN feedback endpoint,
  * capture = bulk (or interrupt) IN,
  * raw MIDI = bulk IN / bulk OUT pipes (`USBMidiDevice`), unless the
    device exposes a class MIDIStreaming interface, which the driver also
    knows how to bind (`findMidiInterface` looks for class 1 / subclass 3).

## Descriptor layout (**confirmed** by `ns7iid probe` on real hardware, 2026-10-15)

```
Device: USB 2.00, class FF/FF/FF, bcdDevice 0x0200, high speed (480 Mbit/s), self-powered
Configuration 1
  Interface 0  class FF  alt 0: no endpoints
               class FF  alt 1: EP 0x02 OUT isochronous, wMaxPacketSize 156, bInterval 1, asynchronous  (playback)
                                EP 0x83 IN  bulk 512                                                     (raw MIDI in)
                                EP 0x04 OUT bulk 512                                                     (raw MIDI out)
  Interface 1  class FF  alt 0: no endpoints
               class FF  alt 1: EP 0x81 IN  isochronous, wMaxPacketSize 64, bInterval 4 (1 ms)            (feedback/status, format open)
                                EP 0x86 IN  bulk 512, bInterval 1                                         (capture)
```

Raw: `12 01 00 02 ff ff ff 40 e4 15 2c 00 00 02 01 02 03 01 09 02 50 00 02 01 00 40 00 09 04 00 00 00 ff 00 00 00 09 04 00 01 03 ff 00 00 00 07 05 02 05 9c 00 01 07 05 83 02 00 02 04 07 05 04 02 00 02 04 09 04 01 00 00 ff 00 00 00 09 04 01 01 02 ff 00 00 00 07 05 81 05 40 00 04 07 05 86 02 00 02 01`

No class MIDI interface, so MIDI goes through the driver. 156 bytes per
microframe is 13 frames of 4 channels x 3 bytes: the playback endpoint is
sized for 4 channels of 24-bit at up to 96 kHz, which supports the plain
24-bit 4-output conclusion below. The 64-byte isochronous IN endpoint is
larger than a UAC feedback endpoint (3-4 bytes); its content is still to be
captured (`RUST_LOG=debug` logs it).

## Control requests (endpoint 0)

| Step | Request | Source | Tag |
|------|---------|--------|-----|
| Firmware version | `C0 56 wValue 0 wIndex 0 wLength 8`; reply `b0=chip id`, `b1=minor (decimal, tens/ones split)`, `b2=major` | kext `scanFirmwareVersion` | confirmed |
| Channel topology | same request with `wLength 5`; `in = (b2 & 0x7F)*2`, `out = (b3 & 0x7F)*2`; sent for PID 0x2C | kext `scanFirmwareVersion` | confirmed |
| Status read | `C0 49 wValue 0 wIndex idx wLength 1` | Ozzy, Jockey 3 | family |
| Status write | `40 49 wValue byte wIndex idx` (byte sign-extended) | Ozzy | family |
| Arm streaming | set bit 0x20 of status[0] if clear | Jockey 3, Ozzy | family, may be unnecessary |
| Set rate | `22 01 wValue 0x0100 wIndex <ep> data 3 bytes LE` to the capture EP, then the playback EP | kext `setFrequency` (one SET_CUR per call, in-pipe first) | confirmed |
| Get rate | `A2 81 wValue 0x0100 wIndex <ep> wLength 3` | family | family |
| `40 42` | sent once in `startStreaming` on some devices | kext | assumed, not sent by default |

Sample rate: 44 100 Hz only, 24-bit, per the official table. The device
accepts `SET_CUR` for other rates on the Xone family but plays at the wrong
speed; the driver only offers 44.1 kHz.

## Channel counts

The official table says 12 capture lanes and 4 playback lanes at high speed
(2/2 at full speed). The second 0x56 reply is authoritative at run time; the
driver uses it and falls back to 12/4.

Playback pairs named by the vendor: Master OUT, OUT 1..4 (five pairs, which
does not fit "4 lanes" unless some pairs are mixed in hardware; another
reason to trust the device's own reply).

## Audio wire format

* The kext chooses the PCM converters in `chooseISOOutEncoder` /
  `configureDevice` by VID/PID. For Numark it falls through every special
  case to the defaults: `pcmTo24LSB` (host → device) and `pcmFrom24LSB`
  (device → host). Those are **plain interleaved 24-bit little-endian
  samples** (`S24_3LE`), channel 0 first. **confirmed** by control flow.
* The bit-plane "junction" codec (48-byte out / 64-byte in frames) that
  Ozzy and the Jockey 3 driver implement is used only for devices in
  `isPloytecBulkDevice` (Allen & Heath, Reloop, ESU...). The NS7II is not in
  that list. The driver still ships that codec behind `--codec junction` in
  case hardware proves otherwise.
* Isochronous OUT packets carry whole frames; the host paces them at
  rate / 8000 frames per microframe (high speed) or rate / 1000 per frame
  (full speed) and corrects from the feedback endpoint (UAC-style 10.14 or
  16.16 value) when one exists. **assumed**: the kext's `isocReadFeedback`
  path exists for this family but the feedback format was not decoded.
* Bulk IN capture: a stream of frames with no header. **assumed** (default
  `pcmFrom24LSB`; the Xone's MIDI/control bytes are *not* interleaved on the
  Numark path because `isPloytecBulkDevice` is false).

## MIDI

* Raw pipes carry plain MIDI bytes. Filler bytes `FD` (and `F9`, `FF` on
  other Ploytec firmware) are stripped on input. **family**
* Ploytec firmware ignores running status, so the driver expands it before
  sending. **family**
* If the device has a class-compliant MIDIStreaming interface, Linux's
  `snd-usb-audio` already provides it and the daemon leaves it alone unless
  `--take-class-midi` is given (the bridge box needs that).
* Firmware updates for the control surface go over MIDI SysEx on this port
  (see 01). Serato's SysEx for platter/strip displays goes the same way.

## What the Windows driver adds

Nothing protocol-wise; same engine. Its strings confirm the mixed
isochronous-out / bulk-in service loop (`MIXED PROCESSING ISOC OUT and BULK
IN`) for this device.
