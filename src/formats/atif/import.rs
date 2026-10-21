//! ATIF → protoledger.
//!
//! Each trajectory document (the file's root, every embedded
//! `subagent_trajectories` entry, every sibling file reached by a
//! `trajectory_path` or `continued_trajectory_ref`) becomes one ledger. The
//! document's root object (with its `steps` emptied) is the native record of a
//! leading `settings` event; each step is the native record of the events made
//! from it; `final_metrics` becomes a trailing cumulative `usage` event that is
//! part 1 of the root record.

use super::jsonfmt::{self, Style};
use super::{FORMAT, Ver, doc_version, join_rel, variant_name};
use crate::carry;
use crate::formats::{ImportOptions, Imported, Losses};
use crate::ledger::{Ledger, LedgerSet};
use crate::model::{Agent, Event, Kind, Link, Native, Obj, Parent, Relation, Source};
use anyhow::{Context, Result, anyhow, bail};
use base64::Engine;
use serde_json::{Value, json};
use std::collections::{BTreeMap, HashMap, HashSet};
use std::path::{Path, PathBuf};

pub const ROOT_ID: &str = "trajectory";

/// Every ATIF document under `dir`, as (relative path, text, parsed), with
/// `trajectory.json` first and the rest in path order.
pub(super) fn atif_files(dir: &Path) -> Vec<(String, String, Value)> {
    let mut out = Vec::new();
    for e in walkdir::WalkDir::new(dir).sort_by_file_name().into_iter().filter_map(|e| e.ok()) {
        if !e.file_type().is_file() || e.path().extension().is_none_or(|x| x != "json") {
            continue;
        }
        let Ok(rel) = e.path().strip_prefix(dir) else { continue };
        let rel = rel.to_string_lossy().replace('\\', "/");
        if rel.starts_with('.') || rel == "expect.json" {
            continue;
        }
        let Ok(text) = std::fs::read_to_string(e.path()) else { continue };
        let Ok(v) = serde_json::from_str::<Value>(&text) else { continue };
        if doc_version(&v).is_some() {
            out.push((rel, text, v));
        }
    }
    out.sort_by_key(|(p, _, _)| (p != "trajectory.json", p.clone()));
    out
}

/// Every `trajectory_path` and `continued_trajectory_ref` in a document,
/// including its embedded subagents.
fn doc_refs(doc: &Value, out: &mut Vec<String>) {
    if let Some(s) = doc.get("continued_trajectory_ref").and_then(Value::as_str) {
        out.push(s.to_string());
    }
    for step in doc.get("steps").and_then(Value::as_array).into_iter().flatten() {
        for r in results(step) {
            for sref in r.get("subagent_trajectory_ref").and_then(Value::as_array).into_iter().flatten() {
                if let Some(p) = sref.get("trajectory_path").and_then(Value::as_str) {
                    out.push(p.to_string());
                }
            }
        }
    }
    for sub in doc.get("subagent_trajectories").and_then(Value::as_array).into_iter().flatten() {
        doc_refs(sub, out);
    }
}

fn results(step: &Value) -> impl Iterator<Item = &Value> {
    step.get("observation").and_then(|o| o.get("results")).and_then(Value::as_array).into_iter().flatten()
}

fn s(v: &Value, k: &str) -> Option<String> {
    v.get(k).and_then(Value::as_str).map(str::to_string)
}

fn stem(file: &str) -> String {
    let base = file.rsplit('/').next().unwrap_or(file);
    base.strip_suffix(".json").unwrap_or(base).to_string()
}

