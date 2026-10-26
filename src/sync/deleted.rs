//! Native records a harness deletes or rewrites on its own (SPEC §4.1, §9.3).
//!
//! Some harnesses delete and rewrite records they own: opencode drops a
//! worktree or kv row from its database, rewrites a kv or permission row in
//! place, and replaces a session's todo list whole (a todo going `pending` →
//! `completed`). Each change is a **newer version** of that record, not
//! divergence:
//!
//! - a record gone from a re-read of such a location is deleted: sync appends
//!   a tombstone ([`Event::tombstone`]: a `native` event naming the record,
//!   body `{"deleted": true}`);
//! - a record the re-read yields with content that differs from the newest
//!   version the ledger holds (rewritten in place, or written again after a
//!   tombstone) gets one `native` event carrying it as the harness now holds
//!   it; an unchanged record yields nothing.
//!
//! Exporters use the newest version of each native record, so they leave a
//! deleted record out and write a rewritten one as it now stands. The re-read
//! yields such a record under the id the ledger already holds, so the
//! divergence test (§9.3) sees no change; the newer version is a later event
//! with an id of its own (`<record id>#<n>`).
//!
//! Which records a harness deletes and rewrites is the format's to say
//! ([`deletable`]); every other record keeps its own rules: one gone from a
//! re-read is still divergence, and one rewritten in place is the format's
//! in-place hook's to version, or not. The same path serves any format whose
//! harness deletes or rewrites records: name them in [`deletable`].

use super::{Cursor, Synced};
use crate::formats::opencode;
use crate::ledger::LedgerSet;
use crate::model::{Event, Kind, Native};
use crate::store::{Published, Store};
use anyhow::Result;
use std::collections::{HashMap, HashSet};

/// Does `format`'s harness delete and rewrite the record `n` names on its
/// own (see the module docs)?
pub(super) fn deletable(format: &str, n: &Native) -> bool {
    if n.format != format {
        return false;
    }
    match format {
        opencode::FORMAT => opencode::live::deletable(n),
        _ => false,
    }
}

/// Per native record id of `format`, the newest version `events` hold: the
/// last event that carries the record whole (`part: 0` with a `record`), or
/// a tombstone. In the order the records first appear.
fn newest<'a>(format: &str, events: &'a [Event]) -> Vec<(&'a str, &'a Event)> {
    let mut at: HashMap<&str, usize> = HashMap::new();
    let mut out: Vec<(&str, &Event)> = Vec::new();
    for e in events {
        let Some(n) = e.native.as_ref().filter(|n| n.format == format && n.part == 0) else { continue };
        let Some(rid) = n.id.as_deref() else { continue };
        if n.record.is_none() && !e.is_tombstone() {
            continue;
        }
        match at.get(rid) {
            Some(&i) => out[i].1 = e,
            None => {
                at.insert(rid, out.len());
                out.push((rid, e));
            }
        }
    }
    out
}

/// The record ids of `format` that `events` hold.
fn records<'a>(format: &str, events: &'a [Event]) -> HashSet<&'a str> {
    events.iter().filter_map(|e| e.native.as_ref().filter(|n| n.format == format)).filter_map(|n| n.id.as_deref()).collect()
}

/// The ids among `missing` (events of `agent` that a re-read of the location
/// no longer yields) that are gone because the harness deleted their record:
/// the store's ledger has the event with a record [`deletable`] names, and
/// `now` (the re-read) holds no event of that record.
pub(super) fn excused(store: &Store, format: &str, agent: &str, missing: &[&str], now: &LedgerSet) -> HashSet<String> {
    let Some(l) = now.get(agent) else { return HashSet::new() };
    let Ok(held) = store.reader(agent).and_then(|r| r.ledger()) else { return HashSet::new() };
    let present = records(format, &l.events);
    let missing: HashSet<&str> = missing.iter().copied().collect();
    held.events
        .iter()
        .filter(|e| missing.contains(e.id.as_str()))
        .filter(|e| e.native.as_ref().is_some_and(|n| deletable(format, n) && n.id.as_deref().is_some_and(|rid| !present.contains(rid))))
        .map(|e| e.id.clone())
        .collect()
}

/// After a whole re-read `set` of a location in `format` was applied, for
/// every record [`deletable`] names whose newest version the ledger holds:
/// append a tombstone when the location yielded it at the last sync (`prev`,
/// the binding's cursor: an event of it is among the ids seen) and the
/// re-read no longer yields it; and a version carrying the record as the
/// re-read holds it when that differs from the newest version (a tombstone,
/// or a record with other content). A record the location never yielded is
/// not deleted from it (a projection holds one session of a store whose store
/// ledger holds more rows); with no cursor (lost with the index), nothing is
/// tombstoned.
pub(super) fn apply(store: &mut Store, format: &str, set: &LedgerSet, prev: Option<&Cursor>, out: &mut Synced) -> Result<()> {
    for l in super::parents_first(set)? {
        let id = l.id();
        if store.agent(id)?.is_none() {
            continue;
        }
        let held = store.reader(id)?.ledger()?.events;
        let present = records(format, &l.events);
        // The records the location yielded at the last sync.
        let seen: HashSet<&str> = prev.and_then(|p| p.seen.get(id)).map(|v| v.iter().map(String::as_str).collect()).unwrap_or_default();
        let was_here: HashSet<&str> = held
            .iter()
            .filter(|e| seen.contains(e.id.as_str()))
            .filter_map(|e| e.native.as_ref().filter(|n| n.format == format))
            .filter_map(|n| n.id.as_deref())
            .collect();
        // The re-read's own version of each record (its part-0 native), and its first event.
        let mut now: HashMap<&str, (&Native, &Event)> = HashMap::new();
        for e in &l.events {
            if let Some(n) = e.native.as_ref().filter(|n| n.format == format && n.part == 0 && n.record.is_some())
                && let Some(rid) = n.id.as_deref()
            {
                now.entry(rid).or_insert((n, e));
            }
        }
        let mut ids: HashSet<String> = held.iter().map(|e| e.id.clone()).chain(l.events.iter().map(|e| e.id.clone())).collect();
        let mut add: Vec<Event> = Vec::new();
        for (rid, newest) in newest(format, &held) {
            let Some(n) = newest.native.as_ref().filter(|n| deletable(format, n)) else { continue };
            let ev = match now.get(rid) {
                // Written again after a deletion, or rewritten in place: a newer version.
                Some((v, first)) if newest.is_tombstone() || v.record_json() != n.record_json() => {
                    let mut ev = Event::new(super::inplace::fresh(rid, &ids), Kind::Native);
                    ev.ts = first.ts.clone();
                    ev.turn = first.turn.clone();
                    ev.group = first.group.clone();
                    ev.native = Some((*v).clone());
                    ev
                }
                // Unchanged.
                Some(_) => continue,
                None if !newest.is_tombstone() && was_here.contains(rid) && !present.contains(rid) => Event::tombstone(super::inplace::fresh(rid, &ids), n),
                None => continue,
            };
            ids.insert(ev.id.clone());
            add.push(ev);
        }
        for mut ev in add {
            let at = store.append(id, ev.clone())?;
            ev.seq = at.seq;
            out.appended.push(Published { agent: id.to_string(), seq: at.seq, offset: at.offset, event: ev });
        }
    }
    Ok(())
}
