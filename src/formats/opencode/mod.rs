//! opencode sessions (sst/opencode, now anomalyco/opencode).
//!
//! opencode has stored sessions in five shapes, each a [`Variant`]:
//!
//! | variant | producer versions | where |
//! |---|---|---|
//! | `v1-storage-json` | <= 1.1.x | `~/.local/share/opencode/storage/{session,message,part}/…/*.json` |
//! | `v1-sqlite` | 1.2.0 – 1.x | `~/.local/share/opencode/opencode.db`, tables `session`, `message`, `part` |
//! | `v1-export` | 1.x | `opencode export <id>`: `{info, messages:[{info, parts}]}` |
//! | `v2-sqlite` | >= 2.0.0 | `opencode.db`, tables `session_v2`, `session_message` |
//! | `v2-transfer` | >= 2.0.0 | `opencode session export <id>`: `SessionTransfer.Data {info, messages}` |
//!
//! Generation 1 (V1) splits a message into an info record and part records;
//! generation 2 (V2) stores one record per message with assistant `content[]`.
//! Both generations map to the same neutral kinds; see `docs/formats/opencode.md`.
//!
//! One opencode session is one ledger. A child session (`parentID`, made by the
//! `task` tool in V1 or the `subagent` tool in V2) is a child ledger with
//! `parent.relation = subagent`; a V2 fork (`fork{sessionID, boundary}`) is a
//! `forked-from` link. An `opencode.db`'s rows that belong to no session
//! are one more ledger, the store ledger ([`StoreSrc`]), which every session
//! ledger of the database names with a `member-of` link.

mod ids;
#[cfg_attr(not(feature = "sync"), allow(dead_code))] // read by the sync follower only
pub(crate) mod live;
#[cfg_attr(not(feature = "sqlite"), allow(dead_code))] // some fields are written to SQLite only
mod render;
#[cfg_attr(not(feature = "sqlite"), allow(dead_code, unused_imports))] // the SQLite store paths
mod store;
mod v1;
mod v2;

use crate::formats::{Detection, ExportOptions, Exported, Format, ImportOptions, Imported, Losses, Variant};
use crate::ledger::{Ledger, LedgerSet};
use crate::model::{Agent, Event, Kind, Link, Native, Obj, Parent, Relation};
use anyhow::{Result, bail};
use serde_json::Value;
use std::collections::{BTreeMap, HashMap};
use std::path::Path;

pub const FORMAT: &str = "opencode";

pub struct Opencode;

const VARIANTS: &[Variant] = &[
    Variant {
        name: "v1-storage-json",
        versions: "<= 1.1.x (JSON file tree; read by 1.2+ once, to migrate it into opencode.db)",
        import: true,
        export: true,
        notes: "storage/{session/<projectID>/<sessionID>,message/<sessionID>/<messageID>,part/<messageID>/<partID>}.json",
    },
    Variant {
        name: "v1-sqlite",
        versions: ">= 1.2.0, < 2.0 (1.x is still released, 1.19.3 on 2026-10-16)",
        import: true,
        export: true,
        notes: "opencode.db: session/message/part rows, message and part JSON in `data`",
    },
    Variant {
        name: "v1-export",
        versions: ">= 0.x, < 2.0 (`opencode export`, read by `opencode import`)",
        import: true,
        export: true,
        notes: "one JSON file per session: {info, messages:[{info, parts}]}",
    },
    Variant {
        name: "v2-sqlite",
        versions: ">= 2.0.0 (and the 2.0 betas)",
        import: true,
        export: true,
        notes: "opencode.db: session_v2/session_message rows; a standalone db, not a merge into a live one",
    },
    Variant {
        name: "v2-transfer",
        versions: ">= 2.0.0 (`opencode session export`, `POST /api/experimental/session/import`)",
        import: true,
        export: true,
        notes: "one JSON file per session: SessionTransfer.Data {info, messages}; the default export",
    },
];

/// Generation of the opencode data model.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum Gen {
    V1,
    V2,
}

