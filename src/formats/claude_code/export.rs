//! protoledger → Claude Code.
//!
//! A native record whose every part is present is written back unchanged.
//! Anything else is rendered from its neutral `kind` and `body` into the
//! lines Claude Code itself writes (2.1.x file dialect), chained by
//! `parentUuid`, so `claude --resume <session>` loads it. With carry on, each
//! rendered line holds the events it stands for under `protoledger`, and
//! events with no line of their own ride on the next line (or the last).

use super::*;
use crate::carry;
use crate::formats::{Losses, Output};
use crate::ledger::Ledger;
use crate::model::{Event, Kind, Relation};
use crate::util::{norm_ts, short_id, uuid_for};
use base64::Engine;
use serde_json::{Map, json};
use std::collections::{BTreeMap, HashMap};

/// Export loss: events on an abandoned branch (SPEC §2.3) are not rendered.
const ABANDONED: &str = "abandoned-branch-not-rendered";
const ABANDONED_DETAIL: &str = "events on an abandoned branch (SPEC §2.3) are not part of the conversation; with carry they ride on the next line";

/// The stop reason written on the last line of a message of reasoning alone:
/// the ledger holds none, and the importer settles usage on it.
const SYNTHESISED_STOP: &str = "max_tokens";
/// Export loss: a stop reason the ledger did not hold was written. Kind
/// `usage`: it is written so that the usage settles there.
const STOP_SYNTHESISED: &str = "stop-reason-synthesised";
const STOP_SYNTHESISED_DETAIL: &str =
    "a message of reasoning alone ends on a line written with stop_reason max_tokens, which the ledger did not hold (it says only where the usage settles)";

/// Where one ledger goes, and as what.
#[derive(Debug, Clone)]
struct Plan {
    rel: String,
    sid: String,
    /// Subagent id (`agentId`), for sidechain ledgers.
    aid: Option<String>,
    sidechain: bool,
    /// An inline sidechain: (main file, line indexes there).
    inline: Option<(String, Vec<usize>)>,
    no_final_newline: bool,
    meta: Option<Vec<u8>>,
}

fn is_native(l: &Ledger) -> bool {
    l.agent().source.as_ref().is_some_and(|s| s.format == FORMAT)
}

fn extra<'a>(l: &'a Ledger, k: &str) -> Option<&'a Value> {
    l.agent().source.as_ref().and_then(|s| s.extra.get(k))
}

/// The structural parent within the set (subagents only; a fork is its own session).
fn parent_in<'a>(set: &'a LedgerSet, l: &Ledger) -> Option<&'a Ledger> {
    l.agent().parent.as_ref().filter(|p| p.relation == Relation::Subagent).and_then(|p| set.get(&p.agent))
}

fn session_id(l: &Ledger) -> String {
    if is_native(l)
        && extra(l, "inline").is_none()
        && !l.id().starts_with("agent-")
        && let Some(n) = l.agent().source.as_ref().and_then(|s| s.native_id.clone())
    {
        return n;
    }
    match uuid::Uuid::parse_str(l.id()) {
        Ok(u) => u.to_string(),
        Err(_) => uuid_for(l.id()),
    }
}

fn plan_all(set: &LedgerSet) -> BTreeMap<String, Plan> {
    let mut plans: BTreeMap<String, Plan> = BTreeMap::new();
    // Roots first, then children in depth order, so a parent's plan exists.
    let mut todo: Vec<&Ledger> = set.iter().collect();
    let depth = |l: &Ledger| {
        let mut d = 0;
        let mut cur = l;
        while let Some(p) = parent_in(set, cur) {
            d += 1;
            cur = p;
            if d > 64 {
                break;
            }
        }
        d
    };
    todo.sort_by_key(|l| (depth(l), l.id().to_string()));
    for l in todo {
        let parent = parent_in(set, l).and_then(|p| plans.get(p.id()).cloned());
        let native_file = if is_native(l) { extra(l, "file").and_then(Value::as_str).map(str::to_string) } else { None };
        let inline = extra(l, "inline").and_then(|v| {
            let f = s(v, "file")?.to_string();
            let idx = v.get("lines")?.as_array()?.iter().filter_map(Value::as_u64).map(|x| x as usize).collect();
            Some((f, idx))
        });
        let no_final_newline = extra(l, "no_final_newline").and_then(Value::as_bool).unwrap_or(false);
        let plan = if let Some((f, idx)) = inline.clone().filter(|_| is_native(l)) {
            let sid = parent.as_ref().map(|p| p.sid.clone()).unwrap_or_else(|| session_id(l));
            Plan { rel: f.clone(), sid, aid: None, sidechain: true, inline: Some((f, idx)), no_final_newline, meta: None }
        } else if let Some(rel) = native_file {
            let child = l.id().starts_with("agent-");
            let sid = if child {
                extra(l, "session")
                    .and_then(Value::as_str)
                    .map(str::to_string)
                    .or_else(|| parent.as_ref().map(|p| p.sid.clone()))
                    .unwrap_or_else(|| session_id(l))
            } else {
                session_id(l)
            };
            let aid = child.then(|| l.agent().source.as_ref().and_then(|s| s.native_id.clone()).unwrap_or_default());
            let meta = match (extra(l, "meta_raw").and_then(Value::as_str), extra(l, "meta")) {
                (Some(raw), _) => Some(raw.as_bytes().to_vec()),
                (None, Some(m)) => Some(serde_json::to_vec(m).unwrap()),
                _ => None,
            };
            Plan { rel, sid, aid, sidechain: child, inline: None, no_final_newline, meta }
        } else if let Some(p) = parent {
            // A foreign subagent: <session>/subagents/agent-<aid>.jsonl beside its session.
            let aid = format!("a{}", short_id(l.id(), 16));
            let dir = match p.rel.rfind("/subagents/") {
                Some(i) => p.rel[..i].to_string(),
                None => p.rel.trim_end_matches(".jsonl").to_string(),
            };
            let rel = format!("{dir}/subagents/agent-{aid}.jsonl");
            let meta = foreign_meta(set, l, &aid, depth(l));
            Plan { rel, sid: p.sid.clone(), aid: Some(aid), sidechain: true, inline: None, no_final_newline: false, meta: Some(meta) }
        } else {
            let sid = session_id(l);
            Plan { rel: format!("{sid}.jsonl"), sid, aid: None, sidechain: false, inline: None, no_final_newline: false, meta: None }
        };
        plans.insert(l.id().to_string(), plan);
    }
    plans
}

