//! Leases (ADR 0077, spec §4): named locks per node, held by an agent or
//! the user, with a FIFO of waiters in memory.
//!
//! This is a synchronous state machine: it owns no timers and reads no
//! clock. Every method that can change what is held takes `now_ms`, and the
//! 1-minute load average comes from the function given at construction, so
//! tests are deterministic. The daemon's driver calls [`Leases::tick`] at
//! [`Leases::next_wake`] (TTL expiry, the reclaim grace, `--max-load`
//! polling) and the connection and agent-exit paths call the matching
//! release methods.

use std::collections::{HashMap, VecDeque};
use std::io;
use std::sync::{Arc, Mutex, MutexGuard};

use tokio::sync::{Notify, oneshot};

use crate::node::ConnId;
use crate::store::{LeaseRow, Store};

/// How long a lease tied to a connection survives a daemon restart waiting
/// for its client to reconnect and reclaim it.
pub const RECLAIM_GRACE_MS: i64 = 30_000;

/// How often a waiter with `--max-load` re-reads the load average.
pub const LOAD_POLL_MS: i64 = 1_000;

/// A request for a lease.
#[derive(Debug, Clone)]
pub struct Want {
    pub name: String,
    /// The agent asking; `None`: the user.
    pub holder: Option<u64>,
    /// The client's key for this hold (see [`LeaseRow::holder_conn`]).
    pub key: i64,
    /// The connection asking.
    pub conn: ConnId,
    /// Tie the lease to `conn`: released when it closes.
    pub bind: bool,
    pub ttl_ms: Option<i64>,
    /// Granted only while the 1-minute load average is below this.
    pub max_load: Option<f64>,
}

/// A lease as `fleet lease ls` shows it.
#[derive(Debug, Clone, PartialEq)]
pub struct Info {
    pub name: String,
    /// Whether anyone holds it (a name with only load-blocked waiters is
    /// listed too).
    pub held: bool,
    pub holder: Option<u64>,
    pub acquired_ms: i64,
    pub expires_ms: Option<i64>,
    /// Tied to a connection.
    pub bound: bool,
    pub waiters: u32,
}

pub enum Acquired {
    Granted(Info),
    /// Held by someone else (or others are queued ahead): `None` when only
    /// waiters block it.
    Busy(Option<Info>),
    /// Free, but the load average is not below `max_load`; the load seen.
    LoadTooHigh(f64),
    /// Queued (`wait`): the grant arrives on the receiver. Dropping the
    /// receiver leaves the queue.
    Queued(oneshot::Receiver<Info>),
}

pub enum Released {
    Released,
    NotHeld,
    /// Held by someone else, and the releaser is not the user.
    NotYours(Info),
}

/// Who releases a lease.
#[derive(Debug, Clone, Copy)]
pub enum Releaser {
    User,
    Agent(u64),
}

struct Held {
    holder: Option<u64>,
    key: i64,
    /// The connection it is tied to, while that connection lives.
    conn: Option<ConnId>,
    bind: bool,
    acquired_ms: i64,
    expires_ms: Option<i64>,
    /// A bound lease loaded from the store: released at this time unless
    /// its client reclaims it.
    orphan_until_ms: Option<i64>,
}

struct Waiter {
    want: Want,
    tx: oneshot::Sender<Info>,
}

#[derive(Default)]
struct State {
    held: HashMap<String, Held>,
    queues: HashMap<String, VecDeque<Waiter>>,
}

pub struct Leases {
    store: Arc<Mutex<Store>>,
    state: Mutex<State>,
    load: Box<dyn Fn() -> f64 + Send + Sync>,
    wake: Notify,
}

/// The 1-minute load average; infinite when the system does not say, so a
/// `--max-load` lease is never granted on a guess.
pub fn load_average() -> f64 {
    let mut avg = [0f64; 1];
    // SAFETY: writes at most one f64 into `avg`.
    let n = unsafe { libc::getloadavg(avg.as_mut_ptr(), 1) };
    if n < 1 { f64::INFINITY } else { avg[0] }
}

