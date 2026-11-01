//! The store's read cache (SPEC §9.6): one byte-bounded, least-recently-used
//! cache per [`Store`](super::Store), shared by every reader the store hands
//! out (they may run on other threads: `serve` reads backfills off its
//! engine thread), holding two kinds of entry in one budget:
//!
//! - **lines**: one line of a ledger's decompressed stream, keyed by
//!   `(agent, decompressed offset)`, as its exact bytes and parsed (an event,
//!   or the header at offset 0). The reader seam is offset-addressed
//!   (SPEC §10.5), and an offset names a line as permanently as a seq does:
//!   the stream is append-only and recompression or migration never changes
//!   a byte of it (SPEC §10.1). A hit that the caller knows the seq of is
//!   checked against it.
//! - **renders**: a thread (an agent and its structural descendants)
//!   exported as one format with one set of options, keyed by that and by
//!   the thread's *view fingerprint*: every ledger of the thread with its
//!   last seq and decompressed length. Everything that changes the projected
//!   view (a new event, a redaction or unredaction fact, a branch, a new
//!   subagent) is an append to one of those ledgers or a new ledger among
//!   them, so it changes the fingerprint, and the stale render is never
//!   looked up again; it ages out.
//!
//! An entry weighs what it holds plus [`ENTRY_OVERHEAD`]: a line, its length
//! twice (its bytes, and its parsed form, estimated at as many again); a
//! render, the bytes of every file it holds and their paths. Inserting evicts
//! the least recently used entries until the total is within the budget; an
//! entry heavier than the whole budget is not cached. A budget of 0 is no
//! cache: the store's readers then carry none and read exactly as they did
//! without one. Each agent read also has an anchor (below), a few hundred
//! bytes kept outside the budget until its entries are dropped.
//!
//! Every entry is of one ledger, and the cache never takes the index's word
//! for which (the index is shared: another handle or process catching it up
//! records a ledger made again under the same id as if it were this one).
//! Each agent has an *anchor*: its files as the cache last saw them (device,
//! inode, birth time, length and modification time of each, one stat) and
//! the last [`PROBE_BYTES`] of the file it grows by. Every answer begins with
//! a stat of the files ([`ReadCache::validate`]): exactly as anchored, the
//! entries stand and nothing is read; grown as appends grow them (the same
//! file, longer, its bytes before the old end unchanged) or as seals and
//! recompressions do (a compressed ledger's block file grown or replaced
//! under the same hot tail), they stand; anything else (other files: garbage
//! collection and a ledger made again, by any handle or process, or a
//! migration; a file shrunk, rewritten at its length, or rewritten just
//! before its old end) drops every entry of the agent and begins a new
//! epoch, and a read begun under the old epoch puts nothing. A render is
//! answered only once every ledger of its thread has passed that check
//! (the view fingerprint alone cannot tell a ledger made again with the
//! same seqs and lengths). Entries are
//! also dropped when the ledger is removed or found damaged, and when a
//! catch-up re-reads it whole and any entry differs from the line at its
//! offset.
//!
//! The one change this cannot see is a rewrite in place that grows the
//! file and leaves its last [`PROBE_BYTES`] before the old end as they were:
//! a violation of append-only (SPEC §1.2) that no stat tells from an append,
//! and that only reading every byte the cache holds would find.

use super::layout::{self, FileStat, Files, Seen};
use crate::formats::Exported;
use crate::model::{Event, Header};
use serde::Serialize;
use std::collections::{BTreeMap, HashMap};
use std::sync::{Arc, Mutex};

/// The default budget: 256 MiB.
pub const DEFAULT_CACHE_BYTES: u64 = 256 * 1024 * 1024;

/// What every entry weighs beyond its contents: its key, its slot and its
/// place in the recency order.
pub const ENTRY_OVERHEAD: u64 = 128;

/// How many bytes before the end of the file a ledger grows by an anchor
/// keeps, to tell an append from a rewrite that grew the file.
pub const PROBE_BYTES: u64 = 256;

