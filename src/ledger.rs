//! Ledgers and ledger sets, and their on-disk JSONL form (SPEC §2).

use crate::model::{Agent, Event, Header, Kind, SPEC_VERSION};
use anyhow::{Context, Result, anyhow, bail};
use serde_json::{Value, json};
use std::collections::{BTreeMap, BTreeSet, HashMap, HashSet};
use std::fs::{self, File, OpenOptions};
use std::io::Write;
use std::path::{Path, PathBuf};

/// One agent's ledger: a header plus its events in order.
#[derive(Debug, Clone, PartialEq)]
pub struct Ledger {
    pub header: Header,
    pub events: Vec<Event>,
    ids: HashSet<String>,
    /// Set only on a ledger made by [`Ledger::projected`]: the native records
    /// (`(format, id)`) of the source ledger that the projection does not
    /// hold intact, so an export of it never writes them (SPEC §11.1). An
    /// exporter whose target names an earlier record from a later one
    /// (Claude Code's `parentUuid`) re-points a reference to one of these.
    struck: BTreeSet<(String, String)>,
}

/// `protoledger/redacted {events, reason}` (SPEC §11.1).
pub const REDACTED: &str = "protoledger/redacted";
/// `protoledger/unredacted {events}` (SPEC §11.1).
pub const UNREDACTED: &str = "protoledger/unredacted";
/// The `app` types of the §11 facts (SPEC §11): protoledger's metadata,
/// never conversation, so no projected view holds them.
pub const FACTS: [&str; 7] =
    [REDACTED, UNREDACTED, "protoledger/projected", "protoledger/message", "protoledger/delivered", "protoledger/cancelled", "protoledger/withdrawn"];

/// The §11 type of `e`, when it is a §11 fact.
pub fn fact_type(e: &Event) -> Option<&'static str> {
    if e.kind != Kind::App || e.unknown_kind.is_some() {
        return None;
    }
    let t = e.str("type")?;
    FACTS.iter().find(|f| **f == t).copied()
}

/// Is `e` a §11 fact (SPEC §11)?
pub fn is_fact(e: &Event) -> bool {
    fact_type(e).is_some()
}

/// Why a redaction or unredaction is refused (SPEC §11.1). A refused one
/// appends nothing.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum RedactError {
    #[error("{0} lists no events")]
    Empty(&'static str),
    #[error("event {0:?} is not in the ledger")]
    Unknown(String),
    #[error("event {0:?} is a {1} fact: a redaction is withdrawn by protoledger/unredacted, never by redacting it")]
    Fact(String, &'static str),
}

fn check_ids<'a>(unredact: bool, ids: &[String], held: impl Fn(&str) -> bool, fact_of: impl Fn(&str) -> Option<&'a str>) -> Result<(), RedactError> {
    if ids.is_empty() {
        return Err(RedactError::Empty(if unredact { UNREDACTED } else { REDACTED }));
    }
    for id in ids {
        if !held(id) {
            return Err(RedactError::Unknown(id.clone()));
        }
        if !unredact && let Some(t) = fact_of(id).filter(|t| [REDACTED, UNREDACTED].contains(t)) {
            return Err(RedactError::Fact(id.clone(), if t == REDACTED { REDACTED } else { UNREDACTED }));
        }
    }
    Ok(())
}

/// A redaction or unredaction fact (SPEC §11.1), ready to append.
pub fn redaction_event(unredact: bool, ids: &[String], reason: &str) -> Event {
    let (ty, data) = if unredact { (UNREDACTED, json!({ "events": ids })) } else { (REDACTED, json!({ "events": ids, "reason": reason })) };
    let now = std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).map(|d| d.as_millis() as i64).unwrap_or(0);
    Event::new(format!("{ty}:{}", uuid::Uuid::new_v4()), Kind::App).ts(Some(crate::util::ms_to_rfc3339(now))).with("type", ty).with("data", data)
}

#[derive(Debug, thiserror::Error)]
pub enum AppendError {
    #[error("event id {0} is already in the ledger")]
    Replay(String),
}

impl Ledger {
    pub fn new(agent: Agent) -> Self {
        Ledger { header: Header { protoledger: SPEC_VERSION.to_string(), agent }, events: Vec::new(), ids: HashSet::new(), struck: BTreeSet::new() }
    }

    pub fn id(&self) -> &str {
        &self.header.agent.id
    }

    pub fn agent(&self) -> &Agent {
        &self.header.agent
    }

    pub fn agent_mut(&mut self) -> &mut Agent {
        &mut self.header.agent
    }

    pub fn contains(&self, id: &str) -> bool {
        self.ids.contains(id)
    }

