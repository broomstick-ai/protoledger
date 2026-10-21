//! A store: a directory of ledgers plus a derived, rebuildable index over them
//! (SPEC §9).
//!
//! ```text
//! <root>/ledgers/<agent>.jsonl        the truth: one ledger per agent, plain,
//! <root>/ledgers/<agent>.jsonl.zst    or compressed: its sealed blocks
//! <root>/ledgers/<agent>.jsonl.tail     and its hot tail (SPEC §10.2)
//! <root>/dicts/<id>.zdict             compression dictionaries (SPEC §10.7)
//! <root>/store.json                   configuration (SPEC §10.10)
//! <root>/index.db                     derived: deletable and rebuildable at any time
//! ```
//!
//! A ledger's form is told by which of its files exist, so plain and
//! compressed ledgers live side by side. Every read goes through a
//! [`LedgerReader`] and every write through the ledger's one writer, so
//! nothing else knows which form a ledger is in. `store.json` says only
//! whether new ledgers are compressed and when a writer seals; changing
//! `compression` migrates every ledger ([`Store::set_compression`]).
//!
//! [`Store::append`] writes through one writer per agent (a
//! [`crate::Appender`] for a plain ledger; for a compressed one, a writer of
//! its hot tail that seals; held for the life of the store), fsyncs, then
//! updates the index and publishes the event to subscribers. If the index
//! update is lost (a crash between the two), the next [`Store::open`]
//! re-tails every ledger whose files' sizes or modification times differ from
//! what the index recorded, from its last indexed offset. [`Store::reindex`] rebuilds the whole index from the
//! ledgers alone.
//!
//! Bindings (SPEC §9.2) are `app` events in the ledgers ([`bindings`]); the
//! index's binding rows are re-derived from them on every indexing path. A
//! binding's sync cursor is the one thing the ledgers do not say: it is kept
//! across [`Store::reindex`] and across a whole-file re-read by carrying it
//! over by binding id, and it is lost only with `index.db` itself, which
//! costs one rescan (the next sync re-reads the location and re-baselines).

pub mod bindings;
mod blocks;
mod config;
mod dicts;
pub mod gc;
mod index;
mod layout;
mod reader;
#[doc(hidden)]
pub mod testing;
mod writer;

pub use blocks::{FrameHeader, frame_header};
pub use config::{Compression, Config};
pub use dicts::DEFAULT_TRAIN_SIZE;
pub use gc::{Plan as GcPlan, Policy as GcPolicy};
pub use index::{AgentInfo, BindingRow, BlockRow, EventRow, SCHEMA_VERSION};
pub use reader::LedgerReader;

use crate::formats::{self, ImportOptions, Losses};
pub use crate::ledger::RedactError;
use crate::ledger::{Appended, file_safe};
use crate::model::{Agent, Event, Kind};
use anyhow::{Context, Result, anyhow, bail};
use layout::{Files, Fingerprint};
use rusqlite::Connection;
use std::collections::{HashMap, HashSet};
use std::fs;
use std::path::{Path, PathBuf};
use std::sync::Mutex;
use std::sync::mpsc::{Receiver, Sender, channel};
use writer::{Migrated, Opts, Writer};

/// One appended event, as published to subscribers (SPEC §9.4).
#[derive(Debug, Clone, PartialEq)]
pub struct Published {
    pub agent: String,
    pub seq: u64,
    /// Byte offset of the event's line in its ledger.
    pub offset: u64,
    pub event: Event,
}

/// Where a subscription starts.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Since {
    /// Every event of every agent from the start, then every append to any
    /// agent, including agents created later.
    All,
    /// Only the listed agents: each from just after the given seq (0 = from
    /// its start), then every append to them. Ledgers are append-only, so
    /// `(agent, last seq seen)` is the whole cursor.
    Agents(Vec<(String, u64)>),
}

/// What [`Store::import`] wrote.
#[derive(Debug, Clone, Default)]
pub struct ImportReport {
    pub format: String,
    pub variant: String,
    /// Agents that were new to the store.
    pub created: Vec<String>,
    /// Agents already in the store that gained events.
    pub extended: Vec<String>,
    pub events_appended: u64,
    /// Events whose id the agent's ledger already held.
    pub events_skipped: u64,
    pub losses: Losses,
}

/// What [`Store::reindex`] read.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Reindexed {
    pub agents: usize,
    pub events: u64,
}

/// What [`Store::set_compression`] did with each ledger.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Migration {
    /// Ledgers moved to the new form.
    pub migrated: Vec<String>,
    /// Ledgers left as they were, each with why: another writer holds it,
    /// or its files changed while it was locked. Running again migrates them.
    pub skipped: Vec<(String, String)>,
    /// Ledgers whose migration failed (its check found a difference, or the
    /// ledger is damaged), each with why. They were left as they were.
    pub failed: Vec<(String, String)>,
}

