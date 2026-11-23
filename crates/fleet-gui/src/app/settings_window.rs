//! The Settings window (⌘, and fleet › Settings…, ADR 0044): Appearance,
//! Behaviour, Agents and Keys. Every control writes its `[gui]` key of
//! `config.toml` at once and takes effect live ([`AppState::change_settings`]);
//! text fields apply on Return or when they lose focus. What each control
//! changes is `core::settings`; this only draws it.

use gpui::prelude::FluentBuilder;
use gpui::{
    App, AppContext, Bounds, Context, Div, ElementId, Entity, FocusHandle, Focusable, Global, Hsla,
    InteractiveElement, IntoElement, KeyDownEvent, MouseButton, ParentElement, PathPromptOptions,
    Render, SharedString, Stateful, StatefulInteractiveElement, Styled, Subscription,
    TitlebarOptions, Window, WindowBackgroundAppearance, WindowBounds, WindowHandle, WindowOptions,
    div, px, size,
};

use super::app_state::AppState;
use super::controls::{
    ButtonKind, button, error_text, move_focus, return_is_not_a_click, tab_stop,
};
use super::keymap::ClosePane;
use super::native;
use super::status_item::set_accessory;
use super::text_field::TextField;
use super::theme::{Theme, Typography, current as theme};
use crate::core::cursor_blink::BlinkChoice;
use crate::core::glass::GlassChoice;
use crate::core::keyboard_nav::{Axis, arrow_step, step_choice};
use crate::core::presets::{Preset, presets_note};
use crate::core::settings::{
    BLUR_MAX, BLUR_MIN, BLUR_STEP, Change, FONT_SIZE_MAX, FONT_SIZE_MIN, FONT_SIZE_STEP,
    MotionChoice, OPACITY_MAX, OPACITY_MIN, OPACITY_STEP, OptionAsMeta, SHORTCUTS, Section,
    ThemeChoice, key_label, parse_command_line,
};
use crate::core::style::{chrome, radius, space, text};
use crate::core::target::Target;

const WIDTH: f32 = 620.0;
const HEIGHT: f32 = 520.0;
/// The width of a row's label column.
const LABEL_WIDTH: f32 = 190.0;

/// The open Settings window, if any.
struct SettingsGlobal(WindowHandle<SettingsWindow>);

impl Global for SettingsGlobal {}

/// Opens the Settings window, or brings the open one to the front. The
/// config file is read again first, so edits made to it meanwhile show.
pub fn open_settings(cx: &mut App) {
    let Some(app) = AppState::global(cx) else {
        return;
    };
    if let Some(handle) = cx.try_global::<SettingsGlobal>().map(|g| g.0)
        && handle
            .update(cx, |_, window, _| window.activate_window())
            .is_ok()
    {
        return;
    }
    app.update(cx, |a, cx| a.reload_config(cx));
    // From the menu bar with no window open: come back to the Dock.
    if cx.windows().is_empty() {
        set_accessory(false);
    }
    let options = WindowOptions {
        window_bounds: Some(WindowBounds::Windowed(Bounds::centered(
            None,
            size(px(WIDTH), px(HEIGHT)),
            cx,
        ))),
        titlebar: Some(TitlebarOptions {
            title: Some("Settings".into()),
            appears_transparent: false,
            traffic_light_position: None,
        }),
        is_resizable: false,
        is_minimizable: false,
        window_background: WindowBackgroundAppearance::Opaque,
        ..Default::default()
    };
    match cx.open_window(options, |window, cx| {
        cx.new(|cx| SettingsWindow::new(app, window, cx))
    }) {
        Ok(handle) => {
            cx.set_global(SettingsGlobal(handle));
            cx.activate(true);
        }
        Err(e) => eprintln!("fleet-gui: cannot open Settings: {e}"),
    }
}

/// The open Settings window, if any (for tests).
pub fn settings_window(cx: &App) -> Option<WindowHandle<SettingsWindow>> {
    cx.try_global::<SettingsGlobal>().map(|g| g.0)
}

/// A text setting, applied on Return or when its field loses focus.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum TextSetting {
    FontFamily,
    DefaultCommand,
    DefaultDir,
}

