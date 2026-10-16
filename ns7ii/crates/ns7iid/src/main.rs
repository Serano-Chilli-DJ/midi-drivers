//! `ns7iid`: Numark NS7II user-space driver and diagnostics.
//!
//! Staged by design. Each subcommand proves one layer on real hardware and
//! prints what the next stage needs:
//!
//! 1. `list` / `probe`: find the device, dump descriptors and the endpoint plan.
//! 2. `handshake`: firmware version, channel counts, status, sample rate.
//! 3. `midi-dump` / `midi-send`: raw MIDI over the Ploytec bulk pipes.
//! 4. `record`: capture audio to a WAV file; `play`: play a WAV or test tone.
//! 5. `run`: the daemon — ALSA sequencer MIDI ports plus PCM bridging.

mod chart;
mod learn;
mod mixxx;

use clap::{Parser, Subcommand, ValueEnum};
use log::{info, warn};
use ns7ii_proto::midi::{strip_filler, RunningStatusExpander};
use ns7ii_proto::pcm::{self, Junction, Plain24, WireCodec};
use ns7ii_usb::descriptors::EndpointKind;
use ns7ii_usb::{AudioSource, IsoOutConfig, Ns7ii, Overrides};
use std::fmt::Write as _;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{mpsc, Arc};
use std::time::{Duration, Instant};

#[derive(Parser)]
#[command(name = "ns7iid", version, about = "Numark NS7II Linux driver (user space, Rust)")]
struct Cli {
    /// Isochronous OUT endpoint for playback (hex, e.g. 0x05).
    #[arg(long, value_parser = parse_hex8, global = true)]
    play_ep: Option<u8>,
    /// Isochronous IN feedback endpoint (hex, e.g. 0x81).
    #[arg(long, value_parser = parse_hex8, global = true)]
    feedback_ep: Option<u8>,
    /// Bulk/interrupt IN endpoint for capture (hex, e.g. 0x86).
    #[arg(long, value_parser = parse_hex8, global = true)]
    capture_ep: Option<u8>,
    /// Bulk IN endpoint for raw MIDI (hex, e.g. 0x83).
    #[arg(long, value_parser = parse_hex8, global = true)]
    midi_in_ep: Option<u8>,
    /// Bulk OUT endpoint for raw MIDI (hex, e.g. 0x04).
    #[arg(long, value_parser = parse_hex8, global = true)]
    midi_out_ep: Option<u8>,
    /// Also detach the kernel's class-MIDI driver and claim that interface.
    #[arg(long, global = true)]
    take_class_midi: bool,
    /// Sample rate.
    #[arg(long, default_value_t = 44_100, global = true)]
    rate: u32,
    /// Wire codec for audio.
    #[arg(long, value_enum, default_value_t = Codec::Plain24, global = true)]
    codec: Codec,
    #[command(subcommand)]
    cmd: Cmd,
}

#[derive(Clone, Copy, ValueEnum, Debug)]
enum Codec {
    /// Interleaved 24-bit little-endian (what the kext selects for Numark).
    Plain24,
    /// Ploytec bit-plane frames (Xone / Reloop layout), in case plain24 is wrong.
    Junction,
}

#[derive(Subcommand)]
enum Cmd {
    /// List NS7II devices found in sysfs.
    List,
    /// Dump descriptors, kernel drivers and the derived endpoint plan (no claim).
    Probe,
    /// Claim the vendor interfaces and run the start-up handshake.
    Handshake,
    /// Print raw MIDI received from the device (Ctrl-C to stop). Also runs the
    /// audio streams (silence out, capture discarded), because the firmware
    /// only forwards MIDI while streaming; pass --no-audio to test without.
    MidiDump {
        /// Do not start the audio streams.
        #[arg(long)]
        no_audio: bool,
    },
    /// Send raw MIDI bytes (hex) to the device, e.g. `90 3c 7f`.
    MidiSend { bytes: Vec<String> },
    /// Guided capture of every control for building a Mixxx mapping.
    MidiLearn {
        /// Capture file (resumed if it exists).
        #[arg(long, default_value = "ns7ii-learn.json")]
        out: String,
    },
    /// Generate a Mixxx mapping (XML + JS) from the built-in NS7II chart.
    MixxxMap {
        /// Capture file from `midi-learn` whose entries override the built-in
        /// chart (for a unit that differs, or to add a control).
        #[arg(long)]
        capture: Option<String>,
        /// Use the capture alone and ignore the built-in chart.
        #[arg(long, requires = "capture")]
        capture_only: bool,
        /// Directory to write `Numark NS7II.midi.xml` and `Numark-NS7II-scripts.js` into.
        /// Default: this user's Mixxx controller folder (`~/.mixxx/controllers`, or the
        /// Flatpak / Snap equivalent, whichever exists).
        #[arg(long)]
        out_dir: Option<String>,
    },
    /// Record capture audio to a WAV file.
    Record {
        /// Output file.
        out: String,
        /// Seconds to record.
        #[arg(long, default_value_t = 10.0)]
        seconds: f32,
        /// Capture channels (defaults to what the device reports, else 12).
        #[arg(long)]
        channels: Option<usize>,
    },
    /// Play a WAV file (or a test tone when no file is given).
    Play {
        /// Input WAV (16/24/32-bit PCM or float).
        file: Option<String>,
        /// Seconds of test tone when no file is given.
        #[arg(long, default_value_t = 5.0)]
        seconds: f32,
        /// Playback channels (defaults to what the device reports, else 4).
        #[arg(long)]
        channels: Option<usize>,
        /// Test tone frequency.
        #[arg(long, default_value_t = 440.0)]
        tone_hz: f32,
    },
    /// Run as a daemon: ALSA sequencer MIDI ports and PCM bridging.
    Run {
        /// ALSA PCM to read playback audio from (what apps play into), e.g. hw:Loopback,1,0.
        #[arg(long)]
        pcm_out: Option<String>,
        /// ALSA PCM to write captured audio to (what apps record from), e.g. hw:Loopback,1,1.
        #[arg(long)]
        pcm_in: Option<String>,
        /// Disable MIDI bridging.
        #[arg(long)]
        no_midi: bool,
        /// ALSA client name.
        #[arg(long, default_value = "Numark NS7II")]
        client_name: String,
    },
}

