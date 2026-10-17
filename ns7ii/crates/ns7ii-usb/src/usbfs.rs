//! Minimal Linux `usbfs` layer (`/dev/bus/usb/BBB/DDD`).
//!
//! `nusb` 0.2 covers control, bulk and interrupt transfers but has no
//! isochronous support, and the NS7II plays audio over an isochronous OUT
//! endpoint. Rather than mixing two USB stacks on one interface (usbfs
//! claims are per file descriptor, so that cannot work), this module talks
//! to usbfs directly for everything. It is the only module in the workspace
//! that contains `unsafe`; every `unsafe` block is a single ioctl or an
//! allocation whose layout is checked right above it.
//!
//! Reference: `include/uapi/linux/usbdevice_fs.h`.

use std::alloc::{alloc_zeroed, dealloc, Layout};
use std::fs::{File, OpenOptions};
use std::io;
use std::os::fd::AsRawFd;
use std::ptr;
use std::time::Duration;

/// Transfer type codes for `usbdevfs_urb.type`.
pub mod urb_type {
    /// Isochronous.
    pub const ISO: u8 = 0;
    /// Interrupt.
    pub const INTERRUPT: u8 = 1;
    /// Control.
    pub const CONTROL: u8 = 2;
    /// Bulk.
    pub const BULK: u8 = 3;
}

/// URB flags.
pub mod urb_flags {
    /// Short IN packets are errors.
    pub const SHORT_NOT_OK: u32 = 0x01;
    /// Start isochronous transfer as soon as possible.
    pub const ISO_ASAP: u32 = 0x02;
    /// Continue a bulk transfer across URBs.
    pub const BULK_CONTINUATION: u32 = 0x04;
    /// Terminate a bulk OUT with a zero-length packet.
    pub const ZERO_PACKET: u32 = 0x40;
    /// Do not signal on completion (kernel hint).
    pub const NO_INTERRUPT: u32 = 0x80;
}

// ioctl number construction (asm-generic/ioctl.h).
const IOC_NRBITS: u32 = 8;
const IOC_TYPEBITS: u32 = 8;
const IOC_SIZEBITS: u32 = 14;
const IOC_NRSHIFT: u32 = 0;
const IOC_TYPESHIFT: u32 = IOC_NRSHIFT + IOC_NRBITS;
const IOC_SIZESHIFT: u32 = IOC_TYPESHIFT + IOC_TYPEBITS;
const IOC_DIRSHIFT: u32 = IOC_SIZESHIFT + IOC_SIZEBITS;
const IOC_NONE: u32 = 0;
const IOC_WRITE: u32 = 1;
const IOC_READ: u32 = 2;

const fn ioc(dir: u32, nr: u32, size: usize) -> libc::c_ulong {
    ((dir << IOC_DIRSHIFT)
        | ((b'U' as u32) << IOC_TYPESHIFT)
        | (nr << IOC_NRSHIFT)
        | ((size as u32) << IOC_SIZESHIFT)) as libc::c_ulong
}

#[repr(C)]
struct CtrlTransfer {
    request_type: u8,
    request: u8,
    value: u16,
    index: u16,
    length: u16,
    timeout: u32,
    data: *mut libc::c_void,
}

#[repr(C)]
struct BulkTransfer {
    ep: u32,
    len: u32,
    timeout: u32,
    data: *mut libc::c_void,
}

#[repr(C)]
struct SetInterface {
    interface: u32,
    altsetting: u32,
}

#[repr(C)]
struct GetDriver {
    interface: u32,
    driver: [u8; 256],
}

#[repr(C)]
struct DisconnectClaim {
    interface: u32,
    flags: u32,
    driver: [u8; 256],
}

#[repr(C)]
struct IoctlPassthrough {
    ifno: libc::c_int,
    ioctl_code: libc::c_int,
    data: *mut libc::c_void,
}

/// Kernel's `struct usbdevfs_iso_packet_desc`.
#[repr(C)]
#[derive(Debug, Clone, Copy, Default)]
pub struct IsoPacketDesc {
    /// Requested length.
    pub length: u32,
    /// Transferred length.
    pub actual_length: u32,
    /// Per-packet status (negative errno).
    pub status: i32,
}

