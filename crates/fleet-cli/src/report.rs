//! `fleet report` (ADR 0075, spec §2): an agent states how it is doing.
//! Exit codes: 0 ok, 2 usage or other error, 3 forbidden (run by the user
//! rather than inside an agent).

use clap::{Args as ClapArgs, ValueEnum};
use fleet_client::{Connection, Result};
use fleet_proto::{ReportStatus, ReportedState, request};

/// The longest message, in characters (the node enforces it too).
const MESSAGE_MAX: usize = 200;

#[derive(Clone, Copy, Debug, ValueEnum)]
pub enum State {
    Working,
    Waiting,
    Done,
    Error,
}

impl From<State> for ReportedState {
    fn from(s: State) -> Self {
        match s {
            State::Working => ReportedState::ReportedWorking,
            State::Waiting => ReportedState::ReportedWaiting,
            State::Done => ReportedState::ReportedDone,
            State::Error => ReportedState::ReportedError,
        }
    }
}

#[derive(ClapArgs, Debug)]
pub struct Args {
    /// How the agent is doing
    #[arg(value_enum)]
    pub state: State,
    /// A short note (at most 200 characters)
    pub message: Option<String>,
    /// Ask the user a question (only with `waiting`; at most 500
    /// characters). They answer in the GUI or with `fleet answer`; read the
    /// answer with `fleet inbox` or `fleet wait --state message`.
    #[arg(long)]
    pub question: Option<String>,
    /// An answer to offer as a button (repeatable, at most 6, each at most
    /// 60 characters; needs --question)
    #[arg(long = "choice", value_name = "TEXT", requires = "question")]
    pub choices: Vec<String>,
}

/// Reports for the calling agent; the exit code.
pub fn run(client: &mut Connection, args: &Args) -> Result<u8> {
    let message = args.message.clone().unwrap_or_default();
    if message.chars().count() > MESSAGE_MAX {
        eprintln!("fleet: the message is longer than {MESSAGE_MAX} characters");
        return Ok(2);
    }
    if args.question.is_some() && !matches!(args.state, State::Waiting) {
        eprintln!("fleet: --question goes with the state waiting");
        return Ok(2);
    }
    let report = ReportStatus {
        state: ReportedState::from(args.state).into(),
        message,
        question: args.question.clone(),
        choices: args.choices.clone(),
    };
    match client.call(request::Kind::Report(report)) {
        Ok(_) => Ok(0),
        // Exit 3, from `main`.
        Err(e) if e.is_forbidden() => Err(e),
        Err(e) => {
            eprintln!("fleet: {e}");
            Ok(2)
        }
    }
}

/// The open question of a report as JSON (`ls --json`, `tree --json`):
/// `{id, text, choices}`, or null.
pub fn question_json(r: &fleet_proto::Reported) -> serde_json::Value {
    r.question.as_ref().map_or(
        serde_json::Value::Null,
        |q| serde_json::json!({"id": q.id, "text": q.text, "choices": q.choices}),
    )
}
