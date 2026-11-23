//! Drawing the workspace: the panes, exit toasts, the cheat sheet and
//! the window itself.

use std::time::{Duration, Instant};

use gpui::prelude::FluentBuilder as _;
use gpui::{
    Animation, AnimationExt, App, Context, Div, FocusHandle, InteractiveElement, IntoElement,
    KeyDownEvent, MouseButton, MouseDownEvent, ParentElement, Render, SharedString,
    StatefulInteractiveElement, Styled, Window, deferred, div, px, relative,
};

use super::{Modal, Workspace, now_ms};
use crate::app::action_sheet::{self};
use crate::app::controls;
use crate::app::controls::{ButtonKind, appear, button};
use crate::app::keymap::{
    CopyLastOutput, FocusDown, FocusLeft, FocusPane1, FocusPane2, FocusPane3, FocusPane4,
    FocusRight, FocusUp, NextAgent, NextNeedsYou, OpenOutput, PreviousAgent, SaveOutput,
    ShowShortcuts, SplitDown, SplitRight, ToggleGraph, WORKSPACE_CONTEXT, ZoomIn, ZoomOut,
    ZoomReset,
};
use crate::app::kill_sheet::{self};
use crate::app::peek_view::{self};
use crate::app::send_sheet::{self};
use crate::app::sidebar_view::{SidebarData, TabMark, render_pending, render_sidebar};
use crate::app::theme::{Theme, Typography};
use crate::app::titlebar_view::{BarData, render_bar, sheet_drag_strip};
use crate::core::feedback::{self, Tween};
use crate::core::layout::{Axis, Dir, MAX_PANES, Rect};
use crate::core::menus::PaneMenuItem;
use crate::core::session::PaneId;
use crate::core::shortcuts::Section;
use crate::core::sidebar::ended;
use crate::core::sidebar_tree::removable_ended;
use crate::core::style::{chrome, motion, radius, space, text};
use crate::core::titlebar::{Link, bar_title, window_title};
use crate::core::zoom::Zoom;

impl Workspace {
    /// The pane cards, [`chrome::PANE_GAP`] apart and from the edges;
    /// only the maximized one when a pane is maximized (the others stay
    /// attached); then the split dividers' strips over the gaps.
    fn render_panes(&mut self, window: &mut Window, cx: &App) -> impl IntoElement + use<> {
        let app = self.app.read(cx);
        let rects = match app.maximized {
            Some(pane) => vec![(
                pane,
                Rect {
                    x: 0.0,
                    y: 0.0,
                    w: 1.0,
                    h: 1.0,
                },
            )],
            None => app.layout().rects(1.0, 1.0),
        };
        let drawn: Vec<PaneId> = rects.iter().map(|(p, _)| *p).collect();
        // New panes (opened, split, or shown again on restore) ease in.
        let now = Instant::now();
        if !self.reduce_motion {
            for pane in feedback::entering(&self.drawn, &drawn) {
                self.entering.insert(pane, now);
            }
        }
        self.drawn = drawn;
        self.entering.retain(|pane, at| {
            !self.reduce_motion
                && self.drawn.contains(pane)
                && Tween::new(*at, feedback::PANE_IN_MS).running(now)
        });
        if !self.entering.is_empty() {
            window.request_animation_frame();
        }
        let entering = &self.entering;
        let dividers = match app.maximized {
            Some(_) => Vec::new(),
            None => self.split_drag.render(app.layout(), &self.app),
        };
        let dragging = self.pane_drag.render(
            rects.clone(),
            app.layout().panes().len() < MAX_PANES,
            &self.app,
            &app.theme,
        );
        let children: Vec<_> = rects
            .into_iter()
            .filter_map(|(pane, r)| {
                let view: gpui::AnyView = match app.diff_pane(pane) {
                    Some(diff) => diff.clone().into(),
                    None => app.pane_view(pane)?.clone().into(),
                };
                // 1 when settled; grows from an inset while fading in.
                let t = entering
                    .get(&pane)
                    .map_or(1.0, |at| Tween::new(*at, feedback::PANE_IN_MS).value(now));
                let drag = self.pane_drag.clone();
                Some(
                    div()
                        .debug_selector(move || format!("pane-card-{}", pane.0))
                        .on_mouse_down(MouseButton::Left, move |ev: &MouseDownEvent, _, _| {
                            drag.press_card(pane, ev.position);
                        })
                        .absolute()
                        .left(relative(r.x))
                        .top(relative(r.y))
                        .w(relative(r.w))
                        .h(relative(r.h))
                        .p(px(
                            chrome::PANE_GAP / 2.0 + feedback::PANE_IN_INSET * (1.0 - t)
                        ))
                        .opacity(t)
                        .child(view),
                )
            })
            .collect();
        div().relative().flex_1().h_full().min_w_0().child(
            div()
                .absolute()
                .inset(px(chrome::PANE_GAP / 2.0))
                .children(children)
                .children(dividers)
                .children(dragging),
        )
    }

