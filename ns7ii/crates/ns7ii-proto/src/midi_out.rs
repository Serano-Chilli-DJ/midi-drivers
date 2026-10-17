//! Host-to-device MIDI scheduling: a bounded, coalescing queue and a pacer.
//!
//! A DJ app can send far more LED, meter and motor messages than the NS7II
//! absorbs: the unit NAKs its bulk OUT endpoint while its internal link to
//! the control-surface MCU (about 400 kbaud, with a 256-byte TX ring that
//! drops bytes when full) catches up, and a host that keeps writing 42-byte
//! transfers back to back then sees one-second timeouts, protocol errors
//! and finally the unit dropping off the bus. So outgoing MIDI goes through
//!
//! * [`OutQueue`]: messages for the same target (note number, controller,
//!   channel-wide message) **replace** the queued one in place, so a meter
//!   that updates 30 times a second costs one slot however far the writer
//!   falls behind, and the latest value always wins. SysEx and system
//!   messages are kept in order. The queue never holds more than its byte
//!   budget; what does not fit is dropped and counted.
//! * [`Pacer`]: a token bucket limiting MIDI bytes per second and the gap
//!   between transfers, which halves its rate on a timeout and creeps back
//!   once writes succeed again.
//!
//! Nothing here performs I/O or reads the clock, so both are unit tested.

use crate::midi::{Parser, OUT_FRAME_PAYLOAD};
use std::collections::{HashMap, VecDeque};
use std::time::{Duration, Instant};

/// What a queued message replaces when a newer one with the same key arrives.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
enum Key {
    /// Note on / note off for (channel, note): the latest one is the LED state.
    Note(u8, u8),
    /// Control change for (channel, controller).
    Cc(u8, u8),
    /// Polyphonic aftertouch for (channel, note).
    PolyAt(u8, u8),
    /// Program change, channel pressure or pitch bend: (kind, channel).
    Channel(u8, u8),
    /// Never coalesced (SysEx, system common, real time); unique sequence number.
    Unique(u64),
}

fn key_of(status: u8, data: &[u8], seq: u64) -> Key {
    let ch = status & 0x0F;
    match status & 0xF0 {
        0x80 | 0x90 => Key::Note(ch, data.first().copied().unwrap_or(0)),
        0xA0 => Key::PolyAt(ch, data.first().copied().unwrap_or(0)),
        0xB0 => Key::Cc(ch, data.first().copied().unwrap_or(0)),
        0xC0 | 0xD0 | 0xE0 => Key::Channel(status & 0xF0, ch),
        _ => Key::Unique(seq),
    }
}

/// Counters of an [`OutQueue`].
#[derive(Debug, Default, Clone, Copy, PartialEq, Eq)]
pub struct QueueStats {
    /// Messages accepted.
    pub accepted: u64,
    /// Messages that replaced a queued message with the same key.
    pub coalesced: u64,
    /// Messages dropped because the byte budget was full.
    pub dropped: u64,
}

/// Bounded, coalescing queue of outgoing MIDI messages (see the module docs).
#[derive(Debug)]
pub struct OutQueue {
    parser: Parser,
    order: VecDeque<Key>,
    slots: HashMap<Key, Vec<u8>>,
    bytes: usize,
    max_bytes: usize,
    seq: u64,
    stats: QueueStats,
}

/// Default byte budget of an [`OutQueue`]: far more than one screen of LED
/// state, small enough that a stalled unit cannot grow the daemon.
pub const DEFAULT_QUEUE_BYTES: usize = 16 * 1024;

impl OutQueue {
    /// A queue holding at most `max_bytes` bytes of MIDI.
    pub fn new(max_bytes: usize) -> Self {
        Self {
            parser: Parser::new(),
            order: VecDeque::new(),
            slots: HashMap::new(),
            bytes: 0,
            max_bytes,
            seq: 0,
            stats: QueueStats::default(),
        }
    }