fn parse_hex8(s: &str) -> Result<u8, String> {
    let t = s.trim_start_matches("0x").trim_start_matches("0X");
    u8::from_str_radix(t, 16).map_err(|e| e.to_string())
}

fn main() {
    env_logger::Builder::from_env(env_logger::Env::default().default_filter_or("info")).init();
    let cli = Cli::parse();
    if let Err(e) = run(cli) {
        eprintln!("error: {e}");
        std::process::exit(1);
    }
}

fn overrides(cli: &Cli) -> Overrides {
    Overrides {
        playback: cli.play_ep,
        feedback: cli.feedback_ep,
        capture: cli.capture_ep,
        midi_in: cli.midi_in_ep,
        midi_out: cli.midi_out_ep,
        take_class_midi: cli.take_class_midi,
    }
}

fn ctrl_c_flag() -> Arc<AtomicBool> {
    let flag = Arc::new(AtomicBool::new(false));
    let f = Arc::clone(&flag);
    // Minimal SIGINT handling without extra crates: a thread polling stdin is
    // not an option for a daemon, so install a C handler that sets the flag.
    static FLAG: std::sync::OnceLock<Arc<AtomicBool>> = std::sync::OnceLock::new();
    let _ = FLAG.set(f);
    extern "C" fn on_sigint(_: i32) {
        if let Some(f) = FLAG.get() {
            if f.swap(true, Ordering::SeqCst) {
                // Second Ctrl-C: give up waiting for streams to drain.
                // SAFETY: _exit is async-signal-safe.
                unsafe { libc::_exit(130) };
            }
        }
    }
    // SAFETY: installing an async-signal-safe handler that only stores an atomic.
    unsafe {
        libc::signal(libc::SIGINT, on_sigint as *const () as usize);
        libc::signal(libc::SIGTERM, on_sigint as *const () as usize);
    }
    flag
}

/// Mixxx's user controller folder: `<settings>/controllers`, where the
/// settings folder is `~/.mixxx` for a native install, or the Flatpak / Snap
/// sandbox equivalent. Only an existing settings folder counts, so the files
/// never land where Mixxx would not look.
fn mixxx_controllers_dir() -> Option<String> {
    let home = std::env::var("HOME").ok()?;
    [".mixxx", ".var/app/org.mixxx.Mixxx/.mixxx", "snap/mixxx/current/.mixxx"]
        .iter()
        .map(|d| format!("{home}/{d}"))
        .find(|d| std::path::Path::new(d).is_dir())
        .map(|d| format!("{d}/controllers"))
}

/// Folders where a native, Flatpak or Snap Mixxx keeps its bundled
/// controller scripts, plus the one next to a `mixxx` binary on PATH.
fn mixxx_resource_dirs() -> Vec<std::path::PathBuf> {
    let home = std::env::var("HOME").unwrap_or_default();
    let mut dirs: Vec<std::path::PathBuf> = [
        "/usr/share/mixxx/controllers".to_string(),
        "/usr/local/share/mixxx/controllers".to_string(),
        "/opt/mixxx/share/mixxx/controllers".to_string(),
        "/var/lib/flatpak/app/org.mixxx.Mixxx/current/active/files/share/mixxx/controllers".to_string(),
        format!("{home}/.local/share/flatpak/app/org.mixxx.Mixxx/current/active/files/share/mixxx/controllers"),
        "/snap/mixxx/current/usr/share/mixxx/controllers".to_string(),
    ]
    .into_iter()
    .map(std::path::PathBuf::from)
    .collect();
    if let Some(paths) = std::env::var_os("PATH") {
        for dir in std::env::split_paths(&paths) {
            if let Ok(real) = std::fs::canonicalize(dir.join("mixxx")) {
                if let Some(prefix) = real.parent().and_then(|p| p.parent()) {
                    dirs.push(prefix.join("share/mixxx/controllers"));
                }
            }
        }
    }
    dirs.into_iter().filter(|d| d.is_dir()).collect()
}