    /// The exit toasts (#44), bottom centre, newest at the bottom: what
    /// exited with its code, Restart and ✕. A success being celebrated
    /// gets a check and, with motion, a small burst.
    fn render_exit_toasts(&self, theme: &Theme, cx: &mut Context<Self>) -> Option<Div> {
        let app = self.app.read(cx);
        let notes: Vec<_> = app.exit_toasts().cloned().collect();
        if notes.is_empty() {
            return None;
        }
        let celebrating = app.celebrating;
        let reduce_motion = self.reduce_motion;
        let theme = *theme;
        let mut column = div().flex().flex_col().items_center().gap(px(space::SM));
        for note in notes {
            let id = note.agent_id;
            let party = celebrating
                .is_some_and(|(a, at)| a == id && at.elapsed() < feedback::EXIT_TOAST_TTL);
            let restart = button(
                ("exit-restart", id),
                "Restart",
                &theme,
                ButtonKind::Secondary,
                true,
            )
            .debug_selector(move || format!("exit-toast-{id}-restart"))
            .on_click(cx.listener(move |this, _, _, cx| {
                this.app.update(cx, |a, cx| a.restart_from_toast(id, cx));
            }));
            let close = div()
                .id(("exit-close", id))
                .debug_selector(move || format!("exit-toast-{id}-close"))
                .px(px(space::XS))
                .text_color(theme.secondary)
                .cursor_pointer()
                .hover(move |s| s.text_color(theme.text))
                .child("✕")
                .on_click(cx.listener(move |this, _, _, cx| {
                    this.app.update(cx, |a, cx| a.dismiss_exit(id, cx));
                }));
            let card = div()
                .id(("exit-toast", id))
                .debug_selector(move || format!("exit-toast-{id}"))
                .flex()
                .flex_row()
                .items_center()
                .gap(px(space::SM))
                .pl(px(space::MD))
                .pr(px(space::XS))
                .py(px(space::XS))
                .rounded(px(radius::MD))
                .border_1()
                .border_color(theme.border)
                .bg(theme.raised)
                .shadow_lg()
                .text_color(theme.text)
                .type_style(text::CALLOUT)
                .occlude()
                .children(party.then(|| celebration(id, &theme, reduce_motion)))
                .child(SharedString::from(note.text()))
                .child(restart)
                .child(close);
            column = column.child(appear(
                div().child(card),
                ("exit-toast-appear", id),
                motion::SHEET_OFFSET,
                motion::TOAST_MS,
                reduce_motion,
            ));
        }
        Some(
            div()
                .absolute()
                .bottom(px(space::LG))
                .left_0()
                .right_0()
                .flex()
                .justify_center()
                .child(column),
        )
    }
}

/// A check in the success colour; with motion, six dots burst out of it
/// and fade once ([`feedback::CELEBRATE_MS`]), then nothing moves.
fn celebration(agent_id: u64, theme: &Theme, reduce_motion: bool) -> gpui::AnyElement {
    const SIZE: f32 = 16.0;
    const DOTS: usize = 6;
    const DOT: f32 = 4.0;
    let check = div()
        .debug_selector(move || format!("exit-toast-{agent_id}-celebration"))
        .relative()
        .size(px(SIZE))
        .flex()
        .items_center()
        .justify_center()
        .text_color(theme.success)
        .font_weight(gpui::FontWeight::BOLD)
        .child("✓");
    if reduce_motion {
        return check.into_any_element();
    }
    let colours = [
        theme.success,
        theme.accent,
        theme.success,
        theme.accent,
        theme.success,
        theme.accent,
    ];
    let mut burst = check;
    for (i, colour) in colours.into_iter().enumerate().take(DOTS) {
        let angle = i as f32 * std::f32::consts::TAU / DOTS as f32;
        let dot = div()
            .absolute()
            .size(px(DOT))
            .rounded_full()
            .bg(colour)
            .with_animation(
                ("celebrate", agent_id * DOTS as u64 + i as u64),
                Animation::new(Duration::from_millis(feedback::CELEBRATE_MS))
                    .with_easing(motion::ease_out),
                move |dot, t| {
                    let r = 2.0 + 12.0 * t;
                    let c = SIZE / 2.0 - DOT / 2.0;
                    dot.left(px(c + r * angle.cos()))
                        .top(px(c + r * angle.sin()))
                        .opacity(1.0 - t)
                },
            );
        burst = burst.child(dot);
    }
    burst.into_any_element()
}

