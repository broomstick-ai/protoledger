//! Bindings and sync (SPEC §9.2, §9.3): a store's ledgers tied to native
//! locations, and the engine that follows them.
//!
//! - [`project`] = export + bind: an agent (and its structural descendants)
//!   is exported into a directory in a harness's store, and the written file
//!   that holds the agent becomes the binding's location.
//! - [`attach`] = import + bind: a native session is imported and followed.
//!   Only ever explicit: nothing here looks for sessions.
//! - [`sync`] re-imports a bound location and appends, in order, the events
//!   whose ids the ledger does not hold. Importers derive ids from native ids
//!   and restore carried events (SPEC §2.2, §6), so a re-read never duplicates
//!   anything and protoledger's own exported events come back as themselves.
//! - [`Follower`] runs [`sync`] when a location changes: a filesystem watcher
//!   with a debounce, and a periodic poll as the fallback.
//! - **Following a growing file** ([`sync_with`], which the [`Follower`]
//!   uses): when the location is a file its format can follow
//!   ([`Format::follow`]), the format's state is kept in [`Follows`] between
//!   syncs, and while the file only grows, only the bytes appended since the
//!   last sync are read and parsed. What that yields is exactly what a whole
//!   re-import would add (tests/follow.rs); a file that shrank, was replaced
//!   (another file at the path) or rewritten (its first or last 4 KiB of what
//!   was read differ), whose other files changed (a session's sidecar
//!   directory), or whose new lines change what earlier ones made, is
//!   re-imported whole, as [`sync`] always does. A rewrite in place that
//!   keeps both ends and does not shrink the file is not seen until the next
//!   whole re-import (a new follower, or a fallback).
//!
//! - **Records rewritten in place** (SPEC §4.1): a format whose harness
//!   rewrites its records under the same id (opencode) goes through its
//!   hook in the private `inplace` module instead of the id comparison: each settled fact is
//!   appended once, transient states never, and a record that changed carries
//!   its newer version on the next event (or on a `native` event of its own).
//!
//! **Rewinds** (SPEC §2.3) need nothing special: a harness rewinds by
//! appending events whose `after` names an earlier event, and those are new
//! ids like any others. **Divergence** (SPEC §9.3): the binding's cursor holds,
//! per agent, the ids the location yielded at the last sync. If any of them is
//! gone, or they no longer come in the same order, the location's earlier
//! content changed in a way that is not an append; sync appends one
//! `protoledger/diverged` event, the binding stops being followed, and nothing
//! else is appended.
//!
//! **Re-projection** (SPEC §11.2, [`reproject`]): a projection binding whose
//! location is a single file its harness only appends to (Claude Code's and
//! Codex's transcripts) is written again from the projected view, between
//! harness runs. Each write is named by a `protoledger/projected` fact in
//! the bound agent's ledger (`project` appends the first one, with its
//! `bound` fact, once the files are durable), and sync measures such a
//! location against **the latest of those facts that the file matches**:
//! the file at least `bytes` long, its first `bytes` bytes hashing to
//! `sha256`. Those bytes are protoledger's own and are never divergence;
//! what follows them is read as an append. Beyond them §9.3 applies per
//! fact: the cursor keeps, for every fact a sync matched, the ids that sync
//! read past the fact's bytes, and one of them gone (or out of order) at a
//! later sync matching the same fact is divergence, unless the ledger holds
//! that event redacted: re-projection leaves a redacted event out, so a file
//! without one lost nothing (this is what keeps the case where the new
//! render is a byte-prefix of the old file from being diverged). A file no fact matches is
//! diverged; a binding made without a fact (legacy, [`bindings::Projections::legacy`])
//! is measured by the plain §9.3 test until a sync has matched one of its
//! facts. A location's other files (Claude Code's sidecar directory) are
//! covered by no fact's bytes: their events are read past the bytes, so the
//! same per-fact test applies to them, redacted events again excepted. That
//! is what makes a crash between a sidecar's rename and the location file's
//! rename safe. Because the facts are in the ledger, the prefix is
//! recognised when the cursor is lost; what is lost with it is only the
//! record of what earlier syncs read past the bytes (§9.1). Formats whose
//! location is not such a file get no `protoledger/projected` fact and are
//! measured by §9.3 alone.
//!
//! **Temporary files** of a re-projection are named
//! `.<file name>.protoledger-tmp` beside the file they replace; sync never
//! reads them ([`files_of`] skips them, and the Claude Code importer skips
//! names that start with a dot), and the next re-projection of the binding
//! deletes any a crash left behind.
//!
//! **The cursor** is the one binding column not read from a ledger
//! ([`crate::store`]). Lost with `index.db`, it costs one rescan: the next
//! sync re-reads the location, appends what the ledger lacks (nothing, if
//! nothing changed) and re-baselines; only a divergence that happened while
//! the cursor was gone goes unnoticed.

