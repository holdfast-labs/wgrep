//! Bounded ingress for watcher notifications.

use std::path::Path;
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use std::sync::{Arc, Condvar, Mutex, mpsc};
use std::sync::mpsc::{SyncSender, TrySendError};

use notify::{Event, EventKind};

const MAX_EVENTS: usize = 256;
const MAX_PATHS: usize = 4096;
const MAX_PAYLOAD_BYTES: usize = 1024 * 1024;

#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
pub(crate) struct Stats {
    pub events: usize,
    pub paths: usize,
    pub payload_bytes: usize,
    pub untrusted: bool,
}

#[derive(Default)]
struct Usage {
    events: AtomicUsize,
    paths: AtomicUsize,
    bytes: AtomicUsize,
    untrusted: AtomicBool,
    cv: Arc<Condvar>,
    gate: Arc<Mutex<()>>,
}

/// Capacity remains charged until the accepted event's ticket is dropped.
pub(crate) struct Ticket(Arc<Usage>, usize, usize);

impl Drop for Ticket {
    fn drop(&mut self) {
        self.0.events.fetch_sub(1, Ordering::AcqRel);
        self.0.paths.fetch_sub(self.1, Ordering::AcqRel);
        self.0.bytes.fetch_sub(self.2, Ordering::AcqRel);
        let _gate = self.0.gate.lock().unwrap();
        self.0.cv.notify_all();
    }
}

pub(crate) struct Work {
    event: notify::Result<Event>,
    _ticket: Ticket,
}

impl Work {
    pub(crate) fn event(&self) -> &notify::Result<Event> { &self.event }
}

/// Read-only health view. It cannot enqueue work or keep a sender alive.
#[derive(Clone)]
pub(crate) struct Health(Arc<Usage>);

impl Health {
    pub(crate) fn condvar(&self) -> Arc<Condvar> { self.0.cv.clone() }
    pub(crate) fn gate(&self) -> Arc<Mutex<()>> { self.0.gate.clone() }
    pub(crate) fn stats(&self) -> Stats {
        Stats {
            events: self.0.events.load(Ordering::Acquire),
            paths: self.0.paths.load(Ordering::Acquire),
            payload_bytes: self.0.bytes.load(Ordering::Acquire),
            untrusted: self.0.untrusted.load(Ordering::Acquire),
        }
    }
    pub(crate) fn mark_untrusted(&self) { self.0.untrusted.store(true, Ordering::Release); }
}

pub(crate) struct Ingress {
    tx: SyncSender<Work>,
    usage: Arc<Usage>,
    max_events: usize,
    max_paths: usize,
    max_bytes: usize,
}

pub(crate) struct Receiver(mpsc::Receiver<Work>);

pub(crate) fn bounded() -> (Ingress, Receiver, Health) {
    make_bounded(MAX_EVENTS, MAX_PATHS, MAX_PAYLOAD_BYTES)
}

#[cfg(test)]
pub(super) fn bounded_with_limits(max_events: usize, max_paths: usize, max_bytes: usize) -> (Ingress, Receiver, Health) {
    make_bounded(max_events, max_paths, max_bytes)
}

fn make_bounded(max_events: usize, max_paths: usize, max_bytes: usize) -> (Ingress, Receiver, Health) {
    let (tx, rx) = mpsc::sync_channel(max_events);
    let usage = Arc::new(Usage { gate: Arc::new(Mutex::new(())), cv: Arc::new(Condvar::new()), ..Usage::default() });
    let health = Health(usage.clone());
    (Ingress { tx, usage, max_events, max_paths, max_bytes }, Receiver(rx), health)
}

impl Ingress {
    /// Return immediately. Read-only access events are ignored before reservation.
    pub(crate) fn try_ingest(&self, event: notify::Result<Event>) {
        if is_read(&event) {
            return;
        }
        if event.is_err() {
            self.usage.untrusted.store(true, Ordering::Release);
        }
        let (paths, bytes) = payload(&event);
        let reserved_events = paths <= self.max_paths && bytes <= self.max_bytes
            && reserve(&self.usage.events, 1, self.max_events);
        let reserved_paths = reserved_events && reserve(&self.usage.paths, paths, self.max_paths);
        let reserved_bytes = reserved_paths && reserve(&self.usage.bytes, bytes, self.max_bytes);
        if !reserved_bytes {
            self.usage.untrusted.store(true, Ordering::Release);
            if reserved_paths { self.usage.paths.fetch_sub(paths, Ordering::AcqRel); }
            if reserved_events { self.usage.events.fetch_sub(1, Ordering::AcqRel); }
            return;
        }
        let ticket = Ticket(self.usage.clone(), paths, bytes);
        match self.tx.try_send(Work { event, _ticket: ticket }) {
            Ok(()) => {}
            Err(TrySendError::Full(work) | TrySendError::Disconnected(work)) => {
                self.usage.untrusted.store(true, Ordering::Release);
                drop(work);
            }
        }
    }

    #[cfg(test)]
    pub(crate) fn stats(&self) -> Stats {
        Health(self.usage.clone()).stats()
    }
}

impl Receiver {
    pub(crate) fn try_recv(&self) -> Result<Work, mpsc::TryRecvError> { self.0.try_recv() }
    pub(crate) fn recv(&self) -> Result<Work, mpsc::RecvError> { self.0.recv() }
    pub(crate) fn recv_timeout(&self, timeout: std::time::Duration) -> Result<Work, mpsc::RecvTimeoutError> { self.0.recv_timeout(timeout) }
}

