//! The daemon's MIDI OUT path: apps -> bounded coalescing queue -> paced
//! 42-byte transfers to the NS7II, on a thread of its own so a busy or
//! stalled unit never blocks MIDI IN or grows memory.
//!
//! Timeouts (the unit NAKing) slow the pacer down, clear the endpoint halt
//! (which also resynchronises the data toggle) and retry the transfer a
//! couple of times; a run of timeouts, or a protocol error / ENODEV, is
//! reported as a device fault so the supervisor can reset or reopen the
//! unit. Failures are logged at most once per second, with counts.

use crate::bridge::Throttle;
use log::{info, warn};
use ns7ii_proto::midi_out::{OutQueue, Pacer};
use std::io;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Condvar, Mutex};
use std::time::{Duration, Instant};

/// Settings of the MIDI OUT writer.
#[derive(Debug, Clone, Copy)]
pub struct Config {
    /// MIDI bytes per second at most.
    pub rate: u32,
    /// Minimum gap between two transfers.
    pub min_gap: Duration,
    /// Timeout of one transfer.
    pub timeout: Duration,
    /// Tries per transfer before it is dropped.
    pub tries: u32,
    /// Consecutive timeouts (across transfers) reported as a stalled unit.
    pub stall_after: u32,
}

impl Default for Config {
    fn default() -> Self {
        Self {
            rate: DEFAULT_RATE,
            min_gap: Duration::from_millis(DEFAULT_GAP_MS),
            // The vendor driver's MIDI OUT timeout (AJ::sendRB, 500 ms).
            timeout: Duration::from_millis(500),
            tries: 3,
            stall_after: 8,
        }
    }
}

/// Default MIDI OUT byte rate (bytes per second): a fifth of the unit's
/// ~400 kbaud link to the control-surface MCU (512-byte RX ring), far above
/// what a coalesced app stream needs (four 30 Hz meters are 360 B/s). The
/// vendor driver does not pace at all (one transfer in flight, the next one
/// on completion), so this is a safety margin, not a protocol requirement.
pub const DEFAULT_RATE: u32 = 8_000;
/// Default minimum gap between transfers in milliseconds.
pub const DEFAULT_GAP_MS: u64 = 1;

/// Where transfers go: the NS7II, or a fake in tests.
pub trait Sink {
    /// Send one transfer's MIDI payload.
    fn write(&self, payload: &[u8], timeout: Duration) -> io::Result<()>;
    /// Clear the MIDI OUT endpoint's halt / data toggle.
    fn clear_halt(&self) -> io::Result<()>;
}

impl Sink for ns7ii_usb::Ns7ii {
    fn write(&self, payload: &[u8], timeout: Duration) -> io::Result<()> {
        self.midi_write_transfer(payload, timeout)
    }
    fn clear_halt(&self) -> io::Result<()> {
        match self.midi_out_ep() {
            Some(ep) => ns7ii_usb::Ns7ii::clear_halt(self, ep).map_err(|e| match e {
                ns7ii_usb::Error::Io(e) => e,
                other => io::Error::other(other.to_string()),
            }),
            None => Ok(()),
        }
    }
}

/// The queue shared between the sequencer thread (producer) and the writer.
#[derive(Default)]
pub struct Shared {
    queue: Mutex<OutQueue>,
    ready: Condvar,
    fault: Mutex<Option<String>>,
}

impl Shared {
    /// New, empty.
    pub fn new() -> Arc<Self> {
        Arc::new(Self::default())
    }

    /// Queue raw MIDI bytes from an app.
    pub fn push(&self, bytes: &[u8]) {
        self.queue.lock().unwrap().push_bytes(bytes);
        self.ready.notify_one();
    }

    /// Take the fault the writer reported, if any.
    pub fn take_fault(&self) -> Option<String> {
        self.fault.lock().unwrap().take()
    }

