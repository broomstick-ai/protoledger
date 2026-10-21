//! Export: turning ledgers into opencode sessions.
//!
//! Each ledger becomes one session. Events that came from opencode records of
//! the target's generation are emitted from those records (unchanged when the
//! container also matches). Every other event is rendered from its neutral
//! fields into opencode messages, and — with carry on — stowed in a carrier on
//! the message it produced (`metadata.protoledger` in V2, `protoledger` on the
//! message info in V1), so a re-import restores it exactly.

use super::ids::{self, Ascending};
use super::store;
use super::{Container, FORMAT, Gen, HEADER_KEY, js_normalize, js_num, ordered, s};
use crate::carry;
use crate::formats::{ExportOptions, Losses};
use crate::ledger::{Ledger, LedgerSet};
use crate::model::{Event, Kind, Obj, SPEC_VERSION};
use crate::util::OrderedMap;
use anyhow::Result;
use serde_json::{Value, json};
use std::collections::HashMap;

#[derive(Debug, Clone)]
pub struct NativeRec {
    pub container: Container,
    pub record: Value,
    /// The record's text exactly as the ledger holds it (SPEC §4).
    pub text: Option<String>,
    pub raw: Option<String>,
    pub file: String,
}

/// A record to write: its logical value, and the native record when it has one.
#[derive(Debug, Clone)]
pub struct OutItem {
    pub value: Value,
    pub native: Option<NativeRec>,
}

#[derive(Debug, Clone)]
pub struct OutMsg {
    pub info: OutItem,
    pub parts: Vec<OutItem>,
    pub seq: Option<i64>,
}

#[derive(Debug, Clone)]
pub struct OutSession {
    pub id: String,
    /// The container the ledger was imported from, when it was opencode.
    pub src: Option<Container>,
    /// That container's facts (file name, storage prefix, schema).
    pub extra: Obj,
    pub info: OutItem,
    pub messages: Vec<OutMsg>,
    pub extras: Vec<OutItem>,
    pub slug: String,
    pub version: String,
    /// A store ledger's rows ([`super::StoreSrc`]): no session, only
    /// `extras`, written to an `opencode.db` and nowhere else.
    pub store: bool,
}

const DEFAULT_V2_VERSION: &str = "2.0.31";
const DEFAULT_V1_VERSION: &str = "1.2.21";

// ---------------------------------------------------------------- roles

fn strip_storage_prefix(file: &str) -> &str {
    for p in ["opencode/storage/", "storage/"] {
        if let Some(r) = file.strip_prefix(p) {
            return r;
        }
    }
    file
}

pub(crate) fn role_of(c: Container, id: &str, file: &str, record: &Value) -> super::Role {
    use super::Role::*;
    // Extras (project files and rows, todo, event_sequence, ...) carry a location anchor.
    if id.starts_with('@') && !matches!(c, Container::Export | Container::Transfer) {
        return Extra;
    }
    match c {
        Container::SqliteV1 | Container::SqliteV2 => match file.rsplit_once(':').map(|(_, t)| t).unwrap_or(file) {
            "session" | "session_v2" => Session,
            "message" | "session_message" => Message,
            "part" => Part,
            _ => Extra,
        },
        Container::Storage => match strip_storage_prefix(file).split('/').next() {
            Some("session") => Session,
            Some("message") => Message,
            Some("part") => Part,
            _ => Extra,
        },
        Container::Export => {
            if record.get("role").is_some() {
                Message
            } else if record.get("messageID").is_some() || (record.get("type").is_some() && record.get("slug").is_none()) {
                Part
            } else {
                Session
            }
        }
        Container::Transfer => {
            if record.get("type").is_some() {
                Message
            } else {
                Session
            }
        }
    }
}

pub(crate) fn logical(c: Container, role: super::Role, record: &Value) -> Value {
    use super::Role::*;
    match (c, role) {
        (Container::SqliteV1, Session) => store::v1_session_from_row(record),
        (Container::SqliteV1, Message) => store::v1_message_from_row(record),
        (Container::SqliteV1, Part) => store::v1_part_from_row(record),
        (Container::SqliteV2, Session) => store::v2_session_from_row(record),
        (Container::SqliteV2, Message) => store::v2_message_from_row(record),
        _ => record.clone(),
    }
}

/// A native record's `raw` while it still matches the record
/// ([`crate::util::current_raw`]). A database row's `raw` is its `data`
/// column's text, so it is matched against the row's `data`.
fn current_raw(c: Container, n: &crate::model::Native) -> Option<String> {
    match c {
        Container::SqliteV1 | Container::SqliteV2 => {
            let raw = n.raw.as_deref()?;
            let data = n.record_text().map(crate::util::minify).and_then(|t| crate::formats::atif::jsonfmt::member(&t, "data").map(str::to_string));
            data.is_some_and(|d| crate::util::raw_matches(raw, &d)).then(|| raw.to_string())
        }
        _ => crate::util::current_raw(n).map(str::to_string),
    }
}

// ---------------------------------------------------------------- context

struct Ctx<'a> {
    set: &'a LedgerSet,
    target: Container,
    carry: bool,
    version: String,
    /// agent id → session id
    sessions: HashMap<String, String>,
    /// (parent agent, event id) → child session id
    children: HashMap<(String, String), String>,
}

/// The opencode session id a ledger exports as.
fn session_id_for(l: &Ledger, target: Container) -> String {
    if let Some(src) = &l.agent().source {
        if src.format == FORMAT
            && let Some(n) = &src.native_id
        {
            // A session rendered for the other generation needs an id opencode accepts.
            let same_gen = src.variant.as_deref().and_then(Container::from_variant).is_some_and(|c| c.generation() == target.generation());
            if same_gen || n.starts_with("ses") {
                return n.clone();
            }
        }
        if let Some(n) = src.extra.get("opencode").and_then(|o| o.get("session")).and_then(Value::as_str) {
            return n.to_string();
        }
    }
    if ids::is_canonical(l.id(), "ses") {
        return l.id().to_string();
    }
    let created = l
        .agent()
        .created_at
        .as_deref()
        .and_then(crate::util::parse_rfc3339_ms)
        .or_else(|| l.events.iter().find_map(|e| e.ts.as_deref().and_then(crate::util::parse_rfc3339_ms)))
        .unwrap_or(0);
    ids::session(created, l.id())
}

pub fn build(set: &LedgerSet, target: Container, opts: &ExportOptions, losses: &mut Losses) -> Result<Vec<OutSession>> {
    let version =
        opts.target_version.clone().unwrap_or_else(|| if target.generation() == Gen::V2 { DEFAULT_V2_VERSION.into() } else { DEFAULT_V1_VERSION.into() });
    let mut ctx = Ctx { set, target, carry: opts.carry, version, sessions: HashMap::new(), children: HashMap::new() };
    for l in set.iter() {
        ctx.sessions.insert(l.id().to_string(), session_id_for(l, target));
    }
    for l in set.iter() {
        if let Some(p) = &l.agent().parent
            && let Some(ev) = &p.event
        {
            ctx.children.insert((p.agent.clone(), ev.clone()), ctx.sessions[l.id()].clone());
        }
    }
    let mut out = Vec::new();
    // (agent, event id) → (session id, message id), for fork boundaries.
    let mut placed: HashMap<(String, String), String> = HashMap::new();
    for l in ordered(set) {
        if super::is_store_ledger(l) {
            out.push(store_rows(l, target, losses));
            continue;
        }
        let mut b = Builder::new(l, &ctx, losses);
        b.run();
        let damaged = b.msg_ids.damaged + b.part_ids.damaged;
        b.losses.add_n("time-unrepresentable", None, damaged, "a time no opencode id can encode (a damaged source); the id follows the one before it");
        let (sess, where_) = b.finish();
        for (ev, mid) in where_ {
            placed.insert((l.id().to_string(), ev), mid);
        }
        out.push(sess);
    }
    for (sess, l) in out.iter_mut().zip(ordered(set)) {
        if l.has_struck() {
            repoint_parents(sess, losses);
        }
    }
    // Forks of rendered sessions point at a message in the session they forked.
    for (sess, l) in out.iter_mut().zip(ordered(set)) {
        if sess.info.native.is_some() || target.generation() != Gen::V2 {
            continue;
        }
        let Some(link) = l.agent().links.iter().find(|k| k.rel == "forked-from") else { continue };
        let Some(from) = link.agent.as_deref() else { continue };
        let from_sid = ctx.sessions.get(from).cloned().unwrap_or_else(|| from.to_string());
        let boundary = link
            .event
            .as_ref()
            .and_then(|ev| placed.get(&(from.to_string(), ev.clone())))
            .map(|mid| json!({"type": "through", "messageID": mid}))
            .or_else(|| link.extra.get("boundary").cloned());
        match boundary {
            Some(b) => {
                let v = &mut sess.info.value;
                let carrier = v.get("metadata").and_then(|m| m.get(HEADER_KEY)).cloned();
                let mut fresh = serde_json::Map::new();
                for (k, val) in v.as_object().unwrap().iter() {
                    fresh.insert(k.clone(), val.clone());
                    if (k == "id" || k == "parentID") && !fresh.contains_key("fork") && (k == "parentID" || v.get("parentID").is_none()) {
                        fresh.insert("fork".into(), json!({"sessionID": from_sid, "boundary": b}));
                    }
                }
                *v = Value::Object(fresh);
                let _ = carrier;
            }
            None => losses.add("link-dropped", None, "a forked-from link with no boundary message could not become a fork"),
        }
    }
    Ok(out)
}