/// The container a record was read from (one per variant).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum Container {
    Storage,
    SqliteV1,
    Export,
    SqliteV2,
    Transfer,
}

impl Container {
    pub fn variant(self) -> &'static str {
        match self {
            Container::Storage => "v1-storage-json",
            Container::SqliteV1 => "v1-sqlite",
            Container::Export => "v1-export",
            Container::SqliteV2 => "v2-sqlite",
            Container::Transfer => "v2-transfer",
        }
    }
    pub fn from_variant(v: &str) -> Option<Container> {
        Some(match v {
            "v1-storage-json" => Container::Storage,
            "v1-sqlite" => Container::SqliteV1,
            "v1-export" => Container::Export,
            "v2-sqlite" => Container::SqliteV2,
            "v2-transfer" => Container::Transfer,
            _ => return None,
        })
    }
    pub fn generation(self) -> Gen {
        match self {
            Container::Storage | Container::SqliteV1 | Container::Export => Gen::V1,
            Container::SqliteV2 | Container::Transfer => Gen::V2,
        }
    }
}

/// What a native record is.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum Role {
    Session,
    Message,
    Part,
    /// A record opencode keeps beside a session (project, todo, event_sequence,
    /// session_diff, the storage `migration` marker, ...): kept for exactness only.
    Extra,
}

/// One native record, as read from a container.
#[derive(Debug, Clone)]
pub(crate) struct Item {
    #[allow(dead_code)]
    pub role: Role,
    /// Native id (session, message or part id; a location anchor for extras).
    pub id: String,
    /// The logical opencode object (a V1 message info with its id, a V2 message
    /// with `id` and `type`, a session info), independent of the container.
    pub value: Value,
    /// The record exactly as the container holds it (file JSON, or a row object
    /// with the `data` column parsed).
    pub record: Value,
    /// Exact source text when `record` would not re-serialise to it (for a row,
    /// the `data` column's text; for a record inside an export document, its
    /// compact text).
    pub raw: Option<String>,
    /// The record's own compact text, spelled as the source spelled it, when
    /// the container has one (JSON files and documents): what the ledger holds.
    pub text: Option<String>,
    /// Locator: a path relative to the source root, or `<db file>:<table>`.
    pub file: String,
}

#[derive(Debug, Clone)]
pub(crate) struct MsgSrc {
    pub info: Item,
    /// V1 parts, in opencode's order (by part id). Empty for V2.
    pub parts: Vec<Item>,
}

#[derive(Debug, Clone)]
pub(crate) struct SessionSrc {
    pub session: Item,
    pub messages: Vec<MsgSrc>,
    pub extras: Vec<Item>,
    /// Container facts the exporter needs (file name, storage prefix, schema).
    pub extra: Obj,
}

pub(crate) struct SourceSet {
    #[allow(dead_code)]
    pub container: Container,
    pub sessions: Vec<SessionSrc>,
    /// An `opencode.db`'s rows that belong to no session: its store ledger.
    pub store: Option<StoreSrc>,
    pub losses: Losses,
}

/// The rows of an `opencode.db` that belong to no session (every project
/// row, worktrees, instruction blobs, an event log entry of no session, ...),
/// for the store's own ledger.
#[derive(Debug, Clone)]
pub(crate) struct StoreSrc {
    /// The store ledger's agent id ([`store_ledger_id`]).
    pub id: String,
    pub rows: Vec<Item>,
    /// Container facts the exporter needs (file name, schema, journals).
    pub extra: Obj,
}

/// The `source.extra` key that marks an opencode store ledger.
pub(crate) const STORE_KEY: &str = "store";

/// The agent id of the ledger holding an `opencode.db`'s session-less rows:
/// `opencode:store:<project id>:<time_created>` of the project opencode wrote
/// first (the smallest `time_created`, then the smallest id), or
/// `opencode:store` for a database with no project row. opencode writes a
/// project row the first time it opens a directory, before any session, and
/// never deletes it or rewrites its `time_created`; adding or removing
/// sessions, reordering rows or vacuuming the file leaves the id unchanged.
pub(crate) fn store_ledger_id(first_project: Option<(&str, i64)>) -> String {
    match first_project {
        Some((id, t)) => format!("opencode:store:{id}:{t}"),
        None => "opencode:store".to_string(),
    }
}