/// What [`Store::recompress`] did with each ledger.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Recompression {
    /// Ledgers rewritten.
    pub recompressed: Vec<String>,
    /// Ledgers left as they were, each with why: another writer holds it, or
    /// it is plain. Running again recompresses the ones that were held.
    pub skipped: Vec<(String, String)>,
    /// Ledgers whose recompression failed (its check found a difference, a
    /// dictionary its blocks need is missing, or the ledger is damaged), each
    /// with why. They were left as they were.
    pub failed: Vec<(String, String)>,
}

/// One dictionary of a store as `store dict ls` shows it (SPEC §10.7,
/// §10.8): from the index.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DictInfo {
    pub id: String,
    pub size: u64,
    /// The store's default dictionary for new blocks.
    pub default: bool,
    /// Each ledger with sealed blocks encoded by it, and how many.
    pub ledgers: Vec<(String, u64)>,
}

impl DictInfo {
    /// How many sealed blocks in the store it encoded.
    pub fn blocks(&self) -> u64 {
        self.ledgers.iter().map(|(_, n)| n).sum()
    }
}

/// Which dictionary a recompression uses.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum DictChoice {
    /// The store's default (`compression.dictionary`).
    Default,
    /// No dictionary (`--dictionary none`).
    None,
    /// This dictionary.
    Id(String),
}

struct Subscriber {
    tx: Sender<Published>,
    /// `None` = every agent.
    agents: Option<HashSet<String>>,
}

pub struct Store {
    root: PathBuf,
    db: Connection,
    writers: HashMap<String, Writer>,
    subs: Mutex<Vec<Subscriber>>,
    config: Config,
}

pub const LEDGERS: &str = "ledgers";
pub const INDEX: &str = "index.db";

impl Store {
    /// Open the store at `root`, creating `ledgers/` and `index.db` if needed,
    /// and bring the index up to date with the ledgers.
    pub fn open(root: impl AsRef<Path>) -> Result<Store> {
        let root = root.as_ref().to_path_buf();
        fs::create_dir_all(root.join(LEDGERS)).with_context(|| format!("creating {}", root.join(LEDGERS).display()))?;
        let db = index::open(&root.join(INDEX))?;
        let config = Config::read(&root)?;
        let store = Store { root, db, writers: HashMap::new(), subs: Mutex::new(Vec::new()), config };
        store.catch_up().context("bringing the index up to date")?;
        Ok(store)
    }

    pub fn root(&self) -> &Path {
        &self.root
    }

    /// The plain path of an agent the index knows: its ledger file when the
    /// ledger is plain; a compressed ledger's files are this path plus
    /// `.zst` and `.tail` (SPEC §10.2). Read a ledger through
    /// [`Store::reader`], not by this path.
    pub fn ledger_path(&self, agent: &str) -> Result<PathBuf> {
        let info = self.agent(agent)?.ok_or_else(|| anyhow!("no agent {agent:?} in the store"))?;
        Ok(self.root.join(info.path))
    }

    /// The reader of an agent's ledger: its decompressed stream (SPEC
    /// §10.1), whatever form the ledger is stored in. Every read the store
    /// makes of a ledger goes through one.
    pub fn reader(&self, agent: &str) -> Result<LedgerReader> {
        let info = self.agent(agent)?.ok_or_else(|| anyhow!("no agent {agent:?} in the store"))?;
        Ok(self.reader_of(&info.path))
    }

    /// The reader of the ledger at `rel` (relative to the store root),
    /// told where the index last saw its sealed blocks.
    fn reader_of(&self, rel: &str) -> LedgerReader {
        let r = LedgerReader::plain(self.root.join(rel));
        match self.db.query_row("SELECT id FROM agents WHERE path = ?1", [rel], |r| r.get::<_, String>(0)) {
            Ok(id) => r.with_hints(index::blocks(&self.db, &id).unwrap_or_default().iter().map(BlockRow::located).collect()),
            Err(_) => r,
        }
    }

    fn files(&self, rel: &str) -> Files {
        Files::of(&self.root.join(rel))
    }

    fn opts(&self) -> Opts {
        Opts {
            compress: self.config.compression.enabled,
            seal_bytes: self.config.compression.seal_bytes,
            dictionary: self.config.compression.dictionary.clone(),
        }
    }

    // ---- configuration (SPEC §10.10) ----

    /// The store's configuration, `store.json` (its defaults when absent).
    pub fn config(&self) -> &Config {
        &self.config
    }

    /// The sealed blocks of `agent`'s ledger as the index knows them
    /// (SPEC §10.8): none for a plain ledger.
    pub fn blocks(&self, agent: &str) -> Result<Vec<BlockRow>> {
        index::blocks(&self.db, agent)
    }

    /// Set the hot tail's size at which writers seal, and record it in
    /// `store.json`.
    pub fn set_seal_bytes(&mut self, n: u64) -> Result<()> {
        let mut c = self.config.clone();
        c.compression.seal_bytes = n;
        c.write(&self.root)?;
        self.config = c;
        for w in self.writers.values_mut() {
            w.set_seal_bytes(n);
        }
        Ok(())
    }