    /// Feed raw MIDI bytes (running status allowed); complete messages are queued.
    pub fn push_bytes(&mut self, bytes: &[u8]) {
        for &b in bytes {
            if let Some(m) = self.parser.push(b) {
                let mut raw = Vec::with_capacity(m.data.len() + 2);
                raw.push(m.status);
                raw.extend_from_slice(&m.data);
                if m.status == 0xF0 {
                    raw.push(0xF7);
                }
                self.push_message(raw);
            }
        }
    }

    /// Queue one complete message (status byte first; SysEx including F0..F7).
    pub fn push_message(&mut self, raw: Vec<u8>) {
        if raw.is_empty() {
            return;
        }
        self.seq += 1;
        let key = key_of(raw[0], &raw[1..], self.seq);
        if let Some(old) = self.slots.get_mut(&key) {
            let new_bytes = self.bytes - old.len() + raw.len();
            if new_bytes > self.max_bytes {
                self.stats.dropped += 1;
                return;
            }
            self.bytes = new_bytes;
            *old = raw;
            self.stats.accepted += 1;
            self.stats.coalesced += 1;
            return;
        }
        if self.bytes + raw.len() > self.max_bytes {
            self.stats.dropped += 1;
            return;
        }
        self.bytes += raw.len();
        self.slots.insert(key, raw);
        self.order.push_back(key);
        self.stats.accepted += 1;
    }

    /// Take up to `max_payload` bytes of whole messages, oldest first. A
    /// message longer than `max_payload` (SysEx) is split across calls.
    /// Returns an empty vector when the queue is empty.
    pub fn pop_frame(&mut self, max_payload: usize) -> Vec<u8> {
        let mut out = Vec::with_capacity(max_payload);
        while let Some(&key) = self.order.front() {
            let len = self.slots.get(&key).map_or(0, Vec::len);
            let room = max_payload - out.len();
            if len <= room {
                self.order.pop_front();
                if let Some(m) = self.slots.remove(&key) {
                    self.bytes -= m.len();
                    out.extend_from_slice(&m);
                }
                continue;
            }
            if out.is_empty() {
                // Longer than a whole transfer: send its head, keep the rest first in line.
                if let Some(m) = self.slots.get_mut(&key) {
                    out.extend(m.drain(..room));
                    self.bytes -= room;
                }
            }
            break;
        }
        out
    }

    /// Pop a frame of at most one NS7II transfer's payload.
    pub fn pop_transfer(&mut self) -> Vec<u8> {
        self.pop_frame(OUT_FRAME_PAYLOAD)
    }

    /// Bytes queued.
    pub fn len_bytes(&self) -> usize {
        self.bytes
    }

    /// No message queued.
    pub fn is_empty(&self) -> bool {
        self.order.is_empty()
    }

    /// Counters since creation.
    pub fn stats(&self) -> QueueStats {
        self.stats
    }
}

impl Default for OutQueue {
    fn default() -> Self {
        Self::new(DEFAULT_QUEUE_BYTES)
    }
}

/// Token bucket for MIDI OUT transfers (see the module docs).
#[derive(Debug, Clone)]
pub struct Pacer {
    max_rate: f64,
    min_rate: f64,
    rate: f64,
    burst: f64,
    tokens: f64,
    min_gap: Duration,
    last_tick: Option<Instant>,
    last_send: Option<Instant>,
    calm_since: Option<Instant>,
}

/// How long writes must succeed before a backed-off rate is raised again.
const RECOVER_AFTER: Duration = Duration::from_secs(5);

impl Pacer {
    /// At most `rate` MIDI bytes per second (bursting up to one full
    /// transfer) and at least `min_gap` between transfers.
    pub fn new(rate: u32, min_gap: Duration) -> Self {
        let rate = rate.max(100) as f64;
        let burst = OUT_FRAME_PAYLOAD as f64;
        Self {
            max_rate: rate,
            min_rate: (rate / 16.0).max(100.0),
            rate,
            burst,
            tokens: burst,
            min_gap,
            last_tick: None,
            last_send: None,
            calm_since: None,
        }
    }