/// Is `l` an opencode store ledger (see [`StoreSrc`])?
pub(crate) fn is_store_ledger(l: &Ledger) -> bool {
    l.agent().source.as_ref().is_some_and(|s| s.format == FORMAT && s.extra.get(STORE_KEY).and_then(Value::as_bool) == Some(true))
}

// ---------------------------------------------------------------- helpers

/// Epoch milliseconds → RFC 3339.
pub(crate) fn ts(ms: Option<i64>) -> Option<String> {
    ms.map(crate::util::ms_to_rfc3339)
}

pub(crate) fn ms_of(v: &Value) -> Option<i64> {
    match v {
        Value::Number(n) => n.as_i64().or_else(|| n.as_f64().map(|f| f as i64)),
        _ => None,
    }
}

pub(crate) fn get_ms(v: &Value, path: &[&str]) -> Option<i64> {
    crate::util::get(v, path).and_then(ms_of)
}

/// A JSON number the way JavaScript would print it: integral values without a
/// fraction, so a value survives `JSON.parse`/`JSON.stringify` unchanged.
pub(crate) fn js_num(f: f64) -> Value {
    if f.is_finite() && f.fract() == 0.0 && f.abs() < 9.007_199_254_740_992e15 {
        Value::from(f as i64)
    } else {
        serde_json::Number::from_f64(f).map(Value::Number).unwrap_or(Value::Null)
    }
}

/// Rewrite every number in `v` into the form `JSON.stringify` would produce
/// (integral floats lose their fraction). opencode re-encodes what it imports,
/// and the carrier hash must survive that.
pub(crate) fn js_normalize(v: &mut Value) {
    match v {
        Value::Number(n) => {
            let s = n.to_string();
            if s.contains(['.', 'e', 'E'])
                && let Some(f) = n.as_f64()
            {
                *v = js_num(f);
            }
        }
        Value::Array(a) => a.iter_mut().for_each(js_normalize),
        Value::Object(o) => o.values_mut().for_each(js_normalize),
        _ => {}
    }
}

/// opencode's placeholder title for an untitled session (`New session - <ISO time>`,
/// `Child session - <ISO time>`): not a fact about the session.
pub(crate) fn is_default_title(t: &str) -> bool {
    ["New session - ", "Child session - "].iter().any(|p| t.strip_prefix(p).is_some_and(|rest| crate::util::parse_rfc3339_ms(rest).is_some()))
}

/// SPEC §3.1.6: a peer message in a harness without a peer channel is user
/// text wrapped as `<peer-message direction="in" peer="ID" network="NET">\nTEXT\n</peer-message>`.
pub(crate) fn peer_wrap(e: &Event) -> String {
    let mut open = String::from("<peer-message");
    for k in ["direction", "peer", "network"] {
        if let Some(v) = e.str(k) {
            open.push_str(&format!(" {k}=\"{v}\""));
        }
    }
    format!("{open}>\n{}\n</peer-message>", e.str("text").unwrap_or(""))
}

/// The `peer_message` a wrapped text stands for, if it is exactly one.
pub(crate) fn peer_unwrap(text: &str) -> Option<Event> {
    let rest = text.strip_prefix("<peer-message")?;
    let (attrs, body) = rest.split_once(">\n")?;
    let body = body.strip_suffix("\n</peer-message>")?;
    let mut e = Event::new("", Kind::PeerMessage);
    let mut a = attrs.trim();
    while !a.is_empty() {
        let (k, r) = a.split_once("=\"")?;
        let (v, r) = r.split_once('"')?;
        if !matches!(k, "direction" | "peer" | "network") {
            return None;
        }
        e = e.with(k, v);
        a = r.trim_start();
    }
    e.str("direction")?;
    Some(e.with("text", body))
}

