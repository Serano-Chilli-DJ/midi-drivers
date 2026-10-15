//! PCM sample packing.
//!
//! Two wire formats exist in the Ploytec family:
//!
//! * [`Plain24`]: interleaved 24-bit little-endian samples (`S24_3LE`).
//!   This is what the kext's default converters `pcmTo24LSB` / `pcmFrom24LSB`
//!   produce, and the kext selects those for the NS7II because the Numark VID
//!   is not in the `isPloytecBulkDevice` list that switches to the junction
//!   codecs. **confirmed** by control flow, **not yet verified** on hardware.
//! * [`Junction`]: the bit-plane interleaved 48-byte-out / 64-byte-in frames
//!   used by the Allen & Heath Xone mixers and Reloop Jockey. Included so the
//!   `record`/`play` tools can be switched over if the NS7II turns out to use
//!   it after all. **family**, verified by Ozzy on Xone hardware.
//!
//! Both codecs work on *frames* of interleaved `i32` samples where the 24
//! significant bits sit in the top of the word (`sample << 8`), the layout
//! the kext's converters consume (they copy bytes 1..=3 of each 32-bit word).

/// Output of a codec: how many bytes one frame occupies on the wire.
pub trait WireCodec {
    /// Bytes per frame sent to the device.
    fn out_frame_bytes(&self) -> usize;
    /// Bytes per frame received from the device.
    fn in_frame_bytes(&self) -> usize;
    /// Encode `frames` interleaved frames of `out_channels` samples into `dst`.
    /// Returns the number of bytes written.
    fn encode(&self, src: &[i32], dst: &mut [u8]) -> usize;
    /// Decode wire bytes into interleaved frames of `in_channels` samples.
    /// Returns the number of samples written.
    fn decode(&self, src: &[u8], dst: &mut [i32]) -> usize;
}

/// Plain `S24_3LE` interleaved packing.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Plain24 {
    /// Playback channels per frame.
    pub out_channels: usize,
    /// Capture channels per frame.
    pub in_channels: usize,
}

impl WireCodec for Plain24 {
    fn out_frame_bytes(&self) -> usize {
        self.out_channels * 3
    }

    fn in_frame_bytes(&self) -> usize {
        self.in_channels * 3
    }

    fn encode(&self, src: &[i32], dst: &mut [u8]) -> usize {
        let n = src.len().min(dst.len() / 3);
        for (s, d) in src.iter().zip(dst.chunks_exact_mut(3)).take(n) {
            let b = s.to_le_bytes();
            d.copy_from_slice(&b[1..4]);
        }
        n * 3
    }

    fn decode(&self, src: &[u8], dst: &mut [i32]) -> usize {
        let n = (src.len() / 3).min(dst.len());
        for (s, d) in src.chunks_exact(3).zip(dst.iter_mut()).take(n) {
            *d = i32::from_le_bytes([0, s[0], s[1], s[2]]);
        }
        n
    }
}

/// Ploytec "junction" bit-plane frames (Xone / Jockey wire format).
///
/// Output frame (48 bytes, up to 8 lanes): bytes `0x00..0x18` carry the odd
/// lanes, bytes `0x18..0x30` the even lanes. Within a block, byte `i` holds
/// sample bit `23 - i` of each lane in bit position `lane_pair_index`.
/// Input frame (64 bytes, up to 16 lanes): bytes `0x00..0x18` odd lanes,
/// `0x20..0x38` even lanes, same bit mapping; bytes `0x18..0x20` and
/// `0x38..0x40` are unused.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Junction {
    /// Playback lanes in use (2, 4, 6 or 8).
    pub out_channels: usize,
    /// Capture lanes in use (2..=16).
    pub in_channels: usize,
}

impl Junction {
    /// Bytes of one output frame.
    pub const OUT_FRAME: usize = 48;
    /// Bytes of one input frame.
    pub const IN_FRAME: usize = 64;
}

impl WireCodec for Junction {
    fn out_frame_bytes(&self) -> usize {
        Self::OUT_FRAME
    }

    fn in_frame_bytes(&self) -> usize {
        Self::IN_FRAME
    }

    fn encode(&self, src: &[i32], dst: &mut [u8]) -> usize {
        let ch = self.out_channels.clamp(2, 8);
        let frames = (src.len() / ch).min(dst.len() / Self::OUT_FRAME);
        for f in 0..frames {
            let frame = &src[f * ch..(f + 1) * ch];
            let out = &mut dst[f * Self::OUT_FRAME..(f + 1) * Self::OUT_FRAME];
            out.fill(0);
            for (lane, sample) in frame.iter().enumerate() {
                let value = (*sample >> 8) as u32 & 0x00FF_FFFF;
                let block = if lane % 2 == 0 { 0 } else { 24 };
                let bit = (lane / 2) as u8;
                for i in 0..24 {
                    if (value >> (23 - i)) & 1 == 1 {
                        out[block + i] |= 1 << bit;
                    }
                }
            }
        }
        frames * Self::OUT_FRAME
    }