    /// `store config --compression on|off` (SPEC §10.10): record the
    /// setting, then migrate every ledger to that form, each on its own,
    /// under its writer lock, checked byte for byte before the old form is
    /// deleted. A ledger another writer holds is skipped (run again to
    /// migrate it); this store's own writers are released first.
    pub fn set_compression(&mut self, on: bool) -> Result<Migration> {
        let mut c = self.config.clone();
        c.compression.enabled = on;
        c.write(&self.root)?;
        self.config = c;
        self.writers.clear();
        self.refresh()?;
        let mut m = Migration::default();
        for a in self.agents()? {
            let files = self.files(&a.path);
            match writer::migrate(&files, on, &self.opts(), self.config.compression.recompress_bytes) {
                Ok(Migrated::Done) => m.migrated.push(a.id),
                Ok(Migrated::NotNeeded) => {}
                Ok(Migrated::Skipped(why)) => m.skipped.push((a.id, why)),
                Err(e) => m.failed.push((a.id, format!("{e:#}"))),
            }
        }
        self.refresh()?;
        Ok(m)
    }

    // ---- dictionaries (SPEC §10.7) ----

    /// `store config --dictionary <id>|none`: set (or, with `None`, clear)
    /// the default dictionary for new blocks. The id must name a dictionary
    /// in the store whose bytes match it. Existing blocks are not touched;
    /// this store's writers seal with it from their next seal.
    pub fn set_dictionary(&mut self, id: Option<&str>) -> Result<()> {
        if let Some(id) = id {
            if !dicts::is_id(id) {
                bail!("{id:?} is not a dictionary id (64 lowercase hex digits)");
            }
            dicts::Dicts::of_store(&self.root).load(id).map_err(|e| anyhow!("{e}: it is not a dictionary in this store"))?;
        }
        let mut c = self.config.clone();
        c.compression.dictionary = id.map(str::to_string);
        c.write(&self.root)?;
        self.config = c;
        self.writers.clear();
        Ok(())
    }

    /// `store dict add <file>`: add the dictionary `bytes` to the store at
    /// `root` (anything zstd accepts as one; refused when empty or rejected).
    /// Adding one already present changes nothing. Returns its id. Needs no
    /// index, so it also restores a dictionary a damaged store is missing.
    pub fn add_dictionary(root: &Path, bytes: &[u8]) -> Result<String> {
        dicts::Dicts::of_store(root).add(bytes)
    }

    /// `store dict train --size <bytes>`: train a dictionary of at most `size`
    /// bytes from the store's own ledgers (lines of their decompressed
    /// streams, the newest ledger and the newest line first) and add it.
    /// Fails, adding nothing, when the store holds too little to train on.
    pub fn train_dictionary(root: &Path, size: usize) -> Result<String> {
        let dir = root.join(LEDGERS);
        let mut ledgers: Vec<(i64, PathBuf)> = Vec::new();
        let names: HashSet<String> = match fs::read_dir(&dir) {
            Ok(rd) => rd.filter_map(|e| e.ok()).filter_map(|e| e.file_name().into_string().ok()).filter_map(|n| layout::ledger_of(&n)).collect(),
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => HashSet::new(),
            Err(e) => return Err(e).with_context(|| format!("reading {}", dir.display())),
        };
        for n in names {
            let files = Files::of(&dir.join(&n));
            let fp = Fingerprint::of(&files)?;
            ledgers.push((fp.mtime_ns.max(fp.tail_mtime_ns), files.plain));
        }
        ledgers.sort_by(|a, b| b.cmp(a));
        let mut streams = Vec::with_capacity(ledgers.len());
        for (_, p) in &ledgers {
            streams.push(LedgerReader::plain(p).stream()?);
        }
        let dict = dicts::train(&dicts::sample(&streams, size), size)?;
        dicts::Dicts::of_store(root).add(&dict)
    }

    /// `store dict rm <id>`: delete a dictionary from the store at `root`.
    /// Refused while it is the default or any block in the store references
    /// it; references are counted by walking the meta frames of every block
    /// file, never from the index, and with the store's dictionary lock held
    /// exclusively, so no seal, recompression or migration is between making
    /// sure the dictionary is on disk and making its block durable.
    pub fn remove_dictionary(root: &Path, id: &str) -> Result<()> {
        let ds = dicts::Dicts::of_store(root);
        let _held = ds.try_lock_exclusive()?;
        if !ds.contains(id)? {
            bail!("no dictionary {id} in the store");
        }
        if Config::read(root)?.compression.dictionary.as_deref() == Some(id) {
            bail!("dictionary {id} is the store's default for new blocks; set another (store config --dictionary) first");
        }
        let refs = dictionary_references(root, id)?;
        if !refs.is_empty() {
            let named: Vec<String> = refs.iter().map(|(l, n)| format!("{l} ({n} blocks)")).collect();
            bail!("dictionary {id} is used by {} ledgers: {}; recompress them with another first", refs.len(), named.join(", "));
        }
        ds.remove(id)
    }