pub(crate) fn s<'a>(v: &'a Value, k: &str) -> Option<&'a str> {
    v.get(k).and_then(Value::as_str)
}

fn native_for(item: &Item, container: Container, version: Option<&str>) -> Native {
    let n = Native::new(FORMAT, Some(item.id.clone())).variant(Some(container.variant())).version(version).file(Some(item.file.clone()));
    // The record as the source spelled it (SPEC §4), when the container gives its text.
    let mut n = match item.text.as_deref().map(|t| n.clone().record_raw(t)) {
        Some(Ok(held)) => held,
        _ => n.record(item.record.clone()),
    };
    n.raw = item.raw.clone();
    n
}

/// Give a record's events their ids (`<id>`, `<id>#1`, …) and natives (the
/// first carries the record).
pub(crate) fn stamp(events: Vec<Event>, item: &Item, container: Container, version: Option<&str>) -> Vec<Event> {
    let base = native_for(item, container, version);
    events
        .into_iter()
        .enumerate()
        .map(|(i, mut e)| {
            e.id = if i == 0 { item.id.clone() } else { format!("{}#{i}", item.id) };
            e.native = Some(if i == 0 { base.clone() } else { base.part_of(i as u32) });
            e
        })
        .collect()
}

// ---------------------------------------------------------------- Format

impl Format for Opencode {
    fn name(&self) -> &'static str {
        FORMAT
    }
    fn describe(&self) -> &'static str {
        "opencode sessions: 1.x JSON storage, 1.x/2.x opencode.db, `opencode export` and `opencode session export` files"
    }
    fn variants(&self) -> &'static [Variant] {
        VARIANTS
    }
    fn weight(&self) -> u8 {
        3
    }
    fn detect(&self, input: &Path) -> Option<Detection> {
        let (c, version) = store::detect(input)?;
        Some(Detection { variant: c.variant(), confidence: 90, version })
    }
    fn import(&self, input: &Path, opts: &ImportOptions) -> Result<Imported> {
        let container = match &opts.variant {
            Some(v) => Container::from_variant(v).ok_or_else(|| anyhow::anyhow!("opencode: unknown variant {v}"))?,
            None => match store::detect(input) {
                Some((c, _)) => c,
                None => bail!("{}: not an opencode session store or export", input.display()),
            },
        };
        let src = store::read(input, container)?;
        let mut losses = src.losses;
        let set = build_set(&src.sessions, src.store.as_ref(), container, opts, &mut losses);
        Ok(Imported { set, losses: losses.sorted(), variant: container.variant().to_string() })
    }
    fn export(&self, set: &LedgerSet, opts: &ExportOptions) -> Result<Exported> {
        let target = match &opts.variant {
            Some(v) => Container::from_variant(v).ok_or_else(|| anyhow::anyhow!("opencode: unknown variant {v}"))?,
            None => default_target(set),
        };
        let mut losses = Losses::default();
        let sessions = render::build(set, target, opts, &mut losses)?;
        let output = store::write(&sessions, target, &mut losses)?;
        Ok(Exported { output, losses: losses.sorted(), variant: target.variant().to_string() })
    }
}

/// Export back to the variant the ledgers came from; `v2-transfer` (what the
/// supported import endpoint takes) for anything else.
fn default_target(set: &LedgerSet) -> Container {
    set.iter()
        .filter_map(|l| l.agent().source.as_ref())
        .filter(|s| s.format == FORMAT)
        .find_map(|s| s.variant.as_deref().and_then(Container::from_variant))
        .unwrap_or(Container::Transfer)
}

// ---------------------------------------------------------------- import

/// The store ledger: one `native` event per row, in table and key order.
/// It has no parent; the sessions name it with a `member-of` link.
fn build_store_ledger(src: &StoreSrc, container: Container) -> Ledger {
    let mut extra = src.extra.clone();
    extra.insert(STORE_KEY.into(), Value::Bool(true));
    let agent = Agent {
        id: src.id.clone(),
        created_at: None,
        source: Some(crate::model::Source { format: FORMAT.into(), variant: Some(container.variant().into()), version: None, native_id: None, extra }),
        parent: None,
        links: Vec::new(),
        meta: Obj::new(),
    };
    let mut l = Ledger::new(agent);
    for x in &src.rows {
        let e = Event::new("", Kind::Native).ts(None::<String>);
        for e in stamp(vec![e], x, container, None) {
            l.append_unique(e);
        }
    }
    l
}