const TEXT_SETTINGS: [TextSetting; 3] = [
    TextSetting::FontFamily,
    TextSetting::DefaultCommand,
    TextSetting::DefaultDir,
];

pub struct SettingsWindow {
    app: Entity<AppState>,
    focus: FocusHandle,
    pub(crate) section: Section,
    pub(crate) font_family: Entity<TextField>,
    pub(crate) command: Entity<TextField>,
    pub(crate) folder: Entity<TextField>,
    /// The command presets as of opening (ADR 0084): edited in
    /// `config.toml`, listed here.
    pub(crate) presets: Vec<Preset>,
    /// They were found on `PATH` (no `[gui] presets`).
    presets_detected: bool,
    /// Why the last change was not applied.
    pub(crate) error: Option<String>,
    _subscriptions: Vec<Subscription>,
}

impl Focusable for SettingsWindow {
    fn focus_handle(&self, _: &App) -> FocusHandle {
        self.focus.clone()
    }
}

impl SettingsWindow {
    pub fn new(app: Entity<AppState>, window: &mut Window, cx: &mut Context<Self>) -> Self {
        let c = app.read(cx).config().clone();
        let presets = app.update(cx, |a, _| a.presets());
        let presets_detected = app.read(cx).presets_detected();
        let font_family =
            cx.new(|cx| TextField::new(c.font_family.clone(), "SF Mono", cx).tab_stop());
        let command = cx.new(|cx| {
            TextField::new(shell_words::join(&c.default_command), "claude", cx).tab_stop()
        });
        let folder = cx.new(|cx| {
            TextField::new(c.default_dir.clone(), "The most recent folder", cx).tab_stop()
        });
        let mut subs = vec![cx.observe(&app, |_, _, cx| cx.notify())];
        for (field, which) in [&font_family, &command, &folder]
            .into_iter()
            .zip(TEXT_SETTINGS)
        {
            let handle = field.read(cx).focus_handle().clone();
            subs.push(cx.on_blur(&handle, window, move |this, _, cx| {
                this.commit(which, cx);
            }));
        }
        let focus = cx.focus_handle();
        window.focus(&focus);
        SettingsWindow {
            app,
            focus,
            section: Section::default(),
            font_family,
            command,
            folder,
            presets,
            presets_detected,
            error: None,
            _subscriptions: subs,
        }
    }

    pub fn section(&self) -> Section {
        self.section
    }

    /// Why the last change was not applied, if it was not.
    pub fn error(&self) -> Option<&str> {
        self.error.as_deref()
    }

    fn field(&self, which: TextSetting) -> &Entity<TextField> {
        match which {
            TextSetting::FontFamily => &self.font_family,
            TextSetting::DefaultCommand => &self.command,
            TextSetting::DefaultDir => &self.folder,
        }
    }

    pub fn field_text(&self, which: TextSetting, cx: &App) -> String {
        self.field(which).read(cx).text().to_owned()
    }

    /// Types into a field (as a paste would), without applying it.
    pub fn set_field_text(&mut self, which: TextSetting, text: String, cx: &mut Context<Self>) {
        self.field(which).update(cx, |f, cx| f.set_text(text, cx));
    }

    pub fn focus_field(&self, which: TextSetting, window: &mut Window, cx: &App) {
        window.focus(self.field(which).read(cx).focus_handle());
    }

    pub fn show_section(&mut self, section: Section, cx: &mut Context<Self>) {
        self.section = section;
        cx.notify();
    }

    /// Applies one change; a failure (the file cannot be written) is shown.
    pub fn change(&mut self, change: Change, cx: &mut Context<Self>) {
        let result = self
            .app
            .update(cx, |a, cx| a.change_settings(vec![change], cx));
        self.error = result.err();
        cx.notify();
    }

