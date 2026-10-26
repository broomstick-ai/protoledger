//! Garbage collection (SPEC §9.5): whole ledgers removed by an explicit policy.
//!
//! An event is never changed or removed while its ledger exists; what GC
//! removes is whole ledger files, the index rows read from them (agent, links,
//! events and the bindings they record), and, only when asked, the native
//! files their projections wrote. It never writes to a ledger.
//!
//! **The policy** is explicit and its flags combine with AND:
//!
//! - `older_than`: the ledger's clock is further back than this. The
//!   clock is the `ts` of its newest event (the latest `ts` any of its events
//!   carries), which travels with the data and survives a copy of the store;
//!   a ledger with no timestamped event falls back to its header's
//!   `created_at`, then to its file's modification time. Every plan records,
//!   per ledger, which clock it read ([`Clock`]), and the dry run prints it;
//! - `larger_than`: the ledger file is larger than this many bytes;
//! - `archived`: the ledger is archived — its newest `protoledger/archived` or
//!   `protoledger/unarchived` `app` event ([`archived_event`],
//!   [`unarchived_event`]) is `protoledger/archived`.
//!
//! A policy with none of the three selects nothing and is refused: there is no
//! implicit "everything". `remove_projections` selects nothing on its own.
//!
//! **Subagents** (SPEC §9.5). Removing a ledger also removes its structural
//! children: the agents whose header `parent` names it (the harness's own
//! subagents and forks, §2.1), listed as `child of <parent>`, whatever the
//! policy says about them. §2.1 requires the parent's ledger to exist for a
//! child to name it, and GC cannot edit the child to drop the reference.
//! Never the reverse: a child the policy selects is removed on its own
//! and its parent stays. And never an agent merely linked to it: `links`
//! (`spawned-by`, `forked-from`, ...) name separate threads and never cascade.
//!
//! **Projections.** With `remove_projections`, the native file a removed
//! agent's projection binding names is deleted, unless any binding of a
//! surviving agent names the same file, or any attachment binding at all does
//! (the native session came first; GC never deletes one). A relative
//! location is never resolved and never deleted.
//!
//! **Order.** Every ledger to be removed is locked first (the store's one
//! writer per file, taken without the writer's torn-tail repair, so nothing
//! is written); a ledger another writer holds stops the GC before anything is
//! deleted. Under the locks the index is caught up and the plan re-checked: a
//! ledger that changed since it was planned (appended to, gone, or gained a
//! child the plan does not remove) is kept, and so are its ancestors in the
//! plan and the children planned only for its sake. Children are deleted
//! before their parents, so an interrupted GC can leave a parent without a
//! child but never a child without its parent. Projections are re-placed
//! against the bindings as they are under the locks.
//!
//! **Residual window.** Two writes need none of the locks GC holds: a new
//! child (`create_agent` naming a planned parent) and any new binding by a
//! surviving agent. One landing after the re-check and before the deletion
//! can leave an orphaned child or a deleted attached file; closing that needs
//! a store-wide lock, which the store does not have.

use super::{AgentInfo, BindingRow, Store, index};
use crate::model::{Event, Kind};
use anyhow::{Context, Result, anyhow, bail};
use std::collections::{BTreeMap, HashSet};
use std::fmt::Write as _;
use std::path::{Path, PathBuf};
use std::time::{Duration, SystemTime, UNIX_EPOCH};

pub const ARCHIVED: &str = "protoledger/archived";
pub const UNARCHIVED: &str = "protoledger/unarchived";

fn marker(ty: &str) -> Event {
    let now = SystemTime::now().duration_since(UNIX_EPOCH).map(|d| d.as_millis() as i64).unwrap_or(0);
    Event::new(format!("{ty}:{}", uuid::Uuid::new_v4()), Kind::App)
        .ts(Some(crate::util::ms_to_rfc3339(now)))
        .with("type", ty)
        .with("data", serde_json::json!({}))
}

