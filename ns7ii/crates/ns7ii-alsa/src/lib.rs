//! Minimal ALSA access without ALSA headers or the `alsa` crate.
//!
//! `libasound.so.2` is loaded at run time with `libloading`, and only the
//! dozen entry points this project needs are bound. That keeps the build
//! pure Rust (it compiles on a machine with no `libasound2-dev`) and keeps
//! the unsafe surface to the FFI calls in this file, each wrapped in a safe
//! method. If ALSA is missing at run time, [`Alsa::load`] returns an error
//! and the daemon falls back to file-based tools.
//!
//! Provided:
//! * [`SeqPort`]: an ALSA sequencer client with one input and one output
//!   port, speaking raw MIDI bytes (Mixxx and every DAW connect to it).
//! * [`Pcm`]: blocking `S24_3LE` playback and capture on any ALSA PCM
//!   (`hw:Loopback,0,0` for apps, or the gadget's UAC2 card on the bridge box).

#![warn(missing_docs)]

use libloading::{Library, Symbol};
use std::ffi::{c_char, c_int, c_long, c_uint, c_void, CString};
use std::ptr;
use thiserror::Error;

/// ALSA errors.
#[derive(Debug, Error)]
pub enum Error {
    /// `libasound.so.2` could not be loaded or lacks a symbol.
    #[error("libasound: {0}")]
    Load(String),
    /// An ALSA call returned a negative errno.
    #[error("alsa {call}: {msg} ({code})")]
    Call {
        /// Function name.
        call: &'static str,
        /// Error text from `snd_strerror`.
        msg: String,
        /// Negative errno.
        code: c_int,
    },
}

/// Result alias.
pub type Result<T> = std::result::Result<T, Error>;

#[repr(C)]
struct SndSeq {
    _p: [u8; 0],
}
#[repr(C)]
struct SndMidiEvent {
    _p: [u8; 0],
}
#[repr(C)]
struct SndPcm {
    _p: [u8; 0],
}

/// `snd_seq_event_t` is 28 bytes; we only read/write it through the
/// `snd_midi_event_*` helpers, so an opaque, suitably aligned buffer is enough.
#[repr(C, align(8))]
#[derive(Clone, Copy)]
struct SeqEvent([u8; 32]);

const SND_SEQ_OPEN_DUPLEX: c_int = 3;
const SND_SEQ_NONBLOCK: c_int = 1;
const SND_SEQ_PORT_CAP_READ: c_uint = 1 << 0;
const SND_SEQ_PORT_CAP_WRITE: c_uint = 1 << 1;
const SND_SEQ_PORT_CAP_SUBS_READ: c_uint = 1 << 5;
const SND_SEQ_PORT_CAP_SUBS_WRITE: c_uint = 1 << 6;
const SND_SEQ_PORT_TYPE_MIDI_GENERIC: c_uint = 1 << 1;
const SND_SEQ_PORT_TYPE_HARDWARE: c_uint = 1 << 16;
const SND_SEQ_PORT_TYPE_PORT: c_uint = 1 << 19;
const SND_SEQ_ADDRESS_SUBSCRIBERS: u8 = 254;
const SND_SEQ_QUEUE_DIRECT: u8 = 253;

const SND_PCM_STREAM_PLAYBACK: c_int = 0;
const SND_PCM_STREAM_CAPTURE: c_int = 1;
const SND_PCM_FORMAT_S24_3LE: c_int = 32;
const SND_PCM_ACCESS_RW_INTERLEAVED: c_int = 3;