    /// Every dictionary in the store, with its size, whether it is the
    /// default, and the ledgers and blocks that use it (SPEC §10.7, §10.8):
    /// from the index, brought up to date first.
    pub fn dictionaries(&self) -> Result<Vec<DictInfo>> {
        self.refresh()?;
        let usage = index::dict_usage(&self.db)?;
        Ok(index::dicts(&self.db)?
            .into_iter()
            .map(|(id, size)| DictInfo {
                default: self.config.compression.dictionary.as_deref() == Some(id.as_str()),
                ledgers: usage.iter().filter(|(d, _, _)| d.as_deref() == Some(id.as_str())).map(|(_, a, n)| (a.clone(), *n)).collect(),
                id,
                size,
            })
            .collect())
    }

    /// How many sealed blocks of each ledger each dictionary encoded, from
    /// the index: `(dictionary or None, agent, blocks)`.
    pub fn dictionary_usage(&self) -> Result<Vec<(Option<String>, String, u64)>> {
        index::dict_usage(&self.db)
    }

    /// `store recompress [--dictionary <id>|none] [agents…]` (SPEC §10.9):
    /// rewrite the named compressed ledgers (every compressed ledger when
    /// none is named) with `dict`, each under its writer lock, checked from
    /// disk byte for byte before it replaces the block file. A plain ledger,
    /// and one another writer holds, is skipped and reported; this store's
    /// own writers are released first. The index's block rows are updated
    /// after.
    pub fn recompress(&mut self, dict: DictChoice, agents: &[String]) -> Result<Recompression> {
        let id = match dict {
            DictChoice::Default => self.config.compression.dictionary.clone(),
            DictChoice::None => None,
            DictChoice::Id(id) => Some(id),
        };
        if let Some(id) = &id {
            dicts::Dicts::of_store(&self.root).load(id).map_err(|e| anyhow!("{e}: it is not a dictionary in this store"))?;
        }
        self.writers.clear();
        self.refresh()?;
        let all = self.agents()?;
        let chosen: Vec<AgentInfo> = if agents.is_empty() {
            all.into_iter().filter(|a| self.files(&a.path).form().is_ok_and(|f| f == layout::Form::Compressed)).collect()
        } else {
            let mut v = Vec::new();
            for a in agents {
                v.push(all.iter().find(|x| &x.id == a).cloned().ok_or_else(|| anyhow!("no agent {a:?} in the store"))?);
            }
            v
        };
        let mut r = Recompression::default();
        for a in chosen {
            match writer::recompress(&self.files(&a.path), id.as_deref(), self.config.compression.recompress_bytes) {
                Ok(Migrated::Done) => r.recompressed.push(a.id),
                Ok(Migrated::NotNeeded) => {}
                Ok(Migrated::Skipped(why)) => r.skipped.push((a.id, why)),
                Err(e) => r.failed.push((a.id, format!("{e:#}"))),
            }
        }
        // 5. The block rows, from the new files' meta frames.
        self.refresh()?;
        Ok(r)
    }

    // ---- writing ----

    /// Create a new agent's ledger (header only). Its structural parent, if
    /// any, must already be in the store (SPEC §2.1).
    pub fn create_agent(&mut self, agent: Agent) -> Result<()> {
        if index::agent(&self.db, &agent.id)?.is_some() {
            bail!("agent {:?} is already in the store", agent.id);
        }
        if let Some(p) = &agent.parent
            && index::agent(&self.db, &p.agent)?.is_none()
        {
            bail!("agent {:?}: parent {:?} is not in the store", agent.id, p.agent);
        }
        let rel = format!("{LEDGERS}/{}.jsonl", file_safe(&agent.id));
        let files = self.files(&rel);
        let ap = Writer::create(&files, agent.clone(), self.opts())?;
        let fp = Fingerprint::of(&files)?;
        let tx = self.db.unchecked_transaction()?;
        index::put_agent(&tx, &agent, &rel, fp, 0, ap.len(), None)?;
        tx.commit()?;
        self.writers.insert(agent.id, ap);
        Ok(())
    }

    /// Append one event to an agent's ledger. Durable (fsynced) when this
    /// returns; the index is updated and subscribers are sent the event in
    /// the same call.
    pub fn append(&mut self, agent: &str, e: Event) -> Result<Appended> {
        self.writer(agent)?;
        let (at, sealed) = self.writers.get_mut(agent).expect("writer opened").append(e)?;
        self.indexed(agent, at, sealed)
    }

    /// Redact `ids` in `agent`'s ledger (SPEC §11.1): append one
    /// `protoledger/redacted {events, reason}` fact through the agent's one
    /// writer, checked against the ledger as that writer holds it, under its
    /// lock. Refused, appending nothing ([`RedactError`]), when `ids` is
    /// empty, or names an event the ledger does not hold, or a
    /// `protoledger/redacted` or `protoledger/unredacted` fact. The ledger
    /// keeps every event; projections and exports leave the redacted out.
    pub fn redact(&mut self, agent: &str, ids: &[String], reason: &str) -> Result<Appended> {
        self.redaction(agent, false, ids, reason)
    }

