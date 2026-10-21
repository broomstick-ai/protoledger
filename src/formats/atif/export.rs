//! protoledger → ATIF.
//!
//! A ledger imported from ATIF is rebuilt from its native records (the root
//! skeleton and one record per step) and printed in its original layout, so
//! the file comes back byte for byte. Events without an ATIF native record are
//! rendered from their neutral fields into new steps. The document is then
//! converted to the requested schema version, and carriers are attached to the
//! rendered steps (`step.extra.protoledger`) and the root (`extra.protoledger`).

use super::convert::convert;
use super::import::ROOT_ID;
use super::jsonfmt::{self, Style};
use super::{AUDIO_TYPES, DEFAULT_EXPORT, FORMAT, IMAGE_TYPES, LATEST, Ver, extension_for, is_external, join_rel, normalise_audio};
use crate::carry;
use crate::formats::{ExportOptions, Exported, Losses, Output};
use crate::ledger::{Ledger, LedgerSet, file_safe};
use crate::model::{Event, Kind, Obj, Relation, SPEC_VERSION};
use crate::util;
use crate::util::OrderedMap;
use anyhow::{Result, anyhow, bail};
use base64::Engine;
use serde_json::{Map, Value, json};
use std::collections::HashMap;

/// Internal marker on a rendered ref until its child's placement is known.
const CHILD: &str = "\u{0}child";

pub(super) fn export(set: &LedgerSet, opts: &ExportOptions) -> Result<Exported> {
    let target = match opts.variant.as_deref() {
        Some(v) => {
            Some(Ver::parse(v).filter(|x| *x >= Ver(1, 0) && *x <= LATEST).ok_or_else(|| anyhow!("unknown ATIF variant {v:?} (ATIF-v1.0 … ATIF-v1.8)"))?)
        }
        None => None,
    };
    let mut ex = Exporter { set, opts, target, out: Output::default(), losses: Losses::default(), first: None };
    let roots: Vec<&Ledger> = set.roots().collect();
    if roots.is_empty() {
        bail!("atif: nothing to export");
    }
    let mut plan = Vec::new();
    for l in &roots {
        let f = native_file(l);
        if let Some(f) = &f {
            ex.out.files.insert(f.clone(), Vec::new());
        }
        plan.push((*l, f));
    }
    let single = roots.len() == 1;
    for (l, f) in plan {
        let name = match f {
            Some(f) => f,
            None => ex.fresh_name("", if single { "trajectory".to_string() } else { file_safe(l.id()) }),
        };
        ex.write_file(l, &name)?;
    }
    let variant = ex.first.unwrap_or(DEFAULT_EXPORT).name();
    Ok(Exported { output: ex.out, losses: ex.losses, variant })
}

fn is_native(l: &Ledger) -> bool {
    l.agent().source.as_ref().is_some_and(|s| s.format == FORMAT)
}

fn native_ver(l: &Ledger) -> Option<Ver> {
    if !is_native(l) {
        return None;
    }
    l.agent().source.as_ref()?.variant.as_deref().and_then(Ver::parse)
}

fn source_extra<'a>(l: &'a Ledger, k: &str) -> Option<&'a Value> {
    l.agent().source.as_ref()?.extra.get(k)
}

/// The file an ATIF-imported top-level document came from.
fn native_file(l: &Ledger) -> Option<String> {
    if !is_native(l) || source_extra(l, "embedded").is_some() {
        return None;
    }
    source_extra(l, "file").and_then(Value::as_str).map(str::to_string)
}

/// The root record's `raw` is the whole file's text (no layout reproduced
/// it), not the skeleton's.
fn whole_file_raw(l: &Ledger) -> bool {
    is_native(l) && source_extra(l, "json").and_then(Value::as_str) == Some("raw")
}

fn embedded_index(l: &Ledger) -> Option<u64> {
    if !is_native(l) {
        return None;
    }
    source_extra(l, "embedded").and_then(Value::as_u64)
}

fn root_record(l: &Ledger) -> Option<Value> {
    l.events.iter().find_map(|e| {
        let n = e.native.as_ref()?;
        (n.format == FORMAT && n.id.as_deref() == Some(ROOT_ID) && n.part == 0).then_some(n.record_json()?)
    })
}

#[derive(Clone)]
enum Place {
    Embedded(String),
    File(String),
}

struct Exporter<'a> {
    set: &'a LedgerSet,
    opts: &'a ExportOptions,
    target: Option<Ver>,
    out: Output,
    losses: Losses,
    first: Option<Ver>,
}

impl<'a> Exporter<'a> {
    fn fresh_name(&self, dir_of: &str, stem: String) -> String {
        let mut n = 1;
        loop {
            let base = if n == 1 { format!("{stem}.json") } else { format!("{stem}~{n}.json") };
            let key = join_rel(dir_of, &base).unwrap_or(base);
            if !self.out.files.contains_key(&key) {
                return key;
            }
            n += 1;
        }
    }

    fn doc_ver(&self, l: &Ledger, parent: Option<Ver>) -> Ver {
        self.target.or_else(|| native_ver(l)).or(parent).unwrap_or(DEFAULT_EXPORT)
    }

    fn write_file(&mut self, l: &'a Ledger, name: &str) -> Result<()> {
        self.out.files.insert(name.to_string(), Vec::new());
        let ver = self.doc_ver(l, None);
        self.first.get_or_insert(ver);
        let (doc, exact, text) = self.build_doc(l, name, ver)?;
        // The whole file's text, kept when no layout reproduces it.
        let raw = l.events.iter().filter(|_| whole_file_raw(l)).find_map(|e| {
            let n = e.native.as_ref()?;
            (n.format == FORMAT && n.id.as_deref() == Some(ROOT_ID)).then_some(n.raw.as_ref()?)
        });
        // Written back whole only while it still says what the records say
        // (their own texts, else the document built from them).
        let rebuilt = || text.clone().unwrap_or_else(|| serde_json::to_string(&doc).expect("json"));
        if exact
            && let Some(raw) = raw
            && crate::util::same_json(raw, &rebuilt())
        {
            self.out.put(name, raw.as_bytes().to_vec());
            return Ok(());
        }
        let style = if is_native(l) { source_extra(l, "json").and_then(Style::from_value) } else { None };
        // Unchanged records are laid out from their own text, never re-spelled.
        if let (Some(st), Some(text)) = (style, text.filter(|_| exact)) {
            self.out.put(name, st.relay(&text).into_bytes());
            return Ok(());
        }
        if exact {
            // Every record is this document's own, yet neither its whole text
            // nor its records' texts could be written: the parsed value is.
            self.losses.add(jsonfmt::EXACT_TEXT_FALLBACK, None, jsonfmt::EXACT_TEXT_FALLBACK_DETAIL);
        }
        self.out.put(name, style.unwrap_or(Style::PRETTY).render(&doc).into_bytes());
        Ok(())
    }