/// Unix time in milliseconds.
pub fn now_ms() -> i64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map_or(0, |d| i64::try_from(d.as_millis()).unwrap_or(i64::MAX))
}

fn io_err(e: rusqlite::Error) -> io::Error {
    io::Error::other(e)
}

fn lock<T>(m: &Mutex<T>) -> MutexGuard<'_, T> {
    m.lock().unwrap_or_else(|e| e.into_inner())
}

impl Leases {
    /// Loads the leases in `store`: those of agents that no longer run are
    /// deleted, and those tied to a connection wait [`RECLAIM_GRACE_MS`] for
    /// their client to come back.
    pub fn load(
        store: Arc<Mutex<Store>>,
        now_ms: i64,
        load: impl Fn() -> f64 + Send + Sync + 'static,
    ) -> io::Result<Leases> {
        let mut state = State::default();
        {
            let s = lock(&store);
            s.delete_leases_of_ended_agents().map_err(io_err)?;
            for row in s.leases().map_err(io_err)? {
                let bind = row.holder_conn.is_some();
                state.held.insert(
                    row.name,
                    Held {
                        holder: row.holder,
                        key: row.holder_conn.unwrap_or(0),
                        conn: None,
                        bind,
                        acquired_ms: row.acquired_ms,
                        expires_ms: row.expires_ms,
                        orphan_until_ms: bind.then_some(now_ms + RECLAIM_GRACE_MS),
                    },
                );
            }
        }
        Ok(Leases {
            store,
            state: Mutex::new(state),
            load: Box::new(load),
            wake: Notify::new(),
        })
    }

    /// Signalled whenever the next deadline may have changed: the driver
    /// re-reads [`Leases::next_wake`].
    pub fn woken(&self) -> &Notify {
        &self.wake
    }

    fn info(name: &str, h: &Held, waiters: u32) -> Info {
        Info {
            name: name.to_owned(),
            held: true,
            holder: h.holder,
            acquired_ms: h.acquired_ms,
            expires_ms: h.expires_ms,
            bound: h.bind,
            waiters,
        }
    }

    fn waiting(st: &State, name: &str) -> u32 {
        st.queues
            .get(name)
            .map_or(0, |q| u32::try_from(q.len()).unwrap_or(u32::MAX))
    }

    /// Stores and records `want` as held.
    fn grant(&self, st: &mut State, want: &Want, now_ms: i64) -> io::Result<Info> {
        let held = Held {
            holder: want.holder,
            key: want.key,
            conn: want.bind.then_some(want.conn),
            bind: want.bind,
            acquired_ms: now_ms,
            expires_ms: want.ttl_ms.map(|t| now_ms.saturating_add(t)),
            orphan_until_ms: None,
        };
        lock(&self.store)
            .put_lease(&LeaseRow {
                name: want.name.clone(),
                holder: held.holder,
                holder_conn: want.bind.then_some(want.key),
                acquired_ms: held.acquired_ms,
                expires_ms: held.expires_ms,
            })
            .map_err(io_err)?;
        let info = Self::info(&want.name, &held, Self::waiting(st, &want.name));
        st.held.insert(want.name.clone(), held);
        self.wake.notify_one();
        Ok(info)
    }

    /// Drops the lease `name` from memory and the store.
    fn drop_held(&self, st: &mut State, name: &str) -> io::Result<()> {
        lock(&self.store).delete_lease(name).map_err(io_err)?;
        st.held.remove(name);
        self.wake.notify_one();
        Ok(())
    }

