//! The connect sheet ("Connect to fleetd"), the startup flow and target
//! switching. The GUI never starts a daemon unless the user clicks "Start
//! fleetd on this Mac".

use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use gpui::{
    App, AppContext, Context, Entity, EventEmitter, Global, InteractiveElement, IntoElement,
    KeyDownEvent, MouseButton, ParentElement, Render, SharedString, StatefulInteractiveElement,
    Styled, Task, Window, div, px,
};

use super::app_state::{AppState, SessionApi};
use super::bench;
use super::new_agent::{TextField, button};
use super::theme::{Theme, current as theme};
use super::workspace::Workspace;
use crate::core::launch::{find_fleetd, start_local};
use crate::core::session::{CLIENT_NAME, Session, UiEvent};
use crate::core::target::{ConnectError, Connected, Startup, Target, connect, startup_action};

/// A startup connect this quick never shows the sheet.
pub const STARTUP_QUIET: Duration = Duration::from_millis(400);
const SPINNER: [&str; 10] = ["⠋", "⠙", "⠹", "⠸", "⠼", "⠴", "⠦", "⠧", "⠇", "⠏"];
const SPINNER_STEP: Duration = Duration::from_millis(80);

/// What the sheet needs from the outside world: real connections, or a
/// recording fake in tests. Everything but [`Backend::session`] blocks and
/// runs on the background executor.
pub trait Backend: Send + Sync {
    fn find_fleetd(&self) -> Option<PathBuf>;
    /// Connects to `target` and handshakes. The real backend keeps the
    /// connection for the next [`Backend::session`] to `target`.
    fn probe(&self, target: &Target) -> Result<(), ConnectError>;
    /// Closes a connection [`Backend::probe`] kept, if any (the attempt
    /// was abandoned).
    fn discard_probe(&self) {}
    /// Starts a local fleetd and waits until it answers.
    fn start_local(&self, fleetd: &Path) -> Result<(), ConnectError>;
    /// A session to `target` (main thread).
    fn session(&self, target: &Target, ui: async_channel::Sender<UiEvent>) -> Arc<dyn SessionApi>;
}

/// A probe's connection older than this is not reused (it may have been
/// cancelled and left idle); the session connects anew.
pub const PROBE_REUSE_WINDOW: Duration = Duration::from_secs(5);

/// The real thing, over `state_dir`.
pub struct RealBackend {
    pub state_dir: PathBuf,
    /// The last probe's connection, for the session that follows it: one
    /// handshake instead of two. Closed after [`PROBE_REUSE_WINDOW`] if no
    /// session takes it (dropping a [`Connected`] closes it and reaps its
    /// ssh child).
    probed: Arc<Mutex<Option<Probed>>>,
    /// Bumped by [`Backend::discard_probe`]: a probe that started before
    /// does not keep its connection.
    probe_epoch: AtomicU64,
}

struct Probed {
    target: Target,
    at: Instant,
    conn: Connected,
}

impl RealBackend {
    pub fn new(state_dir: PathBuf) -> Self {
        RealBackend {
            state_dir,
            probed: Arc::default(),
            probe_epoch: AtomicU64::new(0),
        }
    }

    /// Keeps `c` (to `target`) for the next session, unless the probe that
    /// made it (in `epoch`) was abandoned; drops the one kept before.
    fn keep(&self, target: &Target, c: Connected, epoch: u64) {
        let at = Instant::now();
        let old = {
            let mut probed = lock(&self.probed);
            if self.probe_epoch.load(Ordering::SeqCst) != epoch {
                return;
            }
            probed.replace(Probed {
                target: target.clone(),
                at,
                conn: c,
            })
        };
        drop(old);
        let probed = Arc::clone(&self.probed);
        let _ = std::thread::Builder::new()
            .name("gui-probe-expiry".into())
            .spawn(move || {
                std::thread::sleep(PROBE_REUSE_WINDOW);
                let mut slot = lock(&probed);
                if slot.as_ref().is_some_and(|p| p.at == at) {
                    let stale = slot.take();
                    drop(slot);
                    drop(stale);
                }
            });
    }

    /// The kept connection, if it is to `target` and recent; a stale one is
    /// dropped (closed).
    fn take_probed(&self, target: &Target) -> Option<Connected> {
        let p = lock(&self.probed).take()?;
        (p.target == *target && p.at.elapsed() < PROBE_REUSE_WINDOW).then_some(p.conn)
    }
}

fn lock<T>(m: &Mutex<T>) -> std::sync::MutexGuard<'_, T> {
    m.lock().unwrap_or_else(|e| e.into_inner())
}

impl Backend for RealBackend {
    fn find_fleetd(&self) -> Option<PathBuf> {
        find_fleetd()
    }

    fn probe(&self, target: &Target) -> Result<(), ConnectError> {
        let epoch = self.probe_epoch.load(Ordering::SeqCst);
        // Handshaken as the session's control connection, which it becomes.
        let c = connect(target, &self.state_dir, CLIENT_NAME)?;
        self.keep(target, c, epoch);
        Ok(())
    }

    fn discard_probe(&self) {
        let kept = {
            let mut probed = lock(&self.probed);
            self.probe_epoch.fetch_add(1, Ordering::SeqCst);
            probed.take()
        };
        drop(kept);
    }

    fn start_local(&self, fleetd: &Path) -> Result<(), ConnectError> {
        start_local(fleetd, &self.state_dir)
    }

    fn session(&self, target: &Target, ui: async_channel::Sender<UiEvent>) -> Arc<dyn SessionApi> {
        let first = self.take_probed(target);
        Arc::new(Session::start_with(
            target.clone(),
            self.state_dir.clone(),
            ui,
            first,
        ))
    }
}

struct BackendGlobal(Arc<dyn Backend>);

impl Global for BackendGlobal {}

pub fn install_backend(backend: Arc<dyn Backend>, cx: &mut App) {
    cx.set_global(BackendGlobal(backend));
}