/// An `app` event that marks its ledger archived (`--archived` selects it).
pub fn archived_event() -> Event {
    marker(ARCHIVED)
}

/// An `app` event that withdraws an earlier [`archived_event`].
pub fn unarchived_event() -> Event {
    marker(UNARCHIVED)
}

/// Which ledgers GC removes. Selecting flags combine with AND.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Policy {
    pub older_than: Option<Duration>,
    pub larger_than: Option<u64>,
    pub archived: bool,
    /// Also delete the native file each removed projection binding names.
    pub remove_projections: bool,
}

impl Policy {
    /// Does the policy name anything to select by?
    pub fn selects(&self) -> bool {
        self.older_than.is_some() || self.larger_than.is_some() || self.archived
    }

    /// Refuse a policy that selects by nothing: there is no implicit "everything".
    pub fn check(&self) -> Result<()> {
        if !self.selects() {
            bail!("gc needs a policy: --older-than, --larger-than and/or --archived (there is no implicit \"everything\")");
        }
        Ok(())
    }

    pub fn describe(&self) -> String {
        let mut p = Vec::new();
        if let Some(d) = self.older_than {
            p.push(format!("older-than {}s", d.as_secs()));
        }
        if let Some(n) = self.larger_than {
            p.push(format!("larger-than {n} bytes"));
        }
        if self.archived {
            p.push("archived".to_string());
        }
        p.join(" AND ")
    }
}

/// `30d`, `12h`, `90m`, `45s`, `2w`; a bare number is seconds.
pub fn parse_duration(s: &str) -> Result<Duration> {
    let s = s.trim();
    let split = s.find(|c: char| !c.is_ascii_digit()).unwrap_or(s.len());
    let (n, unit) = s.split_at(split);
    let n: u64 = n.parse().with_context(|| format!("{s:?}: expected a number followed by s, m, h, d or w"))?;
    let mult = match unit {
        "" | "s" => 1,
        "m" => 60,
        "h" => 3600,
        "d" => 86_400,
        "w" => 7 * 86_400,
        _ => bail!("{s:?}: unknown unit {unit:?} (use s, m, h, d or w)"),
    };
    Ok(Duration::from_secs(n.checked_mul(mult).ok_or_else(|| anyhow!("{s:?} is too long"))?))
}

/// Bytes: `1500`, `1500B`, `10K`/`10KiB` (1024), `10KB` (1000), and likewise M and G.
pub fn parse_size(s: &str) -> Result<u64> {
    let s = s.trim();
    let split = s.find(|c: char| !c.is_ascii_digit()).unwrap_or(s.len());
    let (n, unit) = s.split_at(split);
    let n: u64 = n.parse().with_context(|| format!("{s:?}: expected a number followed by an optional unit"))?;
    let mult: u64 = match unit {
        "" | "B" => 1,
        "K" | "KiB" => 1 << 10,
        "KB" => 1000,
        "M" | "MiB" => 1 << 20,
        "MB" => 1_000_000,
        "G" | "GiB" => 1 << 30,
        "GB" => 1_000_000_000,
        _ => bail!("{s:?}: unknown unit {unit:?} (use B, K/KiB, KB, M/MiB, MB, G/GiB or GB)"),
    };
    n.checked_mul(mult).ok_or_else(|| anyhow!("{s:?} is too large"))
}

/// Which time `older_than` measured a ledger by, most preferred first.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Clock {
    /// The latest `ts` among the ledger's events.
    NewestEvent,
    /// No event carries a `ts`: the header's `created_at`.
    CreatedAt,
    /// Neither: the ledger file's modification time.
    Mtime,
}

impl Clock {
    /// The label the plan prints.
    pub fn label(self) -> &'static str {
        match self {
            Clock::NewestEvent => "newest-event-ts",
            Clock::CreatedAt => "header-created-at",
            Clock::Mtime => "file-mtime",
        }
    }
}

/// Why an agent is removed.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Why {
    /// The policy selected it.
    Selected,
    /// Its structural parent (named) is removed.
    ChildOf(String),
}