    /// Asks for a lease. Without `wait`, a lease that is held (or has
    /// waiters ahead) is `Busy`; with it, the request queues behind them.
    pub fn acquire(&self, want: Want, wait: bool, now_ms: i64) -> io::Result<Acquired> {
        let mut st = lock(&self.state);
        let waiters = Self::waiting(&st, &want.name);
        if let Some(h) = st.held.get_mut(&want.name) {
            // The same client again after a daemon restart.
            if h.bind && want.bind && h.key == want.key && h.holder == want.holder {
                h.conn = Some(want.conn);
                h.orphan_until_ms = None;
                let info = Self::info(&want.name, h, waiters);
                self.wake.notify_one();
                return Ok(Acquired::Granted(info));
            }
            if !wait {
                let info = Self::info(&want.name, h, waiters);
                return Ok(Acquired::Busy(Some(info)));
            }
            return Ok(self.queue(&mut st, want));
        }
        let blocked_by_queue = Self::waiting(&st, &want.name) > 0;
        if blocked_by_queue {
            return Ok(if wait {
                self.queue(&mut st, want)
            } else {
                Acquired::Busy(None)
            });
        }
        if let Some(max) = want.max_load {
            let load = (self.load)();
            if load >= max {
                return Ok(if wait {
                    self.queue(&mut st, want)
                } else {
                    Acquired::LoadTooHigh(load)
                });
            }
        }
        let info = self.grant(&mut st, &want, now_ms)?;
        Ok(Acquired::Granted(info))
    }

    fn queue(&self, st: &mut State, want: Want) -> Acquired {
        let (tx, rx) = oneshot::channel();
        st.queues
            .entry(want.name.clone())
            .or_default()
            .push_back(Waiter { want, tx });
        self.wake.notify_one();
        Acquired::Queued(rx)
    }

    /// Releases `name` for `by`: the user may release any lease, an agent
    /// only one it holds.
    pub fn release(&self, name: &str, by: Releaser, now_ms: i64) -> io::Result<Released> {
        let mut st = lock(&self.state);
        let Some(h) = st.held.get(name) else {
            return Ok(Released::NotHeld);
        };
        if let Releaser::Agent(a) = by
            && h.holder != Some(a)
        {
            let info = Self::info(name, h, Self::waiting(&st, name));
            return Ok(Released::NotYours(info));
        }
        self.drop_held(&mut st, name)?;
        self.pump(&mut st, now_ms)?;
        Ok(Released::Released)
    }

    /// A connection closed: its waiters leave their queues and the leases
    /// tied to it are released.
    pub fn conn_closed(&self, conn: ConnId, now_ms: i64) -> io::Result<()> {
        let mut st = lock(&self.state);
        let mut changed = false;
        for q in st.queues.values_mut() {
            let before = q.len();
            q.retain(|w| w.want.conn != conn);
            changed |= q.len() != before;
        }
        let names: Vec<String> = st
            .held
            .iter()
            .filter(|(_, h)| h.bind && h.conn == Some(conn))
            .map(|(n, _)| n.clone())
            .collect();
        for name in &names {
            self.drop_held(&mut st, name)?;
        }
        if changed || !names.is_empty() {
            self.pump(&mut st, now_ms)?;
        }
        Ok(())
    }

    /// An agent ended: it releases what it holds and stops waiting.
    pub fn agent_gone(&self, agent: u64, now_ms: i64) -> io::Result<()> {
        self.agents_gone(|a| a == agent, now_ms)
    }

    /// Every agent `gone` names ended (a sweep, for an exit event that was
    /// missed): they release what they hold and stop waiting.
    pub fn agents_gone(&self, gone: impl Fn(u64) -> bool, now_ms: i64) -> io::Result<()> {
        let is_gone = |h: Option<u64>| h.is_some_and(&gone);
        let mut st = lock(&self.state);
        let mut changed = false;
        for q in st.queues.values_mut() {
            let before = q.len();
            q.retain(|w| !is_gone(w.want.holder));
            changed |= q.len() != before;
        }
        let names: Vec<String> = st
            .held
            .iter()
            .filter(|(_, h)| is_gone(h.holder))
            .map(|(n, _)| n.clone())
            .collect();
        for name in &names {
            self.drop_held(&mut st, name)?;
        }
        if changed || !names.is_empty() {
            self.pump(&mut st, now_ms)?;
        }
        Ok(())
    }