/// A store ledger's rows for `target`: the newest version of each native
/// record of the target's own container. Rows of another container and the
/// ledger's other events have nowhere to go (`native-dropped`).
fn store_rows(l: &Ledger, target: Container, losses: &mut Losses) -> OutSession {
    let src = l.agent().source.as_ref().filter(|s| s.format == FORMAT);
    let mut order: Vec<String> = Vec::new();
    let mut rows: HashMap<String, OutItem> = HashMap::new();
    for e in &l.events {
        let native = e.native.as_ref().filter(|n| n.format == FORMAT && n.part == 0);
        let c = native.and_then(|n| n.variant.as_deref()).and_then(Container::from_variant);
        match (native, c, native.and_then(|n| n.record_json())) {
            (Some(n), Some(c), Some(rec)) if c == target => {
                let id = n.id.clone().unwrap_or_else(|| e.id.clone());
                let file = n.file.clone().unwrap_or_default();
                let item = OutItem {
                    value: Value::Null,
                    native: Some(NativeRec { container: c, record: rec, text: n.record_text().map(str::to_string), raw: current_raw(c, n), file }),
                };
                if rows.insert(id.clone(), item).is_none() {
                    order.push(id);
                }
            }
            _ => losses.add("native-dropped", Some(e.kind), "records with no neutral meaning are not rendered"),
        }
    }
    OutSession {
        id: l.id().to_string(),
        src: src.and_then(|s| s.variant.as_deref()).and_then(Container::from_variant),
        extra: src.map(|s| s.extra.clone()).unwrap_or_default(),
        info: OutItem { value: Value::Null, native: None },
        messages: Vec::new(),
        extras: order.into_iter().filter_map(|id| rows.remove(&id)).collect(),
        slug: String::new(),
        version: String::new(),
        store: true,
    }
}

/// SPEC §11.1, what opencode needs across a gap: an assistant message names
/// the user message it answers (`parentID`). When a projection left that
/// message out (it was redacted whole), the answer is re-pointed at the
/// nearest earlier user message of the session, the one change made to its
/// record; with none earlier, its `parentID` is left naming the message that
/// is gone, and that is named as a loss.
fn repoint_parents(sess: &mut OutSession, losses: &mut Losses) {
    let held: std::collections::HashSet<String> = sess.messages.iter().filter_map(|m| s(&m.info.value, "id").map(str::to_string)).collect();
    let mut last_user: Option<String> = None;
    for m in &mut sess.messages {
        let v = &m.info.value;
        if s(v, "role") == Some("user") {
            last_user = s(v, "id").map(str::to_string);
            continue;
        }
        let Some(old) = s(v, "parentID").map(str::to_string) else { continue };
        if held.contains(&old) {
            continue;
        }
        let Some(new) = last_user.clone() else {
            losses.add(
                "parent-message-redacted",
                None,
                "an answer whose user message was redacted, with no earlier user message to name, still names it (parentID)",
            );
            continue;
        };
        let (from, to) = (format!("\"parentID\":{}", json!(old)), format!("\"parentID\":{}", json!(new)));
        m.info.value["parentID"] = json!(new);
        if let Some(n) = &mut m.info.native {
            if n.record.get("parentID").is_some() {
                n.record["parentID"] = json!(new);
            }
            let swap = |t: &mut Option<String>| {
                *t = t.take().and_then(|t| t.contains(&from).then(|| t.replacen(&from, &to, 1)));
            };
            swap(&mut n.text);
            swap(&mut n.raw);
        }
    }
}

// ---------------------------------------------------------------- builder

/// What the builder knows about a rendered tool, to settle it from a result.
#[derive(Debug, Clone, Copy)]
struct ToolAt {
    msg: usize,
    /// V2: index into content; V1: index into parts.
    idx: usize,
}

struct Builder<'a, 'b> {
    l: &'a Ledger,
    ctx: &'a Ctx<'a>,
    losses: &'b mut Losses,
    generation: Gen,
    sid: String,
    src: Option<Container>,
    session_native: Option<OutItem>,
    messages: Vec<OutMsg>,
    /// Carried events per message (rendered messages only).
    carried: Vec<Vec<Event>>,
    /// Ledger events each native message came from (for a carrier, when one is needed).
    own: Vec<Vec<Event>>,
    rendered: Vec<bool>,
    pending: Vec<Event>,
    cur: Option<usize>,
    extras: Vec<OutItem>,
    msg_index: HashMap<String, usize>,
    /// native record id (message or part) → message index
    record_msg: HashMap<String, usize>,
    open_assistant: Option<usize>,
    open_group: Option<String>,
    open_user: Option<(usize, Option<String>)>,
    tools: HashMap<String, ToolAt>,
    msg_ids: Ascending,
    part_ids: Ascending,
    last_ms: i64,
    last_seq: i64,
    // state
    agent: Option<String>,
    model: Option<(String, String)>,
    variant: Option<String>,
    cwd: Option<String>,
    system_prompt: Option<String>,
    tools_on: Option<Vec<String>>,
    title: Option<String>,
    last_user: Option<String>,
    boundary_pending: Option<String>,
    /// event id → message id, for fork boundaries.
    placed: Vec<(String, String)>,
    created_ms: i64,
    // session defaults gathered before the first message
    first_agent: Option<String>,
    first_model: Option<(String, String)>,
    first_cwd: Option<String>,
    /// The header carrier, once built (1.x also stows it on the first rendered
    /// message, since a 1.2 `session` row has no column to hold it).
    header: Option<Value>,
    /// The last cumulative usage, for 2.x session totals.
    cumulative: Option<Event>,
    /// 1.x compaction: summary message index → the user message its carrier covers.
    covers: HashMap<usize, String>,
    covered: std::collections::HashSet<usize>,
    /// The last reasoning item rendered (message, content index), to end it when text follows.
    last_reasoning: Option<(usize, usize)>,
    /// The open assistant step's response is over (its usage arrived, or a tool
    /// result came back): an ungrouped event after it starts the next step.
    step_closed: bool,
    /// group → the assistant step rendered for it.
    step_of_group: HashMap<String, usize>,
    /// assistant step → the usage applied to it.
    step_usage: HashMap<usize, Event>,
    /// A provider stated before any model (opencode keeps it with the model).
    provider_hint: Option<String>,
}

fn event_ms(e: &Event) -> Option<i64> {
    e.ts.as_deref().and_then(crate::util::parse_rfc3339_ms)
}

/// A plausible provider for a model id, when the ledger does not say.
fn provider_for(model: &str) -> &'static str {
    let m = model.to_ascii_lowercase();
    if m.starts_with("claude") {
        "anthropic"
    } else if m.starts_with("gpt") || m.starts_with("o1") || m.starts_with("o3") || m.starts_with("o4") || m.starts_with("codex") {
        "openai"
    } else if m.starts_with("gemini") {
        "google"
    } else {
        "unknown"
    }
}

/// An absolute directory opencode can hold in `location.directory`: POSIX, or a
/// Windows drive or UNC path (opencode runs on Windows too).
pub(crate) fn is_absolute(d: &str) -> bool {
    let b = d.as_bytes();
    d.starts_with('/') || d.starts_with("\\\\") || (b.len() >= 3 && b[0].is_ascii_alphabetic() && b[1] == b':' && (b[2] == b'\\' || b[2] == b'/'))
}

fn is_base64(s: &str) -> bool {
    !s.is_empty()
        && s.len().is_multiple_of(4)
        && s.bytes().all(|b| b.is_ascii_alphanumeric() || b == b'+' || b == b'/' || b == b'=')
        && !s.trim_end_matches('=').contains('=')
}

fn text_of_output(out: &Value) -> String {
    match out {
        Value::String(s) => s.clone(),
        Value::Array(a) => a
            .iter()
            .filter_map(|p| match s(p, "type") {
                Some("text") => s(p, "text").map(str::to_string),
                _ => None,
            })
            .collect::<Vec<_>>()
            .join("\n"),
        Value::Null => String::new(),
        other => serde_json::to_string(other).unwrap_or_default(),
    }
}

impl<'a, 'b> Builder<'a, 'b> {
    fn new(l: &'a Ledger, ctx: &'a Ctx<'a>, losses: &'b mut Losses) -> Self {
        let src = l.agent().source.as_ref().filter(|s| s.format == FORMAT).and_then(|s| s.variant.as_deref()).and_then(Container::from_variant);
        let created_ms = l.agent().created_at.as_deref().and_then(crate::util::parse_rfc3339_ms).or_else(|| l.events.iter().find_map(event_ms)).unwrap_or(0);
        Builder {
            l,
            ctx,
            losses,
            generation: ctx.target.generation(),
            sid: ctx.sessions[l.id()].clone(),
            src,
            session_native: None,
            messages: Vec::new(),
            carried: Vec::new(),
            own: Vec::new(),
            rendered: Vec::new(),
            pending: Vec::new(),
            cur: None,
            extras: Vec::new(),
            msg_index: HashMap::new(),
            record_msg: HashMap::new(),
            open_assistant: None,
            open_group: None,
            open_user: None,
            tools: HashMap::new(),
            msg_ids: Ascending::new(),
            part_ids: Ascending::new(),
            last_ms: created_ms,
            last_seq: 0,
            agent: None,
            model: None,
            variant: None,
            cwd: None,
            system_prompt: None,
            tools_on: None,
            title: None,
            last_user: None,
            boundary_pending: None,
            placed: Vec::new(),
            created_ms,
            first_agent: None,
            first_model: None,
            first_cwd: None,
            header: None,
            cumulative: None,
            covers: HashMap::new(),
            covered: std::collections::HashSet::new(),
            last_reasoning: None,
            step_closed: false,
            step_of_group: HashMap::new(),
            step_usage: HashMap::new(),
            provider_hint: None,
        }
    }

    fn loss(&mut self, code: &str, kind: Option<Kind>, detail: &str) {
        self.losses.add(code, kind, detail);
    }