    fn decode(&self, src: &[u8], dst: &mut [i32]) -> usize {
        let ch = self.in_channels.clamp(2, 16);
        let frames = (src.len() / Self::IN_FRAME).min(dst.len() / ch);
        for f in 0..frames {
            let inp = &src[f * Self::IN_FRAME..(f + 1) * Self::IN_FRAME];
            let out = &mut dst[f * ch..(f + 1) * ch];
            for (lane, sample) in out.iter_mut().enumerate() {
                let block = if lane % 2 == 0 { 0x00 } else { 0x20 };
                let bit = (lane / 2) as u8;
                let mut value: u32 = 0;
                for i in 0..24 {
                    if (inp[block + i] >> bit) & 1 == 1 {
                        value |= 1 << (23 - i);
                    }
                }
                *sample = (value << 8) as i32; // keep 24 bits in the top of the word
            }
        }
        frames
    }
}

/// Reorder channels into the pairing the kext's `pcmTo24JunctionNCH` helpers
/// produce: lane order `0, N/2, 1, N/2+1, ...` (all "left" channels first in the
/// host buffer become interleaved pairs on the wire). Returns a new frame.
///
/// `pcmTo24Junction4CH` maps host `[0,1,2,3]` to wire `[0,2,1,3]`;
/// `pcmTo24Junction6CH` maps `[0..6]` to `[0,3,1,4,2,5]`. **confirmed**.
pub fn pair_reorder(frame: &[i32]) -> Vec<i32> {
    let n = frame.len();
    let half = n / 2;
    let mut out = Vec::with_capacity(n);
    for i in 0..half {
        out.push(frame[i]);
        out.push(frame[i + half]);
    }
    if n % 2 == 1 {
        out.push(frame[n - 1]);
    }
    out
}

/// Convert a 16-bit sample to the 24-in-32 layout the codecs expect.
pub fn from_i16(s: i16) -> i32 {
    (s as i32) << 16
}

/// Convert a 24-in-32 sample to `f32` in `-1.0..1.0`.
pub fn to_f32(s: i32) -> f32 {
    s as f32 / 2_147_483_648.0
}

/// Convert `f32` in `-1.0..1.0` to the 24-in-32 layout (clamped).
pub fn from_f32(s: f32) -> i32 {
    let clamped = s.clamp(-1.0, 1.0);
    ((clamped * 8_388_607.0) as i32) << 8
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn plain24_roundtrip() {
        let codec = Plain24 { out_channels: 2, in_channels: 2 };
        let src = [0x1234_5600, -0x0012_3400, 0x7FFF_FF00, i32::MIN];
        let mut wire = [0u8; 12];
        assert_eq!(codec.encode(&src, &mut wire), 12);
        assert_eq!(&wire[0..3], &[0x56, 0x34, 0x12]);
        let mut back = [0i32; 4];
        assert_eq!(codec.decode(&wire, &mut back), 4);
        assert_eq!(back, src);
    }

    #[test]
    fn junction_roundtrip_8_lanes() {
        let codec = Junction { out_channels: 8, in_channels: 8 };
        let frame: Vec<i32> = (0..8).map(|i| (0x0012_3400 * (i + 1)) ^ (i << 20)).collect();
        let mut wire = [0u8; 48];
        assert_eq!(codec.encode(&frame, &mut wire), 48);
        // Build the input frame the device would produce for the same samples
        // (odd lanes at 0x00, even at 0x20) and decode it.
        let mut inp = [0u8; 64];
        inp[0x00..0x18].copy_from_slice(&wire[0..24]);
        inp[0x20..0x38].copy_from_slice(&wire[24..48]);
        let mut back = vec![0i32; 8];
        assert_eq!(codec.decode(&inp, &mut back), 1);
        for (a, b) in frame.iter().zip(back.iter()) {
            assert_eq!(a & !0xFF, b & !0xFF);
        }
    }

    #[test]
    fn junction_matches_ozzy_reference_vector() {
        // Ozzy's ploytec_convert_from_s24_3le: channel 1 MSB (src[2] & 0x80) -> dest[0] bit 0.
        let codec = Junction { out_channels: 8, in_channels: 8 };
        let mut frame = vec![0i32; 8];
        frame[0] = 0x0080_0000 << 8; // bit 23 of lane 0
        frame[2] = 0x0000_0001 << 8; // bit 0 of lane 2 (second odd lane -> bit 1)
        let mut wire = [0u8; 48];
        codec.encode(&frame, &mut wire);
        assert_eq!(wire[0], 0x01);
        assert_eq!(wire[23], 0x02);
        assert!(wire[24..].iter().all(|b| *b == 0));
    }

    #[test]
    fn pair_reorder_matches_kext_helpers() {
        assert_eq!(pair_reorder(&[0, 1, 2, 3]), vec![0, 2, 1, 3]);
        assert_eq!(pair_reorder(&[0, 1, 2, 3, 4, 5]), vec![0, 3, 1, 4, 2, 5]);
    }

    #[test]
    fn float_conversions() {
        assert_eq!(from_f32(0.0), 0);
        assert!(to_f32(from_f32(0.5)) > 0.49 && to_f32(from_f32(0.5)) < 0.51);
        assert_eq!(from_i16(i16::MAX) >> 16, i16::MAX as i32);
    }
}