/// Warnings for Mixxx libraries the mapping lists that no Mixxx install on
/// this machine provides. Silent when no Mixxx controllers folder exists at
/// all, since the mapping may be generated on another machine.
fn missing_mixxx_libs() -> Vec<String> {
    let dirs = mixxx_resource_dirs();
    if dirs.is_empty() {
        return Vec::new();
    }
    let shown: Vec<String> = dirs.iter().map(|d| d.display().to_string()).collect();
    mixxx::MIXXX_LIBS
        .iter()
        .filter(|lib| !dirs.iter().any(|d| d.join(lib).is_file()))
        .map(|lib| format!("warning: the mapping loads {lib} from Mixxx but it is missing from {}; Mixxx ships it in res/controllers, copy it next to the mapping", shown.join(", ")))
        .collect()
}

fn open(cli: &Cli) -> Result<Ns7ii, Box<dyn std::error::Error>> {
    let dev = Ns7ii::find()?.ok_or("no NS7II (15e4:002c) found; is it powered on and plugged in?")?;
    let ns = Ns7ii::open(dev, &overrides(cli))?;
    info!("endpoint plan:\n  {}", ns.plan().describe().replace('\n', "\n  "));
    Ok(ns)
}

fn make_codec(codec: Codec, in_ch: usize, out_ch: usize) -> Box<dyn WireCodec + Send> {
    match codec {
        Codec::Plain24 => Box::new(Plain24 { out_channels: out_ch, in_channels: in_ch }),
        Codec::Junction => Box::new(Junction { out_channels: out_ch, in_channels: in_ch }),
    }
}

