//! protoledger → Codex rollout.
//!
//! Events that came from Codex carry their native line, which is written back
//! unchanged. Every other event is rendered from its neutral fields into the
//! records Codex itself writes, following the loader and the transcript
//! builder (cited inline), and Codex's own external-session importer
//! (`external-agent-migration/src/sessions/export.rs:80-162`) for the shape of
//! an imported thread.

use super::{
    DEFAULT_CLI_VERSION, FORMAT, ORIGINATOR, SESSION_INDEX, V_LEGACY, V_PAGINATED, V_TS, V_V0, attach_carrier, canonical_variant, codex_ts, is_uuid,
    zstd_decode, zstd_encode,
};
use crate::formats::{ExportOptions, Exported, Losses, Output};
use crate::ledger::{Ledger, LedgerSet};
use crate::model::{Agent, Event, Kind, Obj};
use crate::util::{parse_rfc3339_ms, uuid_for};
use anyhow::{Result, bail};
use serde_json::{Value, json};
use std::collections::{HashMap, HashSet};

pub(super) fn export(set: &LedgerSet, opts: &ExportOptions) -> Result<Exported> {
    let requested = match &opts.variant {
        Some(v) => Some(canonical_variant(v).ok_or_else(|| anyhow::anyhow!("codex: unknown variant {v:?}"))?),
        None => None,
    };
    let threads = ThreadTable::new(set);
    let mut output = Output::default();
    let mut losses = Losses::default();
    let mut variant_out = String::new();
    let mut index: Vec<IndexLine> = Vec::new();
    for l in set.iter() {
        let (path, bytes, variant) = render_ledger(l, set, &threads, requested, opts, &mut losses, &mut index)?;
        if output.files.contains_key(&path) {
            bail!("codex export: two agents map to the same rollout path {path}");
        }
        output.put(path, bytes);
        if variant_out.is_empty() || variant != V_LEGACY {
            variant_out = variant.to_string();
        }
    }
    if !index.is_empty() {
        // Codex's own lines in their original order, then the new names.
        index.sort_by_key(|l| match l {
            IndexLine::Native { offset, .. } => (0, *offset),
            IndexLine::Synth(_) => (1, 0),
        });
        let mut text = String::new();
        for l in index {
            let (IndexLine::Native { text: t, .. } | IndexLine::Synth(t)) = l;
            text.push_str(&t);
            text.push('\n');
        }
        output.put_append(SESSION_INDEX, text.into_bytes());
    }
    Ok(Exported { output, losses, variant: variant_out })
}

/// A line of `session_index.jsonl`.
enum IndexLine {
    /// Read from Codex, at this byte offset of its index.
    Native { offset: u64, text: String },
    /// A title rendered from a neutral event.
    Synth(String),
}

/// Agent id → Codex thread id (a UUID, `protocol/src/thread_id.rs:41`).
struct ThreadTable {
    ids: HashMap<String, String>,
}

impl ThreadTable {
    fn new(set: &LedgerSet) -> Self {
        let mut ids = HashMap::new();
        for l in set.iter() {
            ids.insert(l.id().to_string(), thread_id_of(l.agent()));
        }
        ThreadTable { ids }
    }
    fn get(&self, agent: &str) -> String {
        self.ids.get(agent).cloned().unwrap_or_else(|| if is_uuid(agent) { agent.to_string() } else { uuid_for(agent) })
    }
}

fn codex_source(a: &Agent) -> Option<&crate::model::Source> {
    a.source.as_ref().filter(|s| s.format == FORMAT)
}

fn thread_id_of(a: &Agent) -> String {
    if let Some(n) = codex_source(a).and_then(|s| s.native_id.as_deref()).filter(|n| is_uuid(n)) {
        return n.to_string();
    }
    if is_uuid(&a.id) { a.id.clone() } else { uuid_for(&a.id) }
}

fn is_codex_native(e: &Event) -> bool {
    e.native.as_ref().is_some_and(|n| n.format == FORMAT)
}

fn render_ledger(
    l: &Ledger,
    set: &LedgerSet,
    threads: &ThreadTable,
    requested: Option<&'static str>,
    opts: &ExportOptions,
    losses: &mut Losses,
    index: &mut Vec<IndexLine>,
) -> Result<(String, Vec<u8>, &'static str)> {
    let src = codex_source(l.agent());
    let has_natives = l.events.iter().any(is_codex_native);
    // A file imported from Codex keeps its own shape; a foreign ledger gets the
    // requested one (default legacy, the shape every Codex since 0.33 loads).
    let variant = match (src.and_then(|s| s.variant.as_deref()).and_then(canonical_variant), has_natives) {
        (Some(v), true) => v,
        _ => requested.unwrap_or(V_LEGACY),
    };
    let extra = src.map(|s| s.extra.clone()).unwrap_or_default();
    let extra = if src.is_none() {
        // A carried foreign header keeps the container facts under `codex`.
        l.agent().source.as_ref().and_then(|s| s.extra.get("codex")).and_then(Value::as_object).cloned().unwrap_or(extra)
    } else {
        extra
    };
    let thread = threads.get(l.id());
    let mut r = Renderer::new(l, set, threads, variant, opts, thread.clone());
    r.run(losses);
    index.append(&mut r.index);
    let text = r.finish(&extra, losses);
    let path = match extra.get("file").and_then(Value::as_str) {
        Some(f) if src.is_some() || l.agent().source.as_ref().is_some_and(|s| s.extra.contains_key("codex")) => f.to_string(),
        _ => default_path(l, &thread, variant),
    };
    let bytes = if extra.get("compression").and_then(Value::as_str) == Some("zstd") { compress(&text, &extra)? } else { text.into_bytes() };
    Ok((path, bytes, variant))
}

fn compress(text: &str, extra: &Obj) -> Result<Vec<u8>> {
    if let Some(frame) = extra.get("zstd_frame").and_then(Value::as_str) {
        use base64::Engine;
        if let Ok(bytes) = base64::engine::general_purpose::STANDARD.decode(frame)
            && zstd_decode(&bytes).ok().as_deref() == Some(text.as_bytes())
        {
            return Ok(bytes);
        }
    }
    zstd_encode(text.as_bytes())
}

/// `sessions/YYYY/MM/DD/rollout-YYYY-MM-DDThh-mm-ss-<thread>.jsonl`
/// (`rollout/src/recorder.rs:1739-1760`, `rollout_file_name.rs:72-86`). Codex
/// uses local time and parses it back as UTC; the ledger's UTC time is used.
fn default_path(l: &Ledger, thread: &str, variant: &str) -> String {
    let ms = start_ms(l);
    let ts = crate::util::ms_to_rfc3339(ms);
    let (date, time) = (&ts[0..10], &ts[11..19]);
    let (y, m, d) = (&date[0..4], &date[5..7], &date[8..10]);
    let stamp = format!("{date}T{}", time.replace(':', "-"));
    if variant == V_TS {
        return format!("sessions/rollout-{date}-{thread}.json");
    }
    format!("sessions/{y}/{m}/{d}/rollout-{stamp}-{thread}.jsonl")
}

fn start_ms(l: &Ledger) -> i64 {
    l.agent().created_at.as_deref().and_then(parse_rfc3339_ms).or_else(|| l.events.iter().find_map(|e| e.ts.as_deref().and_then(parse_rfc3339_ms))).unwrap_or(0)
}

// ---------------------------------------------------------------------------

enum Line {
    /// A native line, written back verbatim. The `Moved`, for a line named
    /// by where it stands in a projection that struck a record (SPEC
    /// §11.1), is written instead when it does not stand there.
    Native(String, Option<Moved>),
    /// A rendered record; `carry` is the events it stands for (empty for a
    /// line that only accompanies another, `None` when carry is off).
    Synth { value: Value, carry: Option<Vec<Event>>, agent: bool },
}

/// A native line's anchor (`@<offset>`, or `@items/<i>` in the JSON
/// document), and the line carrying its own events.
struct Moved {
    anchor: String,
    carried: String,
}

#[derive(Clone, Copy, PartialEq)]
enum CallKind {
    Function,
    Custom,
}

