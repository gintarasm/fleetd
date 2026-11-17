//! Questions agents ask the user (ADR 0078, spec §5): which agents have
//! one open, and what an answer may be. gpui-free.

use fleet_proto::{AgentInfo, ReportedState, status::live_report};

/// The longest answer, in bytes (fleetd refuses more).
pub const ANSWER_MAX: usize = 4096;

/// An open question, as the views show it.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct OpenQuestion {
    /// Names the question to fleetd, so an answer to one that was closed
    /// meanwhile is refused instead of landing on the next.
    pub id: u64,
    pub text: String,
    pub choices: Vec<String>,
}

/// The question `info` has open: a live `waiting` report that asked one.
/// An ended agent has none.
pub fn open_question(info: &AgentInfo) -> Option<OpenQuestion> {
    let report = live_report(info)?;
    if report.state() != ReportedState::ReportedWaiting {
        return None;
    }
    let q = report.question.as_ref()?;
    Some(OpenQuestion {
        id: q.id,
        text: q.text.clone(),
        choices: q.choices.clone(),
    })
}

/// The answer typed in the free-text field: trimmed, not empty, within
/// [`ANSWER_MAX`].
pub fn check_answer(text: &str) -> Result<String, String> {
    let text = text.trim();
    if text.is_empty() {
        return Err("Type an answer, or pick one of the choices.".into());
    }
    if text.len() > ANSWER_MAX {
        return Err(format!("An answer is at most {ANSWER_MAX} bytes."));
    }
    Ok(text.to_owned())
}

#[cfg(test)]
mod tests {
    use super::*;
    use fleet_proto::{AgentState, Question, Reported};

    fn info(state: AgentState, rep: ReportedState, question: Option<Question>) -> AgentInfo {
        AgentInfo {
            id: 3,
            state: state.into(),
            reported: Some(Reported {
                state: rep.into(),
                message: String::new(),
                at_unix_ms: 1,
                question,
            }),
            ..Default::default()
        }
    }

    fn q() -> Question {
        Question {
            id: 9,
            text: "Which?".into(),
            choices: vec!["a".into(), "b".into()],
        }
    }

    #[test]
    fn a_waiting_report_with_a_question_is_open() {
        let got = open_question(&info(
            AgentState::Running,
            ReportedState::ReportedWaiting,
            Some(q()),
        ));
        assert_eq!(
            got,
            Some(OpenQuestion {
                id: 9,
                text: "Which?".into(),
                choices: vec!["a".into(), "b".into()],
            })
        );
    }

    #[test]
    fn no_question_when_none_was_asked_or_it_was_closed_by_the_agent_ending() {
        assert_eq!(
            open_question(&info(
                AgentState::Running,
                ReportedState::ReportedWaiting,
                None
            )),
            None
        );
        assert_eq!(
            open_question(&info(
                AgentState::Running,
                ReportedState::ReportedWorking,
                Some(q())
            )),
            None
        );
        for ended in [AgentState::Exited, AgentState::Lost] {
            assert_eq!(
                open_question(&info(ended, ReportedState::ReportedWaiting, Some(q()))),
                None
            );
        }
        assert_eq!(open_question(&AgentInfo::default()), None);
    }

    #[test]
    fn answers_are_trimmed_and_bounded() {
        assert_eq!(check_answer("  yes \n"), Ok("yes".into()));
        assert!(check_answer("   ").is_err());
        assert!(check_answer(&"x".repeat(ANSWER_MAX)).is_ok());
        assert!(check_answer(&"x".repeat(ANSWER_MAX + 1)).is_err());
    }
}