    /// One trajectory document for `l` (its embedded subagents included),
    /// whether it is exactly what was imported, and, when it is, its compact
    /// text rebuilt from the records' own texts.
    fn build_doc(&mut self, l: &'a Ledger, file: &str, ver: Ver) -> Result<(Value, bool, Option<String>)> {
        let native = is_native(l);
        let mut exact = native && native_ver(l) == Some(ver);
        let mut root_base: Option<Value> = None;
        let mut root_text: Option<String> = None;
        let mut step_texts: Vec<Option<String>> = Vec::new();
        let mut steps: Vec<StepOut> = Vec::new();
        let mut payloads: HashMap<String, Vec<Obj>> = HashMap::new();
        let mut r = Renderer::new(l, file);
        let on = crate::formats::active_path_only(l, &mut self.losses, "with carry they ride on the next step", |e| {
            e.native.as_ref().is_some_and(|n| n.format == FORMAT)
        });
        for (e, on) in l.events.iter().zip(on) {
            if !on {
                exact = false;
                r.pending.push(e.clone());
                continue;
            }
            match &e.native {
                Some(n) if n.format == FORMAT => {
                    let nid = n.id.clone().unwrap_or_default();
                    payloads.entry(nid.clone()).or_default().extend(media_payloads(e));
                    if n.part != 0 {
                        continue;
                    }
                    match &n.record_json() {
                        Some(rec) if nid == ROOT_ID => {
                            root_base = Some(rec.clone());
                            let raw = crate::util::current_raw(n).map(str::to_string).filter(|_| !whole_file_raw(l));
                            root_text = raw.or_else(|| n.record_text().map(str::to_string));
                        }
                        Some(rec) => {
                            r.note_native_step(rec, steps.len());
                            steps.push(StepOut::native(rec.clone()));
                            // `raw` first: it is the source text when a carrier
                            // brought the record back re-spelled.
                            step_texts.push(crate::util::native_text(n));
                        }
                        None => exact = false,
                    }
                }
                _ => {
                    exact = false;
                    r.event(e, &mut steps, &mut self.out);
                }
            }
        }
        r.finish(&mut steps);
        self.losses.extend(std::mem::take(&mut r.losses));
        for s in steps.iter().filter(|s| s.native) {
            self.write_native_media(&s.v, file, &payloads);
        }

        // Children: embedded in this document, or files of their own.
        let mut kids: Vec<&'a Ledger> = self.set.children_of(l.id()).collect();
        let first_mention = |id: &str| l.events.iter().position(|e| e.kind == Kind::Subagent && e.str("agent") == Some(id)).unwrap_or(usize::MAX);
        kids.sort_by_key(|c| (embedded_index(c).unwrap_or(u64::MAX), first_mention(c.id()), c.agent().created_at.clone()));
        let mut embedded = Vec::new();
        let mut embedded_texts: Vec<Option<String>> = Vec::new();
        let mut places: HashMap<String, Place> = HashMap::new();
        let mut tids: HashMap<String, String> = HashMap::new();
        for c in kids {
            let cver = self.doc_ver(c, Some(ver));
            let subagent = c.agent().parent.as_ref().is_some_and(|p| p.relation == Relation::Subagent);
            let tid = root_record(c).and_then(|r| r.get("trajectory_id").and_then(Value::as_str).map(str::to_string)).unwrap_or_else(|| c.id().to_string());
            let place = if embedded_index(c).is_some() && ver.at_least(1, 7) && cver.at_least(1, 7) {
                Place::Embedded(tid.clone())
            } else if let Some(f) = native_file(c) {
                Place::File(f)
            } else if !is_native(c) && subagent && ver.at_least(1, 7) {
                Place::Embedded(tid.clone())
            } else {
                Place::File(self.fresh_name(file, format!("trajectory.{}", file_safe(c.id()))))
            };
            if embedded_index(c).is_some() && matches!(place, Place::File(_)) {
                exact = false;
            }
            match &place {
                Place::Embedded(_) => {
                    let (d, ex2, t2) = self.build_doc(c, file, cver)?;
                    exact &= ex2;
                    embedded.push(d);
                    embedded_texts.push(t2);
                }
                Place::File(f) => {
                    let f = f.clone();
                    self.write_file(c, &f)?;
                }
            }
            tids.insert(tid, c.id().to_string());
            places.insert(c.id().to_string(), place);
        }

        // Steps → JSON, refs resolved.
        let mut doc = match root_base {
            Some(b) => b,
            None => {
                exact = false;
                r.fresh_root(l, ver, self.opts)
            }
        };
        let mut vals: Vec<Value> = Vec::new();
        let mut carried: Vec<Vec<Event>> = Vec::new();
        for (i, s) in steps.into_iter().enumerate() {
            let (mut v, c) = s.into_value(i + 1);
            if v.get("step_id").and_then(Value::as_u64) != Some(i as u64 + 1) {
                // A held step that is not an object (a damaged source) has no
                // `step_id` to set; it is written as it was read.
                if let Some(o) = v.as_object_mut() {
                    o.insert("step_id".into(), json!(i + 1));
                }
                exact = false;
            }
            if !exact {
                resolve_refs(&mut v, file, &places, &tids, ver);
            }
            vals.push(v);
            carried.push(c);
        }
        let o = doc.as_object_mut().ok_or_else(|| anyhow!("ATIF root record is not an object"))?;
        if o.get("steps").is_none_or(Value::is_array) {
            o.insert("steps".into(), Value::Array(vals));
        }
        if !embedded.is_empty() {
            o.insert("subagent_trajectories".into(), Value::Array(embedded));
        }
        if !exact {
            convert(&mut doc, ver, l.id(), &mut self.losses);
            if self.opts.carry {
                let steps = doc["steps"].as_array_mut().unwrap();
                for (v, c) in steps.iter_mut().zip(carried) {
                    carry::attach(v, &["extra"], &c);
                }
                if !native && ver.at_least(1, 1) {
                    let a = l.agent();
                    let mut h = Map::new();
                    h.insert("id".into(), json!(a.id));
                    if let Some(c) = &a.created_at {
                        h.insert("created_at".into(), json!(c));
                    }
                    if !a.meta.is_empty() {
                        h.insert("meta".into(), Value::Object(a.meta.clone()));
                    }
                    if !a.links.is_empty() {
                        h.insert("links".into(), serde_json::to_value(&a.links)?);
                    }
                    let o = doc.as_object_mut().unwrap();
                    let x = o.entry("extra").or_insert_with(|| json!({}));
                    if x.is_null() {
                        *x = json!({});
                    }
                    x[carry::KEY] = json!({"v": SPEC_VERSION, "agent": h});
                }
            }
        }
        let text = if exact { Self::exact_text(&doc, root_text, step_texts, embedded_texts) } else { None };
        Ok((doc, exact, text))
    }

    /// The compact text of an unchanged document: the root record's own text with
    /// its `steps` and `subagent_trajectories` lists filled from the records' own
    /// texts. `None` when a piece has no text or the shape does not allow it.
    fn exact_text(doc: &Value, root: Option<String>, steps: Vec<Option<String>>, embedded: Vec<Option<String>>) -> Option<String> {
        // The same placement rules as the value path in `build_doc`: `steps` is
        // filled when it is a list (and added when absent, which no text holds);
        // `subagent_trajectories` is set when there are any.
        let mut t = root?;
        let steps: Vec<String> = steps.into_iter().collect::<Option<_>>()?;
        match jsonfmt::member(&t, "steps") {
            Some(v) if v.starts_with('[') => t = jsonfmt::with_member(&t, "steps", &jsonfmt::array_text(steps.iter().map(String::as_str)))?,
            Some(_) => {}
            None => return None,
        }
        let embedded: Vec<String> = embedded.into_iter().collect::<Option<_>>()?;
        if !embedded.is_empty() {
            t = jsonfmt::with_member(&t, "subagent_trajectories", &jsonfmt::array_text(embedded.iter().map(String::as_str)))?;
        }
        // A guard on the rebuild: the text must say what the document says.
        (serde_json::from_str::<Value>(&t).ok().as_ref() == Some(doc)).then_some(t)
    }

    /// Re-create the image/audio files a native step references, from the
    /// bytes the importer read into its media events.
    fn write_native_media(&mut self, rec: &Value, file: &str, payloads: &HashMap<String, Vec<Obj>>) {
        let sid = match rec.get("step_id") {
            Some(Value::String(x)) => x.clone(),
            Some(v) => v.to_string(),
            None => return,
        };
        let Some(p) = payloads.get(&format!("step-{sid}")) else { return };
        let paths = media_paths(rec);
        for (path, payload) in paths.iter().zip(p) {
            let (Some(path), Some(data)) = (path, payload.get("data").and_then(Value::as_str)) else { continue };
            let Some(key) = join_rel(file, path) else { continue };
            if let Ok(bytes) = base64::engine::general_purpose::STANDARD.decode(data) {
                self.out.put(key, bytes);
            }
        }
    }
}

/// Media parts of a step record, in the order the importer made events from
/// them: message parts, then observation result content parts.
fn media_paths(rec: &Value) -> Vec<Option<String>> {
    let compaction = rec.get("source").and_then(Value::as_str) == Some("system")
        && rec.get("extra").and_then(|x| x.get("context_management")).and_then(|c| c.get("type")).and_then(Value::as_str) == Some("compaction");
    let mut out = Vec::new();
    let mut walk = |c: Option<&Value>| {
        for p in c.and_then(Value::as_array).into_iter().flatten() {
            if matches!(p.get("type").and_then(Value::as_str), Some("image") | Some("audio")) {
                out.push(p.get("source").and_then(|s| s.get("path")).and_then(Value::as_str).map(str::to_string));
            }
        }
    };
    if compaction {
        return out;
    }
    walk(rec.get("message"));
    for r in rec.get("observation").and_then(|o| o.get("results")).and_then(Value::as_array).into_iter().flatten() {
        walk(r.get("content"));
    }
    out
}

/// Media bodies an event holds, in order (a media event, or the media parts of
/// a tool result's output).
fn media_payloads(e: &Event) -> Vec<Obj> {
    match e.kind {
        Kind::Media => vec![e.body.clone()],
        Kind::ToolResult => e
            .body
            .get("output")
            .and_then(Value::as_array)
            .into_iter()
            .flatten()
            .filter(|p| p.get("type").and_then(Value::as_str) == Some("media"))
            .filter_map(|p| p.as_object().cloned())
            .collect(),
        _ => Vec::new(),
    }
}

