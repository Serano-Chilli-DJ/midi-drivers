//! One thread per device that submits, reaps and dispatches URBs.
//!
//! usbfs returns completed URBs for the whole file descriptor, so a single
//! owner has to reap them and route each to the stream that submitted it.
//! Streams register a [`UrbHandler`]; the reactor calls it on completion with
//! the URB (buffer and status) and resubmits or parks it as the handler asks.

use crate::usbfs::{Urb, UsbfsDevice};
use log::{debug, warn};
use std::collections::HashMap;
use std::io;
use std::os::fd::AsRawFd;
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use std::sync::mpsc::{self, Receiver, Sender};
use std::sync::Arc;
use std::thread::{self, JoinHandle};
use std::time::Duration;

/// What to do with a URB after its handler ran.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Action {
    /// Submit it again (the handler refilled the buffer for OUT streams).
    Resubmit,
    /// Keep it parked; the stream is stopping.
    Park,
}

/// Completion callback for one stream. Runs on the reactor thread, so it
/// must be quick: copy data to a channel or ring buffer and return.
pub trait UrbHandler: Send {
    /// Called with every completed URB of this stream.
    fn complete(&mut self, urb: &mut Urb) -> Action;
    /// Called once when the stream is removed (all URBs parked).
    fn stopped(&mut self) {}
}

/// Identifier of a registered stream.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub struct StreamId(usize);

enum Command {
    Add { id: StreamId, urbs: Vec<Urb>, handler: Box<dyn UrbHandler>, reply: Sender<io::Result<()>> },
    Remove { id: StreamId, reply: Sender<()> },
    Shutdown,
}

struct StreamState {
    urbs: HashMap<usize, Urb>,
    handler: Box<dyn UrbHandler>,
    stopping: bool,
    in_flight: usize,
    pending_remove: Option<Sender<()>>,
}

/// Handle to the reactor thread.
pub struct Reactor {
    dev: Arc<UsbfsDevice>,
    tx: Sender<Command>,
    thread: Option<JoinHandle<()>>,
    next_id: AtomicUsize,
    disconnected: Arc<AtomicBool>,
}

static NEXT_TOKEN: AtomicUsize = AtomicUsize::new(1);

/// Allocate a process-unique URB token.
pub fn next_token() -> usize {
    NEXT_TOKEN.fetch_add(1, Ordering::Relaxed)
}

impl Reactor {
    /// Start the reactor thread for `dev`.
    pub fn start(dev: Arc<UsbfsDevice>) -> Self {
        let (tx, rx) = mpsc::channel();
        let d = Arc::clone(&dev);
        let disconnected = Arc::new(AtomicBool::new(false));
        let flag = Arc::clone(&disconnected);
        let thread = thread::Builder::new()
            .name("ns7ii-usb-reactor".into())
            .spawn(move || run(d, rx, flag))
            .expect("spawn reactor thread");
        Self { dev, tx, thread: Some(thread), next_id: AtomicUsize::new(1), disconnected }
    }

    /// True once the device has gone away (reap returned ENODEV).
    pub fn is_disconnected(&self) -> bool {
        self.disconnected.load(Ordering::SeqCst)
    }

    /// The device this reactor serves.
    pub fn device(&self) -> &Arc<UsbfsDevice> {
        &self.dev
    }

    /// Register a stream: the reactor submits every URB in `urbs` right away.
    pub fn add_stream(&self, urbs: Vec<Urb>, handler: Box<dyn UrbHandler>) -> io::Result<StreamId> {
        let id = StreamId(self.next_id.fetch_add(1, Ordering::Relaxed));
        let (reply, wait) = mpsc::channel();
        self.tx
            .send(Command::Add { id, urbs, handler, reply })
            .map_err(|_| io::Error::new(io::ErrorKind::BrokenPipe, "reactor gone"))?;
        wait.recv().map_err(|_| io::Error::new(io::ErrorKind::BrokenPipe, "reactor gone"))??;
        Ok(id)
    }

    /// Cancel a stream's URBs and wait until all of them are reaped.
    pub fn remove_stream(&self, id: StreamId) {
        let (reply, wait) = mpsc::channel();
        if self.tx.send(Command::Remove { id, reply }).is_ok() {
            let _ = wait.recv_timeout(Duration::from_secs(3));
        }
    }
}

impl Drop for Reactor {
    fn drop(&mut self) {
        let _ = self.tx.send(Command::Shutdown);
        if let Some(t) = self.thread.take() {
            let _ = t.join();
        }
    }
}

fn poll_completion(dev: &UsbfsDevice, timeout_ms: i32) {
    let mut pfd = libc::pollfd { fd: dev_fd(dev), events: libc::POLLOUT, revents: 0 };
    // SAFETY: `pfd` is a valid pollfd array of length 1 for the duration of the call.
    let _ = unsafe { libc::poll(&mut pfd, 1, timeout_ms) };
}

fn dev_fd(dev: &UsbfsDevice) -> i32 {
    dev.as_raw_fd()
}