/// One agent GC removes, with what goes with it.
#[derive(Debug, Clone, PartialEq)]
pub struct Removal {
    pub agent: AgentInfo,
    pub why: Why,
    /// Index rows read from this ledger.
    pub event_rows: u64,
    pub link_rows: u64,
    pub bindings: Vec<BindingRow>,
}

/// What GC removes (or, in a dry run, would remove).
#[derive(Debug, Clone, PartialEq, Default)]
pub struct Plan {
    pub policy: Policy,
    /// Children before their parents: the order of deletion.
    pub removals: Vec<Removal>,
    /// Native files of removed projections that are deleted (with
    /// `remove_projections`) ...
    pub projections_removed: Vec<PathBuf>,
    /// ... or left in place, each with why.
    pub projections_kept: Vec<(PathBuf, String)>,
    /// Agents the plan named that [`Store::gc_apply`] kept, each with why
    /// (the ledger changed between planning and locking, or is damaged).
    pub skipped: Vec<(String, String)>,
    /// The store's damaged ledgers ([`Store::damaged`]) when the plan was
    /// made: never removed, whatever the policy says.
    pub damaged: Vec<super::Damaged>,
    /// Agents the policy reached that the plan keeps because of a damaged
    /// ledger (a parent whose child's ledger is damaged, and the children
    /// planned only for that parent's sake), each with why.
    pub kept: Vec<(String, String)>,
    /// With `older_than`: every agent's age clock, as epoch milliseconds,
    /// selected or not, by agent id.
    pub clocks: Vec<(String, Clock, i64)>,
    /// Whether this plan was carried out.
    pub done: bool,
}

impl Plan {
    pub fn agents(&self) -> Vec<&str> {
        self.removals.iter().map(|r| r.agent.id.as_str()).collect()
    }

    /// The plan as text: one tab-separated line per item, prefixed by what it is.
    pub fn render(&self) -> String {
        let mut o = String::new();
        let verb = if self.done { "removed" } else { "would remove" };
        if self.done {
            let _ = writeln!(o, "gc: policy {}", self.policy.describe());
        } else {
            let _ = writeln!(o, "gc: dry run, nothing changed (pass --yes to act); policy {}", self.policy.describe());
        }
        for (a, clock, ms) in &self.clocks {
            let _ = writeln!(o, "clock\t{a}\t{}\t{}", clock.label(), crate::util::ms_to_rfc3339(*ms));
        }
        for r in &self.removals {
            let why = match &r.why {
                Why::Selected => "selected".to_string(),
                Why::ChildOf(p) => format!("child of {p}"),
            };
            let _ = writeln!(o, "agent\t{}\t{why}", r.agent.id);
        }
        for r in &self.removals {
            let _ = writeln!(o, "file\t{}\t{} bytes", r.agent.path, r.agent.ledger_size());
        }
        for r in &self.removals {
            let _ = writeln!(o, "index\t{}\tagents 1, links {}, events {}, bindings {}", r.agent.id, r.link_rows, r.event_rows, r.bindings.len());
        }
        for r in &self.removals {
            for b in &r.bindings {
                let state = if b.active { "active".to_string() } else { b.ended.clone().unwrap_or_else(|| "inactive".into()) };
                let _ = writeln!(o, "binding\t{}\t{}\t{} {}\t{}\t{state}", b.binding, b.agent, b.mode, b.format, b.location);
            }
        }
        for p in &self.projections_removed {
            let _ = writeln!(o, "projection\t{}\tdelete", p.display());
        }
        for (p, why) in &self.projections_kept {
            let _ = writeln!(o, "projection\t{}\tkeep ({why})", p.display());
        }
        for (a, why) in &self.skipped {
            let _ = writeln!(o, "skipped\t{a}\t{why}");
        }
        for d in &self.damaged {
            let _ = writeln!(o, "damaged\t{}\tnever removed: {}", d.ledger, d.reason);
        }
        for (a, why) in &self.kept {
            let _ = writeln!(o, "kept\t{a}\t{why}");
        }
        let events: u64 = self.removals.iter().map(|r| r.event_rows).sum();
        let bindings: usize = self.removals.iter().map(|r| r.bindings.len()).sum();
        let _ = writeln!(
            o,
            "gc: {verb} {} agents ({} bytes), {events} event rows, {bindings} bindings, {} projected files",
            self.removals.len(),
            self.removals.iter().map(|r| r.agent.ledger_size()).sum::<u64>(),
            self.projections_removed.len()
        );
        o
    }
}