    fn run(&mut self) {
        let events: Vec<&Event> = self.l.events.iter().collect();
        // SPEC §2.3: the active path is rendered. A staged revert opencode
        // itself recorded (`session.revert`, a part of the native session
        // record) keeps its reverted messages, written back of this
        // generation; any other abandoned branch only rides along in a carrier.
        let generation = self.generation;
        let on = self.l.on_path(|e| {
            !e.native
                .as_ref()
                .is_some_and(|n| n.format == FORMAT && n.variant.as_deref().and_then(Container::from_variant).is_some_and(|c| c.generation() == generation))
        });
        let abandoned = on.iter().filter(|o| !**o).count() as u64;
        self.losses.add_n(
            "abandoned-branch-not-rendered",
            None,
            abandoned,
            "events on an abandoned branch (SPEC §2.3) are not part of the conversation; with carry they ride on the next message",
        );
        // Native records that cannot move to the target container (no id where
        // the target needs one): their events are rendered instead.
        let mut rendered_ids: std::collections::HashSet<String> = std::collections::HashSet::new();
        for (e, on) in events.into_iter().zip(on) {
            if !on {
                // Filed like any event with no message of its own, so order holds.
                self.file_event(e);
                continue;
            }
            if let Some(n) = &e.native
                && n.format == FORMAT
                && let Some(c) = n.variant.as_deref().and_then(Container::from_variant)
                && c.generation() == self.generation
                && !n.id.as_ref().is_some_and(|i| rendered_ids.contains(i))
            {
                if n.part > 0 {
                    if let Some(i) = n.id.as_ref().and_then(|r| self.record_msg.get(r)) {
                        self.own[*i].push(e.clone());
                    }
                    continue;
                }
                if let Some(rec) = &n.record_json() {
                    let id = n.id.clone().unwrap_or_default();
                    let file = n.file.clone().unwrap_or_default();
                    let role = role_of(c, &id, &file, rec);
                    let value = logical(c, role, rec);
                    // Extras only live in their own container.
                    if c != self.ctx.target && (role == super::Role::Extra || s(&value, "id").is_none()) {
                        rendered_ids.insert(id);
                        self.foreign(e);
                        continue;
                    }
                    let item = OutItem {
                        value,
                        native: Some(NativeRec { container: c, record: rec.clone(), text: n.record_text().map(str::to_string), raw: current_raw(c, n), file }),
                    };
                    if !self.newer_version(role, &item, e, &id) {
                        self.native_item(role, item, e, &id);
                    }
                    continue;
                }
            }
            self.foreign(e);
        }
    }

    // ------------------------------------------------ native records

    /// SPEC §4.1: a later event carrying a newer version of a message or part
    /// already placed (opencode rewrote it in place, and sync appended the
    /// version) replaces the earlier version where it stands: exporters use
    /// the newest version of each native record. False when the record was
    /// not placed yet.
    fn newer_version(&mut self, role: super::Role, item: &OutItem, e: &Event, native_id: &str) -> bool {
        let Some(&i) = self.record_msg.get(native_id) else { return false };
        match role {
            super::Role::Message if self.msg_index.get(native_id) == Some(&i) => self.messages[i].info = item.clone(),
            super::Role::Part => {
                let Some(k) = self.messages[i].parts.iter().position(|p| s(&p.value, "id") == Some(native_id)) else { return false };
                self.messages[i].parts[k] = item.clone();
            }
            _ => return false,
        }
        self.own[i].push(e.clone());
        true
    }

    fn native_item(&mut self, role: super::Role, item: OutItem, e: &Event, native_id: &str) {
        use super::Role::*;
        match role {
            Session => {
                let v = &item.value;
                if let Some(t) = s(v, "title") {
                    self.title = Some(t.to_string());
                }
                if let Some(c) = item.native.as_ref().map(|n| n.container) {
                    self.src.get_or_insert(c);
                }
                self.session_native = Some(item);
            }
            Message => {
                self.close_assistant();
                self.open_user = None;
                let v = &item.value;
                // The record's native id (an anchor when the record has no id).
                let id = native_id.to_string();
                if let Some(ms) = super::get_ms(v, &["time", "created"]) {
                    self.msg_ids.observe_ms(ms);
                    self.last_ms = self.last_ms.max(ms);
                }
                if let Some(sq) = item.native.as_ref().and_then(|n| n.record.get("seq")).and_then(Value::as_i64) {
                    self.last_seq = self.last_seq.max(sq);
                }
                self.observe_native_state(v);
                let seq = item.native.as_ref().and_then(|n| n.record.get("seq")).and_then(Value::as_i64);
                match self.msg_index.get(&id).copied() {
                    Some(i) if self.messages[i].info.value.is_null() => {
                        self.messages[i].info = item;
                        self.own[i].push(e.clone());
                    }
                    _ => {
                        self.msg_index.insert(id.clone(), self.messages.len());
                        self.messages.push(OutMsg { info: item, parts: vec![], seq });
                        self.carried.push(std::mem::take(&mut self.pending));
                        self.own.push(vec![e.clone()]);
                        self.rendered.push(false);
                    }
                }
                if let Some(i) = self.msg_index.get(&id).copied() {
                    self.record_msg.insert(id.clone(), i);
                }
                self.placed.push((e.id.clone(), id));
                self.cur = None;
            }
            Part => {
                let mid = s(&item.value, "messageID")
                    .map(str::to_string)
                    .or_else(|| native_id.rsplit_once("/parts/").map(|(m, _)| m.to_string()))
                    .or_else(|| e.group.clone())
                    .unwrap_or_default();
                if let Some(ms) = super::get_ms(&item.value, &["time", "start"]) {
                    self.part_ids.observe_ms(ms);
                }
                let i = match self.msg_index.get(&mid) {
                    Some(i) => *i,
                    None => {
                        self.msg_index.insert(mid.clone(), self.messages.len());
                        self.messages.push(OutMsg { info: OutItem { value: Value::Null, native: None }, parts: vec![], seq: None });
                        self.carried.push(std::mem::take(&mut self.pending));
                        self.own.push(vec![]);
                        self.rendered.push(false);
                        self.messages.len() - 1
                    }
                };
                self.messages[i].parts.push(item);
                self.own[i].push(e.clone());
                self.record_msg.insert(native_id.to_string(), i);
                self.cur = None;
            }
            Extra => self.extras.push(item),
        }
    }

    /// Track agent/model from native messages so rendered ones continue them.
    fn observe_native_state(&mut self, v: &Value) {
        match self.generation {
            Gen::V2 => match s(v, "type") {
                Some("assistant") | Some("model-switched") => {
                    if let Some(m) = v.get("model") {
                        self.model = Some((s(m, "providerID").unwrap_or("").into(), s(m, "id").unwrap_or("").into()));
                    }
                    if let Some(a) = s(v, "agent") {
                        self.agent = Some(a.into());
                    }
                }
                Some("agent-switched") => self.agent = s(v, "agent").map(str::to_string),
                _ => {}
            },
            Gen::V1 => {
                if s(v, "role") == Some("user") {
                    self.last_user = s(v, "id").map(str::to_string);
                    if let Some(m) = v.get("model") {
                        self.model = Some((s(m, "providerID").unwrap_or("").into(), s(m, "modelID").unwrap_or("").into()));
                    }
                } else if let (Some(p), Some(m)) = (s(v, "providerID"), s(v, "modelID")) {
                    self.model = Some((p.into(), m.into()));
                }
                if let Some(a) = s(v, "agent") {
                    self.agent = Some(a.into());
                }
            }
        }
    }

    // ------------------------------------------------ foreign events

    fn ms(&mut self, e: &Event) -> i64 {
        let ms = event_ms(e).unwrap_or(self.last_ms);
        self.last_ms = self.last_ms.max(ms);
        ms
    }

    /// File `e` with the current rendered message, or hold it for the next one.
    fn file_event(&mut self, e: &Event) {
        match self.cur {
            Some(i) => self.carried[i].push(e.clone()),
            None => self.pending.push(e.clone()),
        }
    }

    fn push_rendered(&mut self, value: Value, seed: &Event) -> usize {
        self.push_rendered_with(value, seed, true)
    }

    /// A rendered message; `carries_seed` is false for the second and later
    /// messages one event produces (its carrier then holds no events).
    fn push_rendered_with(&mut self, value: Value, seed: &Event, carries_seed: bool) -> usize {
        let i = self.messages.len();
        let id = s(&value, "id").unwrap_or("").to_string();
        self.msg_index.insert(id.clone(), i);
        self.last_seq += 1;
        self.messages.push(OutMsg { info: OutItem { value, native: None }, parts: vec![], seq: Some(self.last_seq) });
        let mut c = std::mem::take(&mut self.pending);
        if carries_seed {
            c.push(seed.clone());
        }
        self.carried.push(c);
        self.own.push(vec![]);
        self.rendered.push(true);
        self.cur = Some(i);
        self.placed.push((seed.id.clone(), id));
        if self.first_agent.is_none() && self.messages.iter().filter(|m| m.info.native.is_none()).count() == 1 {
            self.first_agent = self.agent.clone();
            self.first_model = self.model.clone();
            self.first_cwd = self.cwd.clone();
        }
        i
    }

    fn new_msg_id(&mut self, ms: i64, seed: &Event) -> String {
        self.msg_ids.next("msg", ms, &format!("{}/{}", self.l.id(), seed.id))
    }

    fn new_part_id(&mut self, ms: i64, seed: &Event, n: usize) -> String {
        self.part_ids.next("prt", ms, &format!("{}/{}#{n}", self.l.id(), seed.id))
    }

    fn model_ref(&mut self) -> (String, String) {
        match &self.model {
            Some((p, m)) => (if p.is_empty() { provider_for(m).to_string() } else { p.clone() }, m.clone()),
            None => {
                self.loss("model-unknown", None, "no model was recorded; `unknown/unknown` was written");
                ("unknown".into(), "unknown".into())
            }
        }
    }

    fn agent_name(&self) -> String {
        self.agent.clone().unwrap_or_else(|| "build".into())
    }

    fn close_assistant(&mut self) {
        self.open_assistant = None;
        self.open_group = None;
    }

    fn foreign(&mut self, e: &Event) {
        match e.kind {
            Kind::Text => self.text(e),
            Kind::Media => self.media(e),
            Kind::Reasoning => self.reasoning(e),
            Kind::ToolCall => self.tool_call(e),
            Kind::ToolResult => self.tool_result(e),
            Kind::Usage => self.usage(e),
            Kind::Settings => self.settings(e),
            Kind::Title => {
                if let Some(t) = e.str("text") {
                    self.title = Some(t.to_string());
                }
                self.file_event(e);
            }
            Kind::Compaction => self.compaction(e),
            Kind::Turn => self.turn(e),
            Kind::Subagent => self.file_event(e),
            Kind::PeerMessage => {
                // SPEC §3.1.6: opencode has no peer channel; the model sees the message
                // wrapped as user text (a synthetic message: no one typed it), and
                // the transcript names the peer.
                let peer = e.str("peer").unwrap_or("peer").to_string();
                let dir = e.str("direction").unwrap_or("in").to_string();
                let network = e.str("network").map(|n| format!(" [{n}]")).unwrap_or_default();
                self.synthetic(e, &super::peer_wrap(e), Some(format!("peer message ({dir}) {peer}{network}")));
            }
            Kind::FileChange => {
                self.loss("file-change-dropped", Some(Kind::FileChange), "file changes have no opencode record without a snapshot store");
                self.file_event(e);
            }
            Kind::Error => self.error(e),
            Kind::App | Kind::Native => {
                self.loss("native-dropped", Some(e.kind), "records with no neutral meaning are not rendered");
                self.file_event(e);
            }
        }
    }