    /// Applies a text field's text (a command that does not parse is
    /// shown, not applied).
    pub fn commit(&mut self, which: TextSetting, cx: &mut Context<Self>) {
        let change = match which {
            TextSetting::FontFamily => {
                Change::FontFamily(self.font_family.read(cx).text().to_owned())
            }
            TextSetting::DefaultDir => Change::DefaultDir(self.folder.read(cx).text().to_owned()),
            TextSetting::DefaultCommand => match parse_command_line(self.command.read(cx).text()) {
                Ok(words) => Change::DefaultCommand(words),
                Err(e) => {
                    self.error = Some(e);
                    cx.notify();
                    return;
                }
            },
        };
        self.change(change, cx);
        // Show what was kept (trimmed, or the default family).
        let c = self.app.read(cx).config();
        let text = match which {
            TextSetting::FontFamily => c.font_family.clone(),
            TextSetting::DefaultDir => c.default_dir.clone(),
            TextSetting::DefaultCommand => shell_words::join(&c.default_command),
        };
        let field = self.field(which).clone();
        if field.read(cx).text() != text {
            field.update(cx, |f, cx| f.set_text(text, cx));
        }
    }

    fn choose_folder(&mut self, cx: &mut Context<Self>) {
        let paths = cx.prompt_for_paths(PathPromptOptions {
            files: false,
            directories: true,
            multiple: false,
            prompt: Some("Choose".into()),
        });
        cx.spawn(async move |this, cx| {
            if let Ok(Ok(Some(paths))) = paths.await
                && let Some(path) = paths.into_iter().next()
            {
                let _ = this.update(cx, |this, cx| {
                    let path = path.to_string_lossy().into_owned();
                    this.folder.update(cx, |f, cx| f.set_text(path, cx));
                    this.commit(TextSetting::DefaultDir, cx);
                });
            }
        })
        .detach();
    }

    fn key_down(&mut self, ev: &KeyDownEvent, window: &mut Window, cx: &mut Context<Self>) {
        if move_focus(ev, window) {
            cx.stop_propagation();
            return;
        }
        if ev.keystroke.key != "enter" {
            return;
        }
        let focused = TEXT_SETTINGS
            .into_iter()
            .find(|w| self.field(*w).read(cx).focus_handle().is_focused(window));
        if let Some(which) = focused {
            self.commit(which, cx);
            cx.stop_propagation();
        }
    }

    fn tabs(&self, theme: &Theme, cx: &mut Context<Self>) -> Div {
        let tabs = self
            .choices(
                Choices {
                    id: "tabs",
                    all: &Section::ALL,
                    current: self.section,
                    enabled: true,
                    label: Section::title,
                    selector: |s| format!("settings-tab-{}", s.title()),
                    pick: |this, s, cx| this.show_section(s, cx),
                },
                theme,
                cx,
            )
            .gap(px(space::XS));
        div()
            .flex()
            .flex_row()
            .justify_center()
            .pb(px(space::MD))
            .border_b_1()
            .border_color(theme.separator)
            .child(tabs)
    }