/// Kernel's `struct usbdevfs_urb` header (iso packet descriptors follow it).
#[repr(C)]
struct UrbHeader {
    typ: u8,
    endpoint: u8,
    status: i32,
    flags: u32,
    buffer: *mut libc::c_void,
    buffer_length: i32,
    actual_length: i32,
    start_frame: i32,
    number_of_packets: i32,
    error_count: i32,
    signr: u32,
    usercontext: *mut libc::c_void,
}

const USBDEVFS_CONTROL: libc::c_ulong = ioc(IOC_READ | IOC_WRITE, 0, std::mem::size_of::<CtrlTransfer>());
const USBDEVFS_BULK: libc::c_ulong = ioc(IOC_READ | IOC_WRITE, 2, std::mem::size_of::<BulkTransfer>());
const USBDEVFS_SETINTERFACE: libc::c_ulong = ioc(IOC_READ, 4, std::mem::size_of::<SetInterface>());
const USBDEVFS_SETCONFIGURATION: libc::c_ulong = ioc(IOC_READ, 5, std::mem::size_of::<u32>());
const USBDEVFS_GETDRIVER: libc::c_ulong = ioc(IOC_WRITE, 8, std::mem::size_of::<GetDriver>());
const USBDEVFS_SUBMITURB: libc::c_ulong = ioc(IOC_READ, 10, std::mem::size_of::<UrbHeader>());
const USBDEVFS_DISCARDURB: libc::c_ulong = ioc(IOC_NONE, 11, 0);
const USBDEVFS_REAPURB: libc::c_ulong = ioc(IOC_WRITE, 12, std::mem::size_of::<*mut libc::c_void>());
const USBDEVFS_REAPURBNDELAY: libc::c_ulong = ioc(IOC_WRITE, 13, std::mem::size_of::<*mut libc::c_void>());
const USBDEVFS_CLAIMINTERFACE: libc::c_ulong = ioc(IOC_READ, 15, std::mem::size_of::<u32>());
const USBDEVFS_RELEASEINTERFACE: libc::c_ulong = ioc(IOC_READ, 16, std::mem::size_of::<u32>());
const USBDEVFS_IOCTL: libc::c_ulong = ioc(IOC_READ | IOC_WRITE, 18, std::mem::size_of::<IoctlPassthrough>());
const USBDEVFS_RESET: libc::c_ulong = ioc(IOC_NONE, 20, 0);
const USBDEVFS_CLEAR_HALT: libc::c_ulong = ioc(IOC_READ, 21, std::mem::size_of::<u32>());
const USBDEVFS_DISCONNECT: libc::c_int = ioc(IOC_NONE, 22, 0) as libc::c_int;
const USBDEVFS_CONNECT: libc::c_int = ioc(IOC_NONE, 23, 0) as libc::c_int;
const USBDEVFS_DISCONNECT_CLAIM: libc::c_ulong = ioc(IOC_READ, 27, std::mem::size_of::<DisconnectClaim>());
const USBDEVFS_GET_SPEED: libc::c_ulong = ioc(IOC_NONE, 31, 0);
const USBDEVFS_DISCONNECT_CLAIM_EXCEPT_DRIVER: u32 = 0x02;

/// An open usbfs device node.
#[derive(Debug)]
pub struct UsbfsDevice {
    file: File,
    path: String,
}

/// USB connection speed as reported by `USBDEVFS_GET_SPEED`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Speed {
    /// 1.5 Mbit/s.
    Low,
    /// 12 Mbit/s.
    Full,
    /// 480 Mbit/s.
    High,
    /// 5 Gbit/s or more.
    Super,
    /// Unknown code.
    Unknown(i32),
}

impl AsRawFd for UsbfsDevice {
    fn as_raw_fd(&self) -> std::os::fd::RawFd {
        self.file.as_raw_fd()
    }
}