fn run(cli: Cli) -> Result<(), Box<dyn std::error::Error>> {
    match &cli.cmd {
        Cmd::List => {
            let devs = ns7ii_usb::sysfs::find(Some(ns7ii_proto::VENDOR_ID), Some(ns7ii_proto::PRODUCT_ID))?;
            if devs.is_empty() {
                println!("no NS7II found");
            }
            for d in devs {
                println!(
                    "{}  bus {} addr {}  {} {}  speed {} Mbit/s  bcdDevice {:04x}",
                    d.devnode(),
                    d.bus,
                    d.address,
                    d.manufacturer.as_deref().unwrap_or("?"),
                    d.product.as_deref().unwrap_or("?"),
                    d.speed_mbps.unwrap_or(0),
                    d.bcd_device.unwrap_or(0)
                );
            }
            Ok(())
        }
        Cmd::Probe => {
            let dev = Ns7ii::find()?.ok_or("no NS7II (15e4:002c) found")?;
            println!("{}  speed {} Mbit/s", dev.devnode(), dev.speed_mbps.unwrap_or(0));
            let topo = Ns7ii::topology(&dev)?;
            print!("{}", topo.dump());
            println!("kernel drivers:");
            for (ifnum, drv) in ns7ii_usb::sysfs::interface_drivers(&dev) {
                println!("  interface {ifnum}: {}", drv.unwrap_or_else(|| "(none)".into()));
            }
            let plan = ns7ii_usb::Plan::derive(&topo, &overrides(&cli));
            println!("derived plan:\n  {}", plan.describe().replace('\n', "\n  "));
            println!("raw descriptors ({} bytes): {}", dev.descriptors.len(), hex(&dev.descriptors));
            println!("\nPlease paste this whole output into the project issue tracker.");
            Ok(())
        }
        Cmd::Handshake => {
            let ns = open(&cli)?;
            let fw = ns.handshake(cli.rate)?;
            println!("firmware {}  in {} ch  out {} ch", fw.version_string, fw.in_channels, fw.out_channels);
            for idx in 0..3u16 {
                match ns.read_status(idx) {
                    Ok(v) => println!("status[{idx}] = {v:02x}"),
                    Err(e) => println!("status[{idx}] read failed: {e}"),
                }
            }
            Ok(())
        }
        Cmd::MidiDump { no_audio } => {
            let ns = open(&cli)?;
            let fw = ns.handshake(cli.rate)?;
            let ep = ns.plan().midi_in.clone().ok_or(
                "no raw MIDI IN endpoint in the plan. If `probe` shows a class-MIDI interface, the kernel already provides MIDI (try `aseqdump -l`).",
            )?;
            // Audio streams first, as the official driver does (AJ::streamingStarted
            // starts the MIDI reads after the PCM pipes run).
            let mut audio = None;
            if !no_audio {
                let out_ch = if fw.out_channels > 0 { fw.out_channels as usize } else { 4 };
                let in_ch = if fw.in_channels > 0 { fw.in_channels as usize } else { 12 };
                let codec = make_codec(cli.codec, in_ch, out_ch);
                let play_ep = ns.plan().playback.clone().ok_or("no playback endpoint")?;
                let cap_ep = ns.plan().capture.clone().ok_or("no capture endpoint")?;
                let (fb_tx, fb_rx) = mpsc::channel();
                let fb_id = match &ns.plan().feedback {
                    Some(fb) => Some(ns.start_feedback(fb, fb_tx)?),
                    None => None,
                };
                let cfg = IsoOutConfig { rate: cli.rate, frame_bytes: codec.out_frame_bytes(), packets_per_urb: if ns.high_speed() { 16 } else { 4 }, urbs: 4 };
                let iso = ns.start_iso_out(&play_ep, cfg, AudioSource::new())?;
                let cap = ns.start_bulk_in(&cap_ep, 4, 16)?;
                println!("audio streams running: silence -> EP {:02x}, capture from EP {:02x} (discarded)", play_ep.address, cap_ep.address);
                audio = Some((iso, cap, fb_rx, fb_id));
            }
            let stream = ns.start_bulk_in(&ep, 4, 1)?;
            let stop = ctrl_c_flag();
            println!("listening on EP {:02x}; move a control on the NS7II (Ctrl-C to stop)", ep.address);
            let mut clean = Vec::new();
            let mut last = Instant::now();
            let mut cap_bytes = 0u64;
            let mut fb_seen = 0u64;
            let mut fb_last = 0u32;
            let mut filler_only = 0u64;
            while !stop.load(Ordering::SeqCst) {
                match stream.rx.recv_timeout(Duration::from_millis(50)) {
                    Ok(raw) if raw.is_empty() => {
                        warn!("EP {:02x} halted; clearing", ep.address);
                        let _ = ns.clear_halt(ep.address);
                    }
                    Ok(raw) => {
                        clean.clear();
                        strip_filler(&raw, &mut clean);
                        if clean.is_empty() {
                            filler_only += 1;
                            if filler_only <= 3 {
                                println!("raw {:<24} (filler only)", hex(&raw));
                            }
                        } else {
                            println!("raw {:<24} midi {}", hex(&raw), hex(&clean));
                        }
                    }
                    Err(mpsc::RecvTimeoutError::Timeout) => {}
                    Err(mpsc::RecvTimeoutError::Disconnected) => break,
                }
                if let Some((iso, cap, fb_rx, _)) = &audio {
                    while let Ok(b) = cap.rx.try_recv() {
                        cap_bytes += b.len() as u64;
                    }
                    while let Ok(fp) = fb_rx.try_recv() {
                        fb_seen += 1;
                        fb_last = fp;
                    }
                    if last.elapsed() >= Duration::from_secs(2) {
                        let s = *iso.stats.lock().unwrap();
                        let cs = *cap.stats.lock().unwrap();
                        let ms = *stream.stats.lock().unwrap();
                        println!(
                            "stats: play urbs {} pkt-err {} | capture {} B ({} B/s) compl {} err {} last {} | midi compl {} err {} last {} filler-only {} | feedback pkts {} last {}.{:04x}",
                            s.urbs, s.packet_errors,
                            cap_bytes, cap_bytes / last.elapsed().as_secs().max(1), cs.completions, cs.errors, cs.last_error,
                            ms.completions, ms.errors, ms.last_error, filler_only,
                            fb_seen, fb_last >> 16, fb_last & 0xFFFF
                        );
                        cap_bytes = 0;
                        last = Instant::now();
                    }
                }
            }
            ns.stop(stream.id);
            if let Some((iso, cap, _, fb_id)) = audio {
                ns.stop(iso.id);
                ns.stop(cap.id);
                if let Some(id) = fb_id {
                    ns.stop(id);
                }
            }
            Ok(())
        }
        Cmd::MidiLearn { out } => {
            let ns = open(&cli)?;
            let fw = ns.handshake(cli.rate)?;
            let ep = ns.plan().midi_in.clone().ok_or("no raw MIDI IN endpoint in the plan")?;
            let audio = AudioBackground::start(&ns, &fw, cli.codec, cli.rate)?;
            let stream = ns.start_bulk_in(&ep, 4, 1)?;
            let mut learned = if std::path::Path::new(out).exists() {
                println!("resuming {out}");
                learn::Learned::load(out)?
            } else {
                learn::Learned::default()
            };
            learned.device = format!("{} {}", ns.info().manufacturer.as_deref().unwrap_or("Numark"), ns.info().product.as_deref().unwrap_or("NS7II"));
            learned.firmware = fw.version_string.clone();
            let result = learn::run(&stream.rx, out, learned);
            ns.stop(stream.id);
            audio.stop(&ns);
            result?;
            println!("next: ns7iid mixxx-map --capture {out}      (built-in chart, overridden by this capture)");
            Ok(())
        }
        Cmd::MixxxMap { capture, capture_only, out_dir } => {
            let mut learned = chart::builtin();
            if let Some(path) = capture {
                let cap = learn::Learned::load(path)?;
                if *capture_only {
                    learned = cap;
                } else {
                    let mut n = 0;
                    for (sec, controls) in cap.sections {
                        let target = learned.sections.entry(sec).or_default();
                        for (id, c) in controls {
                            target.insert(id, c);
                            n += 1;
                        }
                    }
                    println!("built-in chart with {n} controls overridden from {path}");
                }
            }
            let (xml, js, notes) = mixxx::generate(&learned)?;
            let out_dir = match out_dir {
                Some(d) => d.clone(),
                None => mixxx_controllers_dir().ok_or(
                    "no Mixxx settings folder found (~/.mixxx, Flatpak or Snap); start Mixxx once or pass --out-dir",
                )?,
            };
            std::fs::create_dir_all(&out_dir)?;
            let xml_path = format!("{out_dir}/Numark NS7II.midi.xml");
            let js_path = format!("{out_dir}/Numark-NS7II-scripts.js");
            std::fs::write(&xml_path, xml)?;
            std::fs::write(&js_path, js)?;
            for n in &notes {
                println!("{n}");
            }
            println!("wrote {xml_path}\nwrote {js_path}");
            for w in missing_mixxx_libs() {
                eprintln!("{w}");
            }
            println!("Now: start the driver (systemctl start ns7iid), start Mixxx, open Preferences > Controllers,");
            println!("click \"Numark NS7II MIDI 1\" in the left list, choose \"Numark NS7II\" under Load Mapping, enable, Apply.");
            println!("Mixxx reads this folder only at start-up, so restart it if it was running.");
            println!("The NS7II is a four-deck controller: the outer mixer strips (printed 3 and 4) and the DECK 3 / DECK 4");
            println!("layers drive Mixxx decks 3 and 4, which most skins hide until you switch them to four decks.");
            println!("Press DECK 1 and DECK 2 on the unit, or show four decks in Mixxx, or nothing seems to respond.");
            Ok(())
        }
        Cmd::MidiSend { bytes } => {
            let ns = open(&cli)?;
            let _ = ns.handshake(cli.rate);
            let msg: Vec<u8> = bytes.iter().map(|b| parse_hex8(b)).collect::<Result<_, _>>()?;
            let expanded = RunningStatusExpander::new().expand(&msg);
            let n = ns.midi_write(&expanded)?;
            println!("sent {n} bytes: {}", hex(&expanded));
            Ok(())
        }
        Cmd::Record { out, seconds, channels } => {
            let ns = open(&cli)?;
            let fw = ns.handshake(cli.rate)?;
            let in_ch = channels.unwrap_or(if fw.in_channels > 0 { fw.in_channels as usize } else { 12 });
            let ep = ns.plan().capture.clone().ok_or("no capture endpoint in the plan; pass --capture-ep")?;
            let codec = make_codec(cli.codec, in_ch, 0);
            let frame_bytes = codec.in_frame_bytes();
            let packets = if ep.kind == EndpointKind::Interrupt { 8 } else { 16 };
            let stream = ns.start_bulk_in(&ep, 4, packets)?;
            let spec = hound::WavSpec { channels: in_ch as u16, sample_rate: cli.rate, bits_per_sample: 24, sample_format: hound::SampleFormat::Int };
            let mut wav = hound::WavWriter::create(out, spec)?;
            let stop = ctrl_c_flag();
            let start = Instant::now();
            let mut pending: Vec<u8> = Vec::new();
            let mut samples = vec![0i32; 4096 * in_ch];
            let mut total_bytes = 0usize;
            let mut frames_written = 0u64;
            println!("recording {in_ch} channels from EP {:02x} for {seconds} s ...", ep.address);
            while !stop.load(Ordering::SeqCst) && start.elapsed().as_secs_f32() < *seconds {
                match stream.rx.recv_timeout(Duration::from_millis(200)) {
                    Ok(raw) => {
                        total_bytes += raw.len();
                        pending.extend_from_slice(&raw);
                        let usable = pending.len() - pending.len() % frame_bytes;
                        let frames = usable / frame_bytes;
                        if samples.len() < frames * in_ch {
                            samples.resize(frames * in_ch, 0);
                        }
                        let n = codec.decode(&pending[..usable], &mut samples[..frames * in_ch]);
                        let n_samples = if matches!(cli.codec, Codec::Junction) { n * in_ch } else { n };
                        for s in &samples[..n_samples] {
                            wav.write_sample(*s >> 8)?;
                        }
                        frames_written += (n_samples / in_ch) as u64;
                        pending.drain(..usable);
                    }
                    Err(mpsc::RecvTimeoutError::Timeout) => {}
                    Err(mpsc::RecvTimeoutError::Disconnected) => break,
                }
            }
            ns.stop(stream.id);
            wav.finalize()?;
            let secs = start.elapsed().as_secs_f32();
            println!(
                "done: {total_bytes} bytes in {secs:.1} s = {:.0} B/s; {frames_written} frames = {:.0} frames/s (expect {})",
                total_bytes as f32 / secs,
                frames_written as f32 / secs,
                cli.rate
            );
            Ok(())
        }
        Cmd::Play { file, seconds, channels, tone_hz } => {
            let ns = open(&cli)?;
            let fw = ns.handshake(cli.rate)?;
            let out_ch = channels.unwrap_or(if fw.out_channels > 0 { fw.out_channels as usize } else { 4 });
            let ep = ns.plan().playback.clone().ok_or("no playback endpoint in the plan; pass --play-ep")?;
            let codec = make_codec(cli.codec, 0, out_ch);
            let frame_bytes = codec.out_frame_bytes();
            let source = AudioSource::new();
            // Pre-encode the whole signal (test tone or file) into the source.
            let frames: Vec<i32> = match file {
                Some(path) => read_wav(path, out_ch, cli.rate)?,
                None => {
                    let n = (cli.rate as f32 * seconds) as usize;
                    (0..n)
                        .flat_map(|i| {
                            let t = i as f32 / cli.rate as f32;
                            let v = (t * tone_hz * std::f32::consts::TAU).sin() * 0.3;
                            std::iter::repeat_n(pcm::from_f32(v), out_ch)
                        })
                        .collect()
                }
            };
            let mut wire = vec![0u8; frames.len() / out_ch * frame_bytes];
            let n = codec.encode(&frames, &mut wire);
            source.push(&wire[..n]);
            let total_frames = frames.len() / out_ch;
            let cfg = IsoOutConfig { rate: cli.rate, frame_bytes, packets_per_urb: if ns.high_speed() { 16 } else { 4 }, urbs: 4 };
            let (fb_tx, fb_rx) = mpsc::channel();
            let fb_id = match &ns.plan().feedback {
                Some(fb) => Some(ns.start_feedback(fb, fb_tx)?),
                None => None,
            };
            let stream = ns.start_iso_out(&ep, cfg, source.clone())?;
            println!("playing {total_frames} frames ({out_ch} ch) to EP {:02x} ...", ep.address);
            let stop = ctrl_c_flag();
            let mut last = Instant::now();
            while !stop.load(Ordering::SeqCst) && source.queued() > 0 {
                std::thread::sleep(Duration::from_millis(50));
                while let Ok(fp) = fb_rx.try_recv() {
                    info!("feedback {}.{:04x} frames/packet", fp >> 16, fp & 0xFFFF);
                }
                if last.elapsed() > Duration::from_secs(1) {
                    let s = *stream.stats.lock().unwrap();
                    println!(
                        "urbs {} pkt errors {} underrun {} B  queued {} B",
                        s.urbs, s.packet_errors, s.underrun_bytes, source.queued()
                    );
                    last = Instant::now();
                }
            }
            std::thread::sleep(Duration::from_millis(200));
            ns.stop(stream.id);
            if let Some(id) = fb_id {
                ns.stop(id);
            }
            Ok(())
        }
        Cmd::Run { pcm_out, pcm_in, no_midi, client_name } => run_daemon(&cli, pcm_out.clone(), pcm_in.clone(), *no_midi, client_name),
    }
}