/// `agent-<aid>.meta.json` for a foreign subagent, from its parent's `subagent` start event.
fn foreign_meta(set: &LedgerSet, l: &Ledger, aid: &str, depth: usize) -> Vec<u8> {
    let p = l.agent().parent.as_ref().unwrap();
    let parent = set.get(&p.agent).unwrap();
    let start = parent.events.iter().find(|e| e.kind == Kind::Subagent && e.str("phase") == Some("start") && e.str("agent") == Some(l.id()));
    let call = start
        .and_then(|e| e.str("call_id").map(str::to_string))
        .or_else(|| p.event.as_ref().and_then(|ev| parent.events.iter().find(|e| &e.id == ev)).and_then(|e| e.str("call_id").map(str::to_string)));
    let mut m = Map::new();
    m.insert("agentType".into(), json!(start.and_then(|e| e.str("agent_type")).unwrap_or("general-purpose")));
    m.insert("description".into(), json!(start.and_then(|e| e.str("description")).unwrap_or("")));
    if let Some(c) = call {
        m.insert("toolUseId".into(), json!(tool_id(&c)));
    }
    m.insert("spawnDepth".into(), json!(depth));
    if depth > 1
        && let Some(pp) = parent_in(set, parent)
    {
        let _ = pp;
        m.insert("parentAgentId".into(), json!(format!("a{}", short_id(parent.id(), 16))));
    }
    let _ = aid;
    serde_json::to_vec(&Value::Object(m)).unwrap()
}

/// Claude's API takes tool ids matching `^[a-zA-Z0-9_-]+$`.
pub(super) fn tool_id(call: &str) -> String {
    if !call.is_empty() && call.len() <= 64 && call.chars().all(|c| c.is_ascii_alphanumeric() || c == '_' || c == '-') {
        call.to_string()
    } else {
        format!("toolu_{}", short_id(call, 24))
    }
}

fn tool_name(name: &str) -> String {
    let n: String = name.chars().map(|c| if c.is_ascii_alphanumeric() || c == '_' || c == '-' { c } else { '_' }).take(128).collect();
    if n.is_empty() { "tool".into() } else { n }
}

pub(super) fn export(set: &LedgerSet, opts: &ExportOptions) -> Result<Exported> {
    let plans = plan_all(set);
    let mut losses = Losses::default();
    let mut output = Output::default();
    // file → rendered lines of its own ledger; inline sidechain lines wait to be merged.
    let mut files: BTreeMap<String, (Vec<Rendered>, bool)> = BTreeMap::new();
    let mut inline: BTreeMap<String, Vec<(usize, Rendered)>> = BTreeMap::new();
    let mut inline_tail: BTreeMap<String, Vec<Rendered>> = BTreeMap::new();
    for l in set.iter() {
        let plan = &plans[l.id()];
        let lines = Renderer::new(l, plan, opts, &mut losses, &mut output).run();
        match &plan.inline {
            Some((f, idx)) => {
                for (k, text) in lines.into_iter().enumerate() {
                    match idx.get(k) {
                        Some(i) => inline.entry(f.clone()).or_default().push((*i, text)),
                        None => inline_tail.entry(f.clone()).or_default().push(text),
                    }
                }
            }
            None => {
                files.insert(plan.rel.clone(), (lines, plan.no_final_newline));
                if let Some(m) = &plan.meta {
                    output.put(format!("{}.meta.json", plan.rel.trim_end_matches(".jsonl")), m.clone());
                }
            }
        }
    }
    for (rel, (mut lines, no_nl)) in files {
        if let Some(mut side) = inline.remove(&rel) {
            side.sort_by_key(|(i, _)| *i);
            let total = lines.len() + side.len();
            let mut main = lines.into_iter();
            let mut side_it = side.into_iter().peekable();
            let mut merged = Vec::with_capacity(total);
            for k in 0..total {
                if side_it.peek().is_some_and(|(i, _)| *i == k) {
                    merged.push(side_it.next().unwrap().1);
                } else if let Some(m) = main.next() {
                    merged.push(m);
                } else if let Some((_, t)) = side_it.next() {
                    merged.push(t);
                }
            }
            merged.extend(main);
            merged.extend(side_it.map(|(_, t)| t));
            lines = merged;
        }
        if let Some(tail) = inline_tail.remove(&rel) {
            lines.extend(tail);
        }
        let text = assemble(&lines, !no_nl);
        output.put(rel, text.into_bytes());
    }
    let variant = set
        .roots()
        .find_map(|l| if is_native(l) { l.agent().source.as_ref().and_then(|s| s.variant.clone()) } else { None })
        .unwrap_or_else(|| V_SIDECAR.to_string());
    Ok(Exported { output, losses: losses.sorted(), variant })
}

// ---------------------------------------------------------------------------
// Rendering one ledger
// ---------------------------------------------------------------------------

/// One rendered line of a file. `moved`, for a native line named by its byte
/// offset in a projection that struck a record (SPEC §11.1): that offset
/// (`@<n>`), and the line carrying its own events, written instead when the
/// line does not land there.
struct Rendered {
    text: String,
    moved: Option<(String, String)>,
}

