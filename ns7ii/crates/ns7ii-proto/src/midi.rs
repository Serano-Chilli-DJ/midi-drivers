//! MIDI byte-stream handling for the Ploytec raw-MIDI pipes and for
//! USB-MIDI class event packets.
//!
//! The kext's `USBMidiDevice` reads and writes **plain MIDI bytes** over bulk
//! pipes (`read`: one `wMaxPacketSize` transfer at a time; `write`: one
//! transfer per message, 1 s timeout). On the Xone family the firmware pads
//! those transfers with `0xFD` (an undefined real-time status) and appends a
//! control byte `0xFF`; `MidiIO::putFrame` drops every `0xFD`. The Reloop
//! driver additionally drops `0xF9`. Both are filtered here. **family**.
//!
//! Ploytec firmware does not understand MIDI running status (Reloop, Ozzy
//! reports), while the ALSA sequencer emits it by default, so outgoing
//! streams are passed through [`RunningStatusExpander`]. **family**.

/// Idle filler byte the firmware inserts into MIDI IN transfers.
pub const IDLE_BYTE: u8 = 0xFD;
/// Second filler byte seen on Reloop hardware (MIDI tick, never used by controllers).
pub const TICK_BYTE: u8 = 0xF9;
/// Control byte that follows a MIDI slot on the Xone family.
pub const CONTROL_IDLE: u8 = 0xFF;

/// Strip firmware filler from a MIDI IN transfer.
///
/// `0xFF` is a legitimate MIDI System Reset, but no DJ controller sends it;
/// the Xone firmware uses it as a control byte, so it is dropped as well.
pub fn strip_filler(raw: &[u8], out: &mut Vec<u8>) {
    out.extend(raw.iter().copied().filter(|b| !matches!(*b, IDLE_BYTE | TICK_BYTE | CONTROL_IDLE)));
}

/// Expand MIDI running status so every channel-voice message carries its
/// status byte. Real-time bytes pass through untouched and do not disturb
/// the state machine; SysEx is passed through verbatim.
#[derive(Debug, Default, Clone)]
pub struct RunningStatusExpander {
    status: Option<u8>,
    data_needed: u8,
    data_count: u8,
    in_sysex: bool,
}

impl RunningStatusExpander {
    /// Create a fresh expander.
    pub fn new() -> Self {
        Self::default()
    }

    /// Feed one byte; appends zero or more bytes to `out`.
    pub fn push(&mut self, byte: u8, out: &mut Vec<u8>) {
        if byte >= 0xF8 {
            out.push(byte);
            return;
        }
        if byte == 0xF0 {
            self.in_sysex = true;
            self.status = None;
            out.push(byte);
            return;
        }
        if self.in_sysex {
            out.push(byte);
            if byte == 0xF7 {
                self.in_sysex = false;
            }
            return;
        }
        if byte & 0x80 != 0 {
            if (0x80..0xF0).contains(&byte) {
                self.status = Some(byte);
                self.data_needed = data_bytes_for(byte);
                self.data_count = 0;
            } else {
                // System common: clears running status.
                self.status = None;
                self.data_needed = match byte {
                    0xF1 | 0xF3 => 1,
                    0xF2 => 2,
                    _ => 0,
                };
                self.data_count = 0;
            }
            out.push(byte);
            return;
        }
        // Data byte.
        match self.status {
            Some(status) => {
                if self.data_count == 0 && self.data_needed > 0 && out.last() != Some(&status) {
                    // Running status in effect: re-insert the status byte,
                    // except when we just emitted it.
                    out.push(status);
                }
                out.push(byte);
                self.data_count += 1;
                if self.data_count >= self.data_needed {
                    self.data_count = 0;
                }
            }
            None => {
                // Data byte for a system-common message or stray data: pass through.
                out.push(byte);
            }
        }
    }

    /// Expand a whole buffer.
    pub fn expand(&mut self, input: &[u8]) -> Vec<u8> {
        let mut out = Vec::with_capacity(input.len() + input.len() / 2);
        for &b in input {
            self.push(b, &mut out);
        }
        out
    }
}

/// A parsed MIDI message (channel-voice, system common or real-time).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Message {
    /// Status byte including channel (e.g. `0x90`).
    pub status: u8,
    /// Data bytes (0 to 2 for short messages; whole body for SysEx).
    pub data: Vec<u8>,
}

impl Message {
    /// MIDI channel (0-based) for channel-voice messages.
    pub fn channel(&self) -> u8 {
        self.status & 0x0F
    }
    /// Message kind (status with channel masked off).
    pub fn kind(&self) -> u8 {
        if self.status >= 0xF0 { self.status } else { self.status & 0xF0 }
    }
    /// First data byte (note or controller number), if any.
    pub fn d1(&self) -> Option<u8> {
        self.data.first().copied()
    }
    /// Second data byte (velocity or value), if any.
    pub fn d2(&self) -> Option<u8> {
        self.data.get(1).copied()
    }
}

