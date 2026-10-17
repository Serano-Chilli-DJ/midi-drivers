//! `ns7iid run`: the long-running driver, supervising USB sessions.
//!
//! What apps see (the ALSA sequencer port, the loopback PCM bridges) lives
//! for the whole process. What talks to the unit (the claimed device, its
//! URB streams) lives in a *session*, and a session that breaks is replaced
//! without the apps noticing:
//!
//! * **device lost** (ENODEV: unplugged, or re-enumerated after a protocol
//!   error or a reset): wait up to [`REAPPEAR_WAIT`] for 15e4:002c to come
//!   back, then open it, handshake and restart the streams;
//! * **stream fault** (a URB that cannot be resubmitted, a stream that
//!   stopped moving, MIDI OUT stalled): reset the port (`USBDEVFS_RESET`)
//!   from here and start a fresh session.
//!
//! The process exits only when told to (exit 0), when the unit stays away
//! ([`EXIT_ABSENT`], which the systemd unit treats as success and does not
//! restart: udev starts the service again on the next plug-in), or when
//! sessions keep failing (exit 1: systemd restarts the service).

use crate::bridge;
use crate::midi_out;
use crate::Codec;
use log::{debug, info, warn};
use ns7ii_proto::midi::strip_filler;
use ns7ii_proto::pcm::{Plain24, WireCodec};
use ns7ii_proto::requests::FirmwareInfo;
use ns7ii_usb::descriptors::EndpointKind;
use ns7ii_usb::device::{BulkStats, IsoStats};
use ns7ii_usb::reactor::StreamId;
use ns7ii_usb::sysfs::SysfsDevice;
use ns7ii_usb::{AudioSource, IsoOutConfig, Ns7ii, Overrides};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::mpsc::{self, Receiver, SyncSender};
use std::sync::{Arc, Mutex, RwLock};
use std::time::{Duration, Instant};

/// Exit status when the NS7II is not on the bus (unplugged or switched off).
pub const EXIT_ABSENT: i32 = 69;
/// How long a lost unit may take to re-enumerate before the daemon gives up.
pub const REAPPEAR_WAIT: Duration = Duration::from_secs(30);
/// How long to wait for the unit at start-up.
const STARTUP_WAIT: Duration = Duration::from_secs(10);
/// Sessions in a row that may fail to start (or die within [`SHORT_SESSION`])
/// before the process exits for systemd to restart it.
const MAX_FAILED_SESSIONS: u32 = 5;
const SHORT_SESSION: Duration = Duration::from_secs(30);
/// A playback stream whose URB count does not move for this long is dead.
const ISO_STALL: Duration = Duration::from_millis(1500);
/// A capture stream that delivered before and then stops for this long is dead.
const CAPTURE_STALL: Duration = Duration::from_secs(5);

/// Settings of `ns7iid run`.
pub struct Options {
    pub overrides: Overrides,
    pub rate: u32,
    pub codec: Codec,
    pub pcm_out: Option<String>,
    pub pcm_in: Option<String>,
    pub no_midi: bool,
    pub client_name: String,
    pub midi: midi_out::Config,
}

/// The device of the current session, shared with the threads that write to it.
#[derive(Default)]
struct Slot(RwLock<Option<Arc<Ns7ii>>>);

impl Slot {
    fn get(&self) -> Option<Arc<Ns7ii>> {
        self.0.read().unwrap().clone()
    }
    fn set(&self, dev: Option<Arc<Ns7ii>>) {
        *self.0.write().unwrap() = dev;
    }
}

/// Why a session ended.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum End {
    /// SIGINT / SIGTERM.
    Stop,
    /// The unit left the bus.
    Lost(String),
    /// The unit is there but a stream broke; reset it.
    Fault(String),
}

