//! The sidebar: vertical tabs, one per agent, grouped by project or
//! directory, "Clear exited" above them and "+ New agent" at the bottom.
//! A click shows the agent in the focused pane; a right-click offers Open
//! in new split, and Kill (running) or Restart and Remove (ended).

use std::collections::HashMap;
use std::path::Path;

use fleet_proto::AgentState;
use gpui::prelude::FluentBuilder;
use gpui::{
    AppContext, Context, InteractiveElement, IntoElement, MouseButton, ParentElement, Render,
    SharedString, StatefulInteractiveElement, Styled, Window, div, px,
};

use super::tail_text::tail_text;
use super::theme::Theme;
use super::workspace::Workspace;
use crate::core::session::AgentRow;
pub use crate::core::sidebar::program_name;
use crate::core::sidebar::{group_agents, since};

/// Width of the sidebar.
pub const SIDEBAR_WIDTH: f32 = 240.0;

/// The state mark of a row: ● running, ○ exited `✕code`, `?` lost.
pub fn state_mark(row: &AgentRow) -> String {
    match row.info.state() {
        AgentState::Running => "●".to_owned(),
        AgentState::Exited => match row.info.exit_code {
            Some(code) => format!("○ ✕{code}"),
            None => "○".to_owned(),
        },
        AgentState::Lost | AgentState::Unspecified => "?".to_owned(),
    }
}

/// How a tab relates to the panes.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum TabMark {
    /// Shown in the focused pane: highlighted, with the accent bar.
    Focused,
    /// Shown in another pane: a subtle marker.
    Shown,
    None,
}

/// What the sidebar draws, gathered from the app state.
pub struct SidebarData<'a> {
    pub rows: &'a [AgentRow],
    pub home: &'a Path,
    pub now_ms: i64,
    /// OSC titles of agents shown in panes.
    pub titles: &'a HashMap<u64, String>,
    pub mark: &'a dyn Fn(u64) -> TabMark,
    /// Exited or lost agents (what Clear exited removes).
    pub ended: usize,
    /// A newer fleetd is on disk: the footer offers to restart into it.
    pub update_available: bool,
}

/// The Clear exited button's tooltip; `None` while it is disabled.
pub fn clear_exited_tooltip(ended: usize) -> Option<String> {
    match ended {
        0 => None,
        1 => Some("Remove 1 exited agent".to_owned()),
        n => Some(format!("Remove {n} exited agents")),
    }
}

/// The footer notice while a newer fleetd is on disk.
pub const UPDATE_NOTICE: &str = "fleetd update available";

/// A plain text tooltip.
struct Tooltip {
    text: SharedString,
    theme: Theme,
}

impl Render for Tooltip {
    fn render(&mut self, _: &mut Window, _: &mut Context<Self>) -> impl IntoElement {
        div()
            .px(px(8.))
            .py(px(4.))
            .rounded(px(4.))
            .border_1()
            .border_color(self.theme.dim_text)
            .bg(self.theme.header_bg)
            .text_color(self.theme.fg())
            .text_size(px(12.))
            .child(self.text.clone())
    }
}

/// A tab's title: the agent's OSC title if a pane knows it, else the
/// program.
pub fn tab_title(row: &AgentRow, titles: &HashMap<u64, String>) -> String {
    titles
        .get(&row.info.id)
        .filter(|t| !t.trim().is_empty())
        .cloned()
        .unwrap_or_else(|| program_name(&row.info.program))
}

pub fn render_sidebar(
    data: &SidebarData<'_>,
    theme: &Theme,
    cx: &mut Context<Workspace>,
) -> impl IntoElement + use<> {
    let groups: Vec<_> = group_agents(data.rows, data.home)
        .into_iter()
        .map(|group| {
            let tabs: Vec<_> = group
                .agents
                .into_iter()
                .map(|row| render_tab(&row, data, theme, cx))
                .collect();
            div()
                .flex()
                .flex_col()
                .gap(px(2.))
                .pt(px(10.))
                .child(
                    div()
                        .px(px(10.))
                        .pb(px(2.))
                        .text_size(px(11.))
                        .flex()
                        .text_color(theme.dim_text)
                        .child(tail_text(group.title)),
                )
                .children(tabs)
        })
        .collect();
    let new_agent = div()
        .id("new-agent")
        .flex_none()
        .mx(px(4.))
        .px(px(10.))
        .py(px(6.))
        .rounded(px(6.))
        .text_color(theme.dim_text)
        .cursor_pointer()
        .hover(|s| s.bg(theme.header_bg).text_color(theme.fg()))
        .child("+ New agent")
        .on_click(cx.listener(|this, _, window, cx| this.open_new_agent(window, cx)));
    // Agents keep running through the restart, so the notice is a
    // suggestion, not a warning.
    let update = data.update_available.then(|| {
        div()
            .id("fleetd-update")
            .debug_selector(|| "fleetd-update".to_owned())
            .flex()
            .flex_row()
            .flex_none()
            .items_center()
            .justify_between()
            .mx(px(4.))
            .px(px(10.))
            .py(px(4.))
            .text_size(px(11.))
            .text_color(theme.dim_text)
            .child(UPDATE_NOTICE)
            .child(
                div()
                    .id("fleetd-update-restart")
                    .debug_selector(|| "fleetd-update-restart".to_owned())
                    .px(px(6.))
                    .py(px(2.))
                    .rounded(px(4.))
                    .text_color(theme.fg())
                    .cursor_pointer()
                    .hover(|s| s.bg(theme.header_bg))
                    .child("Restart")
                    .on_click(cx.listener(|this, _, _, cx| {
                        this.app().read(cx).restart_daemon();
                    })),
            )
    });
    let tooltip = clear_exited_tooltip(data.ended);
    let enabled = tooltip.is_some();
    let tooltip_theme = *theme;
    let clear = div()
        .id("clear-exited")
        .debug_selector(|| "clear-exited".to_owned())
        .px(px(6.))
        .py(px(2.))
        .rounded(px(4.))
        .text_size(px(11.))
        .text_color(theme.dim_text)
        .child("Clear exited")
        .when(!enabled, |s| s.opacity(0.4))
        .when_some(tooltip, |s, text| {
            let text = SharedString::from(text);
            s.cursor_pointer()
                .hover(|s| s.bg(theme.header_bg).text_color(theme.fg()))
                .tooltip(move |_, cx| {
                    let (text, theme) = (text.clone(), tooltip_theme);
                    cx.new(|_| Tooltip { text, theme }).into()
                })
                .on_click(cx.listener(|this, _, _, cx| this.clear_exited(cx)))
        });
    let header = div()
        .flex()
        .flex_row()
        .flex_none()
        .justify_end()
        .px(px(8.))
        .child(clear);
    div()
        .flex()
        .flex_col()
        .flex_none()
        .w(px(SIDEBAR_WIDTH))
        .h_full()
        .py(px(8.))
        .text_color(theme.fg())
        .text_size(px(13.))
        .child(header)
        .child(
            div()
                .id("tabs")
                .flex()
                .flex_col()
                .flex_1()
                .min_h_0()
                .px(px(4.))
                .overflow_y_scroll()
                .children(groups),
        )
        .children(update)
        .child(new_agent)
}