    /// Append one event, assigning its `seq`. A repeated id is refused (SPEC §1.5).
    pub fn append(&mut self, mut e: Event) -> Result<&Event, AppendError> {
        if !self.ids.insert(e.id.clone()) {
            return Err(AppendError::Replay(e.id));
        }
        e.seq = self.events.len() as u64 + 1;
        self.events.push(e);
        Ok(self.events.last().unwrap())
    }

    /// Append, making the id unique with a `~n` suffix if it repeats. For
    /// importers whose sources legitimately repeat a native id.
    pub fn append_unique(&mut self, mut e: Event) -> &Event {
        if self.ids.contains(&e.id) {
            let base = e.id.clone();
            let mut n = 2;
            while self.ids.contains(&format!("{base}~{n}")) {
                n += 1;
            }
            e.id = format!("{base}~{n}");
        }
        self.append(e).expect("id made unique")
    }

    pub fn last_seq(&self) -> u64 {
        self.events.len() as u64
    }

    // ---- branches (SPEC §2.3) ----

    /// The **active path** (SPEC §2.3): start at the last event and follow
    /// `after` (or, where `after` is absent, the previous event) back to the
    /// start; returned in file order. Events on abandoned branches stay in the
    /// ledger and are not on it.
    pub fn active_path(&self) -> Vec<&Event> {
        let on = self.on_path(|_| true);
        self.events.iter().zip(on).filter(|(_, on)| *on).map(|(e, _)| e).collect()
    }

    /// Does any event start a branch, i.e. continue from an earlier event that
    /// is not simply the previous one?
    pub fn is_branched(&self) -> bool {
        (0..self.events.len()).any(|i| self.branch_target(i).is_some())
    }

    /// The index of the event `events[i].after` continues from, when that is an
    /// earlier event other than the previous one. An `after` naming nothing
    /// earlier in this ledger (a parent outside the file) starts no branch: the
    /// predecessor is then the previous event.
    fn branch_target(&self, i: usize) -> Option<usize> {
        let after = self.events[i].after.as_deref()?;
        let t = self.events[..i].iter().rposition(|e| e.id == after)?;
        (t + 1 < i).then_some(t)
    }

    /// Which events are on the path an exporter renders, by index. Walking back
    /// from the last event, an event that starts a branch jumps to the event it
    /// continues from (abandoning everything in between) when `follow_after`
    /// says so. It says no when the target can express that branch itself — as
    /// a harness's own record of its rewind does when written back to that
    /// harness — and the walk then goes on through the abandoned events too.
    /// `on_path(|_| true)` is the active path.
    pub fn on_path(&self, mut follow_after: impl FnMut(&Event) -> bool) -> Vec<bool> {
        let mut on = vec![false; self.events.len()];
        let mut cur = self.events.len().checked_sub(1);
        while let Some(i) = cur {
            on[i] = true;
            cur = match self.branch_target(i) {
                Some(t) if follow_after(&self.events[i]) => Some(t),
                _ => i.checked_sub(1),
            };
        }
        on
    }

    // ---- redaction and the projected view (SPEC §11.1) ----

    /// Check a `protoledger/redacted` (`unredact` false) or
    /// `protoledger/unredacted` (`unredact` true) of `ids` against the ledger
    /// as it is now: the refusals of SPEC §11.1.
    pub fn check_redaction(&self, unredact: bool, ids: &[String]) -> Result<(), RedactError> {
        let fact_of: HashMap<&str, &str> = self.events.iter().filter_map(|e| fact_type(e).map(|t| (e.id.as_str(), t))).collect();
        check_ids(unredact, ids, |id| self.contains(id), |id| fact_of.get(id).copied())
    }

    /// The ids of the events that are **redacted** (SPEC §11.1): those whose
    /// latest `protoledger/redacted` or `protoledger/unredacted` fact is a
    /// `protoledger/redacted`. The facts are read from the whole ledger,
    /// abandoned branches included, in `seq` order; a fact its writer would
    /// have refused where it stands (no events, an id not yet in the ledger,
    /// a redaction naming a redaction fact, a malformed `data`) is ignored.
    pub fn redacted(&self) -> HashSet<String> {
        let mut held: HashSet<&str> = HashSet::new();
        let mut fact_of: HashMap<&str, &str> = HashMap::new();
        let mut state: HashMap<&str, bool> = HashMap::new();
        for e in &self.events {
            let ty = fact_type(e);
            if let Some(t) = ty.filter(|t| [REDACTED, UNREDACTED].contains(t)) {
                let unredact = t == UNREDACTED;
                let ids: Option<Vec<String>> = e
                    .body
                    .get("data")
                    .and_then(|d| d.get("events"))
                    .and_then(Value::as_array)
                    .and_then(|a| a.iter().map(|v| v.as_str().map(str::to_string)).collect());
                let ok = ids.as_ref().is_some_and(|ids| check_ids(unredact, ids, |id| held.contains(id), |id| fact_of.get(id).copied()).is_ok());
                if ok {
                    for id in ids.unwrap() {
                        let k = held.get(id.as_str()).copied().expect("checked: held");
                        state.insert(k, !unredact);
                    }
                }
            }
            held.insert(&e.id);
            if let Some(t) = ty {
                fact_of.insert(&e.id, t);
            }
        }
        state.into_iter().filter(|(_, r)| *r).map(|(id, _)| id.to_string()).collect()
    }