/// Find the NS7II, polling sysfs every 250 ms for up to `timeout`.
fn wait_for_device(timeout: Duration, stop: &AtomicBool) -> Option<SysfsDevice> {
    let until = Instant::now() + timeout;
    loop {
        if let Ok(Some(d)) = Ns7ii::find() {
            return Some(d);
        }
        if Instant::now() >= until || stop.load(Ordering::SeqCst) {
            return None;
        }
        std::thread::sleep(Duration::from_millis(250));
    }
}

fn open_device(info: SysfsDevice, opts: &Options) -> Result<(Arc<Ns7ii>, FirmwareInfo), Box<dyn std::error::Error>> {
    let ns = Ns7ii::open(info, &opts.overrides)?;
    info!("endpoint plan:\n  {}", ns.plan().describe().replace('\n', "\n  "));
    let fw = ns.handshake(opts.rate)?;
    Ok((Arc::new(ns), fw))
}

/// Everything that outlives a session and feeds or drains its streams.
struct Persistent {
    source: AudioSource,
    capture_tx: SyncSender<Vec<u8>>,
    midi_tx: Option<SyncSender<Vec<u8>>>,
    feedback_tx: mpsc::Sender<u32>,
}

/// The streams of one session.
struct Session {
    ns: Arc<Ns7ii>,
    ids: Vec<StreamId>,
    iso: Arc<Mutex<IsoStats>>,
    capture: Arc<Mutex<BulkStats>>,
    started: Instant,
}

impl Session {
    fn start(ns: Arc<Ns7ii>, fw: &FirmwareInfo, opts: &Options, p: &Persistent) -> Result<Self, Box<dyn std::error::Error>> {
        let (in_ch, out_ch) = channels(fw);
        let codec = crate::make_codec(opts.codec, in_ch, out_ch);
        let mut ids = Vec::new();
        let result = (|| -> Result<(Arc<Mutex<IsoStats>>, Arc<Mutex<BulkStats>>), Box<dyn std::error::Error>> {
            // Audio first, as the vendor driver does: the unit services MIDI only while it streams.
            if let Some(fb) = &ns.plan().feedback {
                ids.push(ns.start_feedback(fb, p.feedback_tx.clone())?);
            }
            let ep = ns.plan().playback.clone().ok_or("no playback endpoint; pass --play-ep")?;
            let cfg = IsoOutConfig {
                rate: opts.rate,
                frame_bytes: codec.out_frame_bytes(),
                packets_per_urb: if ns.high_speed() { 16 } else { 4 },
                urbs: 4,
            };
            let iso = ns.start_iso_out(&ep, cfg, p.source.clone())?;
            ids.push(iso.id);
            let cap_ep = ns.plan().capture.clone().ok_or("no capture endpoint; pass --capture-ep")?;
            let packets = if cap_ep.kind == EndpointKind::Interrupt { 8 } else { 16 };
            let (cap_id, cap_stats) = ns.start_bulk_in_to(&cap_ep, 4, packets, p.capture_tx.clone())?;
            ids.push(cap_id);
            if let (Some(tx), Some(ep)) = (&p.midi_tx, ns.plan().midi_in.clone()) {
                let (id, _) = ns.start_bulk_in_to(&ep, 4, 1, tx.clone())?;
                ids.push(id);
            }
            Ok((iso.stats, cap_stats))
        })();
        match result {
            Ok((iso, capture)) => Ok(Self { ns, ids, iso, capture, started: Instant::now() }),
            Err(e) => {
                for id in ids.iter().rev() {
                    ns.stop(*id);
                }
                Err(e)
            }
        }
    }

