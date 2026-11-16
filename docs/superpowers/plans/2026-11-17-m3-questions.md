# M3-G: questions answered in the GUI — implementation plan

Issue #108, ADR 0078, spec §5. Proto block 70–74, schema v12.

1. **Store (v12).** `messages.choices`, `agents.question_id/_text/_choices`; `set_reported_asking` (report + question to the user + copy to the parent, one transaction), `answer_question` (answer to the agent + copy to the parent, closes it); `set_reported` clears the question. Test: persistence, stale id, migration from every older version.
2. **Node.** `report_question` (validation: only with `waiting`; limits), `answer` (user only), notices for waiters, the question in `AgentInfo.reported` and `AgentReported`. Tests in `fleet-node/tests/question.rs`.
3. **fleetd.** `Request.answer = 70`; `Report` handler passes the question; scope row `Answer` → forbidden for agents; wire test (scoped event, agents refused, user answers).
4. **CLI.** `fleet report … --question … --choice …`; `fleet answer <id|name> "…"` in `answer.rs` (0/1/2/3); `question` in `ls --json` / `tree --json`; choices in `fleet inbox`. End-to-end test with the real CLI.
5. **GUI core.** `core::question` (`open_question`, `check_answer`), `Session::answer`, `AppSession::answer`.
6. **GUI views.** Question block (text + choice buttons + Reply…) on the sidebar row and under the pane header; the card shows the text and Reply…; the Answer sheet. Workspace tests with the fake session; a session test against a real daemon.
7. ADR, PR.