    fn text(&mut self, e: &Event) {
        let role = e.str("role").unwrap_or("user");
        let text = e.str("text").unwrap_or("").to_string();
        if e.effective_audience() == crate::model::Audience::Record {
            match self.generation {
                // 2.x shows every message to the model: a record-only text has no place.
                Gen::V2 => {
                    self.loss("record-text-dropped", Some(Kind::Text), "text the model never saw has no 2.x message the model would not see");
                    self.file_event(e);
                    return;
                }
                // 1.x keeps it as an `ignored` text part.
                Gen::V1 => {
                    self.close_assistant();
                    self.open_user = None;
                    let i = self.v1_user(e, &text, role != "user");
                    if let Some(p) = self.messages[i].parts.last_mut() {
                        p.value["ignored"] = json!(true);
                    }
                    return;
                }
            }
        }
        match role {
            "assistant" => {
                let i = self.ensure_assistant(e);
                // A reasoning item ends where the text after it begins (opencode's own
                // `time.completed`); the text, which has no time of its own, is dated by it.
                if let (Some((ri, idx)), Some(ms), Gen::V2) = (self.last_reasoning, event_ms(e), self.generation) {
                    let content = &mut self.messages[i].info.value["content"];
                    if ri == i && content.as_array().is_some_and(|c| c.len() == idx + 1) {
                        let item = &mut content[idx];
                        let start = item["time"]["created"].as_i64().unwrap_or(ms);
                        item["time"]["completed"] = json!(ms.max(start));
                    }
                }
                self.add_content(i, e, json!({"type": "text", "text": text}));
            }
            "system" | "developer" => {
                if role == "developer" {
                    self.loss("role-developer-as-system", Some(Kind::Text), "developer messages are written as system messages");
                }
                self.close_assistant();
                self.open_user = None;
                match self.generation {
                    Gen::V2 => {
                        let ms = self.ms(e);
                        let id = self.new_msg_id(ms, e);
                        self.push_rendered(json!({"id": id, "time": {"created": ms}, "type": "system", "text": text}), e);
                    }
                    Gen::V1 => {
                        self.loss("role-system-as-synthetic", Some(Kind::Text), "1.x has no system message; written as synthetic user text");
                        self.v1_user(e, &text, true);
                    }
                }
            }
            _ => {
                if e.body.get("synthetic").and_then(Value::as_bool) == Some(true) {
                    self.synthetic(e, &text, None);
                    return;
                }
                // Consecutive user text of one message merges.
                if let Some((i, g)) = self.open_user.clone()
                    && g.is_some()
                    && g == e.group
                    && self.rendered[i]
                {
                    self.carried[i].push(e.clone());
                    match self.generation {
                        Gen::V2 => {
                            let m = &mut self.messages[i].info.value;
                            let prev = s(m, "text").unwrap_or("").to_string();
                            m["text"] = json!(if prev.is_empty() { text } else { format!("{prev}\n\n{text}") });
                        }
                        Gen::V1 => {
                            let ms = self.ms(e);
                            let pid = self.new_part_id(ms, e, 0);
                            let p = json!({"id": pid, "sessionID": self.sid, "messageID": s(&self.messages[i].info.value, "id"), "type": "text", "text": text});
                            self.messages[i].parts.push(OutItem { value: p, native: None });
                        }
                    }
                    return;
                }
                self.close_assistant();
                match self.generation {
                    Gen::V2 => {
                        let ms = self.ms(e);
                        let id = self.new_msg_id(ms, e);
                        let i = self.push_rendered(json!({"id": id, "time": {"created": ms}, "text": text, "type": "user"}), e);
                        self.open_user = Some((i, e.group.clone()));
                    }
                    Gen::V1 => {
                        let i = self.v1_user(e, &text, false);
                        self.open_user = Some((i, e.group.clone()));
                    }
                }
            }
        }
    }

    fn synthetic(&mut self, e: &Event, text: &str, description: Option<String>) {
        self.close_assistant();
        self.open_user = None;
        match self.generation {
            Gen::V2 => {
                let ms = self.ms(e);
                let id = self.new_msg_id(ms, e);
                let mut m = json!({"id": id, "time": {"created": ms}, "text": text});
                if let Some(d) = description {
                    m["description"] = json!(d);
                }
                m["type"] = json!("synthetic");
                self.push_rendered(m, e);
            }
            Gen::V1 => {
                self.v1_user(e, text, true);
            }
        }
    }

    /// A V1 user message with one text part.
    fn v1_user(&mut self, e: &Event, text: &str, synthetic: bool) -> usize {
        let ms = self.ms(e);
        let id = self.new_msg_id(ms, e);
        let (p, m) = self.model_ref();
        let mut info = json!({"id": id, "sessionID": self.sid, "role": "user", "time": {"created": ms}, "agent": self.agent_name(), "model": {"providerID": p, "modelID": m}});
        if let Some(sys) = self.system_prompt.take() {
            info["system"] = json!(sys);
        }
        if let Some(tools) = self.tools_on.take() {
            info["tools"] = Value::Object(tools.into_iter().map(|t| (t, json!(true))).collect());
        }
        let i = self.push_rendered(info, e);
        self.last_user = Some(id.clone());
        let pid = self.new_part_id(ms, e, 0);
        let mut part = json!({"id": pid, "sessionID": self.sid, "messageID": id, "type": "text", "text": text});
        if synthetic {
            part["synthetic"] = json!(true);
        }
        self.messages[i].parts.push(OutItem { value: part, native: None });
        i
    }

    fn media(&mut self, e: &Event) {
        let role = e.str("role").unwrap_or("user");
        let mime = e.str("media_type").unwrap_or("application/octet-stream").to_string();
        let data = e.str("data").map(str::to_string);
        let url = e.str("url").map(str::to_string);
        let name = e.str("name").map(str::to_string);
        if role == "assistant" {
            self.loss("media-dropped", Some(Kind::Media), "opencode has no assistant media");
            self.file_event(e);
            return;
        }
        match self.generation {
            Gen::V2 => {
                let Some(data) = data.filter(|d| is_base64(d)) else {
                    self.loss("media-dropped", Some(Kind::Media), "2.x attachments need inline base64 data");
                    self.file_event(e);
                    return;
                };
                let i = match self.open_user.clone() {
                    Some((i, g)) if self.rendered[i] && (g == e.group || g.is_none()) => {
                        self.carried[i].push(e.clone());
                        i
                    }
                    _ => {
                        self.close_assistant();
                        let ms = self.ms(e);
                        let id = self.new_msg_id(ms, e);
                        let i = self.push_rendered(json!({"id": id, "time": {"created": ms}, "text": "", "type": "user"}), e);
                        self.open_user = Some((i, e.group.clone()));
                        i
                    }
                };
                let mut f = json!({"data": data, "mime": mime, "source": {"type": "inline"}});
                if let Some(n) = name {
                    f["name"] = json!(n);
                }
                let m = &mut self.messages[i].info.value;
                let o = m.as_object_mut().unwrap();
                let ty = o.remove_ordered("type");
                o.entry("files").or_insert(json!([])).as_array_mut().unwrap().push(f);
                if let Some(t) = ty {
                    o.insert("type".into(), t);
                }
            }
            Gen::V1 => {
                let url = match (data, url) {
                    (Some(d), _) => format!("data:{mime};base64,{d}"),
                    (None, Some(u)) => u,
                    (None, None) => {
                        self.loss("media-dropped", Some(Kind::Media), "media with no data or url");
                        self.file_event(e);
                        return;
                    }
                };
                let i = match self.open_user.clone() {
                    Some((i, g)) if self.rendered[i] && (g == e.group || g.is_none()) => {
                        self.carried[i].push(e.clone());
                        i
                    }
                    _ => {
                        self.close_assistant();
                        let i = self.v1_user(e, "", false);
                        // v1_user made an empty text part; a media-only message keeps just the file.
                        self.messages[i].parts.clear();
                        self.open_user = Some((i, e.group.clone()));
                        i
                    }
                };
                let ms = self.ms(e);
                let pid = self.new_part_id(ms, e, 0);
                let mut p = json!({"id": pid, "sessionID": self.sid, "messageID": s(&self.messages[i].info.value, "id"), "type": "file", "mime": mime});
                if let Some(n) = name {
                    p["filename"] = json!(n);
                }
                p["url"] = json!(url);
                self.messages[i].parts.push(OutItem { value: p, native: None });
            }
        }
    }

    /// The open assistant message for `e`, or a new one.
    fn ensure_assistant(&mut self, e: &Event) -> usize {
        if let Some(i) = self.open_assistant {
            // One step is one model response: events of its group, or ungrouped
            // events until its response is over.
            let cont = match (&e.group, &self.open_group) {
                (Some(a), Some(b)) => a == b,
                _ => !self.step_closed,
            };
            if self.rendered[i] && cont {
                if self.cur == Some(i) {
                    self.carried[i].push(e.clone());
                } else {
                    self.file_event(e);
                }
                return i;
            }
        }
        self.open_user = None;
        let ms = self.ms(e);
        let id = self.new_msg_id(ms, e);
        let (p, m) = self.model_ref();
        let agent = self.agent_name();
        let v = match self.generation {
            Gen::V2 => {
                let mut model = json!({"id": m, "providerID": p});
                if let Some(v) = &self.variant {
                    model["variant"] = json!(v);
                }
                json!({"id": id, "time": {"created": ms, "completed": ms}, "type": "assistant", "agent": agent, "model": model, "content": []})
            }
            Gen::V1 => {
                let parent = self.last_user.clone().unwrap_or_else(|| id.clone());
                let cwd = self.cwd.clone().unwrap_or_else(|| "/".into());
                json!({
                    "id": id, "sessionID": self.sid, "role": "assistant", "time": {"created": ms, "completed": ms},
                    "parentID": parent, "modelID": m, "providerID": p, "mode": agent, "agent": agent,
                    "path": {"cwd": cwd, "root": cwd}, "cost": 0,
                    "tokens": {"input": 0, "output": 0, "reasoning": 0, "cache": {"read": 0, "write": 0}},
                })
            }
        };
        let i = self.push_rendered(v, e);
        self.open_assistant = Some(i);
        self.open_group = e.group.clone();
        self.step_closed = false;
        if let Some(g) = &e.group {
            self.step_of_group.insert(g.clone(), i);
        }
        i
    }