pub(super) fn import(input: &Path, opts: &ImportOptions) -> Result<Imported> {
    let (root, docs, multi) = if input.is_file() {
        let text = std::fs::read_to_string(input).with_context(|| input.display().to_string())?;
        let doc: Value = serde_json::from_str(&text).with_context(|| format!("{} is not JSON", input.display()))?;
        doc_version(&doc).ok_or_else(|| anyhow!("{} has no ATIF schema_version", input.display()))?;
        let name = input.file_name().unwrap().to_string_lossy().to_string();
        (input.parent().map(Path::to_path_buf).unwrap_or_default(), vec![(name, text, doc)], false)
    } else {
        (input.to_path_buf(), atif_files(input), true)
    };
    if docs.is_empty() {
        bail!("{}: no ATIF trajectory found", input.display());
    }
    let variant = doc_version(&docs[0].2).map(variant_name).unwrap_or("ATIF-v1.7").to_string();
    let mut im = Importer {
        root,
        multi,
        restore: opts.restore_carried,
        docs: docs.iter().map(|(p, t, v)| (p.clone(), (t.clone(), v.clone()))).collect(),
        done: HashSet::new(),
        ids: HashSet::new(),
        used_files: HashSet::new(),
        set: LedgerSet::new(),
        losses: Losses::default(),
    };
    let mut referenced = HashSet::new();
    for (rel, _, doc) in &docs {
        let mut refs = Vec::new();
        doc_refs(doc, &mut refs);
        referenced.extend(refs.iter().filter_map(|p| join_rel(rel, p)));
    }
    let order: Vec<&String> = docs.iter().map(|(p, _, _)| p).filter(|p| !referenced.contains(*p)).chain(docs.iter().map(|(p, _, _)| p)).collect();
    for rel in order {
        if !im.done.contains(rel) {
            im.import_file(rel, None, None, Vec::new())?;
        }
    }
    if multi {
        let mut stray = 0;
        for e in walkdir::WalkDir::new(input).into_iter().filter_map(|e| e.ok()) {
            if !e.file_type().is_file() {
                continue;
            }
            let rel = e.path().strip_prefix(input)?.to_string_lossy().replace('\\', "/");
            if rel.starts_with('.') || rel == "SOURCE.md" || rel == "expect.json" {
                continue;
            }
            if !im.done.contains(&rel) && !im.used_files.contains(&rel) {
                stray += 1;
            }
        }
        im.losses.add_n("file-not-imported", None, stray, "a file in the trajectory directory that no trajectory references");
    }
    Ok(Imported { set: im.set, losses: im.losses, variant })
}

struct Importer {
    root: PathBuf,
    multi: bool,
    restore: bool,
    docs: BTreeMap<String, (String, Value)>,
    done: HashSet<String>,
    ids: HashSet<String>,
    used_files: HashSet<String>,
    set: LedgerSet,
    losses: Losses,
}

/// Per-document mapping state.
struct Doc {
    file: String,
    native: Native,
    default_model: Option<String>,
    cur_model: Option<String>,
    cur_effort: Option<Value>,
    turn: Option<String>,
    /// embedded trajectory_id → child agent id
    embedded: HashMap<String, String>,
    /// resolved trajectory_path → child agent id
    files: HashMap<String, String>,
}

/// An event plus, when it was restored from a carrier, its original seq.
type Ev = (Event, Option<u64>);

impl Importer {
    fn reserve(&mut self, want: String, fallback: impl Fn() -> String) -> String {
        let mut id = want;
        if self.ids.contains(&id) {
            id = fallback();
        }
        let base = id.clone();
        let mut n = 2;
        while self.ids.contains(&id) {
            id = format!("{base}~{n}");
            n += 1;
        }
        self.ids.insert(id.clone());
        id
    }

    fn file_id(&mut self, rel: &str) -> String {
        let doc = &self.docs[rel].1;
        let want = s(doc, "trajectory_id").or_else(|| s(doc, "session_id")).unwrap_or_else(|| stem(rel));
        let st = stem(rel);
        self.reserve(want.clone(), || format!("{want}:{st}"))
    }

    fn import_file(&mut self, rel: &str, id: Option<String>, parent: Option<Parent>, links: Vec<Link>) -> Result<String> {
        self.done.insert(rel.to_string());
        let id = match id {
            Some(i) => i,
            None => self.file_id(rel),
        };
        let (text, doc) = self.docs[rel].clone();
        let compact = jsonfmt::minify(&text);
        self.import_doc(&doc, rel, Some(&text), Some(&compact), id, parent, None, links)
    }

