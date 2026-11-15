//! The seam between the node and whatever runs its agents' terminals.
//!
//! [`Node`](crate::node::Node) drives agents only through [`AgentHost`], so a
//! daemon restart can hand running agents to the next daemon: it freezes the
//! host, passes each agent's terminal on, and the new daemon adopts them.
//! [`PtyHost`](crate::PtyHost) is the implementation; a later one may keep
//! terminals in a holder process, named by a socket instead of an fd.

use crate::pty::{AgentId, AgentStats, CommandSpec, OutputSink, WaitTimeout};
use std::ffi::OsString;
use std::io;
use std::time::{Duration, Instant};

/// How to reach a running agent's terminal.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum TerminalHandle {
    /// The PTY master, an fd open in this process.
    Fd(std::os::fd::RawFd),
}

/// A live agent at the moment its host froze.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct FrozenAgent {
    pub id: AgentId,
    pub pid: u32,
    pub terminal: TerminalHandle,
}

/// A running agent to take over: its process and terminal, at this size.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct AdoptSpec {
    pub pid: u32,
    pub terminal: TerminalHandle,
    pub cols: u16,
    pub rows: u16,
}

/// Runs agents in terminals and drains their output into sinks.
///
/// Output reaches each agent's [`OutputSink`] on a thread of the host's own,
/// as described there; every other method may block briefly and may be
/// called from any thread.
pub trait AgentHost: Send + Sync {
    /// Spawns an agent; `base` (if any) replaces the daemon's environment
    /// under `spec.env`.
    fn spawn_with_base_env(
        &self,
        spec: CommandSpec,
        base: Option<&[(OsString, OsString)]>,
        sink: Box<dyn OutputSink>,
    ) -> io::Result<AgentId>;

    /// Takes over a running agent another daemon froze.
    fn adopt(&self, spec: AdoptSpec, sink: Box<dyn OutputSink>) -> io::Result<AgentId>;

    /// Writes `bytes` to the agent's terminal input, blocking while its input
    /// queue is full. Fails with "fleetd is restarting" while frozen.
    fn write_input(&self, id: AgentId, bytes: &[u8]) -> io::Result<()>;

    /// Resizes the agent's terminal (the child gets SIGWINCH).
    fn resize(&self, id: AgentId, cols: u16, rows: u16) -> io::Result<()>;

    /// SIGTERM to the agent's process group; a no-op once it has exited.
    fn kill(&self, id: AgentId) -> io::Result<()>;

    /// SIGKILL to the agent's process group; a no-op once it has exited.
    fn force_kill(&self, id: AgentId) -> io::Result<()>;

    /// Forgets a finished agent. Fails with `ResourceBusy` while it runs.
    fn remove(&self, id: AgentId) -> io::Result<()>;

    /// Blocks until every agent has exited and been fully drained.
    fn wait_all(&self, timeout: Duration) -> Result<(), WaitTimeout>;

    fn stats(&self, id: AgentId) -> Option<AgentStats>;

    /// Stops every agent's output at a clean boundary: once this returns
    /// `Ok`, no reader holds bytes it has read but not passed to its sink,
    /// and none reads again until [`AgentHost::thaw`]; what agents write
    /// meanwhile stays in the kernel's buffers. Returns the agents whose
    /// output had not ended. Past `deadline` it thaws and fails with
    /// `TimedOut`. While frozen, input writes fail with "fleetd is
    /// restarting", including one already blocked when the freeze began.
    fn freeze(&self, deadline: Instant) -> io::Result<Vec<FrozenAgent>>;

    /// Resumes what [`AgentHost::freeze`] stopped.
    fn thaw(&self);
}