fn resolve_refs(step: &mut Value, file: &str, places: &HashMap<String, Place>, tids: &HashMap<String, String>, ver: Ver) {
    let results = step.get_mut("observation").and_then(|o| o.get_mut("results")).and_then(Value::as_array_mut);
    for r in results.into_iter().flatten() {
        for sref in r.get_mut("subagent_trajectory_ref").and_then(Value::as_array_mut).into_iter().flatten() {
            let Some(o) = sref.as_object_mut() else { continue };
            let child = match o.remove_ordered(CHILD) {
                Some(Value::String(c)) => c,
                _ => {
                    // A native ref whose embedded child had to become a file.
                    let Some(t) = o.get("trajectory_id").and_then(Value::as_str) else { continue };
                    let Some(c) = tids.get(t) else { continue };
                    if let Some(Place::File(f)) = places.get(c)
                        && o.get("trajectory_path").is_none_or(Value::is_null)
                    {
                        o.insert("trajectory_path".into(), json!(rel_from(file, f)));
                    }
                    continue;
                }
            };
            let extra = o.remove_ordered("extra");
            o.clear();
            match places.get(&child) {
                Some(Place::Embedded(tid)) => {
                    o.insert("trajectory_id".into(), json!(tid));
                }
                Some(Place::File(f)) => {
                    o.insert("trajectory_id".into(), json!(child));
                    o.insert("session_id".into(), json!(child));
                    o.insert("trajectory_path".into(), json!(rel_from(file, f)));
                }
                None if ver.at_least(1, 7) => {
                    o.insert("trajectory_id".into(), json!(child));
                }
                None => {
                    o.insert("session_id".into(), json!(child));
                    o.insert("trajectory_path".into(), json!(format!("trajectory.{}.json", file_safe(&child))));
                }
            }
            if let Some(x) = extra {
                o.insert("extra".into(), x);
            }
        }
    }
}

/// `target` (relative to the output root) as seen from `file`'s directory.
fn rel_from(file: &str, target: &str) -> String {
    let dir: Vec<&str> = file.split('/').collect();
    let dir = &dir[..dir.len() - 1];
    let t: Vec<&str> = target.split('/').collect();
    let common = dir.iter().zip(&t).take_while(|(a, b)| a == b).count();
    let mut parts: Vec<String> = std::iter::repeat_n("..".to_string(), dir.len() - common).collect();
    parts.extend(t[common..].iter().map(|s| s.to_string()));
    parts.join("/")
}

/// One step being assembled.
#[derive(Default)]
struct StepOut {
    native: bool,
    v: Value,
    source: &'static str,
    group: Option<String>,
    ts: Option<String>,
    model_name: Option<String>,
    effort: Option<Value>,
    parts: Vec<Value>,
    reasoning: Vec<String>,
    tool_calls: Vec<Value>,
    results: Vec<Map<String, Value>>,
    metrics: Option<Map<String, Value>>,
    llm_calls: u64,
    copied: bool,
    extra: Map<String, Value>,
    compaction: bool,
    carried: Vec<Event>,
}

impl StepOut {
    fn native(v: Value) -> Self {
        StepOut { native: true, v, ..Default::default() }
    }

    fn new(source: &'static str, e: &Event) -> Self {
        StepOut { source, group: e.group.clone(), ts: e.ts.clone(), ..Default::default() }
    }

    fn into_value(self, n: usize) -> (Value, Vec<Event>) {
        if self.native {
            return (self.v, Vec::new());
        }
        let mut o = Map::new();
        o.insert("step_id".into(), json!(n));
        if let Some(t) = self.ts {
            o.insert("timestamp".into(), json!(t));
        }
        o.insert("source".into(), json!(self.source));
        let agent = self.source == "agent";
        if agent {
            if let Some(m) = self.model_name {
                o.insert("model_name".into(), json!(m));
            }
            if let Some(e) = self.effort {
                o.insert("reasoning_effort".into(), e);
            }
        }
        let message = match self.parts.as_slice() {
            [] => json!(""),
            [p] if p.get("type").and_then(Value::as_str) == Some("text") => p["text"].clone(),
            _ => Value::Array(self.parts),
        };
        o.insert("message".into(), message);
        if agent {
            if !self.reasoning.is_empty() {
                o.insert("reasoning_content".into(), json!(self.reasoning.join("\n\n")));
            }
            if !self.tool_calls.is_empty() {
                o.insert("tool_calls".into(), Value::Array(self.tool_calls));
            }
        }
        if !self.results.is_empty() {
            let rs: Vec<Value> = self.results.into_iter().map(Value::Object).collect();
            o.insert("observation".into(), json!({ "results": rs }));
        }
        if agent && let Some(m) = self.metrics {
            o.insert("metrics".into(), Value::Object(m));
        }
        if self.copied {
            o.insert("is_copied_context".into(), json!(true));
        }
        if agent && self.llm_calls > 1 {
            o.insert("llm_call_count".into(), json!(self.llm_calls));
        }
        if !self.extra.is_empty() {
            o.insert("extra".into(), Value::Object(self.extra));
        }
        (Value::Object(o), self.carried)
    }
}

/// Renders events that have no ATIF native record into steps.
struct Renderer<'l> {
    l: &'l Ledger,
    file: String,
    agent_model: Option<String>,
    tools: Option<Value>,
    cur_model: Option<String>,
    cur_effort: Option<Value>,
    /// The open agent step, if any.
    cur: Option<usize>,
    calls: HashMap<String, usize>,
    root_events: Vec<Event>,
    pending: Vec<Event>,
    final_usage: Option<Obj>,
    last: Option<usize>,
    losses: Losses,
}

impl<'l> Renderer<'l> {
    fn new(l: &'l Ledger, file: &str) -> Self {
        Renderer {
            l,
            file: file.to_string(),
            agent_model: None,
            tools: None,
            cur_model: None,
            cur_effort: None,
            cur: None,
            calls: HashMap::new(),
            root_events: Vec::new(),
            pending: Vec::new(),
            final_usage: None,
            last: None,
            losses: Losses::default(),
        }
    }

    fn note_native_step(&mut self, rec: &Value, idx: usize) {
        for tc in rec.get("tool_calls").and_then(Value::as_array).into_iter().flatten() {
            if let Some(c) = tc.get("tool_call_id").and_then(Value::as_str) {
                self.calls.insert(c.to_string(), idx);
            }
        }
        if rec.get("source").and_then(Value::as_str) == Some("agent")
            && let Some(m) = rec.get("model_name").and_then(Value::as_str)
        {
            self.cur_model = Some(m.to_string());
        }
        self.cur = None;
    }

    fn push(&mut self, steps: &mut Vec<StepOut>, mut s: StepOut) -> usize {
        s.carried.append(&mut self.pending);
        steps.push(s);
        let i = steps.len() - 1;
        if steps[i].source != "agent" {
            self.cur = None;
        }
        i
    }

    /// The agent step `e` belongs to, opening one if needed.
    fn agent_step(&mut self, e: &Event, steps: &mut Vec<StepOut>) -> usize {
        if let Some(i) = self.cur {
            let s = &steps[i];
            let order_ok = match e.kind {
                Kind::Reasoning => s.parts.is_empty() && s.tool_calls.is_empty(),
                Kind::Text | Kind::Media | Kind::PeerMessage => s.tool_calls.is_empty(),
                _ => true,
            };
            let join = match (&s.group, &e.group) {
                (Some(a), Some(b)) => a == b,
                (None, None) => s.results.is_empty() && order_ok,
                _ => false,
            };
            if join && !s.native {
                return i;
            }
        }
        let mut s = StepOut::new("agent", e);
        if self.cur_model.is_some() && self.cur_model != self.agent_model {
            s.model_name = self.cur_model.clone();
        }
        s.effort = self.cur_effort.clone();
        let i = self.push(steps, s);
        self.cur = Some(i);
        i
    }

    /// A user or system step for `e`, joining the previous one when it is the
    /// same message.
    fn plain_step(&mut self, source: &'static str, e: &Event, steps: &mut Vec<StepOut>) -> usize {
        if let (Some(last), Some(s)) = (self.last, steps.last()) {
            let same = last + 1 == steps.len()
                && !s.native
                && s.source == source
                && !s.compaction
                && s.results.is_empty()
                && match (&s.group, &e.group) {
                    (Some(a), Some(b)) => a == b,
                    (None, None) => e.kind == Kind::Media,
                    _ => false,
                };
            if same {
                return last;
            }
        }
        self.push(steps, StepOut::new(source, e))
    }

    fn last_neutral(&self, steps: &[StepOut]) -> Option<usize> {
        steps.iter().rposition(|s| !s.native)
    }

    fn carry_to(&mut self, idx: Option<usize>, e: &Event, steps: &mut [StepOut]) {
        match idx {
            Some(i) if !steps[i].native => steps[i].carried.push(e.clone()),
            _ => self.pending.push(e.clone()),
        }
    }

