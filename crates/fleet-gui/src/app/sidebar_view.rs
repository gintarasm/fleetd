//! The sidebar: vertical tabs, one per agent, grouped by project or
//! directory, and "+ New agent" at the bottom. A click shows the agent in
//! the focused pane; a right-click offers Kill and Open in new split.

use std::collections::HashMap;
use std::path::Path;

use fleet_proto::AgentState;
use gpui::prelude::FluentBuilder;
use gpui::{
    Context, InteractiveElement, IntoElement, MouseButton, ParentElement, SharedString,
    StatefulInteractiveElement, Styled, div, px,
};

use super::theme::Theme;
use super::workspace::Workspace;
use crate::core::session::AgentRow;
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

/// The program's file name (`/bin/sh` shows as `sh`).
pub fn program_name(program: &str) -> String {
    Path::new(program)
        .file_name()
        .map_or_else(|| program.to_owned(), |n| n.to_string_lossy().into_owned())
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
                        .text_color(theme.dim_text)
                        .truncate()
                        .child(SharedString::from(group.title)),
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
    div()
        .flex()
        .flex_col()
        .flex_none()
        .w(px(SIDEBAR_WIDTH))
        .h_full()
        .py(px(8.))
        .text_color(theme.fg())
        .text_size(px(13.))
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
    use super::{program_name, state_mark};
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
    fn marks_and_names() {
        assert_eq!(state_mark(&row(AgentState::Running, None)), "●");
        assert_eq!(state_mark(&row(AgentState::Exited, Some(2))), "○ ✕2");
        assert_eq!(state_mark(&row(AgentState::Lost, None)), "?");
        assert_eq!(program_name("/bin/sh"), "sh");
        assert_eq!(program_name("claude"), "claude");
    }
}
