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
24-bit 4-output conclusion below. The 64-byte isochronous IN endpoint
carries 3-byte feedback packets (see "Playback feedback" below).

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
* Isochronous OUT packets carry whole frames, sized from the feedback
  endpoint as described in "Playback feedback". **kext-decoded**
* Bulk IN capture: a stream of frames with no header. **assumed** (default
  `pcmFrom24LSB`; the Xone's MIDI/control bytes are *not* interleaved on the
  Numark path because `isPloytecBulkDevice` is false).

## Playback feedback (EP 0x81) **kext-decoded 2026-10-17, byte 0 to confirm on hardware**

From the 3.3.17 kext (`isocReadFeedbackCompleteStatic` 0x1a6e6,
`isocFeedbackReadCompleteFullFrame` 0x1afaa, `PGFramePatternObserver`,
`PGIsocRB::initWrite_Std` 0x1b808, `rtsIsocAudioRun` 0x1796e):

* Every 1 ms (bInterval 4 at high speed) the unit sends a **3-byte** packet.
  **Byte 0 = frames the unit played in that millisecond** (42..46 at
  44.1 kHz; the kext accepts `rate/1000 ± 2`). Bytes 1-2 are ignored by the
  kext. It is not a UAC 10.14/16.16 value. Other lengths are counted; after
  40 of them the kext stops streaming.
* The kext keeps a 16 ms ring of frames-per-OUT-microframe (128 slots). Each
  millisecond's count is spread over its 8 microframes (44 -> 6,5,6,5,6,5,6,5;
  45 -> 6,6,5,6,5,6,5,6) and the OUT stream reads one slot per packet, so it
  replays the unit's own frame counts 16 ms later. An out-of-range count
  ("FramePattern Lost Sync") plays the nominal pattern for that ms; 16 good
  ms in a row is "Locked".
* OUT and feedback start on the same USB frame; OUT URBs are 32 microframes
  (4 ms), feedback URBs 4 packets (4 ms); 3 OUT and 2 feedback URBs at first.
* `ns7iid` does the same (`ns7ii-usb/src/feedback.rs`), except that it places
  the write position half a ring ahead of the read position when feedback
  starts (usbfs cannot start both streams on one frame) and plays an exact
  rational nominal rate for slots no feedback refreshed. The journal shows
  `feedback EP 81: first packet [...]` and `feedback locked` at start-up.

## MIDI

* Raw pipes carry plain MIDI bytes. Filler bytes `FD` (and `F9`, `FF` on
  other Ploytec firmware) are stripped on input. **family**
* Ploytec firmware ignores running status, so the driver expands it before
  sending. **family**
* MIDI OUT transfers on the NS7II are 42 bytes: MIDI bytes first (at most
  39-40 per transfer, kext table `USBMidiPattern_NS7`), `FD` filler, and
  `E0` as the last byte (kext `AJ::configurationDone` picks 42 bytes and
  the NS7 pattern for device model 8; `AJ::fireOutputInternal` fills the
  frame). MIDI IN transfers are 42 bytes too (seen on hardware). A bare
  3-byte bulk write completes but the unit ignores it (seen on hardware).
  The output pipe is only serviced while the audio streams run
  (`AJ::streamingStarted`). **kext; LED test pending**
* MIDI OUT pacing in the kext (`AJ::fireOutput` 0x3504,
  `AJ::outAsyncCompletionRoutine` 0x372c, `AJ::sendRB` 0x366a): event
  driven, one 42-byte transfer in flight, the next one queued on completion,
  nothing sent while idle, 500 ms timeouts, no retry. `initNS7` sets all 256
  pattern entries to 39 MIDI bytes per transfer. `ns7iid` sends from a
  bounded queue that coalesces messages for the same note / controller
  (latest value wins), paced to 8000 B/s by default (`--midi-out-rate`),
  with 500 ms timeouts that halve the rate and clear the endpoint halt; a
  run of 8 timeouts or a protocol error resets the device. **kext**
* If the device has a class-compliant MIDIStreaming interface, Linux's
  `snd-usb-audio` already provides it and the daemon leaves it alone unless
  `--take-class-midi` is given (the bridge box needs that).
* Firmware updates for the control surface go over MIDI SysEx on this port
  (see 01). Serato's SysEx for platter/strip displays goes the same way.

## What the Windows driver adds

Nothing protocol-wise; same engine. Its strings confirm the mixed
isochronous-out / bulk-in service loop (`MIXED PROCESSING ISOC OUT and BULK
IN`) for this device.