impl UsbfsDevice {
    /// Open `/dev/bus/usb/BBB/DDD` read/write.
    pub fn open(path: &str) -> io::Result<Self> {
        let file = OpenOptions::new().read(true).write(true).open(path)?;
        Ok(Self { file, path: path.to_string() })
    }

    /// Path this device was opened from.
    pub fn path(&self) -> &str {
        &self.path
    }

    fn ioctl<T>(&self, req: libc::c_ulong, arg: *mut T) -> io::Result<libc::c_int> {
        // SAFETY: `req` is a usbfs ioctl whose argument type is `T` by
        // construction above, and `arg` points to a live `T` owned by the caller.
        let r = unsafe { libc::ioctl(self.file.as_raw_fd(), req as _, arg) };
        if r < 0 {
            Err(io::Error::last_os_error())
        } else {
            Ok(r)
        }
    }

    /// Current connection speed.
    pub fn speed(&self) -> io::Result<Speed> {
        let code = self.ioctl::<libc::c_void>(USBDEVFS_GET_SPEED, ptr::null_mut())?;
        Ok(match code {
            1 => Speed::Low,
            2 => Speed::Full,
            3 => Speed::High,
            4..=6 => Speed::Super,
            other => Speed::Unknown(other),
        })
    }

    /// Name of the kernel driver bound to `interface`, if any.
    pub fn kernel_driver(&self, interface: u8) -> io::Result<Option<String>> {
        let mut gd = GetDriver { interface: interface as u32, driver: [0; 256] };
        match self.ioctl(USBDEVFS_GETDRIVER, &mut gd) {
            Ok(_) => {
                let end = gd.driver.iter().position(|b| *b == 0).unwrap_or(256);
                Ok(Some(String::from_utf8_lossy(&gd.driver[..end]).into_owned()))
            }
            Err(e) if e.raw_os_error() == Some(libc::ENODATA) => Ok(None),
            Err(e) => Err(e),
        }
    }

    /// Detach any kernel driver from `interface` and claim it atomically.
    pub fn detach_and_claim(&self, interface: u8) -> io::Result<()> {
        let mut dc = DisconnectClaim {
            interface: interface as u32,
            flags: USBDEVFS_DISCONNECT_CLAIM_EXCEPT_DRIVER,
            driver: [0; 256],
        };
        dc.driver[..5].copy_from_slice(b"usbfs");
        self.ioctl(USBDEVFS_DISCONNECT_CLAIM, &mut dc).map(|_| ())
    }

    /// Claim `interface` without detaching a kernel driver.
    pub fn claim(&self, interface: u8) -> io::Result<()> {
        let mut i = interface as u32;
        self.ioctl(USBDEVFS_CLAIMINTERFACE, &mut i).map(|_| ())
    }

    /// Release `interface`.
    pub fn release(&self, interface: u8) -> io::Result<()> {
        let mut i = interface as u32;
        self.ioctl(USBDEVFS_RELEASEINTERFACE, &mut i).map(|_| ())
    }

    /// Ask the kernel driver for `interface` to disconnect (before claiming).
    pub fn disconnect_kernel_driver(&self, interface: u8) -> io::Result<()> {
        let mut p = IoctlPassthrough { ifno: interface as _, ioctl_code: USBDEVFS_DISCONNECT, data: ptr::null_mut() };
        self.ioctl(USBDEVFS_IOCTL, &mut p).map(|_| ())
    }

    /// Re-attach the kernel driver to `interface` (after releasing it).
    pub fn reconnect_kernel_driver(&self, interface: u8) -> io::Result<()> {
        let mut p = IoctlPassthrough { ifno: interface as _, ioctl_code: USBDEVFS_CONNECT, data: ptr::null_mut() };
        self.ioctl(USBDEVFS_IOCTL, &mut p).map(|_| ())
    }

    /// `SET_CONFIGURATION`.
    pub fn set_configuration(&self, config: u8) -> io::Result<()> {
        let mut c = config as u32;
        self.ioctl(USBDEVFS_SETCONFIGURATION, &mut c).map(|_| ())
    }