/// The text of a file of `lines`, each written where it lands (see
/// [`Rendered`]).
fn assemble(lines: &[Rendered], final_newline: bool) -> String {
    let mut text = String::new();
    for (i, r) in lines.iter().enumerate() {
        if i > 0 {
            text.push('\n');
        }
        let line = match &r.moved {
            Some((anchor, carried)) if *anchor != format!("@{}", text.len()) => carried,
            _ => &r.text,
        };
        text.push_str(line);
    }
    if final_newline && !lines.is_empty() {
        text.push('\n');
    }
    text
}

struct OutLine {
    value: Option<Value>,
    /// Exact text of a native record (or an unparsable line).
    raw: Option<String>,
    native: bool,
    own: Vec<Event>,
    pre: Vec<Event>,
    post: Vec<Event>,
}

struct Run {
    key: Option<String>,
    id: String,
    req: String,
    lines: Vec<usize>,
}

struct Renderer<'a> {
    l: &'a Ledger,
    plan: &'a Plan,
    carry: bool,
    losses: &'a mut Losses,
    output: &'a mut Output,
    version: String,
    /// `None` until a settings event (or a native line) states it: an unknown
    /// value is left out of the line rather than invented.
    cwd: Option<String>,
    branch: String,
    perm: Option<String>,
    model: Option<String>,
    prev: Option<String>,
    last_ts: String,
    lines: Vec<OutLine>,
    pending: Vec<Event>,
    /// native id → line index, for parts after part 0
    native_line: HashMap<String, usize>,
    /// call id → (tool id, assistant line uuid)
    tools: HashMap<String, (String, String)>,
    run: Option<Run>,
    last_assistant: bool,
    /// event id → the uuid of the last line written at or before it, kept
    /// only for a projection that struck native records (SPEC §11.1).
    prev_at: HashMap<String, Option<String>>,
}

impl<'a> Renderer<'a> {
    fn new(l: &'a Ledger, plan: &'a Plan, opts: &ExportOptions, losses: &'a mut Losses, output: &'a mut Output) -> Self {
        // Initial values come only from the foreign prefix (events before the
        // first native record), so a ledger that later gains native lines
        // renders its foreign lines exactly as it did before.
        let prefix: Vec<&Event> = l.events.iter().take_while(|e| !e.native.as_ref().is_some_and(|n| n.format == FORMAT)).collect();
        let first_setting = |k: &str| prefix.iter().filter(|e| e.kind == Kind::Settings).find_map(|e| e.str(k).map(str::to_string));
        let version = opts.target_version.clone().unwrap_or_else(|| DEFAULT_VERSION.to_string());
        let model = first_setting("model").or_else(|| prefix.iter().filter(|e| e.kind == Kind::Usage).find_map(|e| e.str("model").map(str::to_string)));
        Renderer {
            l,
            plan,
            carry: opts.carry,
            losses,
            output,
            version,
            cwd: first_setting("cwd"),
            branch: first_setting("git_branch").unwrap_or_default(),
            perm: None,
            model,
            prev: None,
            last_ts: l.agent().created_at.as_deref().map(norm_ts).unwrap_or_else(|| UNKNOWN_TS.into()),
            lines: vec![],
            pending: vec![],
            native_line: HashMap::new(),
            tools: HashMap::new(),
            run: None,
            last_assistant: false,
            prev_at: HashMap::new(),
        }
    }

    fn run(mut self) -> Vec<Rendered> {
        // SPEC §2.3: the active path is rendered. A rewind Claude Code itself
        // wrote (a native line whose `parentUuid` goes back) is its own
        // `parentUuid` tree, so its abandoned lines are written back too;
        // any other abandoned branch only rides along in a carrier.
        let on = self.l.on_path(|e| !e.native.as_ref().is_some_and(|n| n.format == FORMAT));
        let abandoned = on.iter().filter(|o| !**o).count() as u64;
        self.losses.add_n(ABANDONED, None, abandoned, ABANDONED_DETAIL);
        let gaps = self.l.has_struck();
        for (e, on) in self.l.events.iter().zip(on) {
            self.place(e, on);
            if gaps {
                self.prev_at.insert(e.id.clone(), self.prev.clone());
            }
        }
        self.finish()
    }

    fn place(&mut self, e: &Event, on: bool) {
        {
            if !on {
                self.defer(e);
                return;
            }
            if let Some(n) = e.native.as_ref().filter(|n| n.format == FORMAT) {
                // A sidecar file (tool-results/…, unknown files).
                if let Some(f) = n.file.as_ref().filter(|f| **f != self.plan.rel)
                    && n.part == 0
                    && (n.raw.is_some() || n.record.is_some())
                {
                    let bytes = match (crate::util::current_raw(n), &n.record_json()) {
                        (Some(r), _) => r.as_bytes().to_vec(),
                        (None, Some(rec)) => s(rec, "base64").and_then(|b| base64::engine::general_purpose::STANDARD.decode(b).ok()).unwrap_or_default(),
                        _ => vec![],
                    };
                    self.output.put(f.clone(), bytes);
                    return;
                }
                let id = n.id.clone().unwrap_or_default();
                if n.part == 0 && (n.raw.is_some() || n.record.is_some()) {
                    self.native_record(e, n);
                    return;
                }
                if let Some(&i) = self.native_line.get(&id) {
                    self.lines[i].own.push(e.clone());
                    return;
                }
            }
            self.foreign(e);
        }
    }

