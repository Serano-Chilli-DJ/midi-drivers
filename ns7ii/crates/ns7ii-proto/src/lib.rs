//! Numark NS7II USB protocol, reconstructed from the vendor drivers.
//!
//! The NS7II is not a USB Audio Class device. Its audio/MIDI chip runs the
//! Ploytec GmbH ("usb-audio.de") firmware, and the Windows/macOS drivers
//! Numark shipped are Ploytec's generic driver with a Numark skin. Everything
//! in this crate was derived from:
//!
//! * the unstripped macOS kext `NumarkNS7IIAudio.kext` 3.3.17 (full C++ symbols),
//! * the Windows driver 2.9.55 (`ns7ii_usb.sys`, INF files),
//! * the open-source Ozzy driver for Allen & Heath Xone mixers (same chipset),
//! * the Linux kernel patch series for the Reloop Jockey 3 (same chipset).
//!
//! Each item carries a confidence tag in its doc comment:
//!
//! * **confirmed** – read directly from the NS7II driver's code or data,
//! * **family** – known for other Ploytec devices and used by shared code
//!   paths that the NS7II driver also executes,
//! * **assumed** – a reasonable default that must be checked on hardware.
//!
//! This crate performs no I/O, so it can be unit tested anywhere.

#![forbid(unsafe_code)]
#![warn(missing_docs)]

pub mod midi;
pub mod pcm;
pub mod requests;

/// USB vendor ID of Numark (inMusic). **confirmed** (INF files, kext plist).
pub const VENDOR_ID: u16 = 0x15E4;
/// USB product ID of the NS7II. **confirmed** (INF files, kext plist).
pub const PRODUCT_ID: u16 = 0x002C;

/// Sample rates the official driver's table (kext `kConfigurationData`)
/// lists for the NS7II: 44.1 kHz only, 24-bit, for every stream.
/// **confirmed**.
pub const SUPPORTED_RATES: &[u32] = &[44_100];

/// Bit depth of every stream in the official table. **confirmed**.
pub const BITS_PER_SAMPLE: u32 = 24;

/// Bytes per sample on the wire (S24_3LE). **confirmed** for the plain
/// `pcmTo24LSB` / `pcmFrom24LSB` converters the kext selects for this VID.
pub const BYTES_PER_SAMPLE: usize = 3;

/// Stream layout the official driver advertises for the NS7II
/// (`kConfigurationData`, four `APIStreamingInfo` records). **confirmed**.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct StreamTable {
    /// Capture channels when enumerated at USB full speed.
    pub fs_in_channels: u8,
    /// Playback channels when enumerated at USB full speed.
    pub fs_out_channels: u8,
    /// Capture channels at USB high speed.
    pub hs_in_channels: u8,
    /// Playback channels at USB high speed.
    pub hs_out_channels: u8,
}

/// The table as shipped in the macOS kext: records are
/// `[24-bit 2ch 44100] [24-bit 2ch 44100] [24-bit 12ch 44100] [24-bit 4ch 44100]`.
///
/// Ozzy's analysis of the sibling Xone kexts established the record order
/// FS-in, FS-out, HS-in, HS-out. That gives 12 capture lanes and 4 playback
/// lanes at high speed, which is odd for a 4-deck controller (the 4 "decks"
/// are the inputs *to* the computer on an NS7II: the mixer is in hardware
/// and the computer only plays back). The channel counts the device itself
/// reports through [`requests::FirmwareInfo`] take precedence at run time.
pub const OFFICIAL_STREAM_TABLE: StreamTable = StreamTable {
    fs_in_channels: 2,
    fs_out_channels: 2,
    hs_in_channels: 12,
    hs_out_channels: 4,
};

/// Output pair names from the kext plist (`kOutChannelNames`). **confirmed**.
pub const OUTPUT_PAIR_NAMES: &[&str] = &["Master OUT", "OUT 1", "OUT 2", "OUT 3", "OUT 4"];

/// Which USB transfer type each stream uses. The kext places the NS7II in
/// its `isPloytecBulkInIsocOutDevice` family. **confirmed**.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Transport {
    /// Playback: isochronous OUT endpoint, with an isochronous IN feedback
    /// endpoint when the device exposes one.
    IsochronousOut,
    /// Capture and raw MIDI: bulk (or interrupt) endpoints.
    BulkIn,
}

/// Interface classes the official driver looks for while configuring the
/// device (`PGConfigurationDescriptor::find*Interface`). **confirmed**.
pub mod classes {
    /// Vendor-specific interface: carries the Ploytec audio endpoints.
    pub const VENDOR: u8 = 0xFF;
    /// USB Audio class.
    pub const AUDIO: u8 = 0x01;
    /// Audio class, AudioControl subclass.
    pub const AUDIO_CONTROL_SUBCLASS: u8 = 0x01;
    /// Audio class, MIDIStreaming subclass. The driver binds a MIDI port to
    /// it; on Linux `snd-usb-audio` does the same without any extra driver.
    pub const MIDI_STREAMING_SUBCLASS: u8 = 0x03;
    /// HID class (used by some Ploytec hosts for control surfaces).
    pub const HID: u8 = 0x03;
}