    /// Withdraw redactions of `ids` in `agent`'s ledger (SPEC §11.1): append
    /// one `protoledger/unredacted {events}` fact, checked as
    /// [`Store::redact`] is. Refused when `ids` is empty or names an event
    /// the ledger does not hold.
    pub fn unredact(&mut self, agent: &str, ids: &[String]) -> Result<Appended> {
        self.redaction(agent, true, ids, "")
    }

    fn redaction(&mut self, agent: &str, unredact: bool, ids: &[String], reason: &str) -> Result<Appended> {
        self.writer(agent)?;
        let (at, sealed) = self.writers.get_mut(agent).expect("writer opened").redaction(unredact, ids, reason)?;
        self.indexed(agent, at, sealed)
    }

    /// Append the event `make` builds from `agent`'s ledger as the agent's
    /// one writer holds it, under that writer's lock, so the check `make`
    /// does and the append are one step (SPEC §11). When `make` refuses,
    /// nothing is appended and its error is returned. The checked §11 facts
    /// that are not redactions ([`crate::delivery`]) go through this.
    pub fn append_checked<E>(&mut self, agent: &str, make: impl FnOnce(&crate::Ledger) -> Result<Event, E>) -> Result<Appended>
    where
        E: std::error::Error + Send + Sync + 'static,
    {
        self.writer(agent)?;
        let e = make(self.writers[agent].ledger())?;
        self.append(agent, e)
    }

    /// `agent`'s ledger as the agent's one writer in this store holds it
    /// (opened on first use).
    pub fn held(&mut self, agent: &str) -> Result<&crate::Ledger> {
        self.writer(agent)?;
        Ok(self.writers[agent].ledger())
    }

    /// The `protoledger/projected` facts of `binding` in `agent`'s ledger
    /// (SPEC §11.2), as the agent's writer holds the ledger.
    pub fn projections(&mut self, agent: &str, binding: &str) -> Result<bindings::Projections> {
        self.writer(agent)?;
        Ok(bindings::projections(&self.writers[agent].ledger().events, binding))
    }

    /// Append `p` to `agent`'s ledger when, and only when, it differs from
    /// the binding's last `protoledger/projected` fact (or the binding has
    /// none): SPEC §11.2 step 6, one fact per change. Checked against the
    /// ledger as the agent's one writer holds it, under its lock. `None` when
    /// no fact was owed.
    pub fn append_projected(&mut self, agent: &str, p: &bindings::Projected) -> Result<Option<Appended>> {
        self.writer(agent)?;
        let held = bindings::projections(&self.writers[agent].ledger().events, &p.binding);
        if held.last().is_some_and(|last| last.data.same(p)) {
            return Ok(None);
        }
        self.append(agent, bindings::projected_event(p)).map(Some)
    }

    /// Index and publish what the writer of `agent` just appended at `at`.
    /// When the append sealed a block, the index's block rows catch up from
    /// the block file's meta frames.
    fn indexed(&mut self, agent: &str, at: Appended, sealed: bool) -> Result<Appended> {
        let ap = &self.writers[agent];
        let event = ap.ledger().events.last().expect("just appended").clone();
        let byte_len = ap.len();
        let rel = self.agent(agent)?.ok_or_else(|| anyhow!("agent {agent:?} left the index"))?.path;
        let files = self.files(&rel);
        let fp = Fingerprint::of(&files)?;
        let blocks = if sealed { Some(block_rows(agent, &files)?) } else { None };
        let title = title_of(&event);
        let tx = self.db.unchecked_transaction()?;
        index::put_event(&tx, &EventRow::of(agent, at.offset, at.len, &event))?;
        if let Some(f) = bindings::fact(&event) {
            index::apply_fact(&tx, agent, &f)?;
        }
        if let Some(rows) = &blocks {
            index::put_blocks(&tx, agent, rows)?;
        }
        index::advance(&tx, agent, at.seq, byte_len, fp, title.as_deref())?;
        tx.commit().with_context(|| format!("{agent} seq {} is durable but not indexed; the next open catches it up", at.seq))?;
        let mut subs = self.subs.lock().expect("subscriber lock");
        publish(&mut subs, vec![Published { agent: agent.to_string(), seq: at.seq, offset: at.offset, event }]);
        Ok(at)
    }

    /// The store's writer for `agent`, opened on first use. Opening takes the
    /// file's lock, so a second writer (another store, or a raw Appender) is
    /// refused. Anything appended before the lock was taken is indexed first.
    fn writer(&mut self, agent: &str) -> Result<()> {
        if self.writers.contains_key(agent) {
            return Ok(());
        }
        let info = self.agent(agent)?.ok_or_else(|| anyhow!("no agent {agent:?} in the store"))?;
        let ap = Writer::open(&self.files(&info.path), self.opts())?;
        let fresh = self.catch_up_file(&info.path, Some(info.clone()))?;
        let mut subs = self.subs.lock().expect("subscriber lock");
        publish(&mut subs, fresh);
        drop(subs);
        self.writers.insert(agent.to_string(), ap);
        Ok(())
    }