    fn native_record(&mut self, e: &Event, n: &crate::model::Native) {
        // `raw` is the line exactly as it is written back: the source text, or
        // the record's own text (SPEC §4) — never the record re-serialised.
        let (value, raw) = match (crate::util::current_raw(n), &n.record_json()) {
            (Some(r), rec) => (rec.clone().or_else(|| serde_json::from_str(r).ok()), Some(r.to_string())),
            (None, Some(rec)) if rec.get("base64").is_some() && rec.as_object().is_some_and(|o| o.len() == 1) => {
                // A line that was not UTF-8.
                let b = s(rec, "base64").and_then(|b| base64::engine::general_purpose::STANDARD.decode(b).ok()).unwrap_or_default();
                (None, Some(String::from_utf8_lossy(&b).into_owned()))
            }
            (None, Some(rec)) => (Some(rec.clone()), n.record_text().map(str::to_string)),
            _ => (None, None),
        };
        let (value, raw) = self.repoint(e, value, raw);
        if let Some(v) = &value {
            if let Some(u) = s(v, "uuid") {
                self.prev = Some(u.to_string());
            }
            if let Some(x) = s(v, "cwd") {
                self.cwd = Some(x.to_string());
            }
            if let Some(x) = s(v, "gitBranch") {
                self.branch = x.to_string();
            }
            if let Some(x) = s(v, "version") {
                self.version = x.to_string();
            }
            if let Some(x) = s(v, "timestamp") {
                self.last_ts = x.to_string();
            }
            if s(v, "type") == Some("assistant") {
                self.last_assistant = true;
            } else if s(v, "type") == Some("user") {
                self.last_assistant = false;
            }
        }
        // Events waiting for a line stay on the foreign line just before, if
        // the last line was one: that is where they rode before this native
        // line existed (a trailing carrier), so a re-export is unchanged.
        let pending = std::mem::take(&mut self.pending);
        let pre = match self.lines.last_mut() {
            Some(last) if !last.native && last.value.is_some() => {
                last.post.extend(pending);
                vec![]
            }
            _ => pending,
        };
        self.native_line.insert(n.id.clone().unwrap_or_default(), self.lines.len());
        self.lines.push(OutLine { value, raw, native: true, own: vec![e.clone()], pre, post: vec![] });
    }

    /// SPEC §11.1: a line written back whose `parentUuid` (or a compact
    /// boundary's `logicalParentUuid`) names a line the projection struck (a
    /// redacted line, or one no longer written intact) continues instead from
    /// the line that now holds its event's predecessor: the last line written
    /// before it, or, for an event that continues from an earlier one
    /// (`after`), the last line written at that one. That reference is the
    /// one change made to a line otherwise written back unchanged.
    fn repoint(&self, e: &Event, value: Option<Value>, raw: Option<String>) -> (Option<Value>, Option<String>) {
        let Some(mut v) = value else { return (None, raw) };
        if !self.l.has_struck() {
            return (Some(v), raw);
        }
        let mut raw = raw;
        let parent = match e.after.as_ref().and_then(|a| self.prev_at.get(a)) {
            Some(p) => p.clone(),
            None => self.prev.clone(),
        };
        for key in ["parentUuid", "logicalParentUuid"] {
            let Some(old) = s(&v, key).map(str::to_string) else { continue };
            if !self.l.struck(FORMAT, &old) {
                continue;
            }
            let new = parent.clone().map(Value::from).unwrap_or(Value::Null);
            raw = raw.and_then(|r| {
                let from = format!("\"{key}\":{}", json!(old));
                r.contains(&from).then(|| r.replacen(&from, &format!("\"{key}\":{new}"), 1))
            });
            v[key] = new;
        }
        let raw = raw.or_else(|| Some(serde_json::to_string(&v).unwrap()));
        (Some(v), raw)
    }

    fn finish(mut self) -> Vec<Rendered> {
        if !self.pending.is_empty() {
            let pending = std::mem::take(&mut self.pending);
            match self.lines.iter_mut().rev().find(|l| l.value.is_some()) {
                Some(last) if self.carry => last.post = pending,
                _ if self.carry => {
                    // Nothing to ride on: a line of its own, which Claude Code skips.
                    let v = json!({"type": "protoledger-carry", "sessionId": self.plan.sid});
                    self.lines.push(OutLine { value: Some(v), raw: None, native: false, own: vec![], pre: pending, post: vec![] });
                }
                _ => {}
            }
        }
        let carry = self.carry;
        let struck = self.l.has_struck();
        // The header rides on the first line when this format cannot express it.
        let mut agent = (carry && self.l.agent().source.as_ref().is_none_or(|s| s.format != FORMAT)).then(|| self.l.agent().clone());
        if agent.is_some() && self.lines.iter().all(|l| l.value.is_none()) {
            // An agent with no events still needs a line to carry its header.
            let v = json!({"type": "protoledger-carry", "sessionId": self.plan.sid});
            self.lines.push(OutLine { value: Some(v), raw: None, native: false, own: vec![], pre: vec![], post: vec![] });
        }
        self.lines
            .into_iter()
            .map(|l| {
                let carried: Vec<Event> = if carry && (!l.native || !l.pre.is_empty() || !l.post.is_empty()) {
                    l.pre.iter().chain(l.own.iter()).chain(l.post.iter()).cloned().collect()
                } else {
                    vec![]
                };
                // SPEC §11.1: a native line Claude Code wrote with no `uuid`
                // is named by its byte offset (`@<offset>`); behind a record
                // a projection struck, it may land at another offset, and
                // read back there would name other events. It is then
                // written carrying its own events (the export entry point's
                // file assembly decides, from where it lands).
                let moved = match (&l.value, l.own.first().and_then(|e| e.native.as_ref()).and_then(|n| n.id.as_deref())) {
                    (Some(v), Some(anchor)) if carry && struck && l.native && carried.is_empty() && anchor.starts_with('@') => {
                        let mut v = v.clone();
                        carry::attach_with(&mut v, &[], &l.own, None);
                        Some((anchor.to_string(), serde_json::to_string(&v).unwrap()))
                    }
                    _ => None,
                };
                let text = match (l.value, l.raw) {
                    (Some(mut v), _) if !carried.is_empty() || agent.is_some() => {
                        carry::attach_with(&mut v, &[], &carried, agent.take().as_ref());
                        serde_json::to_string(&v).unwrap()
                    }
                    (_, Some(r)) => r,
                    (Some(v), None) => serde_json::to_string(&v).unwrap(),
                    (None, None) => String::new(),
                };
                Rendered { text, moved }
            })
            .collect()
    }

