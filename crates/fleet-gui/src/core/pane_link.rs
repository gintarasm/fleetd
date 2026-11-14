//! One pane's connection (`gui-pane-<n>`): a reader thread that connects,
//! feeds the [`PaneModel`] and reconnects, and a writer thread that owns the
//! [`Sender`] and everything sent on it.

use std::collections::VecDeque;
use std::sync::mpsc::{self, RecvTimeoutError};
use std::sync::{Arc, Mutex};
use std::thread::{self, JoinHandle};
use std::time::{Duration, Instant};

use fleet_client::Sender;
use fleet_proto::{
    Attach, AttachOk, Detach, Frame, ReadHistory, Resize, Response, SendInput, SpawnAgent, event,
    request, response,
};
use fleet_term::Palette;

use super::backoff::Backoff;
use super::pane::{HISTORY_PAGE_LINES, PaneModel, PaneState};
use super::session::{ConnHandle, Ctx, PaneId, Stop, UiEvent, lock, read_batch};
use super::target::connect;

/// Resizes are sent once the size has been still this long.
const RESIZE_DEBOUNCE: Duration = Duration::from_millis(50);
/// A spawn asked for while disconnected is dropped, with an error, if the
/// connection is not back within this long.
const DEFERRED_SPAWN_TTL: Duration = Duration::from_secs(5);

enum Outgoing {
    /// A new connection is up; the writer sends on it from now on.
    Connected(Sender),
    Disconnected,
    /// Show this agent (from the UI).
    Attach(u64),
    /// Attach again to the agent the pane shows (after a reconnect or a
    /// `Lagged`).
    Reattach,
    Spawn(SpawnAgent),
    /// Typed or pasted input, to the attached agent.
    Input(Vec<u8>),
    /// Terminal query replies, to the agent whose output asked.
    Reply(u64, Vec<u8>),
    Resize(u16, u16),
    /// A history page computed by the reader after the attach numbered
    /// `epoch`.
    ReadHistory {
        epoch: u64,
        agent_id: u64,
        generation: u64,
        before_line: u64,
    },
    Palette(Palette),
    /// The view scrolled: ask for the next history page if it now needs
    /// one (no output may come to prompt the reader).
    CheckPaging,
    /// Detach and drop the connection.
    Close,
}

/// A request whose response the reader must recognise.
#[derive(Clone, Copy, Debug)]
enum Pending {
    Attach(u64),
    Spawn,
    Page { generation: u64 },
}

#[derive(Default)]
struct Shared {
    /// The agent the pane should show (`None` until the first attach or
    /// spawn reply).
    agent: Option<u64>,
    /// The agent the pane showed before a spawn on its way into it; shown
    /// again if the spawn fails.
    before_spawn: Option<u64>,
    /// The agent the current connection is subscribed to (or attaching).
    subscribed: Option<u64>,
    /// The agent whose attach the model last applied; frames for any other
    /// agent are ignored.
    shown: Option<u64>,
    /// Tracked requests on the current connection, by id (ascending).
    pending: VecDeque<(u64, Pending)>,
    /// The in-flight `ReadHistory`, issued after the latest attach.
    page: Option<u64>,
    /// Bumped by every applied attach and every new connection, so a page
    /// request computed before either is dropped.
    epoch: u64,
    conn: Option<ConnHandle>,
    #[cfg(any(test, feature = "test-hooks"))]
    pause_until: Option<Instant>,
    #[cfg(any(test, feature = "test-hooks"))]
    lags: u32,
}

impl Shared {
    /// The tracked request answered by response `id` (responses come in
    /// request order, so older entries are done).
    fn take_pending(&mut self, id: u64) -> Option<Pending> {
        while self.pending.front().is_some_and(|(p, _)| *p < id) {
            self.pending.pop_front();
        }
        match self.pending.front() {
            Some(&(p, kind)) if p == id => {
                self.pending.pop_front();
                Some(kind)
            }
            _ => None,
        }
    }

    fn attach_pending(&self, agent: u64) -> bool {
        self.pending
            .iter()
            .any(|(_, p)| matches!(p, Pending::Attach(a) if *a == agent))
    }
}

struct Link {
    ctx: Arc<Ctx>,
    pane: PaneId,
    model: Arc<Mutex<PaneModel>>,
    shared: Mutex<Shared>,
    stop: Stop,
}

impl Link {
    fn changed(&self) {
        self.ctx.notify(UiEvent::PaneChanged(self.pane));
    }
}

pub(crate) struct PaneLink {
    link: Arc<Link>,
    out: mpsc::Sender<Outgoing>,
    writer: JoinHandle<()>,
}