/// Which ledger an agent's entries were read from (SPEC §9.6): its files as
/// the last check of them saw them, the last bytes of the file it grows by
/// then, and the epoch every entry of the agent was put under. An entry put
/// under another epoch is refused, so a read that began before the ledger
/// was found changed never fills the cache.
#[derive(Debug, Clone)]
struct Anchor {
    epoch: u64,
    seen: Seen,
    /// `(offset, bytes)`: the last [`PROBE_BYTES`] of the growing file (the
    /// plain file, or the hot tail) as `seen` saw it.
    probe: Option<(u64, Box<[u8]>)>,
}

/// How a ledger's files changed between two looks at them.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Change {
    Same,
    /// The same ledger, grown as appends and seals grow it; `check`: the
    /// bytes before the old end of the growing file are to be compared.
    Grown {
        check: bool,
    },
    /// Another ledger, or this one rewritten in a way no append, seal or
    /// recompression rewrites it.
    Other,
}

/// What became of a compressed ledger's block file.
enum Zst {
    Unchanged,
    /// Sealed into (grown in place, or made by the first seal), or replaced
    /// under the same hot tail (a recompression).
    Moved,
}

/// How `n` follows from `a`. The ledger's identity is the file it grows by:
/// the plain file, or the hot tail, which is made with a compressed ledger
/// and kept (emptied by every seal and recompression) until the ledger
/// leaves that form. Garbage collection and a ledger made again under the
/// same id, by any handle or process, make new files; a migration does too.
fn classify(a: &Seen, n: &Seen) -> Change {
    if a == n {
        return Change::Same;
    }
    match (a, n) {
        (Seen::Plain(a), Seen::Plain(n)) if a.same_file(n) && n.size > a.size => Change::Grown { check: true },
        (Seen::Compressed { zst: az, tail: at }, Seen::Compressed { zst: nz, tail: nt }) => {
            let tail: Option<(&FileStat, &FileStat)> = match (at, nt) {
                (Some(a), Some(n)) if a.same_file(n) => Some((a, n)),
                (Some(_), _) => return Change::Other,
                (None, _) => None,
            };
            let zst = match (az, nz) {
                (None, None) => Zst::Unchanged,
                (None, Some(_)) => Zst::Moved,
                (Some(_), None) => return Change::Other,
                (Some(a), Some(n)) if a == n => Zst::Unchanged,
                (Some(a), Some(n)) if a.same_file(n) && n.size > a.size => Zst::Moved,
                (Some(a), Some(n)) if a.same_file(n) => return Change::Other,
                (Some(_), Some(_)) if tail.is_some() => Zst::Moved,
                (Some(_), Some(_)) => return Change::Other,
            };
            match (zst, tail) {
                (Zst::Moved, _) => Change::Grown { check: false },
                (Zst::Unchanged, Some((a, n))) if n.size > a.size => Change::Grown { check: true },
                // A hot tail made next to the block file (a writer opened it).
                (Zst::Unchanged, None) => Change::Grown { check: false },
                _ => Change::Other,
            }
        }
        _ => Change::Other,
    }
}

/// The last [`PROBE_BYTES`] of the file `seen` grows by, read now; `None`
/// when it is no longer that file or no longer that long.
fn probe_of(files: &Files, seen: &Seen) -> Option<Option<(u64, Box<[u8]>)>> {
    match seen.growing(files) {
        Some((p, st)) if st.size > 0 => {
            let at = st.size.saturating_sub(PROBE_BYTES);
            Some(Some((at, layout::bytes_of(p, &st, at, (st.size - at) as usize)?.into_boxed_slice())))
        }
        _ => Some(None),
    }
}

/// One line of a ledger's decompressed stream, parsed.
#[derive(Debug)]
pub(crate) struct Line {
    /// Its exact bytes, `\n` included.
    pub bytes: Box<[u8]>,
    pub parsed: Parsed,
}