    #[allow(clippy::too_many_arguments)]
    fn import_doc(
        &mut self,
        doc: &Value,
        file: &str,
        text: Option<&str>,
        compact: Option<&str>,
        mut id: String,
        parent: Option<Parent>,
        embedded: Option<usize>,
        mut links: Vec<Link>,
    ) -> Result<String> {
        let vname = s(doc, "schema_version").unwrap_or_default();
        let ver = Ver::parse(&vname).unwrap_or(Ver(1, 0));
        let agent_obj = doc.get("agent").cloned().unwrap_or(Value::Null);
        let agent_version = s(&agent_obj, "version");
        let mut extra = Obj::new();
        match embedded {
            Some(i) => {
                extra.insert("embedded".into(), json!(i));
            }
            None => {
                extra.insert("file".into(), json!(file));
            }
        }
        if let Some(n) = s(&agent_obj, "name") {
            extra.insert("agent_name".into(), json!(n));
        }
        let mut raw = None;
        if let Some(t) = text {
            match Style::detect(t, doc) {
                Some(st) => {
                    extra.insert("json".into(), st.to_value());
                }
                None => {
                    extra.insert("json".into(), json!("raw"));
                    raw = Some(t.to_string());
                }
            }
        }
        let mut created_at = None;
        let mut meta = Obj::new();
        // Header carried by a protoledger export (root extra), restored as is.
        if self.restore
            && let Some(h) = doc.get("extra").and_then(|e| e.get(carry::KEY)).and_then(|c| c.get("agent"))
        {
            if let Some(cid) = s(h, "id")
                && cid != id
                && !self.ids.contains(&cid)
            {
                self.ids.remove(&id);
                self.ids.insert(cid.clone());
                id = cid;
            }
            created_at = s(h, "created_at");
            if let Some(m) = h.get("meta").and_then(Value::as_object) {
                meta = m.clone();
            }
            if let Some(ls) = h.get("links")
                && let Ok(ls) = serde_json::from_value::<Vec<Link>>(ls.clone())
            {
                links.extend(ls);
            }
        }
        let native =
            Native::new(FORMAT, None).variant(Some(vname.clone())).version(agent_version.clone()).file(if self.multi { Some(file.to_string()) } else { None });
        let default_model = s(&agent_obj, "model_name");
        let mut st = Doc {
            file: file.to_string(),
            native: native.clone(),
            default_model: default_model.clone(),
            cur_model: default_model.clone(),
            cur_effort: None,
            turn: None,
            embedded: HashMap::new(),
            files: HashMap::new(),
        };

        // Child ids, reserved before any event names them.
        let subs: Vec<Value> = doc.get("subagent_trajectories").and_then(Value::as_array).cloned().unwrap_or_default();
        // Each record as the source spelled it (SPEC §4): the compact text of
        // every step and embedded trajectory, cut from the document's text.
        let texts_of = |key: &str, n: usize| -> Vec<Option<String>> {
            let found = compact.and_then(|c| jsonfmt::member(c, key)).and_then(jsonfmt::items).filter(|t| t.len() == n);
            match found {
                Some(t) => t.into_iter().map(|x| Some(x.to_string())).collect(),
                None => vec![None; n],
            }
        };
        let sub_texts = texts_of("subagent_trajectories", subs.len());
        let mut sub_ids = Vec::new();
        for (i, sub) in subs.iter().enumerate() {
            let want = s(sub, "trajectory_id").or_else(|| s(sub, "session_id")).unwrap_or_else(|| format!("{id}.sub-{i}"));
            let pid = id.clone();
            let w2 = want.clone();
            let cid = self.reserve(want.clone(), || format!("{pid}/{w2}"));
            if let Some(t) = s(sub, "trajectory_id") {
                st.embedded.insert(t, cid.clone());
            }
            sub_ids.push(cid);
        }
        let mut file_children = Vec::new();
        for step in doc.get("steps").and_then(Value::as_array).into_iter().flatten() {
            for r in results(step) {
                for sref in r.get("subagent_trajectory_ref").and_then(Value::as_array).into_iter().flatten() {
                    let Some(p) = sref.get("trajectory_path").and_then(Value::as_str) else { continue };
                    let Some(rel) = join_rel(file, p) else { continue };
                    if self.docs.contains_key(&rel) && !self.done.contains(&rel) && !st.files.contains_key(&rel) {
                        let cid = self.file_id(&rel);
                        st.files.insert(rel.clone(), cid);
                        file_children.push(rel);
                    }
                }
            }
        }
        let mut continuation = None;
        if let Some(c) = doc.get("continued_trajectory_ref").and_then(Value::as_str)
            && let Some(rel) = join_rel(file, c)
            && self.docs.contains_key(&rel)
            && !self.done.contains(&rel)
            && !st.files.contains_key(&rel)
        {
            let cid = self.file_id(&rel);
            links.push(Link { rel: "continued-in".into(), agent: Some(cid.clone()), event: None, extra: Obj::new() });
            continuation = Some((rel, cid));
        }

        // Events.
        let mut evs: Vec<Ev> = Vec::new();
        let steps: Vec<Value> = doc.get("steps").and_then(Value::as_array).cloned().unwrap_or_default();
        let step_texts = texts_of("steps", steps.len());
        let mut step_evs: Vec<Vec<Ev>> = Vec::new();
        let mut first_restored = false;
        for (i, step) in steps.iter().enumerate() {
            let mut out = Vec::new();
            let mut rec = step.clone();
            if self.restore
                && let Some(carried) = carry::take(&mut rec, &["extra"])
            {
                if i == 0 {
                    first_restored = true;
                }
                out.extend(carried.into_iter().map(|e| {
                    let seq = e.seq;
                    (e, Some(seq))
                }));
                step_evs.push(out);
                continue;
            }
            for e in self.map_step(&mut st, step, &rec, step_texts[i].as_deref()) {
                out.push((e, None));
            }
            step_evs.push(out);
        }
        if !first_restored {
            let mut skel = doc.clone();
            if let Some(o) = skel.as_object_mut() {
                if o.get("steps").is_some_and(Value::is_array) {
                    o.insert("steps".into(), json!([]));
                }
                if o.get("subagent_trajectories").is_some_and(Value::is_array) {
                    o.insert("subagent_trajectories".into(), json!([]));
                }
            }
            let mut body = Obj::new();
            if let Some(m) = &default_model {
                body.insert("model".into(), json!(m));
            }
            if let Some(t) = agent_obj.get("tool_definitions").filter(|t| !t.is_null()) {
                body.insert("tools".into(), t.clone());
            }
            let kind = if body.is_empty() { Kind::Native } else { Kind::Settings };
            let mut n = st.native.clone();
            n.id = Some(ROOT_ID.into());
            // The skeleton's text: the document's own, its lists emptied in place.
            let skel_text = compact.and_then(|c| {
                let mut t = c.to_string();
                for key in ["steps", "subagent_trajectories"] {
                    if doc.get(key).is_some_and(Value::is_array) {
                        t = jsonfmt::with_member(&t, key, "[]")?;
                    }
                }
                Some(t)
            });
            // With no layout to lay it out again, `raw` is the whole file's
            // text; otherwise it is the skeleton's, when a parsed value would
            // re-spell it.
            match raw {
                Some(file_text) => {
                    jsonfmt::hold(&mut n, skel_text.as_deref(), &skel);
                    n.raw = Some(file_text);
                }
                None => jsonfmt::hold(&mut n, skel_text.as_deref(), &skel),
            }
            let mut e = Event::new(ROOT_ID, kind);
            e.body = body;
            e.ts = steps.iter().find_map(|x| s(x, "timestamp"));
            e.native = Some(n.clone());
            evs.push((e, None));
            for v in step_evs.drain(..) {
                evs.extend(v);
            }
            if let Some(fm) = doc.get("final_metrics").filter(|v| v.is_object()) {
                let mut body = usage_body(fm, "total_", default_model.as_deref()).unwrap_or_default();
                body.insert("cumulative".into(), json!(true));
                let mut e = Event::new(format!("{ROOT_ID}#1"), Kind::Usage);
                e.body = body;
                e.ts = steps.iter().rev().find_map(|x| s(x, "timestamp"));
                e.native = Some(n.part_of(1));
                evs.push((e, None));
            }
        } else {
            for v in step_evs.drain(..) {
                evs.extend(v);
            }
        }
        sort_restored_runs(&mut evs);
        if created_at.is_none() {
            created_at = steps.iter().find_map(|x| s(x, "timestamp"));
        }

        let agent = Agent {
            id: id.clone(),
            created_at,
            source: Some(Source { format: FORMAT.into(), variant: Some(vname.clone()), version: agent_version, native_id: s(doc, "session_id"), extra }),
            parent,
            links,
            meta,
        };
        let mut l = Ledger::new(agent);
        for (e, _) in evs {
            l.append_unique(e);
        }
        let start_of = |l: &Ledger, child: &str| {
            l.events.iter().find(|e| e.kind == Kind::Subagent && e.str("agent") == Some(child) && e.str("phase") != Some("end")).map(|e| e.id.clone())
        };
        let mut children = Vec::new();
        for (i, sub) in subs.iter().enumerate() {
            children.push((sub_ids[i].clone(), start_of(&l, &sub_ids[i]), i, sub.clone(), sub_texts[i].clone()));
        }
        let file_parents: Vec<(String, String, Option<String>)> =
            file_children.iter().map(|rel| (rel.clone(), st.files[rel].clone(), start_of(&l, &st.files[rel]))).collect();
        let _ = ver;
        self.set.insert(l);

        for (cid, ev, i, sub, sub_text) in children {
            let p = Parent { agent: id.clone(), event: ev, relation: Relation::Subagent };
            self.import_doc(&sub, file, None, sub_text.as_deref(), cid, Some(p), Some(i), Vec::new())?;
        }
        for (rel, cid, ev) in file_parents {
            if self.done.contains(&rel) {
                continue;
            }
            let p = Parent { agent: id.clone(), event: ev, relation: Relation::Subagent };
            self.import_file(&rel, Some(cid), Some(p), Vec::new())?;
        }
        if let Some((rel, cid)) = continuation
            && !self.done.contains(&rel)
        {
            let link = Link { rel: "continued-from".into(), agent: Some(id.clone()), event: None, extra: Obj::new() };
            self.import_file(&rel, Some(cid), None, vec![link])?;
        }
        Ok(id)
    }

