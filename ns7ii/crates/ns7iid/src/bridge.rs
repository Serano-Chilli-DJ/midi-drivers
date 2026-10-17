//! Error handling for the daemon's ALSA PCM bridges.
//!
//! A bridge PCM can fail for reasons a plain `snd_pcm_recover` cannot fix:
//! snd-aloop returns EIO for good once the loopback's other end is opened
//! with a different format, channel count or rate, and ENODEV when the card
//! goes away. Retrying such a handle every few milliseconds only floods the
//! journal. [`Bridge`] instead closes the handle, reopens it with
//! exponential backoff, and reports failures at most once per second with a
//! count and a hint about the likely cause.

use log::{info, warn};
use std::time::{Duration, Instant};

/// What a bridge reads from or writes to (an ALSA PCM, or a test double).
pub trait PcmIo {
    /// Read whole interleaved frames into `buf`; returns the bytes read.
    #[allow(dead_code)] // the capture bridge only writes; reading is tested
    fn read(&mut self, buf: &mut [u8]) -> Result<usize, ns7ii_alsa::Error>;
    /// Write whole interleaved frames from `buf`; returns the bytes written.
    fn write(&mut self, buf: &[u8]) -> Result<usize, ns7ii_alsa::Error>;
}

impl PcmIo for ns7ii_alsa::Pcm {
    fn read(&mut self, buf: &mut [u8]) -> Result<usize, ns7ii_alsa::Error> {
        ns7ii_alsa::Pcm::read(self, buf)
    }
    fn write(&mut self, buf: &[u8]) -> Result<usize, ns7ii_alsa::Error> {
        ns7ii_alsa::Pcm::write(self, buf)
    }
}

/// Exponential retry delay between `min` and `max`.
#[derive(Debug)]
pub struct Backoff {
    min: Duration,
    max: Duration,
    next: Duration,
}

impl Backoff {
    /// A backoff starting at `min` and doubling up to `max`.
    pub fn new(min: Duration, max: Duration) -> Self {
        Self { min, max, next: min }
    }
    /// The delay to wait now; the following call returns twice as much.
    pub fn next_delay(&mut self) -> Duration {
        let d = self.next;
        self.next = (self.next * 2).min(self.max);
        d
    }
    /// Start over from `min` (after a success).
    pub fn reset(&mut self) {
        self.next = self.min;
    }
}

/// Lets one report through per `interval` and counts the rest.
#[derive(Debug)]
pub struct Throttle {
    interval: Duration,
    last: Option<Instant>,
    suppressed: u64,
}

impl Throttle {
    /// A throttle allowing one report per `interval`.
    pub fn new(interval: Duration) -> Self {
        Self { interval, last: None, suppressed: 0 }
    }
    /// Note one event at `now`. Returns `Some(n)` when it should be
    /// reported, `n` being how many events were held back since the last
    /// report; `None` when it is held back.
    pub fn hit(&mut self, now: Instant) -> Option<u64> {
        match self.last {
            Some(t) if now.duration_since(t) < self.interval => {
                self.suppressed += 1;
                None
            }
            _ => {
                self.last = Some(now);
                Some(std::mem::take(&mut self.suppressed))
            }
        }
    }
}

/// Why an ALSA errno most likely happened on a bridge PCM, and what to do.
pub fn hint(code: Option<i32>) -> &'static str {
    match code.map(|c| -c) {
        Some(libc::EIO) | Some(libc::EINVAL) => {
            "the loopback's other end is probably open with a different format, channel count or rate \
             (snd-aloop needs both ends to match); open the loopback through plughw: on either end, \
             e.g. plughw:CARD=Loopback,DEV=0 in the app and plughw:CARD=Loopback,DEV=1,SUBDEV=0 for ns7iid"
        }
        Some(libc::EBUSY) => "another program has this PCM open; close it or pick another subdevice",
        Some(libc::ENODEV) | Some(libc::ENOENT) | Some(libc::ENXIO) => {
            "the PCM does not exist (is snd-aloop loaded with id=Loopback? see /proc/asound/cards)"
        }
        Some(libc::EACCES) | Some(libc::EPERM) => "no permission to open the PCM (is the user in the audio group?)",
        _ => "retrying",
    }
}

/// A bridge PCM that reopens itself after errors.
pub struct Bridge<P, F> {
    label: &'static str,
    name: String,
    open: F,
    pcm: Option<P>,
    retry_at: Option<Instant>,
    backoff: Backoff,
    throttle: Throttle,
    errors: u64,
}

