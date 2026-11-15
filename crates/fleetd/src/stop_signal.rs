//! SIGTERM, SIGINT and SIGHUP stop the daemon (decision 0039).
//!
//! fleetd installs its own handlers instead of tokio's `signal()`, so that
//! they can be armed before the socket is bound, with no runtime yet, and
//! so that no signal is lost: tokio installs its handler and only then
//! subscribes, and a signal broadcast in between reaches no one (issue #23).
//! Here the handler records the first signal and writes a byte to a
//! self-pipe; the byte stays there until [`StopSignal::recv`] reads it.

use std::io;
use std::os::fd::IntoRawFd;
use std::os::unix::net::UnixStream as StdUnixStream;
use std::sync::atomic::{AtomicI32, Ordering};

/// The signals that stop the daemon.
const STOP: [libc::c_int; 3] = [libc::SIGTERM, libc::SIGINT, libc::SIGHUP];

/// The pipe's write end, or -1 while no handler is armed.
static WRITE_FD: AtomicI32 = AtomicI32::new(-1);
/// The first stop signal received, or 0.
static RECEIVED: AtomicI32 = AtomicI32::new(0);

extern "C" fn on_stop(sig: libc::c_int) {
    // Only async-signal-safe work: atomics and write(2), errno preserved.
    let errno = io::Error::last_os_error().raw_os_error().unwrap_or(0);
    let _ = RECEIVED.compare_exchange(0, sig, Ordering::SeqCst, Ordering::SeqCst);
    let fd = WRITE_FD.load(Ordering::SeqCst);
    if fd >= 0 {
        // Non-blocking: once a byte is pending, a full pipe changes nothing.
        // SAFETY: writes one byte from a valid buffer.
        unsafe { libc::write(fd, [1u8].as_ptr().cast(), 1) };
    }
    set_errno(errno);
}

fn set_errno(v: libc::c_int) {
    // SAFETY: the thread's errno location is always valid.
    unsafe {
        #[cfg(target_os = "macos")]
        {
            *libc::__error() = v;
        }
        #[cfg(not(target_os = "macos"))]
        {
            *libc::__errno_location() = v;
        }
    }
}

/// The armed stop signals: the pipe's read end. Pass it to
/// [`crate::Daemon::serve`]. The write end stays open for the life of the
/// process (or until [`disarm`]), so a late signal never writes to a
/// reused fd number.
pub struct StopSignal {
    read: StdUnixStream,
}

/// Installs fleetd's handlers for SIGTERM, SIGINT and SIGHUP. From here on
/// those signals no longer kill the process: they are kept until
/// [`StopSignal::recv`]. Call it once, before binding the socket. Exec
/// resets the handlers, so children (and a restart's new binary) start with
/// the default dispositions.
pub fn arm() -> io::Result<StopSignal> {
    // std creates both ends close-on-exec.
    let (read, write) = StdUnixStream::pair()?;
    read.set_nonblocking(true)?;
    write.set_nonblocking(true)?;
    WRITE_FD.store(write.into_raw_fd(), Ordering::SeqCst);
    for sig in STOP {
        // SAFETY: a zeroed sigaction with a valid handler and mask.
        unsafe {
            let mut sa: libc::sigaction = std::mem::zeroed();
            sa.sa_sigaction = on_stop as *const () as libc::sighandler_t;
            sa.sa_flags = libc::SA_RESTART;
            libc::sigemptyset(&mut sa.sa_mask);
            if libc::sigaction(sig, &sa, std::ptr::null_mut()) != 0 {
                return Err(io::Error::last_os_error());
            }
        }
    }
    Ok(StopSignal { read })
}

/// Restores the default dispositions of the stop signals, for a process
/// that armed them and then does not serve (see `main`'s upgrade prompt).
/// A signal received in the meantime is not re-raised.
pub fn disarm(stop: StopSignal) {
    for sig in STOP {
        // SAFETY: resetting a signal to its default disposition.
        unsafe { libc::signal(sig, libc::SIG_DFL) };
    }
    let fd = WRITE_FD.swap(-1, Ordering::SeqCst);
    if fd >= 0 {
        // SAFETY: the write end `arm` leaked; no handler uses it any more.
        unsafe { libc::close(fd) };
    }
    drop(stop);
}

/// The name of the first stop signal received, if any.
pub(crate) fn received() -> Option<&'static str> {
    match RECEIVED.load(Ordering::SeqCst) {
        libc::SIGTERM => Some("SIGTERM"),
        libc::SIGINT => Some("SIGINT"),
        libc::SIGHUP => Some("SIGHUP"),
        _ => None,
    }
}

impl StopSignal {
    /// Waits for a stop signal (one received before this call counts) and
    /// returns its name.
    pub(crate) async fn recv(self) -> io::Result<&'static str> {
        let read = tokio::net::UnixStream::from_std(self.read)?;
        loop {
            if let Some(name) = received() {
                return Ok(name);
            }
            read.readable().await?;
            let mut buf = [0u8; 16];
            match read.try_read(&mut buf) {
                Ok(_) => {}
                Err(e) if e.kind() == io::ErrorKind::WouldBlock => {}
                Err(e) => return Err(e),
            }
        }
    }
}