    fn event(&mut self, e: &Event, steps: &mut Vec<StepOut>, out: &mut Output) {
        if e.meta.get("atif").and_then(|m| m.get("is_copied_context")).and_then(Value::as_bool) == Some(true) {
            // applied below once the step is known
        }
        let idx: Option<usize> = match e.kind {
            Kind::Settings => self.settings(e, steps),
            Kind::Text | Kind::PeerMessage => {
                let wrapped;
                let (role, text) = if e.kind == Kind::PeerMessage {
                    if crate::formats::pi_session::peer_has_more(e) {
                        self.losses.add(
                            crate::formats::pi_session::PEER_FIELDS_DROPPED,
                            Some(Kind::PeerMessage),
                            crate::formats::pi_session::PEER_FIELDS_DROPPED_DETAIL,
                        );
                    }
                    let (role, t) = crate::formats::pi_session::peer_as_text(e);
                    wrapped = t;
                    (role, wrapped.as_str())
                } else {
                    (e.str("role").unwrap_or("user"), e.str("text").unwrap_or(""))
                };
                let i = match role {
                    "assistant" => self.agent_step(e, steps),
                    "system" => self.plain_step("system", e, steps),
                    "developer" => {
                        self.losses.add("developer-as-system", Some(Kind::Text), "ATIF has no developer role");
                        self.plain_step("system", e, steps)
                    }
                    _ => self.plain_step("user", e, steps),
                };
                steps[i].parts.push(json!({"type": "text", "text": text}));
                Some(i)
            }
            Kind::Media => {
                let role = e.str("role").unwrap_or("user");
                let i = match role {
                    "assistant" => self.agent_step(e, steps),
                    "system" | "developer" => self.plain_step("system", e, steps),
                    _ => self.plain_step("user", e, steps),
                };
                if let Some(p) = self.media_part(&e.body, &e.id, out) {
                    steps[i].parts.push(p);
                }
                Some(i)
            }
            Kind::Reasoning => {
                let i = self.agent_step(e, steps);
                let mut text = e.str("text").unwrap_or("").to_string();
                if text.is_empty()
                    && let Some(s) = e.body.get("summary").and_then(Value::as_array)
                {
                    text = s.iter().filter_map(Value::as_str).collect::<Vec<_>>().join("\n");
                }
                if e.body.get("sealed").is_some_and(|v| !v.is_null()) {
                    self.losses.add("reasoning-sealed-dropped", Some(Kind::Reasoning), "ATIF has no field for signed or encrypted reasoning");
                }
                if text.is_empty() {
                    if e.body.get("redacted").and_then(Value::as_bool) == Some(true) {
                        self.losses.add("reasoning-redacted-dropped", Some(Kind::Reasoning), "redacted reasoning has no text to write");
                    }
                } else {
                    steps[i].reasoning.push(text);
                }
                Some(i)
            }
            Kind::ToolCall => {
                let i = self.agent_step(e, steps);
                let call_id = e.str("call_id").map(str::to_string).unwrap_or_else(|| format!("call_{}", util::short_id(&e.id, 12)));
                let mut tc = Map::new();
                tc.insert("tool_call_id".into(), json!(call_id));
                tc.insert("function_name".into(), json!(e.str("name").unwrap_or("unknown")));
                let mut x = Map::new();
                let args = match (e.body.get("input"), e.str("input_text")) {
                    (Some(Value::Object(m)), _) => Value::Object(m.clone()),
                    (None, Some(t)) | (Some(Value::Null), Some(t)) => {
                        x.insert("input_text".into(), json!(true));
                        json!({"input": t})
                    }
                    (None, None) | (Some(Value::Null), None) => json!({}),
                    (Some(v), _) => {
                        self.losses.add("tool-input-wrapped", Some(Kind::ToolCall), "ATIF arguments must be an object; wrapped as {\"input\": …}");
                        json!({"input": v})
                    }
                };
                tc.insert("arguments".into(), args);
                if let Some(t) = e.str("tool_type").filter(|t| *t != "function") {
                    x.insert("tool_type".into(), json!(t));
                }
                if let Some(sv) = e.body.get("server").filter(|v| !v.is_null()) {
                    x.insert("server".into(), sv.clone());
                }
                if !x.is_empty() {
                    tc.insert("extra".into(), Value::Object(x));
                }
                steps[i].tool_calls.push(Value::Object(tc));
                self.calls.insert(call_id, i);
                Some(i)
            }
            Kind::ToolResult => Some(self.tool_result(e, steps, out)),
            Kind::Usage => self.usage(e, steps),
            Kind::Compaction => Some(self.compaction(e, steps)),
            Kind::Subagent => Some(self.subagent(e, steps)),
            other => {
                let code = match other {
                    Kind::Title => "title-dropped",
                    Kind::Turn => "turn-dropped",
                    Kind::Error => "error-dropped",
                    Kind::FileChange => "file-change-dropped",
                    Kind::App => "app-dropped",
                    _ => "native-dropped",
                };
                self.losses.add(code, Some(other), "ATIF has no equivalent; carried when carry is on");
                let i = self.last_neutral(steps);
                self.carry_to(i, e, steps);
                self.last = None;
                return;
            }
        };
        match idx {
            Some(i) => {
                if e.meta.get("atif").and_then(|m| m.get("is_copied_context")).and_then(Value::as_bool) == Some(true) {
                    steps[i].copied = true;
                }
                self.carry_to(Some(i), e, steps);
                self.last = Some(i);
            }
            None => self.last = None,
        }
    }

    fn settings(&mut self, e: &Event, steps: &mut Vec<StepOut>) -> Option<usize> {
        let b = &e.body;
        let model = b.get("model").and_then(Value::as_str).map(str::to_string);
        let before_any = steps.is_empty();
        if before_any && self.agent_model.is_none() && model.is_some() {
            self.agent_model = model.clone();
        }
        if before_any && self.tools.is_none() {
            if let Some(t) = b.get("tools").and_then(tool_definitions) {
                self.tools = Some(t);
            } else if b.get("tools").is_some_and(|v| !v.is_null()) {
                self.losses.add("tools-dropped", Some(Kind::Settings), "tools are not OpenAI function definitions");
            }
        }
        if model.is_some() {
            self.cur_model = model;
        }
        if let Some(eff) = b.get("effort").filter(|v| !v.is_null()) {
            self.cur_effort = Some(eff.clone());
        }
        let other = b.keys().any(|k| !matches!(k.as_str(), "model" | "tools" | "effort" | "system_prompt" | "complete"));
        if other {
            self.losses.add("settings-dropped", Some(Kind::Settings), "settings other than model, effort, tools and system prompt");
        }
        if let Some(sp) = b.get("system_prompt").and_then(Value::as_str) {
            let i = self.push(steps, StepOut::new("system", e));
            steps[i].parts.push(json!({"type": "text", "text": sp}));
            return Some(i);
        }
        if before_any {
            self.root_events.push(e.clone());
            None
        } else {
            let i = self.last_neutral(steps);
            self.carry_to(i, e, steps);
            None
        }
    }

    fn tool_result(&mut self, e: &Event, steps: &mut Vec<StepOut>, out: &mut Output) -> usize {
        let call_id = e.str("call_id").map(str::to_string);
        let content = match e.body.get("output") {
            Some(Value::Array(parts)) => {
                let mut ps = Vec::new();
                for p in parts {
                    match p.get("type").and_then(Value::as_str) {
                        Some("media") => {
                            if let Some(m) = p.as_object().and_then(|o| self.media_part(o, &e.id, out)) {
                                ps.push(m);
                            }
                        }
                        _ => ps.push(json!({"type": "text", "text": p.get("text").and_then(Value::as_str).unwrap_or("")})),
                    }
                }
                Value::Array(ps)
            }
            Some(Value::Null) | None => json!(""),
            Some(Value::String(s)) => json!(s),
            Some(other) => json!(other.to_string()),
        };
        let mut x = Map::new();
        if let Some(v) = e.body.get("is_error").filter(|v| !v.is_null()) {
            x.insert("is_error".into(), v.clone());
        }
        if let Some(v) = e.body.get("status").filter(|v| !v.is_null()) {
            x.insert("status".into(), v.clone());
        }
        let matched = call_id.as_ref().and_then(|c| self.calls.get(c).copied()).filter(|i| !steps[*i].native);
        let i = match matched {
            Some(i) => i,
            None => {
                if call_id.is_some() {
                    self.losses.add("tool-result-unmatched", Some(Kind::ToolResult), "no tool call with this id in a rendered step");
                }
                match self.cur.or_else(|| self.last_neutral(steps)) {
                    Some(i) => i,
                    None => self.push(steps, StepOut::new("system", e)),
                }
            }
        };
        let source_call_id = if matched.is_some() { call_id } else { None };
        let s = &mut steps[i];
        let slot = source_call_id
            .as_ref()
            .and_then(|c| s.results.iter().position(|r| r.get("source_call_id").and_then(Value::as_str) == Some(c.as_str()) && !r.contains_key("content")));
        let r = match slot {
            Some(k) => &mut s.results[k],
            None => {
                let mut r = Map::new();
                if let Some(c) = &source_call_id {
                    r.insert("source_call_id".into(), json!(c));
                }
                s.results.push(r);
                s.results.last_mut().unwrap()
            }
        };
        r.insert("content".into(), content);
        if !x.is_empty() {
            r.insert("extra".into(), Value::Object(x));
        }
        // Keep source_call_id first, content before refs, extra last.
        let mut ordered = Map::new();
        for k in ["source_call_id", "content", "subagent_trajectory_ref", "extra"] {
            if let Some(v) = r.remove_ordered(k) {
                ordered.insert(k.into(), v);
            }
        }
        *r = ordered;
        i
    }