    /// The **projected view** (SPEC §11.1): the active path (§2.3) in file
    /// order, minus every redacted event and minus every §11 fact.
    pub fn projected_view(&self) -> Vec<&Event> {
        let redacted = self.redacted();
        self.active_path().into_iter().filter(|e| !redacted.contains(&e.id) && !is_fact(e)).collect()
    }

    /// The ledger an exporter renders (SPEC §11.1): this one without its
    /// redacted events and, unless `keep_facts` (the `protoledger` format's
    /// own export), without its §11 facts. An exporter computes the active
    /// path of the result itself, so what it renders as the conversation is
    /// [`Ledger::projected_view`]; one that renders abandoned branches finds
    /// the redacted events left out of those too. The rest stays a valid
    /// conversation: an event whose `after` (or previous event) was left out
    /// continues from the nearest earlier event left in on its path; a native
    /// record any of whose parts was left out loses its `native` on the
    /// parts that remain (they are rendered from their neutral bodies), and
    /// is named in [`Ledger::struck`]; of the versions of one record (§4.1)
    /// the newest one left in is the one an exporter finds. `seq`s are
    /// renumbered. `None` when nothing is left out: the ledger exports as
    /// itself.
    pub fn projected(&self, keep_facts: bool) -> Option<Ledger> {
        let redacted = self.redacted();
        let drop: Vec<bool> = self.events.iter().map(|e| if is_fact(e) { !keep_facts } else { redacted.contains(&e.id) }).collect();
        if !drop.contains(&true) {
            return None;
        }
        let pred = |i: usize| self.branch_target(i).or_else(|| i.checked_sub(1));
        // (format, native id) → (parts kept, parts left out)
        type Parts = (BTreeSet<u32>, BTreeSet<u32>);
        let mut parts: HashMap<(&str, &str), Parts> = HashMap::new();
        for (e, d) in self.events.iter().zip(&drop) {
            if let Some(n) = &e.native
                && let Some(id) = &n.id
            {
                let (kept, dropped) = parts.entry((n.format.as_str(), id.as_str())).or_default();
                if *d {
                    dropped.insert(n.part)
                } else {
                    kept.insert(n.part)
                };
            }
        }
        let broken: HashSet<(&str, &str)> = parts.iter().filter(|(_, (kept, dropped))| dropped.iter().any(|p| !kept.contains(p))).map(|(k, _)| *k).collect();
        let mut out = Ledger::new(self.header.agent.clone());
        out.header = self.header.clone();
        let mut last_kept: Option<usize> = None;
        for (i, e) in self.events.iter().enumerate() {
            if drop[i] {
                continue;
            }
            let mut e = e.clone();
            if pred(i).is_some_and(|p| drop[p]) {
                let mut p = pred(i);
                while let Some(j) = p.filter(|j| drop[*j]) {
                    p = pred(j);
                }
                e.after = match p {
                    // Simply the previous event of the projection: no `after`,
                    // unless it names a parent outside this ledger (no branch).
                    Some(j) if Some(j) == last_kept => e.after.filter(|a| !self.contains(a)),
                    Some(j) => Some(self.events[j].id.clone()),
                    None => e.after.filter(|a| !self.contains(a)),
                };
            }
            if e.native.as_ref().is_some_and(|n| n.id.as_deref().is_some_and(|id| broken.contains(&(n.format.as_str(), id)))) {
                e.native = None;
            }
            e.seq = 0;
            out.append(e).expect("ids are unique in the source");
            last_kept = Some(i);
        }
        out.struck = broken.into_iter().map(|(f, id)| (f.to_string(), id.to_string())).collect();
        Some(out)
    }

    /// Did [`Ledger::projected`] strike any native record from this ledger?
    pub fn has_struck(&self) -> bool {
        !self.struck.is_empty()
    }

    /// Is the native record `(format, id)` one this ledger's source holds and
    /// a projection of it does not hold intact ([`Ledger::projected`])?
    pub fn struck(&self, format: &str, id: &str) -> bool {
        !self.struck.is_empty() && self.struck.contains(&(format.to_string(), id.to_string()))
    }

    // ---- serialisation ----

