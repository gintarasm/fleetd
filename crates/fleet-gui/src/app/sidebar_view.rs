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
use super::theme::{Theme, Typography};
use super::workspace::Workspace;
use crate::core::session::AgentRow;
pub use crate::core::sidebar::program_name;
use crate::core::sidebar::{group_agents, since};
use crate::core::style::{chrome, radius, space, text};

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
}

/// The Clear exited button's tooltip; `None` while it is disabled.
pub fn clear_exited_tooltip(ended: usize) -> Option<String> {
    match ended {
        0 => None,
        1 => Some("Remove 1 exited agent".to_owned()),
        n => Some(format!("Remove {n} exited agents")),
    }
}

/// A plain text tooltip.
pub(super) struct Tooltip {
    pub text: SharedString,
    pub theme: Theme,
}

impl Render for Tooltip {
    fn render(&mut self, _: &mut Window, _: &mut Context<Self>) -> impl IntoElement {
        div()
            .px(px(space::SM))
            .py(px(space::XS))
            .rounded(px(radius::MD))
            .border_1()
            .border_color(self.theme.border)
            .bg(self.theme.raised)
            .shadow_md()
            .text_color(self.theme.text)
            .type_style(text::CALLOUT)
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
                .gap(px(space::XXS))
                .child(
                    div()
                        .flex()
                        .px(px(space::SM))
                        .pt(px(space::MD))
                        .pb(px(space::XS))
                        .type_style(text::SECTION)
                        .text_color(theme.secondary)
                        .child(tail_text(group.title)),
                )
                .children(tabs)
        })
        .collect();
    let new_agent = div()
        .id("new-agent")
        .flex_none()
        .mx(px(space::SM))
        .px(px(space::SM))
        .py(px(space::XS + space::XXS))
        .rounded(px(radius::MD))
        .text_color(theme.secondary)
        .cursor_pointer()
        .hover(|s| s.bg(theme.hover).text_color(theme.text))
        .active(|s| s.bg(theme.pressed))
        .child("+ New agent")
        .on_click(cx.listener(|this, _, window, cx| this.open_new_agent(window, cx)));
    let tooltip = clear_exited_tooltip(data.ended);
    let enabled = tooltip.is_some();
    let tooltip_theme = *theme;
    let clear = div()
        .id("clear-exited")
        .debug_selector(|| "clear-exited".to_owned())
        .px(px(space::SM))
        .py(px(space::XXS))
        .rounded(px(radius::SM))
        .type_style(text::SUBHEADLINE)
        .text_color(theme.secondary)
        .child("Clear exited")
        .when(!enabled, |s| s.text_color(theme.tertiary))
        .when_some(tooltip, |s, text| {
            let text = SharedString::from(text);
            s.cursor_pointer()
                .hover(|s| s.bg(theme.hover).text_color(theme.text))
                .active(|s| s.bg(theme.pressed))
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
        .px(px(space::SM))
        .child(clear);
    div()
        .flex()
        .flex_col()
        .flex_none()
        .w(px(chrome::SIDEBAR_WIDTH))
        .h_full()
        .py(px(space::SM))
        .text_color(theme.text)
        .type_style(text::BODY)
        .child(header)
        .child(
            div()
                .id("tabs")
                .flex()
                .flex_col()
                .flex_1()
                .min_h_0()
                .px(px(space::SM))
                .pb(px(space::SM))
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
    let running = state == AgentState::Running;
    let dot_color = if running {
        theme.success
    } else {
        theme.tertiary
    };
    let title_color = if running { theme.text } else { theme.secondary };
    let line1 = div()
        .flex()
        .flex_row()
        .items_center()
        .gap(px(space::SM - space::XXS))
        .child(
            div()
                .flex_none()
                .type_style(text::CAPTION)
                .text_color(dot_color)
                .child(SharedString::from(state_mark(row))),
        )
        .child(
            div()
                .flex_1()
                .min_w_0()
                .truncate()
                .text_color(title_color)
                .when(mark == TabMark::Focused, |s| {
                    s.font_weight(gpui::FontWeight::MEDIUM)
                })
                .child(SharedString::from(tab_title(row, data.titles))),
        )
        .child(
            div()
                .flex_none()
                .type_style(text::CAPTION)
                .text_color(theme.tertiary)
                .child(SharedString::from(since(
                    row.last_output_unix_ms,
                    data.now_ms,
                ))),
        );
    let line2 = div()
        .pl(px(space::LG))
        .type_style(text::SUBHEADLINE)
        .text_color(if running {
            theme.secondary
        } else {
            theme.tertiary
        })
        .truncate()
        .child(SharedString::from(one_line(&row.last_line)));
    let bar = div()
        .absolute()
        .left_0()
        .top(px(space::SM))
        .bottom(px(space::SM))
        .w(px(3.))
        .rounded(px(2.))
        .bg(match mark {
            TabMark::Focused => theme.accent,
            TabMark::Shown => theme.tertiary.opacity(0.6),
            TabMark::None => theme.tertiary.opacity(0.),
        });
    div()
        .id(("agent", id))
        .debug_selector(move || format!("tab-{id}"))
        .relative()
        .flex()
        .flex_col()
        .gap(px(space::XXS))
        .px(px(space::SM))
        .py(px(space::XS + space::XXS))
        .rounded(px(radius::MD))
        .cursor_pointer()
        .when(mark == TabMark::Focused, |s| s.bg(theme.selected))
        .hover(|s| s.bg(theme.hover))
        .active(|s| s.bg(theme.pressed))
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