    /// Releases what expired (TTL, the reclaim grace) and grants waiters
    /// whose turn has come (FIFO; the head waits for its `--max-load`).
    pub fn tick(&self, now_ms: i64) -> io::Result<()> {
        let mut st = lock(&self.state);
        let due: Vec<String> = st
            .held
            .iter()
            .filter(|(_, h)| {
                h.expires_ms.is_some_and(|t| t <= now_ms)
                    || h.orphan_until_ms.is_some_and(|t| t <= now_ms)
            })
            .map(|(n, _)| n.clone())
            .collect();
        for name in &due {
            self.drop_held(&mut st, name)?;
        }
        self.pump(&mut st, now_ms)
    }

    /// Grants every free lease to the head of its queue, skipping waiters
    /// that left.
    fn pump(&self, st: &mut State, now_ms: i64) -> io::Result<()> {
        let mut load: Option<f64> = None;
        let names: Vec<String> = st.queues.keys().cloned().collect();
        for name in names {
            while !st.held.contains_key(&name) {
                let Some(q) = st.queues.get_mut(&name) else {
                    break;
                };
                while q.front().is_some_and(|w| w.tx.is_closed()) {
                    q.pop_front();
                }
                let Some(head) = q.front() else { break };
                if let Some(max) = head.want.max_load {
                    let now_load = *load.get_or_insert_with(|| (self.load)());
                    if now_load >= max {
                        break;
                    }
                }
                let waiter = q.pop_front().expect("a head was just seen");
                let info = self.grant(st, &waiter.want, now_ms)?;
                if waiter.tx.send(info).is_err() {
                    // Left between the check and the grant.
                    self.drop_held(st, &name)?;
                }
            }
            if st.queues.get(&name).is_some_and(VecDeque::is_empty) {
                st.queues.remove(&name);
            }
        }
        Ok(())
    }

    /// In how many milliseconds [`Leases::tick`] next has something to do;
    /// `None` when nothing is time-dependent (the driver then sleeps until
    /// woken).
    pub fn next_wake(&self, now_ms: i64) -> Option<i64> {
        let st = lock(&self.state);
        let mut next: Option<i64> = None;
        let mut soonest = |t: i64| next = Some(next.map_or(t, |n| n.min(t)));
        for h in st.held.values() {
            if let Some(t) = h.expires_ms {
                soonest(t);
            }
            if let Some(t) = h.orphan_until_ms {
                soonest(t);
            }
        }
        for (name, q) in &st.queues {
            if !st.held.contains_key(name) && q.front().is_some_and(|w| w.want.max_load.is_some()) {
                soonest(now_ms + LOAD_POLL_MS);
            }
        }
        next.map(|t| (t - now_ms).max(0))
    }

    /// Every lease and every name with waiters, by name.
    pub fn list(&self) -> Vec<Info> {
        let st = lock(&self.state);
        let mut out: Vec<Info> = st
            .held
            .iter()
            .map(|(n, h)| Self::info(n, h, Self::waiting(&st, n)))
            .collect();
        for (name, q) in &st.queues {
            if !st.held.contains_key(name) && !q.is_empty() {
                out.push(Info {
                    name: name.clone(),
                    held: false,
                    holder: None,
                    acquired_ms: 0,
                    expires_ms: None,
                    bound: false,
                    waiters: Self::waiting(&st, name),
                });
            }
        }
        out.sort_by(|a, b| a.name.cmp(&b.name));
        out
    }
}

#[cfg(test)]
mod tests {
    use std::sync::atomic::{AtomicU64, Ordering};

    use super::*;
    use crate::store::{NewAgent, Store};

    struct Fixture {
        _dir: tempfile::TempDir,
        path: std::path::PathBuf,
        leases: Leases,
        load: Arc<AtomicU64>,
    }

    fn fixture() -> Fixture {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("state.db");
        let load = Arc::new(AtomicU64::new(0));
        let leases = open(&path, 0, &load);
        Fixture {
            _dir: dir,
            path,
            leases,
            load,
        }
    }