fn run(dev: Arc<UsbfsDevice>, rx: Receiver<Command>, disconnected: Arc<AtomicBool>) {
    let mut streams: HashMap<StreamId, StreamState> = HashMap::new();
    let mut token_owner: HashMap<usize, StreamId> = HashMap::new();
    let mut shutdown = false;
    loop {
        // Commands.
        loop {
            match rx.try_recv() {
                Ok(Command::Add { id, mut urbs, handler, reply }) => {
                    let mut st = StreamState {
                        urbs: HashMap::new(),
                        handler,
                        stopping: false,
                        in_flight: 0,
                        pending_remove: None,
                    };
                    let mut result = Ok(());
                    for urb in urbs.iter_mut() {
                        if let Err(e) = dev.submit(urb) {
                            warn!("submit failed on EP {:02x}: {e}", urb.endpoint());
                            result = Err(e);
                            break;
                        }
                        st.in_flight += 1;
                    }
                    if result.is_err() {
                        for urb in urbs.iter_mut() {
                            let _ = dev.discard(urb);
                        }
                        // Reap what we submitted so the URBs can be dropped safely.
                        while st.in_flight > 0 {
                            match dev.reap() {
                                Ok(tok) => {
                                    if let Some(u) = urbs.iter_mut().find(|u| u.token == tok) {
                                        u.mark_reaped();
                                        st.in_flight -= 1;
                                    } else {
                                        // Belongs to another stream: dispatch below.
                                        dispatch(&dev, &mut streams, &mut token_owner, tok);
                                    }
                                }
                                Err(_) => break,
                            }
                        }
                        let _ = reply.send(result);
                        continue;
                    }
                    for urb in urbs {
                        token_owner.insert(urb.token, id);
                        st.urbs.insert(urb.token, urb);
                    }
                    streams.insert(id, st);
                    let _ = reply.send(Ok(()));
                }
                Ok(Command::Remove { id, reply }) => {
                    if let Some(st) = streams.get_mut(&id) {
                        st.stopping = true;
                        for urb in st.urbs.values_mut() {
                            let _ = dev.discard(urb);
                        }
                        if st.in_flight == 0 {
                            finish_stream(&mut streams, &mut token_owner, id);
                            let _ = reply.send(());
                        } else {
                            st.pending_remove = Some(reply);
                        }
                    } else {
                        let _ = reply.send(());
                    }
                }
                Ok(Command::Shutdown) => {
                    shutdown = true;
                    for st in streams.values_mut() {
                        st.stopping = true;
                        for urb in st.urbs.values_mut() {
                            let _ = dev.discard(urb);
                        }
                    }
                }
                Err(mpsc::TryRecvError::Empty) => break,
                Err(mpsc::TryRecvError::Disconnected) => {
                    shutdown = true;
                    break;
                }
            }
        }
        let any_in_flight = streams.values().any(|s| s.in_flight > 0);
        if shutdown && !any_in_flight {
            for (_, mut st) in streams.drain() {
                st.handler.stopped();
            }
            debug!("reactor exit");
            return;
        }
        if any_in_flight {
            poll_completion(&dev, 5);
            loop {
                match dev.try_reap() {
                    Ok(Some(tok)) => dispatch(&dev, &mut streams, &mut token_owner, tok),
                    Ok(None) => break,
                    Err(e) => {
                        // ENODEV: device unplugged. Park everything.
                        warn!("reap failed: {e}");
                        if e.raw_os_error() == Some(libc::ENODEV) {
                            disconnected.store(true, Ordering::SeqCst);
                        }
                        for st in streams.values_mut() {
                            st.stopping = true;
                            for urb in st.urbs.values_mut() {
                                urb.mark_reaped();
                            }
                            st.in_flight = 0;
                        }
                        let ids: Vec<_> = streams.keys().copied().collect();
                        for id in ids {
                            if let Some(reply) = streams.get_mut(&id).and_then(|s| s.pending_remove.take()) {
                                finish_stream(&mut streams, &mut token_owner, id);
                                let _ = reply.send(());
                            }
                        }
                        break;
                    }
                }
            }
        } else {
            thread::sleep(Duration::from_millis(5));
        }
    }
}

fn dispatch(
    dev: &UsbfsDevice,
    streams: &mut HashMap<StreamId, StreamState>,
    token_owner: &mut HashMap<usize, StreamId>,
    token: usize,
) {
    let Some(id) = token_owner.get(&token).copied() else {
        warn!("reaped unknown URB token {token}");
        return;
    };
    let Some(st) = streams.get_mut(&id) else { return };
    let Some(urb) = st.urbs.get_mut(&token) else { return };
    urb.mark_reaped();
    st.in_flight -= 1;
    if st.stopping {
        if st.in_flight == 0 {
            if let Some(reply) = st.pending_remove.take() {
                finish_stream(streams, token_owner, id);
                let _ = reply.send(());
            }
        }
        return;
    }
    let action = st.handler.complete(urb);
    if action == Action::Resubmit {
        match dev.submit(urb) {
            Ok(()) => st.in_flight += 1,
            Err(e) => {
                warn!("resubmit failed on EP {:02x}: {e}", urb.endpoint());
                st.stopping = true;
            }
        }
    }
}

fn finish_stream(
    streams: &mut HashMap<StreamId, StreamState>,
    token_owner: &mut HashMap<usize, StreamId>,
    id: StreamId,
) {
    if let Some(mut st) = streams.remove(&id) {
        for tok in st.urbs.keys() {
            token_owner.remove(tok);
        }
        st.handler.stopped();
    }
}