fn backend(cx: &App) -> Option<Arc<dyn Backend>> {
    cx.try_global::<BackendGlobal>().map(|g| g.0.clone())
}

/// The remote field's target: `user@host` or `host` (an `ssh://` prefix
/// is allowed); `None` when empty, containing whitespace or starting with
/// `-` (an ssh option).
pub fn remote_target(text: &str) -> Option<Target> {
    let text = text.trim();
    let dest = text.strip_prefix("ssh://").unwrap_or(text);
    if dest.contains(char::is_whitespace) {
        return None;
    }
    format!("ssh://{dest}").parse().ok()
}

fn startup_label(saved: Option<&Target>) -> String {
    match saved {
        Some(t) => format!("Connecting to {}…", target_label(t)),
        None => "Looking for fleetd on this Mac…".to_owned(),
    }
}

/// The launch's connect: `saved`, else a local fleetd if one answers
/// (never starting one), else ask the user (`Err`, with the reason).
pub fn startup_work(b: &dyn Backend, saved: Option<Target>) -> Result<Target, Option<String>> {
    let local_answers = saved.is_none() && b.probe(&Target::Local).is_ok();
    match startup_action(saved.as_ref(), local_answers) {
        Startup::Connect(t) if local_answers => Ok(t),
        Startup::Connect(t) => b.probe(&t).map(|()| t).map_err(|e| Some(e.to_string())),
        Startup::AskUser { reason } => Err(reason),
    }
}

/// The launch's connect, started on its own thread before the window
/// opens so the two overlap; the connect sheet waits for its result.
pub struct StartupAttempt {
    saved: Option<Target>,
    result: async_channel::Receiver<Result<Target, Option<String>>>,
}

impl StartupAttempt {
    pub fn start(backend: Arc<dyn Backend>, saved: Option<Target>) -> Self {
        let (tx, rx) = async_channel::bounded(1);
        let target = saved.clone();
        let spawned = std::thread::Builder::new()
            .name("gui-startup".into())
            .spawn(move || {
                let _ = tx.send_blocking(startup_work(backend.as_ref(), target));
            });
        if let Err(e) = spawned {
            // The receiver then reports the attempt as failed.
            eprintln!("fleet-gui: cannot start the startup connect: {e}");
        }
        StartupAttempt { saved, result: rx }
    }

    /// Its result if it is already done, else the attempt.
    pub fn into_result(self) -> Result<Result<Target, Option<String>>, StartupAttempt> {
        match self.result.try_recv() {
            Ok(r) => Ok(r),
            Err(_) => Err(self),
        }
    }

    /// An attempt whose result is already known.
    pub fn finished(saved: Option<Target>, result: Result<Target, Option<String>>) -> Self {
        let (tx, rx) = async_channel::bounded(1);
        let _ = tx.try_send(result);
        StartupAttempt { saved, result: rx }
    }
}

/// How a target is named in the sheet.
pub fn target_label(target: &Target) -> String {
    match target {
        Target::Local => "fleetd on this Mac".to_owned(),
        Target::Ssh { dest } => dest.clone(),
    }
}

pub enum ConnectEvent {
    /// `target` answered; `save` makes it the saved connection.
    Connected { target: Target, save: bool },
    /// Closed without connecting (only offered while connected).
    Dismiss,
}

/// A connect on its way.
struct Busy {
    label: String,
    frame: usize,
    _work: Task<()>,
    _spinner: Task<()>,
}

pub struct ConnectSheet {
    backend: Arc<dyn Backend>,
    pub(crate) remote: Entity<TextField>,
    pub(crate) recents: Vec<Target>,
    /// `fleetd` to start; `None` disables "Start fleetd on this Mac".
    pub(crate) fleetd: Option<PathBuf>,
    /// Why the sheet shows, or why the last attempt failed.
    pub(crate) reason: Option<String>,
    /// Closing without connecting is offered (a fleetd is connected).
    pub(crate) dismissable: bool,
    /// A startup connect that has not yet taken [`STARTUP_QUIET`]: not drawn.
    pub(crate) quiet: bool,
    busy: Option<Busy>,
    _quiet_timer: Option<Task<()>>,
}

impl EventEmitter<ConnectEvent> for ConnectSheet {}

impl ConnectSheet {
    pub fn new(
        backend: Arc<dyn Backend>,
        reason: Option<String>,
        recents: Vec<Target>,
        dismissable: bool,
        cx: &mut Context<Self>,
    ) -> Self {
        let fleetd = backend.find_fleetd();
        ConnectSheet {
            backend,
            remote: cx.new(|cx| TextField::new(String::new(), "user@host", cx)),
            recents,
            fleetd,
            reason,
            dismissable,
            quiet: false,
            busy: None,
            _quiet_timer: None,
        }
    }

    pub fn focus(&self, window: &mut Window, cx: &App) {
        window.focus(self.remote.read(cx).focus_handle());
    }

    pub fn is_busy(&self) -> bool {
        self.busy.is_some()
    }

    pub fn busy_label(&self) -> Option<&str> {
        self.busy.as_ref().map(|b| b.label.as_str())
    }

    /// The remote field holds a target: Connect is enabled.
    pub fn remote_target(&self, cx: &App) -> Option<Target> {
        remote_target(self.remote.read(cx).text())
    }

    /// Runs `work` on the background executor; its `Ok` target is
    /// emitted as connected, its `Err` shown as the reason.
    fn run<F>(&mut self, label: String, save: bool, work: F, cx: &mut Context<Self>)
    where
        F: FnOnce(&dyn Backend) -> Result<Target, Option<String>> + Send + 'static,
    {
        let backend = self.backend.clone();
        let bg = cx
            .background_executor()
            .spawn(async move { work(backend.as_ref()) });
        self.run_future(label, save, bg, cx);
    }