    // ---- foreign events ----

    fn lose(&mut self, code: &str, kind: Kind, detail: &str) {
        self.losses.add(code, Some(kind), detail);
    }

    fn ts(&mut self, e: &Event) -> String {
        if let Some(t) = &e.ts {
            self.last_ts = norm_ts(t);
        }
        self.last_ts.clone()
    }

    /// Seed for minted ids: the session (and subagent) id, which survives a
    /// trip through Claude Code, unlike the ledger id.
    fn seed(&self) -> String {
        match &self.plan.aid {
            Some(a) => format!("{}/{a}", self.plan.sid),
            None => self.plan.sid.clone(),
        }
    }

    fn uuid(&self, e: &Event) -> String {
        uuid_for(&format!("{}/{}", self.seed(), e.id))
    }

    /// The fields every conversation line starts with.
    fn head(&self) -> Map<String, Value> {
        let mut m = Map::new();
        m.insert("parentUuid".into(), self.prev.clone().map(Value::from).unwrap_or(Value::Null));
        m.insert("isSidechain".into(), json!(self.plan.sidechain));
        if let Some(a) = &self.plan.aid {
            m.insert("agentId".into(), json!(a));
        }
        m
    }

    /// The environment fields every conversation line ends with.
    fn tail(&self, m: &mut Map<String, Value>) {
        m.insert("userType".into(), json!("external"));
        m.insert("entrypoint".into(), json!("cli"));
        if let Some(c) = &self.cwd {
            m.insert("cwd".into(), json!(c));
        }
        m.insert("sessionId".into(), json!(self.plan.sid));
        m.insert("version".into(), json!(self.version));
        m.insert("gitBranch".into(), json!(self.branch));
    }

    fn push(&mut self, e: &Event, v: Value) -> usize {
        if let Some(u) = s(&v, "uuid") {
            self.prev = Some(u.to_string());
        }
        let pre = std::mem::take(&mut self.pending);
        self.lines.push(OutLine { value: Some(v), raw: None, native: false, own: vec![e.clone()], pre, post: vec![] });
        self.lines.len() - 1
    }

    fn user_line(&mut self, e: &Event, content: Value, flags: &[(&str, Value)], extra_after: &[(&str, Value)]) -> usize {
        let uuid = self.uuid(e);
        let ts = self.ts(e);
        let mut m = self.head();
        if let Some(t) = &e.turn {
            m.insert("promptId".into(), json!(uuid_for(&format!("prompt:{}/{t}", self.seed()))));
        }
        m.insert("type".into(), json!("user"));
        m.insert("message".into(), json!({"role": "user", "content": content}));
        for (k, v) in flags {
            m.insert((*k).into(), v.clone());
        }
        m.insert("uuid".into(), json!(uuid));
        m.insert("timestamp".into(), json!(ts));
        for (k, v) in extra_after {
            m.insert((*k).into(), v.clone());
        }
        if let Some(p) = &self.perm {
            m.insert("permissionMode".into(), json!(p));
        }
        self.tail(&mut m);
        self.last_assistant = false;
        self.push(e, Value::Object(m))
    }

    fn assistant_line(&mut self, e: &Event, block: Value) -> usize {
        let reuse = match &self.run {
            Some(r) => (self.last_assistant && (e.group.is_none() || r.key == e.group)) || (e.group.is_some() && r.key == e.group),
            None => false,
        };
        if !reuse {
            let seed = format!("{}/{}", self.seed(), e.group.clone().unwrap_or_else(|| e.id.clone()));
            self.run = Some(Run {
                key: e.group.clone(),
                id: format!("msg_{}", short_id(&seed, 24)),
                req: format!("req_{}", short_id(&format!("req:{seed}"), 24)),
                lines: vec![],
            });
        }
        let (id, req) = {
            let r = self.run.as_ref().unwrap();
            (r.id.clone(), r.req.clone())
        };
        let uuid = self.uuid(e);
        let ts = self.ts(e);
        let mut m = self.head();
        m.insert(
            "message".into(),
            json!({
                "id": id,
                "type": "message",
                "role": "assistant",
                "content": [block],
                "stop_reason": null,
                "stop_sequence": null,
                "usage": {"input_tokens": 0, "output_tokens": 0},
            }),
        );
        if let Some(model) = &self.model {
            // Claude Code writes `model` first in `message`.
            let msg = m["message"].as_object_mut().unwrap();
            let rest = std::mem::take(msg);
            msg.insert("model".into(), json!(model));
            msg.extend(rest);
        }
        m.insert("requestId".into(), json!(req));
        m.insert("type".into(), json!("assistant"));
        m.insert("uuid".into(), json!(uuid));
        m.insert("timestamp".into(), json!(ts));
        self.tail(&mut m);
        self.last_assistant = true;
        let i = self.push(e, Value::Object(m));
        self.run.as_mut().unwrap().lines.push(i);
        i
    }

    fn system_line(&mut self, e: &Event, subtype: &str, fields: Vec<(&str, Value)>, level: Option<&str>) -> usize {
        let uuid = self.uuid(e);
        let ts = self.ts(e);
        let mut m = self.head();
        m.insert("type".into(), json!("system"));
        m.insert("subtype".into(), json!(subtype));
        for (k, v) in fields {
            m.insert(k.into(), v);
        }
        m.insert("isMeta".into(), json!(false));
        m.insert("timestamp".into(), json!(ts));
        m.insert("uuid".into(), json!(uuid));
        if let Some(l) = level {
            m.insert("level".into(), json!(l));
        }
        self.tail(&mut m);
        self.push(e, Value::Object(m))
    }

