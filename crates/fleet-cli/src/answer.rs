//! `fleet answer <id|name> "text"` (ADR 0078, spec §5): the user answers an
//! agent's open question. Agents may not (exit 3). Exit codes: 0 ok, 1 no
//! such agent or no open question, 2 usage or other error, 3 forbidden.

use clap::Args as ClapArgs;
use fleet_client::{Connection, Error, Result, names};
use fleet_proto::{AnswerQuestion, ListAgents, request, response};

#[derive(ClapArgs, Debug)]
pub struct Args {
    /// The agent that asked (id or name)
    pub agent: String,
    /// The answer
    pub text: String,
}

/// Answers; the exit code.
pub fn run(client: &mut Connection, args: &Args) -> Result<u8> {
    let id = match resolve(client, &args.agent)? {
        Ok(id) => id,
        Err(message) => {
            // An agent that cannot see the name is told it may not answer
            // at all (exit 3), not that the name is unknown: probe with an
            // answer to no agent, which only the user is let through to.
            let probe = answer(0, "");
            if let Err(e) = client.call(request::Kind::Answer(probe))
                && e.is_forbidden()
            {
                return Err(e);
            }
            eprintln!("fleet: {message}");
            return Ok(1);
        }
    };
    match client.call(request::Kind::Answer(answer(id, &args.text))) {
        Ok(_) => Ok(0),
        // Exit 3, from `main`.
        Err(e) if e.is_forbidden() => Err(e),
        Err(e) => {
            eprintln!("fleet: {e}");
            Ok(code_of(&e))
        }
    }
}

fn answer(agent_id: u64, text: &str) -> AnswerQuestion {
    AnswerQuestion {
        agent_id,
        text: text.to_owned(),
        question_id: None,
    }
}

/// The id `arg` names; the inner `Err` is the message for an unknown name.
fn resolve(client: &mut Connection, arg: &str) -> Result<std::result::Result<u64, String>> {
    if names::is_id(arg) {
        return Ok(arg.parse().map_err(|_| format!("bad id {arg}")));
    }
    match client.call(request::Kind::List(ListAgents::default()))? {
        response::Kind::Agents(list) => Ok(names::resolve(&list.agents, arg)),
        _ => Err(Error::Protocol("unexpected reply to list".into())),
    }
}

/// No such agent or no open question is 1, anything else 2.
fn code_of(e: &Error) -> u8 {
    match e {
        Error::Server(m) if m.contains("no open question") || m.contains("not found") => 1,
        _ => 2,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn errors_map_to_exit_codes() {
        assert_eq!(
            code_of(&Error::Server("agent 3 has no open question".into())),
            1
        );
        assert_eq!(
            code_of(&Error::Server("an answer must not be empty".into())),
            2
        );
    }
}