    fn refill(&mut self, now: Instant) {
        if let Some(t) = self.last_tick {
            let dt = now.saturating_duration_since(t).as_secs_f64();
            self.tokens = (self.tokens + dt * self.rate).min(self.burst);
        }
        self.last_tick = Some(now);
        if let Some(c) = self.calm_since {
            if self.rate < self.max_rate && now.saturating_duration_since(c) >= RECOVER_AFTER {
                self.rate = (self.rate * 1.5).min(self.max_rate);
                self.calm_since = Some(now);
            }
        }
    }

    /// How long to wait at `now` before a transfer of `bytes` MIDI bytes may
    /// go out (zero: send now).
    pub fn delay(&mut self, now: Instant, bytes: usize) -> Duration {
        self.refill(now);
        let need = (bytes as f64).min(self.burst);
        let by_tokens = if self.tokens >= need { 0.0 } else { (need - self.tokens) / self.rate };
        let by_gap = self
            .last_send
            .map_or(Duration::ZERO, |t| self.min_gap.saturating_sub(now.saturating_duration_since(t)));
        Duration::from_secs_f64(by_tokens).max(by_gap)
    }

    /// A transfer of `bytes` MIDI bytes went out at `now`.
    pub fn sent(&mut self, now: Instant, bytes: usize) {
        self.refill(now);
        self.tokens -= (bytes as f64).min(self.burst);
        self.last_send = Some(now);
        if self.calm_since.is_none() {
            self.calm_since = Some(now);
        }
    }

    /// A transfer timed out at `now`: halve the rate (down to a floor) and
    /// wait a while before raising it again.
    pub fn timed_out(&mut self, now: Instant) {
        self.refill(now);
        self.rate = (self.rate / 2.0).max(self.min_rate);
        self.tokens = 0.0;
        self.last_send = Some(now);
        self.calm_since = Some(now);
    }