struct Renderer<'a> {
    l: &'a Ledger,
    set: &'a LedgerSet,
    threads: &'a ThreadTable,
    variant: &'static str,
    opts: &'a ExportOptions,
    thread: String,
    lines: Vec<Line>,
    /// ts-json: the session object, once known.
    ts_session: Option<Value>,
    /// The native session object's own text (SPEC §4), written back as is.
    ts_session_text: Option<String>,
    pending: Vec<Event>,
    last_ts: String,
    next_ordinal: u64,
    /// The turn being rendered and whether the renderer opened it itself.
    turn: Option<(String, bool)>,
    turn_count: usize,
    turn_started_ms: Option<i64>,
    last_agent_message: Option<String>,
    /// Index of the event being rendered.
    pos: usize,
    open_calls: Vec<(String, CallKind)>,
    known_calls: HashSet<String>,
    ledger_has_turns: bool,
    ledger_has_usage: bool,
    total: Usage,
    /// The last response's usage.
    last_usage: Option<Usage>,
    /// The turn the running per-turn usage belongs to, and that usage.
    turn_usage: (String, Usage),
    content_bytes: usize,
    model: Option<String>,
    self_path: String,
    /// `session_index.jsonl` lines for this thread.
    index: Vec<IndexLine>,
    /// group → how many model output items its events render into.
    item_counts: HashMap<String, usize>,
    header_carried: bool,
}

/// Neutral usage in Codex's terms (SPEC §3.1.1): `input` excludes cache reads
/// and writes, Codex's `input_tokens` includes them
/// (`codex-api/src/sse/responses.rs:128-141`).
#[derive(Default, Clone, Copy, PartialEq)]
struct Usage {
    /// Non-cached input.
    input: i64,
    cached: i64,
    cache_write: i64,
    output: i64,
    reasoning: i64,
    /// `cache_write_input_tokens` is `#[serde(default)]`: written only when stated.
    has_cache_write: bool,
}

impl Usage {
    fn input_tokens(&self) -> i64 {
        self.input + self.cached + self.cache_write
    }
    fn json(&self) -> Value {
        let mut m = serde_json::Map::new();
        m.insert("input_tokens".into(), json!(self.input_tokens()));
        m.insert("cached_input_tokens".into(), json!(self.cached));
        if self.has_cache_write {
            m.insert("cache_write_input_tokens".into(), json!(self.cache_write));
        }
        m.insert("output_tokens".into(), json!(self.output));
        m.insert("reasoning_output_tokens".into(), json!(self.reasoning));
        m.insert("total_tokens".into(), json!(self.input_tokens() + self.output));
        Value::Object(m)
    }
    /// A `TokenUsage` that reports no response, only a context estimate
    /// (the shape of Codex's `recompute_token_usage`, `core/src/session/mod.rs:5150-5170`).
    fn estimate_json(total: i64) -> Value {
        json!({
            "input_tokens": 0, "cached_input_tokens": 0, "output_tokens": 0,
            "reasoning_output_tokens": 0, "total_tokens": total,
        })
    }
    fn from_event(e: &Event) -> Usage {
        let n = |k: &str| e.body.get(k).and_then(Value::as_i64).unwrap_or(0);
        Usage {
            input: n("input"),
            cached: n("cache_read"),
            cache_write: n("cache_write"),
            output: n("output"),
            reasoning: n("reasoning"),
            has_cache_write: e.body.get("cache_write").is_some_and(|v| !v.is_null()),
        }
    }
    /// A Codex `TokenUsage` record.
    fn from_codex(u: &Value) -> Usage {
        let n = |k: &str| u.get(k).and_then(Value::as_i64).unwrap_or(0);
        Usage {
            input: (n("input_tokens") - n("cached_input_tokens") - n("cache_write_input_tokens")).max(0),
            cached: n("cached_input_tokens"),
            cache_write: n("cache_write_input_tokens"),
            output: n("output_tokens"),
            reasoning: n("reasoning_output_tokens"),
            has_cache_write: u.get("cache_write_input_tokens").is_some(),
        }
    }
    fn plus(&self, o: &Usage) -> Usage {
        Usage {
            input: self.input + o.input,
            cached: self.cached + o.cached,
            cache_write: self.cache_write + o.cache_write,
            output: self.output + o.output,
            reasoning: self.reasoning + o.reasoning,
            has_cache_write: self.has_cache_write || o.has_cache_write,
        }
    }
}

impl<'a> Renderer<'a> {
    fn new(l: &'a Ledger, set: &'a LedgerSet, threads: &'a ThreadTable, variant: &'static str, opts: &'a ExportOptions, thread: String) -> Self {
        let foreign: Vec<&Event> = l.events.iter().filter(|e| !is_codex_native(e)).collect();
        let self_path = agent_path(l, set);
        let item_counts = output_items_per_group(&foreign);
        Renderer {
            l,
            set,
            threads,
            variant,
            opts,
            thread,
            lines: Vec::new(),
            ts_session: None,
            ts_session_text: None,
            pending: Vec::new(),
            last_ts: l
                .agent()
                .created_at
                .as_deref()
                .and_then(codex_ts)
                .or_else(|| l.events.iter().find_map(|e| e.ts.as_deref().and_then(codex_ts)))
                .unwrap_or_else(|| super::UNKNOWN_TS.into()),
            next_ordinal: 0,
            turn: None,
            turn_count: 0,
            turn_started_ms: None,
            last_agent_message: None,
            pos: 0,
            open_calls: Vec::new(),
            known_calls: HashSet::new(),
            ledger_has_turns: foreign.iter().any(|e| e.kind == Kind::Turn),
            ledger_has_usage: l.events.iter().any(|e| e.kind == Kind::Usage),
            total: Usage::default(),
            last_usage: None,
            turn_usage: (String::new(), Usage::default()),
            content_bytes: 0,
            model: None,
            self_path,
            item_counts,
            index: Vec::new(),
            header_carried: false,
        }
    }

    fn legacy(&self) -> bool {
        self.variant == V_LEGACY
    }
    fn paginated(&self) -> bool {
        self.variant == V_PAGINATED
    }
    fn enveloped(&self) -> bool {
        self.legacy() || self.paginated()
    }

    /// Give a model output item its event's group as the item `id` when that
    /// group is this one item and is not shaped like a Responses API id: Codex
    /// keeps such an id on disk and strips it from requests
    /// (`core/src/client.rs:1098-1103`), so it names the source message without
    /// claiming to be an OpenAI item. A group of several items is named by the
    /// response's `token_usage_record` instead.
    fn with_item_id(&self, item: Value, e: &Event) -> Value {
        let Some(g) = e.group.as_deref() else {
            return item;
        };
        if g.is_empty() || super::import::is_api_item_id(g) || self.item_counts.get(g).copied() != Some(1) {
            return item;
        }
        let Value::Object(m) = item else { return item };
        let mut out = serde_json::Map::new();
        for (k, v) in m {
            let first = k == "type";
            out.insert(k, v);
            if first {
                out.insert("id".into(), json!(g));
            }
        }
        Value::Object(out)
    }

    // ---- line output ----

    fn ts_of(&mut self, e: Option<&Event>) -> String {
        if let Some(t) = e.and_then(|e| e.ts.as_deref()).and_then(codex_ts) {
            self.last_ts = t;
        }
        self.last_ts.clone()
    }

    /// Wrap a record in the envelope for this variant.
    fn envelope(&mut self, ty: &str, payload: Value, ts: String) -> Value {
        match self.variant {
            V_LEGACY => json!({"timestamp": ts, "type": ty, "payload": payload}),
            V_PAGINATED => {
                let o = self.next_ordinal;
                self.next_ordinal += 1;
                json!({"timestamp": ts, "ordinal": o, "type": ty, "payload": payload})
            }
            // v0 and ts-json hold bare items.
            _ => payload,
        }
    }

    /// Emit a rendered line standing for `events` (the carrier holds them).
    fn emit(&mut self, value: Value, events: &[Event]) {
        let carry = self.opts.carry.then(|| {
            let mut v: Vec<Event> = std::mem::take(&mut self.pending);
            v.extend(events.iter().cloned());
            v
        });
        let agent = self.opts.carry && !self.header_carried;
        self.header_carried |= agent;
        self.lines.push(Line::Synth { value, carry, agent });
    }

    /// Emit a line that only accompanies the previous one (a UI copy).
    fn emit_companion(&mut self, value: Value) {
        self.emit(value, &[]);
    }

    fn emit_native(&mut self, e: &Event) {
        let n = e.native.as_ref().unwrap();
        if let Some(rec) = &n.record_json() {
            if let Some(o) = rec.get("ordinal").and_then(Value::as_u64) {
                self.next_ordinal = o + 1;
            }
            if let Some(t) = rec.get("timestamp").and_then(Value::as_str).and_then(codex_ts) {
                self.last_ts = t;
            }
        }
        if self.variant == V_TS {
            // Items of the JSON document, as their own text; the session object is kept aside.
            let text = match (crate::util::current_raw(n), n.record_text()) {
                (Some(raw), _) => raw.to_string(),
                (None, Some(rec)) => rec.to_string(),
                (None, None) => "null".to_string(),
            };
            if n.id.as_deref() == Some("@session") {
                self.ts_session = n.record_json();
                self.ts_session_text = Some(text);
            } else {
                let moved = self.moved(n);
                self.lines.push(Line::Native(text, moved));
            }
            return;
        }
        let text = match (crate::util::current_raw(n), n.record_text()) {
            (Some(raw), _) => raw.to_string(),
            (None, Some(rec)) => rec.to_string(),
            (None, None) => String::new(),
        };
        let moved = self.moved(n);
        self.lines.push(Line::Native(text, moved));
    }