    /// A segmented control (ADR 0101): one Tab stop; ← and → pick the
    /// previous or next choice, a click picks one. A disabled one is
    /// dimmed and takes neither.
    fn choices<T: Copy + PartialEq + 'static>(
        &self,
        c: Choices<'_, T>,
        theme: &Theme,
        cx: &mut Context<Self>,
    ) -> Stateful<Div> {
        let Choices {
            id,
            all,
            current,
            enabled,
            label,
            selector,
            pick,
        } = c;
        let mut row = div()
            .id(id)
            .debug_selector(move || format!("choices-{id}"))
            .flex()
            .flex_row()
            .gap(px(space::XXS))
            .rounded(px(radius::MD));
        for (i, &choice) in all.iter().enumerate() {
            let p = pill((id, i), label(choice), choice == current, theme)
                .debug_selector(move || selector(choice));
            row = row.child(if enabled {
                p.on_click(cx.listener(move |this, _, _, cx| pick(this, choice, cx)))
            } else {
                p.opacity(0.5)
            });
        }
        if !enabled {
            return row;
        }
        let all = all.to_vec();
        tab_stop(row, theme).on_key_down(cx.listener(move |this, ev: &KeyDownEvent, _, cx| {
            let step = arrow_step(
                &ev.keystroke.key,
                ev.keystroke.modifiers.modified(),
                Axis::Row,
            );
            if let Some(next) = step.and_then(|d| step_choice(&all, current, d)) {
                pick(this, next, cx);
                cx.stop_propagation();
            }
        }))
    }

    /// `−  value  +` (ADR 0101): one Tab stop; ← and ↓ step down, → and ↑
    /// up. A button at the end of the range is dimmed.
    fn stepper(
        &self,
        s: Stepper,
        theme: &Theme,
        cx: &mut Context<Self>,
        step: impl Fn(&mut Self, f32, &mut Context<Self>) + Clone + 'static,
    ) -> Stateful<Div> {
        let Stepper {
            id,
            value,
            can_down,
            can_up,
        } = s;
        let side = |suffix: &'static str, label: &'static str, sign: f32, enabled: bool| {
            let b = button(
                SharedString::from(format!("{id}-{suffix}")),
                label,
                theme,
                ButtonKind::Secondary,
                enabled,
            )
            .w(px(chrome::CONTROL_HEIGHT))
            .debug_selector(move || format!("{id}-{suffix}"));
            let step = step.clone();
            if enabled {
                b.on_click(cx.listener(move |this, _, _, cx| step(this, sign, cx)))
            } else {
                b
            }
        };
        let down = side("down", "−", -1.0, can_down);
        let up = side("up", "+", 1.0, can_up);
        let row = div()
            .id(id)
            .debug_selector(move || format!("{id}-stepper"))
            .flex()
            .flex_row()
            .items_center()
            .gap(px(space::SM))
            .rounded(px(radius::MD))
            .child(down)
            .child(
                div()
                    .min_w(px(56.))
                    .flex()
                    .justify_center()
                    .debug_selector(move || format!("{id}-value"))
                    .child(SharedString::from(value)),
            )
            .child(up);
        if !can_down && !can_up {
            return row;
        }
        tab_stop(row, theme).on_key_down(cx.listener(move |this, ev: &KeyDownEvent, _, cx| {
            let d = arrow_step(
                &ev.keystroke.key,
                ev.keystroke.modifiers.modified(),
                Axis::Stepper,
            );
            match d {
                Some(-1) if can_down => step(this, -1.0, cx),
                Some(1) if can_up => step(this, 1.0, cx),
                Some(_) => {}
                None => return,
            }
            cx.stop_propagation();
        }))
    }

    fn appearance(&self, theme: &Theme, cx: &mut Context<Self>) -> Div {
        let c = self.app.read(cx).config().clone();
        // Reduce Transparency draws the window opaque: no blur, no tint.
        let solid = native::reduce_transparency();
        let solid_hint = solid.then_some("Off while Reduce Transparency is on in System Settings");
        let themes = self.choices(
            Choices {
                id: "theme",
                all: &ThemeChoice::ALL,
                current: c.theme,
                enabled: true,
                label: ThemeChoice::title,
                selector: |t| format!("theme-{}", t.title()),
                pick: |this, t, cx| this.change(Change::Theme(t), cx),
            },
            theme,
            cx,
        );
        let blur = self.stepper(
            Stepper {
                id: "blur",
                value: format!("{} pt", c.blur),
                can_down: !solid && c.blur > BLUR_MIN,
                can_up: !solid && c.blur < BLUR_MAX,
            },
            theme,
            cx,
            move |this, sign, cx| this.change(Change::Blur(c.blur + sign * BLUR_STEP), cx),
        );
        let opacity = c.window_opacity;
        let opacity_stepper = self.stepper(
            Stepper {
                id: "opacity",
                value: format!("{} %", (opacity * 100.0).round()),
                can_down: !solid && opacity > OPACITY_MIN,
                can_up: !solid && opacity < OPACITY_MAX,
            },
            theme,
            cx,
            move |this, sign, cx| {
                this.change(Change::WindowOpacity(opacity + sign * OPACITY_STEP), cx);
            },
        );
        // Liquid glass (ADR 0049): macOS 26 only, and not under Reduce
        // Transparency.
        let has_glass = native::glass_available();
        let glass_hint = if solid {
            "Off while Reduce Transparency is on in System Settings"
        } else if !has_glass {
            "Needs macOS 26; the blur is used instead"
        } else {
            "Liquid glass behind the window or only its sidebar and bar"
        };
        let glass = self.choices(
            Choices {
                id: "glass",
                all: &GlassChoice::ALL,
                current: c.glass,
                enabled: !solid && has_glass,
                label: GlassChoice::title,
                selector: |g| format!("glass-{}", g.title()),
                pick: |this, g, cx| this.change(Change::Glass(g), cx),
            },
            theme,
            cx,
        );
        let size = c.font_size;
        let font_size = self.stepper(
            Stepper {
                id: "font-size",
                value: format!("{size} pt"),
                can_down: size > FONT_SIZE_MIN,
                can_up: size < FONT_SIZE_MAX,
            },
            theme,
            cx,
            move |this, sign, cx| this.change(Change::FontSize(size + sign * FONT_SIZE_STEP), cx),
        );
        section_body()
            .child(setting_row("Theme", None, themes, theme))
            .child(setting_row("Liquid glass", Some(glass_hint), glass, theme))
            .child(setting_row("Blur strength", solid_hint, blur, theme))
            .child(setting_row(
                "Window opacity",
                solid_hint,
                opacity_stepper,
                theme,
            ))
            .child(setting_row(
                "Terminal font",
                Some("An installed family; Menlo when it is missing"),
                div().w(px(220.)).child(self.font_family.clone()),
                theme,
            ))
            .child(setting_row("Font size", None, font_size, theme))
    }

    fn behaviour(&self, theme: &Theme, cx: &mut Context<Self>) -> Div {
        let c = self.app.read(cx).config().clone();
        let motion = self.choices(
            Choices {
                id: "motion",
                all: &MotionChoice::ALL,
                current: c.reduce_motion,
                enabled: true,
                label: MotionChoice::title,
                selector: |m| format!("motion-{}", m.title()),
                pick: |this, m, cx| this.change(Change::ReduceMotion(m), cx),
            },
            theme,
            cx,
        );
        let blink = self.choices(
            Choices {
                id: "cursor-blink",
                all: &BlinkChoice::ALL,
                current: c.cursor_blink,
                enabled: true,
                label: BlinkChoice::title,
                selector: |b| format!("cursor-blink-{}", b.as_str()),
                pick: |this, b, cx| this.change(Change::CursorBlink(b), cx),
            },
            theme,
            cx,
        );
        let option_meta = self.choices(
            Choices {
                id: "option-as-meta",
                all: &OptionAsMeta::ALL,
                current: c.option_as_meta,
                enabled: true,
                label: OptionAsMeta::title,
                selector: |o| format!("option-as-meta-{}", o.title()),
                pick: |this, o, cx| this.change(Change::OptionAsMeta(o), cx),
            },
            theme,
            cx,
        );
        let system_hint = if native::reduce_motion() {
            "System: Reduce Motion is on in System Settings"
        } else {
            "System: Reduce Motion is off in System Settings"
        };
        section_body()
            .child(setting_row("Motion", Some(system_hint), motion, theme))
            .child(setting_row(
                "Cursor blink",
                Some("Only in the focused pane, and never with reduced motion"),
                blink,
                theme,
            ))
            .child(setting_row(
                "Confirm before killing",
                Some("Kill asks first; closing a pane only detaches"),
                switch("confirm-kill", c.confirm_kill, theme).on_click(cx.listener(
                    move |this, _, _, cx| this.change(Change::ConfirmKill(!c.confirm_kill), cx),
                )),
                theme,
            ))
            .child(setting_row(
                "Notifications",
                Some("When an agent needs you or finishes"),
                switch("notifications", c.notifications, theme).on_click(cx.listener(
                    move |this, _, _, cx| {
                        this.change(Change::Notifications(!c.notifications), cx);
                    },
                )),
                theme,
            ))
            .child(setting_row(
                "Focus mode",
                Some("Dim the panes that do not have focus"),
                switch("focus-mode", c.focus_mode, theme).on_click(cx.listener(
                    move |this, _, _, cx| this.change(Change::FocusMode(!c.focus_mode), cx),
                )),
                theme,
            ))
            .child(setting_row(
                "Celebrate success",
                Some("A soft sound and a small burst when an agent exits with code 0"),
                switch("celebrate", c.celebrate, theme).on_click(cx.listener(
                    move |this, _, _, cx| this.change(Change::Celebrate(!c.celebrate), cx),
                )),
                theme,
            ))
            .child(setting_row(
                "Option as Meta",
                Some("Option+key sends Esc and the key. The other Option types # @ € [ ]"),
                option_meta,
                theme,
            ))
    }

    fn agents(&self, theme: &Theme, cx: &mut Context<Self>) -> Div {
        let local = *self.app.read(cx).target() == Target::Local;
        let mut folder = div()
            .flex()
            .flex_row()
            .gap(px(space::SM))
            .w(px(300.))
            .child(div().flex_1().child(self.folder.clone()));
        if local {
            folder = folder.child(
                tab_stop(
                    button(
                        "choose-folder",
                        "Choose…",
                        theme,
                        ButtonKind::Secondary,
                        true,
                    ),
                    theme,
                )
                .on_click(cx.listener(|this, _, _, cx| this.choose_folder(cx))),
            );
        }
        section_body()
            .child(setting_row(
                "Default command",
                Some("New agent (⌘N) starts with it; shell quoting applies"),
                div().w(px(300.)).child(self.command.clone()),
                theme,
            ))
            .child(setting_row(
                "Default folder",
                Some("New agent's folder; empty: the most recent one"),
                folder,
                theme,
            ))
            .child(setting_row(
                "Presets",
                Some(presets_note(self.presets_detected)),
                self.preset_list(theme),
                theme,
            ))
    }

    /// The presets, read-only: name, then command and labels.
    fn preset_list(&self, theme: &Theme) -> Stateful<Div> {
        let mut list = div()
            .id("presets")
            .debug_selector(|| "settings-presets".to_owned())
            .w(px(300.))
            .flex()
            .flex_col()
            .p(px(space::XS))
            .rounded(px(radius::MD))
            .border_1()
            .border_color(theme.separator)
            .bg(theme.field)
            .max_h(px(180.))
            .overflow_y_scroll();
        if self.presets.is_empty() {
            list = list.child(
                div()
                    .px(px(space::SM))
                    .py(px(space::XS))
                    .type_style(text::CALLOUT)
                    .text_color(theme.tertiary)
                    .child("None"),
            );
        }
        for (i, p) in self.presets.iter().enumerate() {
            let labels = p.labels_text();
            let detail = if labels.is_empty() {
                p.command_line()
            } else {
                format!("{} · {labels}", p.command_line())
            };
            list = list.child(
                div()
                    .id(("preset", i))
                    .flex()
                    .flex_col()
                    .px(px(space::SM))
                    .py(px(space::XS))
                    .type_style(text::CALLOUT)
                    .child(SharedString::from(p.name.clone()))
                    .child(
                        div()
                            .truncate()
                            .text_color(theme.secondary)
                            .child(SharedString::from(detail)),
                    ),
            );
        }
        list
    }

    fn keys(&self, theme: &Theme) -> Div {
        let mut list = div()
            .id("shortcuts")
            .flex()
            .flex_col()
            .p(px(space::XS))
            .rounded(px(radius::MD))
            .border_1()
            .border_color(theme.separator)
            .bg(theme.field)
            .max_h(px(HEIGHT - 150.0))
            .overflow_y_scroll();
        for (i, s) in SHORTCUTS.iter().enumerate() {
            list = list.child(
                div()
                    .id(("shortcut", i))
                    .flex()
                    .flex_row()
                    .justify_between()
                    .px(px(space::SM))
                    .py(px(space::XS))
                    .type_style(text::CALLOUT)
                    .child(s.what)
                    .child(
                        div()
                            .text_color(theme.secondary)
                            .child(SharedString::from(key_label(s.keys))),
                    ),
            );
        }
        section_body().child(list).child(
            div()
                .type_style(text::SUBHEADLINE)
                .text_color(theme.tertiary)
                .child("Shortcuts cannot be changed yet."),
        )
    }
}

