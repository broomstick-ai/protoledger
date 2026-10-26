//! A live opencode store, read again and again by sync (SPEC §4.1, §9.3).
//!
//! opencode rewrites its records in place while they progress: a 1.x part
//! row or file, and a 2.x `session_message` row, change under the same id as
//! a tool goes `pending`/`streaming` → `running` → `completed`, as text
//! streams in, and as a step finishes. A whole re-import of such a record
//! yields the same event ids with new content, and a 2.x message's event ids
//! are positions (`<message id>#<n>`) that can move as content is added. So
//! sync's opencode hook (`src/sync/inplace.rs`) does not compare ids; it asks
//! this module two things about each event:
//!
//! - [`facts`]: is the event a **settled** fact, and what is its identity
//!   (the [`Fact::key`])? Transient states (a tool's `pending` or `streaming`
//!   input, text and reasoning still growing, a step's usage before the
//!   response ended) are never appended.
//! - [`versioned`]: may a newer version of this record be appended on its
//!   own, when it states no new fact (a settled part or message rewritten in
//!   place)? Sessions and container bookkeeping are not: opencode rewrites the
//!   session row on every message (`time_updated`), and its newer versions
//!   ride only on new session facts (a title).
//!
//! Sync's deleted-records path (`src/sync/deleted.rs`) asks one more,
//! [`deletable`]: is the record a row opencode deletes and rewrites on its
//! own (a todo, a store row)? Its deletion and each rewrite are newer
//! versions of it.

use super::{Container, FORMAT, Gen, Role, get_ms, render, s};
use crate::model::{Event, Kind, Native};
use serde_json::Value;
use std::collections::HashMap;

/// One event, as sync's opencode hook sees it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Fact {
    /// What the event states, independent of its id: equal keys are the same fact.
    pub key: String,
    /// A transient state (SPEC §4.1): not appended.
    pub transient: bool,
}

/// The opencode record id an event came from, if it came from one.
pub fn record_id(e: &Event) -> Option<&str> {
    let n = e.native.as_ref()?;
    if n.format != FORMAT {
        return None;
    }
    n.id.as_deref()
}

/// A native record's container, role and logical value.
fn record(n: &Native) -> Option<(Container, Role, Value)> {
    let c = n.variant.as_deref().and_then(Container::from_variant)?;
    let rec = n.record_json()?;
    let role = render::role_of(c, n.id.as_deref().unwrap_or(""), n.file.as_deref().unwrap_or(""), &rec);
    let value = render::logical(c, role, &rec);
    Some((c, role, value))
}

/// May a newer version of this record be appended when it states no new fact?
/// Only a settled message or part (see the module docs).
pub fn versioned(n: &Native) -> bool {
    let Some((c, role, v)) = record(n) else { return false };
    match (c.generation(), role) {
        (Gen::V1, Role::Part) => part_settled(&v),
        (Gen::V1, Role::Message) => s(&v, "role") != Some("assistant") || get_ms(&v, &["time", "completed"]).is_some(),
        (Gen::V2, Role::Message) => match s(&v, "type") {
            Some("assistant") => get_ms(&v, &["time", "completed"]).is_some(),
            Some("shell") | Some("compaction") => s(&v, "status") != Some("running"),
            _ => true,
        },
        _ => false,
    }
}

/// Does opencode delete and rewrite this record on its own, so that its
/// absence from a re-read is a newer version of it (a tombstone, SPEC §4.1)
/// and not divergence, and a re-read of it with other content is a newer
/// version too? A row of an `opencode.db` that is not a session, message or
/// part: a store row (a worktree, a kv, permission, project or account row,
/// ...) or a row kept beside a session (a todo, which opencode replaces whole
/// when the list changes, a share, ...). Sessions, messages and parts are
/// not: a message gone from a session is a rewrite of the conversation
/// (§9.3), and a message or part rewritten in place is versioned only as
/// [`versioned`] says, by sync's in-place hook.
pub fn deletable(n: &Native) -> bool {
    let Some(c) = n.variant.as_deref().and_then(Container::from_variant) else { return false };
    matches!(c, Container::SqliteV1 | Container::SqliteV2)
        && render::role_of(c, n.id.as_deref().unwrap_or(""), n.file.as_deref().unwrap_or(""), &Value::Null) == Role::Extra
}

/// Has a 1.x text or reasoning part ended (a part with no `time` never streams)?
fn ended(p: &Value) -> bool {
    p.get("time").is_none_or(|t| !t.is_object() || t.get("end").is_some_and(|e| !e.is_null()))
}

fn part_settled(p: &Value) -> bool {
    match s(p, "type") {
        Some("tool") => matches!(p.get("state").and_then(|st| s(st, "status")), Some("completed") | Some("error")),
        Some("text") | Some("reasoning") => ended(p),
        _ => true,
    }
}