fn read_wav(path: &str, out_ch: usize, rate: u32) -> Result<Vec<i32>, Box<dyn std::error::Error>> {
    let mut reader = hound::WavReader::open(path)?;
    let spec = reader.spec();
    if spec.sample_rate != rate {
        warn!("WAV is {} Hz, device runs at {rate} Hz; no resampling, pitch will be off", spec.sample_rate);
    }
    let file_ch = spec.channels as usize;
    let samples: Vec<i32> = match spec.sample_format {
        hound::SampleFormat::Float => reader.samples::<f32>().map(|s| s.map(pcm::from_f32)).collect::<Result<_, _>>()?,
        hound::SampleFormat::Int => {
            let shift = 32 - spec.bits_per_sample as i32;
            reader.samples::<i32>().map(|s| s.map(|v| v << shift)).collect::<Result<_, _>>()?
        }
    };
    // Map file channels onto device channels: repeat stereo pairs across all outputs.
    let mut out = Vec::with_capacity(samples.len() / file_ch * out_ch);
    for frame in samples.chunks_exact(file_ch) {
        for c in 0..out_ch {
            out.push(frame[c % file_ch]);
        }
    }
    Ok(out)
}

fn run_daemon(
    cli: &Cli,
    pcm_out: Option<String>,
    pcm_in: Option<String>,
    no_midi: bool,
    client_name: &str,
) -> Result<(), Box<dyn std::error::Error>> {
    let ns = Arc::new(open(cli)?);
    let fw = ns.handshake(cli.rate)?;
    let stop = ctrl_c_flag();
    let alsa = ns7ii_alsa::Alsa::load()?;
    let mut threads = Vec::new();

    // MIDI: device -> sequencer and sequencer -> device.
    if !no_midi {
        if let Some(ep) = ns.plan().midi_in.clone() {
            let mut seq = alsa.open_seq(client_name)?;
            println!("ALSA MIDI ports: from device {}  to device {}", seq.source_address(), seq.sink_address());
            let stream = ns.start_bulk_in(&ep, 4, 1)?;
            let ns2 = Arc::clone(&ns);
            let stop2 = Arc::clone(&stop);
            threads.push(std::thread::spawn(move || {
                let mut clean = Vec::new();
                let mut outgoing = Vec::new();
                let mut expander = RunningStatusExpander::new();
                while !stop2.load(Ordering::SeqCst) {
                    match stream.rx.recv_timeout(Duration::from_millis(5)) {
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
                        if n > 0 && ns2.plan().midi_out.is_some() {
                            let expanded = expander.expand(&outgoing);
                            if let Err(e) = ns2.midi_write(&expanded) {
                                warn!("midi write: {e}");
                            }
                        }
                    }
                }
                ns2.stop(stream.id);
            }));
        } else if ns.plan().class_midi_interface.is_some() {
            println!("MIDI: the device has a class-compliant MIDI interface; the kernel's snd-usb-audio provides it (see `aseqdump -l`).");
        } else {
            println!("MIDI: no raw MIDI endpoint found; pass --midi-in-ep/--midi-out-ep after checking `probe`.");
        }
    }

    // Audio streams always run: the firmware forwards MIDI only while
    // streaming. Without --pcm-in the capture data is discarded; without
    // --pcm-out silence is played.
    let in_ch = if fw.in_channels > 0 { fw.in_channels as usize } else { 12 };
    let out_ch = if fw.out_channels > 0 { fw.out_channels as usize } else { 4 };
    let in_codec = make_codec(cli.codec, in_ch, out_ch);
    let out_codec = make_codec(cli.codec, in_ch, out_ch);
    let is_junction = matches!(cli.codec, Codec::Junction);

    // Playback: ALSA PCM (apps play into the other side of a loopback) -> device.
    {
        let ep = ns.plan().playback.clone().ok_or("no playback endpoint; pass --play-ep")?;
        let frame_bytes = out_codec.out_frame_bytes();
        let source = AudioSource::new();
        let cfg = IsoOutConfig { rate: cli.rate, frame_bytes, packets_per_urb: if ns.high_speed() { 16 } else { 4 }, urbs: 4 };
        let (fb_tx, fb_rx) = mpsc::channel();
        let fb_id = match &ns.plan().feedback {
            Some(fb) => Some(ns.start_feedback(fb, fb_tx)?),
            None => None,
        };
        let stream = ns.start_iso_out(&ep, cfg, source.clone())?;
        let ns2 = Arc::clone(&ns);
        let stop2 = Arc::clone(&stop);
        let plain = Plain24 { out_channels: out_ch, in_channels: out_ch };
        let mut pcm = match &pcm_out {
            Some(name) => Some(alsa.open_pcm(name, true, out_ch as u32, cli.rate, 50_000)?),
            None => None,
        };
        let rate = cli.rate;
        threads.push(std::thread::spawn(move || {
            let period = 256usize;
            let mut in_bytes = vec![0u8; period * out_ch * 3];
            let mut samples = vec![0i32; period * out_ch];
            let mut wire = vec![0u8; period * frame_bytes];
            // Keep roughly 20 ms queued ahead of the USB side.
            let target = (rate as usize / 50) * frame_bytes;
            while !stop2.load(Ordering::SeqCst) {
                while let Ok(fp) = fb_rx.try_recv() {
                    log::debug!("feedback {}.{:04x}", fp >> 16, fp & 0xFFFF);
                }
                let Some(pcm) = pcm.as_mut() else {
                    std::thread::sleep(Duration::from_millis(100));
                    continue;
                };
                if source.queued() > target {
                    std::thread::sleep(Duration::from_millis(2));
                    continue;
                }
                match pcm.read(&mut in_bytes) {
                    Ok(n) => {
                        let got = plain.decode(&in_bytes[..n], &mut samples);
                        let m = out_codec.encode(&samples[..got], &mut wire);
                        source.push(&wire[..m]);
                    }
                    Err(e) => {
                        warn!("pcm read: {e}");
                        std::thread::sleep(Duration::from_millis(10));
                    }
                }
            }
            ns2.stop(stream.id);
            if let Some(id) = fb_id {
                ns2.stop(id);
            }
        }));
    }

    // Capture: device -> ALSA PCM (apps record from the other side of a loopback).
    {
        let ep = ns.plan().capture.clone().ok_or("no capture endpoint; pass --capture-ep")?;
        let mut pcm = match &pcm_in {
            Some(name) => Some(alsa.open_pcm(name, false, in_ch as u32, cli.rate, 50_000)?),
            None => None,
        };
        let frame_bytes = in_codec.in_frame_bytes();
        let packets = if ep.kind == EndpointKind::Interrupt { 8 } else { 16 };
        let stream = ns.start_bulk_in(&ep, 4, packets)?;
        let ns2 = Arc::clone(&ns);
        let stop2 = Arc::clone(&stop);
        threads.push(std::thread::spawn(move || {
            let mut pending = Vec::new();
            let mut samples = vec![0i32; 8192 * in_ch];
            let plain = Plain24 { out_channels: in_ch, in_channels: in_ch };
            let mut out_bytes = Vec::new();
            while !stop2.load(Ordering::SeqCst) {
                match stream.rx.recv_timeout(Duration::from_millis(20)) {
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
                        if let Err(e) = pcm.write(&out_bytes) {
                            warn!("pcm write: {e}");
                        }
                        pending.drain(..usable);
                    }
                    Err(mpsc::RecvTimeoutError::Timeout) => {}
                    Err(mpsc::RecvTimeoutError::Disconnected) => break,
                }
            }
            ns2.stop(stream.id);
        }));
    }

    println!("running; Ctrl-C to stop");
    while !stop.load(Ordering::SeqCst) {
        if !ns.is_connected() {
            info!("NS7II unplugged; exiting");
            stop.store(true, Ordering::SeqCst);
            break;
        }
        std::thread::sleep(Duration::from_millis(100));
    }
    for t in threads {
        let _ = t.join();
    }
    Ok(())
}