impl Render for SettingsWindow {
    fn render(&mut self, _window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let theme = theme(cx);
        let body = match self.section {
            Section::Appearance => self.appearance(&theme, cx),
            Section::Behaviour => self.behaviour(&theme, cx),
            Section::Agents => self.agents(&theme, cx),
            Section::Keys => self.keys(&theme),
        };
        let tabs = self.tabs(&theme, cx);
        return_is_not_a_click(div().id("settings"))
            .debug_selector(|| "settings-window".to_owned())
            .track_focus(&self.focus)
            .size_full()
            .flex()
            .flex_col()
            .gap(px(space::MD))
            .p(px(space::LG))
            .bg(opaque(theme.raised))
            .text_color(theme.text)
            .type_style(text::BODY)
            .on_key_down(cx.listener(Self::key_down))
            // ⌘W closes Settings (it is bound to Close Pane).
            .on_action(|_: &ClosePane, window, _| window.remove_window())
            // A click off the fields ends their editing (and applies it).
            .on_mouse_down(
                MouseButton::Left,
                cx.listener(|this, _, window, _| window.focus(&this.focus)),
            )
            .child(tabs)
            .child(body)
            .children(self.error.clone().map(|e| {
                error_text(SharedString::from(e), &theme)
                    .debug_selector(|| "settings-error".to_owned())
            }))
    }
}