impl PaneLink {
    pub(crate) fn open(
        ctx: Arc<Ctx>,
        pane: PaneId,
        agent: Option<u64>,
        palette: Palette,
    ) -> PaneLink {
        let link = Arc::new(Link {
            ctx,
            pane,
            model: Arc::new(Mutex::new(PaneModel::new(palette))),
            shared: Mutex::new(Shared {
                agent,
                ..Shared::default()
            }),
            stop: Stop::default(),
        });
        let (out, rx) = mpsc::channel();
        let w = Arc::clone(&link);
        let writer = thread::Builder::new()
            .name(format!("gui-pane-{}-writer", pane.0))
            .spawn(move || Writer::new(w).run(&rx))
            .expect("spawn pane writer");
        let r = Arc::clone(&link);
        let tx = out.clone();
        thread::Builder::new()
            .name(format!("gui-pane-{}-reader", pane.0))
            .spawn(move || reader(&r, &tx))
            .expect("spawn pane reader");
        PaneLink { link, out, writer }
    }

    pub(crate) fn model(&self) -> Arc<Mutex<PaneModel>> {
        Arc::clone(&self.link.model)
    }

    fn send(&self, o: Outgoing) {
        let _ = self.out.send(o);
    }

    pub(crate) fn attach(&self, agent: u64) {
        self.send(Outgoing::Attach(agent));
    }

    pub(crate) fn spawn(&self, spawn: SpawnAgent) {
        self.send(Outgoing::Spawn(spawn));
    }

    pub(crate) fn input(&self, bytes: Vec<u8>) {
        self.send(Outgoing::Input(bytes));
    }

    pub(crate) fn resize(&self, cols: u16, rows: u16) {
        self.send(Outgoing::Resize(cols, rows));
    }

    pub(crate) fn set_palette(&self, palette: Palette) {
        self.send(Outgoing::Palette(palette));
    }

    pub(crate) fn check_paging(&self) {
        self.send(Outgoing::CheckPaging);
    }

    /// Stops reconnecting, then detaches and closes (on the writer).
    pub(crate) fn close(&self) {
        self.link.stop.set();
        self.send(Outgoing::Close);
    }

    /// Waits until the writer has detached and closed, or `deadline`.
    pub(crate) fn wait_closed(&self, deadline: Instant) {
        while !self.writer.is_finished() && Instant::now() < deadline {
            thread::sleep(Duration::from_millis(2));
        }
    }

    /// Closes the connection now, whatever the writer is doing.
    pub(crate) fn force_close(&self) {
        self.link.stop.set();
        if let Some(h) = lock(&self.link.shared).conn.as_mut() {
            h.close();
        }
    }

    #[cfg(any(test, feature = "test-hooks"))]
    pub(crate) fn pause_reader(&self, d: Duration) {
        lock(&self.link.shared).pause_until = Some(Instant::now() + d);
    }

    #[cfg(any(test, feature = "test-hooks"))]
    pub(crate) fn lag_count(&self) -> u32 {
        lock(&self.link.shared).lags
    }
}

/// Connects, reads until the connection ends, and reconnects with
/// [`Backoff`] until the pane is closed.
fn reader(link: &Link, out: &mpsc::Sender<Outgoing>) {
    let name = format!("gui-pane-{}", link.pane.0);
    let mut backoff = Backoff::new();
    while !link.stop.is_set() {
        if let Ok(c) = connect(&link.ctx.target, &link.ctx.state_dir, &name) {
            backoff.reset();
            {
                let mut sh = lock(&link.shared);
                let mut h = ConnHandle::new(&c.conn, c.child.take());
                if link.stop.is_set() {
                    h.close();
                    drop(sh);
                    h.finish();
                    return;
                }
                sh.conn = Some(h);
            }
            let (mut frames, sender) = c.conn.split();
            let _ = out.send(Outgoing::Connected(sender));
            let _ = out.send(Outgoing::Reattach);
            loop {
                #[cfg(any(test, feature = "test-hooks"))]
                {
                    let pause = lock(&link.shared).pause_until.take();
                    if let Some(until) = pause {
                        thread::sleep(until.saturating_duration_since(Instant::now()));
                    }
                }
                let Ok(batch) = read_batch(&mut frames) else {
                    break;
                };
                apply(link, out, batch);
            }
            let _ = out.send(Outgoing::Disconnected);
            let h = lock(&link.shared).conn.take();
            if let Some(h) = h {
                h.finish();
            }
        }
        if link.stop.is_set() {
            return;
        }
        lock(&link.model).on_disconnected();
        link.changed();
        if link.stop.wait(backoff.next_delay()) {
            return;
        }
    }
}

