pub mod env;
pub mod git;
mod handoff;
pub mod history;
pub mod host;
pub mod node;
pub mod priority;
pub mod pty;
pub mod scope;
pub mod scrollback;
pub mod store;
pub mod token;
pub mod term {
    pub use fleet_term::*;
}
pub use handoff::end_unadopted;
pub use host::{AdoptSpec, AgentHost, FrozenAgent, TerminalHandle};
pub use pty::{AgentId, AgentInput, AgentStats, CommandSpec, OutputSink, PtyHost, WaitTimeout};
pub use term::Terminal;
