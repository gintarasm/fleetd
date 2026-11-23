//! The sheets over the panes: connect, New agent, Rename, Answer, Edit
//! Labels, Send to Agents, the kill question, and the ⌘/ cheat sheet's
//! toggle.

use fleet_client::names::name_of;
use gpui::{App, AppContext, Context, Entity, Window};

use super::{Modal, Workspace};
use crate::app::answer_sheet::{AnswerEvent, AnswerSheet};
use crate::app::connect::{ConnectEvent, ConnectSheet, StartupAttempt, new_sheet, switch_target};
use crate::app::keymap;
use crate::app::keymap::{Kill, NewAgent, NewTerminal, SendToAgents};
use crate::app::kill_sheet::{self, KillEvent, KillSheet};
use crate::app::labels_sheet::{LabelsEvent, LabelsSheet};
use crate::app::new_agent::{NewAgentSheet, SheetEvent, SheetSetup};
use crate::app::rename::{RenameEvent, RenameSheet};
use crate::app::send_sheet::{SendEvent, SendSheet};
use crate::core::broadcast::sendable;
use crate::core::config::{save_confirm_kill, save_connection};
use crate::core::kill::{KillFlow, kill_flow};
use crate::core::presets::Prefill;
use crate::core::question::open_question;
use crate::core::sidebar::program_and_id;
use crate::core::status::Subject;
use crate::core::target::Target;

impl Workspace {
    pub fn sheet(&self) -> Option<&Entity<NewAgentSheet>> {
        match &self.modal {
            Some(Modal::NewAgent(s, _)) => Some(s),
            _ => None,
        }
    }

    pub fn rename_sheet(&self) -> Option<&Entity<RenameSheet>> {
        match &self.modal {
            Some(Modal::Rename(s, _)) => Some(s),
            _ => None,
        }
    }

    pub fn labels_sheet(&self) -> Option<&Entity<LabelsSheet>> {
        match &self.modal {
            Some(Modal::Labels(s, _)) => Some(s),
            _ => None,
        }
    }

    pub fn connect_sheet(&self) -> Option<&Entity<ConnectSheet>> {
        match &self.modal {
            Some(Modal::Connect(s, _)) => Some(s),
            _ => None,
        }
    }

    /// The connect sheet is drawn (not a quiet startup attempt).
    pub fn connect_sheet_shown(&self, cx: &App) -> bool {
        self.connect_sheet()
            .is_some_and(|sheet| !sheet.read(cx).quiet)
    }

    pub fn kill_sheet(&self) -> Option<&Entity<KillSheet>> {
        match &self.modal {
            Some(Modal::Kill(s, _)) => Some(s),
            _ => None,
        }
    }

    /// The Send to Agents sheet, while it is open.
    pub fn send_sheet(&self) -> Option<&Entity<SendSheet>> {
        match &self.modal {
            Some(Modal::Send(s, _)) => Some(s),
            _ => None,
        }
    }

    /// ⌘/: shows the cheat sheet of every key binding, or hides it.
    pub fn toggle_shortcuts(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        if matches!(self.modal, Some(Modal::Shortcuts(..))) {
            self.close_modal(window, cx);
        } else if !self.has_modal() {
            let handle = cx.focus_handle();
            window.focus(&handle);
            self.modal = Some(Modal::Shortcuts(keymap::cheat_sheet(cx), handle));
        }
        cx.notify();
    }

    /// Opens the connect sheet (or brings it up), showing `reason`.
    pub fn open_connect_sheet(
        &mut self,
        reason: Option<String>,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        if let Some(Modal::Connect(sheet, _)) = &self.modal {
            let sheet = sheet.clone();
            sheet.update(cx, |s, cx| {
                if reason.is_some() {
                    s.reason = reason;
                }
                s.quiet = false;
                cx.notify();
            });
            sheet.read(cx).focus(window, cx);
            cx.notify();
            return;
        }
        // Over everything else: whatever was open closes.
        self.close_modal(window, cx);
        let Some(sheet) = self.install_connect_sheet(reason, window, cx) else {
            return;
        };
        sheet.read(cx).focus(window, cx);
    }