#[derive(Debug)]
pub(crate) enum Parsed {
    Header(Box<Header>),
    Event(Box<Event>),
    /// A line holding nothing but white space, which a reader skips.
    Blank,
}

impl Line {
    fn weight(&self) -> u64 {
        2 * self.bytes.len() as u64 + ENTRY_OVERHEAD
    }
}

/// What a render entry is: which thread, as what, with which options.
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub(crate) struct RenderKey {
    pub agent: String,
    pub format: String,
    pub variant: Option<String>,
    pub carry: bool,
    pub target_version: Option<String>,
    /// Every ledger of the thread: `(agent, last seq, decompressed length)`.
    pub view: Vec<(String, u64, u64)>,
}

/// One thread rendered: the export, and the id of the bound agent's last
/// event in the projected view (what a `protoledger/projected` fact names as
/// `through`, SPEC §11.2).
#[derive(Debug)]
pub struct Render {
    pub exported: Exported,
    pub through: Option<String>,
}

impl Render {
    fn weight(&self) -> u64 {
        let files: u64 = self.exported.output.files.iter().map(|(k, v)| (k.len() + v.len()) as u64).sum();
        files + self.through.as_ref().map_or(0, |t| t.len() as u64) + ENTRY_OVERHEAD
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Hash)]
enum Key {
    Line(Arc<str>, u64),
    Render(Box<RenderKey>),
}

impl Key {
    fn names(&self, agent: &str) -> bool {
        match self {
            Key::Line(a, _) => &**a == agent,
            Key::Render(k) => k.view.iter().any(|(a, _, _)| a == agent),
        }
    }
    fn is_line(&self) -> bool {
        matches!(self, Key::Line(..))
    }
}

#[derive(Debug, Clone)]
enum Value {
    Line(Arc<Line>),
    Render(Arc<Render>),
}

struct Slot {
    value: Value,
    weight: u64,
    tick: u64,
}

/// One kind of entry's counters.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize)]
pub struct KindStats {
    /// Lookups that found an entry.
    pub hits: u64,
    /// Lookups that found none.
    pub misses: u64,
    /// Entries put in.
    pub inserts: u64,
    /// Entries evicted to keep within the budget.
    pub evictions: u64,
    /// Entries not cached because each alone weighs more than the budget.
    pub oversized: u64,
    /// Entries dropped because their ledger was removed, damaged or rewritten.
    pub dropped: u64,
    /// Entries held now.
    pub entries: u64,
    /// What they weigh now.
    pub bytes: u64,
}

/// [`Store::cache_stats`](super::Store::cache_stats).
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize)]
pub struct CacheStats {
    /// The budget in bytes; 0 when the cache is off.
    pub budget: u64,
    /// What every entry weighs now; never more than `budget`.
    pub bytes: u64,
    pub entries: u64,
    /// Lines of ledgers.
    pub events: KindStats,
    /// Threads rendered into a harness.
    pub projections: KindStats,
}

struct Inner {
    budget: u64,
    tick: u64,
    /// Which ledger each agent's entries were read from.
    anchors: HashMap<Arc<str>, Anchor>,
    /// The last epoch given out.
    epochs: u64,
    map: HashMap<Key, Slot>,
    /// Recency: the oldest tick first.
    order: BTreeMap<u64, Key>,
    bytes: u64,
    events: KindStats,
    projections: KindStats,
}

impl Inner {
    fn kind(&mut self, line: bool) -> &mut KindStats {
        if line { &mut self.events } else { &mut self.projections }
    }

    fn get(&mut self, key: &Key) -> Option<Value> {
        self.tick += 1;
        let tick = self.tick;
        let found = match self.map.get_mut(key) {
            Some(slot) => {
                self.order.remove(&slot.tick);
                slot.tick = tick;
                Some(slot.value.clone())
            }
            None => None,
        };
        match &found {
            Some(_) => {
                self.order.insert(tick, key.clone());
                self.kind(key.is_line()).hits += 1;
            }
            None => self.kind(key.is_line()).misses += 1,
        }
        found
    }