fn opaque(c: Hsla) -> Hsla {
    Hsla { a: 1.0, ..c }
}

fn section_body() -> Div {
    div().flex().flex_col().gap(px(space::MD))
}

/// A label (and a hint under it) on the left, the control on the right.
fn setting_row(
    label: &'static str,
    hint: Option<&'static str>,
    control: impl IntoElement,
    theme: &Theme,
) -> Div {
    div()
        .flex()
        .flex_row()
        .items_center()
        .gap(px(space::MD))
        .child(
            div()
                .w(px(LABEL_WIDTH))
                .flex_none()
                .flex()
                .flex_col()
                .child(div().child(label))
                .children(hint.map(|h| {
                    div()
                        .type_style(text::SUBHEADLINE)
                        .text_color(theme.tertiary)
                        .child(h)
                })),
        )
        .child(div().flex_1().flex().flex_row().child(control))
}

/// What a segmented control offers ([`SettingsWindow::choices`]).
struct Choices<'a, T> {
    id: &'static str,
    all: &'a [T],
    current: T,
    enabled: bool,
    label: fn(T) -> &'static str,
    selector: fn(T) -> String,
    pick: fn(&mut SettingsWindow, T, &mut Context<SettingsWindow>),
}

/// What a stepper shows ([`SettingsWindow::stepper`]).
struct Stepper {
    id: &'static str,
    value: String,
    can_down: bool,
    can_up: bool,
}