    /// Name the provider of assistant step `i` when it was not known.
    fn adopt_provider(&mut self, i: usize, provider: &str) {
        let v = &mut self.messages[i].info.value;
        match self.generation {
            Gen::V2 => {
                if v.get("model").and_then(|m| s(m, "providerID")) == Some("unknown") {
                    v["model"]["providerID"] = json!(provider);
                }
            }
            Gen::V1 => {
                if s(v, "providerID") == Some("unknown") {
                    v["providerID"] = json!(provider);
                }
            }
        }
    }

    /// Name the model of assistant step `i` from the usage that billed it.
    fn adopt_model(&mut self, i: usize, model: &str) {
        let v = &mut self.messages[i].info.value;
        let (idk, pk) = match self.generation {
            Gen::V2 => (&["model", "id"][..], &["model", "providerID"][..]),
            Gen::V1 => (&["modelID"][..], &["providerID"][..]),
        };
        if crate::util::get(v, idk).and_then(Value::as_str) == Some(model) {
            return;
        }
        let provider = crate::util::get(v, pk).and_then(Value::as_str).unwrap_or("unknown").to_string();
        let provider = if provider == "unknown" { provider_for(model).to_string() } else { provider };
        match self.generation {
            Gen::V2 => {
                v["model"]["id"] = json!(model);
                v["model"]["providerID"] = json!(provider);
            }
            Gen::V1 => {
                v["modelID"] = json!(model);
                v["providerID"] = json!(provider);
            }
        }
    }

    fn bump_completed(&mut self, i: usize, ms: i64) {
        if let Some(t) = self.messages[i].info.value.get_mut("time") {
            let c = t.get("completed").and_then(Value::as_i64).unwrap_or(0);
            if ms > c {
                t["completed"] = json!(ms);
            }
        }
    }

    /// Add one assistant content item (V2) or part (V1).
    fn add_content(&mut self, i: usize, e: &Event, v2: Value) -> usize {
        let ms = self.ms(e);
        self.bump_completed(i, ms);
        match self.generation {
            Gen::V2 => {
                let mut v2 = v2;
                if let Some(o) = v2.as_object_mut() {
                    o.remove_ordered("vendor");
                }
                let c = self.messages[i].info.value["content"].as_array_mut().unwrap();
                c.push(v2);
                c.len() - 1
            }
            Gen::V1 => {
                let n = self.messages[i].parts.len();
                let pid = self.new_part_id(ms, e, n);
                let mid = s(&self.messages[i].info.value, "id").unwrap_or("").to_string();
                let mut p = json!({"id": pid, "sessionID": self.sid, "messageID": mid});
                let o = p.as_object_mut().unwrap();
                match s(&v2, "type") {
                    Some("text") => {
                        o.insert("type".into(), json!("text"));
                        o.insert("text".into(), v2["text"].clone());
                        o.insert("time".into(), json!({"start": ms, "end": ms}));
                    }
                    Some("reasoning") => {
                        o.insert("type".into(), json!("reasoning"));
                        o.insert("text".into(), v2["text"].clone());
                        if let (Some(st), Some(vendor)) = (v2.get("state"), v2.get("vendor").and_then(Value::as_str)) {
                            o.insert("metadata".into(), json!({vendor: st}));
                        }
                        o.insert("time".into(), json!({"start": ms, "end": ms}));
                    }
                    Some("tool") => {
                        o.insert("type".into(), json!("tool"));
                        o.insert("callID".into(), v2["id"].clone());
                        o.insert("tool".into(), v2["name"].clone());
                        o.insert("state".into(), json!({"status": "error", "input": v2["state"]["input"].clone(), "error": "Tool execution was interrupted", "time": {"start": ms, "end": ms}}));
                        if let Some(md) = v2["state"].get("metadata") {
                            o["state"]["metadata"] = md.clone();
                        }
                    }
                    _ => {}
                }
                self.messages[i].parts.push(OutItem { value: p, native: None });
                n
            }
        }
    }

    fn reasoning(&mut self, e: &Event) {
        let mut text = e.str("text").unwrap_or("").to_string();
        let summaries: Option<Vec<String>> =
            e.body.get("summary").and_then(Value::as_array).map(|a| a.iter().filter_map(Value::as_str).map(str::to_string).collect());
        // Reasoning known only by its summary is OpenAI Responses reasoning: opencode
        // keeps a summary as the item's text, with OpenAI's provider state.
        let summary_mode = text.is_empty() && summaries.is_some();
        if summary_mode {
            let sum = summaries.clone().unwrap_or_default();
            if sum.len() > 1 {
                self.loss("reasoning-summaries-joined", Some(Kind::Reasoning), "several reasoning summaries are written as one opencode reasoning item");
            }
            text = sum.join("\n\n");
        } else if summaries.as_ref().is_some_and(|s| !s.is_empty()) {
            self.loss("reasoning-summary-dropped", Some(Kind::Reasoning), "opencode reasoning holds the reasoning text or a summary, not both");
        }
        let sealed = e.body.get("sealed").filter(|v| !v.is_null()).cloned();
        let mut state = sealed.as_ref().and_then(|x| super::v2::state_of(x, summary_mode));
        if sealed.is_some() && state.is_none() {
            self.loss("reasoning-sealed-dropped", Some(Kind::Reasoning), "a reasoning seal opencode has no field for");
        }
        if summary_mode && state.is_none() {
            state = Some(json!({"reasoningEncryptedContent": null}));
        }
        if text.is_empty() && !summary_mode && e.body.get("redacted").and_then(Value::as_bool) == Some(true) && state.is_none() {
            self.loss("reasoning-redacted-dropped", Some(Kind::Reasoning), "redacted reasoning has no text to write");
            self.file_event(e);
            return;
        }
        let i = self.ensure_assistant(e);
        // The seal's vendor is the provider that issued it: name it when nothing else does.
        if let Some(v) = sealed.as_ref().and_then(|x| s(x, "vendor")).filter(|v| *v != "unknown") {
            self.adopt_provider(i, v);
        }
        let ms = self.ms(e);
        let mut c = json!({"type": "reasoning", "text": text});
        if let Some(st) = state {
            // V2: provider-scoped `state`; V1: AI SDK provider metadata `{vendor: state}`.
            c["state"] = st;
            c["vendor"] = sealed.as_ref().and_then(|x| x.get("vendor")).cloned().unwrap_or(json!("openai"));
        }
        c["time"] = json!({"created": ms, "completed": ms});
        let idx = self.add_content(i, e, c);
        self.last_reasoning = Some((i, idx));
    }

    fn tool_call(&mut self, e: &Event) {
        let call = e.str("call_id").unwrap_or(&e.id).to_string();
        let name = e.str("name").unwrap_or("tool").to_string();
        let input = match (e.body.get("input"), e.str("input_text")) {
            (Some(Value::Object(o)), _) => Value::Object(o.clone()),
            // Arguments recorded as JSON text: opencode keeps them parsed.
            (Some(Value::String(t)), _) if serde_json::from_str::<Value>(t).is_ok_and(|v| v.is_object()) => {
                self.loss("tool-input-parsed", Some(Kind::ToolCall), "tool arguments recorded as JSON text are written parsed");
                serde_json::from_str(t).unwrap()
            }
            // A plain string that is not free-text input (`input_text`): kept as a value,
            // so it does not read back as a free-text tool.
            (Some(Value::String(t)), _) => {
                self.loss("tool-input-wrapped", Some(Kind::ToolCall), "a string tool input is written as {\"value\": …}");
                json!({"value": t})
            }
            (Some(v), _) if !v.is_null() => {
                self.loss("tool-input-wrapped", Some(Kind::ToolCall), "a non-object tool input is written as {\"input\": …}");
                json!({"input": v})
            }
            (_, Some(t)) => {
                self.loss("tool-input-wrapped", Some(Kind::ToolCall), "a raw-text tool input is written as {\"input\": …}");
                json!({"input": t})
            }
            _ => json!({}),
        };
        let i = self.ensure_assistant(e);
        let ms = self.ms(e);
        let mut state =
            json!({"status": "error", "input": input, "error": {"type": "tool.interrupted", "message": "No result was recorded for this tool call"}});
        if let Some(child) = self.ctx.children.get(&(self.l.id().to_string(), e.id.clone())) {
            let key = if self.generation == Gen::V2 { "sessionID" } else { "sessionId" };
            state["metadata"] = json!({key: child});
        }
        let idx = self.add_content(i, e, json!({"type": "tool", "id": call, "name": name, "state": state, "time": {"created": ms}}));
        self.tools.insert(call, ToolAt { msg: i, idx });
    }

    fn tool_result(&mut self, e: &Event) {
        let call = e.str("call_id").unwrap_or("").to_string();
        let Some(at) = self.tools.get(&call).copied() else {
            self.loss("tool-result-orphan", Some(Kind::ToolResult), "a tool result whose call is not in the ledger");
            self.file_event(e);
            return;
        };
        self.file_event(e);
        if self.open_assistant == Some(at.msg) {
            self.step_closed = true;
        }
        let ms = self.ms(e);
        let out = e.body.get("output").cloned().unwrap_or(json!(""));
        let is_error = e.body.get("is_error").and_then(Value::as_bool) == Some(true);
        self.bump_completed(at.msg, ms);
        match self.generation {
            Gen::V2 => {
                let mut content: Vec<Value> = Vec::new();
                match &out {
                    Value::Array(parts) => {
                        for p in parts {
                            match s(p, "type") {
                                Some("text") => content.push(json!({"type": "text", "text": s(p, "text").unwrap_or("")})),
                                Some("media") | Some("image") => {
                                    let mime = s(p, "media_type").unwrap_or("application/octet-stream");
                                    if let Some(d) = s(p, "data") {
                                        content.push(json!({"type": "file", "uri": format!("data:{mime};base64,{d}"), "mime": mime}));
                                    } else if let Some(u) = s(p, "url") {
                                        content.push(json!({"type": "file", "uri": u, "mime": mime}));
                                    } else {
                                        self.losses.add("media-dropped", Some(Kind::ToolResult), "tool output media with no data or url");
                                    }
                                }
                                _ => content.push(json!({"type": "text", "text": serde_json::to_string(p).unwrap_or_default()})),
                            }
                        }
                    }
                    other => content.push(json!({"type": "text", "text": text_of_output(other)})),
                }
                if content.is_empty() {
                    content.push(json!({"type": "text", "text": ""}));
                }
                let tool = &mut self.messages[at.msg].info.value["content"][at.idx];
                let input = tool["state"]["input"].clone();
                let md = tool["state"].get("metadata").cloned();
                let mut st = if is_error {
                    let msg = text_of_output(&out);
                    json!({"status": "error", "input": input, "error": {"type": "tool.execution", "message": msg}, "content": content})
                } else {
                    json!({"status": "completed", "input": input, "content": content})
                };
                if let Some(md) = md {
                    st["metadata"] = md;
                }
                tool["state"] = st;
                tool["time"]["completed"] = json!(ms);
            }
            Gen::V1 => {
                let part = &mut self.messages[at.msg].parts[at.idx].value;
                let input = part["state"]["input"].clone();
                let start = part["state"]["time"]["start"].clone();
                let md = part["state"].get("metadata").cloned().unwrap_or(json!({}));
                part["state"] = if is_error {
                    json!({"status": "error", "input": input, "error": text_of_output(&out), "metadata": md, "time": {"start": start, "end": ms}})
                } else {
                    json!({"status": "completed", "input": input, "output": text_of_output(&out), "title": "", "metadata": md, "time": {"start": start, "end": ms}})
                };
            }
        }
    }