    fn remove(&mut self, key: &Key) -> Option<Slot> {
        let slot = self.map.remove(key)?;
        self.order.remove(&slot.tick);
        self.bytes -= slot.weight;
        let k = self.kind(key.is_line());
        k.entries -= 1;
        k.bytes -= slot.weight;
        Some(slot)
    }

    fn put(&mut self, key: Key, value: Value, weight: u64) {
        if self.budget == 0 {
            return;
        }
        self.remove(&key);
        if weight > self.budget {
            self.kind(key.is_line()).oversized += 1;
            return;
        }
        self.tick += 1;
        let tick = self.tick;
        self.order.insert(tick, key.clone());
        let line = key.is_line();
        self.map.insert(key, Slot { value, weight, tick });
        self.bytes += weight;
        let k = self.kind(line);
        k.inserts += 1;
        k.entries += 1;
        k.bytes += weight;
        self.evict();
    }

    /// Evict the least recently used until within the budget.
    fn evict(&mut self) {
        while self.bytes > self.budget {
            let Some((_, key)) = self.order.pop_first() else { break };
            if let Some(slot) = self.map.remove(&key) {
                self.bytes -= slot.weight;
                let k = self.kind(key.is_line());
                k.entries -= 1;
                k.bytes -= slot.weight;
                k.evictions += 1;
            }
        }
    }

    /// Whether `epoch` is `agent`'s epoch now.
    fn current(&self, agent: &str, epoch: u64) -> bool {
        self.anchors.get(agent).is_some_and(|a| a.epoch == epoch)
    }

    /// Drop every entry of `agent` and its anchor.
    fn drop_agent(&mut self, agent: &str) {
        self.anchors.remove(agent);
        self.drop_where(|k| k.names(agent));
    }

    fn drop_where(&mut self, f: impl Fn(&Key) -> bool) {
        let keys: Vec<Key> = self.map.keys().filter(|k| f(k)).cloned().collect();
        for k in keys {
            if self.remove(&k).is_some() {
                self.kind(k.is_line()).dropped += 1;
            }
        }
    }
}

/// The cache. Every method takes one lock, briefly; nothing is read from
/// disk under it.
pub(crate) struct ReadCache {
    inner: Mutex<Inner>,
}

impl std::fmt::Debug for ReadCache {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "ReadCache({:?})", self.stats())
    }
}

impl ReadCache {
    pub(crate) fn new(budget: u64) -> ReadCache {
        ReadCache {
            inner: Mutex::new(Inner {
                budget,
                tick: 0,
                anchors: HashMap::new(),
                epochs: 0,
                map: HashMap::new(),
                order: BTreeMap::new(),
                bytes: 0,
                events: KindStats::default(),
                projections: KindStats::default(),
            }),
        }
    }