    /// Current byte rate.
    pub fn rate(&self) -> u32 {
        self.rate as u32
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn coalesces_same_controller_and_keeps_latest_value_in_place() {
        let mut q = OutQueue::new(1024);
        q.push_bytes(&[0xB0, 0x4A, 0x01, 0x90, 0x3C, 0x7F, 0xB0, 0x4A, 0x05, 0xB0, 0x4B, 0x02]);
        q.push_bytes(&[0xB0, 0x4A, 0x09]);
        assert_eq!(q.pop_transfer(), vec![0xB0, 0x4A, 0x09, 0x90, 0x3C, 0x7F, 0xB0, 0x4B, 0x02]);
        assert!(q.is_empty());
        assert_eq!(q.stats().coalesced, 2);
    }

    #[test]
    fn note_off_supersedes_note_on_for_the_same_led() {
        let mut q = OutQueue::new(1024);
        q.push_bytes(&[0x91, 0x36, 0x7F, 0x81, 0x36, 0x00, 0x91, 0x37, 0x10]);
        assert_eq!(q.pop_transfer(), vec![0x81, 0x36, 0x00, 0x91, 0x37, 0x10]);
    }

    #[test]
    fn running_status_input_is_expanded_into_whole_messages() {
        let mut q = OutQueue::new(1024);
        q.push_bytes(&[0xB0, 0x4A, 0x01, 0x4B, 0x02]);
        assert_eq!(q.pop_transfer(), vec![0xB0, 0x4A, 0x01, 0xB0, 0x4B, 0x02]);
    }

    #[test]
    fn frames_hold_whole_messages_up_to_the_payload() {
        let mut q = OutQueue::new(4096);
        for cc in 0..20u8 {
            q.push_bytes(&[0xB0, cc, 1]);
        }
        let f1 = q.pop_transfer();
        assert_eq!(f1.len(), 39);
        let f2 = q.pop_transfer();
        assert_eq!(f2.len(), 21);
        assert!(q.pop_transfer().is_empty());
    }

    #[test]
    fn sysex_is_kept_in_order_and_split_across_transfers() {
        let mut q = OutQueue::new(4096);
        let mut sx = vec![0xF0];
        sx.extend(std::iter::repeat_n(0x11, 60));
        sx.push(0xF7);
        q.push_bytes(&[0x90, 0x01, 0x7F]);
        q.push_bytes(&sx);
        q.push_bytes(&sx);
        let mut all = Vec::new();
        loop {
            let f = q.pop_transfer();
            if f.is_empty() {
                break;
            }
            assert!(f.len() <= 39);
            all.extend(f);
        }
        let mut want = vec![0x90, 0x01, 0x7F];
        want.extend(&sx);
        want.extend(&sx);
        assert_eq!(all, want);
        assert_eq!(q.len_bytes(), 0);
    }

    #[test]
    fn memory_stays_bounded_while_nothing_is_sent() {
        let mut q = OutQueue::new(2048);
        // Ten minutes of a busy app: meters, strip bars, pad colours, SysEx.
        for i in 0..200_000u32 {
            let v = (i % 12) as u8;
            q.push_bytes(&[0xB0, 0x4A + (i % 4) as u8, v]);
            q.push_bytes(&[0xB1 + (i % 4) as u8, 0x6E, v]);
            q.push_bytes(&[0x90 + (i % 4) as u8, (i % 128) as u8, v]);
            if i % 100 == 0 {
                q.push_bytes(&[0xF0, 0x00, 0x01, 0x3F, 0x7F, 0x2C, 0x60, 0x00, 0x01, 0x00, 0xF7]);
            }
        }
        assert!(q.len_bytes() <= 2048, "{}", q.len_bytes());
        assert!(q.stats().dropped > 0);
        // Keyed state never needs more than one slot per target.
        assert!(q.order.len() == q.slots.len());
    }

    #[test]
    fn pacer_limits_byte_rate_and_gap() {
        let t0 = Instant::now();
        let mut p = Pacer::new(3900, Duration::from_millis(1));
        let mut t = t0;
        let mut sent = 0usize;
        // Simulate one second of a writer that always has a full transfer.
        while t < t0 + Duration::from_secs(1) {
            let d = p.delay(t, 39);
            if d.is_zero() {
                p.sent(t, 39);
                sent += 39;
                t += Duration::from_micros(100);
            } else {
                t += d;
            }
        }
        // 3900 B/s plus the initial burst.
        assert!((3900..=3900 + 2 * 39).contains(&sent), "{sent}");
        // Small transfers are limited by the gap.
        let mut p = Pacer::new(1_000_000, Duration::from_millis(2));
        p.sent(t0, 3);
        assert_eq!(p.delay(t0 + Duration::from_millis(1), 3), Duration::from_millis(1));
        assert_eq!(p.delay(t0 + Duration::from_millis(2), 3), Duration::ZERO);
    }

    #[test]
    fn pacer_backs_off_on_timeouts_and_recovers() {
        let t0 = Instant::now();
        let mut p = Pacer::new(4000, Duration::ZERO);
        p.timed_out(t0);
        assert_eq!(p.rate(), 2000);
        p.timed_out(t0);
        p.timed_out(t0);
        p.timed_out(t0);
        p.timed_out(t0);
        assert_eq!(p.rate(), 250, "floor is 1/16 of the configured rate");
        // Calm for a while: the rate climbs back to the configured value.
        let mut t = t0;
        for _ in 0..200 {
            t += Duration::from_secs(1);
            let _ = p.delay(t, 3);
            p.sent(t, 3);
        }
        assert_eq!(p.rate(), 4000);
    }
}