mod follow;
mod inplace;
mod reproject;

pub use follow::{FollowOptions, Follower, Trigger};
pub use reproject::{ATTEMPTS, Reprojected, Stage, reproject, reproject_with, reprojectable};

use crate::formats::{self, ExportOptions, Format, ImportOptions, Imported};
use crate::ledger::{Ledger, LedgerSet};
use crate::store::bindings::{self, Bound, Mode};
use crate::store::{BindingRow, Published, Store};
use anyhow::{Context, Result, anyhow, bail};
use serde::{Deserialize, Serialize};
use std::collections::{BTreeMap, HashMap, HashSet};
use std::path::{Path, PathBuf};

/// What sync knew about a location after its last read: stored as the
/// binding's cursor (JSON).
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct Cursor {
    /// Fingerprint of the location's files (path, size, mtime).
    pub stamp: String,
    /// Per agent, the event ids the location yielded, in order.
    pub seen: BTreeMap<String, Vec<String>>,
    /// The `protoledger/projected` fact (its event id) the last sync matched
    /// (SPEC §11.2); `None` for a location measured by §9.3 alone.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub matched: Option<String>,
    /// Some sync of this binding has matched one of its
    /// `protoledger/projected` facts: from then on a file none of them
    /// matches is diverged, legacy binding or not (SPEC §11.2).
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    pub ever_matched: bool,
    /// Per `protoledger/projected` fact (event id), per agent: the ids the
    /// latest sync matching that fact read past its bytes, in order.
    #[serde(default, skip_serializing_if = "BTreeMap::is_empty")]
    pub past: BTreeMap<String, BTreeMap<String, Vec<String>>>,
}

impl Cursor {
    fn of(set: &LedgerSet, stamp: String) -> Cursor {
        Cursor { stamp, seen: set.iter().map(|l| (l.id().to_string(), l.events.iter().map(|e| e.id.clone()).collect())).collect(), ..Default::default() }
    }
    /// A binding's cursor as the store keeps it (JSON); `None` when it is not one.
    pub fn parse(s: &str) -> Option<Cursor> {
        serde_json::from_str(s).ok()
    }
    /// The cursor as the store keeps it.
    pub fn text(&self) -> String {
        serde_json::to_string(self).expect("a cursor serialises")
    }
}

/// What one sync of one binding did.
#[derive(Debug, Clone, Default)]
pub struct Synced {
    pub binding: String,
    /// Agents the location gained that the store did not hold.
    pub created: Vec<String>,
    /// Every appended event, in the order it was appended.
    pub appended: Vec<Published>,
    /// Set when this sync found the location diverged (and appended the
    /// `protoledger/diverged` event, which is not in `appended`).
    pub diverged: Option<String>,
    /// Set when the location was not read this time (a torn final line, a
    /// missing file, a binding no longer followed): try again later.
    pub deferred: Option<String>,
    /// This sync read only the bytes appended to the location since the last
    /// one (a format's [`Follow`](formats::Follow)); false when it re-read
    /// the location whole, or did not read it.
    pub followed: bool,
}

impl Synced {
    /// Did this sync change the store?
    pub fn changed(&self) -> bool {
        !self.appended.is_empty() || !self.created.is_empty() || self.diverged.is_some()
    }
}