    fn last_assistant(&self) -> Option<usize> {
        (0..self.messages.len()).rev().find(|i| {
            self.rendered[*i]
                && match self.generation {
                    Gen::V2 => s(&self.messages[*i].info.value, "type") == Some("assistant"),
                    Gen::V1 => s(&self.messages[*i].info.value, "role") == Some("assistant"),
                }
        })
    }

    fn usage(&mut self, e: &Event) {
        if e.body.get("cumulative").and_then(Value::as_bool) == Some(true) {
            self.file_event(e);
            if self.generation == Gen::V2 {
                // 2.x keeps running totals on the session.
                self.cumulative = Some(e.clone());
            } else {
                self.loss("usage-cumulative-dropped", Some(Kind::Usage), "1.x sessions keep no running totals");
            }
            return;
        }
        // The step this usage reports: the one rendered for its group, the open
        // one, a new one for a group not rendered yet (usage may come first), or
        // the last one.
        let by_group = e.group.as_ref().and_then(|g| self.step_of_group.get(g)).copied().filter(|i| self.rendered[*i]);
        let open = self.open_assistant.filter(|i| self.rendered[*i]).filter(|_| e.group.is_none() || self.open_group.is_none() || e.group == self.open_group);
        let i = match by_group.or(open) {
            Some(i) => {
                self.file_event(e);
                i
            }
            None if e.group.is_some() => self.ensure_assistant(e),
            None => match self.last_assistant() {
                Some(i) => {
                    self.file_event(e);
                    i
                }
                None => self.ensure_assistant(e),
            },
        };
        if self.open_assistant == Some(i) {
            self.step_closed = true;
        }
        let n = |ev: &Event, k: &str| ev.body.get(k).and_then(Value::as_f64).unwrap_or(0.0);
        if let Some(prev) = self.step_usage.get(&i) {
            let same = ["input", "output", "reasoning", "cache_read", "cache_write"].iter().all(|k| n(prev, k) == n(e, k));
            if same {
                self.loss("usage-duplicate", Some(Kind::Usage), "a second, identical usage for one response is written once");
                return;
            }
            self.loss("usage-merged", Some(Kind::Usage), "several usages for one response are summed into its step");
        }
        self.step_usage.insert(i, e.clone());
        if let Some(m) = e.str("model") {
            self.adopt_model(i, m);
        }
        // The response finished streaming when its usage was reported, and the
        // step is not complete before that.
        if let Some(ms) = event_ms(e) {
            self.bump_completed(i, ms);
            if self.generation == Gen::V2
                && let Some(t) = self.messages[i].info.value.get_mut("time").and_then(Value::as_object_mut)
            {
                let created = t.get("created").and_then(Value::as_i64).unwrap_or(ms);
                let completed = t.remove_ordered("completed");
                t.entry("streamed").or_insert(json!(ms.max(created)));
                if let Some(c) = completed {
                    t.insert("completed".into(), c);
                }
            }
        }
        let m = &mut self.messages[i].info.value;
        // 2.x and 1.19 store `output` without reasoning (SPEC §3.1.1 counts it in);
        // 1.1 and 1.2 counted it in, as the AI SDK reports it.
        let reasoning = n(e, "reasoning");
        let version = self.session_native.as_ref().and_then(|n| s(&n.value, "version")).map(str::to_string).unwrap_or_else(|| self.ctx.version.clone());
        let excludes = self.generation == Gen::V2 || super::v1::output_excludes_reasoning(Some(&version));
        let output = if excludes { (n(e, "output") - reasoning).max(0.0) } else { n(e, "output") };
        let tok = m.get("tokens").cloned().unwrap_or(json!({"input": 0, "output": 0, "reasoning": 0, "cache": {"read": 0, "write": 0}}));
        let g = |path: &[&str]| crate::util::get(&tok, path).and_then(Value::as_f64).unwrap_or(0.0);
        let new_tok = json!({
            "input": js_num(g(&["input"]) + n(e, "input")),
            "output": js_num(g(&["output"]) + output),
            "reasoning": js_num(g(&["reasoning"]) + reasoning),
            "cache": {"read": js_num(g(&["cache", "read"]) + n(e, "cache_read")), "write": js_num(g(&["cache", "write"]) + n(e, "cache_write"))},
        });
        let had_cost = e.body.get("cost_usd").is_some_and(|c| !c.is_null());
        let o = m.as_object_mut().unwrap();
        if had_cost || self.generation == Gen::V1 {
            // 2.x cost is optional and written only when known; 1.x requires it.
            let cost = o.get("cost").and_then(Value::as_f64).unwrap_or(0.0) + n(e, "cost_usd");
            let ty = o.remove_ordered("type");
            o.insert("cost".into(), js_num(cost));
            if let Some(t) = ty {
                o.insert("type".into(), t);
            }
        }
        o.insert("tokens".into(), new_tok);
    }

    fn settings(&mut self, e: &Event) {
        self.file_event(e);
        let mut seeded = false;
        let had_messages = self.messages.iter().any(|m| !m.info.value.is_null());
        let switch = had_messages;
        let mut dropped = 0;
        for k in e.body.keys() {
            if !["model", "provider", "agent", "cwd", "effort", "complete", "system_prompt", "tools"].contains(&k.as_str()) {
                dropped += 1;
            }
        }
        if dropped > 0 {
            self.losses.add_n(
                "settings-field-dropped",
                Some(Kind::Settings),
                dropped,
                "settings opencode has no place for (permission mode, sandbox, approval, git branch)",
            );
        }
        if let Some(sp) = e.str("system_prompt") {
            if self.generation == Gen::V1 {
                self.system_prompt = Some(sp.to_string());
            } else {
                self.loss("settings-field-dropped", Some(Kind::Settings), "2.x keeps no per-session system prompt");
            }
        }
        if let Some(tools) = e.body.get("tools").and_then(Value::as_array) {
            if self.generation == Gen::V1 {
                self.tools_on = Some(tools.iter().filter_map(|t| t.as_str().or_else(|| s(t, "name")).map(str::to_string)).collect());
            } else {
                self.loss("settings-field-dropped", Some(Kind::Settings), "2.x keeps no per-session tool list");
            }
        }
        // Model and effort (opencode's model variant). Absent fields are unchanged.
        // A provider stated alone changes the current model's provider, or stays in
        // force for the model that follows.
        let model_in = match (e.str("provider"), e.str("model")) {
            (Some(p), None) => match &self.model {
                Some((_, m)) => Some((p.to_string(), m.clone())),
                None => {
                    self.provider_hint = Some(p.to_string());
                    None
                }
            },
            (p, Some(m)) => {
                let p = p.map(str::to_string).or_else(|| self.provider_hint.clone()).unwrap_or_else(|| provider_for(m).to_string());
                Some((p, m.to_string()))
            }
            (None, None) => None,
        };
        let new_model = model_in.or_else(|| self.model.clone());
        if new_model.is_none() && e.body.get("effort").is_some() {
            self.loss("settings-field-dropped", Some(Kind::Settings), "an effort with no model: opencode's effort is a variant of a model");
        }
        let new_variant = match e.body.get("effort") {
            Some(Value::String(v)) => Some(v.clone()),
            Some(_) => None,
            None => self.variant.clone(),
        };
        if let Some(new) = new_model.filter(|n| self.model.as_ref() != Some(n) || new_variant != self.variant) {
            let prev = self.model.clone();
            let prev_variant = self.variant.clone();
            self.model = Some(new.clone());
            self.variant = new_variant.clone();
            // A change after the session began (a message, or an earlier settings
            // event) is a switch opencode records as a message.
            if switch && self.generation == Gen::V2 {
                self.close_assistant();
                self.open_user = None;
                let ms = self.ms(e);
                let id = self.new_msg_id(ms, e);
                let mut model = json!({"id": new.1, "providerID": new.0});
                if let Some(v) = &new_variant {
                    model["variant"] = json!(v);
                }
                let mut msg = json!({"id": id, "time": {"created": ms}, "type": "model-switched", "model": model});
                if let Some((pp, pm)) = prev {
                    msg["previous"] = json!({"id": pm, "providerID": pp});
                    if let Some(v) = prev_variant {
                        msg["previous"]["variant"] = json!(v);
                    }
                }
                if !seeded {
                    self.carried_pop_last(e);
                }
                self.push_rendered_with(msg, e, !seeded);
                seeded = true;
            }
        }
        if let Some(a) = e.str("agent")
            && self.agent.as_deref() != Some(a)
        {
            let prev = self.agent.replace(a.to_string());
            if switch && self.generation == Gen::V2 {
                self.close_assistant();
                self.open_user = None;
                let ms = self.ms(e);
                let id = self.new_msg_id(ms, e);
                let mut msg = json!({"id": id, "time": {"created": ms}, "type": "agent-switched", "agent": a});
                if let Some(p) = prev {
                    msg["previous"] = json!(p);
                }
                if !seeded {
                    self.carried_pop_last(e);
                }
                self.push_rendered_with(msg, e, !seeded);
                seeded = true;
            }
        }
        if let Some(d) = e.str("cwd") {
            if self.generation == Gen::V2 && !is_absolute(d) {
                // 2.x locations are absolute; the session stays where it was.
                self.loss("settings-field-dropped", Some(Kind::Settings), "a working directory that is not an absolute path has no 2.x location");
            } else if self.cwd.as_deref() != Some(d) {
                let prev = self.cwd.replace(d.to_string());
                if switch && self.generation == Gen::V2 && is_absolute(d) {
                    self.close_assistant();
                    self.open_user = None;
                    let ms = self.ms(e);
                    let id = self.new_msg_id(ms, e);
                    let mut msg = json!({"id": id, "time": {"created": ms}, "type": "location-switched", "location": {"directory": d}});
                    if let Some(p) = prev.filter(|p| is_absolute(p)) {
                        msg["previous"] = json!({"location": {"directory": p}});
                    }
                    if !seeded {
                        self.carried_pop_last(e);
                    }
                    self.push_rendered_with(msg, e, !seeded);
                }
            }
        }
    }