impl<P: PcmIo, F: FnMut() -> Result<P, ns7ii_alsa::Error>> Bridge<P, F> {
    /// Wrap the already opened `pcm`; `open` reopens `name` after errors.
    /// `label` names the direction in log lines ("playback bridge").
    pub fn new(label: &'static str, name: &str, pcm: P, open: F) -> Self {
        Self::with(label, name, Some(pcm), open)
    }

    fn with(label: &'static str, name: &str, pcm: Option<P>, open: F) -> Self {
        Self {
            label,
            name: name.to_string(),
            open,
            pcm,
            retry_at: None,
            backoff: Backoff::new(Duration::from_millis(100), Duration::from_secs(2)),
            throttle: Throttle::new(Duration::from_secs(1)),
            errors: 0,
        }
    }

    /// Open `name` with `open` now. If that fails the error is logged and
    /// the bridge keeps retrying with backoff, so the daemon (and MIDI)
    /// still starts when, say, an app holds the loopback in another format.
    pub fn start(label: &'static str, name: &str, mut open: F, now: Instant) -> Self {
        match open() {
            Ok(p) => Self::new(label, name, p, open),
            Err(e) => {
                let mut b = Self::with(label, name, None, open);
                b.fail("open", &e, now);
                b
            }
        }
    }

    /// Read from the PCM. `None` means it is failing or waiting to be
    /// reopened; the caller should sleep briefly (the error is logged).
    #[allow(dead_code)] // the playback side moved to `playback`; kept for tests and other bridges
    pub fn read(&mut self, buf: &mut [u8], now: Instant) -> Option<usize> {
        self.io(now, |p| p.read(buf))
    }

    /// Write to the PCM. `None` means the data was dropped (see [`Bridge::read`]).
    pub fn write(&mut self, buf: &[u8], now: Instant) -> Option<usize> {
        self.io(now, |p| p.write(buf))
    }

    /// Whether a handle is currently open.
    #[cfg(test)]
    pub fn is_open(&self) -> bool {
        self.pcm.is_some()
    }

    fn io(&mut self, now: Instant, f: impl FnOnce(&mut P) -> Result<usize, ns7ii_alsa::Error>) -> Option<usize> {
        if self.pcm.is_none() {
            if self.retry_at.is_some_and(|t| now < t) {
                return None;
            }
            match (self.open)() {
                Ok(p) => self.pcm = Some(p),
                Err(e) => {
                    self.fail("reopen", &e, now);
                    return None;
                }
            }
        }
        let pcm = self.pcm.as_mut()?;
        match f(pcm) {
            Ok(n) => {
                if self.errors > 0 {
                    info!("{} {}: working again after {} error(s)", self.label, self.name, self.errors);
                    self.errors = 0;
                }
                self.backoff.reset();
                self.retry_at = None;
                Some(n)
            }
            Err(e) => {
                // Close now: a loopback end stuck in EIO only recovers on reopen.
                self.pcm = None;
                self.fail("I/O", &e, now);
                None
            }
        }
    }