/// Incremental MIDI byte-stream parser with running status.
#[derive(Debug, Default, Clone)]
pub struct Parser {
    status: Option<u8>,
    buf: Vec<u8>,
    need: usize,
    in_sysex: bool,
}

impl Parser {
    /// Fresh parser.
    pub fn new() -> Self {
        Self::default()
    }

    /// Feed one byte; returns a message when one completes.
    pub fn push(&mut self, byte: u8) -> Option<Message> {
        if byte >= 0xF8 {
            return Some(Message { status: byte, data: Vec::new() });
        }
        if self.in_sysex {
            if byte == 0xF7 {
                self.in_sysex = false;
                let data = std::mem::take(&mut self.buf);
                return Some(Message { status: 0xF0, data });
            }
            if byte & 0x80 == 0 {
                self.buf.push(byte);
            } else {
                // Interrupted SysEx: drop it and process the new status.
                self.in_sysex = false;
                self.buf.clear();
                return self.push(byte);
            }
            return None;
        }
        if byte == 0xF0 {
            self.in_sysex = true;
            self.buf.clear();
            return None;
        }
        if byte & 0x80 != 0 {
            self.buf.clear();
            let need = match byte {
                0x80..=0xEF => data_bytes_for(byte) as usize,
                0xF1 | 0xF3 => 1,
                0xF2 => 2,
                _ => 0,
            };
            if need == 0 {
                self.status = None;
                return Some(Message { status: byte, data: Vec::new() });
            }
            self.status = Some(byte);
            self.need = need;
            return None;
        }
        let status = self.status?;
        self.buf.push(byte);
        if self.buf.len() >= self.need {
            let data = std::mem::take(&mut self.buf);
            return Some(Message { status, data });
        }
        None
    }

    /// Parse a whole buffer.
    pub fn parse(&mut self, bytes: &[u8]) -> Vec<Message> {
        bytes.iter().filter_map(|b| self.push(*b)).collect()
    }
}

/// Number of data bytes that follow a channel-voice status byte.
pub fn data_bytes_for(status: u8) -> u8 {
    match status & 0xF0 {
        0xC0 | 0xD0 => 1,
        _ => 2,
    }
}

/// USB-MIDI 1.0 class event packets (4 bytes: cable/CIN + up to 3 MIDI bytes).
/// Used when the NS7II's MIDI port is a class MIDIStreaming interface and the
/// kernel driver is deliberately detached (the converter box does that).
pub mod usb_midi {
    use super::data_bytes_for;

    /// Encoder from a MIDI byte stream into 4-byte event packets.
    #[derive(Debug, Default, Clone)]
    pub struct Encoder {
        msg: [u8; 3],
        len: u8,
        need: u8,
        status: Option<u8>,
        sysex: bool,
    }

    impl Encoder {
        /// Fresh encoder.
        pub fn new() -> Self {
            Self::default()
        }

        /// Feed one byte. Returns a packet when one completes.
        pub fn push(&mut self, cable: u8, byte: u8) -> Option<[u8; 4]> {
            let cn = (cable & 0x0F) << 4;
            if byte >= 0xF8 {
                return Some([cn | 0xF, byte, 0, 0]);
            }
            if byte == 0xF0 {
                self.sysex = true;
                self.msg = [byte, 0, 0];
                self.len = 1;
                return None;
            }
            if self.sysex {
                self.msg[self.len as usize] = byte;
                self.len += 1;
                if byte == 0xF7 {
                    self.sysex = false;
                    let cin = match self.len {
                        1 => 0x5,
                        2 => 0x6,
                        _ => 0x7,
                    };
                    let pkt = [cn | cin, self.msg[0], self.msg[1], self.msg[2]];
                    self.len = 0;
                    self.msg = [0; 3];
                    return Some(pkt);
                }
                if self.len == 3 {
                    let pkt = [cn | 0x4, self.msg[0], self.msg[1], self.msg[2]];
                    self.len = 0;
                    self.msg = [0; 3];
                    return Some(pkt);
                }
                return None;
            }
            if byte & 0x80 != 0 {
                self.msg = [byte, 0, 0];
                self.len = 1;
                if (0x80..0xF0).contains(&byte) {
                    self.status = Some(byte);
                    self.need = 1 + data_bytes_for(byte);
                } else {
                    self.status = None;
                    self.need = match byte {
                        0xF1 | 0xF3 => 2,
                        0xF2 => 3,
                        _ => 1,
                    };
                }
            } else {
                if self.len == 0 {
                    match self.status {
                        Some(s) => {
                            self.msg = [s, 0, 0];
                            self.len = 1;
                            self.need = 1 + data_bytes_for(s);
                        }
                        None => return None,
                    }
                }
                self.msg[self.len as usize] = byte;
                self.len += 1;
            }
            if self.len >= self.need {
                let status = self.msg[0];
                let cin = match status {
                    0x80..=0xEF => status >> 4,
                    0xF1 | 0xF3 => 0x2,
                    0xF2 => 0x3,
                    _ => 0x5,
                };
                let pkt = [cn | cin, self.msg[0], self.msg[1], self.msg[2]];
                self.len = 0;
                return Some(pkt);
            }
            None
        }
    }