fn build_set(sessions: &[SessionSrc], store: Option<&StoreSrc>, container: Container, opts: &ImportOptions, losses: &mut Losses) -> LedgerSet {
    // (native session id, ledger)
    let mut built: Vec<(String, Ledger)> = Vec::new();
    // (session, child session) → the session's tool event that spawned the child.
    // Keyed by both: a fork copies its parent's tool calls, child links and all.
    let mut spawned: HashMap<(String, String), String> = HashMap::new();
    for src in sessions {
        let mut l = build_ledger(src, container, opts, losses);
        if let Some(st) = store
            && !l.agent().links.iter().any(|k| k.rel == "member-of" && k.agent.as_deref() == Some(st.id.as_str()))
        {
            l.agent_mut().links.push(Link { rel: "member-of".into(), agent: Some(st.id.clone()), event: None, extra: Obj::new() });
        }
        for e in &l.events {
            if e.kind == Kind::Subagent
                && let Some(child) = e.str("agent")
            {
                let call = e.id.split('#').next().unwrap_or(&e.id).to_string();
                spawned.entry((src.session.id.clone(), child.to_string())).or_insert(call);
            }
        }
        built.push((src.session.id.clone(), l));
    }
    let by_native: HashMap<String, String> = built.iter().map(|(n, l)| (n.clone(), l.id().to_string())).collect();
    let mut set = LedgerSet::new();
    if let Some(st) = store {
        set.insert(build_store_ledger(st, container));
    }
    for ((sid, mut l), src) in built.into_iter().zip(sessions) {
        if l.agent().parent.is_none()
            && let Some(p) = s(&src.session.value, "parentID")
        {
            let agent = by_native.get(p).cloned().unwrap_or_else(|| p.to_string());
            let event = spawned.get(&(p.to_string(), sid.clone())).cloned();
            l.agent_mut().parent = Some(Parent { agent, event, relation: Relation::Subagent });
        }
        set.insert(l);
    }
    // A parent event that is not in the parent ledger is dropped rather than left dangling.
    let ids: Vec<String> = set.ledgers.keys().cloned().collect();
    for id in ids {
        let dangling = match &set.ledgers[&id].agent().parent {
            Some(Parent { agent, event: Some(ev), .. }) => set.ledgers.get(agent).is_some_and(|p| !p.contains(ev)),
            _ => false,
        };
        if dangling && let Some(p) = set.ledgers.get_mut(&id).unwrap().agent_mut().parent.as_mut() {
            p.event = None;
        }
    }
    set
}

/// The session-level carrier: the protoledger header of the agent the session
/// was exported from, valid only for the session id it was written for (opencode
/// copies session metadata into child and forked sessions).
pub(crate) const HEADER_KEY: &str = "protoledger";

/// A carrier holding `events` under the hash `h`, built by [`crate::carry::attach`]:
/// a carried native record whose `Value` would re-spell it also has its exact
/// text under `"records"` (SPEC §6), so [`carried_events`] restores it byte for
/// byte. opencode hashes a message its own way (a 1.x message is its info and
/// all its parts), so the hash is the caller's. An empty carrier still marks a
/// message as rendered from the ledger.
pub(crate) fn carrier_of(events: &[Event], h: &str) -> Value {
    let mut scratch = serde_json::json!({});
    crate::carry::attach(&mut scratch, &[], events);
    let mut c = scratch
        .as_object_mut()
        .and_then(|m| m.remove(crate::carry::KEY))
        .unwrap_or_else(|| serde_json::json!({"v": crate::model::SPEC_VERSION, "h": "", "events": []}));
    c["h"] = serde_json::json!(h);
    c
}

