//! Playback pacing from the NS7II's feedback endpoint (EP 0x81).
//!
//! The NS7II's DAC runs on its own crystal, so the host must send exactly as
//! many frames as the unit plays, not `rate / 8000` per microframe on the
//! host's clock. The unit tells how many it played: every millisecond EP
//! 0x81 delivers a 3-byte packet whose **byte 0 is the number of frames of
//! that millisecond** (42..46 at 44.1 kHz). This is not a UAC 10.14/16.16
//! rate. Decoded from the vendor kext (`isocReadFeedbackCompleteStatic`,
//! `isocFeedbackReadCompleteFullFrame`, `PGFramePatternObserver`,
//! `PGIsocRB::initWrite_Std`):
//!
//! * a 16 ms ring of frames-per-OUT-packet (128 microframe slots at high
//!   speed) is filled with each millisecond's count spread over its 8
//!   microframes (e.g. 44 -> 6,5,6,5,6,5,6,5), and the OUT stream reads the
//!   ring one slot per packet, so it **replays the unit's own frame counts**
//!   a fixed time later and the totals match exactly, without an integrator;
//! * a count more than 2 frames from `rate / 1000` is "lost sync": that
//!   millisecond plays the nominal pattern; 16 good milliseconds in a row
//!   are "locked".
//!
//! Differences from the kext: the write position is placed half a ring
//! ahead of the read position when feedback starts (the kext starts both
//! streams on the same frame, which usbfs cannot promise), and is re-placed
//! if the two ever drift close; and a slot that no feedback refreshed since
//! it was last read plays an exact rational nominal rate instead of a
//! repeating integer pattern (which would run 567 ppm fast at 44.1 kHz).

/// Milliseconds of history in the ring (the kext's 128 microframes).
const RING_MS: usize = 16;
/// Allowed distance of a per-ms count from `rate / 1000` (kext: 2).
const MAX_DEVIATION: i32 = 2;
/// Consecutive good milliseconds that count as locked (one ring revolution).
const LOCK_MS: u32 = RING_MS as u32;

/// What one feedback packet did.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Event {
    /// A good count was stored.
    Ok,
    /// A good count completed a run of [`LOCK_MS`]: locked.
    Locked,
    /// A count out of range while locked: lost sync.
    LostSync,
    /// A count out of range while not locked.
    OutOfRange,
    /// An empty packet.
    Empty,
}

/// Counters, for logs and the drift controller.
#[derive(Debug, Default, Clone, Copy, PartialEq, Eq)]
pub struct Stats {
    /// Feedback packets seen.
    pub packets: u64,
    /// Packets with a usable count.
    pub good: u64,
    /// Packets whose count was out of range.
    pub out_of_range: u64,
    /// Packets whose length was not 3 bytes (the kext stops streaming after 40).
    pub bad_length: u64,
    /// Currently locked.
    pub locked: bool,
    /// Last count (frames in one millisecond).
    pub last_frames: u8,
    /// OUT packets sized from feedback.
    pub from_feedback: u64,
    /// OUT packets sized from the nominal rate.
    pub from_nominal: u64,
    /// Frames handed to OUT packets in total.
    pub frames_out: u64,
}

/// The frame pattern ring shared by the feedback and OUT streams.
#[derive(Debug, Clone)]
pub struct FramePattern {
    slots_per_ms: usize,
    nominal_ms: i32,
    ring: Vec<u8>,
    fresh: Vec<bool>,
    read: usize,
    write: Option<usize>,
    good_run: u32,
    // Exact nominal pacing when a slot has no feedback: `rate` frames over
    // `packets_per_sec` packets.
    rate: u64,
    packets_per_sec: u64,
    accum: u64,
    stats: Stats,
}

impl FramePattern {
    /// A pattern for `rate` Hz; `high_speed` selects 8 OUT packets per ms (else 1).
    pub fn new(rate: u32, high_speed: bool) -> Self {
        let slots_per_ms = if high_speed { 8 } else { 1 };
        let len = RING_MS * slots_per_ms;
        Self {
            slots_per_ms,
            nominal_ms: (rate / 1000) as i32,
            ring: vec![0; len],
            fresh: vec![false; len],
            read: 0,
            write: None,
            good_run: 0,
            rate: rate as u64,
            packets_per_sec: 1000 * slots_per_ms as u64,
            accum: 0,
            stats: Stats::default(),
        }
    }

    fn len(&self) -> usize {
        self.ring.len()
    }

    /// Handle one feedback packet (`raw` = the bytes the unit sent).
    pub fn feedback(&mut self, raw: &[u8]) -> Event {
        self.stats.packets += 1;
        let Some(&v) = raw.first() else { return Event::Empty };
        if raw.len() != 3 {
            self.stats.bad_length += 1;
        }
        let len = self.len();
        // Place (or re-place) the write position half a ring ahead of the reader.
        let w = match self.write {
            Some(w) if (len / 8..=len * 7 / 8).contains(&((w + len - self.read) % len)) => w,
            _ => (self.read + len / 2) % len,
        };
        let spm = self.slots_per_ms;
        if (v as i32 - self.nominal_ms).abs() <= MAX_DEVIATION {
            let (base, extra) = (v as usize / spm, v as usize % spm);
            for i in 0..spm {
                let n = base + (i + 1) * extra / spm - i * extra / spm;
                self.ring[(w + i) % len] = n as u8;
                self.fresh[(w + i) % len] = true;
            }
            self.write = Some((w + spm) % len);
            self.stats.good += 1;
            self.stats.last_frames = v;
            self.good_run += 1;
            if !self.stats.locked && self.good_run >= LOCK_MS {
                self.stats.locked = true;
                return Event::Locked;
            }
            Event::Ok
        } else {
            for i in 0..spm {
                self.fresh[(w + i) % len] = false;
            }
            self.write = Some((w + spm) % len);
            self.stats.out_of_range += 1;
            self.good_run = 0;
            if std::mem::take(&mut self.stats.locked) {
                Event::LostSync
            } else {
                Event::OutOfRange
            }
        }
    }

