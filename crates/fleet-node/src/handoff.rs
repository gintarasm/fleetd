//! The mechanics of handing running agents to a successor daemon, apart
//! from the [`Node`](crate::node::Node) state they act on: the gate that
//! holds agent spawns, restarts and forgets out of a handoff, claiming the
//! terminals a handoff record names, and ending agents that could not be
//! restored.
//!
//! [`Node::prepare_handoff`](crate::node::Node::prepare_handoff),
//! [`Node::thaw`](crate::node::Node::thaw) and
//! [`Node::open_with_handoff`](crate::node::Node::open_with_handoff) use them.

use fleet_proto::handoff::handoff_agent::Terminal;
use std::collections::HashSet;
use std::io;
use std::os::fd::{FromRawFd, OwnedFd, RawFd};
use std::sync::{Condvar, Mutex, MutexGuard};
use std::time::{Duration, Instant};

/// How often [`end_unrestored`] checks whether the agents have gone.
const REAP_POLL: Duration = Duration::from_millis(10);
/// How long [`end_unrestored`] waits for agents after SIGKILL.
const KILL_WAIT: Duration = Duration::from_millis(500);

pub(crate) fn restarting() -> io::Error {
    io::Error::other("fleetd is restarting")
}

/// A handoff found an agent whose output has ended but whose exit is not
/// recorded yet, and it did not finish by the deadline.
pub(crate) fn agent_exiting() -> io::Error {
    io::Error::new(io::ErrorKind::WouldBlock, "an agent is exiting; try again")
}

/// How often a handoff checks whether exiting agents have been retired.
pub(crate) const EXITING_POLL: Duration = Duration::from_millis(5);

/// Holds operations that create or remove agents (spawn, M2c restart,
/// forget) out of a handoff: a handoff waits for those in flight, and none
/// starts while it lasts.
#[derive(Default)]
pub(crate) struct HandoffGate {
    state: Mutex<GateState>,
    /// Signalled when the last operation in flight leaves.
    settled: Condvar,
}

#[derive(Default)]
struct GateState {
    handing_off: bool,
    in_flight: usize,
}

/// An operation in flight; leaves the gate when dropped.
pub(crate) struct InFlight<'a>(&'a HandoffGate);

impl Drop for InFlight<'_> {
    fn drop(&mut self) {
        let mut state = self.0.lock();
        state.in_flight -= 1;
        if state.in_flight == 0 {
            self.0.settled.notify_all();
        }
    }
}

impl HandoffGate {
    fn lock(&self) -> MutexGuard<'_, GateState> {
        self.state.lock().unwrap_or_else(|e| e.into_inner())
    }

    /// Admits an operation; fails with "fleetd is restarting" during a
    /// handoff. Operations may nest (a restart spawns).
    pub(crate) fn enter(&self) -> io::Result<InFlight<'_>> {
        let mut state = self.lock();
        if state.handing_off {
            return Err(restarting());
        }
        state.in_flight += 1;
        Ok(InFlight(self))
    }

    /// Starts a handoff: refuses new operations at once, then waits until
    /// `deadline` for those in flight to finish. Fails, leaving no handoff
    /// started, if one already is or operations are still in flight at the
    /// deadline.
    pub(crate) fn begin(&self, deadline: Instant) -> io::Result<()> {
        let mut state = self.lock();
        if state.handing_off {
            return Err(io::Error::new(
                io::ErrorKind::ResourceBusy,
                "a restart is already in progress",
            ));
        }
        // Set before waiting, so a stream of spawns cannot hold it off.
        state.handing_off = true;
        while state.in_flight > 0 {
            let left = deadline.saturating_duration_since(Instant::now());
            if left.is_zero() {
                state.handing_off = false;
                return Err(io::Error::new(
                    io::ErrorKind::WouldBlock,
                    "agents are starting; try again",
                ));
            }
            state = self
                .settled
                .wait_timeout(state, left)
                .unwrap_or_else(|e| e.into_inner())
                .0;
        }
        Ok(())
    }

    /// Ends the handoff (if any): operations are admitted again.
    pub(crate) fn end(&self) {
        self.lock().handing_off = false;
    }
}

/// Takes ownership of the PTY master a record names, if it is a terminal
/// open in this process and not already `claimed` (the fds taken so far
/// from the same record). Called for every agent before the node opens
/// anything: an fd number not open on entry (a stale or corrupt record) is
/// never claimed, so it cannot later name, and close, one of the node's own
/// files. The terminal check keeps a record from naming one of the
/// daemon's own fds that are open by then (the listener, the lock, the log),
/// and the `claimed` check from making two owners of one fd (a double
/// close). `None` for any of those, a negative fd, or a holder socket (not
/// supported yet).
pub(crate) fn claim_terminal(
    terminal: Option<Terminal>,
    claimed: &mut HashSet<RawFd>,
) -> Option<OwnedFd> {
    let fd: RawFd = match terminal? {
        Terminal::Fd(fd) if fd >= 0 => fd,
        Terminal::Fd(_) | Terminal::HolderSocket(_) => return None,
    };
    // SAFETY: fcntl and isatty on any integer are memory-safe; both fail on
    // a closed fd.
    if unsafe { libc::fcntl(fd, libc::F_GETFD) } < 0 || unsafe { libc::isatty(fd) } != 1 {
        return None;
    }
    if !claimed.insert(fd) {
        return None;
    }
    // SAFETY: the fd is open, a terminal, claimed once, and the record
    // hands it to this node.
    Some(unsafe { OwnedFd::from_raw_fd(fd) })
}