fn render_tab(
    row: &AgentRow,
    data: &SidebarData<'_>,
    theme: &Theme,
    cx: &mut Context<Workspace>,
) -> impl IntoElement + use<> {
    let id = row.info.id;
    let state = row.info.state();
    let mark = (data.mark)(id);
    let dot_color = match state {
        AgentState::Running => theme.resolve(fleet_term::Color::Named(2), true),
        _ => theme.dim_text,
    };
    let line1 = div()
        .flex()
        .flex_row()
        .items_center()
        .gap(px(6.))
        .child(
            div()
                .flex_none()
                .text_size(px(10.))
                .text_color(dot_color)
                .child(SharedString::from(state_mark(row))),
        )
        .child(
            div()
                .flex_1()
                .min_w_0()
                .truncate()
                .child(SharedString::from(tab_title(row, data.titles))),
        )
        .child(
            div()
                .flex_none()
                .text_size(px(11.))
                .text_color(theme.dim_text)
                .child(SharedString::from(since(
                    row.last_output_unix_ms,
                    data.now_ms,
                ))),
        );
    let line2 = div()
        .pl(px(16.))
        .text_size(px(11.))
        .text_color(theme.dim_text)
        .truncate()
        .child(SharedString::from(one_line(&row.last_line)));
    let bar = div()
        .absolute()
        .left_0()
        .top(px(6.))
        .bottom(px(6.))
        .w(px(3.))
        .rounded(px(2.))
        .bg(match mark {
            TabMark::Focused => theme.accent,
            TabMark::Shown => theme.dim_text.opacity(0.5),
            TabMark::None => theme.dim_text.opacity(0.),
        });
    div()
        .id(("agent", id))
        .debug_selector(move || format!("tab-{id}"))
        .relative()
        .px(px(10.))
        .py(px(5.))
        .rounded(px(6.))
        .cursor_pointer()
        .when(mark == TabMark::Focused, |s| s.bg(theme.header_bg))
        .hover(|s| s.bg(theme.header_bg))
        .when(state != AgentState::Running, |s| s.opacity(0.5))
        .child(bar)
        .child(line1)
        .child(line2)
        .on_click(cx.listener(move |this, _, window, cx| this.show_agent(id, window, cx)))
        .on_mouse_down(
            MouseButton::Right,
            cx.listener(move |this, ev: &gpui::MouseDownEvent, _, cx| {
                this.open_row_menu(id, ev.position, cx);
            }),
        )
}

/// The last line, on one line.
fn one_line(s: &str) -> String {
    s.lines().next_back().unwrap_or("").trim().to_owned()
}

#[cfg(test)]
mod tests {
    use super::{clear_exited_tooltip, program_name, state_mark};
    use crate::core::session::AgentRow;
    use fleet_proto::{AgentInfo, AgentState};

    fn row(state: AgentState, code: Option<i32>) -> AgentRow {
        let mut info = AgentInfo {
            exit_code: code,
            ..Default::default()
        };
        info.set_state(state);
        AgentRow {
            info,
            last_line: String::new(),
            last_output_unix_ms: None,
        }
    }

    #[test]
    fn clear_exited_tooltip_counts_ended_agents() {
        assert_eq!(clear_exited_tooltip(0), None);
        assert_eq!(
            clear_exited_tooltip(1).as_deref(),
            Some("Remove 1 exited agent")
        );
        assert_eq!(
            clear_exited_tooltip(3).as_deref(),
            Some("Remove 3 exited agents")
        );
    }

    #[test]
    fn marks_and_names() {
        assert_eq!(state_mark(&row(AgentState::Running, None)), "●");
        assert_eq!(state_mark(&row(AgentState::Exited, Some(2))), "○ ✕2");
        assert_eq!(state_mark(&row(AgentState::Lost, None)), "?");
        assert_eq!(program_name("/bin/sh"), "sh");
        assert_eq!(program_name("claude"), "claude");
    }
}
