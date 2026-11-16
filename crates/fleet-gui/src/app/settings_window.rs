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
use super::controls::{ButtonKind, button, error_text};
use super::keymap::ClosePane;
use super::native;
use super::status_item::set_accessory;
use super::text_field::TextField;
use super::theme::{Theme, Typography, current as theme};
use crate::core::settings::{
    BLUR_MAX, BLUR_MIN, BLUR_STEP, Change, FONT_SIZE_MAX, FONT_SIZE_MIN, FONT_SIZE_STEP,
    MotionChoice, OPACITY_MAX, OPACITY_MIN, OPACITY_STEP, SHORTCUTS, Section, ThemeChoice,
    key_label, parse_command_line,
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
        let font_family = cx.new(|cx| TextField::new(c.font_family.clone(), "SF Mono", cx));
        let command =
            cx.new(|cx| TextField::new(shell_words::join(&c.default_command), "claude", cx));
        let folder =
            cx.new(|cx| TextField::new(c.default_dir.clone(), "The most recent folder", cx));
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
        let mut row = div()
            .flex()
            .flex_row()
            .justify_center()
            .gap(px(space::XS))
            .pb(px(space::MD))
            .border_b_1()
            .border_color(theme.separator);
        for section in Section::ALL {
            let selected = section == self.section;
            row = row.child(
                pill(("tab", section as usize), section.title(), selected, theme)
                    .debug_selector(move || format!("settings-tab-{}", section.title()))
                    .on_click(cx.listener(move |this, _, _, cx| this.show_section(section, cx))),
            );
        }
        row
    }

    fn appearance(&self, theme: &Theme, cx: &mut Context<Self>) -> Div {
        let c = self.app.read(cx).config().clone();
        // Reduce Transparency draws the window opaque: no blur, no tint.
        let solid = native::reduce_transparency();
        let solid_hint = solid.then_some("Off while Reduce Transparency is on in System Settings");
        let mut themes = segmented();
        for choice in ThemeChoice::ALL {
            themes = themes.child(
                pill(
                    ("theme", choice as usize),
                    choice.title(),
                    c.theme == choice,
                    theme,
                )
                .debug_selector(move || format!("theme-{}", choice.title()))
                .on_click(cx.listener(move |this, _, _, cx| {
                    this.change(Change::Theme(choice), cx);
                })),
            );
        }
        let blur = stepper(
            "blur",
            format!("{} pt", c.blur),
            !solid && c.blur > BLUR_MIN,
            !solid && c.blur < BLUR_MAX,
            theme,
            cx.listener(move |this, _, _, cx| this.change(Change::Blur(c.blur - BLUR_STEP), cx)),
            cx.listener(move |this, _, _, cx| this.change(Change::Blur(c.blur + BLUR_STEP), cx)),
        );
        let opacity = c.window_opacity;
        let opacity_stepper = stepper(
            "opacity",
            format!("{} %", (opacity * 100.0).round()),
            !solid && opacity > OPACITY_MIN,
            !solid && opacity < OPACITY_MAX,
            theme,
            cx.listener(move |this, _, _, cx| {
                this.change(Change::WindowOpacity(opacity - OPACITY_STEP), cx);
            }),
            cx.listener(move |this, _, _, cx| {
                this.change(Change::WindowOpacity(opacity + OPACITY_STEP), cx);
            }),
        );
        let size = c.font_size;
        let font_size = stepper(
            "font-size",
            format!("{size} pt"),
            size > FONT_SIZE_MIN,
            size < FONT_SIZE_MAX,
            theme,
            cx.listener(move |this, _, _, cx| {
                this.change(Change::FontSize(size - FONT_SIZE_STEP), cx);
            }),
            cx.listener(move |this, _, _, cx| {
                this.change(Change::FontSize(size + FONT_SIZE_STEP), cx);
            }),
        );
        section_body()
            .child(setting_row("Theme", None, themes, theme))
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
        let mut motion = segmented();
        for choice in MotionChoice::ALL {
            motion = motion.child(
                pill(
                    ("motion", choice as usize),
                    choice.title(),
                    c.reduce_motion == choice,
                    theme,
                )
                .debug_selector(move || format!("motion-{}", choice.title()))
                .on_click(cx.listener(move |this, _, _, cx| {
                    this.change(Change::ReduceMotion(choice), cx);
                })),
            );
        }
        let system_hint = if native::reduce_motion() {
            "System: Reduce Motion is on in System Settings"
        } else {
            "System: Reduce Motion is off in System Settings"
        };
        section_body()
            .child(setting_row("Motion", Some(system_hint), motion, theme))
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
                Some("Option+key sends Esc and the key to the terminal"),
                switch("option-as-meta", c.option_as_meta, theme).on_click(cx.listener(
                    move |this, _, _, cx| {
                        this.change(Change::OptionAsMeta(!c.option_as_meta), cx);
                    },
                )),
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
                button(
                    "choose-folder",
                    "Choose…",
                    theme,
                    ButtonKind::Secondary,
                    true,
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
        div()
            .id("settings")
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

/// A row of [`pill`]s on one well (a segmented control).
fn segmented() -> Div {
    div().flex().flex_row().gap(px(space::XXS))
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
                p.bg(theme.accent).text_color(gpui::white())
            } else {
                p.border_1()
                    .border_color(theme.border)
                    .bg(theme.hover)
                    .hover(move |s| s.bg(theme.pressed))
            }
        })
}

/// An on/off switch.
fn switch(id: &'static str, on: bool, theme: &Theme) -> Stateful<Div> {
    let (w, h) = (32.0, 18.0);
    let knob = h - 4.0;
    div()
        .id(id)
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

type Click = Box<dyn Fn(&gpui::ClickEvent, &mut Window, &mut App) + 'static>;

/// `−  value  +`; a button at the end of the range is dimmed.
fn stepper(
    id: &'static str,
    value: String,
    can_down: bool,
    can_up: bool,
    theme: &Theme,
    down: impl Fn(&gpui::ClickEvent, &mut Window, &mut App) + 'static,
    up: impl Fn(&gpui::ClickEvent, &mut Window, &mut App) + 'static,
) -> Div {
    let side = |suffix: &'static str, label: &'static str, enabled: bool, f: Click| {
        let b = button(
            SharedString::from(format!("{id}-{suffix}")),
            label,
            theme,
            ButtonKind::Secondary,
            enabled,
        )
        .w(px(chrome::CONTROL_HEIGHT))
        .debug_selector(move || format!("{id}-{suffix}"));
        if enabled { b.on_click(f) } else { b }
    };
    div()
        .flex()
        .flex_row()
        .items_center()
        .gap(px(space::SM))
        .child(side("down", "−", can_down, Box::new(down)))
        .child(
            div()
                .min_w(px(56.))
                .flex()
                .justify_center()
                .debug_selector(move || format!("{id}-value"))
                .child(SharedString::from(value)),
        )
        .child(side("up", "+", can_up, Box::new(up)))
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