    /// SPEC §11.1: the events of a rollout line are named by where it stands
    /// (`@<offset>`, `@items/<i>`), and a projection that struck a record
    /// moves the lines behind it: read back where it lands, a line would
    /// name other events. With carry, such a line has a second spelling that
    /// carries its own events, which [`Renderer::finish`] writes when the
    /// line does not land at its anchor. Nothing else about it changes.
    fn moved(&self, n: &crate::model::Native) -> Option<Moved> {
        if !self.opts.carry || !self.l.has_struck() {
            return None;
        }
        let anchor = n.id.as_deref().filter(|a| a.starts_with('@'))?.to_string();
        let mut rec = n.record_json().filter(Value::is_object)?;
        let own: Vec<Event> = self.l.events.iter().filter(|x| x.native.as_ref().is_some_and(|m| m.format == n.format && m.id == n.id)).cloned().collect();
        attach_carrier(&mut rec, &own, None);
        Some(Moved { anchor, carried: serde_json::to_string(&rec).expect("json") })
    }

    // ---- the pass ----

    fn run(&mut self, losses: &mut Losses) {
        let events: Vec<Event> = self.l.events.clone();
        // A rollout imported from Codex brings its own first line (or, for a
        // fragment, none); anything else gets a session_meta.
        let from_codex = codex_source(self.l.agent()).is_some() && events.iter().any(is_codex_native);
        if !from_codex {
            self.session_meta(losses);
        }
        // SPEC §2.3: the active path is rendered. A rollback Codex itself
        // wrote (a native `thread_rolled_back`) is Codex's own record of the
        // branch, so the turns it drops are written back with it; any other
        // abandoned branch only rides along in a carrier.
        let on = self.l.on_path(|e| !is_codex_native(e));
        losses.add_n(
            "abandoned-branch-not-rendered",
            None,
            on.iter().filter(|o| !**o).count() as u64,
            "events on an abandoned branch (SPEC §2.3) are not part of the conversation; with carry they ride on the next line",
        );
        let mut i = 0;
        while i < events.len() {
            self.pos = i;
            let e = &events[i];
            if !on[i] {
                self.pending.push(e.clone());
                i += 1;
                continue;
            }
            if is_codex_native(e) {
                let n = e.native.as_ref().unwrap();
                if n.file.as_deref() == Some(SESSION_INDEX) {
                    let text = match (crate::util::current_raw(n), n.record_text()) {
                        (Some(raw), _) => raw.to_string(),
                        (None, Some(rec)) => rec.to_string(),
                        (None, None) => String::new(),
                    };
                    let offset = n.id.as_deref().and_then(|i| i.strip_prefix('@')).and_then(|o| o.parse().ok());
                    self.index.push(IndexLine::Native { offset: offset.unwrap_or(u64::MAX), text });
                    i += 1;
                    continue;
                }
                if n.part == 0 && (n.record.is_some() || n.raw.is_some()) {
                    self.close_synth_turn(losses);
                    self.note_native(e);
                    self.emit_native(e);
                }
                i += 1;
                continue;
            }
            // Consecutive text/media of one role and group make one message.
            let mut j = i + 1;
            if matches!(e.kind, Kind::Text | Kind::Media) && e.group.is_some() {
                while j < events.len()
                    && on[j]
                    && !is_codex_native(&events[j])
                    && matches!(events[j].kind, Kind::Text | Kind::Media)
                    && events[j].group == e.group
                    && events[j].str("role") == e.str("role")
                {
                    j += 1;
                }
            }
            let run: Vec<Event> = events[i..j].to_vec();
            self.render(&run, losses);
            i = j;
        }
        self.pos = events.len();
        self.close_synth_turn(losses);
    }

    /// Track the state a native line implies (calls, turns, model).
    fn note_native(&mut self, e: &Event) {
        match e.kind {
            Kind::ToolCall => {
                if let Some(c) = e.str("call_id") {
                    self.known_calls.insert(c.to_string());
                }
            }
            Kind::Turn if e.str("phase") == Some("start") => self.turn = e.turn.clone().map(|t| (t, false)),
            Kind::Turn => self.turn = None,
            Kind::Usage => {
                let rec = e.native.as_ref().and_then(|n| n.record_json());
                let rec = rec.as_ref();
                if let Some(t) = rec.and_then(|r| r.pointer("/payload/info/total_token_usage")) {
                    self.total = Usage::from_codex(t);
                }
                if let Some(l) = rec.and_then(|r| r.pointer("/payload/info/last_token_usage")) {
                    self.last_usage = Some(Usage::from_codex(l));
                }
            }
            _ => {}
        }
    }

    fn session_meta(&mut self, losses: &mut Losses) {
        let a = self.l.agent().clone();
        let ts = self.last_ts.clone();
        let settings = inherited_settings(self.l, self.set);
        let get = |k: &str| settings.get(k).cloned();
        let cwd = get("cwd").and_then(|v| v.as_str().map(str::to_string)).unwrap_or_else(|| super::UNKNOWN_CWD.into());
        let system = get("system_prompt").and_then(|v| v.as_str().map(str::to_string));
        let branch = get("git_branch").and_then(|v| v.as_str().map(str::to_string));
        if self.variant == V_V0 || self.variant == V_TS {
            let mut meta = json!({"id": self.thread, "timestamp": ts, "instructions": system});
            if self.variant == V_TS {
                meta = json!({"timestamp": ts, "id": self.thread, "instructions": system.unwrap_or_default()});
                // The carrier goes on the session object at finish().
                self.ts_session = Some(meta);
                return;
            }
            if let Some(b) = branch {
                meta["git"] = json!({"branch": b});
            }
            self.emit(meta, &[]);
            return;
        }
        let parent = a.parent.as_ref().filter(|p| p.relation == crate::model::Relation::Subagent);
        let forked = a.links.iter().find(|k| k.rel == "forked-from").and_then(|k| k.agent.clone());
        let root = root_thread(self.l, self.set, self.threads);
        let mut p = serde_json::Map::new();
        p.insert("session_id".into(), json!(root));
        p.insert("id".into(), json!(self.thread));
        if let Some(f) = &forked {
            p.insert("forked_from_id".into(), json!(self.threads.get(f)));
        }
        if let Some(par) = parent {
            p.insert("parent_thread_id".into(), json!(self.threads.get(&par.agent)));
        }
        p.insert("timestamp".into(), json!(ts));
        p.insert("cwd".into(), json!(cwd));
        p.insert("originator".into(), json!(ORIGINATOR));
        p.insert("cli_version".into(), json!(self.opts.target_version.clone().unwrap_or_else(|| DEFAULT_CLI_VERSION.into())));
        let agent = settings.get("agent").cloned().unwrap_or(Value::Null);
        let nick = agent.get("nickname").and_then(Value::as_str).map(str::to_string);
        let role = agent.get("role").and_then(Value::as_str).map(str::to_string);
        match parent {
            Some(par) => {
                // `SubAgentSource::ThreadSpawn` (`protocol.rs:3039-3053`).
                p.insert(
                    "source".into(),
                    json!({"subagent": {"thread_spawn": {
                        "parent_thread_id": self.threads.get(&par.agent),
                        "depth": depth(self.l, self.set),
                        "agent_path": self.self_path,
                        "agent_nickname": nick,
                        "agent_role": role,
                    }}}),
                );
                p.insert("thread_source".into(), json!("subagent"));
                if let Some(n) = &nick {
                    p.insert("agent_nickname".into(), json!(n));
                }
                if let Some(r) = &role {
                    p.insert("agent_role".into(), json!(r));
                }
                p.insert("agent_path".into(), json!(self.self_path));
            }
            None => {
                // `cli` is one of the INTERACTIVE_SESSION_SOURCES the resume picker lists
                // (`rollout/src/lib.rs:89-96`).
                p.insert("source".into(), json!("cli"));
                p.insert("thread_source".into(), json!("user"));
            }
        }
        // `model_provider` is the id of a provider in the user's Codex config. A
        // thread whose provider is unknown or not OpenAI's says nothing (null),
        // which the resume picker matches with the default provider
        // (`rollout/src/list.rs:958-963`); naming a foreign provider would hide
        // the thread, and naming OpenAI would be false.
        let prov = get("provider").and_then(|v| v.as_str().map(str::to_string));
        match prov.as_deref() {
            Some("openai") => {
                p.insert("model_provider".into(), json!("openai"));
            }
            Some(_) => {
                losses.add("provider-not-rendered", Some(Kind::Settings), "a provider Codex has no id for is left unset (model_provider: null)");
                p.insert("model_provider".into(), Value::Null);
            }
            None => {
                p.insert("model_provider".into(), Value::Null);
            }
        }
        p.insert(
            "base_instructions".into(),
            match system {
                Some(t) => json!({"text": t}),
                None => Value::Null,
            },
        );
        p.insert("history_mode".into(), json!(if self.paginated() { "paginated" } else { "legacy" }));
        let mut payload = Value::Object(p);
        if let Some(b) = branch {
            payload["git"] = json!({"branch": b});
        }
        if (a.links.iter().any(|k| k.rel != "forked-from") || !a.meta.is_empty()) && !self.opts.carry {
            losses.add("agent-meta-dropped", None, "header links and metadata have no place in session_meta");
        }
        let line = self.envelope("session_meta", payload, ts);
        self.emit(line, &[]);
    }