    /// The events one step makes. `step` is the record as stored; `rec` is the
    /// same step with any (unrestorable) carrier removed, which is what is mapped.
    fn map_step(&mut self, st: &mut Doc, step: &Value, rec: &Value, text: Option<&str>) -> Vec<Event> {
        let sid = match step.get("step_id") {
            Some(Value::String(x)) => x.clone(),
            Some(v) if !v.is_null() => v.to_string(),
            _ => "?".into(),
        };
        let nid = format!("step-{sid}");
        let ts = s(rec, "timestamp");
        let source = s(rec, "source").unwrap_or_default();
        let role = match source.as_str() {
            "user" => "user",
            "agent" => "assistant",
            _ => "system",
        };
        if source == "user" {
            st.turn = Some(format!("turn-{sid}"));
        }
        let mut out: Vec<Event> = Vec::new();
        fn put(out: &mut Vec<Event>, kind: Kind, body: Obj) {
            let mut e = Event::new(String::new(), kind);
            e.body = body;
            out.push(e);
        }
        let cm = rec.get("extra").and_then(|x| x.get("context_management"));
        let compaction = source == "system" && cm.and_then(|c| c.get("type")).and_then(Value::as_str) == Some("compaction");

        if source == "agent" {
            let model = s(rec, "model_name").or_else(|| st.default_model.clone());
            let effort = rec.get("reasoning_effort").filter(|v| !v.is_null()).cloned();
            let mut b = Obj::new();
            if model.is_some() && model != st.cur_model {
                b.insert("model".into(), json!(model.clone()));
                st.cur_model = model;
            }
            if effort.is_some() && effort != st.cur_effort {
                b.insert("effort".into(), effort.clone().unwrap());
                st.cur_effort = effort;
            }
            if !b.is_empty() {
                put(&mut out, Kind::Settings, b);
            }
            if let Some(r) = rec.get("reasoning_content").and_then(Value::as_str) {
                put(&mut out, Kind::Reasoning, obj(json!({"text": r})));
            }
        }
        if compaction {
            let c = cm.unwrap();
            let mut b = obj(json!({"phase": "boundary"}));
            for k in ["trigger", "tokens_before", "tokens_after"] {
                if let Some(v) = c.get(k).filter(|v| !v.is_null()) {
                    b.insert(k.into(), v.clone());
                }
            }
            put(&mut out, Kind::Compaction, b);
        } else if let Some(m) = rec.get("message") {
            for (kind, body) in self.content_events(m, role, &st.file) {
                put(&mut out, kind, body);
            }
        }
        for tc in rec.get("tool_calls").and_then(Value::as_array).into_iter().flatten() {
            let x = tc.get("extra");
            let mut b = Obj::new();
            if let Some(id) = tc.get("tool_call_id").filter(|v| !v.is_null()) {
                b.insert("call_id".into(), id.clone());
            }
            if let Some(n) = tc.get("function_name").filter(|v| !v.is_null()) {
                b.insert("name".into(), n.clone());
            }
            let args = tc.get("arguments").cloned().unwrap_or(Value::Null);
            let input_text = x.and_then(|x| x.get("input_text")).and_then(Value::as_bool) == Some(true);
            match (&args, input_text) {
                (Value::Object(m), true) if m.len() == 1 && m.get("input").is_some_and(Value::is_string) => {
                    b.insert("input_text".into(), m["input"].clone());
                }
                (Value::Object(_), _) => {
                    b.insert("input".into(), args.clone());
                }
                _ => {
                    self.losses.add("tool-arguments-not-object", Some(Kind::ToolCall), "ATIF arguments must be an object; kept as given");
                    if !args.is_null() {
                        b.insert("input".into(), args.clone());
                    }
                }
            }
            let tt = x.and_then(|x| x.get("tool_type")).and_then(Value::as_str).unwrap_or("function");
            b.insert("tool_type".into(), json!(tt));
            if let Some(sv) = x.and_then(|x| x.get("server")).filter(|v| !v.is_null()) {
                b.insert("server".into(), sv.clone());
            }
            put(&mut out, Kind::ToolCall, b);
        }
        // SPEC §3.1.1: a response's usage right after its own events, before
        // any tool result answering it.
        if let Some(m) = rec.get("metrics").filter(|m| m.is_object()) {
            let model = s(rec, "model_name").or_else(|| st.default_model.clone());
            if let Some(b) = usage_body(m, "", model.as_deref()) {
                put(&mut out, Kind::Usage, b);
            }
        }
        for r in results(rec) {
            let call_id = r.get("source_call_id").filter(|v| !v.is_null()).cloned();
            let refs: Vec<Value> = r.get("subagent_trajectory_ref").and_then(Value::as_array).cloned().unwrap_or_default();
            let mut ends = Vec::new();
            for sref in &refs {
                let child = self.ref_child(st, sref);
                let x = sref.get("extra");
                let mut b = obj(json!({"phase": "start", "agent": child}));
                if let Some(c) = &call_id {
                    b.insert("call_id".into(), c.clone());
                }
                if let Some(t) = x.and_then(|x| x.get("agent_type")).filter(|v| !v.is_null()) {
                    b.insert("agent_type".into(), t.clone());
                }
                if let Some(d) = x.and_then(|x| x.get("summary").or_else(|| x.get("description"))).filter(|v| v.is_string()) {
                    b.insert("description".into(), d.clone());
                }
                put(&mut out, Kind::Subagent, b);
                let mut e = obj(json!({"phase": "end", "agent": child}));
                if let Some(c) = &call_id {
                    e.insert("call_id".into(), c.clone());
                }
                if let Some(o) = x.and_then(|x| x.get("outcome")).filter(|v| !v.is_null()) {
                    e.insert("outcome".into(), o.clone());
                }
                ends.push(e);
            }
            for e in ends {
                put(&mut out, Kind::Subagent, e);
            }
            let Some(content) = r.get("content").filter(|v| !v.is_null()) else { continue };
            if compaction {
                let text = match content {
                    Value::String(t) => t.clone(),
                    Value::Array(parts) => parts.iter().filter_map(|p| p.get("text").and_then(Value::as_str)).collect::<Vec<_>>().join("\n"),
                    other => other.to_string(),
                };
                put(&mut out, Kind::Compaction, obj(json!({"phase": "summary", "summary": text})));
                continue;
            }
            let mut b = Obj::new();
            if let Some(c) = &call_id {
                b.insert("call_id".into(), c.clone());
            }
            b.insert("output".into(), self.output_value(content, &st.file));
            let x = r.get("extra");
            if let Some(v) = x.and_then(|x| x.get("is_error")).and_then(Value::as_bool) {
                b.insert("is_error".into(), json!(v));
            }
            if let Some(v) = x.and_then(|x| x.get("status")).filter(|v| !v.is_null()) {
                b.insert("status".into(), v.clone());
            }
            put(&mut out, Kind::ToolResult, b);
        }
        if out.is_empty() {
            match rec.get("message") {
                Some(Value::String(t)) if source != "agent" => put(&mut out, Kind::Text, obj(json!({"role": role, "text": t}))),
                _ => put(&mut out, Kind::Native, Obj::new()),
            }
        }
        let copied = rec.get("is_copied_context").and_then(Value::as_bool) == Some(true);
        let group = if source == "agent" { Some(nid.clone()) } else { None };
        for (i, e) in out.iter_mut().enumerate() {
            e.id = if i == 0 { nid.clone() } else { format!("{nid}#{i}") };
            e.ts = ts.clone();
            e.turn = st.turn.clone();
            // Only what the model emitted shares the step's message group,
            // and its usage (SPEC §3.1.1).
            if matches!(e.kind, Kind::Reasoning | Kind::Text | Kind::Media | Kind::ToolCall | Kind::Usage) {
                e.group = group.clone();
            }
            if copied {
                e.meta.insert("atif".into(), json!({"is_copied_context": true}));
            }
            let mut n = st.native.clone();
            n.id = Some(nid.clone());
            if i == 0 {
                jsonfmt::hold(&mut n, text, step);
            } else {
                n.part = i as u32;
            }
            e.native = Some(n);
        }
        out
    }