fn hex(b: &[u8]) -> String {
    let mut s = String::with_capacity(b.len() * 3);
    for x in b {
        let _ = write!(s, "{x:02x} ");
    }
    s.trim_end().to_string()
}

/// The audio streams the firmware needs before it forwards MIDI: silence to
/// the playback endpoint, capture drained and discarded, feedback drained.
struct AudioBackground {
    iso: ns7ii_usb::device::IsoOutStream,
    cap_id: ns7ii_usb::reactor::StreamId,
    fb_id: Option<ns7ii_usb::reactor::StreamId>,
    drain: Option<std::thread::JoinHandle<()>>,
    stop: Arc<AtomicBool>,
}

impl AudioBackground {
    fn start(ns: &Ns7ii, fw: &ns7ii_proto::requests::FirmwareInfo, codec: Codec, rate: u32) -> Result<Self, Box<dyn std::error::Error>> {
        let out_ch = if fw.out_channels > 0 { fw.out_channels as usize } else { 4 };
        let in_ch = if fw.in_channels > 0 { fw.in_channels as usize } else { 12 };
        let codec = make_codec(codec, in_ch, out_ch);
        let play_ep = ns.plan().playback.clone().ok_or("no playback endpoint")?;
        let cap_ep = ns.plan().capture.clone().ok_or("no capture endpoint")?;
        let (fb_tx, fb_rx) = mpsc::channel();
        let fb_id = match &ns.plan().feedback {
            Some(fb) => Some(ns.start_feedback(fb, fb_tx)?),
            None => None,
        };
        let cfg = IsoOutConfig { rate, frame_bytes: codec.out_frame_bytes(), packets_per_urb: if ns.high_speed() { 16 } else { 4 }, urbs: 4 };
        let iso = ns.start_iso_out(&play_ep, cfg, AudioSource::new())?;
        let cap = ns.start_bulk_in(&cap_ep, 4, 16)?;
        let cap_id = cap.id;
        let stop = Arc::new(AtomicBool::new(false));
        let stop2 = Arc::clone(&stop);
        let drain = std::thread::spawn(move || {
            while !stop2.load(Ordering::SeqCst) {
                let _ = cap.rx.recv_timeout(Duration::from_millis(50));
                while fb_rx.try_recv().is_ok() {}
            }
        });
        Ok(Self { iso, cap_id, fb_id, drain: Some(drain), stop })
    }

    fn stop(mut self, ns: &Ns7ii) {
        self.stop.store(true, Ordering::SeqCst);
        if let Some(d) = self.drain.take() {
            let _ = d.join();
        }
        ns.stop(self.iso.id);
        ns.stop(self.cap_id);
        if let Some(id) = self.fb_id {
            ns.stop(id);
        }
    }
}
