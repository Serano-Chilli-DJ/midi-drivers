# Numark NS7II MIDI chart

Numark never published this. The table was assembled from a capture posted
on a DJ forum by SuperC0C0 (2022), checked message by message against a
`ns7iid midi-learn` capture of a real NS7II (2026-10-15) and against the
control-surface firmware's MIDI input handlers (`ns7ii_firmware_updater.exe`
image, see `docs/01-vendor-package-analysis.md`). It is built into the
driver (`crates/ns7iid/src/chart.rs`); `ns7iid chart` prints it as JSON for
building a mapping in whatever software drives the unit.

Notation: `9n 34` means note 0x34 on MIDI channel n (status 0x90 + n);
buttons send velocity 0x7F on press and a note-off (`8n`) on release.

## Channels

| Channel | Used for |
|---|---|
| 0 (`90`/`B0`) | mixer, FX section, library, front and rear panel switches |
| 1 | deck 1 (left side, layer 1) |
| 2 | deck 2 (right side, layer 1) |
| 3 | deck 3 (left side, layer 2, after its DECK button) |
| 4 | deck 4 (right side, layer 2) |

The DECK buttons change which channel a side uses; the firmware also only
lights a side's LEDs for messages on its current layer's channel.

## Deck (channel n = deck number)

| Control | Message | Notes |
|---|---|---|
| Platter | `Bn 00` value | 7-bit position counter, wraps; `En` pitch bend sent alongside |
| Pitch fader | `Bn 01` MSB, `Bn 21` LSB | 14-bit |
| Strip Search | `Bn 02` | 7-bit absolute |
| Stop Time knob | `Bn 0C` | |
| Start Time knob | `Bn 0D` | |
| Load | `9n 31` | LOAD A / LOAD B, on the side's current deck channel |
| Sync | `9n 32` | |
| Cue | `9n 33` | |
| Play / Pause | `9n 34` | |
| Shift | `9n 35` | |
| Hot Cue 1–5 | `9n 36`–`9n 3A` | dedicated row |
| Pitch Bend − / + | `9n 3B` / `9n 3C` | |
| Range / Master Tempo | `9n 3D` | |
| Bleep | `9n 3E` | switch position |
| Reverse | `9n 3F` | switch position |
| Tap | `9n 41` | |
| Adjust / Set | `9n 42` | beat grid |
| Slip / Clear | `9n 43` | beat grid |
| Slip Mode / Motor Off | `9n 44` | |
| Parameter < / > | `9n 45` / `9n 46` | |
| Pads 1–8 | `9n 47`–`9n 4E` | plus channel pressure `A0 47`–`A0 4E` |
| Pad mode Cues | `9n 4F` | |
| Pad mode Auto / Roll | `9n 50` | |
| Pad mode Loop | `9n 51` | |
| Pad mode Sampler | `9n 52` | |
| Pad mode Slicer | `9n 53` | |

### Motor and LEDs (host to device, channel n)

Decoded from the firmware's deck-channel controller handler; the numbers
are the same ones the Numark V7 and NS7 use on channel 0.

| Message | Effect |
|---|---|
| `Bn 43 00` | start platter motor |
| `Bn 44 00` | stop platter motor |
| `Bn 45 v` | 0 = 33⅓ rpm, 1 = 45 rpm |
| `Bn 46 v` | 1 = reverse direction |
| `Bn 47 v` | start time |
| `Bn 48 v` | stop time |
| `Bn 69 hi`, `Bn 49 lo` | motor pitch, 14-bit (`hi << 7 | lo`) |
| `Bn 6A hi`, `Bn 4A lo` | second 14-bit value, purpose not confirmed |
| `9n xx 7F/00` | LED of the control with note `xx` (only on the side's current layer) |

## FX section (channel 0, one per side)

| Control | Left | Right |
|---|---|---|
| FX 1 / 2 / 3 buttons | `90 0F` / `10` / `11` | `90 13` / `14` / `15` |
| Beat / Mode button | `90 12` | `90 16` |
| FX 1 / 2 / 3 knobs | `B0 04` / `05` / `06` | `B0 49` / `4A` / `4B` |
| Beat knob | `B0 48` | `B0 4C` |
| Beat knob press | `90 35` | `90 36` |
| Knob touch (FX 1–3, Beat) | `90 64`–`67` | `90 68`–`6B` |
| Knob LED rings (host to device) | `B0 4E`–`51` | `B0 52`–`55` |

The knobs are endless encoders: clockwise `01`, anticlockwise `7F`.

## Mixer (channel 0)

Strips are numbered by their printed channel; left to right they sit as
3, 1, 2, 4. Knobs and faders are 14-bit (LSB number = MSB number + 0x20).

| Control | Ch 1 | Ch 2 | Ch 3 | Ch 4 |
|---|---|---|---|---|
| Gain | `B0 0C` | `B0 11` | `B0 17` | `B0 1C` |
| Treble | `B0 0B` | `B0 10` | `B0 16` | `B0 1B` |
| Mid | `B0 0A` | `B0 0F` | `B0 15` | `B0 1A` |
| Bass | `B0 09` | `B0 0E` | `B0 14` | `B0 19` |
| Filter | `B0 5B` | `B0 5C` | `B0 5A` | `B0 5D` |
| Fader | `B0 08` | `B0 0D` | `B0 13` | `B0 18` |
| PFL | `90 17` | `90 18` | `90 19` | `90 1A` |
| FX assign A / B | `90 23` / `24` | `90 25` / `26` | `90 27` / `28` | `90 29` / `2A` |
| Crossfader assign A / B | `90 1B` / `1C` | `90 1D` / `1E` | `90 1F` / `20` | `90 21` / `22` |
| Knob touch (bass, mid, treble, gain) | `90 58`–`5B` | `90 5C`–`5F` | `90 54`–`57` | `90 60`–`63` |
| Filter knob touch | `90 70` | `90 71` | `90 6F` | `90 72` |
| Input selector | `90/80 2D`, `31` | `2E`, `32` | `2F`, `33` | `30`, `34` |
| VU meter (host to device) | | | `B0 4C` | |

Crossfader `B0 07` / `B0 27`; crossfader slope `B0 41`.

## Master, library and panels (channel 0)

| Control | Message |
|---|---|
| Master volume | `B0 42` |
| Booth volume | `B0 43` |
| Headphone volume | `B0 44` |
| Cue blend | `B0 12` / `B0 32` |
| Split cue switch | `90 00` |
| Panel / View | `90 01` |
| Deck select 1 / 2 / 3 / 4 | `90 02` / `03` / `04` / `05` |
| Back / Fwd | `90 06` / `90 07` |
| Scroll knob press | `90 08` |
| Scroll knob turn | `B0 03` (clockwise `01`) |
| Scroll knob touch | `90 6C` |
| Prepare / Files / Crates | `90 09` / `0A` / `0B` |
| Load Prepare | `90 0C` |
| Touch Mode | `90 0D` |
| Filter Mode (Filter Roll / FX) | `90 0E` |
| FX send A / B (master) | `90 2B` / `90 2C` |
| Mic treble / bass / gain | `B0 45` / `46` / `47` |
| Mic 1 on/off switch | `90 6E` |
| Motor torque switch | `90 74` |
| BPM meter (host to device) | `B0 47`, value 0–11 |

## Marked silent in the built-in chart

Knob touch notes, channel pressure, input selectors, the touch mode and
filter mode buttons, the motor torque switch and the second 14-bit motor
value. They are in the built-in chart as silent controls: present with
their messages, flagged for a mapping to ignore unless it wants them.