fn format_named(name: &str, input: &Path) -> Result<&'static dyn Format> {
    if name == "auto" {
        return Ok(formats::detect(input).ok_or_else(|| anyhow!("{}: no format recognised it", input.display()))?.0);
    }
    formats::by_name(name).ok_or_else(|| anyhow!("unknown format {name:?}"))
}

fn absolute(p: &Path) -> Result<PathBuf> {
    std::path::absolute(p).with_context(|| format!("resolving {}", p.display()))
}

/// Export `agent` and its structural descendants as `format` into `dir`, and
/// bind the agent to the written file that holds it. Files already in `dir`
/// are never overwritten (a shared append-only index only gains lines). The
/// export must come back as itself: re-importing the location has to yield
/// only agents and event ids the store already holds, or nothing is bound
/// (and the files just written are removed), since sync would otherwise
/// append those strangers as new events.
///
/// When the location is one [`reproject`] can write again (a single file the
/// harness only appends to, [`reprojectable`]), the `protoledger/bound` fact
/// is followed by a `protoledger/projected` fact for the file written (SPEC
/// §11.2), so sync has a fact to measure the file against before its first
/// re-projection. Both are appended only once the files are durably written
/// (each is fsynced, and so is its directory): a crash before them leaves an
/// unbound file and no binding, never a binding whose fact names bytes that
/// are not on disk. A crash between the two leaves a binding with no fact
/// right after its `bound` fact, which sync measures as a legacy binding.
pub fn project(store: &mut Store, agent: &str, format: &str, dir: &Path) -> Result<BindingRow> {
    project_with(store, agent, format, dir, &mut |_| Ok(()))
}