    fn fail(&mut self, what: &str, e: &ns7ii_alsa::Error, now: Instant) {
        self.errors += 1;
        let delay = self.backoff.next_delay();
        self.retry_at = Some(now + delay);
        if let Some(held) = self.throttle.hit(now) {
            let more = if held > 0 { format!(" (+{held} more since the last report)") } else { String::new() };
            warn!(
                "{} {}: {what} failed: {e}{more}; {}; reopening in {} ms",
                self.label,
                self.name,
                hint(e.code()),
                delay.as_millis()
            );
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::cell::RefCell;
    use std::collections::VecDeque;
    use std::rc::Rc;

    fn err(code: i32) -> ns7ii_alsa::Error {
        ns7ii_alsa::Error::Call { call: "snd_pcm_readi", msg: "test".into(), code }
    }

    #[test]
    fn backoff_doubles_up_to_max_and_resets() {
        let mut b = Backoff::new(Duration::from_millis(100), Duration::from_millis(500));
        let got: Vec<u128> = (0..5).map(|_| b.next_delay().as_millis()).collect();
        assert_eq!(got, [100, 200, 400, 500, 500]);
        b.reset();
        assert_eq!(b.next_delay(), Duration::from_millis(100));
    }

    #[test]
    fn throttle_reports_once_per_interval_with_count() {
        let t0 = Instant::now();
        let mut t = Throttle::new(Duration::from_secs(1));
        assert_eq!(t.hit(t0), Some(0));
        for i in 1..=99 {
            assert_eq!(t.hit(t0 + Duration::from_millis(i * 10 - 5)), None);
        }
        assert_eq!(t.hit(t0 + Duration::from_millis(1000)), Some(99));
        assert_eq!(t.hit(t0 + Duration::from_millis(1500)), None);
        assert_eq!(t.hit(t0 + Duration::from_millis(2500)), Some(1));
    }

    #[test]
    fn hints_name_the_format_mismatch_for_eio() {
        assert!(hint(Some(-libc::EIO)).contains("plughw:"));
        assert!(hint(Some(-libc::EIO)).contains("format"));
        assert!(hint(Some(-libc::EBUSY)).contains("another program"));
        assert!(hint(Some(-libc::ENOENT)).contains("/proc/asound/cards"));
        assert_eq!(hint(None), "retrying");
    }

    /// A PCM that pops scripted results, and counts opens.
    struct Fake {
        script: Rc<RefCell<VecDeque<Result<usize, i32>>>>,
    }
    impl PcmIo for Fake {
        fn read(&mut self, _: &mut [u8]) -> Result<usize, ns7ii_alsa::Error> {
            self.script.borrow_mut().pop_front().unwrap_or(Ok(0)).map_err(err)
        }
        fn write(&mut self, b: &[u8]) -> Result<usize, ns7ii_alsa::Error> {
            self.script.borrow_mut().pop_front().unwrap_or(Ok(b.len())).map_err(err)
        }
    }

    #[test]
    fn eio_closes_and_reopens_with_backoff_instead_of_spinning() {
        let script = Rc::new(RefCell::new(VecDeque::from([Ok(6), Err(-libc::EIO), Err(-libc::EIO), Ok(6)])));
        let opens = Rc::new(RefCell::new(Vec::<Result<(), i32>>::from([Err(-libc::EINVAL), Ok(()), Ok(())])));
        let s2 = Rc::clone(&script);
        let o2 = Rc::clone(&opens);
        let open = move || -> Result<Fake, ns7ii_alsa::Error> {
            let r = o2.borrow_mut().remove(0);
            r.map(|()| Fake { script: Rc::clone(&s2) }).map_err(err)
        };
        let mut b = Bridge::new("playback bridge", "plughw:Loopback,1,0", Fake { script: Rc::clone(&script) }, open);
        let t0 = Instant::now();
        let ms = |n| t0 + Duration::from_millis(n);
        let mut buf = [0u8; 6];

        assert_eq!(b.read(&mut buf, ms(0)), Some(6));
        // EIO: handle closed, next attempt only after 100 ms.
        assert_eq!(b.read(&mut buf, ms(1)), None);
        assert!(!b.is_open());
        for t in 2..100 {
            assert_eq!(b.read(&mut buf, ms(t)), None);
        }
        assert_eq!(opens.borrow().len(), 3, "no reopen inside the backoff window");
        // First reopen fails (EINVAL): wait 200 ms more.
        assert_eq!(b.read(&mut buf, ms(101)), None);
        assert_eq!(opens.borrow().len(), 2);
        assert_eq!(b.read(&mut buf, ms(250)), None);
        assert_eq!(opens.borrow().len(), 2);
        // Reopens, reads EIO again: 400 ms.
        assert_eq!(b.read(&mut buf, ms(301)), None);
        assert_eq!(opens.borrow().len(), 1);
        assert_eq!(b.read(&mut buf, ms(600)), None);
        // Reopens and works; backoff resets.
        assert_eq!(b.read(&mut buf, ms(701)), Some(6));
        assert!(b.is_open());
        assert_eq!(b.backoff.next_delay(), Duration::from_millis(100));
    }

    #[test]
    fn failed_first_open_retries_later() {
        let mut tries = 0;
        let t0 = Instant::now();
        let mut b = Bridge::start("capture bridge", "x", move || {
            tries += 1;
            if tries == 1 { Err(err(-libc::EBUSY)) } else { Ok(Fake { script: Rc::new(RefCell::new(VecDeque::new())) }) }
        }, t0);
        assert!(!b.is_open());
        assert_eq!(b.write(&[0; 6], t0 + Duration::from_millis(10)), None);
        assert_eq!(b.write(&[0; 6], t0 + Duration::from_millis(100)), Some(6));
    }

    #[test]
    fn write_errors_drop_data_and_reopen() {
        let script = Rc::new(RefCell::new(VecDeque::from([Err(-libc::ENODEV)])));
        let s2 = Rc::clone(&script);
        let mut b = Bridge::new("capture bridge", "x", Fake { script: Rc::clone(&script) }, move || Ok(Fake { script: Rc::clone(&s2) }));
        let t0 = Instant::now();
        assert_eq!(b.write(&[0; 6], t0), None);
        assert_eq!(b.write(&[0; 6], t0 + Duration::from_millis(50)), None);
        assert_eq!(b.write(&[0; 6], t0 + Duration::from_millis(100)), Some(6));
    }
}