    fn ref_child(&self, st: &Doc, sref: &Value) -> String {
        if let Some(t) = s(sref, "trajectory_id")
            && let Some(c) = st.embedded.get(&t)
        {
            return c.clone();
        }
        if let Some(p) = s(sref, "trajectory_path")
            && let Some(c) = join_rel(&st.file, &p).and_then(|r| st.files.get(&r))
        {
            return c.clone();
        }
        s(sref, "trajectory_id").or_else(|| s(sref, "session_id")).or_else(|| s(sref, "trajectory_path").map(|p| stem(&p))).unwrap_or_else(|| "unknown".into())
    }

    /// Text and media events for a `message` (string or ContentPart array).
    fn content_events(&mut self, m: &Value, role: &str, file: &str) -> Vec<(Kind, Obj)> {
        let mut out = Vec::new();
        match m {
            Value::String(t) => {
                if !t.is_empty() {
                    out.push((Kind::Text, obj(json!({"role": role, "text": t}))));
                }
            }
            Value::Array(parts) => {
                for p in parts {
                    match p.get("type").and_then(Value::as_str) {
                        Some("text") => {
                            let t = p.get("text").and_then(Value::as_str).unwrap_or("");
                            out.push((Kind::Text, obj(json!({"role": role, "text": t}))));
                        }
                        Some("image") | Some("audio") => {
                            let mut b = obj(json!({"role": role}));
                            b.extend(self.media_source(p.get("source").unwrap_or(&Value::Null), file));
                            out.push((Kind::Media, b));
                        }
                        _ => {
                            self.losses.add("content-part-unknown", None, "a content part of unknown type kept only in native");
                        }
                    }
                }
            }
            _ => {}
        }
        out
    }