impl Render for Workspace {
    fn render(&mut self, window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        crate::app::bench::frame_started();
        self.follow_pane_focus(window, cx);
        self.follow_choosers(window, cx);
        {
            let app = self.app.read(cx);
            self.agent_mru.observe(app.layout().agent(app.focused()));
        }
        let theme = self.app.read(cx).theme;
        let agent_emoji = self.app.read(cx).config.agent_emoji;
        let (rows, error, notice, titles, marks, update, statuses, bells) = {
            let app = self.app.read(cx);
            (
                app.agents.clone(),
                app.error.clone(),
                app.notice.clone(),
                app.titles.clone(),
                app.tab_marks(),
                app.update_button(),
                app.statuses.clone(),
                app.bells.clone(),
            )
        };
        let (pending, fresh, next) = {
            let app = self.app.read(cx);
            let now = Instant::now();
            (
                app.pending_spawns.clone(),
                app.new_children.strengths(now, self.reduce_motion),
                app.new_children.next_change(now, self.reduce_motion),
            )
        };
        self.new_child_timer.schedule(next, window, cx);
        let mark = move |id: u64| marks.get(&id).copied().unwrap_or(TabMark::None);
        // Rows that went away leave the selection.
        self.row_selection
            .retain(|id| rows.iter().any(|r| r.info.id == id));
        let groups = self.sidebar_groups(cx);
        #[cfg(test)]
        {
            self.drawn_sidebar = groups
                .iter()
                .map(|g| {
                    (
                        g.title.clone(),
                        g.agents.iter().map(|r| r.info.id).collect(),
                    )
                })
                .collect();
        }
        let data = SidebarData {
            rows: &rows,
            now_ms: now_ms(),
            titles: &titles,
            mark: &mark,
            statuses: &statuses,
            bells: &bells,
            reduce_motion: self.reduce_motion,
            agent_emoji,
            groups,
            filter_field: self.filter_field.clone(),
            status_filter: self.status_filter,
            focus: self.sidebar_focus.clone(),
            selected: self.row_selection.ids(),
            pending: &pending,
            fresh: &fresh,
        };
        let sidebar = render_sidebar(data, &theme, cx);
        // The sidebar is hidden under the graph: its spawn requests float
        // over the graph instead (ADR 0079).
        let graph_pending = if self.graph_shown {
            render_pending(&pending, &rows, &titles, &theme, cx).map(|block| {
                div()
                    .id("graph-pending-spawns")
                    .absolute()
                    .top(px(chrome::BAR_HEIGHT + space::SM))
                    .right(px(space::LG))
                    .w(px(chrome::SIDEBAR_WIDTH))
                    .max_h(px(480.))
                    .overflow_y_scroll()
                    .pb(px(space::SM))
                    .rounded(px(radius::LG))
                    .border_1()
                    .border_color(theme.border)
                    .bg(theme.raised)
                    .shadow_lg()
                    .occlude()
                    .child(block)
            })
        } else {
            None
        };
        let ended_rows = rows.iter().filter(|r| ended(r)).count();
        // What Clear exited removes (an ended parent of a running agent stays).
        let removable_rows = removable_ended(&rows);
        let bar = {
            let app = self.app.read(cx);
            let link = if !app.is_live() {
                Link::Connecting
            } else if app.is_reconnecting() {
                Link::Reconnecting
            } else {
                Link::Live
            };
            let title = bar_title(app.target(), link, rows.len() - ended_rows, ended_rows);
            let window_title = window_title(&title);
            if self.window_title != window_title {
                window.set_window_title(&window_title);
                self.window_title = window_title;
            }
            BarData {
                title,
                ended: removable_rows.len(),
                fullscreen: window.is_fullscreen(),
                update,
                tabs: self.tab_labels(cx),
                active_tab: app.tabs().active(),
                graph: self.graph_shown,
            }
        };
        let bar = render_bar(&bar, &theme, cx);
        crate::app::bench::sidebar_drawn(window);
        let panes = self.render_panes(window, cx);
        let exits = self.render_exit_toasts(&theme, cx);
        // Under glass gpui leaves the tint to the glass (ADR 0049).
        let tint = self.backdrop.tint();
        // Without glass the root's one fill covers all and nothing is
        // wrapped (the default frame is unchanged).
        let (bar, sidebar, panes) = if tint.everywhere() {
            (
                bar.into_any_element(),
                sidebar.into_any_element(),
                panes.into_any_element(),
            )
        } else {
            let fill = |el: gpui::Div, on: bool| if on { el.bg(theme.window) } else { el };
            (
                fill(div().flex_none().child(bar), tint.chrome).into_any_element(),
                fill(div().flex_none().h_full().child(sidebar), tint.chrome).into_any_element(),
                fill(
                    div().flex_1().min_w_0().h_full().flex().child(panes),
                    tint.content,
                )
                .into_any_element(),
            )
        };
        let reduce_motion = self.reduce_motion;
        // An error wins over a neutral notice; the notice has no red border.
        let is_error = error.is_some();
        let toast = error.or(notice).map(|e| {
            let card = div()
                .id("error")
                .debug_selector(|| "error-toast".to_owned())
                .max_w(px(480.))
                .px(px(space::MD))
                .py(px(space::SM))
                .rounded(px(radius::MD))
                .border_1()
                .border_color(if is_error {
                    theme.danger.opacity(0.6)
                } else {
                    theme.border
                })
                .bg(theme.raised)
                .shadow_lg()
                .text_color(theme.text)
                .type_style(text::CALLOUT)
                .occlude()
                .cursor_pointer()
                .child(SharedString::from(format!("{e}   ✕")))
                .on_click(cx.listener(|this, _, _, cx| {
                    this.app.update(cx, |a, cx| a.dismiss_error(cx));
                }));
            div()
                .absolute()
                .bottom(px(space::LG))
                .right(px(space::LG))
                .child(appear(
                    div().child(card),
                    "toast-appear",
                    motion::SHEET_OFFSET,
                    motion::TOAST_MS,
                    reduce_motion,
                ))
        });
        let connect = self
            .connect_sheet()
            .filter(|_| self.connect_sheet_shown(cx))
            .map(|sheet| {
                deferred(
                    div()
                        .id("connect-backdrop")
                        .absolute()
                        .inset_0()
                        .occlude()
                        .bg(gpui::black().opacity(0.35))
                        .flex()
                        // A card sized to its content, not a full-height column.
                        .items_start()
                        .justify_center()
                        .pt(px(chrome::BAR_HEIGHT + space::XL))
                        .child(sheet_drag_strip("connect-drag-strip"))
                        .child(appear(
                            div().child(sheet.clone()),
                            "connect-appear",
                            -motion::SHEET_OFFSET,
                            motion::SHEET_MS,
                            reduce_motion,
                        )),
                )
                .with_priority(3)
            });
        let shortcuts = match &self.modal {
            Some(Modal::Shortcuts(sections, handle)) => Some(render_shortcuts(
                sections,
                handle,
                &theme,
                reduce_motion,
                cx,
            )),
            _ => None,
        };
        let palette = self.palette().map(|palette| {
            // A click beside it closes it, never reaching the panes.
            deferred(
                div()
                    .id("palette-backdrop")
                    .absolute()
                    .inset_0()
                    .occlude()
                    .flex()
                    .items_start()
                    .justify_center()
                    .pt(px(chrome::BAR_HEIGHT + space::XL))
                    .on_mouse_down(
                        gpui::MouseButton::Left,
                        cx.listener(|this, _, window, cx| {
                            if this.palette().is_some() {
                                this.toggle_palette(window, cx);
                            }
                        }),
                    )
                    .child(appear(
                        div().child(palette.clone()),
                        "palette-appear",
                        -motion::SHEET_OFFSET,
                        motion::SHEET_MS,
                        reduce_motion,
                    )),
            )
            .with_priority(2)
        });
        let search_all = self.search_all().map(|view| {
            // Placed as the palette is; a click beside it closes it.
            deferred(
                div()
                    .id("search-all-backdrop")
                    .absolute()
                    .inset_0()
                    .occlude()
                    .flex()
                    .items_start()
                    .justify_center()
                    .pt(px(chrome::BAR_HEIGHT + space::XL))
                    .on_mouse_down(
                        gpui::MouseButton::Left,
                        cx.listener(|this, _, window, cx| {
                            if this.search_all().is_some() {
                                this.close_modal(window, cx);
                            }
                        }),
                    )
                    .child(appear(
                        div().child(view.clone()),
                        "search-all-appear",
                        -motion::SHEET_OFFSET,
                        motion::SHEET_MS,
                        reduce_motion,
                    )),
            )
            .with_priority(2)
        });
        let switcher = self.switcher().map(|view| {
            // Centred, like ⌘Tab; a click beside it cancels.
            deferred(
                div()
                    .id("switcher-backdrop")
                    .absolute()
                    .inset_0()
                    .occlude()
                    .flex()
                    .items_center()
                    .justify_center()
                    .on_mouse_down(
                        gpui::MouseButton::Left,
                        cx.listener(|this, _, window, cx| {
                            if this.switcher().is_some() {
                                this.close_modal(window, cx);
                            }
                        }),
                    )
                    .child(appear(
                        div().child(view.clone()),
                        "switcher-appear",
                        -motion::SHEET_OFFSET,
                        motion::SHEET_MS,
                        reduce_motion,
                    )),
            )
            .with_priority(2)
        });
        let rename = self.rename_sheet().map(|sheet| {
            deferred(
                div()
                    .id("rename-backdrop")
                    .absolute()
                    .inset_0()
                    .occlude()
                    .flex()
                    .items_start()
                    .justify_center()
                    .pt(px(chrome::BAR_HEIGHT + space::SM))
                    .child(sheet_drag_strip("rename-drag-strip"))
                    .child(appear(
                        div().child(sheet.clone()),
                        "rename-appear",
                        -motion::SHEET_OFFSET,
                        motion::SHEET_MS,
                        reduce_motion,
                    )),
            )
            .with_priority(2)
        });
        let answer = self.answer_sheet().map(|sheet| {
            deferred(
                div()
                    .id("answer-backdrop")
                    .absolute()
                    .inset_0()
                    .occlude()
                    .flex()
                    .items_start()
                    .justify_center()
                    .pt(px(chrome::BAR_HEIGHT + space::SM))
                    .child(sheet_drag_strip("answer-drag-strip"))
                    .child(appear(
                        div().child(sheet.clone()),
                        "answer-appear",
                        -motion::SHEET_OFFSET,
                        motion::SHEET_MS,
                        reduce_motion,
                    )),
            )
            .with_priority(2)
        });
        let labels = self.labels_sheet().map(|sheet| {
            deferred(
                div()
                    .id("labels-backdrop")
                    .absolute()
                    .inset_0()
                    .occlude()
                    .flex()
                    .items_start()
                    .justify_center()
                    .pt(px(chrome::BAR_HEIGHT + space::SM))
                    .child(sheet_drag_strip("labels-drag-strip"))
                    .child(appear(
                        div().child(sheet.clone()),
                        "labels-appear",
                        -motion::SHEET_OFFSET,
                        motion::SHEET_MS,
                        reduce_motion,
                    )),
            )
            .with_priority(2)
        });
        let sheet = self.sheet().map(|sheet| {
            // The backdrop takes every click and wheel event over the panes.
            deferred(
                div()
                    .id("sheet-backdrop")
                    .absolute()
                    .inset_0()
                    .occlude()
                    .flex()
                    .items_start()
                    .justify_center()
                    // Hung from the bar, like an AppKit sheet.
                    .pt(px(chrome::BAR_HEIGHT + space::SM))
                    .child(sheet_drag_strip("sheet-drag-strip"))
                    .child(appear(
                        div().child(sheet.clone()),
                        "sheet-appear",
                        -motion::SHEET_OFFSET,
                        motion::SHEET_MS,
                        reduce_motion,
                    )),
            )
            .with_priority(2)
        });
        div()
            .id("workspace")
            .key_context(WORKSPACE_CONTEXT)
            .size_full()
            .relative()
            .flex()
            .flex_col()
            .when(tint.everywhere(), |s| s.bg(theme.window))
            .text_color(theme.text)
            .capture_key_down(cx.listener(|this, ev: &KeyDownEvent, window, cx| {
                // Esc ends a drag before it reaches the focused pane.
                if ev.keystroke.key == "escape" && this.pane_drag.cancel() {
                    cx.stop_propagation();
                    window.refresh();
                }
            }))
            .on_action(cx.listener(Self::toggle_maximize))
            .on_action(
                cx.listener(|this, _: &SplitRight, w, cx| this.split(Axis::Horizontal, w, cx)),
            )
            .on_action(cx.listener(|this, _: &SplitDown, w, cx| this.split(Axis::Vertical, w, cx)))
            .on_action(cx.listener(Self::close_pane))
            .on_action(cx.listener(|this, _: &FocusLeft, w, cx| this.focus_dir(Dir::Left, w, cx)))
            .on_action(cx.listener(|this, _: &FocusRight, w, cx| this.focus_dir(Dir::Right, w, cx)))
            .on_action(cx.listener(|this, _: &FocusUp, w, cx| this.focus_dir(Dir::Up, w, cx)))
            .on_action(cx.listener(|this, _: &FocusDown, w, cx| this.focus_dir(Dir::Down, w, cx)))
            .on_action(cx.listener(|this, _: &FocusPane1, w, cx| this.focus_number(1, w, cx)))
            .on_action(cx.listener(|this, _: &FocusPane2, w, cx| this.focus_number(2, w, cx)))
            .on_action(cx.listener(|this, _: &FocusPane3, w, cx| this.focus_number(3, w, cx)))
            .on_action(cx.listener(|this, _: &FocusPane4, w, cx| this.focus_number(4, w, cx)))
            .on_action(cx.listener(Self::new_agent))
            .on_action(cx.listener(Self::new_terminal))
            .on_action(cx.listener(Self::focus_filter))
            .on_action(cx.listener(Self::kill))
            .on_action(cx.listener(Self::kill_pane))
            .on_action(cx.listener(Self::rename_pane))
            .on_action(cx.listener(Self::edit_labels_pane))
            .on_action(cx.listener(Self::duplicate_pane))
            .on_action(cx.listener(Self::remove_pane))
            .on_action(cx.listener(Self::restart))
            .on_action(cx.listener(Self::remove))
            .on_action(cx.listener(Self::remove_exited))
            .on_action(cx.listener(Self::kill_selected))
            .on_action(cx.listener(Self::restart_selected))
            .on_action(cx.listener(Self::remove_selected))
            .on_action(cx.listener(|this, _: &SaveOutput, _, cx| {
                this.output_focused(PaneMenuItem::SaveOutput, cx)
            }))
            .on_action(cx.listener(|this, _: &OpenOutput, _, cx| {
                this.output_focused(PaneMenuItem::OpenOutput, cx)
            }))
            .on_action(cx.listener(|this, _: &CopyLastOutput, _, cx| {
                this.output_focused(PaneMenuItem::CopyLastOutput, cx)
            }))
            .on_action(cx.listener(Self::send_to_agents))
            .on_action(cx.listener(Self::search_all_agents))
            .on_action(cx.listener(|this, _: &NextNeedsYou, w, cx| this.next_needs_you(w, cx)))
            .on_action(cx.listener(Self::restart_pane))
            .on_action(cx.listener(|this, _: &ZoomIn, w, cx| this.zoom(Zoom::In, w, cx)))
            .on_action(cx.listener(|this, _: &ZoomOut, w, cx| this.zoom(Zoom::Out, w, cx)))
            .on_action(cx.listener(|this, _: &ZoomReset, w, cx| this.zoom(Zoom::Reset, w, cx)))
            .on_action(cx.listener(|this, _: &ToggleGraph, w, cx| this.toggle_graph(w, cx)))
            .on_action(cx.listener(Self::reopen_closed_pane))
            .on_action(cx.listener(|this, _: &ShowShortcuts, w, cx| this.toggle_shortcuts(w, cx)))
            .on_action(cx.listener(Self::command_palette))
            .on_action(cx.listener(Self::show_changes_command))
            .on_action(cx.listener(Self::run_action_command))
            .on_action(cx.listener(Self::archive_worktree_command))
            .on_action(cx.listener(|this, _: &NextAgent, w, cx| this.switch_agent(false, w, cx)))
            .on_action(cx.listener(|this, _: &PreviousAgent, w, cx| this.switch_agent(true, w, cx)))
            .on_modifiers_changed(cx.listener(Self::switcher_modifiers))
            .on_action(cx.listener(Self::new_tab))
            .on_action(cx.listener(Self::close_shown_tab))
            .on_action(cx.listener(Self::next_tab))
            .on_action(cx.listener(Self::prev_tab))
            .on_action(cx.listener(Self::select_numbered_tab))
            .child(bar)
            .child(if self.graph_shown {
                fill_content(
                    div().flex().flex_1().min_h_0().child(self.graph.0.clone()),
                    (tint.content && !tint.everywhere()).then_some(theme.window),
                )
            } else {
                div()
                    .flex()
                    .flex_row()
                    .flex_1()
                    .min_h_0()
                    .child(sidebar)
                    .child(panes)
            })
            .children(exits)
            .children(graph_pending)
            .children(toast)
            .children(palette)
            .children(search_all)
            .children(switcher)
            .children(
                self.peek()
                    .map(|p| peek_view::render_over(p, reduce_motion)),
            )
            .children(sheet)
            .children(rename)
            .children(answer)
            .children(labels)
            .children(
                self.kill_sheet()
                    .map(|sheet| kill_sheet::render_over(sheet, reduce_motion)),
            )
            .children(
                self.send_sheet()
                    .map(|sheet| send_sheet::render_over(sheet, reduce_motion)),
            )
            .children(
                self.action_sheet()
                    .map(|sheet| action_sheet::render_over(sheet, reduce_motion)),
            )
            .children(shortcuts)
            .children(connect)
    }
}