    /// Waits for `result` (a connect on its way) as [`ConnectSheet::run`]
    /// does. Dropping it (Cancel) drops the result; [`ConnectSheet::cancel`]
    /// also discards the probe's kept connection.
    fn run_future(
        &mut self,
        label: String,
        save: bool,
        result: impl Future<Output = Result<Target, Option<String>>> + 'static,
        cx: &mut Context<Self>,
    ) {
        let bg = result;
        let work = cx.spawn(async move |this, cx| {
            let result = bg.await;
            let _ = this.update(cx, |s, cx| s.finished(result, save, cx));
        });
        let spinner = cx.spawn(async move |this, cx| {
            loop {
                cx.background_executor().timer(SPINNER_STEP).await;
                let ok = this.update(cx, |s, cx| {
                    if let Some(b) = &mut s.busy {
                        b.frame = (b.frame + 1) % SPINNER.len();
                        cx.notify();
                    }
                });
                if ok.is_err() {
                    break;
                }
            }
        });
        self.busy = Some(Busy {
            label,
            frame: 0,
            _work: work,
            _spinner: spinner,
        });
        cx.notify();
    }

    fn finished(
        &mut self,
        result: Result<Target, Option<String>>,
        save: bool,
        cx: &mut Context<Self>,
    ) {
        self.busy = None;
        self.quiet = false;
        self._quiet_timer = None;
        match result {
            Ok(target) => cx.emit(ConnectEvent::Connected { target, save }),
            Err(reason) => {
                if bench::active() {
                    bench::connect_failed(reason.as_deref());
                }
                if let Some(r) = &reason {
                    eprintln!("fleet-gui: connect sheet: {r}");
                } else {
                    eprintln!("fleet-gui: connect sheet: no saved connection, no local fleetd");
                }
                self.reason = reason;
            }
        }
        cx.notify();
    }

    /// The launch: `saved` (`--target`, else `[gui] connection`), else a
    /// local fleetd if one answers, else ask. The sheet stays hidden for
    /// [`STARTUP_QUIET`] or until it has to ask.
    pub fn startup(&mut self, saved: Option<Target>, cx: &mut Context<Self>) {
        self.arm_quiet(cx);
        let label = startup_label(saved.as_ref());
        self.run(label, false, move |b| startup_work(b, saved), cx);
    }

    /// [`ConnectSheet::startup`] for an attempt that started before the
    /// window opened ([`StartupAttempt::start`]).
    pub fn startup_from(&mut self, attempt: StartupAttempt, cx: &mut Context<Self>) {
        self.arm_quiet(cx);
        let label = startup_label(attempt.saved.as_ref());
        let rx = attempt.result;
        let result = async move {
            rx.recv()
                .await
                .unwrap_or_else(|_| Err(Some("the startup connect thread failed".to_owned())))
        };
        self.run_future(label, false, result, cx);
    }

    /// Hides the sheet for [`STARTUP_QUIET`]. A bench run never shows it:
    /// it stays quiet, and a failure exits.
    fn arm_quiet(&mut self, cx: &mut Context<Self>) {
        self.quiet = true;
        self._quiet_timer = (!bench::active()).then(|| {
            cx.spawn(async move |this, cx| {
                cx.background_executor().timer(STARTUP_QUIET).await;
                let _ = this.update(cx, |s, cx| {
                    s.quiet = false;
                    cx.notify();
                });
            })
        });
    }

    /// "Start fleetd on this Mac": the only way the GUI starts a daemon.
    pub fn start_local(&mut self, cx: &mut Context<Self>) {
        let Some(fleetd) = self.fleetd.clone() else {
            return;
        };
        if self.busy.is_some() {
            return;
        }
        self.run(
            "Starting fleetd…".to_owned(),
            true,
            move |b| {
                b.start_local(&fleetd)
                    .map(|()| Target::Local)
                    .map_err(|e| Some(e.to_string()))
            },
            cx,
        );
    }

    /// Connects to the remote field's target, if it holds one.
    pub fn connect_remote(&mut self, cx: &mut Context<Self>) {
        if let Some(t) = self.remote_target(cx) {
            self.connect_to(t, cx);
        }
    }

    pub fn connect_to(&mut self, target: Target, cx: &mut Context<Self>) {
        if self.busy.is_some() {
            return;
        }
        let label = format!("Connecting to {}…", target_label(&target));
        self.run(
            label,
            true,
            move |b| {
                b.probe(&target)
                    .map(|()| target)
                    .map_err(|e| Some(e.to_string()))
            },
            cx,
        );
    }

    /// Abandons the attempt on its way; a late success is dropped, and the
    /// connection its probe keeps is closed.
    pub fn cancel(&mut self, cx: &mut Context<Self>) {
        if self.busy.take().is_some() {
            self.backend.discard_probe();
            self.quiet = false;
            self._quiet_timer = None;
            cx.notify();
        } else if self.dismissable {
            cx.emit(ConnectEvent::Dismiss);
        }
    }

    fn key_down(&mut self, ev: &KeyDownEvent, _window: &mut Window, cx: &mut Context<Self>) {
        match ev.keystroke.key.as_str() {
            "enter" => {
                self.connect_remote(cx);
                cx.stop_propagation();
            }
            "escape" => {
                self.cancel(cx);
                cx.stop_propagation();
            }
            _ => {}
        }
    }
}

/// A button that does nothing while `enabled` is false.
fn action_button(
    id: &'static str,
    label: &'static str,
    theme: &Theme,
    primary: bool,
    enabled: bool,
) -> gpui::Stateful<gpui::Div> {
    let b = button(id, label, theme, primary);
    if enabled {
        b
    } else {
        b.text_color(theme.dim_text)
            .border_color(theme.border)
            .cursor_default()
    }
}