struct Api {
    _lib: Library,
    strerror: unsafe extern "C" fn(c_int) -> *const c_char,
    seq_open: unsafe extern "C" fn(*mut *mut SndSeq, *const c_char, c_int, c_int) -> c_int,
    seq_close: unsafe extern "C" fn(*mut SndSeq) -> c_int,
    seq_set_client_name: unsafe extern "C" fn(*mut SndSeq, *const c_char) -> c_int,
    seq_create_simple_port: unsafe extern "C" fn(*mut SndSeq, *const c_char, c_uint, c_uint) -> c_int,
    seq_event_input: unsafe extern "C" fn(*mut SndSeq, *mut *mut SeqEvent) -> c_int,
    seq_event_output_direct: unsafe extern "C" fn(*mut SndSeq, *mut SeqEvent) -> c_int,
    seq_client_id: unsafe extern "C" fn(*mut SndSeq) -> c_int,
    midi_event_new: unsafe extern "C" fn(usize, *mut *mut SndMidiEvent) -> c_int,
    midi_event_free: unsafe extern "C" fn(*mut SndMidiEvent),
    midi_event_no_status: unsafe extern "C" fn(*mut SndMidiEvent, c_int),
    midi_event_encode: unsafe extern "C" fn(*mut SndMidiEvent, *const u8, c_long, *mut SeqEvent) -> c_long,
    midi_event_decode: unsafe extern "C" fn(*mut SndMidiEvent, *mut u8, c_long, *const SeqEvent) -> c_long,
    pcm_open: unsafe extern "C" fn(*mut *mut SndPcm, *const c_char, c_int, c_int) -> c_int,
    pcm_close: unsafe extern "C" fn(*mut SndPcm) -> c_int,
    pcm_set_params: unsafe extern "C" fn(*mut SndPcm, c_int, c_int, c_uint, c_uint, c_int, c_uint) -> c_int,
    pcm_writei: unsafe extern "C" fn(*mut SndPcm, *const c_void, c_long) -> c_long,
    pcm_readi: unsafe extern "C" fn(*mut SndPcm, *mut c_void, c_long) -> c_long,
    pcm_prepare: unsafe extern "C" fn(*mut SndPcm) -> c_int,
    pcm_recover: unsafe extern "C" fn(*mut SndPcm, c_int, c_int) -> c_int,
}

/// A loaded `libasound`.
pub struct Alsa {
    api: std::sync::Arc<Api>,
}

macro_rules! sym {
    ($lib:expr, $name:literal) => {{
        // SAFETY: the symbol's C signature is the one declared in the `Api`
        // field it is assigned to; both come from alsa/asoundlib.h.
        let s: Symbol<'_, _> = unsafe { $lib.get($name) }.map_err(|e| Error::Load(format!("{}: {e}", String::from_utf8_lossy($name))))?;
        *s
    }};
}

impl Alsa {
    /// Load `libasound.so.2`.
    pub fn load() -> Result<Self> {
        // SAFETY: loading libasound runs its constructors, which only set up
        // internal state; this is how every ALSA application starts.
        let lib = unsafe { Library::new("libasound.so.2") }.map_err(|e| Error::Load(e.to_string()))?;
        let api = Api {
            strerror: sym!(lib, b"snd_strerror\0"),
            seq_open: sym!(lib, b"snd_seq_open\0"),
            seq_close: sym!(lib, b"snd_seq_close\0"),
            seq_set_client_name: sym!(lib, b"snd_seq_set_client_name\0"),
            seq_create_simple_port: sym!(lib, b"snd_seq_create_simple_port\0"),
            seq_event_input: sym!(lib, b"snd_seq_event_input\0"),
            seq_event_output_direct: sym!(lib, b"snd_seq_event_output_direct\0"),
            seq_client_id: sym!(lib, b"snd_seq_client_id\0"),
            midi_event_new: sym!(lib, b"snd_midi_event_new\0"),
            midi_event_free: sym!(lib, b"snd_midi_event_free\0"),
            midi_event_no_status: sym!(lib, b"snd_midi_event_no_status\0"),
            midi_event_encode: sym!(lib, b"snd_midi_event_encode\0"),
            midi_event_decode: sym!(lib, b"snd_midi_event_decode\0"),
            pcm_open: sym!(lib, b"snd_pcm_open\0"),
            pcm_close: sym!(lib, b"snd_pcm_close\0"),
            pcm_set_params: sym!(lib, b"snd_pcm_set_params\0"),
            pcm_writei: sym!(lib, b"snd_pcm_writei\0"),
            pcm_readi: sym!(lib, b"snd_pcm_readi\0"),
            pcm_prepare: sym!(lib, b"snd_pcm_prepare\0"),
            pcm_recover: sym!(lib, b"snd_pcm_recover\0"),
            _lib: lib,
        };
        Ok(Self { api: std::sync::Arc::new(api) })
    }

