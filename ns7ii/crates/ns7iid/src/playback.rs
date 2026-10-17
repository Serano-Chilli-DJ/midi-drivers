//! The playback bridge: app audio from the loopback to the NS7II.
//!
//! Three clocks meet here. The app writes at its own rate (often 48 kHz)
//! into snd-aloop, which runs on the kernel's timer; the NS7II plays at
//! 44.1 kHz on its own crystal and, through its feedback endpoint, pulls
//! exactly that many frames from [`AudioSource`]. The bridge therefore
//!
//! * opens its loopback end at whatever rate the app chose and converts
//!   with [`Resampler`] (instead of alsa-lib's linear `plughw:` converter);
//! * keeps the queue in front of the USB stream at a target level with a
//!   slow PI controller ([`Drift`]) that nudges the resampling ratio by a few
//!   ppm, so neither side ever over- or underflows and nothing clicks;
//! * holds the loopback only while an app has it open (seen in
//!   `/proc/asound/<card>/pcm<dev>p/sub<n>/status`), so the app always gets
//!   its own format, and while nothing plays it sends silence without
//!   logging errors (one info line when an app starts and stops);
//! * logs underruns, trims and the queue level / clock correction, rate-limited.

use crate::bridge::{hint, Backoff, Throttle};
use crate::resample::Resampler;
use log::{debug, info, warn};
use ns7ii_proto::pcm::{Plain24, WireCodec};
use ns7ii_usb::AudioSource;
use std::path::PathBuf;
use std::time::{Duration, Instant};

/// What the app side of the loopback is doing.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Peer {
    /// Not a loopback, or its status cannot be read: always bridge.
    Unknown,
    /// No app has the other end open.
    Closed,
    /// Opened but not configured yet (hw_params not set).
    Opening,
    /// Configured; `running` while it actually plays. `owner` names the app.
    Ready { running: bool, owner: Option<String> },
}

impl Peer {
    fn wants_bridge(&self) -> bool {
        matches!(self, Peer::Unknown | Peer::Ready { .. })
    }
    fn running(&self) -> bool {
        matches!(self, Peer::Unknown | Peer::Ready { running: true, .. })
    }
}

/// Parse a `/proc/asound/.../status` file.
pub fn parse_status(text: &str) -> Peer {
    let text = text.trim();
    if text.starts_with("closed") {
        return Peer::Closed;
    }
    let field = |name: &str| {
        text.lines()
            .find_map(|l| l.split_once(':').filter(|(k, _)| k.trim() == name).map(|(_, v)| v.trim().to_string()))
    };
    match field("state").as_deref() {
        Some("OPEN") => Peer::Opening,
        Some(state) => Peer::Ready {
            running: state == "RUNNING",
            owner: field("owner_pid").and_then(|pid| {
                let comm = std::fs::read_to_string(format!("/proc/{pid}/comm")).ok()?;
                Some(format!("{} (pid {pid})", comm.trim()))
            }),
        },
        None => Peer::Unknown,
    }
}

/// The `/proc` status file of the app's end of the loopback cable that
/// `pcm_name` (ns7iid's capture end, e.g. `plughw:CARD=Loopback,DEV=1,SUBDEV=0`)
/// reads from: same card and subdevice, the other device, playback direction.
/// `None` when the name is not a snd-aloop device.
pub fn peer_status_path(pcm_name: &str, proc_root: &str) -> Option<PathBuf> {
    let (_, args) = pcm_name.split_once(':')?;
    let (mut card, mut dev, mut sub) = (None, None, None);
    for (i, part) in args.split(',').enumerate() {
        let (k, v) = match part.split_once('=') {
            Some((k, v)) => (k.trim().to_ascii_uppercase(), v.trim()),
            None => (["CARD", "DEV", "SUBDEV"].get(i)?.to_string(), part.trim()),
        };
        match k.as_str() {
            "CARD" => card = Some(v.trim_matches('"').to_string()),
            "DEV" => dev = v.parse::<u32>().ok(),
            "SUBDEV" => sub = v.parse::<u32>().ok(),
            _ => {}
        }
    }
    let card = card?;
    let card_dir = if card.chars().all(|c| c.is_ascii_digit()) { format!("card{card}") } else { card };
    let peer_dev = match dev.unwrap_or(0) {
        0 => 1,
        1 => 0,
        _ => return None,
    };
    let base = PathBuf::from(proc_root).join(card_dir).join(format!("pcm{peer_dev}p"));
    let info = std::fs::read_to_string(base.join("info")).ok()?;
    if !info.contains("Loopback") {
        return None;
    }
    Some(base.join(format!("sub{}", sub.unwrap_or(0))).join("status"))
}