    fn open(path: &std::path::Path, now: i64, load: &Arc<AtomicU64>) -> Leases {
        let store = Arc::new(Mutex::new(Store::open(path).unwrap()));
        let load = Arc::clone(load);
        Leases::load(store, now, move || {
            f64::from_bits(load.load(Ordering::SeqCst))
        })
        .unwrap()
    }

    impl Fixture {
        fn set_load(&self, v: f64) {
            self.load.store(v.to_bits(), Ordering::SeqCst);
        }
    }

    fn want(name: &str, holder: Option<u64>, conn: u64) -> Want {
        Want {
            name: name.into(),
            holder,
            key: conn as i64 * 100,
            conn: ConnId(conn),
            bind: true,
            ttl_ms: None,
            max_load: None,
        }
    }

    fn granted(a: Acquired) -> Info {
        match a {
            Acquired::Granted(i) => i,
            _ => panic!("expected a grant"),
        }
    }

    fn queued(a: Acquired) -> oneshot::Receiver<Info> {
        match a {
            Acquired::Queued(rx) => rx,
            _ => panic!("expected to queue"),
        }
    }

    #[test]
    fn a_free_lease_is_granted_and_a_held_one_is_busy() {
        let f = fixture();
        let i = granted(f.leases.acquire(want("x", Some(1), 1), false, 10).unwrap());
        assert_eq!((i.holder, i.acquired_ms, i.bound), (Some(1), 10, true));
        match f.leases.acquire(want("x", Some(2), 2), false, 11).unwrap() {
            Acquired::Busy(Some(i)) => assert_eq!(i.holder, Some(1)),
            _ => panic!("expected busy"),
        }
        // Other names are independent.
        granted(f.leases.acquire(want("y", Some(2), 2), false, 11).unwrap());
    }

    #[test]
    fn waiters_are_served_in_order_on_release() {
        let f = fixture();
        granted(f.leases.acquire(want("x", Some(1), 1), false, 0).unwrap());
        let mut a = queued(f.leases.acquire(want("x", Some(2), 2), true, 1).unwrap());
        let mut b = queued(f.leases.acquire(want("x", None, 3), true, 2).unwrap());
        assert!(a.try_recv().is_err());
        let r = f.leases.release("x", Releaser::Agent(1), 5).unwrap();
        assert!(matches!(r, Released::Released));
        assert_eq!(a.try_recv().unwrap().holder, Some(2));
        assert!(b.try_recv().is_err(), "b waits for a");
        f.leases.release("x", Releaser::Agent(2), 6).unwrap();
        assert_eq!(b.try_recv().unwrap().holder, None);
    }

    #[test]
    fn only_the_holder_or_the_user_releases() {
        let f = fixture();
        granted(f.leases.acquire(want("x", Some(1), 1), false, 0).unwrap());
        assert!(matches!(
            f.leases.release("x", Releaser::Agent(2), 1).unwrap(),
            Released::NotYours(_)
        ));
        assert!(matches!(
            f.leases.release("nope", Releaser::User, 1).unwrap(),
            Released::NotHeld
        ));
        assert!(matches!(
            f.leases.release("x", Releaser::User, 1).unwrap(),
            Released::Released
        ));
        assert!(f.leases.list().is_empty());
    }

    #[test]
    fn ttl_releases_and_hands_over() {
        let f = fixture();
        let mut w = want("x", None, 1);
        w.bind = false;
        w.ttl_ms = Some(1000);
        let i = granted(f.leases.acquire(w, false, 100).unwrap());
        assert_eq!(i.expires_ms, Some(1100));
        let mut next = queued(f.leases.acquire(want("x", Some(2), 2), true, 200).unwrap());
        assert_eq!(f.leases.next_wake(200), Some(900));
        f.leases.tick(1099).unwrap();
        assert!(next.try_recv().is_err());
        f.leases.tick(1100).unwrap();
        assert_eq!(next.try_recv().unwrap().holder, Some(2));
    }