fn reserve(counter: &AtomicUsize, amount: usize, limit: usize) -> bool {
    let mut old = counter.load(Ordering::Acquire);
    loop {
        let Some(next) = old.checked_add(amount) else { return false };
        if next > limit { return false; }
        match counter.compare_exchange_weak(old, next, Ordering::AcqRel, Ordering::Acquire) {
            Ok(_) => return true,
            Err(actual) => old = actual,
        }
    }
}

fn payload(event: &notify::Result<Event>) -> (usize, usize) {
    match event {
        Ok(event) => {
            let mut bytes = 0usize;
            for path in &event.paths {
                bytes = match bytes.checked_add(path_bytes(path)) { Some(n) => n, None => return (usize::MAX, usize::MAX) };
            }
            (event.paths.len(), bytes)
        }
        Err(_) => (0, 0),
    }
}

fn path_bytes(path: &Path) -> usize {
    #[cfg(unix)] { use std::os::unix::ffi::OsStrExt; path.as_os_str().as_bytes().len() }
    #[cfg(windows)] { use std::os::windows::ffi::OsStrExt; path.as_os_str().encode_wide().fold(0usize, |n, _| n.saturating_add(2)) }
    #[cfg(not(any(unix, windows)))] { path.to_string_lossy().len() }
}

fn is_read(event: &notify::Result<Event>) -> bool {
    if let Ok(event) = event {
        if let EventKind::Access(kind) = event.kind {
            use notify::event::{AccessKind, AccessMode};
            return !matches!(kind, AccessKind::Close(AccessMode::Write));
        }
    }
    false
}

#[cfg(test)]
mod tests {
    use super::*;
    use notify::event::{AccessKind, AccessMode};

    fn event(path: &str) -> notify::Result<Event> { Ok(Event::new(EventKind::Create(notify::event::CreateKind::File)).add_path(path.into())) }
    fn small() -> (Ingress, Receiver) {
        let (ingress, rx, _) = bounded_with_limits(2, MAX_PATHS, MAX_PAYLOAD_BYTES);
        (ingress, rx)
    }

    #[test]
    fn full_queue_returns_and_marks_untrusted() {
        let (ingress, _rx) = small();
        ingress.try_ingest(event("a"));
        ingress.try_ingest(event("b"));
        ingress.try_ingest(event("c"));
        assert_eq!(ingress.stats().events, 2);
        assert!(ingress.stats().untrusted);
    }

    #[test]
    fn path_and_payload_limits_reject_without_leaking_partial_reservations() {
        let (ingress, _rx) = small();
        let mut many_paths = Event::new(EventKind::Create(notify::event::CreateKind::File));
        many_paths.paths = (0..=MAX_PATHS).map(|_| "p".into()).collect();
        ingress.try_ingest(Ok(many_paths));
        assert_eq!(ingress.stats().events, 0);
        assert_eq!(ingress.stats().paths, 0);

        let huge = "x".repeat(MAX_PAYLOAD_BYTES + 1);
        ingress.try_ingest(event(&huge));
        assert_eq!(ingress.stats().events, 0);
        assert_eq!(ingress.stats().paths, 0);
        assert_eq!(ingress.stats().payload_bytes, 0);
        assert!(ingress.stats().untrusted);
    }

    #[test]
    fn tickets_charge_deferred_work_until_drop() {
        let (ingress, rx) = small();
        ingress.try_ingest(event("a"));
        let deferred = rx.try_recv().unwrap();
        ingress.try_ingest(event("b"));
        ingress.try_ingest(event("c"));
        assert_eq!(ingress.stats().events, 2);
        assert!(ingress.stats().untrusted);
        drop(deferred);
        assert_eq!(ingress.stats().events, 1);
    }

    #[test]
    fn reservations_release_on_success_error_and_drop() {
        let (ingress, rx) = small();
        ingress.try_ingest(event("a"));
        let work = rx.try_recv().unwrap();
        drop(work);
        ingress.try_ingest(Err(notify::Error::generic("test")));
        drop(rx.try_recv().unwrap());
        assert_eq!(ingress.stats().events, 0);
        assert_eq!(ingress.stats().paths, 0);
        assert_eq!(ingress.stats().payload_bytes, 0);
    }

    #[test]
    fn notify_errors_are_counted_and_make_health_untrusted() {
        let (ingress, rx) = small();
        ingress.try_ingest(Err(notify::Error::generic("watch failed")));
        assert_eq!(ingress.stats().events, 1);
        assert!(ingress.stats().untrusted);
        let work = rx.try_recv().unwrap();
        assert!(work.event().is_err());
        drop(work);
        assert_eq!(ingress.stats().events, 0);
    }

    #[test]
    fn disconnected_receiver_marks_health_untrusted() {
        let (ingress, rx) = small();
        drop(rx);
        ingress.try_ingest(event("lost"));
        assert!(ingress.stats().untrusted);
        assert_eq!(ingress.stats().events, 0);
    }

    #[test]
    fn reads_are_ignored_and_loss_stays_sticky_after_drain() {
        let (ingress, rx) = small();
        let read = Event::new(EventKind::Access(AccessKind::Read)).add_path("ignored".into());
        ingress.try_ingest(Ok(read));
        assert_eq!(ingress.stats().events, 0);
        ingress.try_ingest(event("a"));
        ingress.try_ingest(event("b"));
        ingress.try_ingest(event("c"));
        drop(rx.try_recv().unwrap());
        drop(rx.try_recv().unwrap());
        assert_eq!(ingress.stats().events, 0);
        assert!(ingress.stats().untrusted);
        let _ = AccessMode::Read;
    }
}