    // ---- turns ----

    fn open_synth_turn(&mut self, e: &Event) {
        self.turn_count += 1;
        let id = e.turn.clone().unwrap_or_else(|| uuid_for(&format!("{}/turn/{}", self.l.id(), self.turn_count)));
        self.start_turn(id, true, Some(e));
    }

    fn start_turn(&mut self, id: String, synth: bool, e: Option<&Event>) {
        let ts = self.ts_of(e);
        self.turn_started_ms = parse_rfc3339_ms(&ts);
        self.last_agent_message = None;
        if self.enveloped() {
            // `TurnStartedEvent` (`protocol.rs:2301-2324`).
            let mut p = json!({"type": "task_started", "turn_id": id});
            if let Some(ms) = self.turn_started_ms {
                p["started_at"] = json!(ms / 1000);
            }
            p["model_context_window"] = Value::Null;
            p["collaboration_mode_kind"] = json!("default");
            let line = self.envelope("event_msg", p, ts);
            if synth {
                self.emit_companion(line);
            } else {
                self.emit(line, &[e.unwrap().clone()]);
            }
        } else if let Some(e) = e.filter(|_| !synth) {
            self.pending.push(e.clone());
        }
        self.turn = Some((id, synth));
    }

    fn end_turn(&mut self, e: Option<&Event>, losses: &mut Losses) {
        let Some((id, synth)) = self.turn.take() else {
            if let Some(e) = e {
                self.pending.push(e.clone());
            }
            return;
        };
        self.abort_open_calls(losses);
        if synth && !self.ledger_has_usage && self.is_last_synth_turn() {
            self.seed_tokens();
        }
        let ts = self.ts_of(e);
        if !self.enveloped() {
            if let Some(e) = e {
                self.pending.push(e.clone());
            }
            return;
        }
        let outcome = e.and_then(|e| e.str("outcome")).unwrap_or("ok");
        let end_ms = parse_rfc3339_ms(&ts);
        let line = if matches!(outcome, "interrupted" | "aborted") {
            // `TurnAbortedEvent` (`protocol.rs:4365-4389`).
            let reason = if outcome == "interrupted" { "interrupted" } else { "replaced" };
            let p = json!({"type": "turn_aborted", "turn_id": id, "reason": reason});
            self.envelope("event_msg", p, ts)
        } else {
            // `TurnCompleteEvent` (`protocol.rs:2271-2296`).
            let mut p = json!({"type": "task_complete", "turn_id": id, "last_agent_message": self.last_agent_message});
            if outcome == "error" {
                let msg = e.and_then(|e| e.str("reason")).unwrap_or("error");
                p["error"] = json!({"message": msg, "codex_error_info": null});
            }
            if let (Some(s), Some(t)) = (self.turn_started_ms, end_ms) {
                p["started_at"] = json!(s / 1000);
                p["completed_at"] = json!(t / 1000);
                p["duration_ms"] = e.and_then(|e| e.body.get("duration_ms").cloned()).unwrap_or(json!(t - s));
            }
            self.envelope("event_msg", p, ts)
        };
        match e {
            Some(e) if !synth || e.kind == Kind::Turn => self.emit(line, std::slice::from_ref(e)),
            _ => self.emit_companion(line),
        }
    }

    fn close_synth_turn(&mut self, losses: &mut Losses) {
        if matches!(self.turn, Some((_, true))) {
            self.end_turn(None, losses);
        }
    }

    /// The renderer's last synthesized turn: no user prompt (which would open
    /// another) is left to render. Token seeding happens there, once, for the
    /// whole thread, as Codex's importer does.
    fn is_last_synth_turn(&self) -> bool {
        !self.l.events[self.pos.min(self.l.events.len())..].iter().any(|e| !is_codex_native(e) && e.kind == Kind::Text && e.str("role") == Some("user"))
    }

    /// Codex's importer seeds context usage with `bytes / 4` of the imported
    /// text (`external-agent-migration/src/sessions/export.rs:131-150,195-207`,
    /// `utils/string/src/truncate.rs:96-100`), so a resumed thread knows how full
    /// its window is. Only written when the ledger has no usage of its own.
    fn seed_tokens(&mut self) {
        if !self.enveloped() || self.content_bytes == 0 {
            return;
        }
        let total = (self.content_bytes as i64 + 3) / 4;
        let u = Usage::estimate_json(total);
        let ts = self.last_ts.clone();
        let p = json!({"type": "token_count", "info": {
            "total_token_usage": u, "last_token_usage": u, "model_context_window": null
        }, "rate_limits": null});
        let line = self.envelope("event_msg", p, ts);
        self.emit_companion(line);
        self.content_bytes = 0;
    }

    /// Codex inserts an `aborted` output for a call without one when it
    /// rebuilds the prompt (`core/src/context_manager/normalize.rs:21-110`);
    /// writing it keeps every call paired in the file itself.
    fn abort_open_calls(&mut self, losses: &mut Losses) {
        let open = std::mem::take(&mut self.open_calls);
        for (call_id, kind) in open {
            // A function call left without output is a state Codex expects (an
            // interrupted turn): it adds the `aborted` output itself when it builds
            // the next prompt. A custom tool call without one is an invariant
            // violation there (`error_or_panic`), so that output is written.
            if kind != CallKind::Custom {
                continue;
            }
            losses.add("tool-output-synthesized", Some(Kind::ToolResult), "a custom tool call with no result got Codex's `aborted` output");
            let ts = self.last_ts.clone();
            let line = self.envelope("response_item", json!({"type": "custom_tool_call_output", "call_id": call_id, "output": "aborted"}), ts);
            self.emit_companion(line);
        }
    }

    fn ensure_turn(&mut self, e: &Event) {
        if self.turn.is_none() && self.enveloped() {
            self.open_synth_turn(e);
        }
    }

    // ---- foreign events ----

    fn render(&mut self, run: &[Event], losses: &mut Losses) {
        let e = &run[0];
        match e.kind {
            Kind::Text | Kind::Media => self.render_message(run, losses),
            Kind::Reasoning => self.render_reasoning(e, losses),
            Kind::ToolCall => self.render_call(e),
            Kind::ToolResult => self.render_result(e, losses),
            Kind::Compaction => self.render_compaction(e, losses),
            Kind::Turn => {
                if e.str("phase") == Some("start") {
                    self.close_synth_turn(losses);
                    let id = e.turn.clone().unwrap_or_else(|| uuid_for(&format!("{}/{}", self.l.id(), e.id)));
                    if self.turn.is_some() {
                        self.end_turn(None, losses);
                    }
                    self.start_turn(id, false, Some(e));
                } else {
                    self.end_turn(Some(e), losses);
                }
            }
            Kind::Usage => self.render_usage(e, losses),
            Kind::Settings => {
                if let Some(m) = e.str("model") {
                    self.model = Some(m.to_string());
                }
                // A turn_context would tell Codex the initial context was already
                // sent, and it would then never send its own (environment,
                // permissions, AGENTS.md) on resume: a missing reference context
                // item triggers full injection (`core/src/session/tests.rs:11166`,
                // `rollout_reconstruction.rs:30-52`). Codex's importer writes none either.
                losses.add("settings-not-rendered", Some(Kind::Settings), "settings are not written as turn_context (see docs)");
                self.pending.push(e.clone());
            }
            Kind::PeerMessage => self.render_peer(e, losses),
            Kind::Title => self.render_title(e, losses),
            Kind::Subagent => {
                losses.add("subagent-event-dropped", Some(Kind::Subagent), "the child is its own rollout with a thread_spawn source");
                self.pending.push(e.clone());
            }
            Kind::FileChange => self.render_file_change(e, losses),
            Kind::Error => {
                losses.add("error-dropped", Some(Kind::Error), "Codex does not persist error events");
                self.pending.push(e.clone());
            }
            Kind::App => {
                losses.add("app-dropped", Some(Kind::App), "");
                self.pending.push(e.clone());
            }
            Kind::Native => {
                losses.add("native-dropped", Some(Kind::Native), "a record native to another format");
                self.pending.push(e.clone());
            }
        }
    }