impl Store {
    /// Is `agent`'s ledger archived (its newest archive marker is `archived`)?
    pub fn archived(&self, agent: &str) -> Result<bool> {
        let mut state = false;
        for row in index::events(&self.db, agent, 0, usize::MAX)?.into_iter().filter(|r| r.kind == Kind::App.as_str()) {
            let e = self.event_at(agent, row.seq)?.ok_or_else(|| anyhow!("{agent} seq {} vanished", row.seq))?;
            match e.str("type") {
                Some(ARCHIVED) => state = true,
                Some(UNARCHIVED) => state = false,
                _ => {}
            }
        }
        Ok(state)
    }

    /// The time `older_than` ages `a` by, and which clock that is:
    /// the latest parseable `ts` among its events, else its header's
    /// `created_at`, else its file's modification time. Epoch milliseconds.
    pub fn age_clock(&self, a: &AgentInfo) -> Result<(Clock, i64)> {
        let newest = index::events(&self.db, &a.id, 0, usize::MAX)?.iter().filter_map(|r| r.ts.as_deref().and_then(crate::util::parse_rfc3339_ms)).max();
        if let Some(ms) = newest {
            return Ok((Clock::NewestEvent, ms));
        }
        if let Some(ms) = a.created_at.as_deref().and_then(crate::util::parse_rfc3339_ms) {
            return Ok((Clock::CreatedAt, ms));
        }
        Ok((Clock::Mtime, a.newest_mtime_ns().max(0) / 1_000_000))
    }

    fn matches(&self, a: &AgentInfo, p: &Policy, now: SystemTime, clocks: &mut Vec<(String, Clock, i64)>) -> Result<bool> {
        if let Some(d) = p.older_than {
            let (clock, ms) = self.age_clock(a)?;
            clocks.push((a.id.clone(), clock, ms));
            let at = if ms >= 0 { UNIX_EPOCH + Duration::from_millis(ms as u64) } else { UNIX_EPOCH };
            let age = now.duration_since(at).unwrap_or(Duration::ZERO);
            if age <= d {
                return Ok(false);
            }
        }
        if let Some(n) = p.larger_than
            && a.ledger_size() <= n
        {
            return Ok(false);
        }
        if p.archived && !self.archived(&a.id)? {
            return Ok(false);
        }
        Ok(true)
    }

    /// A dry run of `policy` on the store at `root`, as of `now`, that writes
    /// nothing at all, not even to `index.db` (a plain [`Store::open`] writes
    /// to it). The index is copied into memory from a read-only view of
    /// `index.db` (or, if that cannot be read, rebuilt in memory from the
    /// ledgers) and caught up there exactly as [`Store::open`] would catch it
    /// up, so the plan is the one [`Store::gc`] would carry out.
    pub fn gc_dry_run(root: impl AsRef<Path>, policy: &Policy, now: SystemTime) -> Result<Plan> {
        Store::in_memory(root.as_ref())?.gc_plan(policy, now)
    }