    /// Import a native thread (or a ledger set, format `protoledger`) into
    /// the store. `format` may be `auto`. Agents new to the store are created,
    /// parents first; agents already in it gain the events whose ids they do
    /// not yet hold.
    pub fn import(&mut self, format: &str, path: &Path) -> Result<ImportReport> {
        let f = if format == "auto" {
            formats::detect(path).ok_or_else(|| anyhow!("{}: no format recognised it", path.display()))?.0
        } else {
            formats::by_name(format).ok_or_else(|| anyhow!("unknown format {format:?}"))?
        };
        let imported = f.import(path, &ImportOptions::standard())?;
        let mut report = ImportReport { format: f.name().to_string(), variant: imported.variant.clone(), losses: imported.losses, ..Default::default() };
        // Parents before children.
        let set = imported.set;
        let mut order: Vec<&crate::Ledger> = Vec::new();
        let mut placed: HashSet<&str> = HashSet::new();
        while order.len() < set.len() {
            let before = order.len();
            for l in set.iter() {
                if placed.contains(l.id()) {
                    continue;
                }
                let ready = match &l.agent().parent {
                    Some(p) => placed.contains(p.agent.as_str()) || set.get(&p.agent).is_none(),
                    None => true,
                };
                if ready {
                    placed.insert(l.id());
                    order.push(l);
                }
            }
            if order.len() == before {
                bail!("the imported set's parent chain has a cycle");
            }
        }
        for l in order {
            let id = l.id().to_string();
            let existed = index::agent(&self.db, &id)?.is_some();
            if !existed {
                self.create_agent(l.agent().clone())?;
            }
            self.writer(&id)?;
            let mut appended = 0;
            for e in &l.events {
                if self.writers[&id].ledger().contains(&e.id) {
                    report.events_skipped += 1;
                    continue;
                }
                self.append(&id, e.clone())?;
                appended += 1;
            }
            report.events_appended += appended;
            if !existed {
                report.created.push(id);
            } else if appended > 0 {
                report.extended.push(id);
            }
        }
        Ok(report)
    }

    // ---- the index ----

    /// Re-read every ledger whose size or modification time differs from the
    /// index, from its last indexed offset; index new ledgers; forget ledgers
    /// that are gone. New events are published. Returns how many were found.
    pub fn refresh(&self) -> Result<usize> {
        let mut subs = self.subs.lock().expect("subscriber lock");
        let fresh = self.catch_up()?;
        let n = fresh.len();
        publish(&mut subs, fresh);
        Ok(n)
    }

    /// Throw the index away and rebuild it from the ledgers alone.
    /// Nothing is published: subscribers have already seen these events.
    /// Binding cursors are not in the ledgers; they are carried over by
    /// binding id to the bindings the rebuilt index still holds.
    pub fn reindex(&mut self) -> Result<Reindexed> {
        let cursors = index::cursors(&self.db)?;
        index::reset(&self.db)?;
        self.catch_up()?;
        index::set_cursors(&self.db, &cursors)?;
        let agents = index::agents(&self.db)?;
        Ok(Reindexed { agents: agents.len(), events: agents.iter().map(|a| a.last_seq).sum() })
    }

    fn catch_up(&self) -> Result<Vec<Published>> {
        let dir = self.root.join(LEDGERS);
        // A ledger is named by its plain file, whichever of its files exist
        // (SPEC §10.2); temporary files belong to no ledger.
        let mut files: Vec<String> = fs::read_dir(&dir)
            .with_context(|| format!("reading {}", dir.display()))?
            .filter_map(|e| e.ok())
            .filter_map(|e| e.file_name().into_string().ok())
            .filter_map(|n| layout::ledger_of(&n))
            .map(|n| format!("{LEDGERS}/{n}"))
            .collect();
        files.sort();
        files.dedup();
        let known: HashMap<String, AgentInfo> = index::agents(&self.db)?.into_iter().map(|a| (a.path.clone(), a)).collect();
        let mut fresh = Vec::new();
        for rel in &files {
            let prev = known.get(rel).cloned();
            if let Some(p) = &prev
                && p.fingerprint() == Fingerprint::of(&self.files(rel))?
            {
                continue;
            }
            fresh.extend(self.catch_up_file(rel, prev)?);
        }
        let present: HashSet<&String> = files.iter().collect();
        let tx = self.db.unchecked_transaction()?;
        index::put_dicts(&tx, &dicts::Dicts::of_store(&self.root).list()?)?;
        for (rel, info) in &known {
            if !present.contains(rel) {
                index::forget_agent(&tx, &info.id)?;
            }
        }
        tx.commit()?;
        Ok(fresh)
    }

