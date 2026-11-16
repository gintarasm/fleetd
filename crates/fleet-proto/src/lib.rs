//! Fleet wire protocol: protobuf schema, frame codec and handshake constants.

mod frame;

mod pb {
    include!(concat!(env!("OUT_DIR"), "/fleet.v1.rs"));
}

/// The record a daemon hands to its successor across `exec` (fleet.handoff.v1).
pub mod handoff {
    include!(concat!(env!("OUT_DIR"), "/fleet.handoff.v1.rs"));
}

pub use frame::{Frame, FrameCodec, FrameError};
pub use pb::*;

/// Wire protocol version; a mismatch is answered with `HandshakeReject`.
pub const PROTOCOL_VERSION: u32 = 2;
/// Maximum accepted value of a frame's length prefix.
pub const MAX_FRAME: usize = 16 * 1024 * 1024;
/// Version of the handoff record; a successor refuses a record it cannot read.
pub const HANDOFF_VERSION: u32 = 1;

/// The environment variable holding an agent's token (ADR 0072): set by the
/// node in every agent's environment, sent by the CLI in `Hello`.
pub const AGENT_TOKEN_ENV: &str = "FLEET_AGENT_TOKEN";