    /// What `policy` removes, as of `now`. Changes nothing.
    pub fn gc_plan(&self, policy: &Policy, now: SystemTime) -> Result<Plan> {
        policy.check()?;
        let agents = self.agents()?;
        let mut why: BTreeMap<String, Why> = BTreeMap::new();
        let mut clocks = Vec::new();
        // A damaged ledger is never removed (its index rows may predate the
        // damage): it is not selected, never goes with its parent, and keeps
        // its ancestors (SPEC §2.1: a child names an existing parent).
        let damaged = |id: &str| self.damaged_agent(id);
        for a in &agents {
            if damaged(&a.id).is_none() && self.matches(a, policy, now, &mut clocks)? {
                why.insert(a.id.clone(), Why::Selected);
            }
        }
        // Close downwards: a child goes with its parent (SPEC §2.1).
        let mut kept: BTreeMap<String, String> = BTreeMap::new();
        let mut queue: Vec<String> = why.keys().cloned().collect();
        while let Some(id) = queue.pop() {
            for c in self.children(&id)? {
                if damaged(&c.id).is_some() {
                    kept.insert(id.clone(), format!("parent of damaged {}", c.id));
                } else if !why.contains_key(&c.id) {
                    why.insert(c.id.clone(), Why::ChildOf(id.clone()));
                    queue.push(c.id);
                }
            }
        }
        // A damaged ledger the index has no row for names its parent, if at
        // all, in its header: read it, when it can be read.
        let rows: HashSet<&str> = agents.iter().map(|a| a.path.as_str()).collect();
        for d in self.damaged() {
            if rows.contains(d.ledger.as_str()) {
                continue;
            }
            let parent = self.reader_of(&d.ledger).header().ok().flatten().and_then(|h| h.agent.parent).map(|p| p.agent);
            if let Some(p) = parent.filter(|p| why.contains_key(p)) {
                kept.insert(p, format!("parent of damaged {}", d.agent.as_deref().unwrap_or(&d.ledger)));
            }
        }
        // A kept agent keeps its planned parent, and the children planned
        // only for its sake stay with it.
        loop {
            let mut grew = false;
            for (id, w) in &why {
                if kept.contains_key(id) {
                    continue;
                }
                let parent_of_kept =
                    why.keys().find(|c| kept.contains_key(*c) && agents.iter().any(|a| &a.id == *c && a.parent.as_ref().is_some_and(|p| &p.agent == id)));
                let reason = match (parent_of_kept, w) {
                    (Some(c), _) => format!("parent of kept {c}"),
                    (None, Why::ChildOf(p)) if kept.contains_key(p) => format!("child of kept {p}"),
                    _ => continue,
                };
                kept.insert(id.clone(), reason);
                grew = true;
                break;
            }
            if !grew {
                break;
            }
        }
        why.retain(|id, _| !kept.contains_key(id));
        // Deletion order: every child before its parent (deepest first).
        let by_id: BTreeMap<&str, &AgentInfo> = agents.iter().map(|a| (a.id.as_str(), a)).collect();
        let depth = |id: &str| {
            let mut d = 0;
            let mut seen = HashSet::new();
            let mut cur = by_id.get(id).and_then(|a| a.parent.as_ref()).map(|p| p.agent.clone());
            while let Some(p) = cur {
                if !seen.insert(p.clone()) {
                    break;
                }
                d += 1;
                cur = by_id.get(p.as_str()).and_then(|a| a.parent.as_ref()).map(|p| p.agent.clone());
            }
            d
        };
        let mut ids: Vec<&String> = why.keys().collect();
        ids.sort_by(|a, b| depth(b).cmp(&depth(a)).then(a.cmp(b)));
        let all_bindings = self.bindings()?;
        let mut plan = Plan { policy: policy.clone(), clocks, damaged: self.damaged(), kept: kept.into_iter().collect(), ..Default::default() };
        for id in ids {
            let agent = (*by_id.get(id.as_str()).ok_or_else(|| anyhow!("agent {id} left the index"))?).clone();
            let bindings: Vec<BindingRow> = all_bindings.iter().filter(|b| &b.agent == id).cloned().collect();
            plan.removals.push(Removal {
                event_rows: index::events(&self.db, id, 0, usize::MAX)?.len() as u64,
                link_rows: agent.links.len() as u64,
                agent,
                why: why[id].clone(),
                bindings,
            });
        }
        plan.place_projections(&all_bindings);
        Ok(plan)
    }