    fn usage(&mut self, e: &Event, steps: &mut [StepOut]) -> Option<usize> {
        if e.body.get("cumulative").and_then(Value::as_bool) == Some(true) {
            self.final_usage = Some(e.body.clone());
            self.root_events.push(e.clone());
            return None;
        }
        let by_group = e.group.as_ref().and_then(|g| steps.iter().rposition(|s| !s.native && s.source == "agent" && s.group.as_ref() == Some(g)));
        let target = by_group.or(self.cur).or_else(|| steps.iter().rposition(|s| !s.native && s.source == "agent"));
        let Some(i) = target else {
            self.losses.add("usage-unattached", Some(Kind::Usage), "usage before any agent step");
            let i = self.last_neutral(steps);
            self.carry_to(i, e, steps);
            return None;
        };
        let m = metrics_from(&e.body, "");
        let s = &mut steps[i];
        match &mut s.metrics {
            None => {
                s.metrics = Some(m);
                s.llm_calls = 1;
            }
            Some(old) => {
                for (k, v) in m {
                    match (old.get(&k).filter(|x| x.is_number()), v.is_number()) {
                        (Some(a), true) if k != "extra" => match sum_like_source(a, &v) {
                            Some(sum) => {
                                old.insert(k, sum);
                            }
                            None => self.losses.add(
                                "usage-sum-unrepresentable",
                                Some(Kind::Usage),
                                "a summed step metric is past what a JSON number holds; the earlier value is kept",
                            ),
                        },
                        _ => {
                            old.insert(k, v);
                        }
                    }
                }
                s.llm_calls += 1;
            }
        }
        if let Some(model) = e.str("model")
            && Some(model) != self.agent_model.as_deref()
            && s.model_name.is_none()
        {
            s.model_name = Some(model.to_string());
        }
        Some(i)
    }

    fn compaction(&mut self, e: &Event, steps: &mut Vec<StepOut>) -> usize {
        let summary = e.str("phase") == Some("summary");
        if summary && let Some(i) = self.last_neutral(steps) {
            let s = &steps[i];
            if s.compaction && s.results.is_empty() && self.last == Some(i) {
                steps[i].results.push(obj(json!({"content": e.str("summary").unwrap_or("")})));
                return i;
            }
        }
        let mut s = StepOut::new("system", e);
        s.compaction = true;
        s.parts.push(json!({"type": "text", "text": "Context compaction"}));
        let mut cm = obj(json!({"type": "compaction", "boundary": "replace"}));
        for k in ["trigger", "tokens_before", "tokens_after"] {
            if let Some(v) = e.body.get(k).filter(|v| !v.is_null()) {
                cm.insert(k.into(), v.clone());
            }
        }
        s.extra.insert("context_management".into(), Value::Object(cm));
        if summary {
            s.results.push(obj(json!({"content": e.str("summary").unwrap_or("")})));
        }
        if e.body.get("preserved").is_some_and(|v| v.as_array().is_some_and(|a| !a.is_empty())) {
            self.losses.add("compaction-preserved-dropped", Some(Kind::Compaction), "ATIF has no list of preserved events");
        }
        self.push(steps, s)
    }

    fn subagent(&mut self, e: &Event, steps: &mut Vec<StepOut>) -> usize {
        let call_id = e.str("call_id").map(str::to_string);
        let child = e.str("agent").unwrap_or("unknown").to_string();
        let matched = call_id.as_ref().and_then(|c| self.calls.get(c).copied()).filter(|i| !steps[*i].native);
        let i = match matched.or(self.cur).or_else(|| self.last_neutral(steps)) {
            Some(i) => i,
            None => {
                let mut s = StepOut::new("system", e);
                s.parts.push(json!({"type": "text", "text": "Subagent delegated"}));
                self.push(steps, s)
            }
        };
        let src = if matched.is_some() { call_id } else { None };
        let s = &mut steps[i];
        let find = |r: &Map<String, Value>| {
            r.get("source_call_id").and_then(Value::as_str) == src.as_deref()
                && r.get("subagent_trajectory_ref")
                    .and_then(Value::as_array)
                    .is_some_and(|a| a.iter().any(|x| x.get(CHILD).and_then(Value::as_str) == Some(child.as_str())))
        };
        if e.str("phase") == Some("end") {
            if let Some(r) = s.results.iter_mut().find(|r| find(r))
                && let Some(o) = e.body.get("outcome").filter(|v| !v.is_null())
            {
                let refs = r.get_mut("subagent_trajectory_ref").and_then(Value::as_array_mut).unwrap();
                for x in refs.iter_mut().filter(|x| x.get(CHILD).and_then(Value::as_str) == Some(child.as_str())) {
                    if x.get("extra").is_none() {
                        x["extra"] = json!({});
                    }
                    x["extra"]["outcome"] = o.clone();
                }
            }
            return i;
        }
        let mut sref = Map::new();
        sref.insert(CHILD.into(), json!(child));
        let mut x = Map::new();
        if let Some(d) = e.body.get("description").filter(|v| !v.is_null()) {
            x.insert("summary".into(), d.clone());
        }
        if let Some(t) = e.body.get("agent_type").filter(|v| !v.is_null()) {
            x.insert("agent_type".into(), t.clone());
        }
        if !x.is_empty() {
            sref.insert("extra".into(), Value::Object(x));
        }
        let slot = s.results.iter().position(|r| r.get("source_call_id").and_then(Value::as_str) == src.as_deref() && src.is_some());
        match slot {
            Some(k) => {
                let r = &mut s.results[k];
                let refs = r.entry("subagent_trajectory_ref").or_insert_with(|| json!([]));
                refs.as_array_mut().unwrap().push(Value::Object(sref));
            }
            None => {
                let mut r = Map::new();
                if let Some(c) = &src {
                    r.insert("source_call_id".into(), json!(c));
                }
                r.insert("subagent_trajectory_ref".into(), json!([Value::Object(sref)]));
                s.results.push(r);
            }
        }
        i
    }

    /// A ContentPart for a media body: image (v1.6) or audio (v1.8). Inline
    /// data is written to a file next to the trajectory.
    fn media_part(&mut self, b: &Obj, eid: &str, out: &mut Output) -> Option<Value> {
        let mt = b.get("media_type").and_then(Value::as_str).unwrap_or("");
        let (kind, mt) = if IMAGE_TYPES.contains(&mt) {
            ("image", mt.to_string())
        } else if AUDIO_TYPES.contains(&normalise_audio(mt).as_str()) {
            ("audio", normalise_audio(mt))
        } else {
            self.losses.add("media-type-unsupported", Some(Kind::Media), "ATIF holds only jpeg/png/gif/webp images and audio");
            return None;
        };
        let path = if let Some(d) = b.get("data").and_then(Value::as_str) {
            let Ok(bytes) = base64::engine::general_purpose::STANDARD.decode(d) else {
                self.losses.add("media-dropped", Some(Kind::Media), "undecodable base64");
                return None;
            };
            let dir = if kind == "image" { "images" } else { "audio" };
            let name = b
                .get("name")
                .and_then(Value::as_str)
                .map(|n| n.rsplit('/').next().unwrap_or(n).to_string())
                .filter(|n| !n.is_empty() && n.chars().all(|c| c.is_ascii_alphanumeric() || "._-".contains(c)))
                .unwrap_or_else(|| format!("{}.{}", util::short_id(eid, 12), extension_for(&mt)));
            let mut rel = format!("{dir}/{name}");
            let mut key = join_rel(&self.file, &rel).unwrap_or(rel.clone());
            if out.files.get(&key).is_some_and(|x| *x != bytes) {
                rel = format!("{dir}/{}-{name}", util::short_id(eid, 8));
                key = join_rel(&self.file, &rel).unwrap_or(rel.clone());
            }
            out.put(key, bytes);
            rel
        } else if let Some(u) = b.get("url").and_then(Value::as_str) {
            u.to_string()
        } else if let Some(p) = b.get("path").and_then(Value::as_str) {
            if !is_external(p) {
                self.losses.add("media-path-relative", Some(Kind::Media), "a relative media path from another tool may not resolve");
            }
            p.to_string()
        } else {
            self.losses.add("media-dropped", Some(Kind::Media), "media with no data, url or path");
            return None;
        };
        Some(json!({"type": kind, "source": {"media_type": mt, "path": path}}))
    }