    fn render_message(&mut self, run: &[Event], losses: &mut Losses) {
        self.render_message_for(run, run, losses)
    }

    /// Render `run` as one message whose line carries `carried` (the events it
    /// stands for, which differ from `run` when a peer message is written as text).
    fn render_message_for(&mut self, run: &[Event], carried: &[Event], losses: &mut Losses) {
        let role = run[0].str("role").unwrap_or("user").to_string();
        if run[0].effective_audience() == crate::model::Audience::Record {
            return self.render_record_message(run, &role, losses);
        }
        let assistant = role == "assistant";
        let mut content = Vec::new();
        let mut text = String::new();
        let mut images = Vec::new();
        for e in run {
            match e.kind {
                Kind::Text => {
                    let t = e.str("text").unwrap_or("");
                    if !text.is_empty() {
                        text.push('\n');
                    }
                    text.push_str(t);
                    let ty = if assistant { "output_text" } else { "input_text" };
                    content.push(json!({"type": ty, "text": t}));
                }
                _ => match media_url(e) {
                    Some(u) if !assistant && e.str("media_type").is_none_or(|m| m.starts_with("image/")) => {
                        images.push(u.clone());
                        content.push(json!({"type": "input_image", "image_url": u}));
                    }
                    _ => losses.add("media-dropped", Some(Kind::Media), "only user images by data or URL have a Codex form"),
                },
            }
        }
        if content.is_empty() {
            self.pending.extend(run.iter().cloned());
            return;
        }
        self.content_bytes += text.len();
        let user = role == "user";
        if user {
            // Each user prompt starts a turn when the ledger has none of its own
            // (as Codex's importer does, `export.rs:94-131`).
            if matches!(self.turn, Some((_, true))) && !self.ledger_has_turns {
                self.end_turn(None, losses);
            }
        }
        if user || assistant {
            self.ensure_turn(&run[0]);
        }
        let ts = self.ts_of(Some(&run[0]));
        let role_out = if role == "system" || role == "developer" || user || assistant { role.as_str() } else { "user" };
        let mut msg = json!({"type": "message", "role": role_out, "content": content});
        if assistant {
            msg = self.with_item_id(msg, &run[0]);
        }
        if assistant && let Some(ph) = run[0].str("phase").filter(|p| matches!(*p, "commentary" | "final_answer" | "partial_answer")) {
            msg["phase"] = json!(ph);
        }
        let item = self.envelope("response_item", msg, ts.clone());
        self.emit(item, carried);
        // The transcript's copy follows the item, the order a live Codex session
        // records both (the message is recorded, then shown).
        if user
            && !is_injected(&run[0])
            && let Some(u) = self.ui_user(&text, &images, ts.clone())
        {
            self.emit_companion(u);
        }
        if assistant {
            self.last_agent_message = Some(text.clone());
            if let Some(u) = self.ui_agent(&text, ts) {
                self.emit_companion(u);
            }
        }
    }

    /// A message people saw but the model did not: only the transcript's copy
    /// (a user or agent message event), never a model-visible item.
    fn render_record_message(&mut self, run: &[Event], role: &str, losses: &mut Losses) {
        let texts: Vec<&str> = run.iter().filter(|e| e.kind == Kind::Text).filter_map(|e| e.str("text")).collect();
        let text = texts.join("\n");
        let ui = match role {
            "user" | "assistant" if !texts.is_empty() && run.iter().all(|e| e.kind == Kind::Text) => {
                if role == "user" {
                    self.ensure_turn(&run[0]);
                    let ts = self.ts_of(Some(&run[0]));
                    self.ui_user(&text, &[], ts)
                } else {
                    self.ensure_turn(&run[0]);
                    let ts = self.ts_of(Some(&run[0]));
                    self.ui_agent(&text, ts)
                }
            }
            _ => None,
        };
        match ui {
            Some(line) => self.emit(line, run),
            None => {
                losses.add("record-message-dropped", Some(Kind::Text), "a message the model did not see has no transcript-only form in Codex for this role");
                self.pending.extend(run.iter().cloned());
            }
        }
    }

    /// `EventMsg::UserMessage` (`protocol.rs:2669-2725`) or a paginated
    /// `ItemCompleted(UserMessage)` (`items.rs:83-89`).
    fn ui_user(&mut self, text: &str, images: &[String], ts: String) -> Option<Value> {
        if self.legacy() {
            let mut p = json!({"type": "user_message", "message": text});
            if !images.is_empty() {
                p["images"] = json!(images);
            }
            p["local_images"] = json!([]);
            p["local_audio"] = json!([]);
            p["text_elements"] = json!([]);
            Some(self.envelope("event_msg", p, ts))
        } else if self.paginated() {
            let mut content = vec![json!({"type": "text", "text": text, "text_elements": []})];
            for u in images {
                content.push(json!({"type": "image", "image_url": u}));
            }
            let item = json!({"type": "UserMessage", "id": uuid_for(&format!("{}/ui/{}", self.thread, self.lines.len())), "content": content});
            Some(self.item_completed(item, ts))
        } else {
            None
        }
    }

    /// `EventMsg::AgentMessage` (`protocol.rs:2646-2659`) or `ItemCompleted(AgentMessage)`.
    fn ui_agent(&mut self, text: &str, ts: String) -> Option<Value> {
        if self.legacy() {
            let p = json!({"type": "agent_message", "message": text, "phase": null, "memory_citation": null});
            Some(self.envelope("event_msg", p, ts))
        } else if self.paginated() {
            let item = json!({"type": "AgentMessage", "id": uuid_for(&format!("{}/ui/{}", self.thread, self.lines.len())), "content": [{"type": "Text", "text": text}]});
            Some(self.item_completed(item, ts))
        } else {
            None
        }
    }

    /// `ItemCompletedEvent` (`protocol.rs:2062-2075`).
    fn item_completed(&mut self, item: Value, ts: String) -> Value {
        let turn = self.turn.as_ref().map(|t| t.0.clone()).unwrap_or_default();
        let ms = parse_rfc3339_ms(&ts).unwrap_or(0);
        let p = json!({"type": "item_completed", "thread_id": self.thread, "turn_id": turn, "item": item, "completed_at_ms": ms});
        self.envelope("event_msg", p, ts)
    }

    fn render_reasoning(&mut self, e: &Event, losses: &mut Losses) {
        let summary: Vec<String> =
            e.body.get("summary").and_then(Value::as_array).map(|a| a.iter().filter_map(|s| s.as_str().map(str::to_string)).collect()).unwrap_or_default();
        let text = e.str("text").filter(|t| !t.is_empty()).map(str::to_string);
        let sealed = e.body.get("sealed");
        let openai = sealed
            .filter(|s| s.get("vendor").and_then(Value::as_str) == Some("openai") && s.get("kind").and_then(Value::as_str) == Some("encrypted"))
            .and_then(|s| s.get("data").and_then(Value::as_str))
            .map(str::to_string);
        if sealed.is_some() && openai.is_none() {
            losses.add("reasoning-sealed-dropped", Some(Kind::Reasoning), "another vendor's sealed reasoning cannot be replayed to OpenAI");
        }
        if summary.is_empty() && text.is_none() && openai.is_none() {
            self.pending.push(e.clone());
            return;
        }
        self.ensure_turn(e);
        let ts = self.ts_of(Some(e));
        // `ResponseItem::Reasoning` (`models.rs:1101-1113`): `encrypted_content` is
        // always serialised, null when there is none, never fabricated.
        let mut item = json!({
            "type": "reasoning",
            "summary": summary.iter().map(|s| json!({"type": "summary_text", "text": s})).collect::<Vec<_>>(),
        });
        if let Some(t) = &text {
            item["content"] = json!([{"type": "reasoning_text", "text": t}]);
            self.content_bytes += t.len();
        }
        item["encrypted_content"] = json!(openai);
        let item = self.with_item_id(item, e);
        let line = self.envelope("response_item", item, ts.clone());
        self.emit(line, std::slice::from_ref(e));
        if self.legacy() {
            for s in &summary {
                let l = self.envelope("event_msg", json!({"type": "agent_reasoning", "text": s}), ts.clone());
                self.emit_companion(l);
            }
            if let Some(t) = &text {
                let l = self.envelope("event_msg", json!({"type": "agent_reasoning_raw_content", "text": t}), ts.clone());
                self.emit_companion(l);
            }
        } else if self.paginated() {
            let item = json!({"type": "Reasoning", "id": uuid_for(&format!("{}/ui/{}", self.thread, self.lines.len())), "summary_text": summary, "raw_content": text.into_iter().collect::<Vec<_>>()});
            let l = self.item_completed(item, ts);
            self.emit_companion(l);
        }
    }