/// Put `events` (and the exact texts of their native records) into the
/// session carrier `c`, which has no hash of its own.
pub(crate) fn carry_events_into(c: &mut Value, events: &[Event]) {
    let k = carrier_of(events, "");
    c["events"] = k["events"].clone();
    if let Some(r) = k.get("records") {
        c["records"] = r.clone();
    }
}

/// The events carrier `c` holds, each carried native record's exact text
/// restored from `"records"` ([`crate::carry::take`]). The caller has already
/// checked whatever hash `c` is bound to.
pub(crate) fn carried_events(c: &Value) -> Option<Vec<Event>> {
    let mut c = c.clone();
    c.as_object()?;
    let mut scratch = serde_json::json!({});
    c["h"] = serde_json::json!(crate::carry::record_hash(&scratch));
    scratch[crate::carry::KEY] = c;
    crate::carry::take(&mut scratch, &[])
}

pub(crate) fn header_carrier(info: &Value, generation: Gen) -> Option<&Value> {
    let c = match generation {
        Gen::V2 => info.get("metadata")?.get(HEADER_KEY)?,
        Gen::V1 => info.get(HEADER_KEY).or_else(|| info.get("metadata").and_then(|m| m.get(HEADER_KEY)))?,
    };
    (c.get("session").and_then(Value::as_str) == s(info, "id")).then_some(c)
}