    fn finish(&mut self, steps: &mut Vec<StepOut>) {
        let mut root = std::mem::take(&mut self.root_events);
        if self.last_neutral(steps).is_none() && (!root.is_empty() || !self.pending.is_empty() || steps.is_empty()) {
            if steps.is_empty() {
                self.losses.add("empty-trajectory-padded", None, "ATIF needs at least one step");
            }
            let e = Event::new("", Kind::Text);
            let mut s = StepOut::new("system", &e);
            s.ts = self.l.agent().created_at.clone();
            steps.push(s);
        }
        if let Some(i) = self.last_neutral(steps) {
            let pending = std::mem::take(&mut self.pending);
            steps[i].carried.extend(pending);
            let first = steps.iter().position(|s| !s.native).unwrap();
            root.append(&mut steps[first].carried);
            steps[first].carried = root;
        }
    }

    /// A root object for a ledger that did not come from ATIF.
    fn fresh_root(&self, l: &Ledger, ver: Ver, opts: &ExportOptions) -> Value {
        let a = l.agent();
        let src = a.source.as_ref();
        let mut o = Map::new();
        o.insert("schema_version".into(), json!(ver.name()));
        let session = if ver.at_least(1, 7) { src.and_then(|s| s.native_id.clone()).unwrap_or_else(|| a.id.clone()) } else { a.id.clone() };
        o.insert("session_id".into(), json!(session));
        if ver.at_least(1, 7) {
            o.insert("trajectory_id".into(), json!(a.id));
        }
        let mut agent = Map::new();
        agent.insert("name".into(), json!(src.map(|s| s.format.as_str()).unwrap_or("protoledger")));
        let version = opts.target_version.clone().or_else(|| src.and_then(|s| s.version.clone())).unwrap_or_else(|| "unknown".into());
        agent.insert("version".into(), json!(version));
        if let Some(m) = &self.agent_model {
            agent.insert("model_name".into(), json!(m));
        }
        if let Some(t) = &self.tools {
            agent.insert("tool_definitions".into(), t.clone());
        }
        o.insert("agent".into(), Value::Object(agent));
        o.insert("steps".into(), json!([]));
        if let Some(u) = &self.final_usage {
            o.insert("final_metrics".into(), Value::Object(metrics_from(u, "total_")));
        }
        Value::Object(o)
    }
}

fn obj(v: Value) -> Map<String, Value> {
    match v {
        Value::Object(m) => m,
        _ => Map::new(),
    }
}

/// `a + b` written the way the source would write it: the exact decimal
/// sum of the two numbers' spellings, with as many decimal places as the more
/// precise of them. Summing in `f64` would write artifacts such as
/// `0.011300000000000001` for `0.0045 + 0.0068`, which no source wrote and
/// which do not re-read as the same number in every build. Falls back to the
/// `f64` sum only for spellings too long for an exact `i128` sum, and is
/// `None` when that sum is not a finite number either (`1.7e308 + 1.7e308`:
/// `json!` of an infinity is `null`, a silent mis-sum).
pub(super) fn sum_like_source(a: &Value, b: &Value) -> Option<Value> {
    let exact = (|| {
        let (x, xs) = decimal(&a.to_string())?;
        let (y, ys) = decimal(&b.to_string())?;
        let scale = xs.max(ys);
        let x = x.checked_mul(10i128.checked_pow(scale - xs)?)?;
        let y = y.checked_mul(10i128.checked_pow(scale - ys)?)?;
        let sum = x.checked_add(y)?;
        let digits = sum.unsigned_abs().to_string();
        let sign = if sum < 0 { "-" } else { "" };
        let text = if scale == 0 {
            format!("{sign}{digits}")
        } else {
            let digits = format!("{digits:0>width$}", width = scale as usize + 1);
            let (int, frac) = digits.split_at(digits.len() - scale as usize);
            format!("{sign}{int}.{frac}")
        };
        // serde_json's own float parser (without `float_roundtrip`) may land
        // an ulp off; Rust's is exact, so a spelling it does not keep goes
        // through Rust's parse.
        serde_json::from_str::<Value>(&text)
            .ok()
            // Not `*v == text` (clippy::cmp_owned's suggestion): that compares a
            // number `Value` with a JSON string and is always false. The check is
            // whether the parsed number prints back with the same spelling.
            .filter(|v| {
                #[allow(clippy::cmp_owned)]
                let same = v.to_string() == text;
                same
            })
            .or_else(|| text.parse::<f64>().ok().map(|f| json!(f)))
    })();
    if exact.is_some() {
        return exact;
    }
    // An operand that is not an f64 is never read as 0.
    let sum = a.as_f64()? + b.as_f64()?;
    if !sum.is_finite() {
        return None;
    }
    Some(if a.is_u64() && b.is_u64() && sum < u64::MAX as f64 { json!(sum as u64) } else { json!(sum) })
}

/// A JSON number's spelling as an integer mantissa and a count of decimal
/// places: `-1.50` → (-150, 2), `2.5e-3` → (25, 4), `1e3` → (1000, 0).
fn decimal(s: &str) -> Option<(i128, u32)> {
    let (neg, s) = match s.strip_prefix('-') {
        Some(r) => (true, r),
        None => (false, s),
    };
    // Checked throughout: an exponent of i32::MIN (`1e-2147483648`) negated
    // in i32 overflowed and panicked.
    let (num, exp) = match s.find(['e', 'E']) {
        Some(i) => (&s[..i], s[i + 1..].parse::<i64>().ok()?),
        None => (s, 0),
    };
    let (int, frac) = num.split_once('.').unwrap_or((num, ""));
    if int.is_empty() || !int.bytes().chain(frac.bytes()).all(|c| c.is_ascii_digit()) {
        return None;
    }
    let mut m: i128 = format!("{int}{frac}").parse().ok()?;
    let mut scale = (frac.len() as i64).checked_sub(exp)?;
    if scale < 0 {
        m = m.checked_mul(10i128.checked_pow(u32::try_from(scale.checked_neg()?).ok()?)?)?;
        scale = 0;
    }
    let scale = u32::try_from(scale).ok().filter(|s| *s <= 30)?;
    Some((if neg { -m } else { m }, scale))
}

/// ATIF metrics (prefix "") or final_metrics ("total_") from a usage body.
fn metrics_from(b: &Obj, prefix: &str) -> Map<String, Value> {
    let int = |k: &str| b.get(k).and_then(Value::as_u64);
    let mut m = Map::new();
    let (input, read, write) = (int("input"), int("cache_read"), int("cache_write"));
    if input.is_some() || read.is_some() || write.is_some() {
        m.insert(format!("{prefix}prompt_tokens"), json!(input.unwrap_or(0) + read.unwrap_or(0) + write.unwrap_or(0)));
    }
    if let Some(v) = b.get("output").filter(|v| !v.is_null()) {
        m.insert(format!("{prefix}completion_tokens"), v.clone());
    }
    if let Some(r) = read {
        m.insert(format!("{prefix}cached_tokens"), json!(r));
    }
    if let Some(c) = b.get("cost_usd").filter(|v| !v.is_null()) {
        m.insert(format!("{prefix}cost_usd"), c.clone());
    }
    let mut x = Map::new();
    if let Some(w) = write {
        x.insert("cache_write_tokens".into(), json!(w));
    }
    if let Some(r) = int("reasoning") {
        x.insert("reasoning_tokens".into(), json!(r));
    }
    if !x.is_empty() {
        m.insert("extra".into(), Value::Object(x));
    }
    m
}

