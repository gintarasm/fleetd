pub mod env;
mod handoff;
pub mod history;
pub mod host;
pub mod node;
pub mod pty;
pub mod scrollback;
pub mod store;
pub mod term {
    pub use fleet_term::*;
}
pub use handoff::end_unadopted;
pub use host::{AdoptSpec, AgentHost, FrozenAgent, TerminalHandle};
pub use pty::{AgentId, AgentInput, AgentStats, CommandSpec, OutputSink, PtyHost, WaitTimeout};
pub use term::Terminal;