    fn check(&self, call: &'static str, code: c_int) -> Result<c_int> {
        if code < 0 {
            // SAFETY: snd_strerror returns a static string for any code.
            let msg = unsafe { std::ffi::CStr::from_ptr((self.api.strerror)(code)) }.to_string_lossy().into_owned();
            Err(Error::Call { call, msg, code })
        } else {
            Ok(code)
        }
    }

    /// Open a sequencer client named `name` with an input and an output port.
    pub fn open_seq(&self, name: &str) -> Result<SeqPort> {
        let mut seq: *mut SndSeq = ptr::null_mut();
        let dflt = CString::new("default").unwrap();
        // SAFETY: `seq` receives a handle; `dflt` outlives the call.
        self.check("snd_seq_open", unsafe { (self.api.seq_open)(&mut seq, dflt.as_ptr(), SND_SEQ_OPEN_DUPLEX, SND_SEQ_NONBLOCK) })?;
        let cname = CString::new(name).unwrap();
        // SAFETY: `seq` is a valid handle from snd_seq_open.
        self.check("snd_seq_set_client_name", unsafe { (self.api.seq_set_client_name)(seq, cname.as_ptr()) })?;
        let in_name = CString::new(format!("{name} MIDI in")).unwrap();
        let out_name = CString::new(format!("{name} MIDI out")).unwrap();
        let caps_out = SND_SEQ_PORT_CAP_READ | SND_SEQ_PORT_CAP_SUBS_READ;
        let caps_in = SND_SEQ_PORT_CAP_WRITE | SND_SEQ_PORT_CAP_SUBS_WRITE;
        let ptype = SND_SEQ_PORT_TYPE_MIDI_GENERIC | SND_SEQ_PORT_TYPE_HARDWARE | SND_SEQ_PORT_TYPE_PORT;
        // SAFETY: valid handle and NUL-terminated names.
        let out_port = self.check("snd_seq_create_simple_port", unsafe { (self.api.seq_create_simple_port)(seq, in_name.as_ptr(), caps_out, ptype) })?;
        // SAFETY: as above.
        let in_port = self.check("snd_seq_create_simple_port", unsafe { (self.api.seq_create_simple_port)(seq, out_name.as_ptr(), caps_in, ptype) })?;
        let mut enc: *mut SndMidiEvent = ptr::null_mut();
        let mut dec: *mut SndMidiEvent = ptr::null_mut();
        // SAFETY: out-pointers receive freshly allocated parsers.
        self.check("snd_midi_event_new", unsafe { (self.api.midi_event_new)(1024, &mut enc) })?;
        // SAFETY: as above.
        self.check("snd_midi_event_new", unsafe { (self.api.midi_event_new)(1024, &mut dec) })?;
        // SAFETY: `dec` is valid; disabling running status makes the decoder
        // emit full messages, which the Ploytec firmware requires.
        unsafe { (self.api.midi_event_no_status)(dec, 1) };
        // SAFETY: valid handle.
        let client = unsafe { (self.api.seq_client_id)(seq) };
        Ok(SeqPort { alsa: self.api.clone(), seq, enc, dec, out_port: out_port as u8, in_port: in_port as u8, client })
    }