/// OpenAI-style function definitions from a neutral `tools` value.
fn tool_definitions(v: &Value) -> Option<Value> {
    let arr = v.as_array()?;
    let mut out = Vec::new();
    for t in arr {
        let o = t.as_object()?;
        if o.contains_key("function") {
            out.push(t.clone());
        } else {
            let name = o.get("name")?;
            let mut f = Map::new();
            f.insert("name".into(), name.clone());
            if let Some(d) = o.get("description") {
                f.insert("description".into(), d.clone());
            }
            if let Some(p) = o.get("parameters").or_else(|| o.get("input_schema")) {
                f.insert("parameters".into(), p.clone());
            }
            out.push(json!({"type": "function", "function": f}));
        }
    }
    Some(Value::Array(out))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::formats::{Format, ImportOptions};
    use crate::model::{Agent, Parent};

    fn ev(id: &str, kind: Kind, body: Value) -> Event {
        let mut e = Event::new(id, kind);
        e.body = obj(body);
        e
    }

    fn sample() -> LedgerSet {
        let mut l = Ledger::new(Agent {
            id: "main".into(),
            created_at: Some("2026-10-16T10:00:00.000Z".into()),
            source: None,
            parent: None,
            links: vec![],
            meta: Obj::new(),
        });
        let evs = vec![
            ev("s", Kind::Settings, json!({"model": "m1", "cwd": "/w"})),
            ev("u", Kind::Text, json!({"role": "user", "text": "hi"})),
            ev("r", Kind::Reasoning, json!({"text": "hmm", "sealed": {"vendor": "x", "kind": "signature", "data": "zz"}})).group(Some("g1")),
            ev("c", Kind::ToolCall, json!({"call_id": "c1", "name": "Task", "input": {"p": 1}})).group(Some("g1")),
            ev("sa", Kind::Subagent, json!({"phase": "start", "agent": "kid", "call_id": "c1", "description": "look"})),
            ev("tr", Kind::ToolResult, json!({"call_id": "c1", "output": "found", "is_error": false})),
            ev("t", Kind::Text, json!({"role": "assistant", "text": "done"})).group(Some("g2")),
            ev("us", Kind::Usage, json!({"model": "m1", "input": 10, "output": 2, "cache_read": 5})).group(Some("g2")),
            ev("fc", Kind::FileChange, json!({"path": "a", "op": "edit"})),
        ];
        for e in evs {
            l.append(e.ts(Some("2026-10-16T10:00:01Z"))).unwrap();
        }
        let mut k = Ledger::new(Agent {
            id: "kid".into(),
            created_at: None,
            source: None,
            parent: Some(Parent { agent: "main".into(), event: Some("sa".into()), relation: Relation::Subagent }),
            links: vec![],
            meta: Obj::new(),
        });
        k.append(ev("k1", Kind::Text, json!({"role": "user", "text": "look"}))).unwrap();
        let mut set = LedgerSet::single(l);
        set.insert(k);
        set
    }

    #[test]
    fn foreign_ledger_renders_valid_shape() {
        let ex = export(&sample(), &ExportOptions::no_carry()).unwrap();
        let doc: Value = serde_json::from_slice(&ex.output.files["trajectory.json"]).unwrap();
        assert_eq!(doc["schema_version"], json!("ATIF-v1.7"));
        assert_eq!(doc["agent"]["model_name"], json!("m1"));
        let steps = doc["steps"].as_array().unwrap();
        assert_eq!(steps.len(), 3);
        assert_eq!(steps[1]["tool_calls"][0]["tool_call_id"], json!("c1"));
        let r = &steps[1]["observation"]["results"][0];
        assert_eq!(r["source_call_id"], json!("c1"));
        assert_eq!(r["content"], json!("found"));
        assert_eq!(r["subagent_trajectory_ref"][0]["trajectory_id"], json!("kid"));
        assert_eq!(steps[2]["metrics"]["prompt_tokens"], json!(15));
        assert_eq!(doc["subagent_trajectories"][0]["trajectory_id"], json!("kid"));
        let codes: Vec<&str> = ex.losses.0.iter().map(|l| l.code.as_str()).collect();
        assert!(codes.contains(&"reasoning-sealed-dropped") && codes.contains(&"file-change-dropped") && codes.contains(&"settings-dropped"));
    }

    #[test]
    fn carry_restores_every_event() {
        let set = sample();
        let ex = export(&set, &ExportOptions::standard()).unwrap();
        let dir = tempfile::tempdir().unwrap();
        let p = crate::roundtrip::materialise(&ex.output, dir.path()).unwrap();
        let back = super::super::Atif.import(&p, &ImportOptions::standard()).unwrap().set;
        let d = crate::loss::compare(&set, &back, crate::loss::CompareOptions { ignore_native: true, ..Default::default() });
        assert!(d.is_clean(), "{}", d.summary());
    }

    #[test]
    fn old_versions_split_subagents_into_files() {
        let ex = export(&sample(), &ExportOptions { variant: Some("1.2".into()), carry: false, target_version: None }).unwrap();
        assert!(ex.output.files.contains_key("trajectory.kid.json"));
        let doc: Value = serde_json::from_slice(&ex.output.files["trajectory.json"]).unwrap();
        let sref = &doc["steps"][1]["observation"]["results"][0]["subagent_trajectory_ref"][0];
        assert_eq!(sref["trajectory_path"], json!("trajectory.kid.json"));
        assert!(doc.get("trajectory_id").is_none());
        assert_eq!(doc["session_id"], json!("main"));
    }

    /// A trajectory spelled the way a parsed `Value` does not re-spell it under
    /// any feature set: keys out of order, the escape `\u00e9` beside a raw
    /// `ü` (so no ASCII style re-escapes it), `1e3`, `0.000005`.
    const ODD: &str = "{\n  \"schema_version\": \"ATIF-v1.6\",\n  \"session_id\": \"s1\",\n  \"agent\": {\n    \"name\": \"x\",\n    \"version\": \"1\",\n    \"model_name\": \"m\"\n  },\n  \"steps\": [\n    {\n      \"step_id\": 1,\n      \"source\": \"user\",\n      \"message\": \"caf\\u00e9 \u{fc}\"\n    },\n    {\n      \"step_id\": 2,\n      \"source\": \"agent\",\n      \"message\": \"ok\",\n      \"metrics\": {\n        \"prompt_tokens\": 1e3,\n        \"cost_usd\": 0.000005\n      }\n    }\n  ]\n}\n";

    /// Steps and the root skeleton are held as their own compact text, and
    /// the file is laid out again from those texts byte for byte.
    #[test]
    fn records_are_held_and_reemitted_as_spelled() {
        let dir = tempfile::tempdir().unwrap();
        let p = dir.path().join("trajectory.json");
        std::fs::write(&p, ODD).unwrap();
        let set = super::super::Atif.import(&p, &ImportOptions::standard()).unwrap().set;
        let l = set.iter().next().unwrap();
        let held: Vec<&str> = l.events.iter().filter_map(|e| e.native.as_ref()).filter(|n| n.part == 0).filter_map(|n| n.record_text()).collect();
        assert_eq!(
            held,
            [
                r#"{"schema_version":"ATIF-v1.6","session_id":"s1","agent":{"name":"x","version":"1","model_name":"m"},"steps":[]}"#,
                "{\"step_id\":1,\"source\":\"user\",\"message\":\"caf\\u00e9 \u{fc}\"}",
                r#"{"step_id":2,"source":"agent","message":"ok","metrics":{"prompt_tokens":1e3,"cost_usd":0.000005}}"#,
            ]
        );
        let ex = export(&set, &ExportOptions::standard()).unwrap();
        assert_eq!(std::str::from_utf8(&ex.output.files["trajectory.json"]).unwrap(), ODD);
        // From the records' own texts alone (no `raw`), too.
        let ex = export(&jsonfmt::without_raw(&set), &ExportOptions::standard()).unwrap();
        assert_eq!(std::str::from_utf8(&ex.output.files["trajectory.json"]).unwrap(), ODD);
    }

    /// An unchanged record whose `raw` is a spelling within
    /// `util::NUMBER_ULPS` of its record (so `current_raw` writes it) cannot be
    /// laid back into the document it came from: the rebuilt text no longer
    /// parses to the document's value, the exporter falls back to the value,
    /// and that is named.
    #[test]
    fn an_unchanged_document_not_rebuilt_from_its_texts_names_the_fallback() {
        let dir = tempfile::tempdir().unwrap();
        let p = dir.path().join("trajectory.json");
        std::fs::write(&p, ODD).unwrap();
        let set = super::super::Atif.import(&p, &ImportOptions::standard()).unwrap().set;
        let has = |ex: &Exported| ex.losses.0.iter().any(|l| l.code == jsonfmt::EXACT_TEXT_FALLBACK);
        // Opposed: as imported, it is rebuilt from its texts and names nothing.
        let ex = export(&set, &ExportOptions::standard()).unwrap();
        assert_eq!(std::str::from_utf8(&ex.output.files["trajectory.json"]).unwrap(), ODD);
        assert!(!has(&ex), "{:?}", ex.losses);
        let ex = export(&nudged(&set, "0.000005", "5.000000000000001e-06"), &ExportOptions::standard()).unwrap();
        assert!(has(&ex), "the fallback is named: {:?}", ex.losses);
    }

    fn nudged(set: &LedgerSet, from: &str, to: &str) -> LedgerSet {
        let mut set = set.clone();
        let id = set.iter().next().unwrap().id().to_string();
        let l = set.ledgers.get_mut(&id).unwrap();
        let n = l
            .events
            .iter_mut()
            .filter_map(|e| e.native.as_mut())
            .find(|n| n.part == 0 && n.record_text().is_some_and(|t| t.contains(from)))
            .expect("the record to nudge");
        n.raw = Some(n.record_text().unwrap().replace(from, to));
        assert_eq!(crate::util::current_raw(n), n.raw.as_deref(), "sentinel: the nudged raw still stands for the record");
        set
    }

    /// A file kept whole (no layout reproduces it) is written back
    /// from its `raw` only while that still says what its records say.
    #[test]
    fn a_whole_file_raw_is_written_only_while_its_records_say_it() {
        let p = concat!(env!("CARGO_MANIFEST_DIR"), "/fixtures/atif/v1.7-letta-embedded-subagent/native/trajectory.json");
        let src = std::fs::read_to_string(p).unwrap();
        let set = super::super::Atif.import(std::path::Path::new(p), &ImportOptions::standard()).unwrap().set;
        let root = set.iter().find(|l| whole_file_raw(l)).expect("sentinel: the fixture is kept whole").id().to_string();
        // Opposed: unchanged, it is the file byte for byte, which only its
        // `raw` gives (from the records alone the layout is not the file's).
        let out = |s: &LedgerSet| String::from_utf8(export(s, &ExportOptions::standard()).unwrap().output.files["trajectory.json"].clone()).unwrap();
        assert_eq!(out(&set), src, "unchanged: the file as read");
        assert_ne!(out(&jsonfmt::without_raw(&set)), src, "sentinel: without raw the layout is not reproduced");
        // Edited: one step's text changed in the ledger, the stale `raw` kept.
        let mut edited = set.clone();
        let l = edited.ledgers.get_mut(&root).unwrap();
        let n = l
            .events
            .iter_mut()
            .filter_map(|e| e.native.as_mut())
            .find(|n| n.record_text().is_some_and(|t| t.contains("The check completed.")))
            .expect("the step");
        let t = n.record_text().unwrap().replace("The check completed.", "EDITED");
        *n = n.clone().record_raw(&t).unwrap();
        let got = out(&edited);
        assert!(got.contains("EDITED") && !got.contains("The check completed."), "the edit is written, not the stale whole file: {got}");
    }

    /// One agent step whose two usage events carry `costs`, exported; returns
    /// the file's text and its step's `metrics.cost_usd` as written.
    fn summed_cost(costs: [Value; 2]) -> (String, Value) {
        let mut l = Ledger::new(Agent { id: "main".into(), created_at: None, source: None, parent: None, links: vec![], meta: Obj::new() });
        l.append(ev("t", Kind::Text, json!({"role": "assistant", "text": "x"})).group(Some("g"))).unwrap();
        for (i, c) in costs.into_iter().enumerate() {
            l.append(ev(&format!("u{i}"), Kind::Usage, json!({"input": 1, "output": 1, "cost_usd": c})).group(Some("g"))).unwrap();
        }
        let ex = export(&LedgerSet::single(l), &ExportOptions::no_carry()).unwrap();
        let text = String::from_utf8(ex.output.files["trajectory.json"].clone()).unwrap();
        let doc: Value = serde_json::from_str(&text).unwrap();
        (text, doc["steps"][0]["metrics"]["cost_usd"].clone())
    }

    /// A summed cost is written as the source would write it (no f64
    /// artifact) and re-reads to the value written; a source cost with genuine
    /// precision is kept, not rounded away.
    #[test]
    fn summed_numbers_are_written_as_the_source_would() {
        // The opencode fixtures' own step costs. Sentinel: their f64 sum does
        // carry the artifact, so this probe can fail. (0.0045 + 0.0068 cannot:
        // its f64 sum prints as 0.0113.)
        assert_eq!((0.0042f64 + 0.0071).to_string(), "0.011300000000000001");
        for pair in [[json!(0.0042), json!(0.0071)], [json!(0.0045), json!(0.0068)]] {
            let (text, cost) = summed_cost(pair);
            assert!(text.contains("\"cost_usd\": 0.0113\n") || text.contains("\"cost_usd\": 0.0113,"), "{text}");
            assert!(!text.contains("0.011300000000000001"), "{text}");
            assert_eq!(cost, json!(0.0113));
            assert_eq!(serde_json::from_str::<Value>(&cost.to_string()).unwrap(), cost, "re-reads to the same value");
        }
        // Opposed: genuine precision in the source survives the sum.
        // (Checked on the text: serde_json without `float_roundtrip` reads
        // this spelling back an ulp off, which is the reader's, not the sum's.)
        let (text, _) = summed_cost([json!(0.011300000000000001), json!(0)]);
        assert!(text.contains("\"cost_usd\": 0.011300000000000001"), "{text}");
        let sum = |a: Value, b: Value| sum_like_source(&a, &b).map(|v| v.to_string()).unwrap_or_else(|| "None".into());
        assert_eq!(sum(json!(0.011300000000000001), json!(0)), "0.011300000000000001");
        // Integers stay integers; decimal places follow the more precise operand.
        assert_eq!(sum(json!(3), json!(4)), "7");
        assert_eq!(sum(json!(1.5), json!(0.25)), "1.75");
        assert_eq!(sum(json!(-0.5), json!(0.25)), "-0.25");
        assert_eq!(decimal("2.5e-3"), Some((25, 4)));
        assert_eq!(decimal("-1.50"), Some((-150, 2)));
        assert_eq!(decimal("1e3"), Some((1000, 0)));
    }

    /// `decimal` on an exponent of i32::MIN
    /// overflowed and panicked; an f64 sum past f64::MAX was written as
    /// `null`. Now `decimal` declines both extremes and the sum is `None`,
    /// which the exporter names as a loss and keeps the earlier value.
    #[test]
    fn extreme_spellings_neither_panic_nor_mis_sum() {
        assert_eq!(decimal("1e-2147483648"), None);
        assert_eq!(decimal("1e2147483647"), None);
        assert_eq!(decimal("1e-9223372036854775808"), None);
        // Opposed: an ordinary exponent still reads.
        assert_eq!(decimal("25e-4"), Some((25, 4)));
        let max = json!(f64::MAX);
        assert_eq!(sum_like_source(&max, &max), None, "f64::MAX + f64::MAX is not a JSON number");
        // Opposed: an f64 sum that stays finite is written, and the exact path still runs.
        assert_eq!(sum_like_source(&max, &json!(-1e308)).map(|v| v.as_f64().unwrap()), Some(f64::MAX - 1e308));
        assert_eq!(
            sum_like_source(&json!(u64::MAX), &json!(1)).map(|v| v.to_string()),
            Some("1.8446744073709552e+19".into()),
            "the exact i128 sum, re-read as the f64 a Value holds"
        );
        // Through the exporter: the step keeps its first cost and names the loss.
        let mut l = Ledger::new(Agent { id: "main".into(), created_at: None, source: None, parent: None, links: vec![], meta: Obj::new() });
        l.append(ev("t", Kind::Text, json!({"role": "assistant", "text": "x"})).group(Some("g"))).unwrap();
        for i in 0..2 {
            l.append(ev(&format!("u{i}"), Kind::Usage, json!({"input": 1, "output": 1, "cost_usd": f64::MAX})).group(Some("g"))).unwrap();
        }
        let ex = export(&LedgerSet::single(l), &ExportOptions::no_carry()).unwrap();
        let doc: Value = serde_json::from_slice(&ex.output.files["trajectory.json"]).unwrap();
        assert_eq!(doc["steps"][0]["metrics"]["cost_usd"], json!(f64::MAX), "{doc}");
        assert!(ex.losses.0.iter().any(|l| l.code == "usage-sum-unrepresentable"), "{:?}", ex.losses);
    }

    /// Found by the mutation harness (seed 0x77, atif/v1.5-rfc-example): a held
    /// step that is not an object panicked on `v["step_id"] = ...`. It is
    /// written as read (a held step carries nothing, so `carry::attach` returns
    /// before touching it). Opposed: the object steps beside it are renumbered.
    #[test]
    fn a_step_that_is_not_an_object_is_written_as_read() {
        let dir = tempfile::tempdir().unwrap();
        let p = dir.path().join("trajectory.json");
        std::fs::write(&p, r#"{"schema_version":"ATIF-v1.5","session_id":"s","agent":{"name":"a","version":"1"},"steps":[{"step_id":1,"source":"user","message":"hi"},7,{"step_id":9,"source":"agent","message":"yo"}]}"#).unwrap();
        let imp = crate::formats::atif::Atif.import(&p, &ImportOptions::standard()).unwrap();
        for opts in [ExportOptions::standard(), ExportOptions::no_carry()] {
            let ex = export(&imp.set, &opts).unwrap();
            let doc: Value = serde_json::from_slice(&ex.output.files["trajectory.json"]).unwrap();
            let steps = doc["steps"].as_array().unwrap();
            assert!(steps.contains(&json!(7)), "{doc}");
            assert!(steps.iter().filter(|s| s.is_object()).all(|s| s["step_id"].is_u64()), "{doc}");
        }
    }

    #[test]
    fn rel_paths() {
        assert_eq!(rel_from("trajectory.json", "trajectory.kid.json"), "trajectory.kid.json");
        assert_eq!(rel_from("a/t.json", "a/k.json"), "k.json");
        assert_eq!(rel_from("a/t.json", "b/k.json"), "../b/k.json");
    }
}