    fn defer(&mut self, e: &Event) {
        self.pending.push(e.clone());
    }

    fn foreign(&mut self, e: &Event) {
        match e.kind {
            Kind::Text => self.text(e),
            Kind::Media => self.media(e),
            Kind::Reasoning => self.reasoning(e),
            Kind::ToolCall => self.tool_call(e),
            Kind::ToolResult => self.tool_result(e),
            Kind::Compaction => self.compaction(e),
            Kind::Turn => match (e.str("phase"), e.body.get("duration_ms").filter(|d| d.is_number())) {
                (Some("end"), Some(d)) => {
                    let d = d.clone();
                    self.system_line(e, "turn_duration", vec![("durationMs", d)], None);
                    if e.body.get("outcome").is_some_and(|o| o != "ok") {
                        self.lose("turn-outcome-dropped", Kind::Turn, "turn_duration records no outcome");
                    }
                }
                _ => {
                    self.lose("turn-not-rendered", Kind::Turn, "Claude Code marks turns only by promptId and turn_duration lines");
                    self.defer(e);
                }
            },
            Kind::Usage => self.usage(e),
            Kind::Settings => self.settings(e),
            Kind::Title => {
                let v = json!({"type": "ai-title", "aiTitle": e.str("text").unwrap_or(""), "sessionId": self.plan.sid});
                self.push(e, v);
            }
            Kind::Subagent => self.defer(e), // structural: the child is its own sidecar file
            Kind::PeerMessage => {
                if e.str("direction") == Some("in") {
                    // SPEC §3.1.6: the wrapped user text the importer turns back into a peer message.
                    let text = peer_wrap(e.str("peer"), e.str("network"), e.str("text").unwrap_or(""));
                    self.user_line(e, json!(text), &[], &[]);
                    if e.body.keys().any(|k| !matches!(k.as_str(), "direction" | "peer" | "network" | "text")) {
                        self.lose("peer-message-fields-dropped", Kind::PeerMessage, "only peer, network and text fit the peer-message wrapper");
                    }
                } else {
                    self.lose("peer-message-out-not-rendered", Kind::PeerMessage, "an outgoing peer message has no Claude Code line");
                    self.defer(e);
                }
            }
            Kind::Error => {
                let msg = e.str("message").unwrap_or("").to_string();
                self.system_line(e, "informational", vec![("content", json!(msg))], Some("error"));
            }
            Kind::FileChange => {
                self.lose("file-change-not-rendered", Kind::FileChange, "Claude Code's file history needs its own backups");
                self.defer(e);
            }
            Kind::App => {
                self.lose("app-not-rendered", Kind::App, "application records have no Claude Code line");
                self.defer(e);
            }
            Kind::Native => {
                self.lose("foreign-native-not-rendered", Kind::Native, "another harness's native record");
                self.defer(e);
            }
        }
    }

    fn text(&mut self, e: &Event) {
        let text = e.str("text").unwrap_or("").to_string();
        if text.is_empty() {
            self.lose("empty-text-not-rendered", Kind::Text, "the API rejects empty text blocks");
            return self.defer(e);
        }
        if e.audience == Some(crate::model::Audience::Record) {
            self.lose("record-text-not-rendered", Kind::Text, "text the model never saw has no Claude Code line");
            return self.defer(e);
        }
        match e.str("role") {
            Some("assistant") => {
                self.assistant_line(e, json!({"type": "text", "text": text}));
            }
            Some("system") | Some("developer") => {
                if e.str("role") == Some("developer") {
                    self.lose("developer-as-meta-user", Kind::Text, "developer text becomes an isMeta user line (re-imported as role system)");
                }
                self.user_line(e, json!(text), &[("isMeta", json!(true))], &[]);
            }
            _ => {
                self.user_line(e, json!(text), &[], &[]);
            }
        }
    }

    fn media_block(body: &crate::model::Obj) -> Option<Value> {
        let mt = body.get("media_type").and_then(Value::as_str).unwrap_or("image/png");
        let source = if let Some(d) = body.get("data").and_then(Value::as_str) {
            json!({"type": "base64", "media_type": mt, "data": d})
        } else {
            let u = body.get("url").and_then(Value::as_str)?;
            json!({"type": "url", "url": u})
        };
        let ty = if mt.starts_with("image/") {
            "image"
        } else if mt == "application/pdf" || mt.starts_with("text/") {
            "document"
        } else {
            return None;
        };
        let mut b = json!({"type": ty, "source": source});
        if ty == "document"
            && let Some(n) = body.get("name")
        {
            b["title"] = n.clone();
        }
        Some(b)
    }

    fn media(&mut self, e: &Event) {
        match (e.str("role"), Self::media_block(&e.body)) {
            (Some("user") | None, Some(b)) => {
                self.user_line(e, json!([b]), &[], &[]);
            }
            _ => {
                self.lose("media-not-rendered", Kind::Media, "only user images and documents with inline data or a URL have a Claude Code block");
                self.defer(e);
            }
        }
    }

