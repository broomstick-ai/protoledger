//! Following bound locations: a filesystem watcher with a debounce, and a
//! periodic poll as the fallback (and the only path when the watcher is off
//! or cannot watch a location).

use super::{Follows, Synced, sidecar, stamp, sync_with, wal};
use crate::store::Store;
use anyhow::Result;
use notify::{RecommendedWatcher, RecursiveMode, Watcher};
use std::collections::{HashMap, HashSet};
use std::path::{Path, PathBuf};
use std::sync::mpsc::{Receiver, RecvTimeoutError, channel};
use std::time::{Duration, Instant};

/// The periods a [`Follower`] waits on.
#[derive(Debug, Clone, Copy)]
pub struct FollowOptions {
    /// After the watcher reports a change, wait until the location has been
    /// quiet this long before reading it.
    pub debounce: Duration,
    /// Every this often, read each active location whose files changed
    /// (size or modification time), whatever the watcher said.
    pub poll: Duration,
    /// Use the filesystem watcher. Off, the poll does all the work.
    pub watcher: bool,
}

impl Default for FollowOptions {
    fn default() -> Self {
        FollowOptions { debounce: Duration::from_millis(150), poll: Duration::from_secs(1), watcher: true }
    }
}

/// What made a sync run.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Trigger {
    /// The watcher reported a change, and the debounce period passed.
    Watcher,
    /// The periodic poll found the location's files changed.
    Poll,
}

/// Follows every active binding of a store.
pub struct Follower {
    opts: FollowOptions,
    rx: Receiver<notify::Result<notify::Event>>,
    watcher: Option<RecommendedWatcher>,
    /// binding → (location, sidecar dir), canonicalised as the watcher reports paths.
    watched: HashMap<String, (PathBuf, Option<PathBuf>)>,
    /// Directories handed to the watcher.
    dirs: HashSet<PathBuf>,
    /// binding → when its debounce ends.
    pending: HashMap<String, Instant>,
    next_poll: Instant,
    /// What the formats keep between reads of the followed files.
    follows: Follows,
    /// How many times a location was synced.
    syncs: u64,
}

fn canon(p: &Path) -> PathBuf {
    p.canonicalize().unwrap_or_else(|_| p.to_path_buf())
}

impl Follower {
    pub fn new(store: &Store, opts: FollowOptions) -> Result<Follower> {
        let (tx, rx) = channel();
        let watcher = if opts.watcher { Some(notify::recommended_watcher(tx)?) } else { None };
        let mut f = Follower {
            opts,
            rx,
            watcher,
            watched: HashMap::new(),
            dirs: HashSet::new(),
            pending: HashMap::new(),
            next_poll: Instant::now() + opts.poll,
            follows: Follows::default(),
            syncs: 0,
        };
        f.reconcile(store)?;
        Ok(f)
    }

    /// Watch the locations of bindings that became active; forget the rest.
    fn reconcile(&mut self, store: &Store) -> Result<()> {
        let active: HashMap<String, PathBuf> = store.bindings()?.into_iter().filter(|b| b.active).map(|b| (b.binding, PathBuf::from(b.location))).collect();
        self.watched.retain(|b, _| active.contains_key(b));
        self.pending.retain(|b, _| active.contains_key(b));
        self.follows.retain(|b| active.contains_key(b));
        for (b, loc) in active {
            if self.watched.contains_key(&b) {
                continue;
            }
            let loc = canon(&loc);
            let side = if loc.is_dir() { None } else { sidecar(&loc).map(|s| canon(&s)) };
            if let Some(w) = self.watcher.as_mut() {
                // A file is watched through its directory, so a rewrite by
                // rename is seen; a directory (or sidecar) recursively.
                let dir = if loc.is_dir() { loc.clone() } else { loc.parent().map(Path::to_path_buf).unwrap_or_default() };
                if self.dirs.insert(dir.clone()) {
                    // A location that cannot be watched is left to the poll.
                    if w.watch(&dir, RecursiveMode::Recursive).is_err() {
                        self.dirs.remove(&dir);
                    }
                }
            }
            self.watched.insert(b, (loc, side));
        }
        Ok(())
    }

