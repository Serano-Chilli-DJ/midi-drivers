# drivers

User-space drivers and reverse-engineering notes for hardware that its
vendor stopped supporting. One directory per device, each self-contained
with its own build, docs and installer.

| Directory | Device | What |
|-----------|--------|------|
| [`ns7ii/`](ns7ii/) | Numark NS7II DJ controller (USB 15e4:002c) | Linux driver in Rust (MIDI + audio bridge), USB protocol notes, the MIDI chart, and a design for a class-compliant bridge box |

Each driver's README covers building, installing and verifying it on
hardware. The vendor packages a driver was reconstructed from are kept
untracked under its `tmp/`.