/// Ends agents a predecessor daemon left to this process that it does not
/// adopt (a handoff whose record could not be read): SIGTERM to each
/// process group, [`SHUTDOWN_GRACE`](crate::node::SHUTDOWN_GRACE) to exit,
/// then SIGKILL; each is reaped, as they are this process's children.
pub fn end_unadopted(pids: &[i32]) {
    end_unrestored(pids, crate::node::SHUTDOWN_GRACE);
}

/// Ends agents a handoff named that could not be restored, as a shutdown
/// does: SIGTERM to each process group, `grace` to exit, then SIGKILL. They
/// are children of this process (an exec keeps them), so each is reaped
/// here; one that is not (`ECHILD`) counts as gone once it no longer exists.
/// Pids that are not positive are skipped: `killpg` would take them as our
/// own group.
pub(crate) fn end_unrestored(pids: &[i32], grace: Duration) {
    let mut left: Vec<libc::pid_t> = pids.iter().copied().filter(|&p| p > 0).collect();
    if left.is_empty() {
        return;
    }
    signal_groups(&left, libc::SIGTERM);
    reap_until(&mut left, Instant::now() + grace);
    if left.is_empty() {
        return;
    }
    signal_groups(&left, libc::SIGKILL);
    reap_until(&mut left, Instant::now() + KILL_WAIT);
    if !left.is_empty() {
        tracing::warn!(agents = left.len(), "unrestored agents did not exit");
    }
}

fn signal_groups(pids: &[libc::pid_t], sig: libc::c_int) {
    for &pid in pids {
        // SAFETY: killpg has no memory-safety preconditions. ESRCH (the
        // group is gone) is fine.
        unsafe { libc::killpg(pid, sig) };
    }
}

/// Drops from `pids` each process that has gone, until none is left or the
/// deadline passes.
fn reap_until(pids: &mut Vec<libc::pid_t>, deadline: Instant) {
    loop {
        pids.retain(|&pid| !gone(pid));
        if pids.is_empty() || Instant::now() >= deadline {
            return;
        }
        std::thread::sleep(REAP_POLL);
    }
}

/// Reaps `pid` if it has exited; whether it is gone.
fn gone(pid: libc::pid_t) -> bool {
    let mut status = 0;
    // SAFETY: waitpid with a valid out-pointer.
    match unsafe { libc::waitpid(pid, &mut status, libc::WNOHANG) } {
        0 => false,
        r if r == pid => true,
        // Not our child: gone once it no longer exists.
        // SAFETY: kill with signal 0 only checks the process exists.
        _ => (unsafe { libc::kill(pid, 0) }) != 0,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::Arc;

    #[test]
    fn gate_waits_for_operations_in_flight() {
        let gate = Arc::new(HandoffGate::default());
        let in_flight = gate.enter().unwrap();
        let waiter = {
            let gate = Arc::clone(&gate);
            std::thread::spawn(move || gate.begin(Instant::now() + Duration::from_secs(10)))
        };
        // Wait until the handoff has begun: new operations are refused.
        while gate.enter().is_ok() {
            std::thread::yield_now();
        }
        assert_eq!(
            gate.enter().err().unwrap().to_string(),
            "fleetd is restarting"
        );
        drop(in_flight);
        waiter.join().unwrap().unwrap();
        let e = gate.begin(Instant::now()).unwrap_err();
        assert_eq!(e.to_string(), "a restart is already in progress");
        gate.end();
        drop(gate.enter().unwrap());
    }

    #[test]
    fn gate_gives_up_on_operations_still_in_flight() {
        let gate = HandoffGate::default();
        let _in_flight = gate.enter().unwrap();
        let e = gate
            .begin(Instant::now() + Duration::from_millis(20))
            .unwrap_err();
        assert_eq!(e.to_string(), "agents are starting; try again");
        // Not handing off: operations are admitted again.
        drop(gate.enter().unwrap());
    }

    #[test]
    fn only_open_terminals_are_claimed_once() {
        let mut claimed = HashSet::new();
        assert!(claim_terminal(None, &mut claimed).is_none());
        assert!(claim_terminal(Some(Terminal::Fd(-1)), &mut claimed).is_none());
        let socket = Some(Terminal::HolderSocket("/x".into()));
        assert!(claim_terminal(socket, &mut claimed).is_none());
        // Open, but not a terminal: left alone.
        let (r, w) = std::io::pipe().unwrap();
        let pipe = std::os::fd::AsRawFd::as_raw_fd(&r);
        assert!(claim_terminal(Some(Terminal::Fd(pipe)), &mut claimed).is_none());
        // SAFETY: fcntl on an fd we own.
        assert!(
            unsafe { libc::fcntl(pipe, libc::F_GETFD) } >= 0,
            "pipe closed"
        );
        drop((r, w));

        let (mut master, mut slave) = (0, 0);
        // SAFETY: openpty with valid out-pointers and no name/termios/size.
        let r = unsafe {
            libc::openpty(
                &mut master,
                &mut slave,
                std::ptr::null_mut(),
                std::ptr::null_mut(),
                std::ptr::null_mut(),
            )
        };
        assert_eq!(r, 0, "openpty");
        let claimed_fd = claim_terminal(Some(Terminal::Fd(master)), &mut claimed).unwrap();
        assert_eq!(std::os::fd::AsRawFd::as_raw_fd(&claimed_fd), master);
        // The same fd again: no second owner.
        assert!(claim_terminal(Some(Terminal::Fd(master)), &mut claimed).is_none());
        drop(claimed_fd);
        // SAFETY: closing the slave we opened.
        unsafe { libc::close(slave) };
    }
}