    /// Carry out `policy` as of `now`: [`Store::gc_plan`], then
    /// [`Store::gc_apply`]. Never writes to a ledger.
    pub fn gc(&mut self, policy: &Policy, now: SystemTime) -> Result<Plan> {
        let plan = self.gc_plan(policy, now)?;
        self.gc_apply(plan)
    }

    /// Carry out a plan [`Store::gc_plan`] made: lock every planned ledger,
    /// catch the index up, keep (in [`Plan::skipped`]) whatever changed since
    /// the plan was made, then delete the rest (children first), their index
    /// rows and bindings, and, with `remove_projections`, their projections'
    /// native files. Never writes to a ledger.
    pub fn gc_apply(&mut self, mut plan: Plan) -> Result<Plan> {
        if plan.done {
            bail!("gc: this plan was already carried out");
        }
        plan.policy.check()?;
        // Take every ledger's lock before deleting anything. The lock is the
        // one an Appender takes, but taken on a read-only handle: nothing is
        // written, not even the writer's torn-tail repair.
        let mut locks = Vec::new();
        for r in &plan.removals {
            if self.writers.contains_key(&r.agent.id) {
                continue; // this store already holds it
            }
            // Every file a writer locks: the plain file, or the hot tail of
            // a compressed ledger (SPEC §10.11). A ledger already gone is
            // re-checked below.
            let files = self.files(&r.agent.path);
            let held = super::writer::lock_for_removal(&files).map_err(|e| anyhow!("gc: {} is in use; nothing was removed ({e})", r.agent.id))?;
            locks.extend(held);
        }
        // Under the locks, what do the ledgers hold now?
        self.refresh().context("gc: catching the index up under the locks")?;
        let planned: HashSet<String> = plan.removals.iter().map(|r| r.agent.id.clone()).collect();
        let mut kept: BTreeMap<String, String> = BTreeMap::new();
        for r in &plan.removals {
            let id = &r.agent.id;
            if let Some(d) = self.damaged_agent(id) {
                kept.insert(id.clone(), format!("damaged, never removed: {}", d.reason));
                continue;
            }
            match self.agent(id)? {
                None => {
                    kept.insert(id.clone(), "gone before gc locked it".into());
                }
                Some(now) if now != r.agent => {
                    kept.insert(id.clone(), "changed after the plan was made".into());
                }
                Some(_) => {
                    if let Some(c) = self.children(id)?.into_iter().find(|c| !planned.contains(&c.id)) {
                        kept.insert(id.clone(), format!("gained child {} after the plan was made", c.id));
                    }
                }
            }
        }
        // A kept child keeps its parent; a child planned only for a kept
        // parent's sake stays with it.
        loop {
            let mut grew = false;
            for r in &plan.removals {
                let id = &r.agent.id;
                if kept.contains_key(id) {
                    continue;
                }
                let kept_child = plan.removals.iter().find(|c| kept.contains_key(&c.agent.id) && c.agent.parent.as_ref().is_some_and(|p| &p.agent == id));
                let why = match (kept_child, &r.why) {
                    (Some(c), _) => format!("parent of kept {}", c.agent.id),
                    (None, Why::ChildOf(p)) if kept.contains_key(p) => format!("child of kept {p}"),
                    _ => continue,
                };
                kept.insert(id.clone(), why);
                grew = true;
            }
            if !grew {
                break;
            }
        }
        if !kept.is_empty() {
            plan.removals.retain(|r| !kept.contains_key(&r.agent.id));
            plan.skipped = kept.into_iter().collect();
        }
        plan.damaged = self.damaged();
        // Always, not only when something was kept: a surviving agent may
        // have bound a planned file since the plan was made.
        plan.place_projections(&self.bindings()?);
        for r in &plan.removals {
            let files = self.files(&r.agent.path);
            super::writer::remove(&files).with_context(|| format!("removing {}", files.plain.display()))?;
            self.writers.remove(&r.agent.id);
            let tx = self.db.unchecked_transaction()?;
            index::forget_agent(&tx, &r.agent.id)?;
            tx.commit()?;
        }
        for p in &plan.projections_removed {
            std::fs::remove_file(p).with_context(|| format!("removing projection {}", p.display()))?;
        }
        drop(locks);
        plan.done = true;
        Ok(plan)
    }
}