/// A capture PCM the bridge reads (ALSA, or a fake in tests).
pub trait Capture {
    /// Non-blocking read of whole S24_3LE frames; 0 when nothing is there.
    fn read(&mut self, buf: &mut [u8]) -> Result<usize, ns7ii_alsa::Error>;
    /// Frame rate.
    fn rate(&self) -> u32;
}

impl Capture for ns7ii_alsa::Pcm {
    fn read(&mut self, buf: &mut [u8]) -> Result<usize, ns7ii_alsa::Error> {
        ns7ii_alsa::Pcm::read(self, buf)
    }
    fn rate(&self) -> u32 {
        ns7ii_alsa::Pcm::rate(self)
    }
}

/// How the bridge opens its PCM and watches the app (ALSA + /proc, or fakes).
pub trait Env {
    /// The PCM type.
    type Pcm: Capture;
    /// Open the loopback end.
    fn open(&mut self) -> Result<Self::Pcm, ns7ii_alsa::Error>;
    /// The app side's state.
    fn peer(&mut self) -> Peer;
}

/// The real environment: an ALSA PCM name and its loopback peer's status file.
pub struct AlsaEnv {
    alsa: ns7ii_alsa::Alsa,
    name: String,
    channels: u32,
    rate: u32,
    status: Option<PathBuf>,
}

impl AlsaEnv {
    /// Bridge `name` (`channels` channels, preferring `rate`).
    pub fn new(alsa: ns7ii_alsa::Alsa, name: &str, channels: u32, rate: u32) -> Self {
        let status = peer_status_path(name, "/proc/asound");
        match &status {
            Some(p) => info!("playback bridge: following the app side of the loopback through {}", p.display()),
            None => info!("playback bridge: {name} is not a snd-aloop device; reading it continuously"),
        }
        Self { alsa, name: name.to_string(), channels, rate, status }
    }
}

impl Env for AlsaEnv {
    type Pcm = ns7ii_alsa::Pcm;
    fn open(&mut self) -> Result<Self::Pcm, ns7ii_alsa::Error> {
        self.alsa.open_capture_native_rate(&self.name, self.channels, self.rate, 40_000)
    }
    fn peer(&mut self) -> Peer {
        match &self.status {
            None => Peer::Unknown,
            Some(p) => std::fs::read_to_string(p).map(|t| parse_status(&t)).unwrap_or(Peer::Unknown),
        }
    }
}

/// PI controller keeping the queue at its target by nudging the ratio.
#[derive(Debug, Clone)]
pub struct Drift {
    target: f64,
    rate: f64,
    level: Option<f64>,
    integral: f64,
    correction: f64,
    last: Option<Instant>,
}

/// Low-pass time constant of the level measurement.
const LEVEL_TAU: f64 = 2.0;
/// Proportional gain: correction per second of level error (20 s time constant).
const KP: f64 = 0.05;
/// Integral gain (critically damped with [`KP`]).
const KI: f64 = KP * KP / 4.0;
/// Largest correction (clock differences are tens to a few hundred ppm).
const MAX_CORRECTION: f64 = 2000e-6;