impl Render for ConnectSheet {
    fn render(&mut self, _window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let theme = theme(cx);
        let idle = self.busy.is_none();
        let label = |text: &'static str| {
            div()
                .text_size(px(12.))
                .text_color(theme.dim_text)
                .child(text)
        };
        let start_enabled = idle && self.fleetd.is_some();
        let mut start = action_button(
            "start-local",
            "Start fleetd on this Mac",
            &theme,
            true,
            start_enabled,
        )
        .debug_selector(|| "connect-start-local".to_owned());
        if start_enabled {
            start = start.on_click(cx.listener(|this, _, _, cx| this.start_local(cx)));
        }
        let mut local = div().flex().flex_col().gap(px(4.)).child(start);
        if self.fleetd.is_none() {
            local = local.child(
                div()
                    .text_size(px(12.))
                    .text_color(theme.dim_text)
                    .child("fleetd not found next to the app or on PATH"),
            );
        }
        let connect_enabled = idle && self.remote_target(cx).is_some();
        let mut connect = action_button("connect-remote", "Connect", &theme, true, connect_enabled)
            .debug_selector(|| "connect-remote".to_owned());
        if connect_enabled {
            connect = connect.on_click(cx.listener(|this, _, _, cx| this.connect_remote(cx)));
        }
        let remote_row = div()
            .flex()
            .flex_row()
            .gap(px(8.))
            .child(div().flex_1().child(self.remote.clone()))
            .child(connect);
        let recents = (!self.recents.is_empty()).then(|| {
            div()
                .id("recent-targets")
                .flex()
                .flex_col()
                .max_h(px(160.))
                .overflow_y_scroll()
                .children(self.recents.iter().enumerate().map(|(i, t)| {
                    let target = t.clone();
                    let row = div()
                        .id(("recent-target", i))
                        .px(px(6.))
                        .py(px(3.))
                        .rounded(px(4.))
                        .text_size(px(12.))
                        .truncate()
                        .child(SharedString::from(target_label(t)));
                    if idle {
                        row.cursor_pointer()
                            .hover(|s| s.bg(theme.bg()))
                            .on_click(cx.listener(move |this, _, _, cx| {
                                this.connect_to(target.clone(), cx);
                            }))
                    } else {
                        row.text_color(theme.dim_text)
                    }
                }))
        });
        let status = self.busy.as_ref().map(|b| {
            div()
                .flex()
                .flex_row()
                .items_center()
                .gap(px(8.))
                .child(div().text_color(theme.accent).child(SPINNER[b.frame]))
                .child(
                    div()
                        .flex_1()
                        .text_color(theme.dim_text)
                        .child(SharedString::from(b.label.clone())),
                )
        });
        let reason = self.reason.clone().filter(|_| idle).map(|r| {
            div()
                .debug_selector(|| "connect-reason".to_owned())
                .text_color(theme.resolve(fleet_term::Color::Named(1), true))
                .child(r)
        });
        let cancel = (!idle || self.dismissable).then(|| {
            div().flex().flex_row().justify_end().child(
                button("connect-cancel", "Cancel", &theme, false)
                    .debug_selector(|| "connect-cancel".to_owned())
                    .on_click(cx.listener(|this, _, _, cx| this.cancel(cx))),
            )
        });
        div()
            .id("connect-sheet")
            .debug_selector(|| "connect-sheet".to_owned())
            .on_key_down(cx.listener(Self::key_down))
            .occlude()
            .on_mouse_down(MouseButton::Left, |_, _, cx| cx.stop_propagation())
            .w(px(440.))
            .flex()
            .flex_col()
            .gap(px(10.))
            .p(px(18.))
            .rounded(px(10.))
            .border_1()
            .border_color(theme.border)
            .bg(theme.header_bg)
            .text_color(theme.fg())
            .text_size(px(13.))
            .child(div().text_size(px(15.)).child("Connect to fleetd"))
            .children(reason)
            .child(local)
            .child(label("Connect to a remote fleetd"))
            .child(remote_row)
            .children(recents.map(|r| {
                div()
                    .flex()
                    .flex_col()
                    .gap(px(4.))
                    .child(label("Recent"))
                    .child(r)
            }))
            .children(status)
            .children(cancel)
    }
}

/// Opens the connect sheet over `window`'s workspace (or brings it up),
/// showing `reason` when given. Not from inside the workspace's own update.
pub fn show_connect_sheet(reason: Option<String>, window: &mut Window, cx: &mut App) {
    if let Some(Some(ws)) = window.root::<Workspace>() {
        ws.update(cx, |ws, cx| ws.open_connect_sheet(reason, window, cx));
    }
}

/// Shows the workspace window with the connect sheet ("Switch fleetd…").
pub fn switch_fleetd(cx: &mut App) {
    super::workspace::open_main_window(cx);
    let Some(handle) = AppState::global(cx).and_then(|a| a.read(cx).window) else {
        return;
    };
    let _ = handle.update(cx, |ws, window, cx| ws.open_connect_sheet(None, window, cx));
}

/// Connects the app to `target`: the old session detaches everything, a
/// new one starts and the target's saved layout is restored.
pub fn switch_target(target: Target, cx: &mut App) {
    let (Some(app), Some(backend)) = (AppState::global(cx), backend(cx)) else {
        return;
    };
    if app.read(cx).is_live() && *app.read(cx).target() == target {
        // Already connected there: keep the session and every pane.
        backend.discard_probe();
        return;
    }
    eprintln!("fleet-gui: connected to {target}");
    let (ui_tx, ui_rx) = async_channel::unbounded();
    let session = backend.session(&target, ui_tx);
    app.update(cx, |a, cx| a.replace_session(session, ui_rx, target, cx));
    bench::connected(cx);
}

/// The launch flow over the open workspace window, waiting for `attempt`.
pub fn begin_startup(attempt: StartupAttempt, cx: &mut App) {
    let Some(handle) = AppState::global(cx).and_then(|a| a.read(cx).window) else {
        return;
    };
    let _ = handle.update(cx, |ws, window, cx| ws.start_up_from(attempt, window, cx));
}

pub(super) fn new_sheet(reason: Option<String>, cx: &mut App) -> Option<Entity<ConnectSheet>> {
    let backend = backend(cx)?;
    let app = AppState::global(cx)?;
    let (recents, live) = {
        let a = app.read(cx);
        (a.gui_state().recent_targets.clone(), a.is_live())
    };
    Some(cx.new(|cx| ConnectSheet::new(backend, reason, recents, live, cx)))
}