    #[test]
    fn closing_the_connection_releases_a_bound_lease_but_not_an_unbound_one() {
        let f = fixture();
        granted(f.leases.acquire(want("bound", Some(1), 1), false, 0).unwrap());
        let mut un = want("unbound", Some(1), 1);
        un.bind = false;
        granted(f.leases.acquire(un, false, 0).unwrap());
        let mut next = queued(f.leases.acquire(want("bound", Some(2), 2), true, 1).unwrap());
        f.leases.conn_closed(ConnId(1), 2).unwrap();
        assert_eq!(next.try_recv().unwrap().holder, Some(2));
        let names: Vec<_> = f.leases.list().into_iter().map(|i| i.name).collect();
        assert_eq!(names, ["bound", "unbound"]);
    }

    #[test]
    fn a_closed_connection_leaves_the_queue() {
        let f = fixture();
        granted(f.leases.acquire(want("x", Some(1), 1), false, 0).unwrap());
        let _gone = queued(f.leases.acquire(want("x", Some(2), 2), true, 1).unwrap());
        let mut kept = queued(f.leases.acquire(want("x", Some(3), 3), true, 1).unwrap());
        f.leases.conn_closed(ConnId(2), 2).unwrap();
        assert_eq!(f.leases.list()[0].waiters, 1);
        f.leases.release("x", Releaser::User, 3).unwrap();
        assert_eq!(kept.try_recv().unwrap().holder, Some(3));
    }

    #[test]
    fn a_dropped_receiver_is_skipped() {
        let f = fixture();
        granted(f.leases.acquire(want("x", Some(1), 1), false, 0).unwrap());
        drop(queued(f.leases.acquire(want("x", Some(2), 2), true, 1).unwrap()));
        let mut kept = queued(f.leases.acquire(want("x", Some(3), 3), true, 1).unwrap());
        f.leases.release("x", Releaser::User, 3).unwrap();
        assert_eq!(kept.try_recv().unwrap().holder, Some(3));
    }

    #[test]
    fn an_ended_agent_releases_and_stops_waiting() {
        let f = fixture();
        let mut held = want("x", Some(1), 1);
        held.bind = false;
        granted(f.leases.acquire(held, false, 0).unwrap());
        granted(f.leases.acquire(want("z", Some(2), 2), false, 0).unwrap());
        let _gone = queued(f.leases.acquire(want("z", Some(1), 1), true, 1).unwrap());
        let mut next = queued(f.leases.acquire(want("x", Some(3), 3), true, 1).unwrap());
        f.leases.agent_gone(1, 5).unwrap();
        assert_eq!(next.try_recv().unwrap().holder, Some(3));
        let z = f.leases.list().into_iter().find(|i| i.name == "z").unwrap();
        assert_eq!(z.waiters, 0);
    }

    #[test]
    fn max_load_is_granted_only_below_the_limit() {
        let f = fixture();
        f.set_load(3.0);
        let mut w = want("x", Some(1), 1);
        w.max_load = Some(2.0);
        match f.leases.acquire(w.clone(), false, 0).unwrap() {
            Acquired::LoadTooHigh(l) => assert_eq!(l, 3.0),
            _ => panic!("expected the load refusal"),
        }
        let mut rx = queued(f.leases.acquire(w, true, 0).unwrap());
        assert_eq!(f.leases.next_wake(0), Some(LOAD_POLL_MS));
        f.leases.tick(1000).unwrap();
        assert!(rx.try_recv().is_err());
        f.set_load(1.9);
        f.leases.tick(2000).unwrap();
        assert_eq!(rx.try_recv().unwrap().holder, Some(1));
        assert_eq!(f.leases.next_wake(2000), None);
    }