    /// `SET_INTERFACE` (alternate setting) on a claimed interface.
    pub fn set_alt_setting(&self, interface: u8, alt: u8) -> io::Result<()> {
        let mut s = SetInterface { interface: interface as u32, altsetting: alt as u32 };
        self.ioctl(USBDEVFS_SETINTERFACE, &mut s).map(|_| ())
    }

    /// Clear a halted endpoint (`endpoint` includes the direction bit).
    pub fn clear_halt(&self, endpoint: u8) -> io::Result<()> {
        let mut e = endpoint as u32;
        self.ioctl(USBDEVFS_CLEAR_HALT, &mut e).map(|_| ())
    }

    /// Port reset. Re-enumerates the device; all claims are kept by usbfs.
    pub fn reset(&self) -> io::Result<()> {
        self.ioctl::<libc::c_void>(USBDEVFS_RESET, ptr::null_mut()).map(|_| ())
    }

    /// Synchronous control transfer. For IN requests `data` is filled and the
    /// number of bytes received is returned; for OUT requests it is sent.
    pub fn control(
        &self,
        request_type: u8,
        request: u8,
        value: u16,
        index: u16,
        data: &mut [u8],
        timeout: Duration,
    ) -> io::Result<usize> {
        let mut ct = CtrlTransfer {
            request_type,
            request,
            value,
            index,
            length: data.len() as u16,
            timeout: timeout.as_millis().min(u32::MAX as u128) as u32,
            data: data.as_mut_ptr() as *mut libc::c_void,
        };
        self.ioctl(USBDEVFS_CONTROL, &mut ct).map(|n| n as usize)
    }

    /// Synchronous bulk (or interrupt) transfer on `endpoint` (direction bit included).
    pub fn bulk(&self, endpoint: u8, data: &mut [u8], timeout: Duration) -> io::Result<usize> {
        let mut bt = BulkTransfer {
            ep: endpoint as u32,
            len: data.len() as u32,
            timeout: timeout.as_millis().min(u32::MAX as u128) as u32,
            data: data.as_mut_ptr() as *mut libc::c_void,
        };
        self.ioctl(USBDEVFS_BULK, &mut bt).map(|n| n as usize)
    }

    /// Submit a URB. The URB must stay alive (not dropped) until it is reaped.
    pub fn submit(&self, urb: &mut Urb) -> io::Result<()> {
        if urb.in_flight {
            return Err(io::Error::new(io::ErrorKind::InvalidInput, "URB already submitted"));
        }
        self.ioctl(USBDEVFS_SUBMITURB, urb.header_mut()).map(|_| ())?;
        urb.in_flight = true;
        Ok(())
    }

    /// Cancel an in-flight URB. It still has to be reaped afterwards.
    pub fn discard(&self, urb: &mut Urb) -> io::Result<()> {
        if !urb.in_flight {
            return Ok(());
        }
        match self.ioctl(USBDEVFS_DISCARDURB, urb.header_mut()) {
            Ok(_) => Ok(()),
            Err(e) if e.raw_os_error() == Some(libc::EINVAL) => Ok(()), // already completed
            Err(e) => Err(e),
        }
    }

    /// Block until any URB completes and return its `token`.
    pub fn reap(&self) -> io::Result<usize> {
        let mut p: *mut libc::c_void = ptr::null_mut();
        self.ioctl(USBDEVFS_REAPURB, &mut p)?;
        Ok(p as usize)
    }

    /// Reap without blocking; `Ok(None)` when nothing has completed.
    pub fn try_reap(&self) -> io::Result<Option<usize>> {
        let mut p: *mut libc::c_void = ptr::null_mut();
        match self.ioctl(USBDEVFS_REAPURBNDELAY, &mut p) {
            Ok(_) => Ok(Some(p as usize)),
            Err(e) if e.raw_os_error() == Some(libc::EAGAIN) => Ok(None),
            Err(e) => Err(e),
        }
    }
}