    /// Watch the session until it ends.
    fn monitor(&self, stop: &AtomicBool, midi: &midi_out::Shared, feedback: &Receiver<u32>) -> End {
        let mut iso_seen = (0u64, Instant::now());
        let mut cap_seen = (0u64, Instant::now());
        let mut cap_flowing = false;
        loop {
            std::thread::sleep(Duration::from_millis(100));
            while let Ok(fp) = feedback.try_recv() {
                debug!("feedback {}.{:04x}", fp >> 16, fp & 0xFFFF);
            }
            if stop.load(Ordering::SeqCst) {
                return End::Stop;
            }
            if !self.ns.is_connected() {
                return End::Lost("the NS7II left the USB bus (ENODEV)".into());
            }
            let fault = self.ns.fault().or_else(|| midi.take_fault());
            if let Some(why) = fault {
                // A protocol error is often the unit dropping off the bus: give
                // the kernel a moment to tell before resetting a device that is gone.
                std::thread::sleep(Duration::from_millis(500));
                if !self.ns.is_connected() || !still_present(self.ns.info()) {
                    return End::Lost(format!("the NS7II left the USB bus after: {why}"));
                }
                return End::Fault(why);
            }
            let now = Instant::now();
            let urbs = self.iso.lock().unwrap().urbs;
            if urbs != iso_seen.0 {
                iso_seen = (urbs, now);
            } else if now - iso_seen.1 >= ISO_STALL {
                return End::Fault(format!("the playback stream stopped moving ({urbs} URBs done)"));
            }
            let cap = self.capture.lock().unwrap().completions;
            if cap != cap_seen.0 {
                cap_flowing |= cap_seen.0 != 0;
                cap_seen = (cap, now);
            } else if cap_flowing && now - cap_seen.1 >= CAPTURE_STALL {
                return End::Fault(format!("the capture stream stopped delivering ({cap} transfers done)"));
            }
        }
    }

    /// Stop every stream and close the device, once no other thread holds it.
    fn close(self) {
        for id in self.ids.iter().rev() {
            self.ns.stop(*id);
        }
        let until = Instant::now() + Duration::from_secs(2);
        while Arc::strong_count(&self.ns) > 1 && Instant::now() < until {
            std::thread::sleep(Duration::from_millis(10));
        }
        if Arc::strong_count(&self.ns) > 1 {
            warn!("closing the NS7II while another thread still holds it");
        }
    }
}

fn still_present(info: &SysfsDevice) -> bool {
    matches!(Ns7ii::find(), Ok(Some(d)) if d.devnode() == info.devnode())
}

fn channels(fw: &FirmwareInfo) -> (usize, usize) {
    let in_ch = if fw.in_channels > 0 { fw.in_channels as usize } else { 12 };
    let out_ch = if fw.out_channels > 0 { fw.out_channels as usize } else { 4 };
    (in_ch, out_ch)
}

/// What the supervisor does after a session ended.
#[derive(Debug, PartialEq, Eq)]
pub enum Next {
    /// Exit with this status.
    Exit(i32),
    /// Reset the port, then reopen.
    ResetAndReopen,
    /// Wait for the unit to re-enumerate, then reopen.
    WaitAndReopen,
}

/// Recovery policy, kept separate from the I/O for tests.
#[derive(Debug, Default)]
pub struct Policy {
    failed: u32,
}

impl Policy {
    /// Decide after a session that ran for `lived` and ended with `end`.
    pub fn after_session(&mut self, end: &End, lived: Duration) -> Next {
        match end {
            End::Stop => return Next::Exit(0),
            _ if lived >= SHORT_SESSION => self.failed = 0,
            _ => self.failed += 1,
        }
        if self.failed >= MAX_FAILED_SESSIONS {
            return Next::Exit(1);
        }
        match end {
            End::Lost(_) => Next::WaitAndReopen,
            _ => Next::ResetAndReopen,
        }
    }

    /// A session could not be started. Returns whether to keep trying.
    pub fn start_failed(&mut self) -> bool {
        self.failed += 1;
        self.failed < MAX_FAILED_SESSIONS
    }

    /// Delay before the next open attempt.
    pub fn backoff(&self) -> Duration {
        Duration::from_millis(500 * (1 << self.failed.min(4)))
    }
}