    fn report(&self, reason: String) {
        let mut f = self.fault.lock().unwrap();
        if f.is_none() {
            *f = Some(reason);
        }
    }

    /// Wait up to `timeout` for a transfer's worth of queued MIDI.
    fn next(&self, timeout: Duration) -> Vec<u8> {
        let mut q = self.queue.lock().unwrap();
        if q.is_empty() && !timeout.is_zero() {
            q = self.ready.wait_timeout(q, timeout).unwrap().0;
        }
        q.pop_transfer()
    }

    /// Bytes queued and the queue's counters.
    pub fn status(&self) -> (usize, ns7ii_proto::midi_out::QueueStats) {
        let q = self.queue.lock().unwrap();
        (q.len_bytes(), q.stats())
    }
}

/// What happened to one transfer.
#[derive(Debug, PartialEq, Eq)]
enum Outcome {
    Sent,
    Retry,
    Dropped,
}

/// The writer's state between transfers (separate from the loop for tests).
pub struct Writer {
    cfg: Config,
    pacer: Pacer,
    pending: Option<(Vec<u8>, u32)>,
    consecutive_timeouts: u32,
    throttle: Throttle,
    timeouts: u64,
    dropped: u64,
    idle_wait: Duration,
}

impl Writer {
    /// New writer with `cfg`.
    pub fn new(cfg: Config) -> Self {
        Self {
            cfg,
            pacer: Pacer::new(cfg.rate, cfg.min_gap),
            pending: None,
            consecutive_timeouts: 0,
            throttle: Throttle::new(Duration::from_secs(1)),
            timeouts: 0,
            dropped: 0,
            idle_wait: Duration::from_millis(50),
        }
    }

    /// One step: fetch a transfer if none is pending, wait for the pacer,
    /// write it. Returns how long the caller should sleep before the next
    /// step (zero to continue at once).
    pub fn step<S: Sink + ?Sized>(&mut self, shared: &Shared, sink: &S, now: Instant) -> Duration {
        if self.pending.is_none() {
            let frame = shared.next(self.idle_wait);
            if frame.is_empty() {
                return Duration::ZERO;
            }
            self.pending = Some((frame, 0));
        }
        let len = self.pending.as_ref().map_or(0, |p| p.0.len());
        let wait = self.pacer.delay(now, len);
        if !wait.is_zero() {
            return wait;
        }
        match self.write(shared, sink, now) {
            Outcome::Sent | Outcome::Dropped => Duration::ZERO,
            Outcome::Retry => Duration::from_millis(20),
        }
    }

    fn write<S: Sink + ?Sized>(&mut self, shared: &Shared, sink: &S, now: Instant) -> Outcome {
        let Some((frame, tries)) = self.pending.as_mut() else { return Outcome::Sent };
        match sink.write(frame, self.cfg.timeout) {
            Ok(()) => {
                self.pacer.sent(now, frame.len());
                self.pending = None;
                if self.consecutive_timeouts > 0 {
                    info!("MIDI OUT: the NS7II accepts transfers again (now at most {} B/s)", self.pacer.rate());
                }
                self.consecutive_timeouts = 0;
                Outcome::Sent
            }
            Err(e) => {
                *tries += 1;
                let errno = e.raw_os_error();
                let gave_up = *tries >= self.cfg.tries;
                if errno == Some(libc::ETIMEDOUT) {
                    self.timeouts += 1;
                    self.consecutive_timeouts += 1;
                    self.pacer.timed_out(now);
                    // The packet may have been taken with its ACK lost: resync the toggle.
                    let _ = sink.clear_halt();
                    if let Some(held) = self.throttle.hit(now) {
                        warn!(
                            "MIDI OUT to the NS7II timed out ({} in total, +{held} since the last report): the unit is busy; \
                             slowed to {} B/s, {} B queued",
                            self.timeouts,
                            self.pacer.rate(),
                            shared.status().0
                        );
                    }
                    if self.consecutive_timeouts >= self.cfg.stall_after {
                        shared.report(format!("MIDI OUT stalled: {} transfers in a row timed out", self.consecutive_timeouts));
                        self.consecutive_timeouts = 0;
                        self.pending = None;
                        self.dropped += 1;
                        return Outcome::Dropped;
                    }
                } else if errno == Some(libc::EPIPE) {
                    let _ = sink.clear_halt();
                } else if matches!(errno, Some(libc::ENODEV) | Some(libc::EPROTO) | Some(libc::ESHUTDOWN) | Some(libc::EILSEQ)) {
                    shared.report(format!("MIDI OUT write failed: {e}"));
                    self.pending = None;
                    self.dropped += 1;
                    return Outcome::Dropped;
                } else if let Some(held) = self.throttle.hit(now) {
                    warn!("MIDI OUT write failed: {e} (+{held} more)");
                }
                if gave_up {
                    self.pending = None;
                    self.dropped += 1;
                    Outcome::Dropped
                } else {
                    Outcome::Retry
                }
            }
        }
    }
}