    /// `media_type` plus `data`+`name` (a file next to the trajectory), `url`, or `path`.
    fn media_source(&mut self, src: &Value, file: &str) -> Obj {
        let mut b = Obj::new();
        let mt = src.get("media_type").and_then(Value::as_str).unwrap_or("application/octet-stream");
        b.insert("media_type".into(), json!(mt));
        let path = src.get("path").and_then(Value::as_str).unwrap_or("");
        if let Some(rel) = join_rel(file, path).filter(|_| !path.is_empty())
            && let Ok(bytes) = std::fs::read(self.root.join(&rel))
        {
            b.insert("data".into(), json!(base64::engine::general_purpose::STANDARD.encode(bytes)));
            b.insert("name".into(), json!(path.rsplit('/').next().unwrap_or(path)));
            self.used_files.insert(rel);
            return b;
        }
        if path.contains("://") || path.starts_with("data:") {
            b.insert("url".into(), json!(path));
        } else {
            b.insert("path".into(), json!(path));
        }
        b
    }

    fn output_value(&mut self, content: &Value, file: &str) -> Value {
        match content {
            Value::Array(parts) => Value::Array(
                parts
                    .iter()
                    .map(|p| match p.get("type").and_then(Value::as_str) {
                        Some("image") | Some("audio") => {
                            let mut b = obj(json!({"type": "media"}));
                            b.extend(self.media_source(p.get("source").unwrap_or(&Value::Null), file));
                            Value::Object(b)
                        }
                        _ => json!({"type": "text", "text": p.get("text").and_then(Value::as_str).unwrap_or("")}),
                    })
                    .collect(),
            ),
            other => other.clone(),
        }
    }
}