/// Run the daemon until stopped; returns the process exit status.
pub fn run(opts: Options, stop: Arc<AtomicBool>) -> Result<i32, Box<dyn std::error::Error>> {
    let Some(info) = wait_for_device(STARTUP_WAIT, &stop) else {
        info!("no NS7II (15e4:002c) on the bus; nothing to do (the service starts again when it is plugged in)");
        return Ok(EXIT_ABSENT);
    };
    let (ns, fw) = open_device(info, &opts)?;
    let alsa = ns7ii_alsa::Alsa::load()?;
    let (in_ch, out_ch) = channels(&fw);
    let slot = Arc::new(Slot::default());
    let midi = midi_out::Shared::new();
    let mut threads = Vec::new();

    let (capture_tx, capture_rx) = mpsc::sync_channel(ns7ii_usb::device::BULK_IN_QUEUE);
    let (feedback_tx, feedback_rx) = mpsc::channel();
    let mut midi_tx = None;

    // MIDI: one sequencer port for the whole process, so apps stay connected
    // across USB reconnects.
    if !opts.no_midi {
        if ns.plan().midi_in.is_some() {
            let mut seq = alsa.open_seq(&opts.client_name)?;
            println!("ALSA MIDI ports: from device {}  to device {}", seq.source_address(), seq.sink_address());
            let (tx, rx) = mpsc::sync_channel::<Vec<u8>>(ns7ii_usb::device::BULK_IN_QUEUE);
            midi_tx = Some(tx);
            let (stop2, out) = (Arc::clone(&stop), Arc::clone(&midi));
            threads.push(std::thread::spawn(move || {
                let mut clean = Vec::new();
                let mut outgoing = Vec::new();
                while !stop2.load(Ordering::SeqCst) {
                    match rx.recv_timeout(Duration::from_millis(2)) {
                        Ok(raw) => {
                            clean.clear();
                            strip_filler(&raw, &mut clean);
                            if !clean.is_empty() {
                                if let Err(e) = seq.send(&clean) {
                                    warn!("seq send: {e}");
                                }
                            }
                        }
                        Err(mpsc::RecvTimeoutError::Timeout) => {}
                        Err(mpsc::RecvTimeoutError::Disconnected) => break,
                    }
                    outgoing.clear();
                    if let Ok(n) = seq.receive(&mut outgoing) {
                        if n > 0 {
                            out.push(&outgoing);
                        }
                    }
                }
            }));
            if ns.plan().midi_out.is_some() {
                let (out, stop2, slot2, cfg) = (Arc::clone(&midi), Arc::clone(&stop), Arc::clone(&slot), opts.midi);
                threads.push(std::thread::spawn(move || midi_out::run(cfg, out, stop2, move || slot2.get())));
            }
        } else if ns.plan().class_midi_interface.is_some() {
            println!("MIDI: the device has a class-compliant MIDI interface; the kernel's snd-usb-audio provides it (see `aseqdump -l`).");
        } else {
            println!("MIDI: no raw MIDI endpoint found; pass --midi-in-ep/--midi-out-ep after checking `probe`.");
        }
    }

    let source = AudioSource::new();
    threads.push(spawn_playback(&opts, &alsa, out_ch, source.clone(), Arc::clone(&stop)));
    threads.push(spawn_capture(&opts, &alsa, in_ch, out_ch, capture_rx, Arc::clone(&stop)));
    let persistent = Persistent { source, capture_tx, midi_tx, feedback_tx };

    println!("running; Ctrl-C to stop");
    let mut policy = Policy::default();
    let mut current = Some((ns, fw));
    let status = 'outer: loop {
        let (ns, fw) = match current.take() {
            Some(c) => c,
            None => unreachable!(),
        };
        let end = match Session::start(Arc::clone(&ns), &fw, &opts, &persistent) {
            Ok(session) => {
                info!("NS7II streaming ({} in / {} out channels)", in_ch, out_ch);
                drop(ns);
                let _ = midi.take_fault();
                slot.set(Some(Arc::clone(&session.ns)));
                let end = session.monitor(&stop, &midi, &feedback_rx);
                slot.set(None);
                let lived = session.started.elapsed();
                let info = session.ns.info().clone();
                session.close();
                match &end {
                    End::Stop => {}
                    End::Lost(why) => warn!("{why}; waiting up to {} s for it to come back", REAPPEAR_WAIT.as_secs()),
                    End::Fault(why) => warn!("{why}; resetting the NS7II's USB port and starting over"),
                }
                match policy.after_session(&end, lived) {
                    Next::Exit(0) => break 'outer 0,
                    Next::Exit(code) => {
                        warn!("the NS7II keeps failing ({end:?}); exiting so systemd restarts the service");
                        break 'outer code;
                    }
                    Next::ResetAndReopen => {
                        match Ns7ii::reset_port(&info) {
                            Ok(()) => info!("USB port reset done"),
                            Err(e) => warn!("USB port reset failed: {e}"),
                        }
                        std::thread::sleep(Duration::from_secs(1));
                    }
                    Next::WaitAndReopen => {}
                }
                end
            }
            Err(e) => {
                warn!("could not start the NS7II streams: {e}");
                drop(ns);
                if !policy.start_failed() {
                    break 'outer 1;
                }
                End::Fault(e.to_string())
            }
        };
        // Reopen, retrying with backoff while the unit is there.
        loop {
            if stop.load(Ordering::SeqCst) {
                break 'outer 0;
            }
            let wait = if matches!(end, End::Lost(_)) { REAPPEAR_WAIT } else { STARTUP_WAIT };
            let Some(info) = wait_for_device(wait, &stop) else {
                if stop.load(Ordering::SeqCst) {
                    break 'outer 0;
                }
                info!("the NS7II did not come back within {} s; exiting (it starts again when plugged in)", wait.as_secs());
                break 'outer EXIT_ABSENT;
            };
            std::thread::sleep(policy.backoff());
            match open_device(info, &opts) {
                Ok(c) => {
                    info!("NS7II reopened and handshaken");
                    current = Some(c);
                    break;
                }
                Err(e) => {
                    warn!("reopening the NS7II failed: {e}");
                    if !policy.start_failed() {
                        break 'outer 1;
                    }
                }
            }
        }
    };
    stop.store(true, Ordering::SeqCst);
    for t in threads {
        let _ = t.join();
    }
    Ok(status)
}

