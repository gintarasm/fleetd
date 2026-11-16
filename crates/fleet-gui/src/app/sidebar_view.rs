//! The sidebar: vertical tabs, one per agent, grouped by project or
//! directory. (New agent and Clear exited live in the unified bar.)
//! A click shows the agent in the focused pane; ⌘-click and ⇧-click pick
//! rows for Send to Selected… (ADR 0083); a right-click opens the row's
//! native menu (`core::menus::row_menu`). Above the tabs, a filter
//! field and status filters; a click on a group's header collapses it
//! (issue #40, ADR 0060), and a right-click offers grouping by folder or
//! by git repository (ADR 0085). A row's second line shows its git
//! checkout and branch when the node reports one.

use std::collections::{HashMap, HashSet};

use fleet_client::names::name_of;
use fleet_proto::AgentState;
use gpui::prelude::FluentBuilder;
use gpui::{
    Action, AppContext, Context, Div, Entity, ExternalPaths, FocusHandle, InteractiveElement,
    IntoElement, MouseButton, MouseDownEvent, ParentElement, SharedString,
    StatefulInteractiveElement, Styled, div, px,
};

use super::agent_mark::{MARK_SLOT, agent_mark};
use super::controls::{self, Tooltip};
use super::keymap::{Kill, Remove, Restart};
use super::status_dot::status_dot;
use super::tail_text::{head_text, tail_text};
use super::text_field::TextField;
use super::theme::{Theme, Typography};
use super::workspace::Workspace;
use crate::core::git;
use crate::core::icons::Icon;
use crate::core::kill::{AgentButtons, agent_buttons};
use crate::core::row_preview::{preview_line, row_meta};
use crate::core::session::AgentRow;
use crate::core::sidebar::ended;
pub use crate::core::sidebar::program_name;
use crate::core::sidebar::{SidebarGroup, StatusFilter, label_chips, program_and_id};
use crate::core::sidebar_tree::{Disclosure, RowLayout};
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
    pub now_ms: i64,
    /// OSC titles of agents shown in panes.
    pub titles: &'a HashMap<u64, String>,
    pub mark: &'a dyn Fn(u64) -> TabMark,
    /// Each non-shell agent's status (ADR 0042).
    pub statuses: &'a HashMap<u64, AgentStatus>,
    /// Agents that rang the bell unseen: their row is tinted.
    pub bells: &'a HashSet<u64>,
    pub reduce_motion: bool,
    /// `[gui] agent_emoji`: each row shows its agent's emoji, not its
    /// colour (ADR 0064).
    pub agent_emoji: bool,
    /// The groups to draw, filtered and collapsed (`core::sidebar::sidebar_groups`).
    pub groups: Vec<SidebarGroup>,
    pub filter_field: Entity<TextField>,
    pub status_filter: StatusFilter,
    /// The tabs' focus: a key typed while it has it goes to the filter.
    pub focus: FocusHandle,
    /// The rows picked with ⌘-click and ⇧-click (ADR 0083).
    pub selected: &'a [u64],
}

/// A tab's title: the agent's name (ADR 0052), else its OSC title if a
/// pane knows it, else the program.
pub fn tab_title(row: &AgentRow, titles: &HashMap<u64, String>) -> String {
    if let Some(name) = name_of(&row.info) {
        return name.to_owned();
    }
    titles
        .get(&row.info.id)
        .filter(|t| !t.trim().is_empty())
        .cloned()
        .unwrap_or_else(|| program_name(&row.info.program))
}

