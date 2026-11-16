//! The sidebar: vertical tabs, one per agent, grouped by project or
//! directory. (New agent and Clear exited live in the unified bar.)
//! A click shows the agent in the focused pane; a right-click opens the
//! row's native menu (`core::menus::row_menu`).

use std::collections::{HashMap, HashSet};
use std::path::Path;

use fleet_proto::AgentState;
use gpui::prelude::FluentBuilder;
use gpui::{
    Action, Context, Div, InteractiveElement, IntoElement, MouseButton, MouseDownEvent,
    ParentElement, SharedString, StatefulInteractiveElement, Styled, div, px,
};

use super::controls;
use super::keymap::{Kill, Remove, Restart};
use super::status_dot::status_dot;
use super::tail_text::tail_text;
use super::theme::{Theme, Typography};
use super::workspace::Workspace;
use crate::core::icons::Icon;
use crate::core::kill::{AgentButtons, agent_buttons};
use crate::core::session::AgentRow;
pub use crate::core::sidebar::program_name;
use crate::core::sidebar::{ended, group_agents, since};
use crate::core::status::AgentStatus;
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
    /// Each non-shell agent's status (ADR 0042).
    pub statuses: &'a HashMap<u64, AgentStatus>,
    /// Agents that rang the bell unseen: their row is tinted.
    pub bells: &'a HashSet<u64>,
    pub reduce_motion: bool,
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
    let empty = data.rows.is_empty().then(|| {
        div()
            .px(px(space::SM))
            .pt(px(space::MD))
            .type_style(text::CALLOUT)
            .text_color(theme.tertiary)
            .child("No agents yet. Press ⌘N to start one.")
    });
    div()
        .flex()
        .flex_col()
        .flex_none()
        .w(px(chrome::SIDEBAR_WIDTH))
        .h_full()
        .text_color(theme.text)
        .type_style(text::BODY)
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
                .children(empty)
                .children(groups),
        )
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
    let status = data.statuses.get(&id).copied();
    let mark_el = match status {
        // A shell keeps its text mark.
        None => div()
            .flex_none()
            .type_style(text::CAPTION)
            .text_color(dot_color)
            .child(SharedString::from(state_mark(row))),
        Some(s) => div()
            .flex()
            .flex_row()
            .flex_none()
            .items_center()
            .gap(px(space::XXS))
            .debug_selector(move || format!("status-{id}-{}", s.label()))
            .child(status_dot(s, theme, data.now_ms, data.reduce_motion))
            .children(
                row.info
                    .exit_code
                    .filter(|c| *c != 0 && s == AgentStatus::Failed)
                    .map(|c| {
                        div()
                            .type_style(text::CAPTION)
                            .text_color(theme.danger)
                            .child(SharedString::from(format!("✕{c}")))
                    }),
            ),
    };
    let rang = data.bells.contains(&id);
    let buttons: Vec<RowButton> = match agent_buttons(true, ended(row)) {
        AgentButtons::None => Vec::new(),
        AgentButtons::Stop => vec![("stop", Icon::Stop, Box::new(Kill { agent_id: id }))],
        AgentButtons::RestartRemove => vec![
            ("restart", Icon::Restart, Box::new(Restart { agent_id: id })),
            ("remove", Icon::Trash, Box::new(Remove { agent_id: id })),
        ],
    };
    let line1 = div()
        .flex()
        .flex_row()
        .items_center()
        .gap(px(space::SM - space::XXS))
        .child(mark_el)
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
                .relative()
                .flex_none()
                .flex()
                .justify_end()
                // Room for the buttons drawn over it.
                .min_w(px(chrome::ICON_BUTTON * buttons.len() as f32))
                .child(
                    div()
                        .type_style(text::CAPTION)
                        .text_color(theme.tertiary)
                        .when(!buttons.is_empty(), |d| {
                            d.group_hover(ROW_GROUP, |s| s.invisible())
                        })
                        .child(SharedString::from(since(
                            row.last_output_unix_ms,
                            data.now_ms,
                        ))),
                )
                .children(row_buttons(buttons, id, theme, cx)),
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
        .group(ROW_GROUP)
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
        .when(rang && mark != TabMark::Focused, |s| {
            s.bg(theme.warning.opacity(0.14))
        })
        .hover(|s| s.bg(theme.hover))
        .active(|s| s.bg(theme.pressed))
        .child(bar)
        .child(line1)
        .child(line2)
        .on_click(cx.listener(move |this, _, window, cx| {
            // A drag that ends on its own row is not a click.
            if this.pane_drag.is_click() {
                this.show_agent(id, window, cx);
            }
        }))
        .on_mouse_down(
            MouseButton::Left,
            cx.listener(move |this, ev: &MouseDownEvent, _, _| {
                this.pane_drag.press_agent(id, ev.position);
            }),
        )
        .on_mouse_down(
            MouseButton::Right,
            cx.listener(move |this, _, _, cx| this.open_row_menu(id, cx)),
        )
}

/// A row's hover group (its agent buttons show on hover).
const ROW_GROUP: &str = "agent-row";

/// A button on a row: its id, icon and the action it dispatches.
type RowButton = (&'static str, Icon, Box<dyn Action>);

/// The row's buttons, over its "since" (hidden meanwhile), invisible and
/// not clickable until the row is hovered.
fn row_buttons(
    buttons: Vec<RowButton>,
    agent_id: u64,
    theme: &Theme,
    cx: &mut Context<Workspace>,
) -> Option<Div> {
    if buttons.is_empty() {
        return None;
    }
    let buttons = buttons.into_iter().map(|(name, icon, action)| {
        controls::icon_button(name, icon, theme, true)
            .debug_selector(move || format!("tab-{agent_id}-{name}"))
            .invisible()
            .group_hover(ROW_GROUP, |s| s.visible())
            .on_click(cx.listener(move |_, _, window, cx| {
                // The row's own click (show the agent) does not run.
                cx.stop_propagation();
                window.dispatch_action(action.boxed_clone(), cx);
            }))
    });
    Some(
        div()
            .absolute()
            .top_0()
            .bottom_0()
            .right_0()
            .flex()
            .flex_row()
            .items_center()
            .children(buttons),
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