    /// Open a PCM for playback or capture, `S24_3LE` interleaved.
    pub fn open_pcm(&self, name: &str, capture: bool, channels: u32, rate: u32, latency_us: u32) -> Result<Pcm> {
        let mut pcm: *mut SndPcm = ptr::null_mut();
        let cname = CString::new(name).unwrap();
        let stream = if capture { SND_PCM_STREAM_CAPTURE } else { SND_PCM_STREAM_PLAYBACK };
        // SAFETY: `pcm` receives a handle; `cname` outlives the call.
        self.check("snd_pcm_open", unsafe { (self.api.pcm_open)(&mut pcm, cname.as_ptr(), stream, 0) })?;
        // SAFETY: valid handle.
        let r = unsafe {
            (self.api.pcm_set_params)(pcm, SND_PCM_FORMAT_S24_3LE, SND_PCM_ACCESS_RW_INTERLEAVED, channels, rate, 1, latency_us)
        };
        if let Err(e) = self.check("snd_pcm_set_params", r) {
            // SAFETY: valid handle, closed exactly once.
            unsafe { (self.api.pcm_close)(pcm) };
            return Err(e);
        }
        Ok(Pcm { alsa: self.api.clone(), pcm, channels: channels as usize, capture })
    }
}

/// ALSA sequencer client with one input and one output port.
pub struct SeqPort {
    alsa: std::sync::Arc<Api>,
    seq: *mut SndSeq,
    enc: *mut SndMidiEvent,
    dec: *mut SndMidiEvent,
    out_port: u8,
    in_port: u8,
    client: c_int,
}

// SAFETY: the handles are only used from one thread at a time through &mut self
// (ALSA sequencer handles are not thread-safe, but moving them is fine).
unsafe impl Send for SeqPort {}

impl SeqPort {
    /// `client:port` of the port apps read from (device MIDI in).
    pub fn source_address(&self) -> String {
        format!("{}:{}", self.client, self.out_port)
    }
    /// `client:port` of the port apps write to (device MIDI out).
    pub fn sink_address(&self) -> String {
        format!("{}:{}", self.client, self.in_port)
    }

    /// Publish raw MIDI bytes from the device to subscribers.
    pub fn send(&mut self, bytes: &[u8]) -> Result<()> {
        let mut off = 0usize;
        while off < bytes.len() {
            let mut ev = SeqEvent([0; 32]);
            // SAFETY: `enc` is a valid parser; the slice is in bounds; `ev` is large enough.
            let used = unsafe { (self.alsa.midi_event_encode)(self.enc, bytes[off..].as_ptr(), (bytes.len() - off) as c_long, &mut ev) };
            if used <= 0 {
                break;
            }
            off += used as usize;
            // Byte 0 of snd_seq_event_t is `type`; 0 means "nothing complete yet".
            if ev.0[0] == 0 {
                continue;
            }
            // Set source port, destination = subscribers, direct queue.
            // snd_seq_event_t layout: type(u8) flags(u8) tag(u8) queue(u8) time(8 bytes)
            // source{client,port} dest{client,port} ... (offsets 12..16).
            ev.0[3] = SND_SEQ_QUEUE_DIRECT;
            ev.0[12] = self.client as u8;
            ev.0[13] = self.out_port;
            ev.0[14] = SND_SEQ_ADDRESS_SUBSCRIBERS;
            ev.0[15] = 0;
            // SAFETY: valid handle and event.
            let r = unsafe { (self.alsa.seq_event_output_direct)(self.seq, &mut ev) };
            if r < 0 && r != -(libc::EAGAIN) {
                return Err(Error::Call { call: "snd_seq_event_output_direct", msg: String::new(), code: r });
            }
        }
        Ok(())
    }