fn build_ledger(src: &SessionSrc, container: Container, opts: &ImportOptions, losses: &mut Losses) -> Ledger {
    let generation = container.generation();
    let info = &src.session.value;
    let sid = src.session.id.clone();
    let version = s(info, "version").map(str::to_string);
    let created = get_ms(info, &["time", "created"]);
    let any_carrier = if !opts.restore_carried {
        None
    } else {
        header_carrier(info, generation).or_else(|| {
            // 1.x also stows it on the first rendered message (a 1.2 session row has no metadata column).
            (generation == Gen::V1)
                .then(|| src.messages.iter().find_map(|m| v1::find_carrier(m).and_then(|c| c.get("header"))))
                .flatten()
                .filter(|h| h.get("session").and_then(Value::as_str) == Some(sid.as_str()))
        })
    };
    // A carrier with an agent header means the session was rendered from a ledger;
    // one without only holds events that had no message to ride on.
    let carrier = any_carrier.filter(|c| c.get("agent").is_some());
    let restored_agent: Option<Agent> = carrier.and_then(|c| c.get("agent")).and_then(|a| serde_json::from_value(a.clone()).ok());

    let mut agent = Agent {
        id: sid.clone(),
        created_at: ts(created),
        source: Some(crate::model::Source {
            format: FORMAT.into(),
            variant: Some(container.variant().into()),
            version: version.clone(),
            native_id: Some(sid.clone()),
            extra: src.extra.clone(),
        }),
        parent: None,
        links: Vec::new(),
        meta: Obj::new(),
    };
    if let Some(fork) = info.get("fork")
        && let Some(from) = s(fork, "sessionID")
    {
        let mut extra = Obj::new();
        if let Some(b) = fork.get("boundary") {
            extra.insert("boundary".into(), b.clone());
        }
        agent.links.push(Link {
            rel: "forked-from".into(),
            agent: Some(from.to_string()),
            event: fork.get("boundary").and_then(|b| s(b, "messageID")).map(str::to_string),
            extra,
        });
    }
    if let Some(r) = restored_agent {
        // The session was rendered from a ledger: restore its header.
        // The whole header is restored (SPEC §6, header carry).
        agent = r;
    }
    let mut l = Ledger::new(agent);
    let ver = version.as_deref();

    // Session info: a settings snapshot (cwd, agent, model) and the title.
    let carried_title = carrier.and_then(|c| c.get("title")).and_then(Value::as_str);
    let mut sess_events = match generation {
        Gen::V1 => v1::session_events(info),
        Gen::V2 => v2::session_events(info, src.messages.iter().map(|m| &m.info.value)),
    };
    if carrier.is_some() {
        // Everything the session says was carried; only a title edited in opencode
        // since the export is a new fact.
        let title_now = s(info, "title");
        sess_events.retain(|e| e.kind == Kind::Title && title_now != carried_title && title_now.is_some());
    }
    // A staged revert (`session.revert`) is one more part of the session
    // record: the event that takes the conversation back (SPEC §2.3). It goes
    // after the messages it abandons.
    let revert = info.get("revert").filter(|r| s(r, "messageID").is_some());
    let marker_at = revert.map(|_| {
        sess_events.push(Event::new("", Kind::Native));
        sess_events.len() - 1
    });
    let mut stamped = stamp(sess_events, &src.session, container, ver);
    let mut revert_marker = marker_at.map(|i| stamped.remove(i));
    // Session totals (a cumulative usage) go after the messages they sum.
    let (session_totals, sess_events): (Vec<Event>, Vec<Event>) = stamped.into_iter().partition(|e| {
        e.kind == Kind::Usage && e.body.get("cumulative").and_then(Value::as_bool) == Some(true) && e.native.as_ref().is_some_and(|n| n.part > 0)
    });
    // Totals that are just the sum of the messages say nothing new.
    let session_totals: Vec<Event> =
        if generation == Gen::V2 && v2::totals_are_sum(info, src.messages.iter().map(|m| &m.info.value)) { Vec::new() } else { session_totals };
    for e in sess_events {
        l.append_unique(e);
    }
    // Extras are container bookkeeping (project row, event_sequence, ...); a
    // session rendered from a ledger regenerates them, so they are not events.
    for x in src.extras.iter().filter(|_| carrier.is_none()) {
        let e = Event::new("", Kind::Native).ts(None::<String>);
        for e in stamp(vec![e], x, container, ver) {
            l.append_unique(e);
        }
    }
    // Messages.
    let mut ctx = MapCtx::default();
    let restored_all: Vec<Option<Vec<Event>>> = src
        .messages
        .iter()
        .map(|m| {
            if !opts.restore_carried {
                return None;
            }
            match generation {
                Gen::V1 => v1::take_carrier(m),
                Gen::V2 => v2::take_carrier(&m.info.value),
            }
        })
        .collect();
    // A 1.x summary message's carrier also covers the compaction prompt before it.
    let covered: std::collections::HashSet<String> = src
        .messages
        .iter()
        .zip(&restored_all)
        .filter(|(_, r)| r.is_some())
        .filter_map(|(m, _)| v1::find_carrier(m).and_then(|c| c.get("covers")).and_then(Value::as_array).cloned())
        .flatten()
        .filter_map(|v| v.as_str().map(str::to_string))
        .collect();
    let last_ms = src.messages.iter().filter_map(|m| get_ms(&m.info.value, &["time", "created"])).max();
    // message id → the range of ledger events it made.
    let mut made: HashMap<String, (usize, usize)> = HashMap::new();
    for (m, restored) in src.messages.iter().zip(restored_all) {
        let start = l.events.len();
        made.insert(m.info.id.clone(), (start, start));
        if generation == Gen::V1 && covered.contains(&m.info.id) {
            ctx.observe_restored(&m.info.value, generation);
            continue;
        }
        if let Some(evs) = restored {
            for e in evs {
                l.append_unique(e);
            }
            made.insert(m.info.id.clone(), (start, l.events.len()));
            ctx.observe_restored(&m.info.value, generation);
            continue;
        }
        let evs = match generation {
            Gen::V1 => v1::message_events(m, &mut ctx, container, ver, losses),
            Gen::V2 => v2::message_events(&m.info, &mut ctx, container, ver),
        };
        for e in evs {
            l.append_unique(e);
        }
        made.insert(m.info.id.clone(), (start, l.events.len()));
    }
    if let (Some(r), Some(mut marker)) = (revert, revert_marker.take()) {
        marker.after = revert_point(&l, r, &made, generation);
        if marker.after.is_none() {
            losses.add(
                "revert-not-located",
                Some(Kind::Native),
                "session.revert names a message (or part) the session does not hold; the revert is kept in the session record only",
            );
        }
        l.append_unique(marker);
    }
    for mut e in session_totals {
        // Dated by the last message, not the session's `time.updated` (which an import rewrites).
        e.ts = ts(last_ms).or(e.ts);
        l.append_unique(e);
    }
    // Events carried with nothing to attach them to (they followed the last
    // rendered message) ride on the session carrier.
    if let Some(evs) = any_carrier.filter(|c| c.get("events").is_some()).and_then(carried_events) {
        for e in evs {
            l.append_unique(e);
        }
    }
    l
}