/// The identity of the fact `e` states, given its record id and its ordinal
/// among the events of that record and kind.
fn key(e: &Event, rid: &str, ordinal: usize) -> String {
    let kind = format!("{:?}", e.kind);
    let call = e.str("call_id").filter(|c| !c.is_empty());
    match e.kind {
        Kind::ToolCall | Kind::ToolResult | Kind::Subagent if call.is_some() => format!("{rid}\u{1f}{kind}\u{1f}call\u{1f}{}", call.unwrap()),
        // A session's title or settings that change are new facts.
        Kind::Title | Kind::Settings => format!("{rid}\u{1f}{kind}\u{1f}{}", serde_json::to_string(&e.body).unwrap_or_default()),
        Kind::Compaction => format!("{rid}\u{1f}{kind}\u{1f}{}\u{1f}{ordinal}", e.str("phase").unwrap_or("")),
        Kind::FileChange => format!("{rid}\u{1f}{kind}\u{1f}{}\u{1f}{ordinal}", e.body.get("path").map(Value::to_string).unwrap_or_default()),
        _ => format!("{rid}\u{1f}{kind}\u{1f}{ordinal}"),
    }
}

/// [`Fact`] of every event of `events` (one ledger, in order). An event that
/// did not come from an opencode record is keyed by its id and never
/// transient. Each record's newest version in `events` decides what is settled.
pub fn facts(events: &[Event]) -> Vec<Fact> {
    // record id → its newest version's (container, role, logical value).
    let mut recs: HashMap<&str, (Container, Role, Value)> = HashMap::new();
    // (record id, kind) → how many events of it.
    let mut count: HashMap<(&str, Kind), usize> = HashMap::new();
    for e in events {
        let Some(rid) = record_id(e) else { continue };
        *count.entry((rid, e.kind)).or_default() += 1;
        if let Some(r) = e.native.as_ref().filter(|n| n.part == 0).and_then(record) {
            recs.insert(rid, r);
        }
    }
    let mut seen: HashMap<(&str, Kind), usize> = HashMap::new();
    events
        .iter()
        .map(|e| {
            let Some(rid) = record_id(e) else {
                return Fact { key: format!("id\u{1f}{}", e.id), transient: false };
            };
            let n = seen.entry((rid, e.kind)).or_default();
            let ordinal = *n;
            *n += 1;
            let last = ordinal + 1 == count[&(rid, e.kind)];
            let transient = recs.get(rid).is_some_and(|(c, role, v)| transient(e, last, c.generation(), *role, v));
            Fact { key: key(e, rid, ordinal), transient }
        })
        .collect()
}

/// Is `e` (the last event of its kind from its record when `last`) a
/// transient state of the record `v`?
fn transient(e: &Event, last: bool, generation: Gen, role: Role, v: &Value) -> bool {
    match (generation, role) {
        (Gen::V1, Role::Part) => match s(v, "type") {
            // A tool's call is a fact once it runs (its input is then final); its
            // result once it completed or failed (the importer makes none before).
            Some("tool") => !matches!(v.get("state").and_then(|st| s(st, "status")), Some("running") | Some("completed") | Some("error")),
            Some("text") | Some("reasoning") => !ended(v),
            _ => false,
        },
        // A 1.x assistant message's usage and error are settled when the step completed.
        (Gen::V1, Role::Message) => s(v, "role") == Some("assistant") && get_ms(v, &["time", "completed"]).is_none(),
        (Gen::V2, Role::Message) => match s(v, "type") {
            Some("assistant") => assistant_transient(e, last, v),
            Some("compaction") => s(v, "status") == Some("running"),
            _ => false,
        },
        _ => false,
    }
}

/// A 2.x assistant step's event that has not settled yet.
fn assistant_transient(e: &Event, last: bool, m: &Value) -> bool {
    let streamed = get_ms(m, &["time", "streamed"]).is_some() || get_ms(m, &["time", "completed"]).is_some();
    let completed = get_ms(m, &["time", "completed"]).is_some();
    let content: &[Value] = m.get("content").and_then(Value::as_array).map(Vec::as_slice).unwrap_or(&[]);
    let last_type = content.last().and_then(|c| s(c, "type"));
    match e.kind {
        Kind::ToolCall | Kind::Subagent => {
            let call = e.str("call_id").unwrap_or("");
            let tool = content.iter().find(|c| s(c, "type") == Some("tool") && s(c, "id") == Some(call));
            // `streaming` (2.x) and `pending` (the betas): the input is still arriving.
            tool.is_some_and(|t| !matches!(t.get("state").and_then(|st| s(st, "status")), Some("running") | Some("completed") | Some("error")))
        }
        Kind::ToolResult => false,
        // A text item has no end of its own: it has ended when something follows it
        // or the response finished streaming.
        Kind::Text => !streamed && last && last_type == Some("text"),
        Kind::Reasoning => {
            let open = content.iter().rev().find(|c| s(c, "type") == Some("reasoning")).is_some_and(|r| get_ms(r, &["time", "completed"]).is_none());
            !streamed && last && (last_type == Some("reasoning") || open)
        }
        Kind::Usage => !streamed,
        _ => !completed,
    }
}