/// Applies one batch of frames under one model lock, then asks for the next
/// history page if the view needs it.
fn apply(link: &Link, out: &mpsc::Sender<Outgoing>, batch: Vec<Frame>) {
    let mut model = lock(&link.model);
    let mut changed = false;
    for frame in batch {
        changed |= apply_frame(link, out, &mut model, frame);
    }
    if let Some(request) = page_request(link, &mut model) {
        let _ = out.send(request);
    }
    drop(model);
    if changed {
        link.changed();
    }
}

/// The `ReadHistory` the view needs now, if any: only for a live pane
/// showing an agent, tagged with the current epoch.
fn page_request(link: &Link, model: &mut PaneModel) -> Option<Outgoing> {
    if model.state() != PaneState::Live {
        return None;
    }
    let (epoch, shown) = {
        let sh = lock(&link.shared);
        (sh.epoch, sh.shown)
    };
    let agent_id = shown?;
    let (generation, before_line) = model.next_page_request()?;
    Some(Outgoing::ReadHistory {
        epoch,
        agent_id,
        generation,
        before_line,
    })
}

/// `true` if the model changed.
fn apply_frame(
    link: &Link,
    out: &mpsc::Sender<Outgoing>,
    model: &mut PaneModel,
    frame: Frame,
) -> bool {
    let shown = lock(&link.shared).shown;
    let is_shown = |id: u64| shown == Some(id);
    match frame {
        Frame::Output { agent_id, data } if is_shown(agent_id) => {
            model.on_output(&data);
            let replies = model.take_replies();
            if !replies.is_empty() {
                let _ = out.send(Outgoing::Reply(agent_id, replies));
            }
            true
        }
        Frame::Response(r) => apply_response(link, out, model, r),
        Frame::Event(ev) => match ev.kind {
            Some(event::Kind::Exited(e)) if is_shown(e.agent_id) => {
                model.on_exited(e.exit_code);
                true
            }
            Some(event::Kind::Role(r)) if is_shown(r.agent_id) => {
                model.on_role(r.primary);
                true
            }
            Some(event::Kind::AgentResized(r)) if is_shown(r.agent_id) => {
                let cols = u16::try_from(r.cols).unwrap_or(u16::MAX);
                let rows = u16::try_from(r.rows).unwrap_or(u16::MAX);
                model.on_resized(cols, rows);
                true
            }
            Some(event::Kind::Lagged(l)) if is_shown(l.agent_id) => {
                // A stale `Lagged` can precede the reply to an attach
                // already on its way (ADR 0022).
                if lock(&link.shared).attach_pending(l.agent_id) {
                    return false;
                }
                #[cfg(any(test, feature = "test-hooks"))]
                {
                    lock(&link.shared).lags += 1;
                }
                model.on_attaching();
                let _ = out.send(Outgoing::Reattach);
                true
            }
            _ => false,
        },
        _ => false,
    }
}

fn apply_response(
    link: &Link,
    out: &mpsc::Sender<Outgoing>,
    model: &mut PaneModel,
    r: Response,
) -> bool {
    let mut sh = lock(&link.shared);
    let pending = sh.take_pending(r.id);
    let is_page = matches!(pending, Some(Pending::Page { .. })) && sh.page == Some(r.id);
    if sh.page == Some(r.id) {
        sh.page = None;
    }
    match (pending, r.kind) {
        (_, Some(response::Kind::Attached(ok))) if sh.agent == Some(ok.agent_id) => {
            attached(&mut sh, model, &ok);
            true
        }
        (_, Some(response::Kind::SpawnedAttached(sa))) => {
            let (Some(info), Some(ok)) = (sa.agent, sa.attached) else {
                return false;
            };
            sh.agent = Some(info.id);
            sh.before_spawn = None;
            sh.subscribed = Some(info.id);
            attached(&mut sh, model, &ok);
            true
        }
        (Some(Pending::Page { generation }), Some(response::Kind::History(page))) if is_page => {
            drop(sh);
            model.on_page(&page, generation);
            true
        }
        (Some(Pending::Page { .. }), _) if is_page => {
            drop(sh);
            model.on_page_error();
            false
        }
        (Some(Pending::Attach(agent)), Some(response::Kind::Error(_)))
            if sh.agent == Some(agent) =>
        {
            if sh.subscribed == Some(agent) {
                sh.subscribed = None;
            }
            drop(sh);
            model.on_lost();
            true
        }
        (Some(Pending::Spawn), Some(response::Kind::Error(e))) => {
            // Back to the agent the pane showed before.
            let restore = sh.agent.is_none() && sh.before_spawn.is_some();
            if restore {
                sh.agent = sh.before_spawn.take();
            }
            drop(sh);
            link.ctx
                .notify(UiEvent::Error(format!("spawn failed: {}", e.message)));
            if restore {
                let _ = out.send(Outgoing::Reattach);
            }
            false
        }
        _ => false,
    }
}

