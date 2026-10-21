//! The engine: the one thread that owns the [`Store`], follows its bindings,
//! runs every request against it, and fans published events out.
//!
//! **The seam.** A client asks for each agent's events strictly after its
//! cursor `from`. When it subscribes, the engine (which is the only thing
//! that publishes, so nothing can slip in between) first forwards everything
//! the hub holds, then takes each agent's indexed last seq `snap`. The client
//! is then registered for live events with a floor of `max(from, snap)` per
//! agent, and handed a backfill: the ledger's events in `(from, snap]`, read
//! through the store's reader of the ledger. Every event of the agent is at or below `snap` (in the
//! ledger: backfill) or above it (published later: live), so there is no
//! hole; the floor keeps the hub from repeating anything the backfill holds,
//! even an event the hub only learns of later (an agent it had not covered
//! yet), so there is no duplicate.
//!
//! **The hub** is one [`Store::subscribe`] over every agent known to it, at
//! the seq it has forwarded through. An agent new to the store (imported,
//! attached, synced in, or found on disk by a refresh) is added by
//! re-subscribing with it at 0: the store backfills it, and the floors above
//! drop what a client already has.

use super::after;
use crate::store::{LedgerReader, Published, Since, Store};
use crate::sync::{self, FollowOptions, Follower};
use anyhow::{Result, anyhow};
use std::collections::{HashMap, HashSet};
use std::path::Path;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::mpsc::{Receiver, Sender, TryRecvError};
use std::time::{Duration, Instant};
use tokio::sync::mpsc::{UnboundedReceiver, UnboundedSender, unbounded_channel};

/// How long the engine waits on the sync engine before looking at requests
/// again: the longest a request waits for the engine to pick it up.
const TICK: Duration = Duration::from_millis(20);

pub(crate) type Job = Box<dyn FnOnce(&mut Engine) + Send>;

/// Sends work to the engine thread.
#[derive(Clone)]
pub(crate) struct Handle {
    jobs: Sender<Job>,
}

impl Handle {
    pub(crate) fn new(jobs: Sender<Job>) -> Handle {
        Handle { jobs }
    }

    /// Run `f` on the engine thread, against the store, and return its answer.
    pub(crate) async fn call<T: Send + 'static>(&self, f: impl FnOnce(&mut Engine) -> T + Send + 'static) -> Result<T> {
        let (tx, rx) = tokio::sync::oneshot::channel();
        self.jobs
            .send(Box::new(move |e: &mut Engine| {
                let _ = tx.send(f(e));
            }))
            .map_err(|_| anyhow!("the engine has stopped"))?;
        rx.await.map_err(|_| anyhow!("the engine dropped the request"))
    }
}

/// Where a subscription starts: every agent from 0 (and agents created
/// later), or the listed agents, each strictly after its seq.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum From {
    All,
    Agents(Vec<(String, u64)>),
}

/// The ledger events one agent owes a new client: those in `(from, through]`,
/// read through the store's reader of its ledger starting at decompressed
/// offset `offset` (the line of seq `max(from, 1)`).
#[derive(Debug, Clone)]
pub(crate) struct Backfill {
    pub agent: String,
    pub reader: LedgerReader,
    pub offset: u64,
    pub from: u64,
    pub through: u64,
}

impl Backfill {
    /// Read it. Blocking file IO.
    pub(crate) fn read(&self) -> Result<Vec<Published>> {
        let (evs, _) = self.reader.tail(self.offset)?;
        Ok(evs
            .into_iter()
            .filter(|(_, e)| after(self.from, e.seq) && e.seq <= self.through)
            .map(|(offset, event)| Published { agent: self.agent.clone(), seq: event.seq, offset, event })
            .collect())
    }
}

struct Client {
    tx: UnboundedSender<Published>,
    /// `None` = every agent.
    agents: Option<HashSet<String>>,
    /// Per agent, the seq live events must come after (`max(from, snap)`);
    /// an agent not listed starts from 0.
    floor: HashMap<String, u64>,
}

impl Client {
    /// Offer one live event; false once the client has gone.
    fn offer(&self, p: &Published) -> bool {
        if self.agents.as_ref().is_some_and(|a| !a.contains(&p.agent)) {
            return !self.tx.is_closed();
        }
        if !after(self.floor.get(&p.agent).copied().unwrap_or(0), p.seq) {
            return !self.tx.is_closed();
        }
        self.tx.send(p.clone()).is_ok()
    }
}

pub(crate) struct Engine {
    pub(crate) store: Store,
    follower: Follower,
    opts: FollowOptions,
    hub: Receiver<Published>,
    /// Agents the hub covers → the last seq it forwarded (or the seq it
    /// started after).
    hub_seen: HashMap<String, u64>,
    clients: Vec<Client>,
    next_agent_check: Instant,
}