    /// Drain pending events from subscribers into raw MIDI bytes (non-blocking).
    pub fn receive(&mut self, out: &mut Vec<u8>) -> Result<usize> {
        let mut total = 0usize;
        loop {
            let mut ev: *mut SeqEvent = ptr::null_mut();
            // SAFETY: valid handle; `ev` points into ALSA's buffer until the next call.
            let r = unsafe { (self.alsa.seq_event_input)(self.seq, &mut ev) };
            if r == -(libc::EAGAIN) || ev.is_null() {
                break;
            }
            if r < 0 {
                return Err(Error::Call { call: "snd_seq_event_input", msg: String::new(), code: r });
            }
            let mut buf = [0u8; 1024];
            // SAFETY: `ev` is valid for this call; `buf` is large enough for any single event.
            let n = unsafe { (self.alsa.midi_event_decode)(self.dec, buf.as_mut_ptr(), buf.len() as c_long, ev) };
            if n > 0 {
                out.extend_from_slice(&buf[..n as usize]);
                total += n as usize;
            }
        }
        Ok(total)
    }
}

impl Drop for SeqPort {
    fn drop(&mut self) {
        // SAFETY: each handle is valid and freed exactly once.
        unsafe {
            (self.alsa.midi_event_free)(self.enc);
            (self.alsa.midi_event_free)(self.dec);
            (self.alsa.seq_close)(self.seq);
        }
    }
}

/// A blocking interleaved `S24_3LE` PCM.
pub struct Pcm {
    alsa: std::sync::Arc<Api>,
    pcm: *mut SndPcm,
    channels: usize,
    capture: bool,
}

// SAFETY: a PCM handle may be moved between threads; it is used from one at a time.
unsafe impl Send for Pcm {}

impl Pcm {
    /// Channels per frame.
    pub fn channels(&self) -> usize {
        self.channels
    }

    /// Write interleaved `S24_3LE` bytes (whole frames). Recovers from xruns.
    pub fn write(&mut self, bytes: &[u8]) -> Result<usize> {
        assert!(!self.capture);
        let frame = self.channels * 3;
        let frames = (bytes.len() / frame) as c_long;
        loop {
            // SAFETY: valid handle; `bytes` holds `frames` whole frames.
            let r = unsafe { (self.alsa.pcm_writei)(self.pcm, bytes.as_ptr() as *const c_void, frames) };
            if r >= 0 {
                return Ok(r as usize * frame);
            }
            // SAFETY: valid handle.
            let rec = unsafe { (self.alsa.pcm_recover)(self.pcm, r as c_int, 1) };
            if rec < 0 {
                return Err(Error::Call { call: "snd_pcm_writei", msg: String::new(), code: r as c_int });
            }
        }
    }

    /// Read interleaved `S24_3LE` bytes (whole frames). Recovers from xruns.
    pub fn read(&mut self, bytes: &mut [u8]) -> Result<usize> {
        assert!(self.capture);
        let frame = self.channels * 3;
        let frames = (bytes.len() / frame) as c_long;
        loop {
            // SAFETY: valid handle; `bytes` has room for `frames` whole frames.
            let r = unsafe { (self.alsa.pcm_readi)(self.pcm, bytes.as_mut_ptr() as *mut c_void, frames) };
            if r >= 0 {
                return Ok(r as usize * frame);
            }
            // SAFETY: valid handle.
            let rec = unsafe { (self.alsa.pcm_recover)(self.pcm, r as c_int, 1) };
            if rec < 0 {
                return Err(Error::Call { call: "snd_pcm_readi", msg: String::new(), code: r as c_int });
            }
        }
    }

    /// Prepare the stream (after an error).
    pub fn prepare(&mut self) -> Result<()> {
        // SAFETY: valid handle.
        let r = unsafe { (self.alsa.pcm_prepare)(self.pcm) };
        if r < 0 {
            Err(Error::Call { call: "snd_pcm_prepare", msg: String::new(), code: r })
        } else {
            Ok(())
        }
    }
}

impl Drop for Pcm {
    fn drop(&mut self) {
        // SAFETY: valid handle, closed once.
        unsafe { (self.alsa.pcm_close)(self.pcm) };
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn loads_libasound_when_present() {
        match Alsa::load() {
            Ok(_) => {}
            Err(Error::Load(_)) => eprintln!("libasound not installed here; skipping"),
            Err(e) => panic!("{e}"),
        }
    }
}