/// Give the calling thread real-time (`SCHED_FIFO`) priority `prio`, so
/// audio keeps flowing while the desktop is busy. Works when running as
/// root (the systemd service) or with an RLIMIT_RTPRIO grant; otherwise the
/// thread stays at normal priority, which is logged once at debug level.
/// Returns whether it worked.
pub fn raise_thread_priority(what: &str, prio: i32) -> bool {
    let param = libc::sched_param { sched_priority: prio };
    // SAFETY: plain syscall on the calling thread (pid 0) with a valid sched_param.
    let r = unsafe { libc::sched_setscheduler(0, libc::SCHED_FIFO, &param) };
    if r == 0 {
        log::info!("{what} thread: real-time priority {prio} (SCHED_FIFO)");
        true
    } else {
        log::debug!("{what} thread: no real-time priority ({}); running at normal priority", io::Error::last_os_error());
        false
    }
}

/// An asynchronous transfer: owns the kernel URB structure and the data buffer.
///
/// The header and its isochronous packet descriptors live in one heap block
/// with a stable address, which is what the kernel requires between submit
/// and reap. The buffer is a plain `Vec<u8>` that is never reallocated while
/// the URB is in flight (the type gives out no `&mut Vec` in that state).
pub struct Urb {
    block: *mut u8,
    layout: Layout,
    packets: usize,
    buffer: Vec<u8>,
    in_flight: bool,
    /// Value returned by `reap` for this URB: the address of the kernel-visible
    /// URB structure (usbfs reports completions by that pointer).
    pub token: usize,
}

// SAFETY: the raw block pointer is owned exclusively by this struct; it is only
// dereferenced through `&self`/`&mut self` methods.
unsafe impl Send for Urb {}

impl std::fmt::Debug for Urb {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Urb")
            .field("endpoint", &self.endpoint())
            .field("packets", &self.packets)
            .field("len", &self.buffer.len())
            .field("in_flight", &self.in_flight)
            .finish()
    }
}

impl Urb {
    /// Allocate a URB for `endpoint` (direction bit included) of `typ`, with a
    /// buffer of `len` bytes and `packets` isochronous packet descriptors
    /// (0 for bulk/interrupt). `tag` is stored in `usercontext` for debugging.
    pub fn new(typ: u8, endpoint: u8, len: usize, packets: usize, tag: usize) -> Self {
        let size = std::mem::size_of::<UrbHeader>() + packets * std::mem::size_of::<IsoPacketDesc>();
        let layout = Layout::from_size_align(size.max(1), std::mem::align_of::<UrbHeader>())
            .expect("valid URB layout");
        // SAFETY: layout has non-zero size; the block is freed in Drop with the same layout.
        let block = unsafe { alloc_zeroed(layout) };
        assert!(!block.is_null(), "URB allocation failed");
        let token = block as usize;
        let mut urb = Self { block, layout, packets, buffer: vec![0u8; len], in_flight: false, token };
        let h = urb.header_mut();
        h.typ = typ;
        h.endpoint = endpoint;
        h.number_of_packets = packets as i32;
        h.usercontext = tag as *mut libc::c_void;
        urb.sync_buffer_ptr();
        urb
    }

    fn header_mut(&mut self) -> &mut UrbHeader {
        // SAFETY: `block` is a live allocation of at least `size_of::<UrbHeader>()`
        // bytes with the header's alignment, zero-initialised at allocation.
        unsafe { &mut *(self.block as *mut UrbHeader) }
    }

    fn header(&self) -> &UrbHeader {
        // SAFETY: as in `header_mut`.
        unsafe { &*(self.block as *const UrbHeader) }
    }

    fn sync_buffer_ptr(&mut self) {
        let ptr = self.buffer.as_mut_ptr() as *mut libc::c_void;
        let len = self.buffer.len() as i32;
        let h = self.header_mut();
        h.buffer = ptr;
        h.buffer_length = len;
    }

    /// Endpoint address.
    pub fn endpoint(&self) -> u8 {
        self.header().endpoint
    }

    /// Whether the URB is between submit and reap.
    pub fn in_flight(&self) -> bool {
        self.in_flight
    }

    /// Mark as reaped (called by the stream code after `reap` returned this token).
    pub fn mark_reaped(&mut self) {
        self.in_flight = false;
    }