/// One segment or tab: filled when selected.
fn pill(
    id: impl Into<ElementId>,
    label: &'static str,
    selected: bool,
    theme: &Theme,
) -> Stateful<Div> {
    let theme = *theme;
    div()
        .id(id)
        .flex()
        .items_center()
        .justify_center()
        .h(px(chrome::CONTROL_HEIGHT - space::XS))
        .px(px(space::MD))
        .rounded(px(radius::MD))
        .type_style(text::CALLOUT)
        .cursor_pointer()
        .child(label)
        .map(|p| {
            if selected {
                p.bg(theme.accent).text_color(theme.on_accent)
            } else {
                p.border_1()
                    .border_color(theme.border)
                    .bg(theme.hover)
                    .hover(move |s| s.bg(theme.pressed))
            }
        })
}

/// An on/off switch: a Tab stop that Space toggles (ADR 0101).
fn switch(id: &'static str, on: bool, theme: &Theme) -> Stateful<Div> {
    let (w, h) = (32.0, 18.0);
    let knob = h - 4.0;
    tab_stop(div().id(id), theme)
        .debug_selector(move || format!("switch-{id}"))
        .w(px(w))
        .h(px(h))
        .flex_none()
        .rounded(px(h / 2.0))
        .cursor_pointer()
        .bg(if on { theme.accent } else { theme.pressed })
        .border_1()
        .border_color(theme.border)
        .child(
            div()
                .mt(px(1.0))
                .ml(px(if on { w - knob - 3.0 } else { 1.0 }))
                .size(px(knob))
                .rounded(px(knob / 2.0))
                .bg(gpui::white())
                .shadow_sm(),
        )
}

#[cfg(test)]
mod tests {
    use gpui::{Keystroke, TestAppContext};

    use crate::app::keymap::bind_keys;
    use crate::core::settings::SHORTCUTS;

    #[gpui::test]
    fn every_listed_shortcut_is_bound(cx: &mut TestAppContext) {
        cx.update(bind_keys);
        for s in SHORTCUTS {
            let ks = Keystroke::parse(s.keys).unwrap();
            let bound = cx.update(|cx| cx.all_bindings_for_input(&[ks]));
            assert!(!bound.is_empty(), "{} ({}) is not bound", s.keys, s.what);
        }
    }
}