/// Applies an attach reply: a new epoch, no page in flight.
fn attached(sh: &mut Shared, model: &mut PaneModel, ok: &AttachOk) {
    sh.shown = Some(ok.agent_id);
    sh.epoch += 1;
    sh.page = None;
    sh.pending
        .retain(|(_, p)| !matches!(p, Pending::Page { .. }));
    model.on_attached(ok);
}

struct Writer {
    link: Arc<Link>,
    sender: Option<Sender>,
    /// A spawn asked for while disconnected, and when: sent on the next
    /// connection unless older than [`DEFERRED_SPAWN_TTL`].
    deferred_spawn: Option<(SpawnAgent, Instant)>,
    size: Option<(u16, u16)>,
    resize_due: Option<Instant>,
}

impl Writer {
    fn new(link: Arc<Link>) -> Self {
        Self {
            link,
            sender: None,
            deferred_spawn: None,
            size: None,
            resize_due: None,
        }
    }

    fn run(mut self, rx: &mpsc::Receiver<Outgoing>) {
        loop {
            let deadline = [
                self.resize_due,
                self.deferred_spawn
                    .as_ref()
                    .map(|(_, at)| *at + DEFERRED_SPAWN_TTL),
            ]
            .into_iter()
            .flatten()
            .min();
            let next = match deadline {
                Some(due) => rx.recv_timeout(due.saturating_duration_since(Instant::now())),
                None => rx.recv().map_err(|_| RecvTimeoutError::Disconnected),
            };
            let o = match next {
                Ok(o) => o,
                Err(RecvTimeoutError::Timeout) => {
                    self.timers();
                    continue;
                }
                Err(RecvTimeoutError::Disconnected) => return,
            };
            if !self.handle(o) {
                return;
            }
            // A steady stream of messages never lets `recv_timeout` time out.
            self.timers();
        }
    }

    /// Sends a resize that is due and drops a deferred spawn that expired.
    fn timers(&mut self) {
        if self.resize_due.is_some_and(|due| due <= Instant::now()) {
            self.resize_due = None;
            self.send_resize();
        }
        self.expire_deferred_spawn();
    }

    /// A spawn deferred for longer than [`DEFERRED_SPAWN_TTL`] is dropped
    /// with an error; the pane shows its old agent again.
    fn expire_deferred_spawn(&mut self) {
        if !self
            .deferred_spawn
            .as_ref()
            .is_some_and(|(_, at)| at.elapsed() >= DEFERRED_SPAWN_TTL)
        {
            return;
        }
        self.deferred_spawn = None;
        {
            let mut sh = lock(&self.link.shared);
            if sh.agent.is_none() {
                sh.agent = sh.before_spawn.take();
            }
        }
        self.link.ctx.notify(UiEvent::Error(
            "spawn dropped: not connected to fleetd".into(),
        ));
    }