impl Drift {
    /// Keep `target` frames queued at `rate` frames per second.
    pub fn new(target: usize, rate: u32) -> Self {
        Self { target: target as f64, rate: rate as f64, level: None, integral: 0.0, correction: 0.0, last: None }
    }

    /// Feed the queue level at `now`; returns the ratio correction (positive:
    /// consume input faster).
    pub fn update(&mut self, level: f64, now: Instant) -> f64 {
        let dt = self.last.map_or(0.0, |t| now.saturating_duration_since(t).as_secs_f64()).min(0.5);
        self.last = Some(now);
        let filtered = match self.level {
            None => level,
            Some(l) => l + (level - l) * (dt / LEVEL_TAU).min(1.0),
        };
        self.level = Some(filtered);
        let err = (filtered - self.target) / self.rate; // seconds
        self.integral = (self.integral + KI * err * dt).clamp(-MAX_CORRECTION, MAX_CORRECTION);
        self.correction = (KP * err + self.integral).clamp(-MAX_CORRECTION, MAX_CORRECTION);
        self.correction
    }

    /// Restart the level filter (after a jump: prefill, trim, reopen). The
    /// integral, i.e. the learnt clock difference, is kept.
    pub fn restart(&mut self) {
        self.level = None;
        self.last = None;
    }

    /// Filtered level in frames.
    pub fn level(&self) -> f64 {
        self.level.unwrap_or(0.0)
    }

    /// Current correction.
    pub fn correction(&self) -> f64 {
        self.correction
    }
}

/// Counters, for logs and tests.
#[derive(Debug, Default, Clone, Copy, PartialEq, Eq)]
pub struct Counters {
    /// Underrun events while an app played (silence inserted).
    pub underruns: u64,
    /// Frames of silence inserted while an app played.
    pub underrun_frames: u64,
    /// Times the queue was trimmed because nothing consumed it.
    pub trims: u64,
    /// Warnings logged.
    pub warnings: u64,
    /// Read errors while the app was not playing (handled quietly).
    pub quiet_errors: u64,
    /// Successful opens.
    pub opens: u64,
}

struct Open<P> {
    pcm: P,
    rs: Resampler,
}

/// The bridge state machine. Call [`Playback::step`] every couple of ms.
pub struct Playback<E: Env> {
    env: E,
    label: String,
    channels: usize,
    frame_bytes: usize,
    out_rate: u32,
    codec: Box<dyn WireCodec + Send>,
    source: AudioSource,
    open: Option<Open<E::Pcm>>,
    drift: Drift,
    target: usize,
    backoff: Backoff,
    retry_at: Option<Instant>,
    throttle: Throttle,
    underrun_throttle: Throttle,
    last_peer: Option<Peer>,
    last_underrun: u64,
    last_pulled: (u64, Instant),
    consumer_running: bool,
    last_status: Instant,
    counters: Counters,
    buf: Vec<u8>,
    samples: Vec<i32>,
    resampled: Vec<i32>,
    wire: Vec<u8>,
}

/// Frames read per step at most.
const CHUNK: usize = 1024;
/// The queue is trimmed back to target when it exceeds the target by this.
const OVERFLOW: Duration = Duration::from_millis(80);
/// How long the consumer may pull nothing before it counts as stopped.
const CONSUMER_IDLE: Duration = Duration::from_millis(100);
/// Interval of the status line while an app plays.
const STATUS_EVERY: Duration = Duration::from_secs(60);