#[cfg(test)]
mod tests {
    use std::collections::HashMap;
    use std::path::{Path, PathBuf};
    use std::sync::{Arc, Mutex};

    use fleet_term::Palette;
    use gpui::{AppContext, Entity, Modifiers, TestAppContext, VisualTestContext};

    use super::{
        Backend, ConnectSheet, StartupAttempt, install_backend, remote_target, startup_work,
        switch_target,
    };
    use crate::app::app_state::{AppState, SessionApi};
    use crate::app::keymap::bind_keys;
    use crate::app::pane_view::InputSink;
    use crate::app::theme::Theme;
    use crate::app::workspace::Workspace;
    use crate::core::config::{GuiConfig, load_config};
    use crate::core::layout::{Axis, Layout};
    use crate::core::pane::PaneModel;
    use crate::core::session::{AgentRow, PaneId, UiEvent};
    use crate::core::state::GuiState;
    use crate::core::target::{ConnectError, Target};

    #[derive(Clone, Debug, PartialEq)]
    enum Call {
        Probe(Target),
        StartLocal(PathBuf),
        Session(Target),
        Shutdown(Target),
        Open(Target, PaneId, Option<u64>),
        DiscardProbe,
    }

    #[derive(Default)]
    struct FakeBackend {
        calls: Arc<Mutex<Vec<Call>>>,
        fleetd: Option<PathBuf>,
        /// Probe results by target; missing means `NotRunning`.
        answers: Mutex<HashMap<Target, Result<(), ConnectError>>>,
    }

    impl FakeBackend {
        fn calls(&self) -> Vec<Call> {
            self.calls.lock().unwrap().clone()
        }

        fn answer(&self, t: Target, r: Result<(), ConnectError>) {
            self.answers.lock().unwrap().insert(t, r);
        }

        fn started_daemon(&self) -> bool {
            self.calls()
                .iter()
                .any(|c| matches!(c, Call::StartLocal(_)))
        }
    }

    impl Backend for FakeBackend {
        fn find_fleetd(&self) -> Option<PathBuf> {
            self.fleetd.clone()
        }

        fn probe(&self, target: &Target) -> Result<(), ConnectError> {
            self.calls.lock().unwrap().push(Call::Probe(target.clone()));
            self.answers
                .lock()
                .unwrap()
                .get(target)
                .cloned()
                .unwrap_or(Err(ConnectError::NotRunning))
        }

        fn discard_probe(&self) {
            self.calls.lock().unwrap().push(Call::DiscardProbe);
        }

        fn start_local(&self, fleetd: &Path) -> Result<(), ConnectError> {
            self.calls
                .lock()
                .unwrap()
                .push(Call::StartLocal(fleetd.to_owned()));
            self.answer(Target::Local, Ok(()));
            Ok(())
        }

        fn session(
            &self,
            target: &Target,
            _ui: async_channel::Sender<UiEvent>,
        ) -> Arc<dyn SessionApi> {
            self.calls
                .lock()
                .unwrap()
                .push(Call::Session(target.clone()));
            Arc::new(FakeSession {
                target: target.clone(),
                calls: self.calls.clone(),
            })
        }
    }

    struct FakeSession {
        target: Target,
        calls: Arc<Mutex<Vec<Call>>>,
    }

    impl InputSink for FakeSession {
        fn input(&self, _pane: PaneId, _bytes: Vec<u8>) {}
        fn resize(&self, _pane: PaneId, _cols: u16, _rows: u16) {}
    }

    impl SessionApi for FakeSession {
        fn agents(&self) -> Vec<AgentRow> {
            Vec::new()
        }

        fn spawn(
            &self,
            _program: String,
            _args: Vec<String>,
            _cwd: String,
            _cols: u16,
            _rows: u16,
            _pane: PaneId,
        ) -> Result<(), String> {
            Ok(())
        }

        fn kill(&self, _agent_id: u64) {}
        fn restart(&self, _agent_id: u64) {}

        fn restart_in_pane(
            &self,
            _pane: PaneId,
            _agent_id: u64,
            _cols: u16,
            _rows: u16,
        ) -> Result<(), String> {
            Ok(())
        }

        fn forget(&self, _ids: Vec<u64>) {}
        fn clear_exited(&self) {}

        fn replaced(&self) -> std::collections::HashMap<u64, u64> {
            std::collections::HashMap::new()
        }

        fn open_pane(&self, pane: PaneId, agent_id: Option<u64>) -> Arc<Mutex<PaneModel>> {
            self.calls
                .lock()
                .unwrap()
                .push(Call::Open(self.target.clone(), pane, agent_id));
            Arc::new(Mutex::new(PaneModel::new(Theme::dark().palette)))
        }

        fn attach(&self, _pane: PaneId, _agent_id: u64) {}
        fn close_pane(&self, _pane: PaneId) {}
        fn set_palette(&self, _palette: Palette) {}

        fn shutdown(&self) {
            self.calls
                .lock()
                .unwrap()
                .push(Call::Shutdown(self.target.clone()));
        }
    }