    fn reasoning(&mut self, e: &Event) {
        let sealed = e.body.get("sealed");
        let vendor = sealed.and_then(|x| s(x, "vendor"));
        let kind = sealed.and_then(|x| s(x, "kind"));
        let data = sealed.and_then(|x| s(x, "data")).unwrap_or("");
        match (vendor, kind) {
            (Some("anthropic"), Some("signature")) => {
                let b = json!({"type": "thinking", "thinking": e.str("text").unwrap_or(""), "signature": data});
                self.assistant_line(e, b);
            }
            (Some("anthropic"), Some("encrypted")) if !e.str("text").unwrap_or("").is_empty() => {
                // Redacted thinking has no text: a seal over visible text is a signature (SPEC §3.1.5).
                self.lose("reasoning-seal-kind-changed", Kind::Reasoning, "an Anthropic seal over readable text is written as a signature");
                let b = json!({"type": "thinking", "thinking": e.str("text").unwrap_or(""), "signature": data});
                self.assistant_line(e, b);
            }
            (Some("anthropic"), Some("encrypted")) => {
                self.assistant_line(e, json!({"type": "redacted_thinking", "data": data}));
            }
            (sealed_by, _) => {
                if sealed_by.is_some() {
                    self.lose("reasoning-sealed-dropped", Kind::Reasoning, "reasoning sealed by another vendor cannot be sent to Claude");
                }
                // Reasoning whose text we have becomes a thinking block with an
                // empty signature: Claude Code keeps it and resumes, and the
                // model no more sees it than it sees its own earlier thinking.
                let text = e.str("text").unwrap_or("");
                if e.body.get("summary").and_then(Value::as_array).is_some_and(|a| !a.is_empty()) {
                    self.lose("reasoning-summary-dropped", Kind::Reasoning, "a thinking block has no place for reasoning summaries");
                }
                if text.is_empty() {
                    if sealed_by.is_none() {
                        self.lose("reasoning-empty-dropped", Kind::Reasoning, "reasoning with no text has no thinking block to fill");
                    }
                    return self.defer(e);
                }
                self.assistant_line(e, json!({"type": "thinking", "thinking": text, "signature": ""}));
            }
        }
    }

    fn tool_call(&mut self, e: &Event) {
        let call = e.str("call_id").unwrap_or(&e.id).to_string();
        let id = tool_id(&call);
        if id != call || e.str("call_id").is_none() {
            self.lose("tool-id-sanitised", Kind::ToolCall, "tool call ids must match ^[a-zA-Z0-9_-]{1,64}$");
        }
        let raw_name = e.str("name").unwrap_or("tool");
        // Claude Code names an MCP tool mcp__<server>__<tool>, and that is the name its model sees.
        let mcp = match (e.str("tool_type"), e.str("server")) {
            (Some("mcp"), Some(server)) if !server.is_empty() && !raw_name.starts_with("mcp__") => {
                self.lose("tool-name-mcp-prefixed", Kind::ToolCall, "an MCP tool takes Claude Code's mcp__<server>__<tool> name");
                Some(format!("mcp__{server}__{raw_name}"))
            }
            _ => None,
        };
        let raw_name = mcp.as_deref().unwrap_or(raw_name);
        let name = tool_name(raw_name);
        if name != raw_name {
            self.lose("tool-name-sanitised", Kind::ToolCall, "tool names must match ^[a-zA-Z0-9_-]{1,128}$");
        }
        let parsed = e.str("input").and_then(|t| serde_json::from_str::<Value>(t).ok()).filter(Value::is_object);
        let input = match (e.body.get("input"), e.str("input_text")) {
            (Some(v @ Value::Object(_)), _) => v.clone(),
            (Some(Value::String(_)), None) if parsed.is_some() => {
                self.lose("tool-input-parsed", Kind::ToolCall, "JSON arguments given as a string become the object they encode");
                parsed.unwrap()
            }
            (_, Some(t)) => {
                // A free-text (custom) call; the importer reads this shape back as one.
                if e.str("tool_type") != Some("custom") {
                    self.lose("tool-input-text-as-custom", Kind::ToolCall, "free-text input comes back as a custom tool call");
                }
                if e.body.get("input").is_some_and(|v| !v.is_null()) {
                    self.lose("tool-input-dropped", Kind::ToolCall, "a call with both input and input_text keeps only the text");
                }
                json!({"input_text": t})
            }
            (Some(Value::Null) | None, None) => json!({}),
            (Some(other), None) => {
                self.lose("tool-input-wrapped", Kind::ToolCall, "a non-object tool input becomes {\"input\": <value>}");
                json!({"input": other})
            }
        };
        let i = self.assistant_line(e, json!({"type": "tool_use", "id": id, "name": name, "input": input}));
        let uuid = s(self.lines[i].value.as_ref().unwrap(), "uuid").unwrap().to_string();
        self.tools.insert(call, (id, uuid));
    }

    fn result_content(&mut self, out: Option<&Value>) -> Value {
        match out {
            None | Some(Value::Null) => json!(""),
            Some(Value::String(t)) => json!(t),
            Some(Value::Array(parts)) => Value::Array(
                parts
                    .iter()
                    .filter_map(|p| match s(p, "type") {
                        Some("text") => Some(json!({"type": "text", "text": s(p, "text").unwrap_or("")})),
                        Some("media") => {
                            let b = p.as_object().and_then(Self::media_block);
                            if b.is_none() {
                                self.losses.add("media-not-rendered", Some(Kind::ToolResult), "a tool output part with no Claude Code block");
                            }
                            b
                        }
                        _ => Some(json!({"type": "text", "text": serde_json::to_string(p).unwrap()})),
                    })
                    .collect(),
            ),
            Some(other) => json!(serde_json::to_string(other).unwrap()),
        }
    }

    fn tool_result(&mut self, e: &Event) {
        let call = e.str("call_id").unwrap_or("").to_string();
        let content = self.result_content(e.body.get("output"));
        let Some((id, src)) = self.tools.get(&call).cloned() else {
            // The API refuses a tool_result whose tool_use is not in the history.
            self.lose("orphan-tool-result-as-text", Kind::ToolResult, "a result whose call is not in this ledger becomes user text");
            let text = orphan_result_wrap(&call, &crate::formats::claude_code::import::text_of(&content));
            self.user_line(e, json!(text), &[], &[]);
            return;
        };
        let mut block = Map::new();
        block.insert("tool_use_id".into(), json!(id));
        block.insert("type".into(), json!("tool_result"));
        block.insert("content".into(), content);
        if e.body.get("is_error").and_then(Value::as_bool) == Some(true) {
            block.insert("is_error".into(), json!(true));
        }
        self.user_line(e, json!([Value::Object(block)]), &[], &[("sourceToolAssistantUUID", json!(src))]);
    }