fn spawn_playback(opts: &Options, alsa: &ns7ii_alsa::Alsa, out_ch: usize, source: AudioSource, stop: Arc<AtomicBool>) -> std::thread::JoinHandle<()> {
    let out_codec = crate::make_codec(opts.codec, 0, out_ch);
    let frame_bytes = out_codec.out_frame_bytes();
    let plain = Plain24 { out_channels: out_ch, in_channels: out_ch };
    let rate = opts.rate;
    let mut pcm = opts.pcm_out.as_ref().map(|name| {
        info!("playback bridge: reading {name} as {out_ch} ch S24_3LE {rate} Hz; a loopback's other end must be opened with the same format (or through plughw:)");
        let (alsa, name2) = (alsa.clone(), name.clone());
        bridge::Bridge::start("playback bridge", name, move || alsa.open_pcm(&name2, true, out_ch as u32, rate, 50_000), Instant::now())
    });
    std::thread::spawn(move || {
        let period = 256usize;
        let mut in_bytes = vec![0u8; period * out_ch * 3];
        let mut samples = vec![0i32; period * out_ch];
        let mut wire = vec![0u8; period * frame_bytes];
        // Keep roughly 20 ms queued ahead of the USB side.
        let target = (rate as usize / 50) * frame_bytes;
        while !stop.load(Ordering::SeqCst) {
            let Some(pcm) = pcm.as_mut() else {
                std::thread::sleep(Duration::from_millis(100));
                continue;
            };
            if source.queued() > target {
                std::thread::sleep(Duration::from_millis(2));
                continue;
            }
            match pcm.read(&mut in_bytes, Instant::now()) {
                Some(n) => {
                    let got = plain.decode(&in_bytes[..n], &mut samples);
                    let m = out_codec.encode(&samples[..got], &mut wire);
                    source.push(&wire[..m]);
                }
                // Failing or waiting to reopen (logged, rate-limited);
                // the device plays silence meanwhile.
                None => std::thread::sleep(Duration::from_millis(20)),
            }
        }
    })
}