    #[test]
    fn the_head_waiter_blocks_those_behind_it_while_load_is_high() {
        let f = fixture();
        f.set_load(5.0);
        let mut strict = want("x", Some(1), 1);
        strict.max_load = Some(1.0);
        let mut rx1 = queued(f.leases.acquire(strict, true, 0).unwrap());
        let mut rx2 = queued(f.leases.acquire(want("x", Some(2), 2), true, 0).unwrap());
        f.leases.tick(10).unwrap();
        assert!(rx1.try_recv().is_err() && rx2.try_recv().is_err());
        f.set_load(0.5);
        f.leases.tick(20).unwrap();
        assert!(rx1.try_recv().is_ok());
        assert!(rx2.try_recv().is_err());
        // Without wait, a name with waiters is busy even though free.
        let f2 = fixture();
        f2.set_load(5.0);
        let mut s = want("x", Some(1), 1);
        s.max_load = Some(1.0);
        let _rx = queued(f2.leases.acquire(s, true, 0).unwrap());
        assert!(matches!(
            f2.leases.acquire(want("x", Some(2), 2), false, 0).unwrap(),
            Acquired::Busy(None)
        ));
    }

    #[test]
    fn leases_survive_a_restart_and_a_bound_one_is_reclaimed_by_its_key() {
        let f = fixture();
        let mut un = want("long", None, 1);
        un.bind = false;
        un.ttl_ms = Some(60_000);
        granted(f.leases.acquire(un, false, 1000).unwrap());
        let w = want("held", Some(7), 2);
        granted(f.leases.acquire(w.clone(), false, 1000).unwrap());
        let live = Store::open(&f.path)
            .unwrap()
            .insert_agent(&NewAgent {
                program: "p".into(),
                args: vec![],
                cwd: "/".into(),
                created_unix_ms: 1,
                scrollback_path: "/s".into(),
                labels: Default::default(),
                priority: 0,
            })
            .unwrap();
        let mut by_agent = want("agent-held", Some(live), 3);
        by_agent.bind = false;
        granted(f.leases.acquire(by_agent, false, 1000).unwrap());
        drop(f.leases);

        // The "new daemon" at 2000 ms: agent 7 does not exist.
        let leases = open(&f.path, 2000, &f.load);
        let names: Vec<_> = leases.list().into_iter().map(|i| i.name).collect();
        assert_eq!(names, ["agent-held", "long"], "agent 7's lease went with it");
        // `long` is unbound and keeps its expiry; nothing is due before it.
        assert_eq!(leases.next_wake(2000), Some(59_000));
    }

    #[test]
    fn a_bound_lease_from_the_store_is_reclaimed_by_its_key_or_released_after_the_grace() {
        let f = fixture();
        let w = want("held", None, 2);
        granted(f.leases.acquire(w.clone(), false, 1000).unwrap());
        drop(f.leases);

        let leases = open(&f.path, 2000, &f.load);
        assert_eq!(leases.next_wake(2000), Some(RECLAIM_GRACE_MS));
        // Someone else is refused meanwhile.
        assert!(matches!(
            leases.acquire(want("held", None, 9), false, 2001).unwrap(),
            Acquired::Busy(Some(_))
        ));
        // The same key on a new connection reclaims it, keeping acquired_ms.
        let mut again = w.clone();
        again.conn = ConnId(50);
        let i = granted(leases.acquire(again, false, 3000).unwrap());
        assert_eq!(i.acquired_ms, 1000);
        assert_eq!(leases.next_wake(3000), None);
        leases.conn_closed(ConnId(50), 3500).unwrap();
        assert!(leases.list().is_empty());

        // Unclaimed, it goes after the grace.
        let f = fixture();
        granted(f.leases.acquire(w, false, 1000).unwrap());
        drop(f.leases);
        let leases = open(&f.path, 2000, &f.load);
        leases.tick(2000 + RECLAIM_GRACE_MS - 1).unwrap();
        assert_eq!(leases.list().len(), 1);
        leases.tick(2000 + RECLAIM_GRACE_MS).unwrap();
        assert!(leases.list().is_empty());
        // ... and is gone from the store.
        drop(leases);
        assert!(Store::open(&f.path).unwrap().leases().unwrap().is_empty());
    }
}