impl<E: Env> Playback<E> {
    /// A bridge feeding `source` with `channels`-channel audio encoded by
    /// `codec` for a device at `out_rate`, keeping `target_ms` queued.
    pub fn new(
        env: E,
        label: &str,
        channels: usize,
        out_rate: u32,
        codec: Box<dyn WireCodec + Send>,
        source: AudioSource,
        target_ms: u32,
        now: Instant,
    ) -> Self {
        let frame_bytes = codec.out_frame_bytes();
        let target = (out_rate as u64 * target_ms as u64 / 1000) as usize;
        Self {
            env,
            label: label.to_string(),
            channels,
            frame_bytes,
            out_rate,
            codec,
            last_underrun: source.underrun(),
            last_pulled: (source.pulled(), now),
            source,
            open: None,
            drift: Drift::new(target, out_rate),
            target,
            backoff: Backoff::new(Duration::from_millis(100), Duration::from_secs(2)),
            retry_at: None,
            throttle: Throttle::new(Duration::from_secs(1)),
            underrun_throttle: Throttle::new(Duration::from_secs(5)),
            last_peer: None,
            consumer_running: false,
            last_status: now,
            counters: Counters::default(),
            buf: vec![0; CHUNK * channels * 3],
            samples: vec![0; CHUNK * channels],
            resampled: Vec::new(),
            wire: Vec::new(),
        }
    }

    /// Counters so far.
    #[cfg(test)]
    pub fn counters(&self) -> Counters {
        self.counters
    }

    /// The controller's state.
    #[cfg(test)]
    pub fn drift(&self) -> &Drift {
        &self.drift
    }

    fn level_frames(&self) -> f64 {
        (self.source.queued() / self.frame_bytes) as f64
    }

    fn close(&mut self, why: &str) {
        if self.open.take().is_some() {
            debug!("{}: closed ({why})", self.label);
        }
    }

    /// One step at `now`. Returns how long to sleep before the next.
    pub fn step(&mut self, now: Instant) -> Duration {
        let peer = self.env.peer();
        if self.last_peer.as_ref() != Some(&peer) {
            self.peer_changed(&peer);
            self.last_peer = Some(peer.clone());
        }
        self.watch_consumer(now);
        if !peer.wants_bridge() {
            self.close("no app on the loopback");
            self.retry_at = None;
            self.backoff.reset();
            return Duration::from_millis(20);
        }
        if self.open.is_none() && !self.try_open(now) {
            return Duration::from_millis(20);
        }
        self.pump(now, &peer);
        Duration::from_millis(2)
    }

    fn peer_changed(&mut self, peer: &Peer) {
        match peer {
            Peer::Closed | Peer::Opening if matches!(self.last_peer, Some(Peer::Ready { .. })) || self.last_peer.is_none() => {
                info!("{}: no app playing into the loopback; sending silence to the NS7II", self.label)
            }
            Peer::Ready { running: true, owner } => {
                info!("{}: {} started playing into the loopback", self.label, owner.as_deref().unwrap_or("an app"))
            }
            Peer::Ready { running: false, owner } if matches!(self.last_peer, Some(Peer::Ready { running: true, .. })) => {
                info!("{}: {} stopped playing (stream open, idle)", self.label, owner.as_deref().unwrap_or("the app"))
            }
            _ => {}
        }
    }

    /// Track whether the USB side consumes; trim the queue when it does not.
    fn watch_consumer(&mut self, now: Instant) {
        let pulled = self.source.pulled();
        if pulled != self.last_pulled.0 {
            self.last_pulled = (pulled, now);
            self.consumer_running = true;
        } else if now.saturating_duration_since(self.last_pulled.1) >= CONSUMER_IDLE {
            self.consumer_running = false;
        }
        let underrun = self.source.underrun();
        let new = underrun - self.last_underrun;
        self.last_underrun = underrun;
        // Silence while no app plays is the point, not an underrun.
        if new > 0 && self.last_peer.as_ref().is_some_and(Peer::running) && self.open.is_some() && self.counters.opens > 0 {
            let frames = new / self.frame_bytes as u64;
            self.counters.underruns += 1;
            self.counters.underrun_frames += frames;
            if let Some(held) = self.underrun_throttle.hit(now) {
                self.counters.warnings += 1;
                warn!(
                    "{}: playback underrun, {frames} frames of silence sent to the NS7II ({} underruns, {} frames in total, +{held} since the last report); queue {:.1} ms, target {:.1} ms, clock correction {:+.0} ppm",
                    self.label,
                    self.counters.underruns,
                    self.counters.underrun_frames,
                    self.level_frames() * 1000.0 / self.out_rate as f64,
                    self.target as f64 * 1000.0 / self.out_rate as f64,
                    self.drift.correction() * 1e6
                );
            }
            self.drift.restart();
        }
    }