    /// `e` was filed with the current message, but it starts a new one: move it.
    fn carried_pop_last(&mut self, e: &Event) {
        let removed = match self.cur {
            Some(i) => self.carried[i].last().is_some_and(|x| x.id == e.id) && self.carried[i].pop().is_some(),
            None => self.pending.last().is_some_and(|x| x.id == e.id) && self.pending.pop().is_some(),
        };
        let _ = removed;
    }

    fn compaction(&mut self, e: &Event) {
        let phase = e.str("phase").unwrap_or("summary");
        let trigger = if e.str("trigger") == Some("manual") { "manual" } else { "auto" };
        if phase != "summary" {
            self.boundary_pending = Some(trigger.into());
            self.file_event(e);
            return;
        }
        let trigger = self.boundary_pending.take().filter(|_| e.str("trigger").is_none()).unwrap_or(trigger.into());
        let summary = e.str("summary").unwrap_or("").to_string();
        self.close_assistant();
        self.open_user = None;
        let ms = self.ms(e);
        match self.generation {
            Gen::V2 => {
                let id = self.new_msg_id(ms, e);
                self.push_rendered(json!({"id": id, "time": {"created": ms}, "type": "compaction", "status": "completed", "reason": trigger, "summary": summary, "recent": ""}), e);
            }
            Gen::V1 => {
                // A user message holding the compaction part, answered by a summary message.
                let uid = self.new_msg_id(ms, e);
                let (p, m) = self.model_ref();
                let agent = self.agent_name();
                let i = self.push_rendered(json!({"id": uid, "sessionID": self.sid, "role": "user", "time": {"created": ms}, "agent": agent, "model": {"providerID": p, "modelID": m}}), e);
                let pid = self.new_part_id(ms, e, 0);
                self.messages[i].parts.push(OutItem {
                    value: json!({"id": pid, "sessionID": self.sid, "messageID": uid, "type": "compaction", "auto": trigger == "auto"}),
                    native: None,
                });
                let aid = self.msg_ids.next("msg", ms, &format!("{}/{}#summary", self.l.id(), e.id));
                let cwd = self.cwd.clone().unwrap_or_else(|| "/".into());
                let a = json!({
                    "id": aid, "sessionID": self.sid, "role": "assistant", "time": {"created": ms, "completed": ms},
                    "parentID": uid, "modelID": m, "providerID": p, "mode": "compaction", "agent": "compaction", "summary": true,
                    "path": {"cwd": cwd, "root": cwd}, "cost": 0,
                    "tokens": {"input": 0, "output": 0, "reasoning": 0, "cache": {"read": 0, "write": 0}},
                });
                // The summary message carries nothing of its own; the user message holds the event.
                let j = self.messages.len();
                self.msg_index.insert(aid.clone(), j);
                self.last_seq += 1;
                self.messages.push(OutMsg { info: OutItem { value: a, native: None }, parts: vec![], seq: Some(self.last_seq) });
                self.carried.push(vec![]);
                self.own.push(vec![]);
                self.rendered.push(true);
                let tp = self.new_part_id(ms, e, 1);
                self.messages[j].parts.push(OutItem {
                    value: json!({"id": tp, "sessionID": self.sid, "messageID": aid, "type": "text", "text": summary, "time": {"start": ms, "end": ms}}),
                    native: None,
                });
                // 1.19's schema decode drops fields a compaction part cannot hold, so the
                // events ride on the summary's text part, covering the user message too.
                let moved = std::mem::take(&mut self.carried[i]);
                self.carried[j] = moved;
                self.covers.insert(j, uid);
                self.covered.insert(i);
                self.cur = Some(j);
            }
        }
    }

    fn turn(&mut self, e: &Event) {
        let phase = e.str("phase").unwrap_or("end");
        if phase == "start" || self.generation == Gen::V1 {
            if self.generation == Gen::V1 && phase == "end" {
                self.loss("turn-dropped", Some(Kind::Turn), "1.x has no turn marker");
            }
            self.file_event(e);
            return;
        }
        let outcome = match e.str("outcome") {
            Some("error") => "failed",
            Some("interrupted") | Some("aborted") => "interrupted",
            _ => "succeeded",
        };
        self.close_assistant();
        self.open_user = None;
        let ms = self.ms(e);
        let id = self.new_msg_id(ms, e);
        let mut m = json!({"id": id, "time": {"created": ms}, "type": "idle", "outcome": outcome});
        if outcome == "failed"
            && let Some(r) = e.str("reason")
        {
            m["error"] = json!({"type": "unknown", "message": r});
        }
        self.push_rendered(m, e);
    }

    fn error(&mut self, e: &Event) {
        // The step that failed: the open one, or the last message if it is a step;
        // otherwise the request failed before any output, which opencode records as
        // an assistant step with no content and the error.
        let no_error = |b: &Self, i: usize| b.rendered[i] && b.messages[i].info.value.get("error").is_none();
        let last_msg = (0..self.messages.len()).rev().find(|i| !self.messages[*i].info.value.is_null());
        let is_step = |b: &Self, i: usize| match b.generation {
            Gen::V2 => s(&b.messages[i].info.value, "type") == Some("assistant"),
            Gen::V1 => s(&b.messages[i].info.value, "role") == Some("assistant") && b.messages[i].info.value.get("summary").is_none(),
        };
        let target = self.open_assistant.filter(|i| no_error(self, *i)).or(last_msg.filter(|i| is_step(self, *i) && no_error(self, *i)));
        let i = match target {
            Some(i) => {
                self.file_event(e);
                i
            }
            // 1.19's `opencode import` rewrites an assistant message with no parts,
            // so 1.x keeps such an error in the carrier only.
            None if self.generation == Gen::V1 => {
                self.file_event(e);
                self.loss("error-dropped", Some(Kind::Error), "an error with no assistant message to hold it");
                return;
            }
            None => {
                self.close_assistant();
                self.open_user = None;
                self.ensure_assistant(e)
            }
        };
        // A failed response is over: what follows is another step.
        if self.open_assistant == Some(i) {
            self.step_closed = true;
        }
        let msg = e.str("message").unwrap_or("").to_string();
        let kind = e.str("kind").unwrap_or("unknown").to_string();
        let m = &mut self.messages[i].info.value;
        match self.generation {
            Gen::V2 => {
                let mut err = json!({"type": kind, "message": msg});
                if let Some(st) = e.body.get("status").and_then(Value::as_i64).filter(|x| (100..600).contains(x)) {
                    err["status"] = json!(st);
                }
                m["error"] = err;
            }
            Gen::V1 => {
                m["error"] = json!({"name": "UnknownError", "data": {"message": msg}});
            }
        }
    }

    // ------------------------------------------------ finish