    pub fn to_jsonl(&self) -> String {
        let mut out = serde_json::to_string(&self.header).expect("header serialises");
        out.push('\n');
        for e in &self.events {
            out.push_str(&serde_json::to_string(e).expect("event serialises"));
            out.push('\n');
        }
        out
    }

    /// Parse a ledger. A final line without a newline is a torn append and is
    /// ignored (SPEC §2); its presence is returned as `torn = true`.
    pub fn from_jsonl(text: &str) -> Result<(Ledger, bool)> {
        let torn = !text.is_empty() && !text.ends_with('\n');
        let mut lines: Vec<&str> = text.split('\n').collect();
        lines.pop(); // the piece after the final '\n' (empty, or the torn tail)
        let mut it = lines.into_iter().enumerate();
        let (_, first) = it.next().ok_or_else(|| anyhow!("empty ledger"))?;
        let header: Header = serde_json::from_str(first).context("line 1 is not a protoledger header")?;
        let mut l = Ledger::new(header.agent.clone());
        l.header = header;
        for (n, line) in it {
            if line.trim().is_empty() {
                continue;
            }
            let e: Event = serde_json::from_str(line).with_context(|| format!("line {}", n + 1))?;
            let want = l.last_seq() + 1;
            if e.seq != want {
                bail!("line {}: seq {} where {} was expected", n + 1, e.seq, want);
            }
            l.append(e).map_err(|err| anyhow!("line {}: {err}", n + 1))?;
        }
        Ok((l, torn))
    }

    pub fn read(path: &Path) -> Result<Ledger> {
        let text = fs::read_to_string(path).with_context(|| format!("reading {}", path.display()))?;
        Ok(Ledger::from_jsonl(&text).with_context(|| path.display().to_string())?.0)
    }
}

/// A set of ledgers that reference each other, keyed by agent id.
#[derive(Debug, Clone, Default, PartialEq)]
pub struct LedgerSet {
    pub ledgers: BTreeMap<String, Ledger>,
}

impl LedgerSet {
    pub fn new() -> Self {
        Self::default()
    }

    pub fn single(l: Ledger) -> Self {
        let mut s = Self::new();
        s.insert(l);
        s
    }

    pub fn insert(&mut self, l: Ledger) {
        self.ledgers.insert(l.id().to_string(), l);
    }

    pub fn get(&self, id: &str) -> Option<&Ledger> {
        self.ledgers.get(id)
    }

    pub fn iter(&self) -> impl Iterator<Item = &Ledger> {
        self.ledgers.values()
    }

    pub fn len(&self) -> usize {
        self.ledgers.len()
    }

    pub fn is_empty(&self) -> bool {
        self.ledgers.is_empty()
    }

    /// Ledgers that name `agent` in a link of relation `rel` (e.g. every agent
    /// `spawned-by` it): the reverse of `agent.links`.
    pub fn linked_from<'a>(&'a self, agent: &'a str, rel: &'a str) -> impl Iterator<Item = &'a Ledger> + 'a {
        self.ledgers.values().filter(move |l| l.agent().links.iter().any(|k| k.rel == rel && k.agent.as_deref() == Some(agent)))
    }

    /// Ledgers with no structural parent inside this set.
    pub fn roots(&self) -> impl Iterator<Item = &Ledger> {
        self.ledgers.values().filter(|l| match &l.agent().parent {
            None => true,
            Some(p) => !self.ledgers.contains_key(&p.agent),
        })
    }

    /// Direct structural children of `id`, in creation order.
    pub fn children_of<'a>(&'a self, id: &'a str) -> impl Iterator<Item = &'a Ledger> + 'a {
        self.ledgers.values().filter(move |l| l.agent().parent.as_ref().is_some_and(|p| p.agent == id))
    }

    /// Check the cross-ledger rules: parents exist, no cycles.
    pub fn validate(&self) -> Vec<String> {
        let mut problems = Vec::new();
        for l in self.ledgers.values() {
            let mut seen = HashSet::new();
            let mut cur = l;
            while let Some(p) = &cur.agent().parent {
                if !seen.insert(cur.id().to_string()) {
                    problems.push(format!("{}: parent chain has a cycle", l.id()));
                    break;
                }
                match self.ledgers.get(&p.agent) {
                    Some(next) => {
                        if let Some(ev) = &p.event
                            && !next.contains(ev)
                        {
                            problems.push(format!("{}: parent event {ev} is not in {}", cur.id(), p.agent));
                        }
                        cur = next;
                    }
                    None => break, // parent outside this set is allowed
                }
            }
        }
        problems
    }

    // ---- directory form ----

    /// Write every ledger as `<dir>/<agent id>.jsonl`. Existing files are
    /// refused unless `overwrite`.
    pub fn write_dir(&self, dir: &Path, overwrite: bool) -> Result<Vec<PathBuf>> {
        fs::create_dir_all(dir)?;
        let mut written = Vec::new();
        for l in self.ledgers.values() {
            let path = dir.join(format!("{}.jsonl", file_safe(l.id())));
            if path.exists() && !overwrite {
                bail!("{} exists (pass --force to overwrite)", path.display());
            }
            write_atomic(&path, l.to_jsonl().as_bytes())?;
            written.push(path);
        }
        Ok(written)
    }

    /// Read every `*.jsonl` ledger in `dir`, or a single ledger file.
    pub fn read_path(path: &Path) -> Result<LedgerSet> {
        let mut set = LedgerSet::new();
        if path.is_file() {
            set.insert(Ledger::read(path)?);
            return Ok(set);
        }
        let mut entries: Vec<_> = fs::read_dir(path)
            .with_context(|| format!("reading {}", path.display()))?
            .filter_map(|e| e.ok().map(|e| e.path()))
            .filter(|p| p.extension().is_some_and(|x| x == "jsonl"))
            .collect();
        entries.sort();
        for p in entries {
            set.insert(Ledger::read(&p)?);
        }
        Ok(set)
    }
}

