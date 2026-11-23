//! The command palette, Search All Agents, the agent switcher and ⌘J.

use gpui::{App, AppContext, Context, Entity, ModifiersChangedEvent, Window};

use super::{Modal, Switcher, Workspace};
use crate::app::keymap::{CommandPalette, SearchAllAgents};
use crate::app::kill_sheet::{self};
use crate::app::palette::{command_action, command_shortcut};
use crate::app::picker_view::{PickerEvent, PickerView};
use crate::app::search_all_view::{SearchAllEvent, SearchAllView};
use crate::core::broadcast::sendable;
use crate::core::palette::{Entry, PaletteState, palette_items};
use crate::core::presets::{Prefill, preset_request};
use crate::core::spawn::SpawnRequest;
use crate::core::switcher::{start_row, switcher_items};

impl Workspace {
    /// The command palette, while it is open.
    pub fn palette(&self) -> Option<&Entity<PickerView>> {
        match &self.modal {
            Some(Modal::Palette(p, _, _)) => Some(p),
            _ => None,
        }
    }

    /// Search All Agents, while it is open.
    pub fn search_all(&self) -> Option<&Entity<SearchAllView>> {
        match &self.modal {
            Some(Modal::SearchAll(v, _)) => Some(v),
            _ => None,
        }
    }

    /// The agent switcher, while it is open.
    pub fn switcher(&self) -> Option<&Entity<PickerView>> {
        match &self.modal {
            Some(Modal::Switcher(s)) => Some(&s.view),
            _ => None,
        }
    }

    /// ⌘P: opens the command palette (actions, agents, recent folders),
    /// or closes it if open. Not over a sheet.
    pub fn toggle_palette(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        if self.palette().is_some() {
            self.close_modal(window, cx);
            return;
        }
        if self.has_modal() {
            return;
        }
        let state = self.palette_state(cx);
        let presets = self.app.update(cx, |a, _| a.presets());
        let app = self.app.read(cx);
        let mut rows = palette_items(
            |c| command_shortcut(c, window),
            &state,
            &presets,
            &app.agents,
            &app.dir_choices(),
            &app.home,
        );
        rows.extend(self.palette_changes(cx));
        let (entries, items): (Vec<Entry>, Vec<_>) = rows.into_iter().unzip();
        let recents = self.palette_recents.clone();
        let palette =
            cx.new(|cx| PickerView::new(items, recents, "Search actions, agents and folders", cx));
        let sub = cx.subscribe_in(&palette, window, Self::palette_event);
        palette.read(cx).focus(window, cx);
        self.modal = Some(Modal::Palette(palette, entries, sub));
        cx.notify();
    }

    /// What the palette's commands depend on: the shown tab's panes, the
    /// tabs, and the focused pane's agent (as its menu has them).
    pub(super) fn palette_state(&self, cx: &App) -> PaletteState {
        let app = self.app.read(cx);
        let pane = app.focused();
        let view = app.pane_view(pane).map(|v| v.read(cx));
        let agent = app.layout().agent(pane);
        let bulk = self.bulk_targets(cx);
        PaletteState {
            panes: app.layout().panes().len(),
            tabs: app.tabs().len(),
            has_agent: agent.is_some(),
            running: agent.is_some_and(|id| kill_sheet::running(app, id)),
            restartable: view.is_some_and(|v| v.restartable()),
            has_selection: view.is_some_and(|v| v.has_selection()),
            any_running: app.agents.iter().any(sendable),
            selected_running: bulk.running.len(),
            selected_ended: bulk.ended.len(),
            can_show_changes: agent.is_some_and(|id| self.offers_changes(id, cx)),
            can_archive: agent.is_some_and(|id| self.offers_archive(id, cx)),
            has_actions: !app.actions().all().is_empty(),
        }
    }

    /// ⌘J: shows the next agent that needs you in the focused pane (ADR
    /// 0100); a short toast when none does. Not over a sheet.
    pub fn next_needs_you(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        if self.has_modal() {
            return;
        }
        let target = {
            let app = self.app.read(cx);
            let current = app.layout().agent(app.focused());
            app.next_needing_you(current)
        };
        match target {
            Some(id) => self.show_agent(id, window, cx),
            None => self.app.update(cx, |a, cx| {
                a.show_notice("No agent needs you".to_owned(), cx)
            }),
        }
    }

    /// ⇧⌘F: opens Search All Agents (or closes it if open). Not over a
    /// sheet.
    pub(super) fn search_all_agents(
        &mut self,
        _: &SearchAllAgents,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        if self.search_all().is_some() {
            self.close_modal(window, cx);
            return;
        }
        if self.has_modal() {
            return;
        }
        let app = self.app.clone();
        let view = cx.new(|cx| SearchAllView::new(app, cx));
        let sub = cx.subscribe_in(&view, window, Self::search_all_event);
        view.read(cx).focus(window, cx);
        self.modal = Some(Modal::SearchAll(view, sub));
        cx.notify();
    }

    /// A Search All Agents pick shows the agent in the focused pane and
    /// finds the line there.
    fn search_all_event(
        &mut self,
        _view: &Entity<SearchAllView>,
        event: &SearchAllEvent,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        if self.search_all().is_none() {
            return;
        }
        self.close_modal(window, cx);
        let SearchAllEvent::Show {
            agent_id,
            line,
            query,
            case_sensitive,
        } = event
        else {
            return;
        };
        self.show_agent(*agent_id, window, cx);
        let app = self.app.read(cx);
        if let Some(view) = app.pane_view(app.focused()).cloned() {
            view.update(cx, |v, cx| {
                v.find_hit(query, *case_sensitive, *line, window, cx)
            });
        }
    }