impl Plan {
    /// Decide, for every projection binding of a removed agent, whether its
    /// native file is deleted or kept, given every binding in the store.
    fn place_projections(&mut self, all: &[BindingRow]) {
        self.projections_removed.clear();
        self.projections_kept.clear();
        let removed: HashSet<&str> = self.removals.iter().map(|r| r.agent.id.as_str()).collect();
        // Locations GC must not delete: named by a surviving agent's binding,
        // or by any attachment binding at all.
        let guarded: Vec<(PathBuf, &BindingRow)> =
            all.iter().filter(|b| !removed.contains(b.agent.as_str()) || b.mode != "projection").map(|b| (normalize(&b.location), b)).collect();
        let mut seen = HashSet::new();
        for b in self.removals.iter().flat_map(|r| &r.bindings).filter(|b| b.mode == "projection") {
            let loc = Path::new(&b.location);
            if !loc.is_absolute() {
                if seen.insert(loc.to_path_buf()) {
                    self.projections_kept.push((loc.to_path_buf(), "relative location, never resolved or deleted".into()));
                }
                continue;
            }
            if !loc.is_file() {
                continue;
            }
            let n = normalize(&b.location);
            if !seen.insert(n.clone()) {
                continue;
            }
            if let Some((_, g)) = guarded.iter().find(|(p, _)| *p == n) {
                self.projections_kept.push((loc.to_path_buf(), format!("named by binding {} of {}", g.binding, g.agent)));
            } else if !self.policy.remove_projections {
                self.projections_kept.push((loc.to_path_buf(), "pass --remove-projections to delete it".into()));
            } else {
                self.projections_removed.push(loc.to_path_buf());
            }
        }
    }
}

/// A binding location as a path to compare: canonical when it is absolute
/// and exists, else as written (a relative one is never resolved).
fn normalize(location: &str) -> PathBuf {
    let p = PathBuf::from(location);
    if !p.is_absolute() {
        return p;
    }
    std::fs::canonicalize(&p).unwrap_or(p)
}

/// Copy every row of the on-disk index at `path` into `db` (an empty index of
/// the same schema), reading the file read-only and as immutable, so nothing
/// is written beside it (no `-wal`, no `-shm`). A newer write still in another
/// process's WAL is not seen; catch-up reads it from the ledgers instead.
pub(crate) fn copy_index(db: &rusqlite::Connection, path: &Path) -> Result<()> {
    if !path.is_file() {
        bail!("{} does not exist", path.display());
    }
    let abs = std::path::absolute(path)?;
    let mut uri = String::from("file:");
    for c in abs.to_string_lossy().chars() {
        match c {
            '%' => uri.push_str("%25"),
            '?' => uri.push_str("%3f"),
            '#' => uri.push_str("%23"),
            c => uri.push(c),
        }
    }
    uri.push_str("?mode=ro&immutable=1");
    db.execute("ATTACH DATABASE ?1 AS disk", rusqlite::params![uri])?;
    let copied = (|| -> Result<()> {
        let v: String = db.query_row("SELECT value FROM disk.meta WHERE key = 'schema'", [], |r| r.get(0))?;
        if v != super::SCHEMA_VERSION.to_string() {
            bail!("index schema {v}");
        }
        for t in ["agents", "links", "events", "bindings", "blocks"] {
            db.execute_batch(&format!("INSERT INTO main.{t} SELECT * FROM disk.{t};"))?;
        }
        Ok(())
    })();
    db.execute_batch("DETACH DATABASE disk;")?;
    copied
}