    fn render_call(&mut self, e: &Event) {
        self.ensure_turn(e);
        let ts = self.ts_of(Some(e));
        let mut call_id = e.str("call_id").unwrap_or("").to_string();
        if call_id.is_empty() {
            call_id = format!("call_{}", crate::util::short_id(&e.id, 24));
        }
        let name = e.str("name").unwrap_or("tool").to_string();
        let custom = e.str("tool_type") == Some("custom") || (e.body.get("input").is_none() && e.str("input_text").is_some());
        let item = if custom {
            let input = e
                .str("input_text")
                .map(str::to_string)
                .unwrap_or_else(|| e.body.get("input").map(|v| v.as_str().map(str::to_string).unwrap_or_else(|| v.to_string())).unwrap_or_default());
            self.content_bytes += input.len();
            // `ResponseItem::CustomToolCall` (`models.rs:1197-1215`).
            json!({"type": "custom_tool_call", "status": "completed", "call_id": call_id, "name": name, "input": input})
        } else {
            let args = match (e.body.get("input"), e.str("input_text")) {
                (Some(v), _) => serde_json::to_string(v).unwrap_or_default(),
                (None, Some(t)) => t.to_string(),
                (None, None) => "{}".into(),
            };
            self.content_bytes += args.len();
            // `ResponseItem::FunctionCall` (`models.rs:1127-1151`).
            let mut item = json!({"type": "function_call", "name": name, "arguments": args, "call_id": call_id});
            // An MCP tool is called in its server's namespace (`codex-mcp/src/mcp/mod.rs:80-86`).
            let ns = match (e.str("tool_type"), e.str("server"), e.str("namespace")) {
                (_, _, Some(ns)) => Some(ns.to_string()),
                (Some("mcp"), Some(server), None) if !name.starts_with("mcp__") => Some(format!("mcp__{server}__")),
                _ => None,
            };
            if let Some(ns) = ns {
                item["namespace"] = json!(ns);
            }
            item
        };
        let item = self.with_item_id(item, e);
        self.open_calls.push((call_id.clone(), if custom { CallKind::Custom } else { CallKind::Function }));
        self.known_calls.insert(call_id);
        let line = self.envelope("response_item", item, ts);
        self.emit(line, std::slice::from_ref(e));
    }

    fn render_result(&mut self, e: &Event, losses: &mut Losses) {
        let call_id = e.str("call_id").unwrap_or("").to_string();
        let pos = self.open_calls.iter().position(|(c, _)| *c == call_id);
        let kind = match pos {
            Some(p) => self.open_calls.remove(p).1,
            None if self.known_calls.contains(&call_id) => CallKind::Function,
            None => {
                // Codex drops an output with no call (`normalize.rs:171`).
                losses.add("tool-result-orphan-dropped", Some(Kind::ToolResult), "a result with no call in this thread");
                self.pending.push(e.clone());
                return;
            }
        };
        if e.body.get("is_error").and_then(Value::as_bool) == Some(true) {
            losses.add("tool-error-flag-dropped", Some(Kind::ToolResult), "Codex outputs carry no error flag on disk");
        }
        let ts = self.ts_of(Some(e));
        let output = match e.body.get("output") {
            Some(Value::String(t)) => {
                self.content_bytes += t.len();
                json!(t)
            }
            Some(Value::Array(parts)) => {
                let mut items = Vec::new();
                let mut all_text = true;
                for part in parts {
                    match part.get("type").and_then(Value::as_str) {
                        Some("text") => {
                            let t = part.get("text").and_then(Value::as_str).unwrap_or("");
                            self.content_bytes += t.len();
                            items.push(json!({"type": "input_text", "text": t}));
                        }
                        Some("media") => {
                            all_text = false;
                            let fake = Event { body: part.as_object().cloned().unwrap_or_default(), ..Event::new("", Kind::Media) };
                            match media_url(&fake) {
                                Some(u) => items.push(json!({"type": "input_image", "image_url": u})),
                                None => losses.add("media-dropped", Some(Kind::ToolResult), "a tool-result media part with no data or URL"),
                            }
                        }
                        _ => all_text = false,
                    }
                }
                // `FunctionCallOutputBody::ContentItems` (`models.rs:2319-2324`).
                let _ = all_text;
                json!(items)
            }
            Some(other) if !other.is_null() => json!(other.to_string()),
            _ => json!(""),
        };
        let ty = if kind == CallKind::Custom { "custom_tool_call_output" } else { "function_call_output" };
        let line = self.envelope("response_item", json!({"type": ty, "call_id": call_id, "output": output}), ts);
        self.emit(line, std::slice::from_ref(e));
    }

    fn render_compaction(&mut self, e: &Event, losses: &mut Losses) {
        let ts = self.ts_of(Some(e));
        if e.str("phase") == Some("boundary") {
            if self.legacy() {
                // `ContextCompactedEvent` is a unit struct (`protocol.rs:2268`).
                let line = self.envelope("event_msg", json!({"type": "context_compacted"}), ts);
                self.emit(line, std::slice::from_ref(e));
            } else {
                self.pending.push(e.clone());
            }
            return;
        }
        let sealed =
            e.body.get("sealed").filter(|s| s.get("vendor").and_then(Value::as_str) == Some("openai")).and_then(|s| s.get("data").and_then(Value::as_str));
        match (e.str("summary"), sealed) {
            (_, Some(enc)) if e.str("summary").is_none_or(str::is_empty) && self.enveloped() => {
                let line = self.envelope("response_item", json!({"type": "compaction", "encrypted_content": enc}), ts);
                self.emit(line, std::slice::from_ref(e));
            }
            (Some(sum), _) if self.enveloped() => {
                // `CompactedItem` (`history/src/lib.rs:289-310`); with no
                // replacement history, Codex rebuilds it from the user messages
                // and this summary on resume.
                if e.body.get("preserved").and_then(Value::as_array).is_some_and(|a| !a.is_empty()) {
                    losses.add("compaction-preserved-dropped", Some(Kind::Compaction), "Codex rebuilds the preserved prefix itself");
                }
                // `compaction_response_id` is the Responses API id of the request that
                // wrote the summary (`history/src/lib.rs:300-301`): the summary's group.
                let mut p = json!({"message": sum});
                if let Some(g) = &e.group {
                    p["compaction_response_id"] = json!(g);
                }
                let line = self.envelope("compacted", p, ts);
                self.emit(line, std::slice::from_ref(e));
            }
            _ => {
                losses.add("compaction-dropped", Some(Kind::Compaction), "this variant has no compaction record");
                self.pending.push(e.clone());
            }
        }
    }