/// [`project`], calling `hook` at [`Stage::Written`] and [`Stage::Bound`]; an
/// error from it stops the projection there, as a crash would (for tests).
pub fn project_with(store: &mut Store, agent: &str, format: &str, dir: &Path, hook: &mut dyn FnMut(Stage<'_>) -> Result<()>) -> Result<BindingRow> {
    let f = formats::by_name(format).ok_or_else(|| anyhow!("unknown format {format:?}"))?;
    let set = projection_set(store, agent)?;
    let ex = f.export(&set, &ExportOptions::standard())?;
    let mut fresh: Vec<PathBuf> = Vec::new();
    for rel in ex.output.files.keys() {
        let p = dir.join(rel);
        if !ex.output.appends.contains(rel) {
            if p.exists() {
                bail!("{} exists; a projection never overwrites a file", p.display());
            }
            fresh.push(p);
        }
    }
    ex.output.write_to(dir, false)?;
    let undo = |e: anyhow::Error| {
        for p in &fresh {
            let _ = std::fs::remove_file(p);
        }
        e
    };
    let (location, imported) = find_location(f, dir, &ex.output, &ex.output.appends, agent).map_err(undo)?;
    // It must come back as itself.
    let mut strangers = strangers(store, &imported.set)?;
    if !strangers.is_empty() {
        let n = strangers.len();
        strangers.truncate(5);
        return Err(undo(anyhow!(
            "{format} does not bring this projection back as itself ({n} events or agents the store does not hold, e.g. {}); not bound",
            strangers.join(", ")
        )));
    }
    let native_id = imported.set.get(agent).and_then(|l| l.agent().source.as_ref()).and_then(|s| s.native_id.clone());
    let b = Bound {
        binding: uuid::Uuid::new_v4().to_string(),
        format: f.name().to_string(),
        variant: Some(ex.variant.clone()),
        location: absolute(&location)?.to_string_lossy().into_owned(),
        native_id,
        mode: Mode::Projection,
    };
    hook(Stage::Written)?;
    if reprojectable(f.name(), &location).is_err() {
        return bind(store, agent, &b, &imported.set, &location);
    }
    // What was written, read back from the disk it is durable on.
    let bytes = std::fs::read(&location)?;
    let through = set.get(agent).and_then(|l| l.projected_view().last().map(|e| e.id.clone()));
    store.append(agent, bindings::bound_event(&b))?;
    hook(Stage::Bound)?;
    store.append_projected(agent, &bindings::Projected::of(&b.binding, &bytes, through))?;
    let facts = store.projections(agent, &b.binding)?;
    let cursor = reproject::baseline(f, &location, &bytes, &facts, &imported.set, stamp(&location)?, None);
    store.set_cursor(&b.binding, &cursor.text())?;
    store.binding(&b.binding)?.ok_or_else(|| anyhow!("binding {} was appended but not indexed", b.binding))
}

/// `agent` and its structural descendants, as the store holds them.
fn projection_set(store: &Store, agent: &str) -> Result<LedgerSet> {
    let mut ids = vec![agent.to_string()];
    let mut i = 0;
    while i < ids.len() {
        let kids = store.children(&ids[i])?;
        ids.extend(kids.into_iter().map(|a| a.id));
        i += 1;
    }
    let mut set = LedgerSet::new();
    for id in &ids {
        set.insert(store.reader(id)?.ledger()?);
    }
    Ok(set)
}

/// The agents and events of `set` the store does not hold (`agent <id>`, `<agent>: <event id>`).
fn strangers(store: &Store, set: &LedgerSet) -> Result<Vec<String>> {
    let mut out = Vec::new();
    for l in set.iter() {
        let held: HashSet<String> = match store.agent(l.id())? {
            Some(_) => store.events(l.id(), 0, usize::MAX)?.into_iter().map(|r| r.id).collect(),
            None => {
                out.push(format!("agent {}", l.id()));
                continue;
            }
        };
        out.extend(l.events.iter().filter(|e| !held.contains(&e.id)).map(|e| format!("{}: {}", l.id(), e.id)));
    }
    Ok(out)
}

/// The written file whose import holds `agent`, nearest the root of `dir`.
fn find_location(f: &dyn Format, dir: &Path, out: &formats::Output, appends: &std::collections::BTreeSet<String>, agent: &str) -> Result<(PathBuf, Imported)> {
    let mut rels: Vec<&String> = out.files.keys().filter(|r| !appends.contains(*r)).collect();
    rels.sort_by_key(|r| (Path::new(r).components().count(), r.to_string()));
    for rel in rels {
        let p = dir.join(rel);
        if f.detect(&p).is_none() {
            continue;
        }
        if let Ok(imp) = f.import(&p, &ImportOptions::standard())
            && imp.set.get(agent).is_some()
        {
            return Ok((p, imp));
        }
    }
    bail!("no file {} wrote into {} brings agent {agent:?} back", f.name(), dir.display())
}

/// Import a native session at `location` into the store and follow it. The
/// binding is recorded in the ledger of the imported set's first root agent.
pub fn attach(store: &mut Store, format: &str, location: &Path) -> Result<(BindingRow, Synced)> {
    let f = format_named(format, location)?;
    let abs = absolute(location)?;
    let loc = abs.to_string_lossy().into_owned();
    if let Some(b) = store.bindings()?.into_iter().find(|b| b.active && b.location == loc && b.format == f.name()) {
        bail!("{loc} is already bound ({})", b.binding);
    }
    if let Some(t) = torn(&abs)? {
        bail!("{}: the final line is incomplete (a write in progress?); try again", t.display());
    }
    let imported = f.import(&abs, &ImportOptions::standard())?;
    // The binding goes on a root conversation, not on a container ledger the
    // others are `member-of` (a Slack workspace, an opencode store); a
    // container no ledger names (an opencode store with no session) is one.
    let set = &imported.set;
    let container = |id: &str| set.iter().any(|l| l.agent().links.iter().any(|k| k.rel == "member-of" && k.agent.as_deref() == Some(id)));
    let root = set.roots().find(|r| !container(r.id())).ok_or_else(|| anyhow!("{loc}: the import holds no agent"))?.id().to_string();
    let mut synced = Synced::default();
    apply(store, f.name(), &imported.set, &mut synced)?;
    let native_id = imported.set.get(&root).and_then(|l| l.agent().source.as_ref()).and_then(|s| s.native_id.clone());
    let b = Bound {
        binding: uuid::Uuid::new_v4().to_string(),
        format: f.name().to_string(),
        variant: Some(imported.variant.clone()),
        location: loc,
        native_id,
        mode: Mode::Attachment,
    };
    synced.binding = b.binding.clone();
    let row = bind(store, &root, &b, &imported.set, &abs)?;
    Ok((row, synced))
}

fn bind(store: &mut Store, agent: &str, b: &Bound, seen: &LedgerSet, location: &Path) -> Result<BindingRow> {
    store.append(agent, bindings::bound_event(b))?;
    store.set_cursor(&b.binding, &Cursor::of(seen, stamp(location)?).text())?;
    store.binding(&b.binding)?.ok_or_else(|| anyhow!("binding {} was appended but not indexed", b.binding))
}

/// Stop following a binding: appends `protoledger/unbound` to its agent's ledger.
pub fn unbind(store: &mut Store, binding: &str) -> Result<()> {
    let row = store.binding(binding)?.ok_or_else(|| anyhow!("no binding {binding:?}"))?;
    if !row.active {
        bail!("binding {binding} is not active ({})", row.ended.as_deref().unwrap_or("?"));
    }
    store.append(&row.agent, bindings::unbound_event(binding))?;
    Ok(())
}

/// Read a bound location again, whole, and append what the ledger lacks (see
/// the module docs for divergence).
pub fn sync(store: &mut Store, binding: &str) -> Result<Synced> {
    sync_with(store, binding, &mut Follows::default())
}

/// The follow states of bound locations, kept between syncs by
/// [`sync_with`] (see the module docs).
#[derive(Default)]
pub struct Follows {
    by_binding: HashMap<String, Following>,
}

impl Follows {
    /// Drop the state of every binding not in `keep`.
    pub fn retain(&mut self, keep: impl Fn(&str) -> bool) {
        self.by_binding.retain(|b, _| keep(b));
    }
    /// The bindings whose locations are followed from where the last sync stopped.
    pub fn bindings(&self) -> Vec<String> {
        let mut out: Vec<String> = self.by_binding.keys().cloned().collect();
        out.sort();
        out
    }
}

/// How much of each end of what was read is compared to tell an append from a rewrite.
const TAIL: u64 = 4096;

/// One bound file, read up to `len`.
struct Following {
    location: PathBuf,
    state: Box<dyn formats::Follow>,
    len: u64,
    /// The file's first bytes, and its last bytes up to `len` (at most [`TAIL`] each).
    head: Vec<u8>,
    tail: Vec<u8>,
    /// The file's identity (device, inode) where the platform has one.
    id: Option<(u64, u64)>,
    /// The stamp of the location's other files.
    others: String,
    /// The binding's cursor as of the last sync.
    cursor: Cursor,
}

fn file_id(m: &std::fs::Metadata) -> Option<(u64, u64)> {
    #[cfg(unix)]
    {
        use std::os::unix::fs::MetadataExt;
        Some((m.dev(), m.ino()))
    }
    #[cfg(not(unix))]
    {
        let _ = m;
        None
    }
}

fn read_range(path: &Path, from: u64, to: u64) -> Result<Vec<u8>> {
    use std::io::{Read, Seek, SeekFrom};
    let mut f = std::fs::File::open(path)?;
    f.seek(SeekFrom::Start(from))?;
    let mut out = Vec::with_capacity((to - from) as usize);
    f.take(to - from).read_to_end(&mut out)?;
    if (out.len() as u64) < to - from {
        bail!("{} shrank while it was read", path.display());
    }
    Ok(out)
}

/// The stamp of a location's files other than the location itself.
fn others_stamp(loc: &Path) -> Result<String> {
    use sha2::{Digest, Sha256};
    let mut h = Sha256::new();
    for p in files_of(loc)?.into_iter().filter(|p| p != loc) {
        let Ok(m) = std::fs::metadata(&p) else { continue };
        let mtime = m.modified().ok().and_then(|t| t.duration_since(std::time::UNIX_EPOCH).ok()).map(|d| d.as_nanos()).unwrap_or(0);
        h.update(format!("{}\0{}\0{}\n", p.display(), m.len(), mtime).as_bytes());
    }
    Ok(hex::encode(h.finalize()))
}

impl Following {
    /// What was appended since the last read, through the format: `None`
    /// when the file is not simply longer (or the format declines).
    fn read_on(&mut self, loc: &Path) -> Result<Option<LedgerSet>> {
        let m = std::fs::metadata(loc)?;
        let len = m.len();
        if len < self.len || file_id(&m) != self.id || others_stamp(loc)? != self.others {
            return Ok(None);
        }
        // A file that shrinks while it is read is not an append either.
        if read_range(loc, 0, self.head.len() as u64).ok().as_ref() != Some(&self.head) {
            return Ok(None);
        }
        let from = self.len - self.tail.len() as u64;
        let Ok(bytes) = read_range(loc, from, len) else { return Ok(None) };
        if bytes[..self.tail.len()] != self.tail[..] {
            return Ok(None);
        }
        let appended = &bytes[self.tail.len()..];
        let keep = (bytes.len() as u64).min(TAIL) as usize;
        self.tail = bytes[bytes.len() - keep..].to_vec();
        self.len = len;
        self.state.feed(appended)
    }
}

/// [`sync`], reading a followed location on from where the last sync of the
/// binding stopped (see the module docs), and keeping the state in `follows`.
pub fn sync_with(store: &mut Store, binding: &str, follows: &mut Follows) -> Result<Synced> {
    let row = store.binding(binding)?.ok_or_else(|| anyhow!("no binding {binding:?}"))?;
    let mut out = Synced { binding: binding.to_string(), ..Default::default() };
    if !row.active {
        follows.by_binding.remove(binding);
        out.deferred = Some(format!("not followed ({})", row.ended.as_deref().unwrap_or("inactive")));
        return Ok(out);
    }
    let loc = PathBuf::from(&row.location);
    if !loc.exists() {
        out.deferred = Some(format!("{} is missing", loc.display()));
        return Ok(out);
    }
    if let Some(t) = torn(&loc)? {
        out.deferred = Some(format!("{}: the final line is incomplete", t.display()));
        return Ok(out);
    }
    let st = stamp(&loc)?;
    if let Some(fl) = follows.by_binding.get_mut(binding).filter(|fl| fl.location == loc)
        && let Some(delta) = fl.read_on(&loc)?
    {
        apply(store, &row.format, &delta, &mut out)?;
        for l in delta.iter() {
            let ids = l.events.iter().map(|e| e.id.clone());
            fl.cursor.seen.entry(l.id().to_string()).or_default().extend(ids.clone());
            // Appended past a matched fact's bytes (the file only grew since
            // the sync that matched it): read there by a sync matching it.
            if let Some(fact) = fl.cursor.matched.clone() {
                fl.cursor.past.entry(fact).or_default().entry(l.id().to_string()).or_default().extend(ids);
            }
        }
        fl.cursor.stamp = st;
        store.set_cursor(binding, &fl.cursor.text())?;
        out.followed = true;
        return Ok(out);
    }
    follows.by_binding.remove(binding);
    let f = formats::by_name(&row.format).ok_or_else(|| anyhow!("binding {binding}: unknown format {:?}", row.format))?;
    let opts = ImportOptions::standard();
    let before = std::fs::metadata(&loc)?;
    let (imported, state) = match f.follow(&loc, &opts) {
        Some(r) => {
            let (i, s) = r.with_context(|| format!("re-reading {}", loc.display()))?;
            (i, Some(s))
        }
        None => (f.import(&loc, &opts).with_context(|| format!("re-reading {}", loc.display()))?, None),
    };
    let prev = store.cursor(binding)?.and_then(|c| Cursor::parse(&c));
    // A re-projectable projection binding with `protoledger/projected`
    // facts is measured against them (SPEC §11.2); every other binding, and
    // a legacy one no fact matches, by §9.3 (the module docs).
    let facts = if row.mode == Mode::Projection.as_str() && reprojectable(&row.format, &loc).is_ok() {
        Some(store.projections(&row.agent, binding)?).filter(|p| !p.facts.is_empty())
    } else {
        None
    };
    let measured = match &facts {
        Some(facts) => reproject::measure(f, &loc, &std::fs::read(&loc)?, facts, &imported.set),
        None => None,
    };
    let verdict = match (&facts, &measured) {
        (Some(_), Some(m)) => match prev.as_ref().and_then(|p| p.past.get(&m.fact)) {
            Some(read) => reproject::past_divergence(read, &m.past, |agent| redacted_in(store, agent)),
            None => None,
        },
        (Some(facts), None) if !facts.legacy || prev.as_ref().is_some_and(|p| p.ever_matched) => Some(format!(
            "no protoledger/projected fact of the binding matches {} (its {} facts name {}; the file holds {} bytes)",
            loc.display(),
            facts.facts.len(),
            facts.facts.iter().map(|f| format!("{} bytes", f.data.bytes)).collect::<Vec<_>>().join(", "),
            std::fs::metadata(&loc).map(|m| m.len()).unwrap_or(0)
        )),
        _ => prev.as_ref().and_then(|prev| divergence(prev, &imported.set)),
    };
    if let Some(detail) = verdict {
        store.append(&row.agent, bindings::diverged_event(binding, &row.agent, &detail))?;
        out.diverged = Some(detail);
        return Ok(out);
    }
    apply(store, f.name(), &imported.set, &mut out)?;
    let mut cursor = Cursor::of(&imported.set, st);
    if let Some(prev) = &prev {
        cursor.past = prev.past.clone();
        cursor.ever_matched = prev.ever_matched;
    }
    if let Some(m) = measured {
        cursor.ever_matched = true;
        cursor.past.insert(m.fact.clone(), m.past);
        cursor.matched = Some(m.fact);
    }
    store.set_cursor(binding, &cursor.text())?;
    // Keep the state when it read the file as it stands: the same file, the
    // length it had before and after the read.
    if let Some(state) = state {
        let after = std::fs::metadata(&loc)?;
        let len = state.len();
        if len > 0 && len == before.len() && len == after.len() && file_id(&before) == file_id(&after) {
            let from = len.saturating_sub(TAIL);
            let following = Following {
                head: read_range(&loc, 0, len.min(TAIL))?,
                tail: read_range(&loc, from, len)?,
                location: loc.clone(),
                state,
                len,
                id: file_id(&after),
                others: others_stamp(&loc)?,
                cursor,
            };
            follows.by_binding.insert(binding.to_string(), following);
        }
    }
    Ok(out)
}

/// The events of `agent`'s ledger that are redacted (SPEC §11.1); none
/// when the store does not hold the agent.
fn redacted_in(store: &Store, agent: &str) -> HashSet<String> {
    store.reader(agent).ok().and_then(|r| r.ledger().ok()).map(|l| l.redacted()).unwrap_or_default()
}

/// Sync every active binding.
pub fn sync_all(store: &mut Store) -> Result<Vec<Synced>> {
    let mut out = Vec::new();
    for b in store.bindings()?.into_iter().filter(|b| b.active) {
        out.push(sync(store, &b.binding)?);
    }
    Ok(out)
}

/// Why `now` is not a continuation of what `prev` saw, if it is not: every
/// id seen before must still be there, in the same order.
fn divergence(prev: &Cursor, now: &LedgerSet) -> Option<String> {
    for (agent, seen) in &prev.seen {
        let Some(l) = now.get(agent) else {
            return Some(format!("agent {agent} is no longer at the location"));
        };
        let mut it = l.events.iter().map(|e| e.id.as_str());
        let missing: Vec<&str> = seen.iter().map(String::as_str).filter(|id| !l.contains(id)).collect();
        if !missing.is_empty() {
            return Some(format!("agent {agent}: {} of {} events read earlier are gone (first: {})", missing.len(), seen.len(), missing[0]));
        }
        if !seen.iter().all(|id| it.any(|x| x == id)) {
            return Some(format!("agent {agent}: events read earlier are no longer in the same order"));
        }
    }
    None
}

/// Append to the store what `set` holds that the store lacks: through the
/// format's in-place hook when it has one ([`inplace`], SPEC §4.1), else
/// every event whose id its agent's ledger lacks.
fn apply(store: &mut Store, format: &str, set: &LedgerSet, out: &mut Synced) -> Result<()> {
    if inplace::applies(format) {
        return inplace::apply(store, set, out);
    }
    apply_ids(store, set, out)
}

/// The ledgers of `set`, parents before their children.
fn parents_first(set: &LedgerSet) -> Result<Vec<&Ledger>> {
    let mut order: Vec<&Ledger> = Vec::new();
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
    Ok(order)
}

/// Append to the store every event of `set` whose id its agent's ledger
/// lacks, in order, creating agents the store does not hold (parents first).
fn apply_ids(store: &mut Store, set: &LedgerSet, out: &mut Synced) -> Result<()> {
    for l in parents_first(set)? {
        let id = l.id();
        let held: HashSet<String> = if store.agent(id)?.is_some() {
            store.events(id, 0, usize::MAX)?.into_iter().map(|r| r.id).collect()
        } else {
            store.create_agent(l.agent().clone())?;
            out.created.push(id.to_string());
            HashSet::new()
        };
        for e in &l.events {
            if held.contains(&e.id) {
                continue;
            }
            let at = store.append(id, e.clone())?;
            let mut event = e.clone();
            event.seq = at.seq;
            out.appended.push(Published { agent: id.to_string(), seq: at.seq, offset: at.offset, event });
        }
    }
    Ok(())
}

/// The files a location is read from: the location itself (a file, or every
/// file under a directory), for a file its SQLite write-ahead log
/// `<file>-wal` when there is one (opencode's `opencode.db`: a write sits
/// there until a checkpoint, and a reader sees it), and for a file
/// `<stem>.<ext>` its sidecar directory `<stem>/` when there is one (Claude
/// Code's subagents). The `-shm` file is not one of them: it is the WAL's
/// index, holds no record, and every write already moves the WAL. Nor is a
/// re-projection's temporary file ([`reproject`]), which sync ignores.
fn files_of(loc: &Path) -> Result<Vec<PathBuf>> {
    let mut out = Vec::new();
    let walk = |d: &Path, out: &mut Vec<PathBuf>| {
        for e in walkdir::WalkDir::new(d).sort_by_file_name().into_iter().filter_map(|e| e.ok()) {
            if e.file_type().is_file() && !reproject::is_temp(e.path()) {
                out.push(e.into_path());
            }
        }
    };
    if loc.is_dir() {
        walk(loc, &mut out);
    } else {
        out.push(loc.to_path_buf());
        if let Some(w) = wal(loc).filter(|w| w.is_file()) {
            out.push(w);
        }
        if let Some(side) = sidecar(loc)
            && side.is_dir()
        {
            walk(&side, &mut out);
        }
    }
    Ok(out)
}

/// A file's SQLite write-ahead log: `<file>-wal`, beside it.
pub(crate) fn wal(loc: &Path) -> Option<PathBuf> {
    let mut name = loc.file_name()?.to_os_string();
    name.push("-wal");
    Some(loc.with_file_name(name))
}

pub(crate) fn sidecar(loc: &Path) -> Option<PathBuf> {
    let stem = loc.file_stem()?;
    Some(loc.parent()?.join(stem))
}

/// Fingerprint of a location's files: path, size, modification time.
pub fn stamp(loc: &Path) -> Result<String> {
    use sha2::{Digest, Sha256};
    let mut h = Sha256::new();
    for p in files_of(loc)? {
        let Ok(m) = std::fs::metadata(&p) else { continue };
        let mtime = m.modified().ok().and_then(|t| t.duration_since(std::time::UNIX_EPOCH).ok()).map(|d| d.as_nanos()).unwrap_or(0);
        h.update(format!("{}\0{}\0{}\n", p.display(), m.len(), mtime).as_bytes());
    }
    Ok(hex::encode(h.finalize()))
}

/// A line-format file of the location whose final line has no newline yet.
fn torn(loc: &Path) -> Result<Option<PathBuf>> {
    use std::io::{Read, Seek, SeekFrom};
    for p in files_of(loc)? {
        if p.extension().is_none_or(|x| x != "jsonl") {
            continue;
        }
        let mut f = std::fs::File::open(&p)?;
        let len = f.metadata()?.len();
        if len == 0 {
            continue;
        }
        f.seek(SeekFrom::Start(len - 1))?;
        let mut b = [0u8; 1];
        f.read_exact(&mut b)?;
        if b[0] != b'\n' {
            return Ok(Some(p));
        }
    }
    Ok(None)
}