/// Agent ids become file names; keep them portable.
pub fn file_safe(id: &str) -> String {
    id.chars().map(|c| if c.is_ascii_alphanumeric() || "-_.@".contains(c) { c } else { '_' }).collect()
}

/// Write via temp file + fsync + rename + directory fsync.
pub fn write_atomic(path: &Path, bytes: &[u8]) -> Result<()> {
    let dir = path.parent().unwrap_or(Path::new("."));
    fs::create_dir_all(dir)?;
    let tmp = path.with_extension(format!("{}.tmp-{}", path.extension().and_then(|e| e.to_str()).unwrap_or(""), std::process::id()));
    {
        let mut f = File::create(&tmp)?;
        f.write_all(bytes)?;
        f.sync_all()?;
    }
    fs::rename(&tmp, path)?;
    if let Ok(d) = File::open(dir) {
        let _ = d.sync_all();
    }
    Ok(())
}

/// Where an appended event landed in its file.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Appended {
    pub seq: u64,
    /// Byte offset of the event's line.
    pub offset: u64,
    /// Length of the line, including its newline.
    pub len: u64,
}

/// An append handle on one ledger file, holding an exclusive advisory lock for
/// as long as it lives (one writer per file). Readers need no lock: an
/// append-only file is always safe to read up to its last newline.
pub struct Appender {
    file: File,
    ledger: Ledger,
    len: u64,
}

impl Appender {
    /// Open (or create with `agent`) a ledger file for appending. A new file
    /// is durable when this returns: its header is fsynced, and so is the
    /// directory entry that names it.
    pub fn open(path: &Path, agent: Option<Agent>) -> Result<Appender> {
        let existed = path.exists();
        let file = OpenOptions::new().read(true).append(true).create(true).open(path)?;
        file.try_lock().map_err(|e| anyhow!("{} is locked by another writer: {e}", path.display()))?;
        Appender::locked(file, path, agent, existed)
    }

    /// An appender on `file`, the ledger file at `path`, already opened for
    /// reading and appending and locked by the caller. The ledger is read
    /// through `file`, the handle that holds the lock, never by path.
    pub(crate) fn locked(file: File, path: &Path, agent: Option<Agent>, existed: bool) -> Result<Appender> {
        let mut text = String::new();
        std::io::Read::read_to_string(&mut &file, &mut text).with_context(|| format!("reading {}", path.display()))?;
        let (ledger, len) = if text.is_empty() {
            let agent = agent.ok_or_else(|| anyhow!("{} is empty and no agent header was given", path.display()))?;
            let l = Ledger::new(agent);
            let mut line = serde_json::to_string(&l.header)?;
            line.push('\n');
            let mut f = &file;
            f.write_all(line.as_bytes())?;
            f.sync_data()?;
            if !existed {
                sync_dir(path)?;
            }
            (l, line.len() as u64)
        } else {
            let (l, torn) = Ledger::from_jsonl(&text)?;
            let keep = text.rfind('\n').map(|i| i + 1).unwrap_or(0) as u64;
            if torn {
                // Repair: cut the torn tail so the next append starts on a line boundary.
                file.set_len(keep)?;
                file.sync_data()?;
            }
            (l, keep)
        };
        Ok(Appender { file, ledger, len })
    }

    pub fn ledger(&self) -> &Ledger {
        &self.ledger
    }

    /// Bytes of the file that hold complete lines.
    pub fn len(&self) -> u64 {
        self.len
    }

    pub fn is_empty(&self) -> bool {
        self.ledger.events.is_empty()
    }