    /// Frames for the next OUT packet.
    pub fn next_frames(&mut self) -> usize {
        let r = self.read;
        self.read = (r + 1) % self.len();
        // Keep the nominal accumulator running so a fallback slot continues
        // the exact nominal sequence.
        self.accum += self.rate;
        let nominal = (self.accum / self.packets_per_sec) as usize;
        self.accum %= self.packets_per_sec;
        let n = if std::mem::take(&mut self.fresh[r]) {
            self.stats.from_feedback += 1;
            self.ring[r] as usize
        } else {
            self.stats.from_nominal += 1;
            nominal
        };
        self.stats.frames_out += n as u64;
        n
    }

    /// Counters.
    pub fn stats(&self) -> Stats {
        self.stats
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Per-ms frame counts of a device whose clock runs at `hz`.
    fn device_counts(hz: f64, ms: usize) -> Vec<u8> {
        let mut acc = 0.0;
        (0..ms)
            .map(|_| {
                acc += hz / 1000.0;
                let n = acc.floor();
                acc -= n;
                n as u8
            })
            .collect()
    }

    #[test]
    fn without_feedback_the_rate_is_exactly_nominal() {
        let mut p = FramePattern::new(44_100, true);
        let total: usize = (0..8000 * 3).map(|_| p.next_frames()).sum();
        assert_eq!(total, 3 * 44_100);
        let mut p = FramePattern::new(44_100, false);
        let total: usize = (0..1000).map(|_| p.next_frames()).sum();
        assert_eq!(total, 44_100);
    }

    #[test]
    fn a_millisecond_count_is_spread_over_eight_microframes() {
        let mut p = FramePattern::new(44_100, true);
        assert_eq!(p.feedback(&[44, 0, 0]), Event::Ok);
        // Written half a ring ahead of the reader.
        for _ in 0..64 {
            p.next_frames();
        }
        let got: Vec<usize> = (0..8).map(|_| p.next_frames()).collect();
        assert_eq!(got.iter().sum::<usize>(), 44);
        assert!(got.iter().all(|n| *n == 5 || *n == 6));
        let mut p = FramePattern::new(44_100, true);
        p.feedback(&[46, 0, 0]);
        for _ in 0..64 {
            p.next_frames();
        }
        assert_eq!((0..8).map(|_| p.next_frames()).sum::<usize>(), 46);
    }

    #[test]
    fn the_out_stream_follows_a_fast_or_slow_device_clock_exactly() {
        for hz in [44_100.0 * (1.0 + 300e-6), 44_100.0 * (1.0 - 300e-6), 44_100.0] {
            let mut p = FramePattern::new(44_100, true);
            let counts = device_counts(hz, 20_000);
            // Feedback URBs complete in bursts of 4 ms, OUT URBs are filled in
            // bursts of 32 packets at another phase.
            let mut sent = 0usize;
            for (chunk_i, chunk) in counts.chunks(4).enumerate() {
                for c in chunk {
                    p.feedback(&[*c, 0, 0]);
                }
                if chunk_i > 0 {
                    for _ in 0..32 {
                        sent += p.next_frames();
                    }
                }
            }
            let device: usize = counts.iter().map(|c| *c as usize).sum();
            // Everything the device played was sent, minus what is still in the ring.
            let diff = device as i64 - sent as i64;
            assert!((0..=46 * 8).contains(&diff), "hz {hz}: device {device} sent {sent}");
            let s = p.stats();
            assert!(s.locked);
            assert!(s.from_nominal < 200, "{s:?}");
        }
    }

    #[test]
    fn out_of_range_counts_lose_sync_and_play_nominal() {
        let mut p = FramePattern::new(44_100, true);
        for i in 0..LOCK_MS {
            let e = p.feedback(&[44, 0, 0]);
            assert_eq!(e, if i + 1 == LOCK_MS { Event::Locked } else { Event::Ok });
        }
        assert_eq!(p.feedback(&[0xAA, 0, 0]), Event::LostSync);
        assert_eq!(p.feedback(&[200, 0, 0]), Event::OutOfRange);
        assert_eq!(p.feedback(&[]), Event::Empty);
        assert_eq!(p.feedback(&[44, 0, 0, 0]), Event::Ok);
        let s = p.stats();
        assert_eq!((s.out_of_range, s.bad_length, s.locked), (2, 1, false));
    }

    #[test]
    fn the_write_position_is_re_placed_when_feedback_lags_behind() {
        let mut p = FramePattern::new(44_100, true);
        p.feedback(&[44, 0, 0]);
        // The reader runs on for 9 ms while feedback is missing.
        for _ in 0..70 {
            p.next_frames();
        }
        p.feedback(&[45, 0, 0]);
        let w = p.write.unwrap();
        let d = (w + p.len() - p.read) % p.len();
        assert!(d >= p.len() / 4, "{d}");
    }
}
