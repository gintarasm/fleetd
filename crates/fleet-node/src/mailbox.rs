//! Mailboxes (spec §3, ADR 0076): messages between agents and the user,
//! automatic reports to an agent's parent, and what `fleet wait` waits for.
//!
//! Every agent has a mailbox, and so does the user. A message is data, not
//! an instruction: only an `answer` (the user's reply to a question, M3-G)
//! carries the user's own words.

/// The longest message body, in bytes.
pub const MAX_BODY: usize = 4096;
/// Messages kept per agent's mailbox (the newest).
pub const AGENT_RETENTION: usize = 1_000;
/// Messages kept in the user's mailbox (the newest).
pub const USER_RETENTION: usize = 10_000;

/// What a message is.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum MessageKind {
    /// Sent with `fleet message`.
    Message,
    /// An agent's reported (or inferred) status changed.
    Status,
    /// An agent exited (with its exit code).
    Exit,
    /// An agent was lost (still running when fleetd stopped).
    Lost,
    /// A question to the user (M3-G).
    Question,
    /// The user's answer to a question (M3-G): the only trusted kind.
    Answer,
}

impl MessageKind {
    pub fn as_str(self) -> &'static str {
        match self {
            MessageKind::Message => "message",
            MessageKind::Status => "status",
            MessageKind::Exit => "exit",
            MessageKind::Lost => "lost",
            MessageKind::Question => "question",
            MessageKind::Answer => "answer",
        }
    }

    pub fn parse(s: &str) -> Option<MessageKind> {
        Some(match s {
            "message" => MessageKind::Message,
            "status" => MessageKind::Status,
            "exit" => MessageKind::Exit,
            "lost" => MessageKind::Lost,
            "question" => MessageKind::Question,
            "answer" => MessageKind::Answer,
            _ => return None,
        })
    }
}

/// A mailbox: an agent's, or the user's (`None`).
pub type Mailbox = Option<u64>;

/// A message to store. `sender`/`recipient` `None` is the user.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct NewMessage {
    pub recipient: Mailbox,
    pub sender: Option<u64>,
    pub kind: MessageKind,
    pub body: String,
    /// For `status`: the state (`working`, `waiting`, `done`, `error`).
    pub status: Option<String>,
    /// For `exit`: the exit code.
    pub exit_code: Option<i32>,
}

/// A stored message; `id` is the mailbox cursor (ids only grow).
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Message {
    pub id: u64,
    pub recipient: Mailbox,
    pub sender: Option<u64>,
    pub kind: MessageKind,
    pub body: String,
    pub status: Option<String>,
    pub exit_code: Option<i32>,
    pub created_unix_ms: i64,
    /// For `question`: the offered choices.
    pub choices: Vec<String>,
}

/// A reported status (spec §2). `fleet report` (M3-D) and the inferred
/// `waiting` heuristic feed [`crate::node::Node::status_changed`].
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum Status {
    Working,
    Waiting,
    Done,
    Error,
}

impl Status {
    pub fn as_str(self) -> &'static str {
        match self {
            Status::Working => "working",
            Status::Waiting => "waiting",
            Status::Done => "done",
            Status::Error => "error",
        }
    }

    pub fn parse(s: &str) -> Option<Status> {
        Some(match s {
            "working" => Status::Working,
            "waiting" => Status::Waiting,
            "done" => Status::Done,
            "error" => Status::Error,
            _ => return None,
        })
    }

    /// The status `fleet report` (ADR 0075) stored; `None` for unspecified.
    pub fn of_reported(state: fleet_proto::ReportedState) -> Option<Status> {
        use fleet_proto::ReportedState as R;
        Some(match state {
            R::ReportedWorking => Status::Working,
            R::ReportedWaiting => Status::Waiting,
            R::ReportedDone => Status::Done,
            R::ReportedError => Status::Error,
            R::Unspecified => return None,
        })
    }
}

/// What `fleet wait` can wait for.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum WaitState {
    Waiting,
    Done,
    Error,
    Exited,
    Lost,
    Message,
}

impl WaitState {
    pub const ALL: [WaitState; 6] = [
        WaitState::Waiting,
        WaitState::Done,
        WaitState::Error,
        WaitState::Exited,
        WaitState::Lost,
        WaitState::Message,
    ];

    /// The state a message in a mailbox stands for: a report of a status,
    /// an exit or a loss, or (any other kind) a message.
    pub fn of_message(m: &Message) -> Option<WaitState> {
        match m.kind {
            MessageKind::Exit => Some(WaitState::Exited),
            MessageKind::Lost => Some(WaitState::Lost),
            MessageKind::Status => m
                .status
                .as_deref()
                .and_then(Status::parse)
                .and_then(WaitState::of_status),
            MessageKind::Message | MessageKind::Question | MessageKind::Answer => {
                Some(WaitState::Message)
            }
        }
    }

    /// The wait state a reported status is; `working` is none.
    pub fn of_status(s: Status) -> Option<WaitState> {
        match s {
            Status::Working => None,
            Status::Waiting => Some(WaitState::Waiting),
            Status::Done => Some(WaitState::Done),
            Status::Error => Some(WaitState::Error),
        }
    }
}

/// Something a waiter may care about, broadcast inside the node (never on
/// the PTY output path): a waiter re-checks its mailbox on a message and
/// matches transitions of its targets.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Notice {
    /// A message was stored in `recipient`'s mailbox.
    Message { recipient: Mailbox },
    /// `agent` reached `state` (an exit, or a reported status).
    State { agent: u64, state: WaitState },
}

/// One thing a wait matched: an agent in a state, and the message that
/// told of it, if one did.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct WaitMatch {
    /// The agent; `None` for a message from the user.
    pub agent: Option<u64>,
    pub state: WaitState,
    pub message: Option<Message>,
}