    /// Append a `protoledger/redacted` (`unredact` false) or
    /// `protoledger/unredacted` (`unredact` true) fact for `ids`, checked
    /// against the ledger as this writer holds it, under its lock (SPEC §11):
    /// a refused one ([`RedactError`]) appends nothing.
    pub fn redaction(&mut self, unredact: bool, ids: &[String], reason: &str) -> Result<Appended> {
        self.ledger.check_redaction(unredact, ids)?;
        self.append(redaction_event(unredact, ids, reason))
    }

    /// Append and fsync one event. Durable when this returns.
    pub fn append(&mut self, e: Event) -> Result<Appended> {
        let e = self.ledger.append(e)?.clone();
        let mut line = serde_json::to_string(&e)?;
        line.push('\n');
        // One write per line, so a crash leaves at most one torn tail.
        self.file.write_all(line.as_bytes())?;
        self.file.sync_data()?;
        let at = Appended { seq: e.seq, offset: self.len, len: line.len() as u64 };
        self.len += at.len;
        Ok(at)
    }
}

fn sync_dir(path: &Path) -> Result<()> {
    let dir = path.parent().filter(|d| !d.as_os_str().is_empty()).unwrap_or(Path::new("."));
    File::open(dir)?.sync_all()?;
    Ok(())
}

/// Read the one event whose line starts at `offset` (as returned by
/// [`Appender::append`] or [`tail`]) in the plain ledger file at `path`.
///
/// For plain files only: `offset` is a byte offset in that file. A ledger in
/// a store may be stored compressed (SPEC §10), so a store's ledgers are read
/// through the store (`Store::reader`, a `store::LedgerReader`), never by path.
pub fn read_at(path: &Path, offset: u64) -> Result<Event> {
    use std::io::{BufRead, BufReader, Seek, SeekFrom};
    let mut f = File::open(path)?;
    f.seek(SeekFrom::Start(offset))?;
    let mut line = String::new();
    BufReader::new(f).read_line(&mut line)?;
    if !line.ends_with('\n') {
        bail!("{}: no complete line at offset {offset}", path.display());
    }
    Ok(serde_json::from_str(line.trim_end())?)
}