    /// Decode one event packet into its MIDI bytes. Returns the byte count (0..=3).
    pub fn decode(packet: [u8; 4], out: &mut [u8; 3]) -> usize {
        let cin = packet[0] & 0x0F;
        let n = match cin {
            0x0 | 0x1 => 0,
            0x2 | 0x6 | 0xC | 0xD => 2,
            0x5 | 0xF => 1,
            _ => 3,
        };
        out[..n].copy_from_slice(&packet[1..1 + n]);
        n
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn filler_is_stripped() {
        let mut out = Vec::new();
        strip_filler(&[0xFD, 0xFD, 0xFD, 0xB0, 0xFF], &mut out);
        strip_filler(&[0xFD, 0xFD, 0xB0, 0x25, 0xFF], &mut out);
        assert_eq!(out, vec![0xB0, 0xB0, 0x25]);
    }

    #[test]
    fn running_status_is_expanded() {
        let mut x = RunningStatusExpander::new();
        let out = x.expand(&[0x90, 0x3C, 0x7F, 0x3E, 0x7F, 0x40, 0x00]);
        assert_eq!(out, vec![0x90, 0x3C, 0x7F, 0x90, 0x3E, 0x7F, 0x90, 0x40, 0x00]);
    }

    #[test]
    fn realtime_does_not_break_running_status() {
        let mut x = RunningStatusExpander::new();
        let out = x.expand(&[0xB0, 0x07, 0xF8, 0x64, 0x08, 0x65]);
        assert_eq!(out, vec![0xB0, 0x07, 0xF8, 0x64, 0xB0, 0x08, 0x65]);
    }

    #[test]
    fn sysex_passes_through() {
        let mut x = RunningStatusExpander::new();
        let msg = [0xF0, 0x00, 0x01, 0x3F, 0x12, 0xF7, 0x90, 0x01, 0x02];
        assert_eq!(x.expand(&msg), msg.to_vec());
    }

    #[test]
    fn parser_handles_running_status_and_realtime() {
        let mut p = Parser::new();
        let msgs = p.parse(&[0x90, 0x3C, 0x7F, 0xF8, 0x3E, 0x00, 0xB1, 0x07, 0x40]);
        assert_eq!(msgs.len(), 4);
        assert_eq!(msgs[0], Message { status: 0x90, data: vec![0x3C, 0x7F] });
        assert_eq!(msgs[1].status, 0xF8);
        assert_eq!(msgs[2], Message { status: 0x90, data: vec![0x3E, 0x00] });
        assert_eq!(msgs[3].channel(), 1);
        assert_eq!(msgs[3].kind(), 0xB0);
    }

    #[test]
    fn usb_midi_note_on_packet() {
        let mut e = usb_midi::Encoder::new();
        assert_eq!(e.push(0, 0x90), None);
        assert_eq!(e.push(0, 0x3C), None);
        assert_eq!(e.push(0, 0x7F), Some([0x09, 0x90, 0x3C, 0x7F]));
        // running status
        assert_eq!(e.push(0, 0x3E), None);
        assert_eq!(e.push(0, 0x00), Some([0x09, 0x90, 0x3E, 0x00]));
    }

    #[test]
    fn usb_midi_sysex_packets() {
        let mut e = usb_midi::Encoder::new();
        let bytes = [0xF0, 0x7E, 0x7F, 0x06, 0x01, 0xF7];
        let pkts: Vec<_> = bytes.iter().filter_map(|b| e.push(1, *b)).collect();
        assert_eq!(pkts, vec![[0x14, 0xF0, 0x7E, 0x7F], [0x17, 0x06, 0x01, 0xF7]]);
        let mut out = [0u8; 3];
        assert_eq!(usb_midi::decode(pkts[1], &mut out), 3);
        assert_eq!(out, [0x06, 0x01, 0xF7]);
    }

    #[test]
    fn usb_midi_realtime_single_byte() {
        let mut e = usb_midi::Encoder::new();
        assert_eq!(e.push(0, 0xF8), Some([0x0F, 0xF8, 0, 0]));
        let mut out = [0u8; 3];
        assert_eq!(usb_midi::decode([0x0F, 0xF8, 0, 0], &mut out), 1);
    }
}