/// The event a staged revert takes the conversation back to: the one just
/// before the first reverted event. opencode reverts the message
/// `revert.messageID` and every later one (2.x `session/revert.ts` at
/// `180fd9d6`, committed by `projector.ts:759-778`: `seq >= boundary`); a 1.x
/// revert with a `partID` keeps that message's parts before the part
/// (1.2.21 `session/revert.ts`, `cleanup`). `None` when the boundary is not in
/// the ledger, or nothing precedes it.
fn revert_point(l: &Ledger, revert: &Value, made: &HashMap<String, (usize, usize)>, generation: Gen) -> Option<String> {
    let &(start, end) = made.get(s(revert, "messageID")?)?;
    let first = match s(revert, "partID").filter(|_| generation == Gen::V1) {
        Some(p) => start + l.events[start..end].iter().position(|e| e.native.as_ref().and_then(|n| n.id.as_deref()) == Some(p))?,
        None => start,
    };
    first.checked_sub(1).map(|i| l.events[i].id.clone())
}

/// State carried across a session's messages while mapping them.
#[derive(Debug, Default)]
pub(crate) struct MapCtx {
    /// Current (provider, model, agent) as last stated, to emit `settings` only on change.
    pub model: Option<(String, String)>,
    pub agent: Option<String>,
    /// Current turn id (the V1 user message / first V2 message after an idle marker).
    pub turn: Option<String>,
    pub turn_open: bool,
    /// The last V2 assistant step's finish reason.
    pub last_finish: Option<String>,
}

impl MapCtx {
    /// A message restored from its carrier still moves the mapping state on, so
    /// the next freshly mapped message compares against it.
    fn observe_restored(&mut self, msg: &Value, generation: Gen) {
        match generation {
            Gen::V1 => {
                if s(msg, "role") == Some("user") {
                    if let Some(mo) = msg.get("model") {
                        self.model = Some((s(mo, "providerID").unwrap_or("").to_string(), s(mo, "modelID").unwrap_or("").to_string()));
                    }
                    if let Some(a) = s(msg, "agent") {
                        self.agent = Some(a.to_string());
                    }
                    self.turn = s(msg, "id").map(str::to_string);
                }
            }
            Gen::V2 => match s(msg, "type") {
                Some("idle") => self.turn_open = false,
                Some("assistant") => self.last_finish = s(msg, "finish").map(str::to_string),
                Some("user") => self.last_finish = None,
                _ => {}
            },
        }
    }
}

/// Order ledgers so a parent precedes its children (SPEC §2.1), then by
/// creation time and id.
pub(crate) fn ordered(set: &LedgerSet) -> Vec<&Ledger> {
    let mut depth: BTreeMap<&str, usize> = BTreeMap::new();
    for l in set.iter() {
        let mut d = 0;
        let mut cur = l;
        let mut guard = 0;
        while let Some(p) = &cur.agent().parent {
            match set.get(&p.agent) {
                Some(next) if guard < 64 => {
                    d += 1;
                    cur = next;
                    guard += 1;
                }
                _ => break,
            }
        }
        depth.insert(l.id(), d);
    }
    let mut v: Vec<&Ledger> = set.iter().collect();
    v.sort_by(|a, b| {
        (depth[a.id()], a.agent().created_at.as_deref().map(crate::util::norm_ts), a.id()).cmp(&(
            depth[b.id()],
            b.agent().created_at.as_deref().map(crate::util::norm_ts),
            b.id(),
        ))
    });
    v
}

#[cfg(test)]
mod tests;