    fn render_usage(&mut self, e: &Event, losses: &mut Losses) {
        if !self.enveloped() {
            self.pending.push(e.clone());
            return;
        }
        if e.body.get("cost_usd").is_some_and(|v| !v.is_null()) {
            losses.add("usage-cost-dropped", Some(Kind::Usage), "Codex token counts carry no cost");
        }
        if e.str("model").is_some() {
            losses.add("usage-model-dropped", Some(Kind::Usage), "Codex names the model only in turn_context, which is not written");
        }
        let u = Usage::from_event(e);
        let ts = self.ts_of(Some(e));
        let cw = e.body.get("context_window").cloned().unwrap_or(Value::Null);
        if e.body.get("cumulative").and_then(Value::as_bool) == Some(true) {
            // A running total (SPEC §3.1.1) is what `total_token_usage` states. It
            // reports no response of its own, so every `last_token_usage` count is
            // zero. A second line then restates the totals with the last response's
            // usage, the way Codex refreshes them, so that the latest token info a
            // resumed thread reads still knows how full its context is.
            self.total = u;
            let p = json!({"type": "token_count", "info": {
                "total_token_usage": u.json(), "last_token_usage": Usage::default().json(), "model_context_window": cw
            }, "rate_limits": null});
            let line = self.envelope("event_msg", p, ts.clone());
            self.emit(line, std::slice::from_ref(e));
            let last = match self.last_usage {
                Some(l) => Some(l.json()),
                None if self.content_bytes > 0 => Some(Usage::estimate_json((self.content_bytes as i64 + 3) / 4)),
                None => None,
            };
            if let Some(last) = last {
                let p = json!({"type": "token_count", "info": {
                    "total_token_usage": u.json(), "last_token_usage": last, "model_context_window": cw
                }, "rate_limits": null});
                let line = self.envelope("event_msg", p, ts);
                self.emit_companion(line);
            }
            return;
        }
        self.total = self.total.plus(&u);
        self.last_usage = Some(u);
        let mut events = vec![e.clone()];
        if let Some(group) = e.group.clone().filter(|_| self.writes_usage_records()) {
            // `TokenUsageRecord` (`protocol.rs:2389-2400`), one per response, whose
            // `response_id` is the response's id.
            let turn = self.turn.as_ref().map(|t| t.0.clone()).unwrap_or_default();
            let root_turn = turn.clone();
            let session = root_thread(self.l, self.set, self.threads);
            let turn_usage = if self.turn_usage.0 == turn { self.turn_usage.1.plus(&u) } else { u };
            self.turn_usage = (turn.clone(), turn_usage);
            let p = json!({
                "thread_id": self.thread, "turn_id": turn, "session_id": session, "root_turn_id": root_turn,
                "response_id": group, "usage": u.json(), "turn_token_usage": turn_usage.json(),
                "thread_token_usage": self.total.json(),
            });
            let line = self.envelope("token_usage_record", p, ts.clone());
            self.emit(line, &events);
            events.clear();
        }
        // `TokenCountEvent` / `TokenUsageInfo` (`protocol.rs:2391-2475`).
        let p = json!({"type": "token_count", "info": {
            "total_token_usage": self.total.json(), "last_token_usage": u.json(), "model_context_window": cw
        }, "rate_limits": null});
        let line = self.envelope("event_msg", p, ts);
        self.emit(line, &events);
    }

    /// `token_usage_record` lines exist from Codex 0.153 (the first release seen
    /// writing them); an older target would skip them as unknown.
    fn writes_usage_records(&self) -> bool {
        let v = self.opts.target_version.clone().unwrap_or_else(|| DEFAULT_CLI_VERSION.into());
        version_at_least(&v, &[0, 153, 0])
    }

    /// A file change is what Codex records when a patch is applied:
    /// `event_msg/patch_apply_end` in legacy rollouts (`protocol.rs:3859-3877`),
    /// an `item_completed` `FileChange` item in paginated ones
    /// (`rollout/src/policy.rs:117-140`). Only a change Codex's `FileChange`
    /// can state is written: a creation or deletion with its content, an edit
    /// with its diff.
    fn render_file_change(&mut self, e: &Event, losses: &mut Losses) {
        let change = match (e.str("op"), e.str("content"), e.str("diff")) {
            (Some("create"), Some(c), _) => Some(json!({"type": "add", "content": c})),
            (Some("delete"), Some(c), _) => Some(json!({"type": "delete", "content": c})),
            (Some("edit"), _, Some(d)) => {
                Some(json!({"type": "update", "unified_diff": d, "move_path": e.body.get("move_path").cloned().unwrap_or(Value::Null)}))
            }
            _ => None,
        };
        // A change Codex could state only in part (another field, a group: a
        // patch record belongs to no model message) is named as a loss rather
        // than written as a different fact.
        let stated: &[&str] = match e.str("op") {
            Some("edit") => &["path", "op", "diff", "move_path"],
            _ => &["path", "op", "content"],
        };
        let whole = e.group.is_none() && e.body.keys().all(|k| stated.contains(&k.as_str()));
        let (Some(change), Some(path), true, true) = (change, e.str("path"), self.enveloped(), whole) else {
            losses.add(
                "file-change-dropped",
                Some(Kind::FileChange),
                "a snapshot, or a change Codex's FileChange cannot state whole (content and diff, a group)",
            );
            self.pending.push(e.clone());
            return;
        };
        let ts = self.ts_of(Some(e));
        let mut changes = serde_json::Map::new();
        changes.insert(path.to_string(), change);
        // Not tied to a Codex patch call: its own id.
        let call_id = format!("patch_{}", uuid_for(&format!("{}/{}", self.l.id(), e.id)));
        let turn = self.turn.as_ref().map(|t| t.0.clone()).unwrap_or_default();
        let line = if self.legacy() {
            let p = json!({"type": "patch_apply_end", "call_id": call_id, "turn_id": turn, "stdout": "", "stderr": "",
                "success": true, "changes": changes, "status": "completed"});
            self.envelope("event_msg", p, ts)
        } else {
            let item = json!({"type": "FileChange", "id": call_id, "changes": changes, "status": "completed",
                "stdout": "", "stderr": ""});
            self.item_completed(item, ts)
        };
        self.emit(line, std::slice::from_ref(e));
    }

    /// A thread name is a `session_index.jsonl` line at the root of
    /// `CODEX_HOME` (`rollout/src/session_index.rs:20-50`); the newest wins. With
    /// carry the event rides the rollout's carrier (so it comes back in place)
    /// and the index line carries an empty one.
    fn render_title(&mut self, e: &Event, losses: &mut Losses) {
        let text = e.str("text").filter(|t| !t.trim().is_empty());
        match text {
            Some(t) if self.enveloped() => {
                let ts = self.ts_of(Some(e));
                let mut v = json!({"id": self.thread, "thread_name": t, "updated_at": ts});
                if self.opts.carry {
                    attach_carrier(&mut v, &[], None);
                }
                self.index.push(IndexLine::Synth(serde_json::to_string(&v).expect("json")));
            }
            _ => losses.add("title-dropped", Some(Kind::Title), "this variant predates session_index.jsonl"),
        }
        self.pending.push(e.clone());
    }

    fn render_peer(&mut self, e: &Event, losses: &mut Losses) {
        if !self.enveloped() {
            self.pending.push(e.clone());
            return;
        }
        let peer = e.str("peer").unwrap_or("");
        // Codex's own channel joins agents of one thread tree, named by agent
        // path, with no network. Any other peer is the SPEC §3.1.6 wrapper: the
        // incoming message as the user text the model was shown.
        let tree_path = (e.str("network").is_none()).then(|| tree_path_for(peer, self.l, self.set)).flatten();
        let Some(peer_path) = tree_path else {
            if e.str("direction") == Some("in") {
                if e.body.keys().any(|k| !matches!(k.as_str(), "direction" | "peer" | "network" | "text")) {
                    losses.add("peer-message-fields-dropped", Some(Kind::PeerMessage), "only peer, network and text fit the peer-message wrapper");
                }
                let text = super::peer_wrap(e.str("peer"), e.str("network"), e.str("text").unwrap_or(""));
                let mut t = e.clone();
                t.kind = Kind::Text;
                t.body = Obj::new();
                t.body.insert("role".into(), json!("user"));
                t.body.insert("text".into(), json!(text));
                t.audience = None;
                self.render_message_for(&[t], std::slice::from_ref(e), losses);
            } else {
                losses.add(
                    "peer-message-out-not-rendered",
                    Some(Kind::PeerMessage),
                    "a message sent to a peer outside the thread's agent tree has no Codex record of its own",
                );
                self.pending.push(e.clone());
            }
            return;
        };
        let (author, recipient) = if e.str("direction") == Some("out") { (self.self_path.clone(), peer_path) } else { (peer_path, self.self_path.clone()) };
        let ts = self.ts_of(Some(e));
        let text = e.str("text").unwrap_or("").to_string();
        self.content_bytes += text.len();
        // `InterAgentCommunication` (`protocol.rs:913-929`).
        let p = json!({"author": author, "recipient": recipient, "other_recipients": [], "content": text, "trigger_turn": false});
        let line = self.envelope("inter_agent_communication", p, ts);
        self.emit(line, std::slice::from_ref(e));
    }

    // ---- assembly ----

