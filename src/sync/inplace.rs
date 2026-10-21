//! Sync for a harness that rewrites its records in place (SPEC §4.1): opencode.
//!
//! The generic engine ([`super::apply_ids`]) appends the events whose ids the
//! ledger lacks. For opencode that is wrong twice over: a record rewritten
//! under the same id (a tool part going `running` → `completed`, a 2.x step
//! growing) yields events with ids the ledger already holds and new content,
//! and a whole re-import of a live store would append transient states.
//! This hook, instead:
//!
//! - drops every event that is a transient state ([`live::facts`]): a tool
//!   call is appended when the tool runs, its result when it completed or
//!   failed, text and reasoning when they ended;
//! - appends each settled event whose **fact** the ledger does not hold yet
//!   (a fact is identified by its record and what it states, not by its id,
//!   since a 2.x message's event ids are positions that move); an event whose
//!   id the ledger already holds for another fact gets the next free
//!   `<record id>#<n>`;
//! - carries a **newer native record version** (SPEC §4.1): an appended event
//!   whose record differs from the newest version the ledger holds carries
//!   the new record, with `part: 0`; a settled record rewritten in place with
//!   no new fact gets a `native` event that carries its new version
//!   ([`live::versioned`]). An unchanged record yields nothing.

use super::Synced;
use crate::formats::opencode::{FORMAT, live};
use crate::ledger::LedgerSet;
use crate::model::{Event, Kind, Native};
use crate::store::{Published, Store};
use anyhow::Result;
use std::collections::{HashMap, HashSet};

/// Does `format` go through this hook?
pub(super) fn applies(format: &str) -> bool {
    format == FORMAT
}

/// The next `<base>#<n>` that names nothing in `taken`.
fn fresh(base: &str, taken: &HashSet<String>) -> String {
    (1..).map(|n| format!("{base}#{n}")).find(|id| !taken.contains(id)).expect("an unused id")
}

/// The records the newest version of which `events` carry, by record id.
fn newest(events: &[Event]) -> HashMap<String, serde_json::Value> {
    let mut out = HashMap::new();
    for e in events {
        if let (Some(rid), Some(rec)) = (live::record_id(e), e.native.as_ref().filter(|n| n.part == 0).and_then(Native::record_json)) {
            out.insert(rid.to_string(), rec);
        }
    }
    out
}

pub(super) fn apply(store: &mut Store, set: &LedgerSet, out: &mut Synced) -> Result<()> {
    for l in super::parents_first(set)? {
        let id = l.id();
        let held: Vec<Event> = if store.agent(id)?.is_some() {
            store.reader(id)?.ledger()?.events
        } else {
            store.create_agent(l.agent().clone())?;
            out.created.push(id.to_string());
            Vec::new()
        };
        let mut keys: HashSet<String> = live::facts(&held).into_iter().map(|f| f.key).collect();
        let mut ids: HashSet<String> = held.iter().map(|e| e.id.clone()).chain(l.events.iter().map(|e| e.id.clone())).collect();
        let mut carried = newest(&held);
        // The import's own version of each record (its part-0 native).
        let mut version: HashMap<&str, &Native> = HashMap::new();
        let mut order: Vec<&str> = Vec::new();
        for e in &l.events {
            if let (Some(rid), Some(n)) = (live::record_id(e), e.native.as_ref().filter(|n| n.part == 0 && n.record.is_some()))
                && version.insert(rid, n).is_none()
            {
                order.push(rid);
            }
        }
        // Ids the ledger holds (and ids appended in this pass).
        let mut used: HashSet<String> = held.iter().map(|e| e.id.clone()).collect();
        let mut append = |store: &mut Store, mut ev: Event, ids: &mut HashSet<String>, used: &mut HashSet<String>| -> Result<()> {
            ids.insert(ev.id.clone());
            used.insert(ev.id.clone());
            let at = store.append(id, ev.clone())?;
            ev.seq = at.seq;
            out.appended.push(Published { agent: id.to_string(), seq: at.seq, offset: at.offset, event: ev });
            Ok(())
        };
        for (e, f) in l.events.iter().zip(live::facts(&l.events)) {
            if f.transient || keys.contains(&f.key) {
                continue;
            }
            let mut ev = e.clone();
            let rid = live::record_id(e).map(str::to_string);
            if used.contains(&ev.id) {
                ev.id = fresh(rid.as_deref().unwrap_or(&e.id), &ids);
            }
            if let Some(rid) = &rid
                && let Some(n) = version.get(rid.as_str())
            {
                let rec = n.record_json();
                if carried.get(rid) != rec.as_ref() {
                    // The record changed since the ledger last carried it (or
                    // never did): this event carries the new version.
                    ev.native = Some((*n).clone());
                    if let Some(r) = rec {
                        carried.insert(rid.clone(), r);
                    }
                }
            }
            keys.insert(f.key);
            append(store, ev, &mut ids, &mut used)?;
        }
        // A settled record rewritten in place with no new fact: its newer version
        // rides on a `native` event of its own.
        for rid in order {
            let n = version[rid];
            let Some(have) = carried.get(rid) else { continue };
            if Some(have) == n.record_json().as_ref() || !live::versioned(n) {
                continue;
            }
            let first = l.events.iter().find(|e| live::record_id(e) == Some(rid)).expect("the record's event");
            let mut ev = Event::new(fresh(rid, &ids), Kind::Native);
            ev.ts = first.ts.clone();
            ev.turn = first.turn.clone();
            ev.group = first.group.clone();
            ev.native = Some(n.clone());
            if let Some(r) = n.record_json() {
                carried.insert(rid.to_string(), r);
            }
            append(store, ev, &mut ids, &mut used)?;
        }
    }
    Ok(())
}