fn obj(v: Value) -> Obj {
    match v {
        Value::Object(m) => m,
        _ => Obj::new(),
    }
}

/// Neutral usage from step `metrics` (prefix "") or `final_metrics` ("total_").
/// ATIF prompt tokens include cached ones; neutral `input` does not.
pub(super) fn usage_body(m: &Value, prefix: &str, model: Option<&str>) -> Option<Obj> {
    let k = |name: &str| m.get(format!("{prefix}{name}")).filter(|v| !v.is_null());
    let int = |v: Option<&Value>| v.and_then(Value::as_u64);
    let x = m.get("extra");
    let xi = |name: &str| x.and_then(|x| x.get(name)).and_then(Value::as_u64);
    let prompt = int(k("prompt_tokens"));
    let cached = int(k("cached_tokens"));
    let cache_write = xi("cache_write_tokens");
    let mut b = Obj::new();
    if let Some(m) = model {
        b.insert("model".into(), json!(m));
    }
    if let Some(p) = prompt {
        b.insert("input".into(), json!(p.saturating_sub(cached.unwrap_or(0)).saturating_sub(cache_write.unwrap_or(0))));
    }
    if let Some(v) = k("completion_tokens") {
        b.insert("output".into(), v.clone());
    }
    if let Some(c) = cached {
        b.insert("cache_read".into(), json!(c));
    }
    if let Some(c) = cache_write {
        b.insert("cache_write".into(), json!(c));
    }
    if let Some(r) = xi("reasoning_tokens") {
        b.insert("reasoning".into(), json!(r));
    }
    if let Some(c) = k("cost_usd") {
        b.insert("cost_usd".into(), c.clone());
    }
    let has = ["input", "output", "cache_read", "cost_usd"].iter().any(|f| b.contains_key(*f));
    if has { Some(b) } else { None }
}