    fn try_open(&mut self, now: Instant) -> bool {
        if self.retry_at.is_some_and(|t| now < t) {
            return false;
        }
        match self.env.open() {
            Ok(pcm) => {
                let rate = pcm.rate();
                let mut rs = Resampler::new(self.channels, rate, self.out_rate);
                rs.set_correction(self.drift.correction());
                info!(
                    "{}: reading {} ch at {rate} Hz, resampling to {} Hz with clock-drift correction (queue target {:.0} ms)",
                    self.label,
                    self.channels,
                    self.out_rate,
                    self.target as f64 * 1000.0 / self.out_rate as f64
                );
                self.open = Some(Open { pcm, rs });
                self.counters.opens += 1;
                self.backoff.reset();
                self.retry_at = None;
                // Start at the target level so the first milliseconds cannot underrun.
                let have = self.source.queued() / self.frame_bytes;
                if have < self.target {
                    self.source.push(&vec![0u8; (self.target - have) * self.frame_bytes]);
                }
                self.last_underrun = self.source.underrun();
                self.drift.restart();
                true
            }
            Err(e) => {
                self.fail("open", &e, now);
                false
            }
        }
    }

    fn fail(&mut self, what: &str, e: &ns7ii_alsa::Error, now: Instant) {
        let delay = self.backoff.next_delay();
        self.retry_at = Some(now + delay);
        // An app that is not playing (or just went away) makes the loopback
        // fail in ways that need no attention: retry quietly.
        let quiet = !self.last_peer.as_ref().is_some_and(Peer::running);
        if quiet {
            self.counters.quiet_errors += 1;
            debug!("{}: {what} failed while no app plays: {e}; retrying in {} ms", self.label, delay.as_millis());
            return;
        }
        if let Some(held) = self.throttle.hit(now) {
            self.counters.warnings += 1;
            let more = if held > 0 { format!(" (+{held} more since the last report)") } else { String::new() };
            warn!("{}: {what} failed: {e}{more}; {}; retrying in {} ms", self.label, hint(e.code()), delay.as_millis());
        }
    }