    fn finish(mut self, extra: &Obj, losses: &mut Losses) -> String {
        // Events with no line of their own ride on the last rendered line.
        if !self.pending.is_empty() && self.opts.carry {
            let pending = std::mem::take(&mut self.pending);
            match self.lines.iter_mut().rev().find_map(|l| match l {
                Line::Synth { carry: Some(c), .. } => Some(c),
                _ => None,
            }) {
                Some(c) => c.extend(pending),
                None if self.variant == V_TS || self.ts_session.is_some() => self.pending = pending,
                None => losses.add_n("event-not-carried", None, pending.len() as u64, "no rendered line to carry it"),
            }
        }
        let agent = self.l.agent().clone();
        let render = |v: &Value, carry: &Option<Vec<Event>>, with_agent: bool| -> Value {
            let mut v = v.clone();
            if let Some(evs) = carry {
                attach_carrier(&mut v, evs, with_agent.then_some(&agent));
            }
            v
        };
        if self.variant == V_TS {
            let mut session = self.ts_session.clone().unwrap_or_else(|| json!({}));
            let synth_session = !self.l.events.iter().any(|e| e.native.as_ref().is_some_and(|n| n.format == FORMAT && n.id.as_deref() == Some("@session")));
            // The document is assembled from its records' texts: a native record
            // exactly as held, a rendered one serialised; keys in Codex's order.
            let mut items: Vec<String> = Vec::new();
            let mut header_done = false;
            for l in &self.lines {
                match l {
                    Line::Native(t, moved) => match moved {
                        Some(m) if m.anchor != format!("@items/{}", items.len()) => items.push(m.carried.clone()),
                        _ => items.push(t.clone()),
                    },
                    Line::Synth { value, carry, agent } => {
                        items.push(serde_json::to_string(&render(value, carry, *agent && !synth_session)).expect("json"));
                        header_done |= *agent && !synth_session;
                    }
                }
            }
            if synth_session && self.opts.carry {
                let pending = std::mem::take(&mut self.pending);
                attach_carrier(&mut session, &pending, (!header_done).then_some(&agent));
            }
            let session_text = match (&self.ts_session_text, synth_session) {
                (Some(t), false) => t.clone(),
                _ => serde_json::to_string(&session).expect("json"),
            };
            let doc = format!("{{\"session\":{session_text},\"items\":[{}]}}", items.join(","));
            if let Some(raw) = extra.get("raw_text").and_then(Value::as_str)
                && serde_json::from_str::<Value>(raw).ok() == serde_json::from_str::<Value>(&doc).ok()
            {
                return raw.to_string();
            }
            let mut s = crate::util::pretty(&doc);
            if extra.get("final_newline").and_then(Value::as_bool) == Some(true) {
                s.push('\n');
            }
            return s;
        }
        let mut out = String::new();
        for l in &self.lines {
            match l {
                Line::Native(t, moved) => match moved {
                    Some(m) if m.anchor != format!("@{}", out.len()) => out.push_str(&m.carried),
                    _ => out.push_str(t),
                },
                Line::Synth { value, carry, agent } => out.push_str(&serde_json::to_string(&render(value, carry, *agent)).expect("json")),
            }
            out.push('\n');
        }
        if extra.get("final_newline").and_then(Value::as_bool) == Some(false) {
            out.pop();
        }
        out
    }
}

/// group → the number of model output items (assistant messages, reasoning,
/// calls) its foreign events render into; consecutive assistant text and media
/// of one group are one message.
fn output_items_per_group(events: &[&Event]) -> HashMap<String, usize> {
    let mut counts: HashMap<String, usize> = HashMap::new();
    let mut prev_msg: Option<&str> = None;
    for e in events {
        let g = e.group.as_deref();
        let assistant_part = matches!(e.kind, Kind::Text | Kind::Media) && e.str("role") == Some("assistant");
        match (e.kind, g) {
            (Kind::Text | Kind::Media, Some(g)) if assistant_part => {
                if prev_msg != Some(g) {
                    *counts.entry(g.to_string()).or_default() += 1;
                }
            }
            (Kind::Reasoning | Kind::ToolCall, Some(g)) => *counts.entry(g.to_string()).or_default() += 1,
            // Any other item of the group (a user message) rules out a per-item id.
            (Kind::Text | Kind::Media, Some(g)) => *counts.entry(g.to_string()).or_default() += 2,
            _ => {}
        }
        prev_msg = if assistant_part { g } else { None };
    }
    counts
}

/// `a.b.c` ≥ `want`, numerically; an unparseable version counts as current.
pub(super) fn version_at_least(v: &str, want: &[u64]) -> bool {
    let parts: Vec<u64> = v.split(['.', '-']).take(3).map_while(|p| p.parse().ok()).collect();
    if parts.is_empty() {
        return true;
    }
    for (i, w) in want.iter().enumerate() {
        let p = parts.get(i).copied().unwrap_or(0);
        if p != *w {
            return p > *w;
        }
    }
    true
}

fn is_injected(e: &Event) -> bool {
    e.meta.get("codex").and_then(|c| c.get("injected")).and_then(Value::as_bool) == Some(true)
}

fn media_url(e: &Event) -> Option<String> {
    if let Some(d) = e.str("data") {
        let mt = e.str("media_type").filter(|m| !m.ends_with("/*")).unwrap_or("image/png");
        return Some(format!("data:{mt};base64,{d}"));
    }
    e.str("url").map(str::to_string)
}

/// The settings in force at the start: the first settings event's fields,
/// later ones filling gaps.
fn first_settings(l: &Ledger) -> Obj {
    let mut out = Obj::new();
    for e in l.events.iter().filter(|e| e.kind == Kind::Settings) {
        for (k, v) in &e.body {
            out.entry(k.clone()).or_insert_with(|| v.clone());
        }
    }
    out
}

/// `first_settings`, with gaps filled from the structural parents (a subagent
/// runs in its parent's working directory unless it says otherwise).
fn inherited_settings(l: &Ledger, set: &LedgerSet) -> Obj {
    let mut out = first_settings(l);
    let mut cur = l;
    let mut seen = HashSet::new();
    while let Some(p) = cur.agent().parent.as_ref() {
        if !seen.insert(cur.id().to_string()) {
            break;
        }
        let Some(n) = set.get(&p.agent) else { break };
        for (k, v) in first_settings(n) {
            if matches!(k.as_str(), "cwd" | "git_branch" | "provider") {
                out.entry(k).or_insert(v);
            }
        }
        cur = n;
    }
    out
}

fn depth(l: &Ledger, set: &LedgerSet) -> i64 {
    let mut d = 0;
    let mut cur = l;
    let mut seen = HashSet::new();
    while let Some(p) = cur.agent().parent.as_ref() {
        d += 1;
        if !seen.insert(cur.id().to_string()) {
            break;
        }
        match set.get(&p.agent) {
            Some(n) => cur = n,
            None => break,
        }
    }
    d.max(1)
}

fn root_thread(l: &Ledger, set: &LedgerSet, threads: &ThreadTable) -> String {
    let mut cur = l;
    let mut seen = HashSet::new();
    while let Some(p) = cur.agent().parent.as_ref() {
        if !seen.insert(cur.id().to_string()) {
            break;
        }
        match set.get(&p.agent) {
            Some(n) => cur = n,
            None => return threads.get(&p.agent),
        }
    }
    threads.get(cur.id())
}

/// A valid Codex agent path (`protocol/src/agent_path.rs:125-170`): `/root`
/// for a root thread, `/root/<name>` below it, names lowercase `[a-z0-9_]`.
fn agent_path(l: &Ledger, set: &LedgerSet) -> String {
    if l.agent().parent.is_none() {
        return "/root".into();
    }
    let mut names = Vec::new();
    let mut cur = l;
    let mut seen = HashSet::new();
    while let Some(p) = cur.agent().parent.as_ref() {
        if !seen.insert(cur.id().to_string()) {
            break;
        }
        names.push(path_name(cur.id()));
        match set.get(&p.agent) {
            Some(n) => cur = n,
            None => break,
        }
    }
    names.reverse();
    format!("/root/{}", names.join("/"))
}

/// The agent path of `peer` when it is an agent of `l`'s own thread tree (an
/// agent path already, or a ledger of the set with the same root, other than
/// `l` itself).
fn tree_path_for(peer: &str, l: &Ledger, set: &LedgerSet) -> Option<String> {
    if peer == "/root" || peer.starts_with("/root/") {
        return Some(peer.to_string());
    }
    let other = set.get(peer).filter(|o| o.id() != l.id())?;
    (root_of(other, set) == root_of(l, set)).then(|| agent_path(other, set))
}

fn root_of<'a>(l: &'a Ledger, set: &'a LedgerSet) -> &'a str {
    let mut cur = l;
    let mut seen = HashSet::new();
    while let Some(p) = cur.agent().parent.as_ref() {
        if !seen.insert(cur.id().to_string()) {
            break;
        }
        match set.get(&p.agent) {
            Some(n) => cur = n,
            None => return p.agent.as_str(),
        }
    }
    cur.id()
}

fn path_name(id: &str) -> String {
    let mut s: String = id.chars().map(|c| if c.is_ascii_alphanumeric() { c.to_ascii_lowercase() } else { '_' }).collect();
    if s.is_empty() || s == "root" {
        s = format!("agent_{s}");
    }
    if s.chars().next().is_some_and(|c| c.is_ascii_digit()) {
        s = format!("a_{s}");
    }
    s
}

#[cfg(test)]
pub(super) fn agent_path_for_test(l: &Ledger, set: &LedgerSet) -> String {
    agent_path(l, set)
}