    /// `false` once the pane is closed.
    fn handle(&mut self, o: Outgoing) -> bool {
        match o {
            Outgoing::Connected(sender) => {
                self.expire_deferred_spawn();
                self.sender = Some(sender);
                let mut sh = lock(&self.link.shared);
                sh.subscribed = None;
                sh.pending.clear();
                sh.page = None;
                sh.epoch += 1;
                let idle = sh.agent.is_none();
                drop(sh);
                if let Some((spawn, _)) = self.deferred_spawn.take() {
                    self.spawn(spawn);
                } else if idle {
                    // Nothing to attach: back to the neutral state a new
                    // pane starts in, not "disconnected".
                    let mut m = lock(&self.link.model);
                    if m.state() == PaneState::Disconnected {
                        m.on_attaching();
                        drop(m);
                        self.link.changed();
                    }
                }
            }
            Outgoing::Disconnected => {
                self.sender = None;
                lock(&self.link.shared).subscribed = None;
            }
            Outgoing::Attach(agent) => {
                {
                    let mut sh = lock(&self.link.shared);
                    sh.agent = Some(agent);
                    sh.before_spawn = None;
                }
                self.attach(agent);
            }
            Outgoing::Reattach => {
                let agent = lock(&self.link.shared).agent;
                if let Some(agent) = agent {
                    self.attach(agent);
                }
            }
            Outgoing::Spawn(spawn) => {
                // Until `SpawnedAttached` names the new agent, nothing
                // re-attaches the old one (a reconnect or a late `Lagged`);
                // a failed spawn shows it again.
                {
                    let mut sh = lock(&self.link.shared);
                    if let Some(old) = sh.agent.take() {
                        sh.before_spawn = Some(old);
                    }
                }
                if self.sender.is_some() {
                    self.spawn(spawn);
                } else {
                    self.deferred_spawn = Some((spawn, Instant::now()));
                }
            }
            Outgoing::Input(data) => {
                let agent = lock(&self.link.shared).subscribed;
                if let Some(agent_id) = agent {
                    self.send(request::Kind::Input(SendInput { agent_id, data }));
                }
            }
            Outgoing::Reply(agent_id, data) => {
                self.send(request::Kind::Input(SendInput { agent_id, data }));
            }
            Outgoing::Resize(cols, rows) => {
                self.size = Some((cols, rows));
                self.resize_due = Some(Instant::now() + RESIZE_DEBOUNCE);
            }
            Outgoing::ReadHistory {
                epoch,
                agent_id,
                generation,
                before_line,
            } => {
                let live = lock(&self.link.model).state() == PaneState::Live;
                let id = self.sender.as_ref().map(Sender::next_id);
                let Some(id) = id.filter(|_| live) else {
                    lock(&self.link.model).cancel_page_request();
                    return true;
                };
                {
                    let mut sh = lock(&self.link.shared);
                    if sh.epoch != epoch || sh.page.is_some() {
                        return true;
                    }
                    sh.pending.push_back((id, Pending::Page { generation }));
                    sh.page = Some(id);
                }
                self.send(request::Kind::ReadHistory(ReadHistory {
                    agent_id,
                    generation,
                    before_line,
                    max_lines: HISTORY_PAGE_LINES,
                }));
            }
            Outgoing::Palette(palette) => lock(&self.link.model).set_palette(palette),
            Outgoing::CheckPaging => {
                let request = page_request(&self.link, &mut lock(&self.link.model));
                if let Some(request) = request {
                    return self.handle(request);
                }
            }
            Outgoing::Close => {
                let agent = lock(&self.link.shared).subscribed;
                if let Some(agent_id) = agent {
                    self.send(request::Kind::Detach(Detach { agent_id }));
                }
                if let Some(h) = lock(&self.link.shared).conn.as_mut() {
                    h.close();
                }
                return false;
            }
        }
        true
    }

    fn attach(&mut self, agent_id: u64) {
        if self.sender.is_none() {
            // The next connection re-attaches `Shared::agent`.
            return;
        }
        lock(&self.link.model).on_attaching();
        self.link.changed();
        let old = lock(&self.link.shared).subscribed.replace(agent_id);
        if let Some(old) = old.filter(|&o| o != agent_id) {
            self.send(request::Kind::Detach(Detach { agent_id: old }));
        }
        let Some(id) = self.sender.as_ref().map(Sender::next_id) else {
            return;
        };
        lock(&self.link.shared)
            .pending
            .push_back((id, Pending::Attach(agent_id)));
        self.send(request::Kind::Attach(Attach {
            agent_id,
            observe: false,
        }));
        // Keep the agent at this pane's size.
        if self.size.is_some() {
            self.send_resize();
        }
    }

    fn spawn(&mut self, spawn: SpawnAgent) {
        let old = {
            let mut sh = lock(&self.link.shared);
            sh.agent = None;
            sh.subscribed.take()
        };
        if let Some(agent_id) = old {
            self.send(request::Kind::Detach(Detach { agent_id }));
        }
        let Some(id) = self.sender.as_ref().map(Sender::next_id) else {
            self.deferred_spawn = Some((spawn, Instant::now()));
            return;
        };
        lock(&self.link.model).on_attaching();
        self.link.changed();
        lock(&self.link.shared)
            .pending
            .push_back((id, Pending::Spawn));
        self.send(request::Kind::Spawn(Box::new(spawn)));
    }

    fn send_resize(&mut self) {
        let agent = lock(&self.link.shared).subscribed;
        if let (Some(agent_id), Some((cols, rows))) = (agent, self.size) {
            self.send(request::Kind::Resize(Resize {
                agent_id,
                cols: cols.into(),
                rows: rows.into(),
            }));
        }
    }