/// The ⌘/ cheat sheet over the window: Escape, ⌘/ or a click outside
/// closes it.
fn render_shortcuts(
    sections: &[Section],
    handle: &FocusHandle,
    theme: &Theme,
    reduce_motion: bool,
    cx: &mut Context<Workspace>,
) -> impl IntoElement + use<> {
    let columns = sections.iter().map(|section| {
        div()
            .flex()
            .flex_col()
            .gap(px(space::XS))
            .min_w(px(220.))
            .child(
                div()
                    .type_style(text::SECTION)
                    .text_color(theme.secondary)
                    .pb(px(space::XXS))
                    .child(SharedString::from(section.title.clone())),
            )
            .children(section.rows.iter().map(|row| {
                div()
                    .flex()
                    .flex_row()
                    .justify_between()
                    .gap(px(space::LG))
                    .type_style(text::CALLOUT)
                    .child(SharedString::from(row.title.clone()))
                    .child(
                        div()
                            .text_color(theme.secondary)
                            .child(SharedString::from(row.keys.clone())),
                    )
            }))
    });
    let card = controls::sheet_card("shortcuts-card", 760.0, theme)
        .debug_selector(|| "shortcuts-sheet".to_owned())
        .max_h(relative(0.8))
        .overflow_y_scroll()
        // A click on the card is not one outside it.
        .on_click(|_, _, cx| cx.stop_propagation())
        .child(controls::sheet_title("Keyboard Shortcuts"))
        .child(
            div()
                .flex()
                .flex_row()
                .flex_wrap()
                .gap(px(space::XL))
                .children(columns),
        );
    deferred(
        div()
            .id("shortcuts-backdrop")
            .track_focus(handle)
            .on_key_down(cx.listener(|this, e: &gpui::KeyDownEvent, window, cx| {
                if e.keystroke.key == "escape" {
                    this.toggle_shortcuts(window, cx);
                }
            }))
            .on_click(cx.listener(|this, _, window, cx| this.toggle_shortcuts(window, cx)))
            .absolute()
            .inset_0()
            .occlude()
            .flex()
            .items_start()
            .justify_center()
            .pt(px(chrome::BAR_HEIGHT + space::SM))
            .child(sheet_drag_strip("shortcuts-drag-strip"))
            .child(appear(
                div().child(card),
                "shortcuts-appear",
                -motion::SHEET_OFFSET,
                motion::SHEET_MS,
                reduce_motion,
            )),
    )
    .with_priority(2)
}

/// `el` filled with `bg` when the content is tinted on its own (glass).
fn fill_content(el: Div, bg: Option<gpui::Hsla>) -> Div {
    match bg {
        Some(bg) => el.bg(bg),
        None => el,
    }
}