    pub(super) fn command_palette(
        &mut self,
        _: &CommandPalette,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        self.toggle_palette(window, cx);
    }

    /// A palette pick: an action is performed, an agent shown in the
    /// focused pane, a folder gets the default command.
    fn palette_event(
        &mut self,
        _palette: &Entity<PickerView>,
        event: &PickerEvent,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let Some(Modal::Palette(_, entries, _)) = &mut self.modal else {
            return;
        };
        let entries = std::mem::take(entries);
        self.close_modal(window, cx);
        let PickerEvent::Pick(key) = event else {
            return;
        };
        let Some(entry) = entries.into_iter().find(|e| e.key() == *key) else {
            return;
        };
        self.palette_recents.touch(key);
        match entry {
            Entry::Command(c) => window.dispatch_action(command_action(c), cx),
            Entry::Agent(id) => self.show_agent(id, window, cx),
            Entry::Changes(id) => self.show_changes(id, window, cx),
            Entry::Folder(dir) => self.spawn_default_in(dir, window, cx),
            Entry::Preset(index) => {
                let presets = self.app.update(cx, |a, _| a.presets());
                if let Some(p) = presets.get(index) {
                    self.open_new_agent_with(Prefill::preset(p), window, cx);
                }
            }
        }
    }

    /// ⌃Tab (⌃⇧Tab, `backward`): opens the agent switcher on the agent
    /// used before the current one (on the least recent), or moves its
    /// selection while open. Not over a sheet or the palette.
    pub fn switch_agent(&mut self, backward: bool, window: &mut Window, cx: &mut Context<Self>) {
        let delta = if backward { -1 } else { 1 };
        if let Some(Modal::Switcher(s)) = &self.modal {
            s.view.update(cx, |v, cx| v.move_selection(delta, cx));
            return;
        }
        if self.has_modal() {
            return;
        }
        let app = self.app.read(cx);
        let current = app.layout().agent(app.focused());
        self.agent_mru.observe(current);
        let items: Vec<_> = switcher_items(&app.agents, &app.home)
            .into_iter()
            .map(|(_, item)| item)
            .collect();
        if items.is_empty() {
            return;
        }
        let recents = self.agent_mru.recents();
        let view = cx.new(|cx| PickerView::list(items, recents, cx));
        view.update(cx, |v, cx| {
            let p = v.picker();
            let current_first = current.is_some_and(|id| {
                p.selected_item()
                    .is_some_and(|i| i.key == Entry::Agent(id).key())
            });
            let row = start_row(p.ranked().len(), current_first, backward);
            v.select(row, cx);
        });
        let sub = cx.subscribe_in(&view, window, Self::switcher_event);
        view.read(cx).focus(window, cx);
        self.modal = Some(Modal::Switcher(Switcher {
            view,
            held: window.modifiers().control,
            _sub: sub,
        }));
        cx.notify();
    }

    /// ⌃ released after a ⌃Tab: the selected agent is shown.
    pub(super) fn switcher_modifiers(
        &mut self,
        ev: &ModifiersChangedEvent,
        _window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let Some(Modal::Switcher(s)) = &mut self.modal else {
            return;
        };
        if ev.modifiers.control {
            s.held = true;
        } else if s.held {
            let view = s.view.clone();
            view.update(cx, |v, cx| v.confirm(cx));
        }
    }

    /// A switcher pick shows the agent in the focused pane.
    fn switcher_event(
        &mut self,
        _view: &Entity<PickerView>,
        event: &PickerEvent,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        if self.switcher().is_none() {
            return;
        }
        self.close_modal(window, cx);
        let PickerEvent::Pick(key) = event else {
            return;
        };
        let id = key
            .strip_prefix("agent:")
            .and_then(|id| id.parse::<u64>().ok());
        if let Some(id) = id {
            self.show_agent(id, window, cx);
        }
    }

    /// A new agent running `[gui] default_command` in `dir`, in the
    /// focused pane.
    pub(super) fn spawn_default_in(
        &mut self,
        dir: String,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let command = self.app.read(cx).config.default_command.clone();
        let result = match command.split_first() {
            Some((program, args)) => {
                let req = SpawnRequest {
                    program: program.clone(),
                    args: args.to_vec(),
                    cwd: dir,
                    labels: Default::default(),
                };
                self.app.update(cx, |a, cx| a.spawn(req, cx))
            }
            None => Err("set [gui] default_command to start an agent from a folder".to_owned()),
        };
        if let Err(e) = result {
            self.app.update(cx, |a, cx| a.show_error(e, cx));
        }
        self.focus_current(window, cx);
    }

    /// A preset (split chooser) started in `dir` in the focused pane, named
    /// as the sheet would name it.
    pub(super) fn spawn_preset_in(&mut self, index: usize, dir: String, cx: &mut Context<Self>) {
        let presets = self.app.update(cx, |a, _| a.presets());
        let Some(preset) = presets.get(index) else {
            return;
        };
        let names = self.app.read(cx).live_names();
        let taken: Vec<&str> = names.iter().map(String::as_str).collect();
        let result = match preset_request(preset, dir, &taken) {
            Some(req) => self.app.update(cx, |a, cx| a.spawn(req, cx)),
            None => Err(format!("preset {:?} has no command", preset.name)),
        };
        if let Err(e) = result {
            self.app.update(cx, |a, cx| a.show_error(e, cx));
        }
    }
}