pub fn render_sidebar(
    mut data: SidebarData<'_>,
    theme: &Theme,
    cx: &mut Context<Workspace>,
) -> impl IntoElement + use<> {
    let groups: Vec<_> = std::mem::take(&mut data.groups)
        .into_iter()
        .map(|group| render_group(group, &data, theme, cx))
        .collect();
    let empty = if data.rows.is_empty() {
        Some(("sidebar-empty", "No agents yet. Press ⌘N to start one."))
    } else if groups.is_empty() {
        Some(("sidebar-no-match", "No agents match."))
    } else {
        None
    };
    let empty = empty.map(|(selector, message)| {
        div()
            .debug_selector(move || selector.to_owned())
            .px(px(space::SM))
            .pt(px(space::MD))
            .type_style(text::CALLOUT)
            .text_color(theme.tertiary)
            .child(message)
    });
    div()
        .flex()
        .flex_col()
        .flex_none()
        .w(px(chrome::SIDEBAR_WIDTH))
        .h_full()
        .text_color(theme.text)
        .type_style(text::BODY)
        .child(render_filter(&data, theme, cx))
        .child(
            div()
                .id("tabs")
                .debug_selector(|| "sidebar".to_owned())
                .track_focus(&data.focus)
                .on_mouse_down(
                    MouseButton::Left,
                    cx.listener(|this, _, window, cx| this.focus_sidebar(window, cx)),
                )
                .on_key_down(cx.listener(Workspace::sidebar_key_down))
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

/// The filter field and, under it, the status filters.
fn render_filter(
    data: &SidebarData<'_>,
    theme: &Theme,
    cx: &mut Context<Workspace>,
) -> impl IntoElement + use<> {
    let chips: Vec<_> = StatusFilter::ALL
        .into_iter()
        .map(|f| {
            let on = f == data.status_filter;
            div()
                .id(SharedString::from(format!("status-filter-{}", f.label())))
                .debug_selector(move || format!("status-filter-{}", f.label()))
                .px(px(space::XS + space::XXS))
                .py(px(space::XXS))
                .rounded(px(radius::SM))
                .type_style(text::CAPTION)
                .cursor_pointer()
                .text_color(if on { theme.text } else { theme.secondary })
                .when(on, |d| d.bg(theme.selected))
                .when(!on, |d| d.hover(|d| d.bg(theme.hover)))
                .child(f.label())
                .on_click(cx.listener(move |this, _, _, cx| this.set_status_filter(f, cx)))
        })
        .collect();
    div()
        .flex()
        .flex_col()
        .flex_none()
        .gap(px(space::XS))
        .px(px(space::SM))
        .pt(px(space::SM))
        .child(
            div()
                .debug_selector(|| "sidebar-filter".to_owned())
                .on_key_down(cx.listener(Workspace::filter_key_down))
                .child(data.filter_field.clone()),
        )
        .child(div().flex().flex_row().gap(px(space::XXS)).children(chips))
}

/// A group's header (a click collapses or expands it), then its tabs.
fn render_group(
    group: SidebarGroup,
    data: &SidebarData<'_>,
    theme: &Theme,
    cx: &mut Context<Workspace>,
) -> impl IntoElement + use<> {
    let tabs: Vec<_> = group
        .agents
        .iter()
        .zip(
            group
                .layout
                .iter()
                .copied()
                .chain(std::iter::repeat(RowLayout::FLAT)),
        )
        .map(|(row, layout)| render_tab(row, layout, data, theme, cx))
        .collect();
    let title = group.title.clone();
    let selector = format!("group-{}", group.title);
    let count = group.collapsed.then(|| {
        let selector = format!("group-count-{}", group.title);
        let exact = format!("{selector}-{}", group.count);
        div()
            .flex_none()
            .debug_selector(move || selector.clone())
            .child(
                div()
                    .debug_selector(move || exact.clone())
                    .type_style(text::CAPTION)
                    .text_color(theme.tertiary)
                    .child(SharedString::from(group.count.to_string())),
            )
    });
    let header = div()
        .id(SharedString::from(selector.clone()))
        .debug_selector(move || selector.clone())
        .flex()
        .flex_row()
        .items_center()
        .gap(px(space::XS))
        .px(px(space::SM))
        .pt(px(space::MD))
        .pb(px(space::XS))
        .cursor_pointer()
        .type_style(text::SECTION)
        .text_color(theme.secondary)
        .child(
            div()
                .flex_none()
                .w(px(space::SM + space::XXS))
                .text_color(theme.tertiary)
                .child(if group.collapsed { "▸" } else { "▾" }),
        )
        .child(div().flex_1().min_w_0().child(tail_text(group.title)))
        .children(count)
        .on_click(cx.listener(move |this, _, _, cx| this.toggle_group(&title, cx)))
        // Group by folder or repository (ADR 0085).
        .on_mouse_down(
            MouseButton::Right,
            cx.listener(|this, _, _, cx| this.open_group_menu(cx)),
        );
    div()
        .flex()
        .flex_col()
        .gap(px(space::XXS))
        .child(header)
        .children(tabs)
}

fn render_tab(
    row: &AgentRow,
    layout: RowLayout,
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
        // The exit code is on the right (`meta`).
        Some(s) => div()
            .flex()
            .flex_row()
            .flex_none()
            .items_center()
            .debug_selector(move || format!("status-{id}-{}", s.label()))
            .child(status_dot(s, theme, data.now_ms, data.reduce_motion)),
    };
    let meta = row_meta(row, data.now_ms);
    let preview = preview_line(&row.last_line);
    let rang = data.bells.contains(&id);
    let picked = data.selected.contains(&id);
    let buttons: Vec<RowButton> = match agent_buttons(true, ended(row)) {
        AgentButtons::None => Vec::new(),
        AgentButtons::Stop => vec![("stop", Icon::Stop, Box::new(Kill { agent_id: id }))],
        AgentButtons::RestartRemove => vec![
            ("restart", Icon::Restart, Box::new(Restart { agent_id: id })),
            ("remove", Icon::Trash, Box::new(Remove { agent_id: id })),
        ],
    };
    let children = layout.children;
    let disclosure = (layout.disclosure != Disclosure::Leaf).then(|| {
        let open = layout.disclosure == Disclosure::Open;
        div()
            .id(("disclose", id))
            .debug_selector(move || format!("disclose-{id}"))
            .flex_none()
            .cursor_pointer()
            .type_style(text::CAPTION)
            .text_color(theme.tertiary)
            .child(SharedString::from(if open {
                "▾".to_owned()
            } else {
                format!("▸ {children}")
            }))
            .on_click(cx.listener(move |this, _, _, cx| {
                this.toggle_disclosure(id, children, cx);
                // Not also the row's click, which would focus a pane.
                cx.stop_propagation();
            }))
    });
    let line1 = div()
        .flex()
        .flex_row()
        .items_center()
        .gap(px(space::SM - space::XXS))
        .children(disclosure)
        .child(agent_mark(id, theme, data.agent_emoji))
        .child(mark_el)
        .child(
            div()
                .id(("agent-name", id))
                .debug_selector(move || format!("tab-{id}-name"))
                .flex_1()
                .min_w_0()
                .truncate()
                .text_color(title_color)
                .when(mark == TabMark::Focused, |s| {
                    s.font_weight(gpui::FontWeight::MEDIUM)
                })
                .child(SharedString::from(tab_title(row, data.titles)))
                // A double-click renames; the row's click still shows it.
                .on_click(cx.listener(move |this, ev: &gpui::ClickEvent, window, cx| {
                    if ev.click_count() == 2 {
                        this.open_rename(id, window, cx);
                        // Not also the row's click, which would focus a pane.
                        cx.stop_propagation();
                    }
                })),
        )
        .child({
            let shown = meta.text.clone();
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
                        .text_color(if meta.failed {
                            theme.danger
                        } else {
                            theme.tertiary
                        })
                        .when(!buttons.is_empty(), |d| {
                            d.group_hover(ROW_GROUP, |s| s.invisible())
                        })
                        .debug_selector(move || format!("meta-{id}-{shown}"))
                        .child(SharedString::from(meta.text)),
                )
                .children(row_buttons(buttons, id, theme, cx))
        });
    // The git checkout and branch (ADR 0085), else `program · id`.
    let git = git::row_subtitle(&row.info);
    let line2 = div()
        .flex()
        .flex_row()
        .gap(px(space::XS))
        // Under the title: past the agent's mark and the status.
        .pl(px(ROW_INDENT))
        .type_style(text::SUBHEADLINE)
        .child(
            div()
                .debug_selector(move || format!("tab-{id}-detail"))
                .text_color(theme.tertiary)
                .map(|d| match git {
                    // Long branch names give way to the last line, up to
                    // a point.
                    Some(git) => {
                        let shown = git.clone();
                        d.flex_shrink()
                            .min_w_0()
                            .max_w(px(chrome::SIDEBAR_WIDTH * 0.6))
                            .child(
                                div()
                                    .debug_selector(move || format!("tab-{id}-git-{shown}"))
                                    .truncate()
                                    .child(SharedString::from(git)),
                            )
                    }
                    None => d
                        .flex_none()
                        .child(SharedString::from(program_and_id(&row.info))),
                }),
        )
        .child(
            div()
                .flex_1()
                .min_w_0()
                .text_color(if running {
                    theme.secondary
                } else {
                    theme.tertiary
                })
                .debug_selector({
                    let shown = preview.clone();
                    move || format!("preview-{id}-{shown}")
                })
                .child(head_text(preview)),
        );
    let chips = label_chips(&row.info);
    let line3 = (!chips.is_empty()).then(|| {
        div()
            .flex()
            .flex_row()
            .flex_wrap()
            .gap(px(space::XXS))
            .pl(px(ROW_INDENT))
            .children(chips.into_iter().map(|(k, v)| chip(id, k, v, theme, cx)))
    });
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
        // Children sit under their parent (ADR 0074).
        .ml(px(TREE_INDENT * layout.depth as f32))
        // An ancestor kept only for the filter's match.
        .when(layout.context, |s| s.opacity(0.55))
        .when(mark == TabMark::Focused, |s| s.bg(theme.selected))
        .when(rang && mark != TabMark::Focused, |s| {
            s.bg(theme.warning.opacity(0.14))
        })
        // Picked for Send to Selected…: an accent tint over the rest.
        .when(picked, |s| s.bg(theme.accent.opacity(0.22)))
        .hover(|s| s.bg(theme.hover))
        .active(|s| s.bg(theme.pressed))
        .child(bar)
        .child(line1)
        .child(line2)
        .children(line3)
        .on_click(cx.listener(move |this, ev: &gpui::ClickEvent, window, cx| {
            // A drag that ends on its own row is not a click.
            if this.pane_drag.is_click() {
                let m = ev.modifiers();
                this.row_click(id, m.platform, m.shift, window, cx);
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
        // Files from Finder: tinted while over the row (stronger than a
        // picked row's tint), typed into its agent when dropped (ADR 0081).
        .drag_over::<ExternalPaths>({
            let tint = theme.accent.opacity(0.34);
            move |s, _, _, _| s.bg(tint)
        })
        .on_drop(cx.listener(move |this, paths: &ExternalPaths, _, cx| {
            this.drop_paths_on_agent(id, paths, cx);
        }))
}

/// A label chip: the value, with `key=value` as its tooltip. A click
/// filters the sidebar by it (ADR 0087).
fn chip(
    id: u64,
    key: String,
    value: String,
    theme: &Theme,
    cx: &mut Context<Workspace>,
) -> impl IntoElement + use<> {
    let tip = SharedString::from(format!("{key}={value}"));
    let tip_theme = *theme;
    let selector = format!("tab-{id}-chip-{key}");
    div()
        .id(SharedString::from(selector.clone()))
        .debug_selector(move || selector.clone())
        .px(px(space::XS))
        .rounded(px(radius::SM))
        .bg(theme.hover)
        .type_style(text::CAPTION)
        .text_color(theme.secondary)
        .max_w(px(chrome::SIDEBAR_WIDTH / 2.0))
        .truncate()
        .cursor_pointer()
        .hover(move |s| s.bg(tip_theme.pressed).text_color(tip_theme.text))
        .child(SharedString::from(value.clone()))
        .on_click(cx.listener(move |this, _, _, cx| {
            this.filter_by_label(&key, &value, cx);
            // Not also the row's click, which would show the agent.
            cx.stop_propagation();
        }))
        .tooltip(move |_, cx| {
            cx.new(|_| Tooltip {
                text: tip.clone(),
                theme: tip_theme,
            })
            .into()
        })
}

/// Where a row's second and third lines start: under the title, past
/// the agent's mark and the status.
const ROW_INDENT: f32 = MARK_SLOT + space::SM - space::XXS + space::LG;

/// How far each level of the tree moves a row to the right.
const TREE_INDENT: f32 = space::MD;

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