    /// Set URB flags (see [`urb_flags`]).
    pub fn set_flags(&mut self, flags: u32) {
        self.header_mut().flags = flags;
    }

    /// Completion status (negative errno, 0 on success).
    pub fn status(&self) -> i32 {
        self.header().status
    }

    /// Bytes actually transferred (bulk/interrupt), or the sum for iso.
    pub fn actual_length(&self) -> usize {
        self.header().actual_length.max(0) as usize
    }

    /// Number of isochronous packets that reported an error.
    pub fn error_count(&self) -> i32 {
        self.header().error_count
    }

    /// Start frame assigned by the controller.
    pub fn start_frame(&self) -> i32 {
        self.header().start_frame
    }

    /// Data buffer (read after an IN completion, fill before an OUT submit).
    pub fn buffer(&self) -> &[u8] {
        &self.buffer
    }

    /// Mutable data buffer. Not available while in flight.
    pub fn buffer_mut(&mut self) -> Option<&mut [u8]> {
        if self.in_flight {
            None
        } else {
            Some(&mut self.buffer)
        }
    }

    /// Set the requested length of the transfer (must not exceed the allocation).
    pub fn set_length(&mut self, len: usize) {
        assert!(len <= self.buffer.len());
        assert!(!self.in_flight);
        self.header_mut().buffer_length = len as i32;
    }

    /// Isochronous packet descriptors.
    pub fn iso_packets(&self) -> &[IsoPacketDesc] {
        if self.packets == 0 {
            return &[];
        }
        // SAFETY: `packets` descriptors were allocated directly after the header.
        unsafe {
            let p = self.block.add(std::mem::size_of::<UrbHeader>()) as *const IsoPacketDesc;
            std::slice::from_raw_parts(p, self.packets)
        }
    }

    /// Mutable isochronous packet descriptors. Not available while in flight.
    pub fn iso_packets_mut(&mut self) -> Option<&mut [IsoPacketDesc]> {
        if self.in_flight || self.packets == 0 {
            return None;
        }
        // SAFETY: as in `iso_packets`; exclusive through `&mut self`.
        Some(unsafe {
            let p = self.block.add(std::mem::size_of::<UrbHeader>()) as *mut IsoPacketDesc;
            std::slice::from_raw_parts_mut(p, self.packets)
        })
    }
}

impl Drop for Urb {
    fn drop(&mut self) {
        debug_assert!(!self.in_flight, "URB dropped while in flight");
        // SAFETY: `block` came from `alloc_zeroed` with exactly this layout.
        unsafe { dealloc(self.block, self.layout) };
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn ioctl_numbers_match_kernel_headers_on_64bit() {
        if std::mem::size_of::<usize>() == 8 {
            assert_eq!(USBDEVFS_CONTROL, 0xC018_5500);
            assert_eq!(USBDEVFS_BULK, 0xC018_5502);
            assert_eq!(USBDEVFS_SUBMITURB, 0x8038_550A);
            assert_eq!(USBDEVFS_REAPURB, 0x4008_550C);
            assert_eq!(USBDEVFS_CLAIMINTERFACE, 0x8004_550F);
            assert_eq!(USBDEVFS_DISCONNECT_CLAIM, 0x8108_551B);
        }
        assert_eq!(USBDEVFS_RESET, 0x5514);
        assert_eq!(USBDEVFS_GET_SPEED, 0x551F);
    }

    #[test]
    fn urb_layout_matches_kernel() {
        if std::mem::size_of::<usize>() == 8 {
            assert_eq!(std::mem::size_of::<UrbHeader>(), 56);
        }
        assert_eq!(std::mem::size_of::<IsoPacketDesc>(), 12);
        let mut u = Urb::new(urb_type::ISO, 0x05, 3 * 512, 3, 7);
        assert_eq!(u.iso_packets().len(), 3);
        for p in u.iso_packets_mut().unwrap() {
            p.length = 512;
        }
        assert_eq!(u.header().number_of_packets, 3);
        assert_eq!(u.header().buffer_length, 1536);
        assert_eq!(u.token, u.block as usize);
        assert_eq!(u.endpoint(), 0x05);
    }
}
