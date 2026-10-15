//! The NS7II as a device: endpoint plan, handshake, and stream factories.

use crate::descriptors::{Endpoint, EndpointKind, Topology};
use crate::reactor::{next_token, Action, Reactor, StreamId, UrbHandler};
use crate::sysfs::SysfsDevice;
use crate::usbfs::{urb_flags, urb_type, Speed, Urb, UsbfsDevice};
use log::{debug, info, warn};
use ns7ii_proto::requests::{self, ControlRequest, FirmwareInfo, RequestData};
use ns7ii_proto::{classes, PRODUCT_ID, VENDOR_ID};
use std::collections::VecDeque;
use std::io;
use std::sync::mpsc::{self, Receiver, Sender};
use std::sync::{Arc, Mutex};
use std::time::Duration;
use thiserror::Error;

/// Errors from the device layer.
#[derive(Debug, Error)]
pub enum Error {
    /// I/O or ioctl failure.
    #[error("usb: {0}")]
    Io(#[from] io::Error),
    /// Descriptor set could not be parsed.
    #[error("could not parse USB descriptors")]
    Descriptors,
    /// Device reply could not be decoded.
    #[error("decode: {0}")]
    Decode(#[from] requests::DecodeError),
    /// The topology lacks an endpoint we need.
    #[error("no suitable {0} endpoint found; pass it explicitly with --{1}")]
    MissingEndpoint(&'static str, &'static str),
}

/// Result alias.
pub type Result<T> = std::result::Result<T, Error>;

/// Endpoint overrides from the command line (addresses include the direction bit).
#[derive(Debug, Clone, Default)]
pub struct Overrides {
    /// Isochronous OUT endpoint for playback.
    pub playback: Option<u8>,
    /// Isochronous IN feedback endpoint.
    pub feedback: Option<u8>,
    /// Bulk/interrupt IN endpoint for capture.
    pub capture: Option<u8>,
    /// Bulk IN endpoint for raw MIDI.
    pub midi_in: Option<u8>,
    /// Bulk OUT endpoint for raw MIDI.
    pub midi_out: Option<u8>,
    /// Also detach the kernel's class-MIDI driver and claim that interface.
    pub take_class_midi: bool,
}

/// Which endpoints will be used for what.
#[derive(Debug, Clone, Default)]
pub struct Plan {
    /// Vendor interfaces to claim, with the alt setting that exposes the endpoints.
    pub claims: Vec<(u8, u8)>,
    /// Playback endpoint.
    pub playback: Option<Endpoint>,
    /// Feedback endpoint.
    pub feedback: Option<Endpoint>,
    /// Capture endpoint.
    pub capture: Option<Endpoint>,
    /// Raw MIDI IN endpoint.
    pub midi_in: Option<Endpoint>,
    /// Raw MIDI OUT endpoint.
    pub midi_out: Option<Endpoint>,
    /// Class MIDIStreaming interface number, if the device has one.
    pub class_midi_interface: Option<u8>,
    /// Interface each endpoint lives on, by address.
    pub ep_interface: Vec<(u8, u8)>,
}

impl Plan {
    /// Derive a plan from the topology, applying overrides. Heuristics:
    /// every vendor-class interface is claimed at its richest alt setting;
    /// the isochronous OUT endpoint is playback; a tiny isochronous IN is
    /// feedback; among bulk/interrupt IN endpoints, the one sharing an
    /// interface with the playback endpoint is MIDI (Xone layout) and the
    /// other one is capture; a bulk OUT is raw MIDI out.
    pub fn derive(topo: &Topology, ov: &Overrides) -> Plan {
        let mut plan = Plan::default();
        let Some(cfg) = topo.configurations.first() else { return plan };
        let mut candidates: Vec<(u8, Endpoint)> = Vec::new();
        for i in &cfg.interfaces {
            let vendor = i.alts.iter().any(|a| a.class == classes::VENDOR);
            let class_midi = i
                .alts
                .iter()
                .any(|a| a.class == classes::AUDIO && a.subclass == classes::MIDI_STREAMING_SUBCLASS);
            if class_midi {
                plan.class_midi_interface = Some(i.number);
            }
            if !(vendor || (class_midi && ov.take_class_midi)) {
                continue;
            }
            // Richest alt: most endpoints, highest alt number on ties.
            let Some(best) = i.alts.iter().max_by_key(|a| (a.endpoints.len(), a.alt)) else { continue };
            plan.claims.push((i.number, best.alt));
            for e in &best.endpoints {
                candidates.push((i.number, e.clone()));
                plan.ep_interface.push((e.address, i.number));
            }
        }
        let find = |addr: u8| candidates.iter().find(|(_, e)| e.address == addr).map(|(_, e)| e.clone());

        plan.playback = ov.playback.and_then(find).or_else(|| {
            candidates
                .iter()
                .find(|(_, e)| e.kind == EndpointKind::Isochronous && !e.is_in())
                .map(|(_, e)| e.clone())
        });
        plan.feedback = ov.feedback.and_then(find).or_else(|| {
            candidates
                .iter()
                .find(|(_, e)| e.kind == EndpointKind::Isochronous && e.is_in() && e.max_packet <= 64)
                .map(|(_, e)| e.clone())
        });
        let playback_if = plan
            .playback
            .as_ref()
            .and_then(|p| candidates.iter().find(|(_, e)| e.address == p.address).map(|(i, _)| *i));
        let ins: Vec<&(u8, Endpoint)> = candidates
            .iter()
            .filter(|(_, e)| e.is_in() && matches!(e.kind, EndpointKind::Bulk | EndpointKind::Interrupt))
            .collect();
        plan.midi_in = ov.midi_in.and_then(find).or_else(|| {
            ins.iter()
                .find(|(i, e)| Some(*i) == playback_if && e.kind == EndpointKind::Bulk)
                .map(|(_, e)| e.clone())
        });
        plan.capture = ov.capture.and_then(find).or_else(|| {
            ins.iter()
                .find(|(_, e)| Some(e.address) != plan.midi_in.as_ref().map(|m| m.address))
                .map(|(_, e)| e.clone())
        });
        if plan.capture.is_none() && plan.midi_in.is_some() && ins.len() == 1 && plan.class_midi_interface.is_some() {
            // Only one bulk IN and the device has class MIDI: that IN is capture.
            plan.capture = plan.midi_in.take();
        }
        plan.midi_out = ov.midi_out.and_then(find).or_else(|| {
            candidates
                .iter()
                .find(|(_, e)| e.kind == EndpointKind::Bulk && !e.is_in())
                .map(|(_, e)| e.clone())
        });
        plan
    }

    /// Human-readable summary.
    pub fn describe(&self) -> String {
        let ep = |e: &Option<Endpoint>| match e {
            Some(e) => format!("EP {:02x} ({:?}, {} bytes)", e.address, e.kind, e.max_packet),
            None => "none".to_string(),
        };
        format!(
            "claim interfaces {:?}\n  playback : {}\n  feedback : {}\n  capture  : {}\n  midi in  : {}\n  midi out : {}\n  class MIDI interface: {}",
            self.claims,
            ep(&self.playback),
            ep(&self.feedback),
            ep(&self.capture),
            ep(&self.midi_in),
            ep(&self.midi_out),
            self.class_midi_interface.map(|i| i.to_string()).unwrap_or_else(|| "none".into())
        )
    }
}

/// An open, claimed NS7II.
pub struct Ns7ii {
    reactor: Reactor,
    info: SysfsDevice,
    topo: Topology,
    plan: Plan,
    speed: Speed,
    claimed: Vec<u8>,
}

impl Ns7ii {
    /// Find the first NS7II in sysfs.
    pub fn find() -> Result<Option<SysfsDevice>> {
        Ok(crate::sysfs::find(Some(VENDOR_ID), Some(PRODUCT_ID))?.into_iter().next())
    }

    /// Parse the topology of a sysfs device without opening it.
    pub fn topology(info: &SysfsDevice) -> Result<Topology> {
        crate::descriptors::parse(&info.descriptors).ok_or(Error::Descriptors)
    }

    /// Open the device node, derive the plan, detach kernel drivers from the
    /// vendor interfaces and claim them.
    pub fn open(info: SysfsDevice, overrides: &Overrides) -> Result<Self> {
        let topo = Self::topology(&info)?;
        let plan = Plan::derive(&topo, overrides);
        let dev = UsbfsDevice::open(&info.devnode())?;
        let speed = dev.speed().unwrap_or(Speed::Unknown(0));
        info!("opened {} ({:?})", info.devnode(), speed);
        let mut claimed = Vec::new();
        for (ifnum, alt) in &plan.claims {
            if let Ok(Some(drv)) = dev.kernel_driver(*ifnum) {
                info!("interface {ifnum}: detaching kernel driver {drv}");
            }
            dev.detach_and_claim(*ifnum)?;
            claimed.push(*ifnum);
            if let Err(e) = dev.set_alt_setting(*ifnum, *alt) {
                warn!("SET_INTERFACE {ifnum} alt {alt} failed: {e}");
            } else {
                debug!("interface {ifnum} alt {alt} selected");
            }
        }
        let reactor = Reactor::start(Arc::new(dev));
        Ok(Self { reactor, info, topo, plan, speed, claimed })
    }

    /// sysfs record.
    pub fn info(&self) -> &SysfsDevice {
        &self.info
    }
    /// Parsed topology.
    pub fn topology_ref(&self) -> &Topology {
        &self.topo
    }
    /// Endpoint plan.
    pub fn plan(&self) -> &Plan {
        &self.plan
    }
    /// Connection speed.
    pub fn speed(&self) -> Speed {
        self.speed
    }
    /// Is this a high-speed (or faster) link?
    pub fn high_speed(&self) -> bool {
        matches!(self.speed, Speed::High | Speed::Super)
    }

    fn dev(&self) -> &UsbfsDevice {
        self.reactor.device()
    }

    /// Issue a control request. Returns the IN data (empty for OUT requests).
    pub fn control(&self, req: &ControlRequest) -> Result<Vec<u8>> {
        let timeout = Duration::from_millis(2000);
        match &req.data {
            RequestData::In(n) => {
                let mut buf = vec![0u8; *n as usize];
                let got = self.dev().control(req.request_type, req.request, req.value, req.index, &mut buf, timeout)?;
                buf.truncate(got);
                debug!(
                    "ctrl IN  {:02x} {:02x} v={:04x} i={:04x} -> {:02x?}",
                    req.request_type, req.request, req.value, req.index, buf
                );
                Ok(buf)
            }
            RequestData::Out(data) => {
                let mut buf = data.clone();
                self.dev().control(req.request_type, req.request, req.value, req.index, &mut buf, timeout)?;
                debug!(
                    "ctrl OUT {:02x} {:02x} v={:04x} i={:04x} data={:02x?}",
                    req.request_type, req.request, req.value, req.index, data
                );
                Ok(Vec::new())
            }
        }
    }

    /// Firmware version and reported channel topology (vendor request 0x56).
    pub fn firmware_info(&self) -> Result<FirmwareInfo> {
        let v = self.control(&requests::firmware_version())?;
        info!("firmware reply (0x56, 8 bytes): {v:02x?}");
        let mut info = requests::decode_firmware_version(&v)?;
        match self.control(&requests::firmware_topology()) {
            Ok(t) => {
                info!("topology reply (0x56, 5 bytes): {t:02x?}");
                match requests::decode_firmware_topology(&t) {
                Ok((i, o)) => {
                    info.in_channels = i;
                    info.out_channels = o;
                }
                    Err(e) => warn!("topology reply too short: {e}"),
                }
            }
            Err(e) => warn!("topology request failed (not fatal): {e}"),
        }
        Ok(info)
    }

    /// Read status register `index`.
    pub fn read_status(&self, index: u16) -> Result<u8> {
        let b = self.control(&requests::read_status(index))?;
        b.first().copied().ok_or_else(|| requests::DecodeError::TooShort { got: 0, need: 1 }.into())
    }

    /// Write status register `index`.
    pub fn write_status(&self, index: u16, value: u8) -> Result<()> {
        self.control(&requests::write_status(index, value)).map(|_| ())
    }

    /// `GET_CUR` sample rate from `endpoint`.
    pub fn get_rate(&self, endpoint: u8) -> Result<u32> {
        let b = self.control(&requests::get_sample_rate(endpoint))?;
        Ok(requests::decode_sample_rate(&b)?)
    }

    /// `SET_CUR` sample rate on `endpoint`.
    pub fn set_rate(&self, endpoint: u8, rate: u32) -> Result<()> {
        self.control(&requests::set_sample_rate(endpoint, rate)).map(|_| ())
    }

    /// The start-up sequence the official driver performs before streaming,
    /// reduced to what the NS7II code path executes. Every step logs its
    /// result and non-essential steps tolerate failure.
    pub fn handshake(&self, rate: u32) -> Result<FirmwareInfo> {
        let fw = self.firmware_info()?;
        info!(
            "firmware {} (chip {:02x}, packed {:08x}); device reports {} in / {} out channels",
            fw.version_string, fw.chip_id, fw.packed_version, fw.in_channels, fw.out_channels
        );
        match self.read_status(0) {
            Ok(s) => {
                info!("status[0] = {s:02x}");
                if s & requests::STATUS_READY == 0 {
                    info!("READY bit clear; setting it (Ozzy/Jockey3 sequence)");
                    if let Err(e) = self.write_status(0, s | requests::STATUS_READY) {
                        warn!("status write failed (not fatal): {e}");
                    }
                }
            }
            Err(e) => warn!("status read failed (not fatal on this device family): {e}"),
        }
        std::thread::sleep(Duration::from_millis(20));
        let mut rate_eps: Vec<u8> = Vec::new();
        if let Some(c) = &self.plan.capture {
            rate_eps.push(c.address);
        }
        if let Some(p) = &self.plan.playback {
            rate_eps.push(p.address);
        }
        for ep in &rate_eps {
            match self.set_rate(*ep, rate) {
                Ok(()) => info!("SET_CUR {rate} Hz -> EP {ep:02x}"),
                Err(e) => warn!("SET_CUR on EP {ep:02x} failed: {e}"),
            }
            std::thread::sleep(Duration::from_millis(50));
        }
        for ep in &rate_eps {
            match self.get_rate(*ep) {
                Ok(r) => info!("GET_CUR EP {ep:02x} = {r} Hz"),
                Err(e) => warn!("GET_CUR on EP {ep:02x} failed: {e}"),
            }
        }
        Ok(fw)
    }

    /// Start a bulk/interrupt IN stream. Completed transfers are delivered on
    /// the returned channel. `urbs` transfers of `packets * wMaxPacketSize`
    /// bytes stay in flight.
    pub fn start_bulk_in(&self, ep: &Endpoint, urbs: usize, packets: usize) -> Result<BulkInStream> {
        let (tx, rx) = mpsc::channel();
        let typ = if ep.kind == EndpointKind::Interrupt { urb_type::INTERRUPT } else { urb_type::BULK };
        let mut list = Vec::new();
        for _ in 0..urbs.max(1) {
            let mut u = Urb::new(typ, ep.address, ep.max_packet * packets.max(1), 0, next_token());
            u.set_flags(0);
            list.push(u);
        }
        let stats = Arc::new(Mutex::new(BulkStats::default()));
        let id = self.reactor.add_stream(list, Box::new(BulkInHandler { tx, stats: Arc::clone(&stats) }))?;
        Ok(BulkInStream { id, rx, stats })
    }

    /// Stop and remove a stream.
    pub fn stop(&self, id: StreamId) {
        self.reactor.remove_stream(id);
    }

    /// Clear a halted endpoint.
    pub fn clear_halt(&self, endpoint: u8) -> Result<()> {
        Ok(self.dev().clear_halt(endpoint)?)
    }

    /// Write one raw MIDI message to the MIDI OUT bulk endpoint (synchronous).
    pub fn midi_write(&self, data: &[u8]) -> Result<usize> {
        let Some(ep) = &self.plan.midi_out else {
            return Err(Error::MissingEndpoint("MIDI OUT", "midi-out"));
        };
        let mut buf = data.to_vec();
        Ok(self.dev().bulk(ep.address, &mut buf, Duration::from_millis(1000))?)
    }

    /// Start an isochronous OUT stream fed by `source`.
    pub fn start_iso_out(&self, ep: &Endpoint, cfg: IsoOutConfig, source: AudioSource) -> Result<IsoOutStream> {
        let high_speed = self.high_speed();
        let pacer = Pacer::new(cfg.rate, high_speed, cfg.frame_bytes, ep.max_packet * ep.packets_per_microframe as usize);
        let mut list = Vec::new();
        let max_packet = ep.max_packet * ep.packets_per_microframe as usize;
        for _ in 0..cfg.urbs.max(2) {
            let mut u = Urb::new(urb_type::ISO, ep.address, max_packet * cfg.packets_per_urb, cfg.packets_per_urb, next_token());
            u.set_flags(urb_flags::ISO_ASAP);
            list.push(u);
        }
        let stats = Arc::new(Mutex::new(IsoStats::default()));
        let mut handler = IsoOutHandler { pacer, source, stats: Arc::clone(&stats), frame_bytes: cfg.frame_bytes };
        for u in list.iter_mut() {
            handler.fill(u);
        }
        let id = self.reactor.add_stream(list, Box::new(handler))?;
        Ok(IsoOutStream { id, stats })
    }

    /// Start reading the isochronous feedback endpoint; values are logged and
    /// forwarded to `pacer_tx` as frames-per-(micro)frame in 16.16 fixed point.
    pub fn start_feedback(&self, ep: &Endpoint, tx: Sender<u32>) -> Result<StreamId> {
        let packets = 8;
        let mut list = Vec::new();
        for _ in 0..2 {
            let mut u = Urb::new(urb_type::ISO, ep.address, ep.max_packet * packets, packets, next_token());
            u.set_flags(urb_flags::ISO_ASAP);
            if let Some(p) = u.iso_packets_mut() {
                for d in p.iter_mut() {
                    d.length = ep.max_packet as u32;
                }
            }
            list.push(u);
        }
        let hs = self.high_speed();
        Ok(self.reactor.add_stream(list, Box::new(FeedbackHandler { tx, max_packet: ep.max_packet, high_speed: hs }))?)
    }
}

impl Drop for Ns7ii {
    fn drop(&mut self) {
        for ifnum in &self.claimed {
            let _ = self.dev().release(*ifnum);
            let _ = self.dev().reconnect_kernel_driver(*ifnum);
        }
    }
}

/// Live statistics of a bulk IN stream.
#[derive(Debug, Default, Clone, Copy)]
pub struct BulkStats {
    /// URBs completed.
    pub completions: u64,
    /// Completions with a non-zero status.
    pub errors: u64,
    /// Last non-zero status (negative errno).
    pub last_error: i32,
    /// Bytes received.
    pub bytes: u64,
}

/// A running bulk IN stream.
pub struct BulkInStream {
    /// Reactor id (pass to [`Ns7ii::stop`]).
    pub id: StreamId,
    /// Completed transfers (raw bytes).
    pub rx: Receiver<Vec<u8>>,
    /// Statistics.
    pub stats: Arc<Mutex<BulkStats>>,
}

struct BulkInHandler {
    tx: Sender<Vec<u8>>,
    stats: Arc<Mutex<BulkStats>>,
}

impl UrbHandler for BulkInHandler {
    fn complete(&mut self, urb: &mut Urb) -> Action {
        let st = urb.status();
        let n = urb.actual_length();
        {
            let mut s = self.stats.lock().unwrap();
            s.completions += 1;
            s.bytes += n as u64;
            if st != 0 {
                s.errors += 1;
                s.last_error = st;
            }
        }
        if st != 0 && st != -libc::EOVERFLOW {
            if st == -libc::ENODEV || st == -libc::ESHUTDOWN {
                return Action::Park;
            }
            debug!("bulk IN EP {:02x} status {st}", urb.endpoint());
            if st == -libc::EPIPE {
                // Halted endpoint: clear and keep going.
                let _ = self.tx.send(Vec::new());
            }
        }
        if n > 0 && self.tx.send(urb.buffer()[..n].to_vec()).is_err() {
            return Action::Park;
        }
        Action::Resubmit
    }
}

/// Configuration of an isochronous OUT stream.
#[derive(Debug, Clone, Copy)]
pub struct IsoOutConfig {
    /// Sample rate.
    pub rate: u32,
    /// Bytes per audio frame on the wire (channels * 3).
    pub frame_bytes: usize,
    /// Packets per URB (one packet per (micro)frame).
    pub packets_per_urb: usize,
    /// URBs in flight.
    pub urbs: usize,
}

/// Audio samples for playback: interleaved wire-ready bytes pulled by the
/// reactor. Producers push encoded bytes; underruns play silence.
#[derive(Clone, Default)]
pub struct AudioSource {
    inner: Arc<Mutex<VecDeque<u8>>>,
}

impl AudioSource {
    /// New empty source.
    pub fn new() -> Self {
        Self::default()
    }
    /// Queue encoded bytes.
    pub fn push(&self, bytes: &[u8]) {
        self.inner.lock().unwrap().extend(bytes.iter().copied());
    }
    /// Bytes currently queued.
    pub fn queued(&self) -> usize {
        self.inner.lock().unwrap().len()
    }
    fn pull(&self, out: &mut [u8]) -> usize {
        let mut q = self.inner.lock().unwrap();
        let n = out.len().min(q.len());
        for (i, b) in q.drain(..n).enumerate() {
            out[i] = b;
        }
        for b in &mut out[n..] {
            *b = 0;
        }
        n
    }
}

/// Live statistics of an isochronous OUT stream.
#[derive(Debug, Default, Clone, Copy)]
pub struct IsoStats {
    /// URBs completed.
    pub urbs: u64,
    /// Packets with a non-zero status.
    pub packet_errors: u64,
    /// Bytes of silence inserted because the source ran dry.
    pub underrun_bytes: u64,
    /// Current frames-per-packet estimate in 16.16.
    pub frames_per_packet_fp: u32,
}

/// A running isochronous OUT stream.
pub struct IsoOutStream {
    /// Reactor id.
    pub id: StreamId,
    /// Statistics.
    pub stats: Arc<Mutex<IsoStats>>,
}

/// Decides how many frames go into each isochronous packet, like
/// `snd-usb-audio`'s `freqn`/`freqm` accumulator, with exact rational
/// arithmetic for the nominal rate. Accepts feedback updates (frames per
/// (micro)frame in 16.16 fixed point).
pub struct Pacer {
    /// Frames per second, scaled by 65536 (so feedback values map exactly).
    nominal_scaled: u64,
    current_scaled: u64,
    /// Packets per second times 65536.
    denom: u64,
    accum: u64,
    per_sec: u64,
    frame_bytes: usize,
    max_packet: usize,
}

impl Pacer {
    /// `rate` Hz, `high_speed` selects 8000 vs 1000 packets per second.
    pub fn new(rate: u32, high_speed: bool, frame_bytes: usize, max_packet: usize) -> Self {
        let per_sec = if high_speed { 8000u64 } else { 1000u64 };
        let scaled = (rate as u64) << 16;
        Self {
            nominal_scaled: scaled,
            current_scaled: scaled,
            denom: per_sec << 16,
            accum: 0,
            per_sec,
            frame_bytes,
            max_packet,
        }
    }
    /// Apply a feedback value (16.16 frames per (micro)frame). Values more
    /// than 1 % away from nominal are ignored as garbage.
    pub fn feedback(&mut self, fp: u32) {
        let scaled = fp as u64 * self.per_sec;
        let lo = self.nominal_scaled - self.nominal_scaled / 100;
        let hi = self.nominal_scaled + self.nominal_scaled / 100;
        if (lo..=hi).contains(&scaled) {
            self.current_scaled = scaled;
        }
    }
    /// Bytes to put in the next packet.
    pub fn next_packet_bytes(&mut self) -> usize {
        self.accum += self.current_scaled;
        let frames = (self.accum / self.denom) as usize;
        self.accum %= self.denom;
        (frames * self.frame_bytes).min(self.max_packet)
    }
    /// Current estimate in 16.16 frames per packet.
    pub fn current(&self) -> u32 {
        (self.current_scaled / self.per_sec) as u32
    }
}

struct IsoOutHandler {
    pacer: Pacer,
    source: AudioSource,
    stats: Arc<Mutex<IsoStats>>,
    frame_bytes: usize,
}

impl IsoOutHandler {
    fn fill(&mut self, urb: &mut Urb) {
        let n_packets = urb.iso_packets().len();
        let mut lengths = Vec::with_capacity(n_packets);
        let mut total = 0usize;
        for _ in 0..n_packets {
            let l = self.pacer.next_packet_bytes();
            lengths.push(l);
            total += l;
        }
        let mut underrun = 0usize;
        {
            let buf = urb.buffer_mut().expect("URB not in flight");
            let mut off = 0usize;
            for l in &lengths {
                let got = self.source.pull(&mut buf[off..off + l]);
                underrun += l - got;
                off += l;
            }
        }
        if let Some(descs) = urb.iso_packets_mut() {
            for (d, l) in descs.iter_mut().zip(lengths.iter()) {
                d.length = *l as u32;
                d.actual_length = 0;
                d.status = 0;
            }
        }
        urb.set_length(total);
        let mut s = self.stats.lock().unwrap();
        s.underrun_bytes += underrun as u64;
        s.frames_per_packet_fp = self.pacer.current();
        let _ = self.frame_bytes;
    }
}

impl UrbHandler for IsoOutHandler {
    fn complete(&mut self, urb: &mut Urb) -> Action {
        let st = urb.status();
        if st == -libc::ENODEV || st == -libc::ESHUTDOWN {
            return Action::Park;
        }
        {
            let mut s = self.stats.lock().unwrap();
            s.urbs += 1;
            s.packet_errors += urb.iso_packets().iter().filter(|p| p.status != 0).count() as u64;
        }
        self.fill(urb);
        Action::Resubmit
    }
}

struct FeedbackHandler {
    tx: Sender<u32>,
    max_packet: usize,
    high_speed: bool,
}

impl UrbHandler for FeedbackHandler {
    fn complete(&mut self, urb: &mut Urb) -> Action {
        let st = urb.status();
        if st == -libc::ENODEV || st == -libc::ESHUTDOWN {
            return Action::Park;
        }
        let descs: Vec<_> = urb.iso_packets().to_vec();
        let buf = urb.buffer();
        let mut off = 0usize;
        for d in &descs {
            let n = d.actual_length as usize;
            if d.status == 0 && n >= 3 {
                let raw = &buf[off..off + n];
                // UAC: full speed 3 bytes 10.14 (frames per ms); high speed 4 bytes 16.16
                // (frames per microframe). Some firmware sends 3 bytes at high speed too.
                let fp = if n >= 4 {
                    u32::from_le_bytes([raw[0], raw[1], raw[2], raw[3]])
                } else {
                    let v = u32::from_le_bytes([raw[0], raw[1], raw[2], 0]);
                    if self.high_speed { v >> 2 } else { v << 2 }
                };
                debug!("feedback raw {:02x?} -> {}.{:04x}", raw, fp >> 16, fp & 0xFFFF);
                if self.tx.send(fp).is_err() {
                    return Action::Park;
                }
            }
            off += self.max_packet;
        }
        if let Some(p) = urb.iso_packets_mut() {
            for d in p.iter_mut() {
                d.length = self.max_packet as u32;
                d.actual_length = 0;
                d.status = 0;
            }
        }
        Action::Resubmit
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn pacer_averages_to_rate_high_speed() {
        let mut p = Pacer::new(44_100, true, 12, 1024);
        let total: usize = (0..8000).map(|_| p.next_packet_bytes()).sum();
        assert_eq!(total / 12, 44_100);
    }

    #[test]
    fn pacer_averages_to_rate_full_speed() {
        let mut p = Pacer::new(44_100, false, 12, 1023);
        let total: usize = (0..1000).map(|_| p.next_packet_bytes()).sum();
        assert_eq!(total / 12, 44_100);
    }

    #[test]
    fn pacer_rejects_wild_feedback() {
        let mut p = Pacer::new(48_000, true, 12, 1024);
        let nominal = p.current();
        p.feedback(nominal * 2);
        assert_eq!(p.current(), nominal);
        p.feedback(nominal + 100);
        assert_eq!(p.current(), nominal + 100);
    }

    #[test]
    fn audio_source_pads_with_silence() {
        let s = AudioSource::new();
        s.push(&[1, 2, 3]);
        let mut out = [9u8; 6];
        assert_eq!(s.pull(&mut out), 3);
        assert_eq!(out, [1, 2, 3, 0, 0, 0]);
    }
}