    struct Setup<'a> {
        ws: Entity<Workspace>,
        app: Entity<AppState>,
        cx: &'a mut VisualTestContext,
    }

    /// An unconnected app over `dir`, with its window open, as at launch.
    fn setup<'a>(cx: &'a mut TestAppContext, backend: &Arc<FakeBackend>, dir: &Path) -> Setup<'a> {
        cx.update(bind_keys);
        let b: Arc<dyn Backend> = backend.clone();
        cx.update(|cx| install_backend(b, cx));
        let dir = dir.to_owned();
        let app = cx.new(|cx| {
            AppState::without_session(Target::Local, dir, GuiConfig::default(), Theme::dark(), cx)
        });
        cx.update(|cx| AppState::install(app.clone(), cx));
        let state = app.clone();
        let (ws, cx) = cx.add_window_view(|window, cx| Workspace::new(state, window, cx));
        let handle = cx.update(|window, _| window.window_handle().downcast::<Workspace>().unwrap());
        app.update(cx, |a, _| a.window = Some(handle));
        cx.update(|window, _| window.activate_window());
        cx.run_until_parked();
        Setup { ws, app, cx }
    }

    fn startup(s: &mut Setup<'_>, saved: Option<Target>) {
        // As `begin_startup` does, from inside the window.
        s.ws.update_in(s.cx, |ws, window, cx| ws.start_up(saved, window, cx));
        s.cx.run_until_parked();
    }

    fn sheet(s: &mut Setup<'_>) -> Option<Entity<ConnectSheet>> {
        s.ws.read_with(s.cx, |w, _| w.connect_sheet().cloned())
    }

    /// `debug_bounds` entries outlive their elements, so whether the
    /// sheet is drawn is asked of the workspace.
    fn shown(s: &mut Setup<'_>) -> bool {
        s.cx.run_until_parked();
        s.ws.read_with(s.cx, |w, cx| w.connect_sheet_shown(cx))
    }

    fn click(s: &mut Setup<'_>, selector: &'static str) {
        s.cx.run_until_parked();
        let at =
            s.cx.debug_bounds(selector)
                .unwrap_or_else(|| panic!("{selector} is drawn"))
                .center();
        s.cx.simulate_click(at, Modifiers::none());
        s.cx.run_until_parked();
    }

    #[gpui::test]
    fn connect_sheet_is_as_tall_as_its_content(cx: &mut TestAppContext) {
        let dir = tempfile::tempdir().unwrap();
        let backend = Arc::new(FakeBackend::default());
        let mut s = setup(cx, &backend, dir.path());
        startup(&mut s, None);
        assert!(shown(&mut s));
        let sheet = s.cx.debug_bounds("connect-sheet").unwrap();
        let window = s.cx.update(|window, _| window.viewport_size());
        // A card under the top, not a column down the whole window.
        assert!(
            sheet.size.height < gpui::px(320.),
            "sheet height {:?}",
            sheet.size.height
        );
        assert!(sheet.bottom() < window.height);
    }

    #[gpui::test]
    fn no_saved_target_and_no_local_daemon_shows_sheet(cx: &mut TestAppContext) {
        let dir = tempfile::tempdir().unwrap();
        let backend = Arc::new(FakeBackend {
            fleetd: Some("/x/fleetd".into()),
            ..Default::default()
        });
        let mut s = setup(cx, &backend, dir.path());
        startup(&mut s, None);
        assert!(shown(&mut s));
        assert!(s.cx.debug_bounds("connect-sheet").is_some());
        let sheet = sheet(&mut s).unwrap();
        let (reason, busy) = sheet.read_with(s.cx, |sh, _| (sh.reason.clone(), sh.is_busy()));
        assert_eq!(reason, None);
        assert!(!busy);
        assert_eq!(backend.calls(), [Call::Probe(Target::Local)]);
        // Asking is all it does: no daemon was started, no session made.
        assert!(!backend.started_daemon());
        assert!(!s.app.read_with(s.cx, |a, _| a.is_live()));
    }

    #[gpui::test]
    fn local_daemon_answering_connects_without_the_sheet(cx: &mut TestAppContext) {
        let dir = tempfile::tempdir().unwrap();
        let backend = Arc::new(FakeBackend::default());
        backend.answer(Target::Local, Ok(()));
        let mut s = setup(cx, &backend, dir.path());
        s.ws.update_in(s.cx, |ws, window, cx| ws.start_up(None, window, cx));
        // On its way, the attempt is quiet: nothing is drawn yet.
        let quiet = s.ws.read_with(s.cx, |w, cx| {
            (w.connect_sheet().is_some(), w.connect_sheet_shown(cx))
        });
        assert_eq!(quiet, (true, false));
        s.cx.run_until_parked();
        assert!(!shown(&mut s));
        assert!(sheet(&mut s).is_none());
        assert!(s.app.read_with(s.cx, |a, _| a.is_live()));
        assert!(backend.calls().contains(&Call::Session(Target::Local)));
        // Not saved: no connection was chosen.
        assert_eq!(load_config(dir.path()).unwrap().connection, None);
    }

    /// As `begin_startup` does, for an attempt started before the window.
    fn startup_from(s: &mut Setup<'_>, attempt: StartupAttempt) {
        s.ws.update_in(s.cx, |ws, window, cx| ws.start_up_from(attempt, window, cx));
        s.cx.run_until_parked();
    }

    #[gpui::test]
    fn early_attempt_that_connected_starts_the_session(cx: &mut TestAppContext) {
        let dir = tempfile::tempdir().unwrap();
        let backend = Arc::new(FakeBackend::default());
        let mut s = setup(cx, &backend, dir.path());
        let attempt = StartupAttempt::finished(Some(Target::Local), Ok(Target::Local));
        startup_from(&mut s, attempt);
        assert!(!shown(&mut s));
        assert!(sheet(&mut s).is_none());
        assert!(s.app.read_with(s.cx, |a, _| a.is_live()));
        // The sheet probes nothing itself: the attempt did.
        assert!(!backend.calls().iter().any(|c| matches!(c, Call::Probe(_))));
        assert!(backend.calls().contains(&Call::Session(Target::Local)));
        assert!(!backend.started_daemon());
    }

    #[gpui::test]
    fn early_attempt_that_failed_shows_the_sheet(cx: &mut TestAppContext) {
        let dir = tempfile::tempdir().unwrap();
        let backend = Arc::new(FakeBackend::default());
        let mut s = setup(cx, &backend, dir.path());
        let attempt = StartupAttempt::finished(
            Some(Target::Local),
            Err(Some("fleetd is not running".into())),
        );
        startup_from(&mut s, attempt);
        assert!(shown(&mut s));
        let sheet = sheet(&mut s).unwrap();
        let reason = sheet.read_with(s.cx, |sh, _| sh.reason.clone());
        assert_eq!(reason.as_deref(), Some("fleetd is not running"));
        assert!(backend.calls().is_empty());
        assert!(!backend.started_daemon());
        assert!(!s.app.read_with(s.cx, |a, _| a.is_live()));
    }

    #[test]
    fn startup_work_never_starts_a_daemon() {
        let backend = FakeBackend {
            fleetd: Some("/x/fleetd".into()),
            ..Default::default()
        };
        assert_eq!(startup_work(&backend, None), Err(None));
        backend.answer(Target::Local, Ok(()));
        assert_eq!(startup_work(&backend, None), Ok(Target::Local));
        assert_eq!(
            startup_work(&backend, Some(Target::Local)),
            Ok(Target::Local)
        );
        assert!(!backend.started_daemon());
    }

    #[gpui::test]
    fn saved_target_failure_shows_sheet_with_reason(cx: &mut TestAppContext) {
        let dir = tempfile::tempdir().unwrap();
        let backend = Arc::new(FakeBackend::default());
        let saved = Target::Ssh {
            dest: "me@box".into(),
        };
        let err = ConnectError::FleetdNotFound { host: "box".into() };
        let display = err.to_string();
        backend.answer(saved.clone(), Err(err));
        // A local daemon answering does not matter: the saved target wins.
        backend.answer(Target::Local, Ok(()));
        let mut s = setup(cx, &backend, dir.path());
        startup(&mut s, Some(saved.clone()));
        assert!(shown(&mut s));
        let sheet = sheet(&mut s).unwrap();
        let reason = sheet.read_with(s.cx, |sh, _| sh.reason.clone());
        assert_eq!(reason.as_deref(), Some(display.as_str()));
        assert_eq!(reason.as_deref(), Some("fleetd is not installed on box"));
        assert!(s.cx.debug_bounds("connect-reason").is_some());
        assert_eq!(backend.calls(), [Call::Probe(saved)]);
        assert!(!backend.started_daemon());
    }

    #[gpui::test]
    fn start_locally_saves_connection_local(cx: &mut TestAppContext) {
        let dir = tempfile::tempdir().unwrap();
        let backend = Arc::new(FakeBackend {
            fleetd: Some("/x/fleetd".into()),
            ..Default::default()
        });
        let mut s = setup(cx, &backend, dir.path());
        startup(&mut s, None);
        assert!(!backend.started_daemon());
        click(&mut s, "connect-start-local");
        assert!(
            backend
                .calls()
                .contains(&Call::StartLocal("/x/fleetd".into()))
        );
        assert_eq!(
            load_config(dir.path()).unwrap().connection,
            Some(Target::Local)
        );
        // The workspace shows, connected.
        assert!(sheet(&mut s).is_none());
        assert!(!shown(&mut s));
        assert!(s.app.read_with(s.cx, |a, _| a.is_live()));
        assert_eq!(
            backend.calls().last(),
            Some(&Call::Open(Target::Local, PaneId(1), None))
        );
        assert!(s.cx.debug_bounds("pane-card-1").is_some());
    }

    #[gpui::test]
    fn start_is_disabled_without_fleetd(cx: &mut TestAppContext) {
        let dir = tempfile::tempdir().unwrap();
        let backend = Arc::new(FakeBackend::default());
        let mut s = setup(cx, &backend, dir.path());
        startup(&mut s, None);
        click(&mut s, "connect-start-local");
        assert!(!backend.started_daemon());
        let sheet = sheet(&mut s).unwrap();
        assert!(sheet.read_with(s.cx, |sh, _| sh.fleetd.is_none()));
    }

    #[gpui::test]
    fn remote_requires_user_at_host(cx: &mut TestAppContext) {
        assert_eq!(remote_target(""), None);
        assert_eq!(remote_target("   "), None);
        assert_eq!(remote_target("me @box"), None);
        assert_eq!(remote_target("-oProxyCommand=x"), None);
        assert_eq!(remote_target("ssh://-p"), None);
        let dir = tempfile::tempdir().unwrap();
        let backend = Arc::new(FakeBackend::default());
        let box_ = Target::Ssh {
            dest: "me@box".into(),
        };
        backend.answer(box_.clone(), Ok(()));
        let mut s = setup(cx, &backend, dir.path());
        startup(&mut s, None);
        let sheet = sheet(&mut s).unwrap();
        let field = sheet.read_with(s.cx, |sh, _| sh.remote.clone());
        for text in ["", "   "] {
            field.update(s.cx, |f, cx| f.set_text(text.into(), cx));
            assert_eq!(sheet.read_with(s.cx, |sh, cx| sh.remote_target(cx)), None);
            click(&mut s, "connect-remote");
            s.cx.simulate_keystrokes("enter");
            s.cx.run_until_parked();
            assert_eq!(backend.calls(), [Call::Probe(Target::Local)], "{text:?}");
        }
        field.update(s.cx, |f, cx| f.set_text(" me@box ".into(), cx));
        assert_eq!(
            sheet.read_with(s.cx, |sh, cx| sh.remote_target(cx)),
            Some(box_.clone())
        );
        click(&mut s, "connect-remote");
        assert_eq!(
            backend.calls()[1..3],
            [Call::Probe(box_.clone()), Call::Session(box_.clone())]
        );
        assert_eq!(
            load_config(dir.path()).unwrap().connection,
            Some(box_.clone())
        );
        assert_eq!(s.app.read_with(s.cx, |a, _| a.target().clone()), box_);
    }

    #[gpui::test]
    fn failed_attempt_is_shown_inline(cx: &mut TestAppContext) {
        let dir = tempfile::tempdir().unwrap();
        let backend = Arc::new(FakeBackend::default());
        let box_ = Target::Ssh { dest: "box".into() };
        backend.answer(
            box_.clone(),
            Err(ConnectError::Ssh {
                message: "Permission denied (publickey).".into(),
            }),
        );
        let mut s = setup(cx, &backend, dir.path());
        startup(&mut s, None);
        let sheet = sheet(&mut s).unwrap();
        sheet.update(s.cx, |sh, cx| sh.connect_to(box_, cx));
        s.cx.run_until_parked();
        let reason = sheet.read_with(s.cx, |sh, _| sh.reason.clone());
        assert_eq!(
            reason.as_deref(),
            Some("ssh: Permission denied (publickey).")
        );
        assert!(shown(&mut s));
        assert!(!s.app.read_with(s.cx, |a, _| a.is_live()));
    }

    #[gpui::test]
    fn cancel_drops_a_late_success(cx: &mut TestAppContext) {
        let dir = tempfile::tempdir().unwrap();
        let backend = Arc::new(FakeBackend::default());
        let box_ = Target::Ssh { dest: "box".into() };
        backend.answer(box_.clone(), Ok(()));
        let mut s = setup(cx, &backend, dir.path());
        startup(&mut s, None);
        let sheet = sheet(&mut s).unwrap();
        sheet.update(s.cx, |sh, cx| {
            sh.connect_to(box_.clone(), cx);
            assert!(sh.is_busy());
            sh.cancel(cx);
        });
        s.cx.run_until_parked();
        assert!(!sheet.read_with(s.cx, |sh, _| sh.is_busy()));
        assert!(shown(&mut s));
        assert!(
            !backend
                .calls()
                .iter()
                .any(|c| matches!(c, Call::Session(_)))
        );
        assert!(!s.app.read_with(s.cx, |a, _| a.is_live()));
        assert_eq!(load_config(dir.path()).unwrap().connection, None);
        // The probe's connection, kept for the session, is closed.
        assert!(backend.calls().contains(&Call::DiscardProbe));
    }

    #[gpui::test]
    fn config_error_stays_shown_after_the_first_connect(cx: &mut TestAppContext) {
        let dir = tempfile::tempdir().unwrap();
        let backend = Arc::new(FakeBackend::default());
        let s = setup(cx, &backend, dir.path());
        let msg = "config.toml line 2: expected a value".to_owned();
        s.app
            .update(s.cx, |a, cx| a.set_config_error(msg.clone(), cx));
        assert_eq!(
            s.app.read_with(s.cx, |a, _| a.error().map(str::to_owned)),
            Some(msg.clone())
        );
        s.cx.update(|_, cx| switch_target(Target::Local, cx));
        assert_eq!(
            s.app.read_with(s.cx, |a, _| a.error().map(str::to_owned)),
            Some(msg)
        );
        // A later switch starts clean.
        s.cx.update(|_, cx| switch_target(Target::Ssh { dest: "box".into() }, cx));
        assert_eq!(
            s.app.read_with(s.cx, |a, _| a.error().map(str::to_owned)),
            None
        );
    }

    #[gpui::test]
    fn switching_to_the_connected_target_keeps_the_session(cx: &mut TestAppContext) {
        let dir = tempfile::tempdir().unwrap();
        let backend = Arc::new(FakeBackend::default());
        let s = setup(cx, &backend, dir.path());
        s.cx.update(|_, cx| switch_target(Target::Local, cx));
        let before = backend.calls();
        s.cx.update(|_, cx| switch_target(Target::Local, cx));
        s.cx.run_until_parked();
        assert_eq!(backend.calls()[before.len()..], [Call::DiscardProbe]);
        assert!(s.app.read_with(s.cx, |a, _| a.is_live()));
    }

    #[gpui::test]
    fn recent_targets_listed_most_recent_first(cx: &mut TestAppContext) {
        let dir = tempfile::tempdir().unwrap();
        let a = Target::Ssh { dest: "a".into() };
        let b = Target::Ssh { dest: "b".into() };
        let state = GuiState {
            recent_targets: vec![Target::Local],
            ..GuiState::default()
        };
        state.save(dir.path()).unwrap();
        let backend = Arc::new(FakeBackend::default());
        let mut s = setup(cx, &backend, dir.path());
        for t in [&a, &b, &a] {
            let t = t.clone();
            s.cx.update(|_, cx| switch_target(t, cx));
        }
        s.cx.update(|window, cx| super::show_connect_sheet(None, window, cx));
        assert!(shown(&mut s));
        let sheet = sheet(&mut s).unwrap();
        let recents = sheet.read_with(s.cx, |sh, _| sh.recents.clone());
        assert_eq!(recents, [a, b, Target::Local]);
        // Connected now: the sheet can be closed without connecting.
        click(&mut s, "connect-cancel");
        assert!(!shown(&mut s));
    }

    #[gpui::test]
    fn switching_detaches_the_old_session_and_restores_the_layout(cx: &mut TestAppContext) {
        let dir = tempfile::tempdir().unwrap();
        let box_ = Target::Ssh { dest: "box".into() };
        let mut layout = Layout::single();
        let second = layout.split(PaneId(1), Axis::Horizontal).unwrap();
        layout.set_agent(second, Some(7));
        let mut state = GuiState::default();
        state.layouts.insert(box_.to_string(), layout.to_saved());
        state.save(dir.path()).unwrap();
        let backend = Arc::new(FakeBackend::default());
        let s = setup(cx, &backend, dir.path());
        s.cx.update(|_, cx| switch_target(Target::Local, cx));
        s.cx.update(|_, cx| switch_target(box_.clone(), cx));
        s.cx.run_until_parked();
        let calls = backend.calls();
        let at = calls
            .iter()
            .position(|c| *c == Call::Shutdown(Target::Local))
            .expect("the local session was shut down");
        assert_eq!(
            calls[at..],
            [
                Call::Shutdown(Target::Local),
                Call::Open(box_.clone(), PaneId(1), None),
                Call::Open(box_.clone(), second, Some(7)),
            ]
        );
        let restored = s.app.read_with(s.cx, |a, _| a.layout().to_saved());
        assert_eq!(restored, layout.to_saved());
        assert!(s.cx.debug_bounds("pane-card-1").is_some());
    }
}