    /// Index what one ledger holds past `prev.byte_len` (or all of it), and
    /// its block rows from its block file's meta frames.
    fn catch_up_file(&self, rel: &str, prev: Option<AgentInfo>) -> Result<Vec<Published>> {
        let files = self.files(rel);
        let reader = self.reader_of(rel);
        // Stat before reading: a line appended after the stat leaves the
        // recorded fingerprint behind the files, so the next catch-up sees it.
        let fp = Fingerprint::of(&files)?;
        if let Some(p) = &prev
            && p.byte_len > 0
            && reader.len().with_context(|| reader.to_string())? >= p.byte_len
        {
            let (evs, end) = reader.tail(p.byte_len).with_context(|| reader.to_string())?;
            if evs.first().is_none_or(|(_, e)| e.seq == p.last_seq + 1) {
                let blocks = block_rows(&p.id, &files)?;
                return self.index_events(&p.id, rel, evs, end, fp, &blocks, None);
            }
        }
        // New to the index, or not a clean continuation: read it whole.
        let Some(header) = reader.header()? else {
            return Ok(Vec::new()); // no complete header line yet
        };
        let (evs, end) = reader.tail(0).with_context(|| reader.to_string())?;
        let blocks = block_rows(&header.agent.id, &files)?;
        self.index_events(&header.agent.id, rel, evs, end, fp, &blocks, Some((&header.agent, prev)))
    }

    #[allow(clippy::too_many_arguments)]
    fn index_events(
        &self,
        agent: &str,
        rel: &str,
        evs: Vec<(u64, Event)>,
        end: u64,
        fp: Fingerprint,
        blocks: &[BlockRow],
        fresh_header: Option<(&Agent, Option<AgentInfo>)>,
    ) -> Result<Vec<Published>> {
        let tx = self.db.unchecked_transaction()?;
        let mut last_seq = 0;
        let mut cursors = Vec::new();
        if let Some((header, prev)) = &fresh_header {
            if let Some(p) = prev {
                cursors.extend(index::cursors_of(&tx, &p.id)?);
                index::forget_agent(&tx, &p.id)?;
            }
            cursors.extend(index::cursors_of(&tx, &header.id)?);
            index::forget_agent(&tx, &header.id)?;
            index::put_agent(&tx, header, rel, fp, 0, 0, None)?;
        } else {
            last_seq = index::agent(&tx, agent)?.map(|a| a.last_seq).unwrap_or(0);
        }
        let mut title = None;
        let mut out = Vec::with_capacity(evs.len());
        let mut starts: Vec<u64> = evs.iter().map(|(o, _)| *o).collect();
        starts.push(end);
        for (i, (offset, e)) in evs.into_iter().enumerate() {
            if e.seq != last_seq + 1 {
                bail!("{rel}: seq {} at offset {offset} where {} was expected", e.seq, last_seq + 1);
            }
            last_seq = e.seq;
            let row = EventRow::of(agent, offset, starts[i + 1] - offset, &e);
            index::put_event(&tx, &row)?;
            if let Some(f) = bindings::fact(&e) {
                index::apply_fact(&tx, agent, &f)?;
            }
            if let Some(t) = title_of(&e) {
                title = Some(t);
            }
            out.push(Published { agent: agent.to_string(), seq: e.seq, offset, event: e });
        }
        index::advance(&tx, agent, last_seq, end, fp, title.as_deref())?;
        index::put_blocks(&tx, agent, blocks)?;
        index::set_cursors(&tx, &cursors)?;
        tx.commit()?;
        Ok(out)
    }

    pub fn agents(&self) -> Result<Vec<AgentInfo>> {
        index::agents(&self.db)
    }

    pub fn agent(&self, id: &str) -> Result<Option<AgentInfo>> {
        index::agent(&self.db, id)
    }

    /// Direct structural children of `id` (agents whose `parent` names it).
    pub fn children(&self, id: &str) -> Result<Vec<AgentInfo>> {
        index::children(&self.db, id)
    }

    /// Agents that name `id` in a link of relation `rel` (e.g. every agent
    /// `spawned-by` it).
    pub fn linked_from(&self, id: &str, rel: &str) -> Result<Vec<AgentInfo>> {
        index::linked_from(&self.db, id, rel)
    }

    /// Up to `limit` index rows of `agent`'s events, from `from_seq` on.
    pub fn events(&self, agent: &str, from_seq: u64, limit: usize) -> Result<Vec<EventRow>> {
        index::events(&self.db, agent, from_seq, limit)
    }

    /// The event at `seq`, read from its ledger at the indexed offset.
    pub fn event_at(&self, agent: &str, seq: u64) -> Result<Option<Event>> {
        let Some(row) = index::event(&self.db, agent, seq)? else {
            return Ok(None);
        };
        let reader = self.reader(agent)?;
        let e = reader.read_seq_at(row.offset, seq)?;
        if e.seq != seq || e.id != row.id {
            bail!("{reader}: the index is stale at seq {seq} (found {} / {}); run reindex", e.seq, e.id);
        }
        Ok(Some(e))
    }

    /// Every binding the index holds (SPEC §9.2).
    pub fn bindings(&self) -> Result<Vec<BindingRow>> {
        index::bindings(&self.db)
    }