    /// The bindings this follower watches, sorted.
    pub fn followed(&self) -> Vec<String> {
        let mut out: Vec<String> = self.watched.keys().cloned().collect();
        out.sort();
        out
    }

    /// How many times this follower has synced a location (read it, whole or
    /// from where it stopped), whatever the sync found.
    pub fn syncs(&self) -> u64 {
        self.syncs
    }

    /// The bindings whose locations this follower reads from where the last sync stopped.
    pub fn incremental(&self) -> Vec<String> {
        self.follows.bindings()
    }

    fn sync(&mut self, store: &mut Store, binding: &str) -> Result<Synced> {
        self.syncs += 1;
        sync_with(store, binding, &mut self.follows)
    }

    fn bindings_for(&self, path: &Path) -> Vec<String> {
        let path = canon(path);
        self.watched
            .iter()
            .filter(|(_, (loc, side))| {
                path == *loc
                    || (loc.is_dir() && path.starts_with(loc))
                    || (!loc.is_dir() && wal(loc).is_some_and(|w| path == w))
                    || side.as_ref().is_some_and(|s| path.starts_with(s))
            })
            .map(|(b, _)| b.clone())
            .collect()
    }

    /// Wait for changes until `until`, syncing as they come. Returns as soon
    /// as a sync changed the store (with every sync that did, and what made
    /// it run), or with nothing when `until` passes.
    pub fn step(&mut self, store: &mut Store, until: Instant) -> Result<Vec<(Trigger, Synced)>> {
        loop {
            let now = Instant::now();
            let mut out = Vec::new();
            // Debounced watcher triggers that are due.
            let due: Vec<String> = self.pending.iter().filter(|(_, t)| **t <= now).map(|(b, _)| b.clone()).collect();
            for b in due {
                self.pending.remove(&b);
                let s = self.sync(store, &b)?;
                if s.changed() {
                    out.push((Trigger::Watcher, s));
                }
            }
            // The poll.
            if now >= self.next_poll {
                self.next_poll = now + self.opts.poll;
                store.refresh()?;
                self.reconcile(store)?;
                let active: Vec<String> = self.watched.keys().cloned().collect();
                for b in active {
                    let Some(row) = store.binding(&b)? else { continue };
                    let cur = store.cursor(&b)?.and_then(|c| super::Cursor::parse(&c)).map(|c| c.stamp);
                    let now_stamp = stamp(Path::new(&row.location)).ok();
                    if now_stamp.is_some() && cur == now_stamp {
                        continue;
                    }
                    let s = self.sync(store, &b)?;
                    if s.changed() {
                        out.push((Trigger::Poll, s));
                    }
                }
            }
            if !out.is_empty() {
                self.reconcile(store)?; // a diverged binding is no longer followed
                return Ok(out);
            }
            let now = Instant::now();
            if now >= until {
                return Ok(out);
            }
            let mut wake = until.min(self.next_poll);
            if let Some(t) = self.pending.values().min() {
                wake = wake.min(*t);
            }
            match self.rx.recv_timeout(wake.saturating_duration_since(now)) {
                Ok(Ok(ev)) => {
                    let at = Instant::now() + self.opts.debounce;
                    for p in &ev.paths {
                        for b in self.bindings_for(p) {
                            // Each change restarts the binding's debounce.
                            self.pending.insert(b, at);
                        }
                    }
                }
                Ok(Err(_)) | Err(RecvTimeoutError::Timeout) => {}
                Err(RecvTimeoutError::Disconnected) => {
                    // No watcher (or it died): the poll carries on alone.
                    std::thread::sleep(wake.saturating_duration_since(Instant::now()));
                }
            }
        }
    }
}