    fn pump(&mut self, now: Instant, peer: &Peer) {
        let Some(open) = self.open.as_mut() else { return };
        let frame_in = self.channels * 3;
        loop {
            match open.pcm.read(&mut self.buf) {
                Ok(0) => break,
                Ok(n) => {
                    let plain = Plain24 { out_channels: self.channels, in_channels: self.channels };
                    let got = plain.decode(&self.buf[..n - n % frame_in], &mut self.samples);
                    self.resampled.clear();
                    open.rs.process(&self.samples[..got], &mut self.resampled);
                    let frames = self.resampled.len() / self.channels;
                    self.wire.resize(frames * self.frame_bytes, 0);
                    let m = self.codec.encode(&self.resampled, &mut self.wire);
                    self.source.push(&self.wire[..m]);
                    if n < self.buf.len() {
                        break;
                    }
                }
                Err(e) => {
                    self.open = None;
                    self.fail("read", &e, now);
                    return;
                }
            }
        }
        // Nothing consumes (no USB session): do not let the queue grow.
        let level = self.level_frames();
        let overflow = (self.out_rate as f64 * OVERFLOW.as_secs_f64()) as usize;
        if level as usize > self.target + overflow {
            let dropped = self.source.trim_to(self.target * self.frame_bytes) / self.frame_bytes;
            self.counters.trims += 1;
            if self.consumer_running {
                if let Some(held) = self.throttle.hit(now) {
                    self.counters.warnings += 1;
                    warn!("{}: playback queue overflowed; dropped {dropped} frames (+{held} more trims)", self.label);
                }
            } else {
                debug!("{}: no USB stream consuming; dropped {dropped} queued frames", self.label);
            }
            self.drift.restart();
            return;
        }
        if self.consumer_running {
            let c = self.drift.update(level, now);
            if let Some(o) = self.open.as_mut() {
                o.rs.set_correction(c);
            }
        }
        if peer.running() && now.saturating_duration_since(self.last_status) >= STATUS_EVERY {
            self.last_status = now;
            info!(
                "{}: queue {:.1} ms (target {:.0} ms), clock correction {:+.0} ppm, {} underruns ({} frames)",
                self.label,
                self.drift.level() * 1000.0 / self.out_rate as f64,
                self.target as f64 * 1000.0 / self.out_rate as f64,
                self.drift.correction() * 1e6,
                self.counters.underruns,
                self.counters.underrun_frames
            );
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::cell::{Cell, RefCell};
    use std::rc::Rc;

    #[test]
    fn status_files_are_understood() {
        assert_eq!(parse_status("closed\n"), Peer::Closed);
        assert_eq!(parse_status("state: OPEN\nowner_pid   : 999999999\n"), Peer::Opening);
        assert_eq!(
            parse_status("state: RUNNING\nowner_pid   : 999999999\ntrigger_time: 1.0\n"),
            Peer::Ready { running: true, owner: None }
        );
        assert_eq!(parse_status("state: PREPARED\n"), Peer::Ready { running: false, owner: None });
        assert_eq!(parse_status("garbage"), Peer::Unknown);
    }

    #[test]
    fn the_peer_of_each_loopback_end_is_found() {
        let dir = std::env::temp_dir().join(format!("ns7iid-proc-{}", std::process::id()));
        for (card, dev) in [("Loopback", 0), ("Loopback", 1), ("card3", 0)] {
            let d = dir.join(card).join(format!("pcm{dev}p"));
            std::fs::create_dir_all(d.join("sub0")).unwrap();
            std::fs::write(d.join("info"), "card: 3\ndevice: 0\nid: Loopback PCM\n").unwrap();
        }
        let root = dir.to_str().unwrap();
        let p = peer_status_path("plughw:CARD=Loopback,DEV=1,SUBDEV=0", root).unwrap();
        assert!(p.ends_with("Loopback/pcm0p/sub0/status"), "{p:?}");
        let p = peer_status_path("plughw:Loopback,0,0", root).unwrap();
        assert!(p.ends_with("Loopback/pcm1p/sub0/status"));
        let p = peer_status_path("hw:3,1", root).unwrap();
        assert!(p.ends_with("card3/pcm0p/sub0/status"));
        assert!(peer_status_path("default", root).is_none());
        assert!(peer_status_path("plughw:CARD=PCH,DEV=0", root).is_none());
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn the_controller_settles_on_the_clock_difference() {
        // Producer 150 ppm faster than the consumer: the queue must stay near
        // target and the correction converge to about +150 ppm.
        let t0 = Instant::now();
        let mut d = Drift::new(882, 44_100);
        let mut level = 882.0;
        let mut t = t0;
        let ppm = 150e-6;
        for _ in 0..(600 * 500) {
            t += Duration::from_millis(2);
            let c = d.update(level, t);
            level += 44.1 * 2.0 * (ppm - c);
        }
        assert!((d.correction() - ppm).abs() < 5e-6, "{}", d.correction());
        assert!((level - 882.0).abs() < 20.0, "{level}");
    }

    /// Simulated loopback: an app at `rate` Hz on a clock `ppm` off,
    /// readable whenever simulated time has advanced.
    struct FakePcm {
        now: Rc<Cell<Instant>>,
        start: Instant,
        rate: f64,
        delivered: u64,
        channels: usize,
        fail: Rc<RefCell<Option<i32>>>,
    }
    impl Capture for FakePcm {
        fn read(&mut self, buf: &mut [u8]) -> Result<usize, ns7ii_alsa::Error> {
            if let Some(code) = self.fail.borrow_mut().take() {
                return Err(ns7ii_alsa::Error::Call { call: "snd_pcm_readi", msg: "test".into(), code });
            }
            // Delivered in 5 ms periods, like snd-aloop.
            let elapsed = self.now.get().duration_since(self.start).as_secs_f64();
            let period = (self.rate * 0.005) as u64;
            let avail = ((elapsed * self.rate) as u64 / period * period).saturating_sub(self.delivered);
            let frames = (avail as usize).min(buf.len() / (self.channels * 3));
            self.delivered += frames as u64;
            // A quiet 1 kHz-ish ramp so the data is not all zeros.
            for (i, b) in buf[..frames * self.channels * 3].iter_mut().enumerate() {
                *b = (i % 7) as u8;
            }
            Ok(frames * self.channels * 3)
        }
        fn rate(&self) -> u32 {
            self.rate.round() as u32
        }
    }

    struct FakeEnv {
        now: Rc<Cell<Instant>>,
        peer: Rc<RefCell<Peer>>,
        rate: f64,
        fail: Rc<RefCell<Option<i32>>>,
        open_fails: Rc<RefCell<u32>>,
    }
    impl Env for FakeEnv {
        type Pcm = FakePcm;
        fn open(&mut self) -> Result<FakePcm, ns7ii_alsa::Error> {
            if *self.open_fails.borrow() > 0 {
                *self.open_fails.borrow_mut() -= 1;
                return Err(ns7ii_alsa::Error::Call { call: "snd_pcm_open", msg: "test".into(), code: -libc::EIO });
            }
            Ok(FakePcm { now: Rc::clone(&self.now), start: self.now.get(), rate: self.rate, delivered: 0, channels: 4, fail: Rc::clone(&self.fail) })
        }
        fn peer(&mut self) -> Peer {
            self.peer.borrow().clone()
        }
    }

    struct Sim {
        now: Rc<Cell<Instant>>,
        peer: Rc<RefCell<Peer>>,
        fail: Rc<RefCell<Option<i32>>>,
        open_fails: Rc<RefCell<u32>>,
        pb: Playback<FakeEnv>,
        source: AudioSource,
        consumed: f64,
        device_rate: f64,
        usb: bool,
    }

    impl Sim {
        fn new(app_rate: f64, device_rate: f64, peer: Peer) -> Self {
            let now = Rc::new(Cell::new(Instant::now()));
            let peer = Rc::new(RefCell::new(peer));
            let fail = Rc::new(RefCell::new(None));
            let open_fails = Rc::new(RefCell::new(0));
            let env = FakeEnv { now: Rc::clone(&now), peer: Rc::clone(&peer), rate: app_rate, fail: Rc::clone(&fail), open_fails: Rc::clone(&open_fails) };
            let source = AudioSource::new();
            let codec = Box::new(Plain24 { out_channels: 4, in_channels: 4 });
            let pb = Playback::new(env, "playback bridge", 4, 44_100, codec, source.clone(), 20, now.get());
            Self { now, peer, fail, open_fails, pb, source, consumed: 0.0, device_rate, usb: true }
        }

        /// Run `secs` of simulated time: the bridge steps every 2 ms, the USB
        /// side pulls 4 ms URBs at the device's rate.
        fn run(&mut self, secs: f64) {
            let steps = (secs / 0.001) as usize;
            let mut urb = vec![0u8; 0];
            for i in 0..steps {
                self.now.set(self.now.get() + Duration::from_millis(1));
                if i % 2 == 0 {
                    self.pb.step(self.now.get());
                }
                if self.usb && i % 4 == 0 {
                    self.consumed += self.device_rate * 0.004;
                    let frames = self.consumed.floor() as usize;
                    self.consumed -= frames as f64;
                    urb.resize(frames * 12, 0);
                    self.source.pull(&mut urb);
                }
            }
        }

        fn level_ms(&self) -> f64 {
            (self.source.queued() / 12) as f64 / 44.1
        }
    }

    fn playing() -> Peer {
        Peer::Ready { running: true, owner: Some("serano (pid 1)".into()) }
    }

    #[test]
    fn drift_between_loopback_and_device_never_underruns_or_overflows() {
        for (app, dev) in [(44_100.0 * (1.0 + 200e-6), 44_100.0), (48_000.0, 44_100.0 * (1.0 + 300e-6)), (48_000.0 * (1.0 - 250e-6), 44_100.0)] {
            let mut sim = Sim::new(app, dev, playing());
            sim.run(5.0);
            let early = sim.pb.counters();
            sim.run(300.0);
            let c = sim.pb.counters();
            assert_eq!(c.underruns, 0, "app {app} dev {dev}: {c:?} (after 5 s: {early:?})");
            assert_eq!(c.trims, 0, "{c:?}");
            assert_eq!(c.warnings, 0, "{c:?}");
            assert!((sim.level_ms() - 20.0).abs() < 8.0, "level {} ms", sim.level_ms());
            // The learnt correction matches the clock ratio.
            let want = (app / app.round()) / (dev / 44_100.0) - 1.0;
            assert!((sim.pb.drift().correction() - want).abs() < 20e-6, "{} vs {want}", sim.pb.drift().correction());
        }
    }

    #[test]
    fn no_app_means_silence_and_no_warnings() {
        let mut sim = Sim::new(44_100.0, 44_100.0, Peer::Closed);
        sim.run(30.0);
        let c = sim.pb.counters();
        assert_eq!((c.opens, c.warnings, c.underruns), (0, 0, 0), "{c:?}");
        // Then an app starts: the bridge opens and plays without underruns.
        *sim.peer.borrow_mut() = playing();
        sim.run(10.0);
        let c = sim.pb.counters();
        assert_eq!((c.opens, c.warnings, c.underruns), (1, 0, 0), "{c:?}");
        // It stops and closes: the bridge lets go and stays quiet.
        *sim.peer.borrow_mut() = Peer::Closed;
        sim.run(10.0);
        let c = sim.pb.counters();
        assert_eq!((c.warnings, c.underruns), (0, 0), "{c:?}");
        assert!(sim.pb.open.is_none());
    }

    #[test]
    fn errors_while_idle_are_quiet_and_real_errors_are_reported() {
        let mut sim = Sim::new(44_100.0, 44_100.0, Peer::Ready { running: false, owner: None });
        sim.run(1.0);
        *sim.fail.borrow_mut() = Some(-libc::EIO);
        *sim.open_fails.borrow_mut() = 3;
        sim.run(5.0);
        let c = sim.pb.counters();
        assert_eq!(c.warnings, 0, "{c:?}");
        assert!(c.quiet_errors >= 2, "{c:?}");
        assert_eq!(c.opens, 2, "reopened after the quiet failures");
        *sim.peer.borrow_mut() = playing();
        sim.run(1.0);
        *sim.fail.borrow_mut() = Some(-libc::EIO);
        sim.run(2.0);
        let c = sim.pb.counters();
        assert_eq!(c.warnings, 1, "{c:?}");
        assert_eq!(c.opens, 3);
    }

    #[test]
    fn without_a_usb_stream_the_queue_stays_bounded() {
        let mut sim = Sim::new(48_000.0, 44_100.0, playing());
        sim.usb = false;
        sim.run(20.0);
        assert!(sim.level_ms() <= 20.0 + 80.0 + 30.0, "{}", sim.level_ms());
        assert_eq!(sim.pb.counters().warnings, 0);
        // The USB side comes back: the level returns to target without warnings.
        sim.usb = true;
        sim.run(120.0);
        assert!((sim.level_ms() - 20.0).abs() < 8.0, "{}", sim.level_ms());
        assert_eq!(sim.pb.counters().underruns, 0);
    }
}