    fn compaction(&mut self, e: &Event) {
        if e.str("phase") == Some("summary") {
            let text = e.str("summary").unwrap_or("").to_string();
            self.user_line(e, json!(text), &[("isVisibleInTranscriptOnly", json!(true)), ("isCompactSummary", json!(true))], &[]);
            return;
        }
        let mut meta = Map::new();
        meta.insert("trigger".into(), json!(e.str("trigger").unwrap_or("auto")));
        if let Some(t) = e.body.get("tokens_before").filter(|t| !t.is_null()) {
            meta.insert("preTokens".into(), t.clone());
        }
        if let Some(t) = e.body.get("tokens_after") {
            meta.insert("postTokens".into(), t.clone());
        }
        if e.body.get("preserved").and_then(Value::as_array).is_some_and(|a| !a.is_empty()) {
            self.lose("compaction-preserved-dropped", Kind::Compaction, "the preserved-segment list is not rendered");
        }
        let logical = self.prev.take();
        let uuid = self.uuid(e);
        let ts = self.ts(e);
        let mut m = Map::new();
        m.insert("parentUuid".into(), Value::Null);
        m.insert("logicalParentUuid".into(), logical.map(Value::from).unwrap_or(Value::Null));
        m.insert("isSidechain".into(), json!(self.plan.sidechain));
        if let Some(a) = &self.plan.aid {
            m.insert("agentId".into(), json!(a));
        }
        m.insert("type".into(), json!("system"));
        m.insert("subtype".into(), json!("compact_boundary"));
        m.insert("content".into(), json!("Conversation compacted"));
        m.insert("isMeta".into(), json!(false));
        m.insert("timestamp".into(), json!(ts));
        m.insert("uuid".into(), json!(uuid));
        m.insert("level".into(), json!("info"));
        m.insert("compactMetadata".into(), Value::Object(meta));
        self.tail(&mut m);
        self.push(e, Value::Object(m));
    }

    fn usage(&mut self, e: &Event) {
        let cumulative = e.body.get("cumulative").and_then(Value::as_bool) == Some(true);
        let target = self.run.as_ref().map(|r| r.lines.clone()).filter(|l| !l.is_empty());
        match target {
            Some(lines) if !cumulative => {
                let n = |k: &str| e.body.get(k).cloned().unwrap_or(json!(0));
                let mut u = Map::new();
                u.insert("input_tokens".into(), n("input"));
                u.insert("output_tokens".into(), n("output"));
                if e.body.contains_key("cache_read") {
                    u.insert("cache_read_input_tokens".into(), n("cache_read"));
                }
                if e.body.contains_key("cache_write") {
                    u.insert("cache_creation_input_tokens".into(), n("cache_write"));
                }
                if let Some(r) = e.body.get("reasoning") {
                    u.insert("output_tokens_details".into(), json!({"thinking_tokens": r}));
                }
                // On the message's last line so far that is not reasoning
                // alone: the line the importer settles the usage on (the
                // others keep zero usage, which settles nothing). A message of
                // reasoning alone ends on a reasoning line, and the stop
                // reasons Claude Code writes on its thinking lines (`end_turn`,
                // `tool_use`) never settle, since a reply may follow; that line
                // is written with `max_tokens`, which the importer settles on
                // without reading ahead (SPEC §9.3). The ledger holds no
                // stop reason, and the importer reads nothing else from it;
                // writing one is named as `stop-reason-synthesised`.
                let reasoning = |v: &Value| {
                    v["message"]["content"]
                        .as_array()
                        .is_some_and(|c| !c.is_empty() && c.iter().all(|b| matches!(b["type"].as_str(), Some("thinking" | "redacted_thinking"))))
                };
                let at = lines.iter().rev().find(|&&i| self.lines[i].value.as_ref().is_some_and(|v| !reasoning(v))).or(lines.last()).copied();
                let mut synthesised = false;
                if let Some(v) = at.and_then(|i| self.lines[i].value.as_mut()) {
                    v["message"]["usage"] = Value::Object(u.clone());
                    if reasoning(v) && v["message"]["stop_reason"] != json!(SYNTHESISED_STOP) {
                        v["message"]["stop_reason"] = json!(SYNTHESISED_STOP);
                        synthesised = true;
                    }
                }
                if synthesised {
                    self.lose(STOP_SYNTHESISED, Kind::Usage, STOP_SYNTHESISED_DETAIL);
                }
                if e.body.get("cost_usd").is_some() {
                    self.lose("usage-cost-dropped", Kind::Usage, "per-message usage has no cost field");
                }
            }
            _ if cumulative => self.lose("usage-cumulative-not-rendered", Kind::Usage, "running totals have no per-message form"),
            _ => self.lose("usage-not-rendered", Kind::Usage, "usage with no assistant message before it"),
        }
        self.defer(e);
    }

    fn settings(&mut self, e: &Event) {
        let mut other = false;
        for (k, v) in &e.body {
            match (k.as_str(), v.as_str()) {
                ("cwd", Some(x)) => self.cwd = Some(x.to_string()),
                ("git_branch", Some(x)) => self.branch = x.to_string(),
                ("permission_mode", Some(x)) => self.perm = Some(x.to_string()),
                ("model", Some(x)) => self.model = Some(x.to_string()),
                ("complete", _) => {}
                _ => other = true,
            }
        }
        if other {
            self.lose("settings-not-rendered", Kind::Settings, "only cwd, git_branch, permission_mode and model reach Claude Code lines");
        }
        self.defer(e);
    }
}