/// Carried events come back in their original order within each run of
/// consecutive restored events.
fn sort_restored_runs(evs: &mut [Ev]) {
    let mut i = 0;
    while i < evs.len() {
        if evs[i].1.is_none() {
            i += 1;
            continue;
        }
        let mut j = i;
        while j < evs.len() && evs[j].1.is_some() {
            j += 1;
        }
        evs[i..j].sort_by_key(|(_, s)| s.unwrap());
        i = j;
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn import_str(text: &str) -> Imported {
        let dir = tempfile::tempdir().unwrap();
        let p = dir.path().join("t.json");
        std::fs::write(&p, text).unwrap();
        import(&p, &ImportOptions::standard()).unwrap()
    }

    #[test]
    fn steps_map_to_neutral_kinds() {
        let im = import_str(
            r#"{"schema_version":"ATIF-v1.5","session_id":"s1","agent":{"name":"a","version":"1","model_name":"m"},
            "steps":[{"step_id":1,"source":"user","message":"hi"},
            {"step_id":2,"source":"agent","message":"ok","reasoning_content":"think","tool_calls":[{"tool_call_id":"c1","function_name":"ls","arguments":{"p":"."}}],
             "observation":{"results":[{"source_call_id":"c1","content":"a.txt"},{"content":"env note"}]},
             "metrics":{"prompt_tokens":100,"completion_tokens":5,"cached_tokens":40,"cost_usd":0.01}}],
            "final_metrics":{"total_prompt_tokens":100,"total_completion_tokens":5}}"#,
        );
        let l = im.set.get("s1").unwrap();
        let kinds: Vec<&str> = l.events.iter().map(|e| e.kind.as_str()).collect();
        assert_eq!(
            kinds,
            // SPEC §3.1.1: the step's usage right after its own events, before the results.
            ["settings", "text", "reasoning", "text", "tool_call", "usage", "tool_result", "tool_result", "usage"]
        );
        assert_eq!(l.events[1].turn.as_deref(), Some("turn-1"));
        assert_eq!(l.events[2].id, "step-2");
        assert_eq!(l.events[3].id, "step-2#1");
        assert_eq!(l.events[5].body["input"], json!(60));
        assert_eq!(l.events[5].body["cache_read"], json!(40));
        assert!(l.events[7].body.get("call_id").is_none());
        assert_eq!(l.events[8].body["cumulative"], json!(true));
        assert_eq!(im.variant, "ATIF-v1.5");
    }

    #[test]
    fn embedded_subagents_become_children() {
        let im = import_str(
            r#"{"schema_version":"ATIF-v1.7","trajectory_id":"root","agent":{"name":"a","version":"1"},
            "steps":[{"step_id":1,"source":"agent","message":"","tool_calls":[{"tool_call_id":"c","function_name":"task","arguments":{}}],
              "observation":{"results":[{"source_call_id":"c","subagent_trajectory_ref":[{"trajectory_id":"kid"}],"content":"done"}]}}],
            "subagent_trajectories":[{"schema_version":"ATIF-v1.7","trajectory_id":"kid","agent":{"name":"b","version":"1"},
              "steps":[{"step_id":1,"source":"user","message":"go"}]}]}"#,
        );
        let kid = im.set.get("kid").unwrap();
        let p = kid.agent().parent.as_ref().unwrap();
        assert_eq!(p.agent, "root");
        assert_eq!(p.relation, Relation::Subagent);
        let root = im.set.get("root").unwrap();
        assert!(root.contains(p.event.as_deref().unwrap()));
        assert_eq!(kid.agent().source.as_ref().unwrap().extra["embedded"], json!(0));
    }
}