    pub fn binding(&self, binding: &str) -> Result<Option<BindingRow>> {
        index::binding(&self.db, binding)
    }

    /// A binding's sync cursor: opaque to the store, the one binding column
    /// not read from a ledger.
    pub fn cursor(&self, binding: &str) -> Result<Option<String>> {
        index::cursor(&self.db, binding)
    }

    pub fn set_cursor(&self, binding: &str, cursor: &str) -> Result<()> {
        if index::binding(&self.db, binding)?.is_none() {
            bail!("no binding {binding:?} in the store");
        }
        index::set_cursors(&self.db, &[(binding.to_string(), cursor.to_string())])
    }

    // ---- events out ----

    /// Subscribe to appends (SPEC §9.4). Events already in the ledgers past
    /// the cursor are sent first, in seq order per agent; then every new
    /// append, in the order it was made.
    pub fn subscribe(&self, since: Since) -> Result<Receiver<Published>> {
        let mut subs = self.subs.lock().expect("subscriber lock");
        let fresh = self.catch_up()?;
        publish(&mut subs, fresh);
        let (tx, rx) = channel();
        let (targets, filter) = match since {
            Since::All => (index::agents(&self.db)?.into_iter().map(|a| (a.id, 0)).collect::<Vec<_>>(), None),
            Since::Agents(v) => {
                let filter = v.iter().map(|(a, _)| a.clone()).collect();
                (v, Some(filter))
            }
        };
        for (agent, seq) in targets {
            let Some(info) = index::agent(&self.db, &agent)? else { continue };
            let Some(first) = index::events(&self.db, &agent, seq + 1, 1)?.pop() else { continue };
            let (evs, _) = self.reader_of(&info.path).tail(first.offset)?;
            for (offset, event) in evs.into_iter().filter(|(o, _)| *o < info.byte_len) {
                let _ = tx.send(Published { agent: agent.clone(), seq: event.seq, offset, event });
            }
        }
        subs.push(Subscriber { tx, agents: filter });
        Ok(rx)
    }
}

fn publish(subs: &mut Vec<Subscriber>, items: Vec<Published>) {
    for p in items {
        subs.retain(|s| {
            if s.agents.as_ref().is_some_and(|a| !a.contains(&p.agent)) {
                return true;
            }
            s.tx.send(p.clone()).is_ok()
        });
    }
}

fn title_of(e: &Event) -> Option<String> {
    (e.kind == Kind::Title && e.unknown_kind.is_none()).then(|| e.str("text").map(str::to_string)).flatten()
}

/// The ledgers of the store at `root` whose block files reference dictionary
/// `id`, with how many blocks each: from the meta frames of every block file
/// (SPEC §10.7). Temporary files are not block files.
fn dictionary_references(root: &Path, id: &str) -> Result<Vec<(String, u64)>> {
    let dir = root.join(LEDGERS);
    let mut out = Vec::new();
    let rd = match fs::read_dir(&dir) {
        Ok(rd) => rd,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(out),
        Err(e) => return Err(e).with_context(|| format!("reading {}", dir.display())),
    };
    let mut names: Vec<String> = rd.filter_map(|e| e.ok()).filter_map(|e| e.file_name().into_string().ok()).filter(|n| n.ends_with(".jsonl.zst")).collect();
    names.sort();
    for n in names {
        let p = dir.join(&n);
        let Some(f) = layout::open_if_present(&p)? else { continue };
        let len = f.metadata()?.len();
        let walked = blocks::walk(&f, len, 0, 0, 0).with_context(|| format!("{}: counting the blocks that use dictionary {id}", p.display()))?;
        let k = walked.blocks.iter().filter(|b| b.meta.dict.as_deref() == Some(id)).count() as u64;
        if k > 0 {
            out.push((n.trim_end_matches(".zst").to_string(), k));
        }
    }
    Ok(out)
}

/// The block rows of a ledger's block file, from its meta frames alone
/// (SPEC §10.8): none for a plain ledger. A torn final block is not a row.
fn block_rows(agent: &str, files: &Files) -> Result<Vec<BlockRow>> {
    if files.form()? != layout::Form::Compressed {
        return Ok(Vec::new());
    }
    let Some(f) = layout::open_if_present(&files.zst)? else { return Ok(Vec::new()) };
    let len = f.metadata()?.len();
    let walked = blocks::walk(&f, len, 0, 0, 0).with_context(|| files.plain.display().to_string())?;
    let mut located = walked.blocks;
    // A torn final block is not a row; one whose dictionary is missing is
    // damage, not torn (SPEC §10.6), and keeps its row: the rows are its meta
    // frames.
    if walked.rest.is_none()
        && let Some(last) = located.last()
        && matches!(blocks::read_block(&f, last, &dicts::Dicts::of_ledgers_dir(files.dir())), Err(blocks::BlockError::NotWhole(_)))
    {
        located.pop();
    }
    Ok(located.iter().map(|b| BlockRow::of(agent, b)).collect())
}