    /// The launch: connects to `saved` (or a local fleetd that answers),
    /// showing the connect sheet if that takes a while or fails.
    pub fn start_up(&mut self, saved: Option<Target>, window: &mut Window, cx: &mut Context<Self>) {
        if let Some(sheet) = self.install_connect_sheet(None, window, cx) {
            sheet.update(cx, |s, cx| s.startup(saved, cx));
        }
    }

    /// [`Workspace::start_up`] for a connect that started before the
    /// window opened.
    pub fn start_up_from(
        &mut self,
        attempt: StartupAttempt,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        if let Some(sheet) = self.install_connect_sheet(None, window, cx) {
            sheet.update(cx, |s, cx| s.startup_from(attempt, cx));
        }
    }

    fn install_connect_sheet(
        &mut self,
        reason: Option<String>,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> Option<Entity<ConnectSheet>> {
        let sheet = new_sheet(reason, cx)?;
        let events = cx.subscribe_in(&sheet, window, Self::connect_event);
        // A quiet startup sheet appearing takes the keyboard.
        let shown = cx.observe_in(&sheet, window, |_, sheet, window, cx| {
            let visible = !sheet.read(cx).quiet;
            let field = sheet.read(cx).remote.read(cx).focus_handle().clone();
            if visible && !field.contains_focused(window, cx) {
                window.focus(&field);
            }
            cx.notify();
        });
        self.modal = Some(Modal::Connect(sheet.clone(), [events, shown]));
        cx.notify();
        Some(sheet)
    }

    fn connect_event(
        &mut self,
        _sheet: &Entity<ConnectSheet>,
        event: &ConnectEvent,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        match event {
            ConnectEvent::Connected { target, save } => {
                if *save {
                    let dir = self.app.read(cx).state_dir().to_owned();
                    if let Err(e) = save_connection(&dir, target) {
                        eprintln!("fleet-gui: cannot save the connection: {e}");
                    }
                }
                switch_target(target.clone(), cx);
            }
            ConnectEvent::Dismiss => {}
        }
        self.close_modal(window, cx);
    }

    pub fn open_new_agent(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        self.open_new_agent_with(Prefill::default(), window, cx);
    }

    /// The New agent sheet, its fields prefilled from a preset or an agent
    /// being duplicated (ADR 0084). An open sheet is only focused.
    pub fn open_new_agent_with(
        &mut self,
        mut prefill: Prefill,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        match &self.modal {
            Some(Modal::Connect(..)) => return,
            Some(Modal::NewAgent(sheet, _)) => {
                sheet.read(cx).focus(window, cx);
                return;
            }
            // It opens over any other (which closes).
            _ => self.drop_modal(cx),
        }
        let presets = self.app.update(cx, |a, _| a.presets());
        let app = self.app.read(cx);
        // Settings › Agents: a default folder beats the recent one.
        if prefill.dir.is_none() && !app.config.default_dir.is_empty() {
            prefill.dir = Some(app.config.default_dir.clone());
        }
        let setup = SheetSetup {
            default_command: app.config.default_command.clone(),
            dirs: app.dir_choices(),
            recent_commands: app.recent_commands(),
            presets,
            prefill,
            worktree: self.worktree_setup(cx),
        };
        let target = app.target.clone();
        let taken = app.live_names();
        let sheet = cx.new(|cx| NewAgentSheet::new(setup, &target, taken, cx));
        let sub = cx.subscribe_in(&sheet, window, Self::sheet_event);
        sheet.read(cx).focus(window, cx);
        self.modal = Some(Modal::NewAgent(sheet, sub));
        cx.notify();
    }

    /// Opens the Rename sheet for an agent (a double-click on its name, or
    /// Rename… in a menu).
    pub fn open_rename(&mut self, agent_id: u64, window: &mut Window, cx: &mut Context<Self>) {
        if self.has_modal() {
            return;
        }
        let Some(info) = self
            .app
            .read(cx)
            .agents
            .iter()
            .find(|r| r.info.id == agent_id)
            .map(|r| r.info.clone())
        else {
            return;
        };
        let name = name_of(&info).unwrap_or_default().to_owned();
        let what = program_and_id(&info);
        let subject = Subject::of(&info);
        let sheet = cx.new(|cx| RenameSheet::new(agent_id, name, what, subject, cx));
        let sub = cx.subscribe_in(&sheet, window, Self::rename_event);
        sheet.read(cx).focus(window, cx);
        self.modal = Some(Modal::Rename(sheet, sub));
        cx.notify();
    }

    fn rename_event(
        &mut self,
        _sheet: &Entity<RenameSheet>,
        event: &RenameEvent,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        if let RenameEvent::Rename { agent_id, name } = event {
            self.app.read(cx).rename(*agent_id, name.clone());
        }
        self.close_modal(window, cx);
    }

    /// Opens the Answer sheet for an agent's open question (Reply… on its
    /// row, header or graph card, ADR 0078).
    pub fn open_answer(&mut self, agent_id: u64, window: &mut Window, cx: &mut Context<Self>) {
        if self.has_modal() {
            return;
        }
        let Some(info) = self
            .app
            .read(cx)
            .agents
            .iter()
            .find(|r| r.info.id == agent_id)
            .map(|r| r.info.clone())
        else {
            return;
        };
        let Some(question) = open_question(&info) else {
            return;
        };
        let what = format!(
            "{} · {}",
            name_of(&info).unwrap_or(&info.program),
            program_and_id(&info)
        );
        let sheet = cx.new(|cx| AnswerSheet::new(agent_id, what, question, cx));
        let sub = cx.subscribe_in(&sheet, window, Self::answer_event);
        sheet.read(cx).focus(window, cx);
        self.modal = Some(Modal::Answer(sheet, sub));
        cx.notify();
    }

    pub fn answer_sheet(&self) -> Option<&Entity<AnswerSheet>> {
        match &self.modal {
            Some(Modal::Answer(s, _)) => Some(s),
            _ => None,
        }
    }

    fn answer_event(
        &mut self,
        _sheet: &Entity<AnswerSheet>,
        event: &AnswerEvent,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        if let AnswerEvent::Send {
            agent_id,
            question,
            text,
        } = event
        {
            self.app.read(cx).answer(*agent_id, *question, text.clone());
        }
        self.close_modal(window, cx);
    }

    /// A click on a choice button: answers question `question` of
    /// `agent_id` with it (ADR 0078).
    pub fn answer_with(
        &mut self,
        agent_id: u64,
        question: u64,
        text: String,
        cx: &mut Context<Self>,
    ) {
        self.app.read(cx).answer(agent_id, question, text);
    }

    /// Opens the Edit Labels sheet for an agent (Edit Labels… in the row
    /// and pane menus).
    pub fn open_labels(&mut self, agent_id: u64, window: &mut Window, cx: &mut Context<Self>) {
        if self.has_modal() {
            return;
        }
        let Some(info) = self
            .app
            .read(cx)
            .agents
            .iter()
            .find(|r| r.info.id == agent_id)
            .map(|r| r.info.clone())
        else {
            return;
        };
        let what = match name_of(&info) {
            Some(name) => format!("{name} · {}", program_and_id(&info)),
            None => program_and_id(&info),
        };
        let sheet = cx.new(|cx| LabelsSheet::new(agent_id, info.labels.clone(), what, cx));
        let sub = cx.subscribe_in(&sheet, window, Self::labels_event);
        sheet.read(cx).focus(window, cx);
        self.modal = Some(Modal::Labels(sheet, sub));
        cx.notify();
    }

    fn labels_event(
        &mut self,
        _sheet: &Entity<LabelsSheet>,
        event: &LabelsEvent,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        if let LabelsEvent::Save { agent_id, edit } = event
            && !edit.is_empty()
        {
            self.app
                .read(cx)
                .set_labels(*agent_id, edit.set.clone(), edit.remove.clone());
        }
        self.close_modal(window, cx);
    }

    /// Opens Send to Agents… with `preselect` checked (those running).
    /// Not over a sheet, and not without a running agent.
    pub fn open_send(&mut self, preselect: &[u64], window: &mut Window, cx: &mut Context<Self>) {
        if self.has_modal() {
            return;
        }
        if !self.app.read(cx).agents.iter().any(sendable) {
            self.app.update(cx, |a, cx| {
                a.show_error("No running agent to send to.".to_owned(), cx);
            });
            return;
        }
        let app = self.app.clone();
        let sheet = cx.new(|cx| SendSheet::new(app, preselect, cx));
        let sub = cx.subscribe_in(&sheet, window, Self::send_event);
        sheet.read(cx).focus(window, cx);
        self.modal = Some(Modal::Send(sheet, sub));
        cx.notify();
    }

    /// ⇧⌘M: Send to Agents…, the sidebar's selection checked.
    pub(super) fn send_to_agents(
        &mut self,
        _: &SendToAgents,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let preselect = self.row_selection.ids().to_vec();
        self.open_send(&preselect, window, cx);
    }

    fn send_event(
        &mut self,
        _sheet: &Entity<SendSheet>,
        event: &SendEvent,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        if let SendEvent::Send(plan) = event {
            self.app.update(cx, |a, cx| a.send_text(plan, cx));
        }
        self.close_modal(window, cx);
    }

    pub(super) fn new_agent(&mut self, _: &NewAgent, window: &mut Window, cx: &mut Context<Self>) {
        self.open_new_agent(window, cx);
    }

    /// ⌘T: a plain login shell, focused (see [`AppState::new_terminal`]).
    pub fn open_new_terminal(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        if self.has_modal() {
            return;
        }
        if let Err(e) = self.app.update(cx, |a, cx| a.new_terminal(cx)) {
            self.app.update(cx, |a, cx| a.show_error(e, cx));
        }
        self.focus_current(window, cx);
        cx.notify();
    }

    pub(super) fn new_terminal(
        &mut self,
        _: &NewTerminal,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        self.open_new_terminal(window, cx);
    }

    fn sheet_event(
        &mut self,
        sheet: &Entity<NewAgentSheet>,
        event: &SheetEvent,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        self.remember_worktree(sheet, cx);
        match event {
            SheetEvent::Cancel => {}
            SheetEvent::SpawnInWorktree {
                req,
                command,
                worktree,
            } => self.spawn_in_worktree(req, command, worktree, window, cx),
            SheetEvent::Spawn { req, command } => {
                let result = self.app.update(cx, |a, cx| {
                    a.spawn_from_sheet(req.clone(), command.clone(), cx)
                });
                if let Err(e) = result {
                    sheet.update(cx, |s, cx| s.set_error(e, cx));
                    return;
                }
            }
        }
        self.close_modal(window, cx);
    }

    /// Asks before killing (only the question's Kill kills), unless
    /// `[gui] confirm_kill` is off.
    pub(super) fn kill(&mut self, action: &Kill, window: &mut Window, cx: &mut Context<Self>) {
        if self.has_modal() {
            return;
        }
        let agent_id = action.agent_id;
        let app = self.app.read(cx);
        match kill_flow(app.config.confirm_kill) {
            KillFlow::Kill => app.kill(agent_id),
            KillFlow::Ask => {
                let question = kill_sheet::question(app, agent_id);
                let sheet = cx.new(|cx| KillSheet::new(agent_id, question, cx));
                let sub = cx.subscribe_in(&sheet, window, Self::kill_event);
                sheet.read(cx).focus(window);
                self.modal = Some(Modal::Kill(sheet, sub));
            }
        }
        cx.notify();
    }

    pub(super) fn kill_event(
        &mut self,
        sheet: &Entity<KillSheet>,
        event: &KillEvent,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        if let KillEvent::Kill { dont_ask } = *event {
            let agent_ids = sheet.read(cx).agent_ids.clone();
            self.app.update(cx, |a, _| {
                if dont_ask {
                    a.config.confirm_kill = false;
                    if let Err(e) = save_confirm_kill(a.state_dir(), false) {
                        eprintln!("fleet-gui: cannot save confirm_kill: {e}");
                    }
                }
                for id in agent_ids {
                    a.kill(id);
                }
            });
        }
        self.close_modal(window, cx);
    }
}