/// Events in complete lines from byte `offset` on (0 = the start; the header
/// is skipped), each with its own offset, plus the offset to resume from.
/// A torn final line is left for the next call.
///
/// For plain files only, as [`read_at`] is: a store's ledgers are read
/// through the store (`Store::reader`), never by path.
pub fn tail(path: &Path, offset: u64) -> Result<(Vec<(u64, Event)>, u64)> {
    use std::io::{Read, Seek, SeekFrom};
    let mut f = File::open(path)?;
    f.seek(SeekFrom::Start(offset))?;
    let mut buf = Vec::new();
    f.read_to_end(&mut buf)?;
    let complete = buf.iter().rposition(|b| *b == b'\n').map(|i| i + 1).unwrap_or(0);
    let mut out = Vec::new();
    let mut at = offset;
    for line in buf[..complete].split_inclusive(|b| *b == b'\n') {
        let start = at;
        at += line.len() as u64;
        let text = std::str::from_utf8(line)?.trim_end();
        if text.is_empty() {
            continue;
        }
        if start == 0 {
            // Line 1 is the header.
            serde_json::from_str::<Header>(text).context("line 1 is not a protoledger header")?;
            continue;
        }
        out.push((start, serde_json::from_str(text).with_context(|| format!("offset {start}"))?));
    }
    Ok((out, at))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::model::{Event, Kind, Obj};

    fn fixture(case: &str) -> PathBuf {
        Path::new(env!("CARGO_MANIFEST_DIR")).join("fixtures/protoledger").join(case).join("native")
    }

    fn ids(path: &[&Event]) -> Vec<String> {
        path.iter().map(|e| e.id.clone()).collect()
    }

    /// SPEC §2.3: the active path of `branch-rewind` is exactly its active
    /// events, in file order; the abandoned ones stay in the ledger.
    #[test]
    fn active_path_of_branch_rewind_is_the_active_events_in_order() {
        let l = Ledger::read(&fixture("branch-rewind").join("rewind-1.jsonl")).unwrap();
        assert_eq!(l.events.len(), 8, "the abandoned branch stays in the file");
        assert!(l.is_branched());
        assert_eq!(ids(&l.active_path()), ["rewind-1:u:1", "rewind-1:a:1", "rewind-1:u:3", "rewind-1:a:3"]);
    }

    /// The opposing case: with no branch, the active path is every event, so a
    /// reader that only drops events (or keeps only some) fails one of the two.
    #[test]
    fn active_path_of_an_unbranched_ledger_is_every_event() {
        let l = Ledger::read(&fixture("minimal").join("hello-1.jsonl")).unwrap();
        assert!(!l.is_branched());
        assert_eq!(ids(&l.active_path()), l.events.iter().map(|e| e.id.clone()).collect::<Vec<_>>());
    }

    /// `after` naming the previous event, or nothing in this ledger (a
    /// parent outside the file), starts no branch; nested branches compose.
    #[test]
    fn after_that_names_no_earlier_event_starts_no_branch_and_branches_nest() {
        let mut l = Ledger::new(crate::model::Agent { id: "x".into(), created_at: None, source: None, parent: None, links: vec![], meta: Obj::new() });
        for (id, after) in [("a", None), ("b", Some("a")), ("c", Some("elsewhere")), ("d", None), ("e", Some("b")), ("f", None), ("g", Some("a"))] {
            l.append(Event::new(id, Kind::Text).after(after)).unwrap();
        }
        // g continues from a: b..f are abandoned (e's own branch inside them too).
        assert_eq!(ids(&l.active_path()), ["a", "g"]);
        l.append(Event::new("h", Kind::Text)).unwrap();
        assert_eq!(ids(&l.active_path()), ["a", "g", "h"]);
        // Without g: e continues from b, abandoning c and d; c's dangling after is no branch.
        l.events.truncate(6);
        assert_eq!(ids(&l.active_path()), ["a", "b", "e", "f"]);
        // A branch the target expresses itself is walked through.
        assert_eq!(l.on_path(|e| e.id != "e"), vec![true; 6]);
    }

    /// (d) The protoledger self trip of `branch-rewind` is byte-exact.
    #[test]
    fn branch_rewind_self_trip_is_byte_exact() {
        let f = crate::formats::by_name("protoledger").unwrap();
        let t = crate::roundtrip::self_trip(f, &fixture("branch-rewind")).unwrap();
        assert!(t.files.is_exact(), "{:?}", t.files);
        assert_eq!(t.reimport.score(), 0);
    }
    // ---- redaction (SPEC §11.1) ----

    fn plain_ledger(ids: &[(&str, Option<&str>)]) -> Ledger {
        let mut l = Ledger::new(crate::model::Agent { id: "x".into(), created_at: None, source: None, parent: None, links: vec![], meta: Obj::new() });
        for (id, after) in ids {
            l.append(Event::new(*id, Kind::Text).after(*after)).unwrap();
        }
        l
    }

    fn fact(l: &mut Ledger, id: &str, unredact: bool, events: &[&str]) {
        let mut e = redaction_event(unredact, &events.iter().map(|s| s.to_string()).collect::<Vec<_>>(), "r");
        e.id = id.into();
        l.append(e).unwrap();
    }

    fn view(l: &Ledger) -> Vec<String> {
        l.projected_view().into_iter().map(|e| e.id.clone()).collect()
    }

    /// The latest fact wins: redact, unredact, redact leaves it
    /// redacted; redact, unredact leaves it in the view. Opposed: an event no
    /// fact names stays in the view throughout.
    #[test]
    fn latest_redaction_fact_wins() {
        let mut l = plain_ledger(&[("a", None), ("b", None), ("c", None)]);
        assert_eq!(view(&l), ["a", "b", "c"]);
        fact(&mut l, "f1", false, &["b"]);
        assert_eq!(view(&l), ["a", "c"]);
        fact(&mut l, "f2", true, &["b"]);
        assert_eq!(view(&l), ["a", "b", "c"]);
        fact(&mut l, "f3", false, &["b"]);
        assert_eq!(view(&l), ["a", "c"]);
        assert_eq!(l.redacted(), HashSet::from(["b".to_string()]));
        assert_eq!(l.events.len(), 6, "every event stays in the ledger");
    }

    /// A redaction fact on an abandoned branch still applies; the view
    /// is computed from the whole ledger, not the active path.
    #[test]
    fn a_redaction_on_an_abandoned_branch_applies() {
        let mut l = plain_ledger(&[("a", None), ("b", None)]);
        fact(&mut l, "f", false, &["a"]);
        l.append(Event::new("c", Kind::Text).after(Some("b"))).unwrap();
        // f is abandoned (c continues from b); a is still redacted.
        assert_eq!(ids(&l.active_path()), ["a", "b", "c"]);
        assert!(!l.active_path().iter().any(|e| e.id == "f"));
        assert_eq!(view(&l), ["b", "c"]);
    }

    /// The writer refusals and a refused-shaped fact written by
    /// another program, which state derivation ignores: naming an id
    /// not yet in the ledger, naming a redaction fact, an empty list, a
    /// malformed list. Opposed: a valid fact in the same ledger applies.
    #[test]
    fn refusals_and_refused_shaped_facts_are_ignored() {
        let mut l = plain_ledger(&[("a", None), ("b", None)]);
        let s = |v: &[&str]| v.iter().map(|x| x.to_string()).collect::<Vec<_>>();
        assert_eq!(l.check_redaction(false, &[]), Err(RedactError::Empty(REDACTED)));
        assert_eq!(l.check_redaction(false, &s(&["a", "zz"])), Err(RedactError::Unknown("zz".into())));
        assert_eq!(l.check_redaction(true, &[]), Err(RedactError::Empty(UNREDACTED)));
        assert_eq!(l.check_redaction(true, &s(&["zz"])), Err(RedactError::Unknown("zz".into())));
        fact(&mut l, "ok", false, &["a"]);
        assert_eq!(l.check_redaction(false, &s(&["ok"])), Err(RedactError::Fact("ok".into(), REDACTED)));
        assert_eq!(l.check_redaction(true, &s(&["ok"])), Ok(()), "unredacting a fact changes nothing and is no refusal");
        // Written anyway (a raw Appender, another program): each is ignored.
        fact(&mut l, "later", false, &["b", "c"]); // c is not in the ledger yet
        l.append(Event::new("c", Kind::Text)).unwrap();
        fact(&mut l, "of-fact", false, &["b", "ok"]); // names a redaction
        fact(&mut l, "empty", false, &[]);
        let mut bad = redaction_event(false, &s(&["b"]), "r");
        bad.id = "malformed".into();
        bad.body.insert("data".into(), json!({"events": "b"}));
        l.append(bad).unwrap();
        fact(&mut l, "un-unknown", true, &["a", "nope"]); // would have unredacted a
        assert_eq!(l.redacted(), HashSet::from(["a".to_string()]));
        assert_eq!(view(&l), ["b", "c"]);
    }

    /// The projection keeps the rest a valid conversation: an event whose
    /// predecessor was redacted continues from the nearest kept one on its
    /// path (an `after` when that is not simply the previous kept event).
    #[test]
    fn projection_repoints_after_across_a_redacted_event() {
        // a b c | d continues from b (c abandoned); b redacted.
        let mut l = plain_ledger(&[("a", None), ("b", None), ("c", None), ("d", Some("b"))]);
        fact(&mut l, "f", false, &["b"]);
        let p = l.projected(false).expect("something is left out");
        let got: Vec<(&str, Option<&str>)> = p.events.iter().map(|e| (e.id.as_str(), e.after.as_deref())).collect();
        assert_eq!(got, [("a", None), ("c", None), ("d", Some("a"))]);
        assert_eq!(ids(&p.active_path()), ["a", "d"]);
        assert_eq!(p.events.iter().map(|e| e.seq).collect::<Vec<_>>(), [1, 2, 3]);
        // Nothing to leave out: no projection.
        assert!(plain_ledger(&[("a", None)]).projected(false).is_none());
    }

    /// A record with a redacted part is not intact: its other
    /// parts lose `native` (rendered from their neutral bodies) and it is
    /// struck. Of two versions of one record (§4.1), redacting the newer
    /// leaves the older version's record intact and used verbatim (pinned:
    /// SPEC §11.1, "the newest one carried by an event of the view").
    #[test]
    fn projection_strikes_part_redacted_records_and_keeps_an_older_version() {
        let n = |id: &str, part: u32, rec: Option<&str>| {
            let mut x = crate::model::Native::new("claude-code", Some(id.into()));
            x.part = part;
            if let Some(r) = rec {
                x = x.record_raw(r).unwrap();
            }
            x
        };
        let mut l = plain_ledger(&[]);
        l.append(Event::new("L", Kind::Text).native(n("L", 0, Some("{\"v\":1}")))).unwrap();
        l.append(Event::new("L#1", Kind::Text).native(n("L", 1, None))).unwrap();
        l.append(Event::new("V1", Kind::ToolCall).native(n("V", 0, Some("{\"v\":\"old\"}")))).unwrap();
        l.append(Event::new("V2", Kind::ToolResult).native(n("V", 0, Some("{\"v\":\"new\"}")))).unwrap();
        fact(&mut l, "f", false, &["L#1", "V2"]);
        let p = l.projected(false).unwrap();
        let by = |id: &str| p.events.iter().find(|e| e.id == id).unwrap();
        assert!(by("L").native.is_none(), "a record with a redacted part was kept");
        assert!(p.struck("claude-code", "L") && !p.struck("claude-code", "V"));
        assert_eq!(by("V1").native.as_ref().and_then(|n| n.record_text()), Some("{\"v\":\"old\"}"));
        assert!(!p.events.iter().any(|e| e.id == "V2" || e.id == "L#1" || e.id == "f"));
        // Every version redacted: struck.
        let mut l2 = l.clone();
        fact(&mut l2, "g", false, &["V1"]);
        assert!(l2.projected(false).unwrap().struck("claude-code", "V"));
    }
}