    /// Sends on the current connection; a failed write drops it (the
    /// reader sees the connection end and reconnects).
    fn send(&mut self, kind: request::Kind) {
        if let Some(s) = &mut self.sender
            && s.send(kind).is_err()
        {
            self.sender = None;
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use bytes::BytesMut;
    use fleet_client::Connection;
    use fleet_proto::{Ack, FrameCodec, HelloOk, HistoryPage, PROTOCOL_VERSION};
    use std::io::{Read, Write};
    use std::os::unix::net::UnixStream;
    use tokio_util::codec::{Decoder, Encoder};

    /// The server end of a handshaken connection: decodes what the writer
    /// sends.
    struct FakeServer {
        stream: UnixStream,
        buf: BytesMut,
    }

    impl FakeServer {
        fn next_frame(&mut self, wait: Duration) -> Option<Frame> {
            let deadline = Instant::now() + wait;
            loop {
                if let Some(f) = FrameCodec::new().decode(&mut self.buf).unwrap() {
                    return Some(f);
                }
                let left = deadline.saturating_duration_since(Instant::now());
                if left.is_zero() {
                    return None;
                }
                // Fails on macOS once the peer has closed; reads then end.
                let _ = self.stream.set_read_timeout(Some(left));
                let mut chunk = [0u8; 4096];
                match self.stream.read(&mut chunk) {
                    Ok(0) | Err(_) => return None,
                    Ok(n) => self.buf.extend_from_slice(&chunk[..n]),
                }
            }
        }

        /// The requests that arrive within `wait`.
        fn requests(&mut self, wait: Duration) -> Vec<request::Kind> {
            let mut out = Vec::new();
            while let Some(f) = self.next_frame(wait) {
                if let Frame::Request(r) = f {
                    out.extend(r.kind);
                }
            }
            out
        }
    }

    fn connected() -> (Sender, FakeServer) {
        let (client, server) = UnixStream::pair().unwrap();
        let mut srv = FakeServer {
            stream: server,
            buf: BytesMut::new(),
        };
        let handshake = thread::spawn(move || {
            assert!(matches!(
                srv.next_frame(Duration::from_secs(5)),
                Some(Frame::Hello(_))
            ));
            let mut out = BytesMut::new();
            FrameCodec::new()
                .encode(
                    Frame::HelloOk(HelloOk {
                        protocol_version: PROTOCOL_VERSION,
                        server: "fake".into(),
                    }),
                    &mut out,
                )
                .unwrap();
            srv.stream.write_all(&out).unwrap();
            srv
        });
        let conn =
            Connection::over(Box::new(client.try_clone().unwrap()), Box::new(client), "t").unwrap();
        (conn.split().1, handshake.join().unwrap())
    }

    fn spawn_req() -> SpawnAgent {
        SpawnAgent {
            program: "/bin/true".into(),
            attach: true,
            ..Default::default()
        }
    }

    fn is_attach(k: &request::Kind) -> bool {
        matches!(k, request::Kind::Attach(_))
    }

    const SHORT: Duration = Duration::from_millis(200);

    fn link() -> Arc<Link> {
        link_with(Ctx::for_test())
    }

    fn link_with(ctx: Arc<Ctx>) -> Arc<Link> {
        Arc::new(Link {
            ctx,
            pane: PaneId(1),
            model: Arc::new(Mutex::new(PaneModel::new(DEFAULT_PALETTE_FOR_TEST))),
            shared: Mutex::new(Shared {
                agent: Some(1),
                ..Shared::default()
            }),
            stop: Stop::default(),
        })
    }

    const DEFAULT_PALETTE_FOR_TEST: Palette = Palette {
        fg: (255, 255, 255),
        bg: (0, 0, 0),
        cursor: (255, 255, 255),
        ansi: [(0, 0, 0); 16],
    };

    /// Line 100 is the oldest loaded; older history exists on the node.
    fn attached_response(id: u64) -> Response {
        Response {
            id,
            kind: Some(response::Kind::Attached(Box::new(AttachOk {
                agent_id: 1,
                cols: 80,
                rows: 24,
                history_tail: vec![b"tail".to_vec().into()],
                tail_first_line: 100,
                primary: true,
                ..Default::default()
            }))),
        }
    }

    fn page_response(id: u64) -> Response {
        Response {
            id,
            kind: Some(response::Kind::History(HistoryPage {
                agent_id: 1,
                first_line: 99,
                lines: vec![b"older".to_vec().into()],
            })),
        }
    }

    /// What the writer records when it sends a `ReadHistory` as `id`.
    fn page_sent(l: &Link, id: u64) {
        let mut sh = lock(&l.shared);
        sh.pending.push_back((id, Pending::Page { generation: 0 }));
        sh.page = Some(id);
    }

    fn first_line(l: &Link) -> u64 {
        lock(&l.model).scrollback().first_line()
    }

    fn respond(l: &Link, r: Response) {
        let mut m = lock(&l.model);
        apply_response(l, &mpsc::channel().0, &mut m, r);
    }

    #[test]
    fn page_reply_for_the_request_in_flight_is_applied() {
        let l = link();
        respond(&l, attached_response(1));
        assert_eq!(lock(&l.model).next_page_request(), Some((0, 100)));
        page_sent(&l, 2);
        respond(&l, page_response(2));
        assert_eq!(first_line(&l), 99);
    }

    #[test]
    fn page_reply_to_a_request_made_before_a_reattach_is_ignored() {
        let l = link();
        respond(&l, attached_response(1));
        assert!(lock(&l.model).next_page_request().is_some());
        // The page goes out while a re-attach (request 2) is on its way.
        page_sent(&l, 3);
        respond(&l, attached_response(2));
        respond(&l, page_response(3));
        assert_eq!(first_line(&l), 100);
        // The re-attached pane asks again.
        assert_eq!(lock(&l.model).next_page_request(), Some((0, 100)));
    }

    #[test]
    fn page_reply_with_an_unexpected_id_is_ignored() {
        let l = link();
        respond(&l, attached_response(1));
        assert!(lock(&l.model).next_page_request().is_some());
        page_sent(&l, 3);
        respond(&l, page_response(2));
        assert_eq!(first_line(&l), 100);
        respond(&l, page_response(3));
        assert_eq!(first_line(&l), 99);
    }

    #[test]
    fn deferred_spawn_then_reattach_sends_no_attach() {
        let l = link();
        let mut w = Writer::new(Arc::clone(&l));
        let (sender, mut srv) = connected();
        w.handle(Outgoing::Spawn(spawn_req()));
        w.handle(Outgoing::Connected(sender));
        w.handle(Outgoing::Reattach);
        let reqs = srv.requests(SHORT);
        assert!(
            matches!(reqs.as_slice(), [request::Kind::Spawn(_)]),
            "{reqs:?}"
        );
    }

    #[test]
    fn spawn_then_late_reattach_does_not_resubscribe_the_old_agent() {
        let l = link();
        let mut w = Writer::new(Arc::clone(&l));
        let (sender, mut srv) = connected();
        w.handle(Outgoing::Connected(sender));
        w.handle(Outgoing::Attach(1));
        w.handle(Outgoing::Spawn(spawn_req()));
        // A reconnect or a late `Lagged` for agent 1.
        w.handle(Outgoing::Reattach);
        let reqs = srv.requests(SHORT);
        assert_eq!(reqs.iter().filter(|k| is_attach(k)).count(), 1, "{reqs:?}");
        assert!(
            matches!(
                reqs.as_slice(),
                [
                    request::Kind::Attach(_),
                    request::Kind::Detach(Detach { agent_id: 1 }),
                    request::Kind::Spawn(_)
                ]
            ),
            "{reqs:?}"
        );
    }

    #[test]
    fn resize_is_sent_under_a_steady_input_stream() {
        let l = link();
        let (sender, mut srv) = connected();
        let (tx, rx) = mpsc::channel();
        tx.send(Outgoing::Connected(sender)).unwrap();
        tx.send(Outgoing::Attach(1)).unwrap();
        tx.send(Outgoing::Resize(100, 30)).unwrap();
        // A backlog that takes the writer far longer than the 50 ms
        // debounce to send, so `recv_timeout` never times out meanwhile.
        for _ in 0..200_000 {
            tx.send(Outgoing::Input(b"x".to_vec())).unwrap();
        }
        tx.send(Outgoing::Close).unwrap();
        let w = Writer::new(Arc::clone(&l));
        let writer = thread::spawn(move || w.run(&rx));
        let reqs = srv.requests(SHORT);
        let resize = reqs.iter().position(|k| {
            matches!(
                k,
                request::Kind::Resize(Resize {
                    agent_id: 1,
                    cols: 100,
                    rows: 30
                })
            )
        });
        let last_input = reqs
            .iter()
            .rposition(|k| matches!(k, request::Kind::Input(_)));
        // Sent while the inputs were still coming, not after they stopped.
        assert!(
            resize.zip(last_input).is_some_and(|(r, i)| r < i),
            "resize at {resize:?}, last input at {last_input:?}"
        );
        writer.join().unwrap();
    }

    #[test]
    fn page_request_is_not_sent_once_the_pane_is_not_live() {
        let l = link();
        let mut w = Writer::new(Arc::clone(&l));
        let (sender, mut srv) = connected();
        w.handle(Outgoing::Connected(sender));
        respond(&l, attached_response(1));
        let (generation, before_line) = lock(&l.model).next_page_request().unwrap();
        lock(&l.model).on_exited(0);
        let epoch = lock(&l.shared).epoch;
        w.handle(Outgoing::ReadHistory {
            epoch,
            agent_id: 1,
            generation,
            before_line,
        });
        let reqs = srv.requests(SHORT);
        assert!(reqs.is_empty(), "{reqs:?}");
        assert!(lock(&l.shared).page.is_none());
        // Not left marked as in flight.
        assert!(lock(&l.model).next_page_request().is_some());
    }

    #[test]
    fn unexpected_reply_to_the_page_in_flight_ends_paging() {
        let l = link();
        respond(&l, attached_response(1));
        assert!(lock(&l.model).next_page_request().is_some());
        page_sent(&l, 2);
        respond(
            &l,
            Response {
                id: 2,
                kind: Some(response::Kind::Ack(Ack {})),
            },
        );
        assert!(lock(&l.shared).page.is_none());
        assert!(lock(&l.model).scrollback().fully_loaded());
    }

    #[test]
    fn check_paging_near_the_top_sends_one_read_history() {
        let l = link();
        let mut w = Writer::new(Arc::clone(&l));
        let (sender, mut srv) = connected();
        w.handle(Outgoing::Connected(sender));
        respond(&l, attached_response(1));
        w.handle(Outgoing::CheckPaging);
        // The request is in flight: checking again sends nothing.
        w.handle(Outgoing::CheckPaging);
        let reqs = srv.requests(SHORT);
        assert!(
            matches!(
                reqs.as_slice(),
                [request::Kind::ReadHistory(ReadHistory {
                    agent_id: 1,
                    generation: 0,
                    before_line: 100,
                    ..
                })]
            ),
            "{reqs:?}"
        );
        assert!(lock(&l.shared).page.is_some());
    }

    fn error_response(id: u64) -> Response {
        Response {
            id,
            kind: Some(response::Kind::Error(fleet_proto::Error {
                message: "no such directory".into(),
            })),
        }
    }

    #[test]
    fn failed_spawn_reattaches_the_agent_the_pane_showed() {
        let (ctx, events) = Ctx::for_test_with_events();
        let l = link_with(ctx);
        let mut w = Writer::new(Arc::clone(&l));
        let (sender, mut srv) = connected();
        w.handle(Outgoing::Connected(sender));
        w.handle(Outgoing::Attach(1));
        w.handle(Outgoing::Spawn(spawn_req()));
        let spawn_id = lock(&l.shared).pending.back().unwrap().0;
        let (tx, rx) = mpsc::channel();
        {
            let mut m = lock(&l.model);
            apply_response(&l, &tx, &mut m, error_response(spawn_id));
        }
        for o in rx.try_iter() {
            w.handle(o);
        }
        let reqs = srv.requests(SHORT);
        assert!(
            matches!(
                reqs.as_slice(),
                [
                    request::Kind::Attach(Attach { agent_id: 1, .. }),
                    request::Kind::Detach(Detach { agent_id: 1 }),
                    request::Kind::Spawn(_),
                    request::Kind::Attach(Attach { agent_id: 1, .. }),
                ]
            ),
            "{reqs:?}"
        );
        assert_eq!(lock(&l.shared).agent, Some(1));
        assert!(
            std::iter::from_fn(|| events.try_recv().ok())
                .any(|e| matches!(e, UiEvent::Error(m) if m.contains("no such directory")))
        );
    }

    #[test]
    fn deferred_spawn_older_than_its_ttl_is_dropped_with_an_error() {
        let (ctx, events) = Ctx::for_test_with_events();
        let l = link_with(ctx);
        let mut w = Writer::new(Arc::clone(&l));
        w.handle(Outgoing::Spawn(spawn_req()));
        let queued = Instant::now()
            .checked_sub(DEFERRED_SPAWN_TTL + Duration::from_millis(1))
            .unwrap();
        w.deferred_spawn.as_mut().unwrap().1 = queued;
        let (sender, mut srv) = connected();
        w.handle(Outgoing::Connected(sender));
        w.handle(Outgoing::Reattach);
        let reqs = srv.requests(SHORT);
        assert!(
            matches!(
                reqs.as_slice(),
                [request::Kind::Attach(Attach { agent_id: 1, .. })]
            ),
            "{reqs:?}"
        );
        assert!(
            std::iter::from_fn(|| events.try_recv().ok())
                .any(|e| matches!(e, UiEvent::Error(m) if m.contains("spawn")))
        );
    }

    #[test]
    fn pane_without_agent_is_not_disconnected_once_connected() {
        let l = link();
        lock(&l.shared).agent = None;
        lock(&l.model).on_disconnected();
        let mut w = Writer::new(Arc::clone(&l));
        let (sender, _srv) = connected();
        w.handle(Outgoing::Connected(sender));
        assert_eq!(lock(&l.model).state(), PaneState::Attaching);
    }
}