    fn finish(mut self) -> (OutSession, Vec<(String, String)>) {
        // Unsettled tools are reported once.
        for (_, at) in self.tools.clone() {
            let st = match self.generation {
                Gen::V2 => s(&self.messages[at.msg].info.value["content"][at.idx]["state"], "status").map(str::to_string),
                Gen::V1 => s(&self.messages[at.msg].parts[at.idx].value["state"], "status").map(str::to_string),
            };
            let interrupted = match self.generation {
                Gen::V2 => s(&self.messages[at.msg].info.value["content"][at.idx]["state"]["error"], "type") == Some("tool.interrupted"),
                Gen::V1 => s(&self.messages[at.msg].parts[at.idx].value["state"], "error") == Some("Tool execution was interrupted"),
            };
            if st.as_deref() == Some("error") && interrupted {
                self.losses.add(
                    "tool-call-unsettled",
                    Some(Kind::ToolCall),
                    "a tool call with no result is written as an interrupted error (opencode imports settled messages only)",
                );
            }
        }
        if self.boundary_pending.is_some() {
            self.losses.add("compaction-boundary-dropped", Some(Kind::Compaction), "a compaction boundary with no summary has no opencode message");
        }
        let generation = self.generation;
        // Finish assistant messages: a finish reason from the last content.
        for i in 0..self.messages.len() {
            if !self.rendered[i] {
                continue;
            }
            if generation == Gen::V2 && s(&self.messages[i].info.value, "type") == Some("assistant") {
                let last = self.messages[i].info.value["content"].as_array().and_then(|c| c.last()).and_then(|c| s(c, "type")).map(str::to_string);
                self.messages[i].info.value["finish"] = json!(if last.as_deref() == Some("tool") { "tool-calls" } else { "stop" });
            }
            if generation == Gen::V1 && s(&self.messages[i].info.value, "role") == Some("assistant") && self.messages[i].info.value.get("summary").is_none() {
                let last = self.messages[i].parts.last().and_then(|p| s(&p.value, "type")).map(str::to_string);
                self.messages[i].info.value["finish"] = json!(if last.as_deref() == Some("tool") { "tool-calls" } else { "stop" });
            }
        }
        // A session export holds settled messages in the shape the import endpoint
        // accepts; records moved there from a database are brought into line.
        if self.ctx.target == Container::Transfer {
            for i in 0..self.messages.len() {
                let from_db = self.messages[i].info.native.as_ref().is_some_and(|n| n.container != Container::Transfer);
                if !from_db {
                    continue;
                }
                let v = &mut self.messages[i].info.value;
                let settled = match s(v, "type") {
                    Some("assistant") => v.get("time").and_then(|t| t.get("completed")).is_some(),
                    Some("shell") | Some("compaction") => s(v, "status") != Some("running"),
                    _ => true,
                };
                if !settled {
                    self.losses.add("unsettled-message-dropped", None, "a message still running in the database is not part of a session export");
                    v.take();
                    continue;
                }
                if super::v2::settle_tools(v) {
                    self.messages[i].info.native = None;
                }
            }
        }
        // Events after the last message ride on it (or, with no message, on the session).
        let last = (0..self.messages.len()).rev().find(|i| !self.messages[*i].info.value.is_null());
        let leftover = if self.ctx.carry && last.is_some() { std::mem::take(&mut self.pending) } else { vec![] };
        // Session info.
        let (info, slug, version) = self.session_info();
        // Carriers.
        if self.ctx.carry {
            for i in 0..self.messages.len() {
                if self.covered.contains(&i) {
                    let m = &mut self.messages[i];
                    js_normalize(&mut m.info.value);
                    for p in &mut m.parts {
                        js_normalize(&mut p.value);
                    }
                    continue;
                }
                let post = if Some(i) == last { leftover.clone() } else { vec![] };
                let evs = if self.rendered[i] {
                    let mut v = std::mem::take(&mut self.carried[i]);
                    v.extend(post);
                    v
                } else {
                    // A native message carries only when foreign events must ride on
                    // it; it then carries its own events too and is written from its
                    // logical value.
                    if self.carried[i].is_empty() && post.is_empty() {
                        continue;
                    }
                    let mut v = std::mem::take(&mut self.carried[i]);
                    v.extend(std::mem::take(&mut self.own[i]));
                    v.extend(post);
                    let m = &mut self.messages[i];
                    m.info.native = None;
                    for p in &mut m.parts {
                        p.native = None;
                    }
                    v
                };
                let m = &mut self.messages[i];
                js_normalize(&mut m.info.value);
                for p in &mut m.parts {
                    js_normalize(&mut p.value);
                }
                match generation {
                    // An empty carrier still marks the message as rendered from the ledger.
                    Gen::V2 => {
                        let c = super::carrier_of(&evs, &carry::record_hash(&m.info.value));
                        let md = m.info.value.as_object_mut().unwrap().entry("metadata").or_insert(json!({}));
                        md.as_object_mut().unwrap().insert(carry::KEY.into(), c);
                    }
                    Gen::V1 => {
                        let mut parts: Vec<Value> = m.parts.iter().map(|p| p.value.clone()).collect();
                        let mut c = super::carrier_of(&evs, &super::v1::carrier_hash(&m.info.value, &parts));
                        if let Some(hd) = self.header.take() {
                            c["header"] = hd;
                        }
                        if let Some(u) = self.covers.get(&i) {
                            c["covers"] = json!([u]);
                        }
                        super::v1::attach_carrier(&mut m.info.value, &mut parts, c);
                        for (p, v) in m.parts.iter_mut().zip(parts) {
                            p.value = v;
                        }
                    }
                }
            }
        } else {
            for m in &mut self.messages {
                if m.info.native.is_none() {
                    js_normalize(&mut m.info.value);
                    for p in &mut m.parts {
                        js_normalize(&mut p.value);
                    }
                }
            }
        }
        let messages: Vec<OutMsg> = self.messages.into_iter().filter(|m| !m.info.value.is_null()).collect();
        let sess = OutSession {
            id: self.sid.clone(),
            src: self.src,
            extra: self.l.agent().source.as_ref().filter(|s| s.format == FORMAT).map(|s| s.extra.clone()).unwrap_or_default(),
            info,
            messages,
            extras: self.extras,
            slug,
            version,
            store: false,
        };
        (sess, self.placed)
    }

    fn session_info(&mut self) -> (OutItem, String, String) {
        let slug = format!("protoledger-{}", crate::util::short_id(&self.sid, 8));
        let version = self.ctx.version.clone();
        if let Some(mut n) = self.session_native.take() {
            let v = n.value.clone();
            let slug = s(&v, "slug").map(str::to_string).unwrap_or(slug);
            let version = s(&v, "version").map(str::to_string).unwrap_or(version);
            // A converted session (not byte-exact anyway) can hold events that
            // found no message to ride on.
            if self.ctx.carry && !self.pending.is_empty() {
                let leftover = std::mem::take(&mut self.pending);
                let mut c = json!({"v": SPEC_VERSION, "session": self.sid});
                super::carry_events_into(&mut c, &leftover);
                n.native = None;
                if let Some(o) = n.value.as_object_mut() {
                    o.entry("metadata").or_insert(json!({})).as_object_mut().unwrap().insert(HEADER_KEY.into(), c);
                }
            }
            return (n, slug, version);
        }
        let created = self.created_ms;
        let updated = self.last_ms.max(created);
        let parent =
            self.l.agent().parent.as_ref().and_then(|p| if self.ctx.set.get(&p.agent).is_some() { self.ctx.sessions.get(&p.agent).cloned() } else { None });
        if self.l.agent().parent.is_some() && parent.is_none() {
            self.losses.add("parent-outside-set", None, "a subagent whose parent is not exported is written as a top-level session");
        }
        for k in &self.l.agent().links {
            if k.rel != "forked-from" || self.generation == Gen::V1 {
                self.losses.add("link-dropped", None, "links other than forked-from (2.x) have no opencode field");
            }
        }
        // 2.x keeps the *current* agent, model and directory on the session (each
        // switch updates it, and records the previous value in its message); 1.x
        // keeps where the session started.
        let (agent, model, cwd) = match self.generation {
            // Running totals name the model they were billed to (the totals'
            // only model record in 2.x).
            Gen::V2 => {
                let billed = self.cumulative.as_ref().and_then(|c| c.str("model")).map(|m| {
                    let p = self.model.as_ref().filter(|(_, cur)| cur == m).map(|(p, _)| p.clone()).unwrap_or_else(|| provider_for(m).to_string());
                    (p, m.to_string())
                });
                (self.agent.clone(), billed.or(self.model.clone()), self.cwd.clone())
            }
            Gen::V1 => {
                (self.first_agent.clone().or(self.agent.clone()), self.first_model.clone().or(self.model.clone()), self.first_cwd.clone().or(self.cwd.clone()))
            }
        };
        let cwd = cwd.filter(|d| is_absolute(d)).unwrap_or_else(|| "/".into());
        let (mut cost, mut tin, mut tout, mut treas, mut tcr, mut tcw) = (0.0, 0.0, 0.0, 0.0, 0.0, 0.0);
        for m in &self.messages {
            let v = &m.info.value;
            let g = |p: &[&str]| crate::util::get(v, p).and_then(Value::as_f64).unwrap_or(0.0);
            cost += g(&["cost"]);
            tin += g(&["tokens", "input"]);
            tout += g(&["tokens", "output"]);
            treas += g(&["tokens", "reasoning"]);
            tcr += g(&["tokens", "cache", "read"]);
            tcw += g(&["tokens", "cache", "write"]);
        }
        // Header carrier: the agent header, and events that followed the last rendered message.
        let mut carrier = None;
        if self.ctx.carry {
            let leftover = std::mem::take(&mut self.pending);
            let mut c = json!({"v": SPEC_VERSION, "session": self.sid, "agent": serde_json::to_value(self.l.agent()).unwrap_or(Value::Null)});
            // The title as written, so only a later edit in opencode reads as new.
            let written = match self.generation {
                Gen::V2 => self.title.clone(),
                Gen::V1 => Some(self.title.clone().unwrap_or_else(|| format!("New session - {}", crate::util::ms_to_rfc3339(created)))),
            };
            if let Some(t) = written {
                c["title"] = json!(t);
            }
            if !leftover.is_empty() {
                super::carry_events_into(&mut c, &leftover);
            }
            js_normalize(&mut c["title"]);
            self.header = Some(c.clone());
            carrier = Some(c);
        } else if !self.pending.is_empty() {
            // Without carry these simply have no place.
        }
        let mut info = match self.generation {
            Gen::V2 => {
                let mut o = json!({"id": self.sid});
                if let Some(p) = &parent {
                    o["parentID"] = json!(p);
                }
                o["projectID"] = json!("global");
                if let Some(a) = &agent {
                    o["agent"] = json!(a);
                }
                if let Some((p, m)) = &model {
                    o["model"] = json!({"id": m, "providerID": if p.is_empty() { provider_for(m) } else { p }});
                    if let Some(v) = &self.variant {
                        o["model"]["variant"] = json!(v);
                    }
                }
                if let Some(c) = &self.cumulative {
                    // Stated totals win over the sum of what was rendered.
                    let g = |k: &str| c.body.get(k).and_then(Value::as_f64);
                    cost = g("cost_usd").unwrap_or(cost);
                    tin = g("input").unwrap_or(0.0);
                    treas = g("reasoning").unwrap_or(0.0);
                    // opencode's output excludes reasoning (SPEC §3.1.1 counts it in).
                    tout = (g("output").unwrap_or(0.0) - treas).max(0.0);
                    tcr = g("cache_read").unwrap_or(0.0);
                    tcw = g("cache_write").unwrap_or(0.0);
                }
                o["cost"] = js_num(cost);
                o["tokens"] =
                    json!({"input": js_num(tin), "output": js_num(tout), "reasoning": js_num(treas), "cache": {"read": js_num(tcr), "write": js_num(tcw)}});
                o["time"] = json!({"created": created, "updated": updated});
                if let Some(t) = &self.title {
                    o["title"] = json!(t);
                }
                o["location"] = json!({"directory": cwd});
                o
            }
            Gen::V1 => {
                let title = self.title.clone().unwrap_or_else(|| format!("New session - {}", crate::util::ms_to_rfc3339(created)));
                let mut o = json!({"id": self.sid, "slug": slug, "version": version, "projectID": "global", "directory": cwd});
                if let Some(p) = &parent {
                    o["parentID"] = json!(p);
                }
                o["title"] = json!(title);
                o["time"] = json!({"created": created, "updated": updated});
                o
            }
        };
        js_normalize(&mut info);
        // The carrier after the numbers are spelled as JavaScript spells them:
        // its events keep the ledger's spelling.
        if let Some(c) = carrier {
            info["metadata"] = json!({HEADER_KEY: c});
        }
        (OutItem { value: info, native: None }, slug, version)
    }
}