/// Run the writer until `stop`, sending to whatever `device()` returns
/// (`None` while the NS7II is being reconnected: messages stay queued,
/// coalesced and bounded).
pub fn run<S, D>(cfg: Config, shared: Arc<Shared>, stop: Arc<AtomicBool>, device: D)
where
    S: Sink + ?Sized,
    D: Fn() -> Option<Arc<S>>,
{
    let mut w = Writer::new(cfg);
    let mut last_report = Instant::now();
    while !stop.load(Ordering::SeqCst) {
        let Some(dev) = device() else {
            std::thread::sleep(Duration::from_millis(20));
            continue;
        };
        let wait = w.step(&shared, &*dev, Instant::now());
        drop(dev);
        if !wait.is_zero() {
            std::thread::sleep(wait);
        }
        if last_report.elapsed() >= Duration::from_secs(60) {
            let (bytes, st) = shared.status();
            if st.dropped > 0 || w.timeouts > 0 || w.dropped > 0 {
                info!(
                    "MIDI OUT: {} messages accepted, {} coalesced, {} dropped (queue full), {} transfers dropped, {} timeouts, {bytes} B queued, rate {} B/s",
                    st.accepted, st.coalesced, st.dropped, w.dropped, w.timeouts, w.pacer.rate()
                );
            }
            last_report = Instant::now();
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::cell::RefCell;
    use std::collections::VecDeque;

    /// A unit that answers writes from a script (`None` = success) and records them.
    #[derive(Default)]
    struct Fake {
        script: RefCell<VecDeque<Option<i32>>>,
        written: RefCell<Vec<Vec<u8>>>,
        halts: RefCell<u32>,
    }
    impl Sink for Fake {
        fn write(&self, payload: &[u8], _: Duration) -> io::Result<()> {
            match self.script.borrow_mut().pop_front().flatten() {
                Some(errno) => Err(io::Error::from_raw_os_error(errno)),
                None => {
                    self.written.borrow_mut().push(payload.to_vec());
                    Ok(())
                }
            }
        }
        fn clear_halt(&self) -> io::Result<()> {
            *self.halts.borrow_mut() += 1;
            Ok(())
        }
    }

    fn drive(w: &mut Writer, shared: &Shared, fake: &Fake, t: &mut Instant, steps: usize) {
        w.idle_wait = Duration::ZERO;
        for _ in 0..steps {
            let d = w.step(shared, fake, *t);
            *t += d.max(Duration::from_micros(100));
        }
    }

    #[test]
    fn timeouts_clear_the_halt_retry_and_slow_down() {
        let shared = Shared::new();
        let fake = Fake::default();
        fake.script.borrow_mut().extend([Some(libc::ETIMEDOUT), None]);
        shared.push(&[0xB0, 0x4A, 0x05]);
        let mut w = Writer::new(Config { rate: 4000, ..Config::default() });
        let mut t = Instant::now();
        drive(&mut w, &shared, &fake, &mut t, 50);
        assert_eq!(*fake.written.borrow(), vec![vec![0xB0, 0x4A, 0x05]]);
        assert_eq!(*fake.halts.borrow(), 1);
        assert_eq!(w.pacer.rate(), 2000);
        assert!(shared.take_fault().is_none());
    }

    #[test]
    fn a_run_of_timeouts_is_reported_as_a_stall_without_blocking() {
        let shared = Shared::new();
        let fake = Fake::default();
        fake.script.borrow_mut().extend(std::iter::repeat_n(Some(libc::ETIMEDOUT), 10_000));
        let mut w = Writer::new(Config::default());
        let mut t = Instant::now();
        for i in 0..40u8 {
            shared.push(&[0xB0, i, 1]);
            drive(&mut w, &shared, &fake, &mut t, 5);
        }
        assert!(shared.take_fault().unwrap().contains("stalled"));
        assert!(fake.written.borrow().is_empty());
    }

    #[test]
    fn protocol_errors_and_disconnects_are_faults() {
        for errno in [libc::EPROTO, libc::ENODEV] {
            let shared = Shared::new();
            let fake = Fake::default();
            fake.script.borrow_mut().push_back(Some(errno));
            shared.push(&[0x90, 0x01, 0x7F]);
            let mut w = Writer::new(Config::default());
            let mut t = Instant::now();
            drive(&mut w, &shared, &fake, &mut t, 3);
            assert!(shared.take_fault().is_some(), "errno {errno}");
        }
    }

    #[test]
    fn a_stalled_unit_does_not_grow_the_queue() {
        let shared = Shared::new();
        let fake = Fake::default();
        fake.script.borrow_mut().extend(std::iter::repeat_n(Some(libc::ETIMEDOUT), 1_000_000));
        let mut w = Writer::new(Config::default());
        let mut t = Instant::now();
        for i in 0..100_000u32 {
            shared.push(&[0xB0, 0x4A + (i % 4) as u8, (i % 12) as u8, 0x90, (i % 128) as u8, 0x7F]);
            shared.push(&[0xF0, 0x7E, 0x7F, 0x06, 0x01, 0xF7]);
            if i % 1000 == 0 {
                drive(&mut w, &shared, &fake, &mut t, 1);
            }
        }
        let (bytes, st) = shared.status();
        assert!(bytes <= ns7ii_proto::midi_out::DEFAULT_QUEUE_BYTES);
        assert!(st.dropped > 0);
    }

    #[test]
    fn steady_app_traffic_is_paced_and_delivered_latest_first() {
        let shared = Shared::new();
        let fake = Fake::default();
        let mut w = Writer::new(Config { rate: 3000, min_gap: Duration::from_millis(2), ..Config::default() });
        w.idle_wait = Duration::ZERO;
        let t0 = Instant::now();
        let mut t = t0;
        // Four VU meters at 30 Hz for 2 s, pushed in bursts.
        for frame in 0..60u32 {
            for m in 0..4u8 {
                shared.push(&[0xB0, 0x4A + m, (frame % 12) as u8]);
            }
            let end = t + Duration::from_millis(33);
            while t < end {
                let d = w.step(&shared, &fake, t);
                t += d.max(Duration::from_micros(200));
            }
        }
        let written = fake.written.borrow();
        let bytes: usize = written.iter().map(Vec::len).sum();
        let secs = (t - t0).as_secs_f64();
        assert!(bytes as f64 <= 3000.0 * secs + 39.0, "{bytes} B in {secs} s");
        // Gap respected.
        assert!(written.len() as f64 <= secs / 0.002 + 1.0);
        // The last value of every meter arrived.
        let all: Vec<u8> = written.concat();
        for m in 0..4u8 {
            let last = all.chunks(3).filter(|c| c[1] == 0x4A + m).last().unwrap();
            assert_eq!(last[2], 59 % 12);
        }
    }
}