fn spawn_capture(
    opts: &Options,
    alsa: &ns7ii_alsa::Alsa,
    in_ch: usize,
    out_ch: usize,
    rx: Receiver<Vec<u8>>,
    stop: Arc<AtomicBool>,
) -> std::thread::JoinHandle<()> {
    let in_codec = crate::make_codec(opts.codec, in_ch, out_ch);
    let is_junction = matches!(opts.codec, Codec::Junction);
    let rate = opts.rate;
    let mut pcm = opts.pcm_in.as_ref().map(|name| {
        info!("capture bridge: writing {name} as {in_ch} ch S24_3LE {rate} Hz; a loopback's other end must be opened with the same format (or through plughw:)");
        let (alsa, name2) = (alsa.clone(), name.clone());
        bridge::Bridge::start("capture bridge", name, move || alsa.open_pcm(&name2, false, in_ch as u32, rate, 50_000), Instant::now())
    });
    std::thread::spawn(move || {
        let frame_bytes = in_codec.in_frame_bytes();
        let mut pending = Vec::new();
        let mut samples = vec![0i32; 8192 * in_ch];
        let plain = Plain24 { out_channels: in_ch, in_channels: in_ch };
        let mut out_bytes = Vec::new();
        while !stop.load(Ordering::SeqCst) {
            match rx.recv_timeout(Duration::from_millis(20)) {
                Ok(raw) => {
                    let Some(pcm) = pcm.as_mut() else { continue };
                    pending.extend_from_slice(&raw);
                    let usable = pending.len() - pending.len() % frame_bytes;
                    let frames = usable / frame_bytes;
                    if samples.len() < frames * in_ch {
                        samples.resize(frames * in_ch, 0);
                    }
                    let n = in_codec.decode(&pending[..usable], &mut samples[..frames * in_ch]);
                    let n_samples = if is_junction { n * in_ch } else { n };
                    out_bytes.resize(n_samples * 3, 0);
                    plain.encode(&samples[..n_samples], &mut out_bytes);
                    // On error the block is dropped; the bridge logs and reopens.
                    let _ = pcm.write(&out_bytes, Instant::now());
                    pending.drain(..usable);
                }
                Err(mpsc::RecvTimeoutError::Timeout) => {}
                Err(mpsc::RecvTimeoutError::Disconnected) => break,
            }
        }
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_lost_unit_is_waited_for_and_a_faulty_one_reset() {
        let mut p = Policy::default();
        assert_eq!(p.after_session(&End::Lost("x".into()), Duration::from_secs(600)), Next::WaitAndReopen);
        assert_eq!(p.after_session(&End::Fault("x".into()), Duration::from_secs(600)), Next::ResetAndReopen);
        assert_eq!(p.after_session(&End::Stop, Duration::from_secs(1)), Next::Exit(0));
    }

    #[test]
    fn sessions_that_keep_dying_quickly_end_the_process_for_systemd() {
        let mut p = Policy::default();
        for _ in 0..MAX_FAILED_SESSIONS - 1 {
            assert_eq!(p.after_session(&End::Fault("x".into()), Duration::from_secs(2)), Next::ResetAndReopen);
        }
        assert_eq!(p.after_session(&End::Fault("x".into()), Duration::from_secs(2)), Next::Exit(1));
    }

    #[test]
    fn a_long_healthy_session_clears_the_failure_count() {
        let mut p = Policy::default();
        for _ in 0..MAX_FAILED_SESSIONS - 1 {
            p.after_session(&End::Lost("x".into()), Duration::from_secs(1));
        }
        assert!(p.backoff() > Duration::from_millis(500));
        assert_eq!(p.after_session(&End::Lost("x".into()), SHORT_SESSION), Next::WaitAndReopen);
        assert_eq!(p.backoff(), Duration::from_millis(500));
        assert!(p.start_failed());
    }
}