    fn lock(&self) -> std::sync::MutexGuard<'_, Inner> {
        // A panic under the lock leaves the counters, never a half-made
        // entry (every change is made in full before it is visible).
        self.inner.lock().unwrap_or_else(|p| p.into_inner())
    }

    pub(crate) fn budget(&self) -> u64 {
        self.lock().budget
    }

    /// Change the budget, evicting what no longer fits (0 empties it).
    pub(crate) fn set_budget(&self, budget: u64) {
        let mut g = self.lock();
        g.budget = budget;
        g.evict();
    }

    pub(crate) fn stats(&self) -> CacheStats {
        let g = self.lock();
        CacheStats { budget: g.budget, bytes: g.bytes, entries: g.map.len() as u64, events: g.events, projections: g.projections }
    }

    pub(crate) fn line(&self, agent: &Arc<str>, offset: u64) -> Option<Arc<Line>> {
        match self.lock().get(&Key::Line(agent.clone(), offset)) {
            Some(Value::Line(l)) => Some(l),
            _ => None,
        }
    }

    /// Put a line read under `epoch` ([`ReadCache::validate`]); refused when
    /// the agent's ledger was found changed since.
    pub(crate) fn put_line(&self, agent: &Arc<str>, offset: u64, line: Arc<Line>, epoch: u64) {
        let w = line.weight();
        let mut g = self.lock();
        if g.current(agent, epoch) {
            g.put(Key::Line(agent.clone(), offset), Value::Line(line), w);
        }
    }

    /// Check that `agent`'s entries were read from the ledger whose files
    /// are `now` (one stat of them), and give the epoch to answer and fill
    /// the cache under; `None`: read from disk this time, without the cache.
    ///
    /// Files exactly as last seen: the entries stand, and nothing is read.
    /// Grown as appends and seals grow them: they stand, once the last
    /// [`PROBE_BYTES`] before the growing file's old end are what they were.
    /// Anything else (other files: a ledger removed and made again by any
    /// handle or process, a migration; a file shrunk, or rewritten at its
    /// size, or rewritten before its old end): every entry of the agent is
    /// dropped and a new epoch begins. Disk is read outside the lock.
    pub(crate) fn validate(&self, agent: &Arc<str>, files: &Files, now: &Seen) -> Option<u64> {
        let held = {
            let g = self.lock();
            match g.anchors.get(&**agent) {
                Some(a) if a.seen == *now => return Some(a.epoch),
                a => a.cloned(),
            }
        };
        let mut change = held.as_ref().map_or(Change::Other, |a| classify(&a.seen, now));
        if change == (Change::Grown { check: true })
            && let Some((at, bytes)) = held.as_ref().and_then(|a| a.probe.as_ref())
        {
            let same = now.growing(files).and_then(|(p, st)| layout::bytes_of(p, &st, *at, bytes.len())).is_some_and(|b| *b == **bytes);
            if !same {
                change = Change::Other;
            }
        }
        let probe = probe_of(files, now)?;
        let mut g = self.lock();
        if g.anchors.get(&**agent).map(|a| (a.epoch, &a.seen)) != held.as_ref().map(|a| (a.epoch, &a.seen)) {
            return None; // another reader got there first
        }
        let epoch = match (change, held) {
            (Change::Same | Change::Grown { .. }, Some(a)) => a.epoch,
            (_, held) => {
                if held.is_some() {
                    g.drop_agent(agent);
                }
                g.epochs += 1;
                g.epochs
            }
        };
        g.anchors.insert(agent.clone(), Anchor { epoch, seen: now.clone(), probe });
        Some(epoch)
    }

    pub(crate) fn render(&self, key: &RenderKey) -> Option<Arc<Render>> {
        match self.lock().get(&Key::Render(Box::new(key.clone()))) {
            Some(Value::Render(r)) => Some(r),
            _ => None,
        }
    }

    /// Put a render read under `epochs`, one per ledger of `key.view` in
    /// its order; refused when any of them was found changed since.
    pub(crate) fn put_render(&self, key: RenderKey, r: Arc<Render>, epochs: &[u64]) {
        let w = r.weight();
        let mut g = self.lock();
        if key.view.len() == epochs.len() && key.view.iter().zip(epochs).all(|((a, _, _), e)| g.current(a, *e)) {
            g.put(Key::Render(Box::new(key)), Value::Render(r), w);
        }
    }

    /// `agent`'s ledger went from `from` to `to` (`(last seq, length)`) by
    /// appending a §11 fact that no projected view holds and that changes no
    /// other event's place in one (not a redaction or unredaction, with no
    /// `after`, SPEC §11.1): every render of a thread holding the ledger at
    /// `from`, as any format but `protoledger` (whose export keeps the
    /// facts), is the render of the thread at `to` too, and is re-keyed so.
    pub(crate) fn carry_renders(&self, agent: &str, from: (u64, u64), to: (u64, u64)) {
        let mut g = self.lock();
        let keys: Vec<Key> = g
            .map
            .keys()
            .filter(|k| matches!(k, Key::Render(r) if r.format != "protoledger" && r.view.iter().any(|(a, s, l)| a == agent && (*s, *l) == from)))
            .cloned()
            .collect();
        for k in keys {
            let Key::Render(r) = &k else { continue };
            let mut moved = (**r).clone();
            for v in moved.view.iter_mut().filter(|(a, _, _)| a == agent) {
                (v.1, v.2) = to;
            }
            let moved = Key::Render(Box::new(moved));
            if g.map.contains_key(&moved) {
                continue;
            }
            let slot = g.map.remove(&k).expect("listed");
            g.order.insert(slot.tick, moved.clone());
            g.map.insert(moved, slot);
        }
    }

    /// Drop every entry of `agent`: its lines, and every render of a thread
    /// it is in.
    pub(crate) fn drop_agent(&self, agent: &str) {
        self.lock().drop_agent(agent);
    }

    /// A catch-up read `agent`'s ledger whole: `lines` is every line of it,
    /// `(offset, bytes)`. Keep its entries when each is byte-identical to
    /// the line at its offset; otherwise the ledger is not the one they were
    /// read from, and every entry of the agent goes.
    pub(crate) fn check_agent(&self, agent: &str, lines: &HashMap<u64, &[u8]>) {
        let mut g = self.lock();
        let stale = g.map.iter().any(|(k, slot)| match (k, &slot.value) {
            (Key::Line(a, offset), Value::Line(l)) if &**a == agent => lines.get(offset).is_none_or(|b| *b != &*l.bytes),
            _ => false,
        });
        if stale {
            g.drop_agent(agent);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn line(n: usize) -> Arc<Line> {
        let h: Header = serde_json::from_str(r#"{"protoledger":"0.1","agent":{"id":"a"}}"#).unwrap();
        Arc::new(Line { bytes: vec![b'x'; n].into_boxed_slice(), parsed: Parsed::Header(Box::new(h)) })
    }

    /// Let `a`'s entries be put under epoch 0.
    fn anchored(c: &ReadCache, a: &Arc<str>) {
        c.lock().anchors.insert(a.clone(), Anchor { epoch: 0, seen: Seen::Compressed { zst: None, tail: None }, probe: None });
    }

    fn f(ino: u64, size: u64, mtime_ns: i64) -> FileStat {
        FileStat { dev: 1, ino, born_ns: Some(ino as i128 * 1000), size, mtime_ns }
    }

    /// Every way a ledger's files can follow from what was seen, told apart
    /// by stats alone.
    #[test]
    fn appends_seals_and_recompressions_grow_and_everything_else_is_another_ledger() {
        let plain = |ino, size, t| Seen::Plain(f(ino, size, t));
        let packed = |z: Option<FileStat>, t: Option<FileStat>| Seen::Compressed { zst: z, tail: t };
        let grown = Change::Grown { check: true };
        let moved = Change::Grown { check: false };
        let cases = [
            ("plain unchanged", plain(1, 10, 1), plain(1, 10, 1), Change::Same),
            ("plain appended", plain(1, 10, 1), plain(1, 20, 2), grown),
            ("plain shrunk", plain(1, 10, 1), plain(1, 5, 2), Change::Other),
            ("plain rewritten at its size", plain(1, 10, 1), plain(1, 10, 2), Change::Other),
            ("plain made again", plain(1, 10, 1), plain(2, 20, 2), Change::Other),
            ("plain made again on a reused inode", plain(1, 10, 1), Seen::Plain(FileStat { born_ns: Some(9), ..f(1, 20, 2) }), Change::Other),
            ("tail appended", packed(Some(f(1, 50, 1)), Some(f(2, 10, 1))), packed(Some(f(1, 50, 1)), Some(f(2, 20, 2))), grown),
            ("sealed", packed(Some(f(1, 50, 1)), Some(f(2, 10, 1))), packed(Some(f(1, 60, 2)), Some(f(2, 0, 2))), moved),
            ("first seal", packed(None, Some(f(2, 10, 1))), packed(Some(f(1, 9, 2)), Some(f(2, 0, 2))), moved),
            ("recompressed", packed(Some(f(1, 50, 1)), Some(f(2, 10, 1))), packed(Some(f(3, 40, 2)), Some(f(2, 0, 2))), moved),
            ("block file shrunk in place", packed(Some(f(1, 50, 1)), Some(f(2, 10, 1))), packed(Some(f(1, 40, 2)), Some(f(2, 10, 1))), Change::Other),
            ("tail shrunk with no seal", packed(Some(f(1, 50, 1)), Some(f(2, 10, 1))), packed(Some(f(1, 50, 1)), Some(f(2, 5, 2))), Change::Other),
            ("made again compressed", packed(Some(f(1, 50, 1)), Some(f(2, 10, 1))), packed(Some(f(4, 50, 2)), Some(f(5, 10, 2))), Change::Other),
            ("tail gone", packed(Some(f(1, 50, 1)), Some(f(2, 10, 1))), packed(Some(f(1, 50, 1)), None), Change::Other),
            ("block file replaced, no tail to tie it", packed(Some(f(1, 50, 1)), None), packed(Some(f(3, 50, 2)), None), Change::Other),
            ("migrated to compressed", plain(1, 10, 1), packed(Some(f(3, 5, 2)), None), Change::Other),
            ("migrated to plain", packed(Some(f(1, 50, 1)), Some(f(2, 10, 1))), plain(6, 90, 2), Change::Other),
        ];
        for (what, a, n, want) in cases {
            assert_eq!(classify(&a, &n), want, "{what}");
        }
    }

    /// A line read under an epoch the agent no longer has (its ledger was
    /// found changed meanwhile) is not put.
    #[test]
    fn a_line_read_under_an_old_epoch_is_refused() {
        let c = ReadCache::new(1 << 20);
        let a: Arc<str> = "a".into();
        c.put_line(&a, 0, line(10), 0);
        assert!(c.line(&a, 0).is_none(), "put with no anchor");
        anchored(&c, &a);
        c.lock().anchors.get_mut(&*a).unwrap().epoch = 7;
        c.put_line(&a, 0, line(10), 6);
        assert!(c.line(&a, 0).is_none(), "put under an old epoch");
        c.put_line(&a, 0, line(10), 7);
        assert!(c.line(&a, 0).is_some());
    }

    /// The weight of a line of n bytes.
    fn w(n: usize) -> u64 {
        2 * n as u64 + ENTRY_OVERHEAD
    }

    /// Three entries fit, a fourth evicts the least recently used: the one
    /// not touched since it went in, not the first one in (which a FIFO
    /// would evict) because that one was read again.
    #[test]
    fn eviction_is_least_recently_used_not_first_in() {
        let c = ReadCache::new(3 * w(10));
        let a: Arc<str> = "a".into();
        anchored(&c, &a);
        for o in 0..3 {
            c.put_line(&a, o, line(10), 0);
        }
        assert!(c.line(&a, 0).is_some()); // 0 is now the most recent
        c.put_line(&a, 3, line(10), 0);
        assert!(c.line(&a, 1).is_none(), "1 was the least recently used");
        assert!(c.line(&a, 0).is_some(), "0 was evicted first in, first out");
        assert!(c.line(&a, 2).is_some() && c.line(&a, 3).is_some());
        let s = c.stats();
        assert_eq!((s.entries, s.bytes, s.events.evictions), (3, 3 * w(10), 1));
    }

    #[test]
    fn an_entry_over_the_budget_is_not_cached_and_evicts_nothing() {
        let c = ReadCache::new(w(10));
        let a: Arc<str> = "a".into();
        anchored(&c, &a);
        c.put_line(&a, 0, line(10), 0);
        c.put_line(&a, 1, line(11), 0);
        assert!(c.line(&a, 1).is_none());
        assert!(c.line(&a, 0).is_some());
        let s = c.stats();
        assert_eq!((s.entries, s.events.oversized, s.events.evictions), (1, 1, 0));
    }
}