/// The engine thread's body: open, report ready, then serve until `stop` is
/// set or every [`Handle`] is gone.
pub(crate) fn run(root: &Path, opts: FollowOptions, jobs: Receiver<Job>, stop: &AtomicBool, ready: Sender<Result<()>>) -> Result<()> {
    let mut e = match Engine::open(root, opts) {
        Ok(e) => {
            let _ = ready.send(Ok(()));
            e
        }
        Err(err) => {
            let _ = ready.send(Err(err));
            return Ok(());
        }
    };
    loop {
        loop {
            match jobs.try_recv() {
                Ok(job) => {
                    job(&mut e);
                    e.settle();
                }
                Err(TryRecvError::Empty) => break,
                Err(TryRecvError::Disconnected) => return Ok(()),
            }
        }
        if stop.load(Ordering::SeqCst) {
            return Ok(());
        }
        match e.follower.step(&mut e.store, Instant::now() + TICK) {
            Ok(r) if !r.is_empty() => e.settle(),
            Ok(_) => {
                if Instant::now() >= e.next_agent_check {
                    e.settle();
                }
            }
            Err(err) => eprintln!("protoledger serve: sync: {err:#}"),
        }
        e.pump();
    }
}

impl Engine {
    fn open(root: &Path, opts: FollowOptions) -> Result<Engine> {
        let mut store = Store::open(root)?;
        // Catch up with whatever the bound locations gained while nobody followed them.
        for s in sync::sync_all(&mut store)? {
            if let Some(d) = &s.diverged {
                eprintln!("protoledger serve: binding {} diverged and is no longer followed: {d}", s.binding);
            }
        }
        let follower = Follower::new(&store, opts)?;
        let hub_seen: HashMap<String, u64> = store.agents()?.into_iter().map(|a| (a.id, a.last_seq)).collect();
        let hub = store.subscribe(Since::Agents(hub_seen.iter().map(|(a, s)| (a.clone(), *s)).collect()))?;
        let mut e = Engine { store, follower, opts, hub, hub_seen, clients: Vec::new(), next_agent_check: Instant::now() };
        e.settle();
        Ok(e)
    }

    /// Forward every event the hub holds to the clients that want it.
    fn pump(&mut self) {
        while let Ok(p) = self.hub.try_recv() {
            self.hub_seen.insert(p.agent.clone(), p.seq);
            self.clients.retain(|c| c.offer(&p));
        }
    }

    /// Pump, then cover agents new to the store. Run after anything that may
    /// have created one, and at least once per poll period (a refresh can
    /// find a ledger written by another program).
    fn settle(&mut self) {
        self.next_agent_check = Instant::now() + self.opts.poll;
        if let Err(err) = self.rehub() {
            eprintln!("protoledger serve: {err:#}");
        }
    }

    fn rehub(&mut self) -> Result<()> {
        self.pump();
        let agents = self.store.agents()?;
        if agents.iter().all(|a| self.hub_seen.contains_key(&a.id)) {
            return Ok(());
        }
        for a in &agents {
            self.hub_seen.entry(a.id.clone()).or_insert(0);
        }
        let since = self.hub_seen.iter().map(|(a, s)| (a.clone(), *s)).collect();
        // The old hub may be sent a refresh's events before it is dropped;
        // the new one's backfill holds them too (they are past `hub_seen`).
        self.hub = self.store.subscribe(Since::Agents(since))?;
        self.pump();
        Ok(())
    }

    /// The bindings changed: follow exactly the active ones now.
    pub(crate) fn rebind(&mut self) -> Result<()> {
        self.follower = Follower::new(&self.store, self.opts)?;
        Ok(())
    }

    /// Register a WebSocket client. Returns its live channel and the
    /// backfill it is owed first (see the module docs for the seam).
    pub(crate) fn subscribe(&mut self, from: &From) -> Result<(UnboundedReceiver<Published>, Vec<Backfill>)> {
        self.rehub()?;
        let (targets, agents): (Vec<(String, u64)>, Option<HashSet<String>>) = match from {
            From::All => (self.store.agents()?.into_iter().map(|a| (a.id, 0)).collect(), None),
            From::Agents(v) => (v.clone(), Some(v.iter().map(|(a, _)| a.clone()).collect())),
        };
        let mut floor = HashMap::new();
        let mut backfill = Vec::new();
        for (agent, from) in targets {
            let snap = self.store.agent(&agent)?.map(|a| a.last_seq).unwrap_or(0);
            floor.insert(agent.clone(), from.max(snap));
            if !after(from, snap) {
                continue;
            }
            if let Some(row) = self.store.events(&agent, from.max(1), 1)?.pop() {
                let reader = self.store.reader(&agent)?;
                backfill.push(Backfill { agent, reader, offset: row.offset, from, through: snap });
            }
        }
        let (tx, rx) = unbounded_channel();
        self.clients.push(Client { tx, agents, floor });
        Ok((rx, backfill))
    }
}
