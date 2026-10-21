//! Google Gemini CLI (`google-gemini/gemini-cli`) chat recordings and
//! `/chat save` checkpoints.
//!
//! Three native shapes, all under `~/.gemini/tmp/<project>/`:
//!
//! - **`session-jsonl`** (>= 0.39.0): `chats/session-<YYYY-MM-DDTHH-MM>-<id8>.jsonl`,
//!   an append-only log of compact JSON lines: a metadata line
//!   (`sessionId`, `projectHash`, ...), message records (a message is
//!   re-appended whole whenever it changes), `{"$set":{...}}` metadata updates
//!   (0.44-0.63 also checkpoint the whole history as `$set.messages`),
//!   `{"$rewindTo":id}` and (>= 0.64.0) `{"$patch":{...}}` deltas. Subagents
//!   are `chats/<parent session id>/<session id>.jsonl` with `kind: subagent`.
//! - **`session-json`** (0.4.0 - 0.38.x): the same `ConversationRecord`, as one
//!   whole-file `JSON.stringify(record, null, 2)` document rewritten on every
//!   change.
//! - **`checkpoint`**: `checkpoint-<tag>.json` from `/chat save`, the Gemini API
//!   history (`Content[]`): a bare array before 0.16.0, `{history, authType}`
//!   since.
//!
//! See `docs/formats/gemini_cli.md`.

use crate::carry;
use crate::formats::atif::jsonfmt;
use crate::formats::{Detection, ExportOptions, Exported, Format, ImportOptions, Imported, Losses, Output, Variant};
use crate::ledger::{Ledger, LedgerSet};
use crate::model::{Agent, Audience, Event, Kind, Native, Obj, Parent, Relation, Source};
use crate::util::OrderedMap;
use crate::util::{self, get_str, jsonl_lines};
use anyhow::{Context, Result, bail};
use serde_json::{Map, Value, json};
use std::collections::{BTreeMap, HashMap, HashSet};
use std::path::Path;

pub const NAME: &str = "gemini-cli";
/// gemini-cli's `INTERRUPTED_TOOL_RESPONSE_ERROR`.
const INTERRUPTED: &str = "Response was lost or interrupted.";
const V_JSONL: &str = "session-jsonl";
const V_JSON: &str = "session-json";
const V_CKPT: &str = "checkpoint";

pub struct GeminiCli;

static VARIANTS: [Variant; 3] = [
    Variant {
        name: V_JSONL,
        versions: ">= 0.39.0",
        import: true,
        export: true,
        notes: "chats/session-*.jsonl append log; $set.messages checkpoints (0.44-0.63) and $patch deltas (>= 0.64) are read; export writes plain message lines + $set",
    },
    Variant {
        name: V_JSON,
        versions: ">= 0.4.0, < 0.39.0",
        import: true,
        export: true,
        notes: "chats/session-*.json, one pretty-printed ConversationRecord rewritten on every change",
    },
    Variant {
        name: V_CKPT,
        versions: "all (bare Content[] < 0.16.0; {history, authType} >= 0.16.0)",
        import: true,
        export: true,
        notes: "checkpoint-<tag>.json written by /chat save; Gemini API Content history",
    },
];

impl Format for GeminiCli {
    fn name(&self) -> &'static str {
        NAME
    }
    fn describe(&self) -> &'static str {
        "Google Gemini CLI chat recordings (session JSONL / JSON) and /chat save checkpoints"
    }
    fn variants(&self) -> &'static [Variant] {
        &VARIANTS
    }
    fn detect(&self, input: &Path) -> Option<Detection> {
        let files = collect_files(input).ok()?;
        let mut best: Option<Detection> = None;
        for (_, bytes) in files {
            let text = String::from_utf8_lossy(&bytes);
            if let Some(d) = sniff(&text)
                && best.as_ref().is_none_or(|b| d.confidence > b.confidence)
            {
                best = Some(d);
            }
        }
        best
    }
    fn import(&self, input: &Path, opts: &ImportOptions) -> Result<Imported> {
        // SPEC §3.1.6: user or assistant text holding the peer-message wrapper is that peer message.
        let mut im = import_records(input, opts)?;
        crate::formats::pi_session::unwrap_peers(&mut im.set, self.name());
        Ok(im)
    }
    fn export(&self, set: &LedgerSet, opts: &ExportOptions) -> Result<Exported> {
        export(set, opts)
    }
}

/// What [`Format::import`] reads before peer messages are unwrapped.
fn import_records(input: &Path, opts: &ImportOptions) -> Result<Imported> {
    import(input, opts)
}

// ---------------------------------------------------------------------------
// detection

fn sniff(text: &str) -> Option<Detection> {
    let t = text.trim_start();
    if t.starts_with('[') {
        let v: Value = serde_json::from_str(text).ok()?;
        let a = v.as_array()?;
        if !a.is_empty() && a.iter().all(is_content) {
            return Some(Detection { variant: V_CKPT, confidence: 70, version: Some("< 0.16.0".into()) });
        }
        return None;
    }
    if !t.starts_with('{') {
        return None;
    }
    // Whole-file JSON?
    if let Ok(v) = serde_json::from_str::<Value>(text) {
        if v.get("sessionId").is_some_and(Value::is_string) && v.get("messages").is_some_and(Value::is_array) {
            return Some(Detection { variant: V_JSON, confidence: 90, version: Some(json_version_hint(&v)) });
        }
        if v.get("history").is_some_and(|h| h.as_array().is_some_and(|a| a.iter().all(is_content))) {
            return Some(Detection { variant: V_CKPT, confidence: 75, version: Some(">= 0.16.0".into()) });
        }
        if v.get("sessionId").is_some() && v.get("projectHash").is_some() {
            // A one-line JSONL log (metadata only).
            return Some(Detection { variant: V_JSONL, confidence: 80, version: Some(">= 0.39.0".into()) });
        }
        return None;
    }
    let first = t.lines().next()?;
    let v: Value = serde_json::from_str(first).ok()?;
    if v.get("sessionId").is_some_and(Value::is_string) && v.get("projectHash").is_some_and(Value::is_string) {
        return Some(Detection { variant: V_JSONL, confidence: 90, version: Some(jsonl_version_hint(text)) });
    }
    None
}

fn is_content(v: &Value) -> bool {
    v.get("role").is_some_and(Value::is_string) && v.get("parts").is_some_and(Value::is_array)
}

/// The earliest producer version that writes what this log contains. Gemini
/// CLI records no version, so this is inferred from shape.
fn jsonl_version_hint(text: &str) -> String {
    if text.contains("\"$patch\":") {
        ">= 0.64.0".into()
    } else if text.contains("{\"$set\":{\"messages\":") {
        ">= 0.44.0, < 0.64.0".into()
    } else if text.contains("\"memoryScratchpad\":") {
        ">= 0.41.0".into()
    } else {
        ">= 0.39.0".into()
    }
}

fn json_version_hint(v: &Value) -> String {
    let msgs = v.get("messages").and_then(Value::as_array).cloned().unwrap_or_default();
    let has = |k: &str| msgs.iter().any(|m| m.get(k).is_some());
    let types: HashSet<&str> = msgs.iter().filter_map(|m| get_str(m, &["type"])).collect();
    if v.get("directories").is_some() {
        ">= 0.28.0, < 0.39.0".into()
    } else if v.get("summary").is_some() {
        ">= 0.21.0, < 0.39.0".into()
    } else if types.contains("error") || types.contains("warning") {
        ">= 0.18.0, < 0.39.0".into()
    } else if has("displayContent") || v.get("kind").is_some() {
        ">= 0.25.0, < 0.39.0".into()
    } else {
        ">= 0.4.0, < 0.39.0".into()
    }
}

// ---------------------------------------------------------------------------
// input files

/// `(relative path, bytes)` for every candidate file of `input`.
fn collect_files(input: &Path) -> Result<Vec<(String, Vec<u8>)>> {
    let mut out = Vec::new();
    if input.is_file() {
        let name = input.file_name().unwrap().to_string_lossy().to_string();
        out.push((name, std::fs::read(input)?));
        return Ok(out);
    }
    for e in walkdir::WalkDir::new(input).sort_by_file_name() {
        let e = e?;
        if !e.file_type().is_file() {
            continue;
        }
        let rel = e.path().strip_prefix(input)?.to_string_lossy().replace('\\', "/");
        let base = rel.rsplit('/').next().unwrap_or(&rel);
        if base.starts_with('.') || rel == "SOURCE.md" || rel == "expect.json" || base == "logs.json" {
            continue;
        }
        if !(base.ends_with(".json") || base.ends_with(".jsonl")) {
            continue;
        }
        out.push((rel, std::fs::read(e.path())?));
    }
    Ok(out)
}

// ---------------------------------------------------------------------------
// import

fn import(input: &Path, opts: &ImportOptions) -> Result<Imported> {
    let files = collect_files(input)?;
    let multi = input.is_dir();
    let mut set = LedgerSet::new();
    let mut losses = Losses::default();
    let mut variant = String::new();
    let mut parents: Vec<(String, String)> = Vec::new(); // (child agent, parent dir name)
    let mut by_session: HashMap<String, String> = HashMap::new(); // session id -> agent id
    for (rel, bytes) in files {
        let text = String::from_utf8(bytes).with_context(|| format!("{rel}: not UTF-8"))?;
        let forced = opts.variant.as_deref();
        let v = match forced {
            Some(v) => v.to_string(),
            None => match sniff(&text) {
                Some(d) => d.variant.to_string(),
                None if multi => continue,
                None => bail!("{rel}: not a Gemini CLI session or checkpoint"),
            },
        };
        let file = if multi { Some(rel.clone()) } else { None };
        let ledger = match v.as_str() {
            V_JSONL => import_jsonl(&rel, file, &text, opts, &mut losses)?,
            V_JSON => import_json(&rel, file, &text, opts, &mut losses)?,
            V_CKPT => import_checkpoint(&rel, file, &text, &mut losses)?,
            other => bail!("gemini-cli: unknown variant {other}"),
        };
        if variant.is_empty() || variant == V_CKPT {
            variant = v.clone();
        }
        // A subagent lives in `<parent session id>/<id>.jsonl`.
        if let Some((dir, _)) = rel.rsplit_once('/') {
            let is_sub = ledger.agent().source.as_ref().is_some_and(|s| s.extra.get("kind").and_then(Value::as_str) == Some("subagent"));
            if is_sub && ledger.agent().parent.is_none() {
                parents.push((ledger.id().to_string(), dir.rsplit('/').next().unwrap().to_string()));
            }
        }
        if let Some(n) = ledger.agent().source.as_ref().and_then(|s| s.native_id.clone()) {
            by_session.insert(n, ledger.id().to_string());
        }
        set.insert(ledger);
    }
    if set.is_empty() {
        bail!("{}: no Gemini CLI sessions found", input.display());
    }
    for (child, parent_session) in parents {
        let Some(parent) = by_session.get(&parent_session).cloned() else { continue };
        let child_session = set.get(&child).and_then(|l| l.agent().source.as_ref()?.native_id.clone()).unwrap_or(child.clone());
        let ev = set
            .get(&parent)
            .unwrap()
            .events
            .iter()
            .find(|e| e.kind == Kind::Subagent && (e.str("agent") == Some(child_session.as_str()) || e.str("agent") == Some(child.as_str())));
        let event = ev.map(|e| e.id.clone());
        let l = set.ledgers.get_mut(&child).unwrap();
        l.agent_mut().parent = Some(Parent { agent: parent, event, relation: Relation::Subagent });
    }
    Ok(Imported { set, losses: losses.sorted(), variant })
}

/// Turns the native records of one file into events, de-duplicating the
/// neutral facts: Gemini CLI re-writes a message whole whenever it changes,
/// so the same fact appears in many records. Each fact has a key; a record
/// only produces events for keys not seen before.
struct Builder {
    variant: &'static str,
    ledger: Ledger,
    seen: HashSet<String>,
    /// call id -> (name, status, timestamp), from a pre-scan.
    calls: HashMap<String, (String, String, Option<String>)>,
    last_model: Option<String>,
    turn: Option<String>,
    restore: bool,
}

/// One neutral fact a record expresses.
struct Cand {
    key: String,
    ev: Event,
}

impl Builder {
    fn new(variant: &'static str, agent: Agent, restore: bool) -> Self {
        Builder { variant, ledger: Ledger::new(agent), seen: HashSet::new(), calls: HashMap::new(), last_model: None, turn: None, restore }
    }

    fn native(&self, id: &str) -> Native {
        Native::new(NAME, Some(id.to_string())).variant(Some(self.variant))
    }

    /// Append the new facts of one native record; a record with none still
    /// gets a `native` event, so the exporter can reproduce it.
    fn push(&mut self, nid: &str, mut native: Native, cands: Vec<Cand>, ts: Option<String>, file: &Option<String>) {
        native.file = file.clone();
        let mut fresh: Vec<Event> = Vec::new();
        for c in cands {
            if self.seen.insert(c.key) {
                fresh.push(c.ev);
            }
        }
        if fresh.is_empty() {
            fresh.push(Event::new("", Kind::Native).ts(ts));
        }
        for (i, mut e) in fresh.into_iter().enumerate() {
            e.id = if i == 0 { nid.to_string() } else { format!("{nid}#{i}") };
            e.native = Some(if i == 0 { native.clone() } else { native.part_of(i as u32) });
            self.ledger.append_unique(e);
        }
    }

    /// Restore carried events if `rec` has an unedited carrier. Returns true
    /// if it did (the record is then fully represented by those events).
    fn restore(&mut self, rec: &Value) -> bool {
        if !self.restore || rec.get(carry::KEY).is_none() {
            return false;
        }
        let mut r = rec.clone();
        match carry::take(&mut r, &[]) {
            Some(events) => {
                for e in events {
                    self.ledger.append_unique(e);
                }
                // Facts from the record must not be re-emitted later.
                let mut tmp = Vec::new();
                self.message_cands(&r, &mut tmp);
                for c in tmp {
                    self.seen.insert(c.key);
                }
                true
            }
            None => false,
        }
    }

    fn prescan(&mut self, m: &Value) {
        if get_str(m, &["type"]) != Some("gemini") {
            return;
        }
        for tc in m.get("toolCalls").and_then(Value::as_array).into_iter().flatten() {
            if let Some(id) = get_str(tc, &["id"]) {
                self.calls.insert(
                    id.to_string(),
                    (
                        get_str(tc, &["name"]).unwrap_or("").to_string(),
                        get_str(tc, &["status"]).unwrap_or("").to_string(),
                        get_str(tc, &["timestamp"]).map(String::from),
                    ),
                );
            }
        }
    }

    /// The neutral facts of one message record.
    fn message_cands(&mut self, m: &Value, out: &mut Vec<Cand>) {
        let Some(mid) = get_str(m, &["id"]) else { return };
        let mid = mid.to_string();
        let ts = get_str(m, &["timestamp"]).map(String::from);
        let ty = get_str(m, &["type"]).unwrap_or("");
        let parts = part_list(m.get("content"));
        match ty {
            "user" => {
                let real = parts
                    .iter()
                    .any(|p| p.get("functionResponse").is_none() && p.get("text").is_some_and(|t| t.as_str().is_some_and(|s| !s.trim().is_empty())));
                if real {
                    self.turn = Some(mid.clone());
                }
                for (i, p) in parts.iter().enumerate() {
                    self.user_part(&mid, i, p, &ts, out);
                }
            }
            "gemini" => {
                let model = get_str(m, &["model"]).map(String::from);
                if let Some(model) = &model
                    && self.last_model.as_deref() != Some(model)
                {
                    let e = self.ev(Kind::Settings, &ts, None).with("model", model.as_str());
                    out.push(Cand { key: format!("{mid}/model:{model}"), ev: e });
                    self.last_model = Some(model.clone());
                }
                let thought_in_content = parts.iter().any(|p| p.get("thought").and_then(Value::as_bool) == Some(true));
                if !thought_in_content {
                    for th in m.get("thoughts").and_then(Value::as_array).into_iter().flatten() {
                        let subject = get_str(th, &["subject"]).unwrap_or("");
                        let desc = get_str(th, &["description"]).unwrap_or("");
                        let text = thought_text(subject, desc);
                        let tts = get_str(th, &["timestamp"]).map(String::from).or(ts.clone());
                        // SPEC §3.1.5: the thought as Gemini CLI shows it (its
                        // bold subject heads the description); the subject is
                        // not a summary of it.
                        let e = self.ev(Kind::Reasoning, &tts, Some(&mid)).with("text", text.as_str());
                        out.push(Cand { key: format!("{mid}/th:{}", norm_key(&text)), ev: e });
                    }
                }
                for p in &parts {
                    if p.get("thought").and_then(Value::as_bool) == Some(true) {
                        let text = p.get("text").and_then(Value::as_str).unwrap_or("");
                        let mut e = self.ev(Kind::Reasoning, &ts, Some(&mid)).with("text", text);
                        if let Some(sig) = p.get("thoughtSignature").and_then(Value::as_str) {
                            e = e.with("sealed", json!({"vendor": "google", "kind": "signature", "data": sig}));
                        }
                        out.push(Cand { key: format!("{mid}/th:{}", norm_key(text)), ev: e });
                    } else if let Some(text) = p.get("text").and_then(Value::as_str) {
                        if text.is_empty() {
                            continue;
                        }
                        let e = self.ev(Kind::Text, &ts, Some(&mid)).with("role", "assistant").with("text", text);
                        out.push(Cand { key: format!("{mid}/t:{}", util::short_id(text, 16)), ev: e });
                    } else if let Some(fc) = p.get("functionCall") {
                        let name = get_str(fc, &["name"]).unwrap_or("");
                        let id = get_str(fc, &["id"]).map(String::from).unwrap_or_else(|| format!("{mid}:{name}"));
                        let e = self
                            .ev(Kind::ToolCall, &ts, Some(&mid))
                            .with("call_id", id.as_str())
                            .with("name", name)
                            .with("input", fc.get("args").cloned().unwrap_or(json!({})));
                        out.push(Cand { key: format!("c:{id}"), ev: e });
                    } else if let Some(media) = media_of(p) {
                        let mut e = self.ev(Kind::Media, &ts, Some(&mid)).with("role", "assistant");
                        e.body.extend(media);
                        out.push(Cand { key: format!("{mid}/m:{}", util::short_id(&p.to_string(), 16)), ev: e });
                    }
                }
                let tcs: Vec<Value> = m.get("toolCalls").and_then(Value::as_array).cloned().unwrap_or_default();
                for tc in &tcs {
                    self.tool_call_cands(&mid, &ts, tc, out, true);
                }
                if let Some(tok) = m.get("tokens").filter(|t| t.is_object()) {
                    let mut e = self.ev(Kind::Usage, &ts, Some(&mid));
                    if let Some(model) = get_str(m, &["model"]) {
                        e = e.with("model", model);
                    }
                    for (from, to) in [("input", "input"), ("output", "output"), ("cached", "cache_read"), ("thoughts", "reasoning")] {
                        if let Some(n) = tok.get(from).filter(|n| n.is_number()) {
                            e = e.with(to, n.clone());
                        }
                    }
                    // SPEC §3.1.1: Gemini's `input` (promptTokenCount) includes
                    // the cached tokens and its `output` (candidatesTokenCount)
                    // excludes the thoughts; neutral `input` excludes cache
                    // reads and `output` includes reasoning.
                    let num = |k: &str| tok.get(k).and_then(Value::as_f64);
                    if let (Some(i), Some(c)) = (num("input"), num("cached")) {
                        e = e.with("input", count((i - c).max(0.0)));
                    }
                    if let (Some(o), Some(t)) = (num("output"), num("thoughts")) {
                        e = e.with("output", count(o + t));
                    }
                    out.push(Cand { key: format!("{mid}/usage"), ev: e });
                }
                // Results follow the calls and the usage, as tool execution does.
                for tc in &tcs {
                    self.tool_call_cands(&mid, &ts, tc, out, false);
                }
            }
            "info" => {
                let text = parts_text(&parts);
                // UI notices: outside any turn, never shown to the model.
                let e = Event::new("", Kind::Text).ts(ts.clone()).with("role", "system").with("text", text.as_str()).audience(Audience::Record);
                out.push(Cand { key: format!("{mid}/i:{}", util::short_id(&text, 16)), ev: e });
                for (i, p) in parts.iter().enumerate() {
                    if let Some(media) = media_of(p) {
                        let mut e = self.ev(Kind::Media, &ts, None).with("role", "user");
                        e.body.extend(media);
                        out.push(Cand { key: format!("{mid}/m{i}"), ev: e });
                    }
                }
            }
            "error" | "warning" => {
                let text = parts_text(&parts);
                let e = Event::new("", Kind::Error).ts(ts.clone()).with("kind", ty).with("message", text.as_str());
                out.push(Cand { key: format!("{mid}/e:{}", util::short_id(&text, 16)), ev: e });
            }
            _ => {}
        }
    }

    fn user_part(&mut self, mid: &str, i: usize, p: &Value, ts: &Option<String>, out: &mut Vec<Cand>) {
        if let Some(fr) = p.get("functionResponse") {
            let id = get_str(fr, &["id"]).map(String::from).unwrap_or_else(|| format!("{mid}:{}", get_str(fr, &["name"]).unwrap_or("")));
            let (status, cts) = self.calls.get(&id).map(|c| (c.1.clone(), c.2.clone())).unwrap_or_default();
            let e = result_event(self.ev(Kind::ToolResult, &cts.or(ts.clone()), Some(mid)), &id, fr.get("response"), &status);
            out.push(Cand { key: format!("r:{id}"), ev: e });
        } else if let Some(text) = p.get("text").and_then(Value::as_str) {
            if text.is_empty() {
                return;
            }
            let e = self.ev(Kind::Text, ts, Some(mid)).with("role", "user").with("text", text);
            out.push(Cand { key: format!("{mid}/t:{}", util::short_id(text, 16)), ev: e });
        } else if let Some(media) = media_of(p) {
            let mut e = self.ev(Kind::Media, ts, Some(mid)).with("role", "user");
            e.body.extend(media);
            out.push(Cand { key: format!("{mid}/m{i}:{}", util::short_id(&p.to_string(), 12)), ev: e });
        }
    }

    /// `calls`: the call (and subagent start) facts; otherwise the result,
    /// file change and subagent end facts.
    fn tool_call_cands(&mut self, mid: &str, ts: &Option<String>, tc: &Value, out: &mut Vec<Cand>, calls: bool) {
        let Some(id) = get_str(tc, &["id"]) else { return };
        let name = get_str(tc, &["name"]).unwrap_or("");
        let status = get_str(tc, &["status"]).unwrap_or("");
        let cts = get_str(tc, &["timestamp"]).map(String::from).or(ts.clone());
        if !calls {
            return self.tool_outcome_cands(mid, id, status, &cts, tc, out);
        }
        let e = self.ev(Kind::ToolCall, ts, Some(mid)).with("call_id", id).with("name", name).with("input", tc.get("args").cloned().unwrap_or(json!({})));
        out.push(Cand { key: format!("c:{id}"), ev: e });
        if let Some(agent) = get_str(tc, &["agentId"]) {
            let mut e = self.ev(Kind::Subagent, ts, Some(mid)).with("phase", "start").with("agent", agent).with("call_id", id).with("agent_type", name);
            if let Some(d) = get_str(tc, &["args", "objective"]).or(get_str(tc, &["description"])) {
                e = e.with("description", d);
            }
            out.push(Cand { key: format!("s:{id}"), ev: e });
        }
    }

    fn tool_outcome_cands(&mut self, mid: &str, id: &str, status: &str, cts: &Option<String>, tc: &Value, out: &mut Vec<Cand>) {
        let result = tc.get("result").filter(|r| !r.is_null());
        let has_result = result.is_some_and(|r| !part_list(Some(r)).is_empty());
        if has_result || matches!(status, "success" | "error" | "cancelled") {
            let resp = result.and_then(|r| part_list(Some(r)).into_iter().find_map(|p| p.get("functionResponse").and_then(|f| f.get("response")).cloned()));
            let e = if let Some(resp) = resp {
                result_event(self.ev(Kind::ToolResult, cts, None), id, Some(&resp), status)
            } else {
                // A bare string / text-part result (older versions).
                let text = result.map(|r| parts_text(&part_list(Some(r)))).unwrap_or_default();
                let mut e = self.ev(Kind::ToolResult, cts, None).with("call_id", id).with("output", text.as_str());
                if matches!(status, "error" | "cancelled") {
                    e = e.with("is_error", true);
                }
                if status == "cancelled" {
                    e = e.with("status", "cancelled");
                }
                e
            };
            out.push(Cand { key: format!("r:{id}"), ev: e });
        }
        if let Some(rd) = tc.get("resultDisplay") {
            if rd.get("fileDiff").is_some() && rd.get("filePath").is_some() {
                let op = if rd.get("isNewFile").and_then(Value::as_bool) == Some(true) || rd.get("originalContent").is_some_and(Value::is_null) {
                    "create"
                } else {
                    "edit"
                };
                let mut e = self.ev(Kind::FileChange, cts, Some(mid)).with("path", rd["filePath"].clone()).with("op", op).with("diff", rd["fileDiff"].clone());
                if let Some(c) = rd.get("newContent").filter(|c| c.is_string()) {
                    e = e.with("content", c.clone());
                }
                out.push(Cand { key: format!("f:{id}"), ev: e });
            }
            if rd.get("isSubagentProgress").and_then(Value::as_bool) == Some(true)
                && let Some(agent) = get_str(tc, &["agentId"])
            {
                let mut e = self.ev(Kind::Subagent, cts, Some(mid)).with("phase", "end").with("agent", agent).with("call_id", id);
                if let Some(s) = get_str(rd, &["state"]) {
                    let outcome = match s {
                        "completed" => "ok",
                        "error" | "failed" => "error",
                        "cancelled" | "aborted" => "aborted",
                        other => other,
                    };
                    e = e.with("outcome", outcome);
                }
                if let Some(r) = rd.get("result").filter(|r| r.is_string()) {
                    e = e.with("result", r.clone());
                }
                out.push(Cand { key: format!("se:{id}"), ev: e });
            }
        }
    }

    fn ev(&self, kind: Kind, ts: &Option<String>, group: Option<&str>) -> Event {
        Event::new("", kind).ts(ts.clone()).group(group).turn(self.turn.clone())
    }
}

fn result_event(e: Event, id: &str, resp: Option<&Value>, status: &str) -> Event {
    let mut e = e.with("call_id", id);
    let mut is_error = matches!(status, "error" | "cancelled");
    let output = match resp {
        Some(Value::Object(r)) if r.len() == 1 && r.get("output").is_some_and(Value::is_string) => r["output"].clone(),
        Some(Value::Object(r)) if r.len() == 1 && r.get("error").is_some_and(Value::is_string) => {
            is_error = true;
            r["error"].clone()
        }
        Some(Value::Object(r)) if r.is_empty() => json!(""),
        Some(v) => Value::String(v.to_string()),
        None => json!(""),
    };
    e = e.with("output", output);
    if is_error {
        e = e.with("is_error", true);
    }
    if status == "cancelled" {
        e = e.with("status", "cancelled");
    }
    e
}

/// `PartListUnion` as a list of part objects.
fn part_list(v: Option<&Value>) -> Vec<Value> {
    match v {
        None | Some(Value::Null) => vec![],
        Some(Value::String(s)) => {
            if s.is_empty() {
                vec![]
            } else {
                vec![json!({"text": s})]
            }
        }
        Some(Value::Array(a)) => a.iter().map(|p| if let Value::String(s) = p { json!({"text": s}) } else { p.clone() }).collect(),
        Some(o) => vec![o.clone()],
    }
}

fn parts_text(parts: &[Value]) -> String {
    parts.iter().filter_map(|p| p.get("text").and_then(Value::as_str)).collect::<Vec<_>>().join("")
}

fn media_of(p: &Value) -> Option<Obj> {
    let mut b = Obj::new();
    if let Some(d) = p.get("inlineData") {
        b.insert("media_type".into(), d.get("mimeType").cloned().unwrap_or(json!("application/octet-stream")));
        b.insert("data".into(), d.get("data").cloned().unwrap_or(json!("")));
        if let Some(n) = d.get("displayName") {
            b.insert("name".into(), n.clone());
        }
        return Some(b);
    }
    if let Some(d) = p.get("fileData") {
        b.insert("media_type".into(), d.get("mimeType").cloned().unwrap_or(json!("application/octet-stream")));
        b.insert("url".into(), d.get("fileUri").cloned().unwrap_or(json!("")));
        if let Some(n) = d.get("displayName") {
            b.insert("name".into(), n.clone());
        }
        return Some(b);
    }
    None
}

fn thought_text(subject: &str, desc: &str) -> String {
    if subject.is_empty() { desc.to_string() } else { format!("**{subject}** {desc}") }
}

fn norm_key(s: &str) -> String {
    let n: String = s.split_whitespace().collect::<Vec<_>>().join(" ");
    util::short_id(&n, 16)
}

fn agent_for(rel: &str, session_id: Option<&str>, variant: &str, meta: &Value) -> Agent {
    let stem = rel.rsplit('/').next().unwrap_or(rel);
    let stem = stem.trim_end_matches(".jsonl").trim_end_matches(".json");
    let id = session_id.map(String::from).unwrap_or_else(|| stem.to_string());
    let mut extra = Obj::new();
    extra.insert("file".into(), json!(rel));
    if let Some(k) = meta.get("kind") {
        extra.insert("kind".into(), k.clone());
    }
    if let Some(h) = meta.get("projectHash") {
        extra.insert("projectHash".into(), h.clone());
    }
    Agent {
        id,
        created_at: get_str(meta, &["startTime"]).map(String::from),
        source: Some(Source { format: NAME.into(), variant: Some(variant.into()), version: None, native_id: session_id.map(String::from), extra }),
        parent: None,
        links: vec![],
        meta: Obj::new(),
    }
}

fn import_jsonl(rel: &str, file: Option<String>, text: &str, opts: &ImportOptions, losses: &mut Losses) -> Result<Ledger> {
    let lines: Vec<(usize, &str)> = jsonl_lines(text).collect();
    let parsed: Vec<Option<Value>> = lines.iter().map(|(_, l)| serde_json::from_str(l).ok()).collect();
    let meta = parsed
        .iter()
        .flatten()
        .find(|v| v.get("sessionId").is_some() && v.get("projectHash").is_some() && v.get("$set").is_none())
        .cloned()
        .unwrap_or(json!({}));
    let mut agent = agent_for(rel, get_str(&meta, &["sessionId"]), V_JSONL, &meta);
    if let Some(src) = agent.source.as_mut() {
        if !text.ends_with('\n') {
            src.extra.insert("no_final_newline".into(), json!(true));
        }
        src.extra.insert("version_hint".into(), json!(jsonl_version_hint(text)));
    }
    let mut b = Builder::new(V_JSONL, agent, opts.restore_carried);
    // Pre-scan tool call status, wherever a message appears.
    for v in parsed.iter().flatten() {
        b.prescan(v);
        if let Some(ms) = v.get("$set").and_then(|s| s.get("messages")).and_then(Value::as_array) {
            for m in ms {
                b.prescan(m);
            }
        }
    }
    let mut state: Map<String, Value> = Map::new();
    let mut occurrences: HashMap<String, usize> = HashMap::new();
    let mut rewinds = 0u64;
    let mut parsed = parsed;
    // The metadata line opens every log; it belongs to the header.
    let mut skip_first = false;
    if let (Some((0, line)), Some(Some(m))) = (lines.first(), parsed.first())
        && m.get("sessionId").is_some()
        && m.get("projectHash").is_some()
        && m.get("$set").is_none()
    {
        let m = m.clone();
        // The metadata line is kept as its own text (SPEC §4).
        let src = b.ledger.agent_mut().source.as_mut().unwrap();
        src.extra.insert("meta_raw".into(), json!(line));
        skip_first = true;
        if opts.restore_carried
            && let Some(a) = carried_agent(&m)
        {
            apply_agent(b.ledger.agent_mut(), &a);
        }
        if !b.restore(&m)
            && let Some(s) = m.get("summary").and_then(Value::as_str)
        {
            b.seen.insert(format!("title:{}", util::short_id(s, 16)));
            let mut n = b.native("@0");
            n.file = file.clone();
            let e = Event::new("@0", Kind::Title).with("text", s).ts(get_str(&m, &["lastUpdated"])).native(n);
            b.ledger.append_unique(e);
        }
        parsed[0] = None;
    }
    for (i, ((off, line), v)) in lines.iter().zip(parsed).enumerate() {
        if i == 0 && skip_first {
            continue;
        }
        let anchor = format!("@{off}");
        let Some(v) = v else {
            losses.add("unparseable-line-kept", Some(Kind::Native), "a line that is not JSON is kept verbatim as a native event");
            let n = b.native(&anchor).raw(*line);
            b.push(&anchor, n, vec![], None, &file);
            continue;
        };
        // Each line is held as its own text (SPEC §4).
        let native_for = |id: &str, b: &Builder| {
            let mut n = b.native(id);
            jsonfmt::hold(&mut n, Some(line), &v);
            n
        };
        let mut cands = Vec::new();
        let mut ts = None;
        let nid;
        if v.get("id").is_none() && b.restore(&v) {
            continue;
        }
        if let Some(to) = v.get("$rewindTo").and_then(Value::as_str) {
            rewinds += 1;
            let mut cut = false;
            let keys: Vec<String> = state.keys().cloned().collect();
            for k in keys {
                if k == to {
                    cut = true;
                }
                if cut {
                    state.remove_ordered(&k);
                }
            }
            if !cut {
                state.clear();
            }
            nid = anchor;
        } else if let Some(p) = v.get("$patch").filter(|p| p.is_object()) {
            let mut touched: Vec<String> = Vec::new();
            let mut apply = |patch: &Value, state: &mut Map<String, Value>| {
                let Some(id) = get_str(patch, &["id"]) else { return };
                let Some(m) = state.get_mut(id) else { return };
                if let Some(c) = patch.get("content") {
                    m["content"] = c.clone();
                }
                if let (Some(tcs), Some(cur)) = (patch.get("toolCalls").and_then(Value::as_array), m.get_mut("toolCalls").and_then(Value::as_array_mut)) {
                    for tp in tcs {
                        if let Some(tc) = cur.iter_mut().find(|t| t.get("id") == tp.get("id"))
                            && let Some(r) = tp.get("result")
                        {
                            tc["result"] = r.clone();
                        }
                    }
                }
                touched.push(id.to_string());
            };
            if p.get("id").is_some() {
                apply(p, &mut state);
            }
            for u in p.get("updates").and_then(Value::as_array).into_iter().flatten() {
                apply(u, &mut state);
            }
            for r in p.get("removeIds").and_then(Value::as_array).into_iter().flatten() {
                if let Some(r) = r.as_str() {
                    state.remove_ordered(r);
                }
            }
            if let Some(order) = p.get("orderIds").and_then(Value::as_array) {
                let ordered: Vec<String> = order.iter().filter_map(|x| x.as_str()).filter(|x| state.contains_key(*x)).map(String::from).collect();
                let oset: HashSet<&String> = ordered.iter().collect();
                let mut next = Map::new();
                for (k, m) in state.iter() {
                    if !oset.contains(k) {
                        next.insert(k.clone(), m.clone());
                    }
                }
                for k in &ordered {
                    next.insert(k.clone(), state[k].clone());
                }
                state = next;
            }
            for id in touched {
                if let Some(m) = state.get(&id).cloned() {
                    b.message_cands(&m, &mut cands);
                }
            }
            nid = anchor;
        } else if let Some(set) = v.get("$set").filter(|s| s.is_object()) {
            if let Some(ms) = set.get("messages").and_then(Value::as_array) {
                state.clear();
                for m in ms {
                    if let Some(id) = get_str(m, &["id"]) {
                        state.insert(id.to_string(), m.clone());
                        b.message_cands(m, &mut cands);
                    }
                }
            }
            if let Some(s) = set.get("summary").and_then(Value::as_str) {
                cands.push(Cand { key: format!("title:{}", util::short_id(s, 16)), ev: Event::new("", Kind::Title).with("text", s) });
            }
            ts = get_str(set, &["lastUpdated"]).map(String::from);
            nid = anchor;
        } else if v.get("sessionId").is_some() && v.get("projectHash").is_some() {
            if let Some(s) = v.get("summary").and_then(Value::as_str) {
                cands.push(Cand { key: format!("title:{}", util::short_id(s, 16)), ev: Event::new("", Kind::Title).with("text", s) });
            }
            ts = get_str(&v, &["startTime"]).map(String::from);
            nid = anchor;
        } else if let Some(id) = get_str(&v, &["id"]) {
            let k = occurrences.entry(id.to_string()).or_default();
            *k += 1;
            nid = if *k == 1 { id.to_string() } else { format!("{id}{anchor}") };
            ts = get_str(&v, &["timestamp"]).map(String::from);
            if b.restore(&v) {
                if let Some(m) = carry_stripped(&v) {
                    state.insert(id.to_string(), m);
                }
                continue;
            }
            state.insert(id.to_string(), v.clone());
            b.message_cands(&v, &mut cands);
        } else {
            nid = anchor;
        }
        let n = native_for(&nid, &b);
        b.push(&nid, n, cands, ts, &file);
    }
    if rewinds > 0 {
        losses.add_n("rewind-not-applied", Some(Kind::Native), rewinds, "$rewindTo records are kept as native events; the rewound messages stay in the ledger");
    }
    Ok(b.ledger)
}

fn carry_stripped(v: &Value) -> Option<Value> {
    let mut r = v.clone();
    carry::strip(&mut r);
    Some(r)
}

/// The JS `JSON.stringify(v, null, 2)` form.
pub(crate) fn pretty(v: &Value) -> String {
    let mut buf = Vec::new();
    let fmt = serde_json::ser::PrettyFormatter::with_indent(b"  ");
    let mut ser = serde_json::Serializer::with_formatter(&mut buf, fmt);
    serde::Serialize::serialize(v, &mut ser).expect("json");
    String::from_utf8(buf).expect("utf8")
}

/// The whole-file document minus its list (`messages` / `history`), for an
/// exact re-render, or the raw text when re-rendering would not be exact;
/// and the compact text of each item of the list, as the source spelled it.
///
/// The layout is matched on the file's own tokens (`util::pretty`), so
/// key order and number spelling never decide it; the skeleton is kept both
/// parsed (`document`, for rendering a changed file) and as its own compact
/// text (`document_text`, for laying an unchanged one out again).
fn skeleton(doc: &Value, list_key: Option<&str>, text: &str) -> (Obj, Vec<Option<String>>) {
    let mut extra = Obj::new();
    let compact = util::minify(text);
    let rendered = util::pretty(&compact);
    if text == rendered {
        extra.insert("style".into(), json!("pretty2"));
    } else if text.strip_suffix('\n') == Some(rendered.as_str()) {
        extra.insert("style".into(), json!("pretty2+newline"));
    } else {
        extra.insert("raw_file".into(), json!(text));
    }
    let mut sk = doc.clone();
    let list = match list_key {
        Some(k) => {
            sk[k] = json!([]);
            if let Some(t) = jsonfmt::with_member(&compact, k, "[]") {
                extra.insert("document_text".into(), json!(t));
            }
            jsonfmt::member(&compact, k)
        }
        None => {
            sk = json!([]);
            extra.insert("document_text".into(), json!("[]"));
            Some(compact.as_str())
        }
    };
    extra.insert("document".into(), sk);
    let n = list_key.map_or(doc.as_array(), |k| doc.get(k).and_then(Value::as_array)).map_or(0, Vec::len);
    let texts = match list.and_then(jsonfmt::items).filter(|t| t.len() == n) {
        Some(t) => t.into_iter().map(|x| Some(x.to_string())).collect(),
        None => vec![None; n],
    };
    (extra, texts)
}

/// An unchanged whole-file document laid out again from its own texts: the
/// skeleton's text (`document_text`) with its list filled from the records'
/// texts, as `JSON.stringify(_, null, 2)` lays it out. `None` when a piece is
/// missing or the text would not say `doc`.
fn own_text(extra: &Obj, list_key: Option<&str>, texts: &[Option<String>], doc: &Value) -> Option<String> {
    let style = extra.get("style").and_then(Value::as_str)?;
    let sk = extra.get("document_text").and_then(Value::as_str)?;
    let texts: Vec<&str> = texts.iter().map(Option::as_deref).collect::<Option<_>>()?;
    let list = jsonfmt::array_text(texts);
    let compact = match list_key {
        Some(k) => jsonfmt::with_member(sk, k, &list)?,
        None => list,
    };
    if serde_json::from_str::<Value>(&compact).ok().as_ref() != Some(doc) {
        return None;
    }
    let mut s = util::pretty(&compact);
    if style == "pretty2+newline" {
        s.push('\n');
    }
    Some(s)
}

/// A native record's text for laying it out again: `raw` (the source text,
/// when a carrier brought the record back re-spelled), else the record's own.
fn own_record_text(n: &Native) -> Option<String> {
    util::native_text(n)
}

fn import_json(rel: &str, file: Option<String>, text: &str, opts: &ImportOptions, _losses: &mut Losses) -> Result<Ledger> {
    let doc: Value = serde_json::from_str(text).with_context(|| format!("{rel}: not JSON"))?;
    let mut agent = agent_for(rel, get_str(&doc, &["sessionId"]), V_JSON, &doc);
    let mut msg_texts = Vec::new();
    if let Some(src) = agent.source.as_mut() {
        let (sk, texts) = skeleton(&doc, Some("messages"), text);
        msg_texts = texts;
        src.extra.extend(sk);
        src.extra.insert("version_hint".into(), json!(json_version_hint(&doc)));
    }
    let mut b = Builder::new(V_JSON, agent, opts.restore_carried);
    // A document-level carrier (titles, events no message shows).
    let mut doc_carried: Option<Vec<Event>> = None;
    if opts.restore_carried
        && let Some(c) = doc.get(carry::KEY)
    {
        let mut head = json!({"summary": doc.get("summary").cloned().unwrap_or(Value::Null)});
        head[carry::KEY] = c.clone();
        if let Some(a) = carried_agent(&head) {
            apply_agent(b.ledger.agent_mut(), &a);
        }
        doc_carried = carry::take(&mut head, &[]);
    }
    let msgs = doc.get("messages").and_then(Value::as_array).cloned().unwrap_or_default();
    for m in &msgs {
        b.prescan(m);
    }
    for (i, m) in msgs.iter().enumerate() {
        let nid = get_str(m, &["id"]).map(String::from).unwrap_or_else(|| format!("@{i}"));
        if b.restore(m) {
            continue;
        }
        let mut cands = Vec::new();
        b.message_cands(m, &mut cands);
        let mut n = b.native(&nid);
        jsonfmt::hold(&mut n, msg_texts.get(i).cloned().flatten().as_deref(), m);
        b.push(&nid, n, cands, get_str(m, &["timestamp"]).map(String::from), &file);
    }
    if let Some(events) = doc_carried {
        for e in events {
            b.ledger.append_unique(e);
        }
    } else if let Some(s) = doc.get("summary").and_then(Value::as_str) {
        // Rendered from the document skeleton on export; the event is the neutral view.
        let mut n = b.native("@summary");
        n.file = file.clone();
        let e = Event::new("@summary", Kind::Title).with("text", s).ts(get_str(&doc, &["lastUpdated"])).native(n);
        b.ledger.append_unique(e);
    }
    Ok(b.ledger)
}

fn import_checkpoint(rel: &str, file: Option<String>, text: &str, losses: &mut Losses) -> Result<Ledger> {
    let doc: Value = serde_json::from_str(text).with_context(|| format!("{rel}: not JSON"))?;
    let (history, list_key) = match &doc {
        Value::Array(a) => (a.clone(), None),
        o => (o.get("history").and_then(Value::as_array).cloned().unwrap_or_default(), Some("history")),
    };
    let stem = rel.rsplit('/').next().unwrap_or(rel).trim_end_matches(".json");
    let tag = stem.strip_prefix("checkpoint-").unwrap_or(stem);
    let tag = percent_decode(tag);
    let (mut extra, texts) = skeleton(&doc, list_key, text);
    extra.insert("file".into(), json!(rel));
    extra.insert("tag".into(), json!(tag));
    let agent = Agent {
        id: format!("checkpoint-{tag}"),
        created_at: None,
        source: Some(Source { format: NAME.into(), variant: Some(V_CKPT.into()), version: None, native_id: Some(tag.clone()), extra }),
        parent: None,
        links: vec![],
        meta: Obj::new(),
    };
    let mut b = Builder::new(V_CKPT, agent, false);
    let mut pending: Vec<(String, String)> = Vec::new(); // (name, call id) awaiting a response
    for (i, c) in history.iter().enumerate() {
        let nid = format!("@{i}");
        let role = get_str(c, &["role"]).unwrap_or("user");
        let parts = part_list(c.get("parts"));
        let mut cands = Vec::new();
        let real_user = role == "user" && parts.iter().any(|p| p.get("text").is_some());
        if real_user {
            b.turn = Some(nid.clone());
        }
        let group = Some(nid.as_str());
        for (j, p) in parts.iter().enumerate() {
            let key = format!("{nid}/{j}");
            let thought = p.get("thought").and_then(Value::as_bool) == Some(true);
            if let Some(fc) = p.get("functionCall") {
                let name = get_str(fc, &["name"]).unwrap_or("").to_string();
                let id = get_str(fc, &["id"]).map(String::from).unwrap_or_else(|| format!("{nid}.{j}"));
                pending.push((name.clone(), id.clone()));
                let e = b
                    .ev(Kind::ToolCall, &None, group)
                    .with("call_id", id.as_str())
                    .with("name", name.as_str())
                    .with("input", fc.get("args").cloned().unwrap_or(json!({})));
                cands.push(Cand { key, ev: e });
            } else if let Some(fr) = p.get("functionResponse") {
                let name = get_str(fr, &["name"]).unwrap_or("");
                let id = match get_str(fr, &["id"]) {
                    Some(id) => {
                        pending.retain(|(_, c)| c != id);
                        id.to_string()
                    }
                    None => match pending.iter().position(|(n, _)| n == name) {
                        Some(k) => pending.remove(k).1,
                        None => format!("{nid}.{j}"),
                    },
                };
                let e = result_event(b.ev(Kind::ToolResult, &None, None), &id, fr.get("response"), "");
                cands.push(Cand { key, ev: e });
            } else if let Some(t) = p.get("text").and_then(Value::as_str) {
                let e = if thought {
                    let mut e = b.ev(Kind::Reasoning, &None, group).with("text", t);
                    if let Some(sig) = p.get("thoughtSignature").and_then(Value::as_str) {
                        e = e.with("sealed", json!({"vendor": "google", "kind": "signature", "data": sig}));
                    }
                    e
                } else {
                    let r = if role == "model" { "assistant" } else { "user" };
                    b.ev(Kind::Text, &None, group).with("role", r).with("text", t)
                };
                cands.push(Cand { key, ev: e });
            } else if let Some(m) = media_of(p) {
                let r = if role == "model" { "assistant" } else { "user" };
                let mut e = b.ev(Kind::Media, &None, group).with("role", r);
                e.body.extend(m);
                cands.push(Cand { key, ev: e });
            } else {
                losses.add(
                    "part-kept-native",
                    Some(Kind::Native),
                    "a Content part with no neutral meaning (executableCode, codeExecutionResult, ...) stays in native",
                );
            }
        }
        let mut n = b.native(&nid);
        jsonfmt::hold(&mut n, texts.get(i).cloned().flatten().as_deref(), c);
        b.push(&nid, n, cands, None, &file);
    }
    Ok(b.ledger)
}

fn percent_decode(s: &str) -> String {
    let b = s.as_bytes();
    let mut out = Vec::with_capacity(b.len());
    let mut i = 0;
    while i < b.len() {
        if b[i] == b'%'
            && i + 2 < b.len()
            && let Ok(x) = u8::from_str_radix(&s[i + 1..i + 3], 16)
        {
            out.push(x);
            i += 3;
            continue;
        }
        out.push(b[i]);
        i += 1;
    }
    String::from_utf8_lossy(&out).to_string()
}

/// `encodeURIComponent`.
fn percent_encode(s: &str) -> String {
    let mut out = String::new();
    for &c in s.as_bytes() {
        if c.is_ascii_alphanumeric() || b"-_.!~*'()".contains(&c) {
            out.push(c as char);
        } else {
            out.push_str(&format!("%{c:02X}"));
        }
    }
    out
}

// ---------------------------------------------------------------------------
// export

fn export(set: &LedgerSet, opts: &ExportOptions) -> Result<Exported> {
    let mut out = Output::default();
    let mut losses = Losses::default();
    let want = opts.variant.as_deref();
    if let Some(v) = want
        && ![V_JSONL, V_JSON, V_CKPT].contains(&v)
    {
        bail!("gemini-cli: unknown variant {v}");
    }
    let mut variant_used = String::new();
    // Session ids, so subagent files can be placed under their parent.
    let sid: BTreeMap<String, String> = set.iter().map(|l| (l.id().to_string(), session_id_for(l))).collect();
    for l in set.iter() {
        let src = l.agent().source.as_ref().filter(|s| s.format == NAME);
        let own_variant = src.and_then(|s| s.variant.clone());
        let variant = match (want, own_variant.as_deref()) {
            (Some(w), _) => w.to_string(),
            (None, Some(v)) => v.to_string(),
            (None, None) => {
                if opts.target_version.as_deref().is_some_and(|v| version_lt(v, "0.39.0")) {
                    V_JSON.to_string()
                } else {
                    V_JSONL.to_string()
                }
            }
        };
        let same = own_variant.as_deref() == Some(variant.as_str());
        let path = match src.and_then(|s| s.extra.get("file")).and_then(Value::as_str).filter(|_| same) {
            Some(f) => f.to_string(),
            None => default_path(l, set, &sid, &variant),
        };
        if variant != V_CKPT {
            // SPEC §2.3: a session is one message list; `render` writes the
            // active path and an abandoned event rides on the next record.
            crate::formats::active_path_only(l, &mut losses, "with carry they ride on the next record", own_native);
        }
        let bytes = match variant.as_str() {
            V_JSONL => export_jsonl(l, same, &sid, opts, &mut losses),
            V_JSON => export_json(l, same, &sid, opts, &mut losses),
            _ => export_checkpoint(l, same, opts, &mut losses),
        };
        if variant_used.is_empty() {
            variant_used = variant.clone();
        }
        out.put(path, bytes);
    }
    Ok(Exported { output: out, losses: losses.sorted(), variant: variant_used })
}

fn version_lt(a: &str, b: &str) -> bool {
    let p = |s: &str| s.trim_start_matches('v').split(['.', '-']).take(3).map(|x| x.parse::<u64>().unwrap_or(0)).collect::<Vec<_>>();
    p(a) < p(b)
}

fn session_id_for(l: &Ledger) -> String {
    let id = l.id();
    if uuid::Uuid::parse_str(id).is_ok() {
        return id.to_string();
    }
    if let Some(n) = l.agent().source.as_ref().filter(|s| s.format == NAME).and_then(|s| s.native_id.clone()) {
        return n;
    }
    util::uuid_for(id)
}

fn default_path(l: &Ledger, set: &LedgerSet, sid: &BTreeMap<String, String>, variant: &str) -> String {
    let me = &sid[l.id()];
    let ext = if variant == V_JSONL { "jsonl" } else { "json" };
    if variant == V_CKPT {
        return format!("checkpoint-{}.json", percent_encode(l.id()));
    }
    if let Some(p) = l.agent().parent.as_ref().filter(|p| set.get(&p.agent).is_some()) {
        return format!("{}/{me}.{ext}", sid[&p.agent]);
    }
    let ts = l.agent().created_at.clone().or_else(|| l.events.iter().find_map(|e| e.ts.clone())).unwrap_or_else(|| "1970-01-01T00:00:00.000Z".into());
    let ts = util::norm_ts(&ts);
    let stamp = ts.get(..16).unwrap_or("1970-01-01T00:00").replace(':', "-");
    format!("session-{stamp}-{}.{ext}", &me[..8.min(me.len())])
}

/// Is `e` a record of this format and variant that the exporter can emit as is?
fn own<'a>(e: &'a Event, variant: &str) -> Option<&'a Native> {
    e.native.as_ref().filter(|n| n.format == NAME && n.variant.as_deref() == Some(variant))
}

fn export_jsonl(l: &Ledger, same: bool, sid: &BTreeMap<String, String>, opts: &ExportOptions, losses: &mut Losses) -> Vec<u8> {
    let extra = l.agent().source.as_ref().filter(|_| same).map(|s| s.extra.clone()).unwrap_or_default();
    let mut lines: Vec<String> = Vec::new();
    let mut have_meta = false;
    if let Some(m) = extra.get("meta_raw").and_then(Value::as_str) {
        lines.push(m.to_string());
        have_meta = true;
    } else if let Some(m) = extra.get("meta_record") {
        lines.push(serde_json::to_string(m).unwrap());
        have_meta = true;
    }
    let mut foreign: Vec<&Event> = Vec::new();
    let mut flush = |foreign: &mut Vec<&Event>, lines: &mut Vec<String>, have_meta: &mut bool| {
        if foreign.is_empty() {
            return;
        }
        let r = render(l, foreign, sid, opts, losses);
        foreign.clear();
        let mut pending = r.meta_events;
        let last_updated = r.meta["lastUpdated"].clone();
        if !*have_meta {
            let mut meta = r.meta;
            if opts.carry {
                attach_with_agent(&mut meta, &pending, l.agent());
            }
            pending.clear();
            lines.push(serde_json::to_string(&meta).unwrap());
            *have_meta = true;
        }
        let at = if r.summary.is_some() { r.summary_at } else { r.messages.len() };
        for m in &r.messages[..at] {
            lines.push(serde_json::to_string(m).unwrap());
        }
        let tail: Vec<String> = r.messages[at..].iter().map(|m| serde_json::to_string(m).unwrap()).collect();
        let mut set = match &r.summary {
            Some(s) => json!({"$set": {"summary": s}}),
            None if !pending.is_empty() || !r.title_events.is_empty() => {
                json!({"$set": {"lastUpdated": last_updated}})
            }
            None => {
                lines.extend(tail);
                return;
            }
        };
        pending.extend(r.title_events);
        if opts.carry {
            carry::attach(&mut set, &[], &pending);
        }
        lines.push(serde_json::to_string(&set).unwrap());
        lines.extend(tail);
    };
    for e in &l.events {
        match own(e, V_JSONL).filter(|_| same) {
            Some(n) => {
                flush(&mut foreign, &mut lines, &mut have_meta);
                if n.part == 0
                    && let Some(t) = util::native_text(n)
                {
                    lines.push(t);
                }
            }
            None => foreign.push(e),
        }
    }
    flush(&mut foreign, &mut lines, &mut have_meta);
    if lines.is_empty() {
        // An agent with no events still gets its metadata line.
        let mut meta = render(l, &[], sid, opts, losses).meta;
        if opts.carry {
            attach_with_agent(&mut meta, &[], l.agent());
        }
        lines.push(serde_json::to_string(&meta).unwrap());
    }
    let mut s = lines.join("\n");
    if extra.get("no_final_newline").is_none() && !s.is_empty() {
        s.push('\n');
    }
    s.into_bytes()
}

fn export_json(l: &Ledger, same: bool, sid: &BTreeMap<String, String>, opts: &ExportOptions, losses: &mut Losses) -> Vec<u8> {
    let extra = l.agent().source.as_ref().filter(|_| same).map(|s| s.extra.clone()).unwrap_or_default();
    let all_own = same && l.events.iter().all(|e| own(e, V_JSON).is_some());
    let mut messages: Vec<Value> = Vec::new();
    let mut texts: Vec<Option<String>> = Vec::new();
    let mut foreign: Vec<&Event> = Vec::new();
    let mut doc = extra.get("document").cloned();
    let mut summary: Option<String> = None;
    let mut last_ts: Option<Value> = None;
    let mut doc_events: Vec<Event> = Vec::new();
    let mut flush =
        |foreign: &mut Vec<&Event>, messages: &mut Vec<Value>, doc: &mut Option<Value>, summary: &mut Option<String>, last_ts: &mut Option<Value>| {
            if foreign.is_empty() {
                return;
            }
            let r = render(l, foreign, sid, opts, losses);
            foreign.clear();
            if doc.is_none() {
                let mut d = r.meta.clone();
                d["messages"] = json!([]);
                *doc = Some(d);
            } else {
                *last_ts = Some(r.meta["lastUpdated"].clone());
            }
            messages.extend(r.messages);
            if r.summary.is_some() {
                *summary = r.summary;
            }
            doc_events.extend(r.meta_events);
            doc_events.extend(r.title_events);
        };
    for e in &l.events {
        match own(e, V_JSON).filter(|_| same) {
            Some(n) => {
                flush(&mut foreign, &mut messages, &mut doc, &mut summary, &mut last_ts);
                if n.part == 0
                    && e.kind != Kind::Title
                    && let Some(r) = &n.record_json()
                {
                    messages.push(r.clone());
                    texts.push(own_record_text(n));
                }
            }
            None => foreign.push(e),
        }
    }
    flush(&mut foreign, &mut messages, &mut doc, &mut summary, &mut last_ts);
    let mut doc = doc.unwrap_or_else(|| {
        let mut d = render(l, &[], sid, opts, losses).meta;
        d["messages"] = json!([]);
        d
    });
    // `messages` keeps its position in the document.
    doc["messages"] = Value::Array(messages);
    if let Some(s) = summary {
        doc["summary"] = json!(s);
    }
    if let Some(t) = last_ts {
        doc["lastUpdated"] = t;
    }
    if opts.carry && (!doc_events.is_empty() || !same) {
        let mut head = json!({"summary": doc.get("summary").cloned().unwrap_or(Value::Null)});
        attach_with_agent(&mut head, &doc_events, l.agent());
        doc[carry::KEY] = head[carry::KEY].clone();
    }
    // A document kept whole (no layout reproduces it) is written back as read
    // only while its records still say what it says (an edit is not lost).
    if all_own
        && let Some(raw) = extra.get("raw_file").and_then(Value::as_str)
        && util::same_json(raw, &serde_json::to_string(&doc).expect("json"))
    {
        return raw.as_bytes().to_vec();
    }
    if all_own {
        if let Some(s) = own_text(&extra, Some("messages"), &texts, &doc) {
            return s.into_bytes();
        }
        losses.add(jsonfmt::EXACT_TEXT_FALLBACK, None, jsonfmt::EXACT_TEXT_FALLBACK_DETAIL);
    }
    let mut s = pretty(&doc);
    if extra.get("style").and_then(Value::as_str) == Some("pretty2+newline") {
        s.push('\n');
    }
    s.into_bytes()
}

/// A token count as JSON: an integer when it is one.
fn count(x: f64) -> Value {
    if x.fract() == 0.0 && (0.0..9.007_199_254_740_992e15).contains(&x) { json!(x as u64) } else { json!(x) }
}

/// A foreign run of events rendered as Gemini CLI records.
struct Rendered {
    /// The metadata line (JSONL) / document head (JSON).
    meta: Value,
    messages: Vec<Value>,
    summary: Option<String>,
    /// Events no message shows (when there are no messages) plus titles,
    /// carried at the line / document level.
    meta_events: Vec<Event>,
    title_events: Vec<Event>,
    /// How many of `messages` precede the summary update.
    summary_at: usize,
}

#[derive(Default)]
struct Msg {
    rec: Map<String, Value>,
    events: Vec<Event>,
    is_gemini: bool,
    has_calls: bool,
}

/// A Gemini CLI record of any variant: its branch, if any, is its own.
fn own_native(e: &Event) -> bool {
    e.native.as_ref().is_some_and(|n| n.format == NAME)
}

fn render(l: &Ledger, events: &[&Event], sid: &BTreeMap<String, String>, opts: &ExportOptions, losses: &mut Losses) -> Rendered {
    let a = l.agent();
    let on = l.on_path(|e| !own_native(e));
    let off: std::collections::HashSet<&str> = l.events.iter().zip(&on).filter(|(_, o)| !**o).map(|(e, _)| e.id.as_str()).collect();
    let session = sid[l.id()].clone();
    let started = a.created_at.clone().or_else(|| l.events.iter().find_map(|e| e.ts.clone())).unwrap_or_else(|| "1970-01-01T00:00:00.000Z".into());
    let last = l.events.iter().rev().find_map(|e| e.ts.clone()).unwrap_or_else(|| started.clone());
    let mut cwd: Option<String> = None;
    for e in &l.events {
        if e.kind == Kind::Settings
            && let Some(c) = e.str("cwd")
        {
            cwd = Some(c.to_string());
        }
    }
    let project_hash =
        a.source.as_ref().filter(|s| s.format == NAME).and_then(|s| s.extra.get("projectHash").and_then(Value::as_str).map(String::from)).unwrap_or_else(
            || {
                use sha2::{Digest, Sha256};
                hex::encode(Sha256::digest(cwd.clone().unwrap_or_else(|| "/".into()).as_bytes()))
            },
        );
    if !opts.carry && (!a.meta.is_empty() || !a.links.is_empty()) {
        losses.add("agent-meta-dropped", None, "agent meta and links have no place in a session without carry");
    }
    let mut meta = Map::new();
    meta.insert("sessionId".into(), json!(session));
    meta.insert("projectHash".into(), json!(project_hash));
    meta.insert("startTime".into(), json!(util::norm_ts(&started)));
    meta.insert("lastUpdated".into(), json!(util::norm_ts(&last)));
    if a.parent.is_some() {
        meta.insert("kind".into(), json!("subagent"));
    } else {
        meta.insert("kind".into(), json!("main"));
    }
    let mut msgs: Vec<Msg> = Vec::new();
    let mut summary = None;
    let mut title_events: Vec<Event> = Vec::new();
    let mut summary_at = 0usize;
    let mut model: Option<String> = None;
    let mut call_at: HashMap<String, (usize, usize)> = HashMap::new();
    let mut orphan: Vec<Event> = Vec::new(); // events no record shows, carried by the next/previous record
    let ts_of = |e: &Event| util::norm_ts(e.ts.as_deref().unwrap_or(&last));
    let new_msg = |msgs: &mut Vec<Msg>, e: &Event, ty: &str| {
        let mut rec = Map::new();
        rec.insert("id".into(), json!(util::uuid_for(&e.id)));
        rec.insert("timestamp".into(), json!(ts_of(e)));
        rec.insert("type".into(), json!(ty));
        rec.insert("content".into(), if ty == "gemini" { json!("") } else { json!([]) });
        msgs.push(Msg { rec, events: vec![], is_gemini: ty == "gemini", has_calls: false });
    };
    // Need a fresh gemini message for this event?
    let need_gemini = |msgs: &Vec<Msg>, e: &Event, after_text: bool| -> bool {
        match msgs.last() {
            // Consecutive model messages merge (a resumed history must
            // alternate); after tool calls, a new model message follows the
            // tool responses.
            Some(m) if m.is_gemini => {
                let g = m.events.iter().rev().find_map(|f| f.group.clone());
                m.has_calls && (after_text || (g.is_some() && e.group.is_some() && g != e.group))
            }
            _ => true,
        }
    };
    for &e in events {
        if off.contains(e.id.as_str()) {
            orphan.push(e.clone());
            continue;
        }
        let k = e.kind;
        match k {
            Kind::Text | Kind::Media | Kind::PeerMessage | Kind::Compaction => {
                let role = match k {
                    Kind::PeerMessage => {
                        if crate::formats::pi_session::peer_has_more(e) {
                            losses.add(crate::formats::pi_session::PEER_FIELDS_DROPPED, Some(k), crate::formats::pi_session::PEER_FIELDS_DROPPED_DETAIL);
                        }
                        crate::formats::pi_session::peer_as_text(e).0
                    }
                    Kind::Compaction => {
                        if e.str("phase") != Some("summary") || e.str("summary").is_none() {
                            losses.add("compaction-boundary-dropped", Some(k), "compaction boundaries have no session record");
                            orphan.push(e.clone());
                            continue;
                        }
                        losses.add(
                            "compaction-as-user-text",
                            Some(k),
                            "a compaction summary becomes a user message, as Gemini CLI's own /compress history does",
                        );
                        "user"
                    }
                    _ => e.str("role").unwrap_or("user"),
                };
                let text = match k {
                    Kind::Compaction => e.str("summary").unwrap_or("").to_string(),
                    Kind::PeerMessage => crate::formats::pi_session::peer_wrap(e),
                    _ => e.str("text").unwrap_or("").to_string(),
                };
                if k == Kind::Media && role != "user" {
                    losses.add("media-dropped", Some(k), "Gemini CLI records no assistant media");
                    orphan.push(e.clone());
                    continue;
                }
                match role {
                    "assistant" => {
                        if need_gemini(&msgs, e, true) {
                            new_msg(&mut msgs, e, "gemini");
                        }
                        let m = msgs.last_mut().unwrap();
                        let cur = m.rec["content"].as_str().unwrap_or("").to_string();
                        m.rec.insert("content".into(), json!(cur + &text));
                        m.events.push(e.clone());
                    }
                    "system" | "developer" => {
                        losses.add("system-text-as-info", Some(k), "system/developer text becomes an `info` record, which the model does not see on resume");
                        new_msg(&mut msgs, e, "info");
                        let m = msgs.last_mut().unwrap();
                        m.rec.insert("content".into(), json!(text));
                        m.events.push(e.clone());
                    }
                    _ => {
                        let part = if k == Kind::Media {
                            match media_part(e) {
                                Some(p) => p,
                                None => {
                                    losses.add("media-dropped", Some(k), "media given only by path cannot be inlined");
                                    orphan.push(e.clone());
                                    continue;
                                }
                            }
                        } else {
                            json!({"text": text})
                        };
                        // Consecutive user records merge, so a resumed history alternates.
                        let fresh = !matches!(msgs.last(), Some(m) if m.rec["type"] == "user");
                        if fresh {
                            new_msg(&mut msgs, e, "user");
                        }
                        let m = msgs.last_mut().unwrap();
                        m.rec.get_mut("content").unwrap().as_array_mut().unwrap().push(part);
                        m.events.push(e.clone());
                    }
                }
            }
            Kind::Reasoning => {
                let text = e.str("text").unwrap_or("").to_string();
                if e.body.get("sealed").is_some() {
                    losses.add("reasoning-sealed-dropped", Some(k), "session records keep thought summaries, not signatures or encrypted reasoning");
                }
                if text.is_empty() {
                    if e.body.get("sealed").is_none() {
                        losses.add("reasoning-redacted-dropped", Some(k), "reasoning with no text");
                    }
                    orphan.push(e.clone());
                    continue;
                }
                if need_gemini(&msgs, e, true) {
                    new_msg(&mut msgs, e, "gemini");
                }
                // A leading `**subject** ` is the thought's subject (see `thought_text`).
                let (subject, text) = match text.strip_prefix("**").and_then(|r| r.split_once("** ")) {
                    Some((s, d)) if !s.is_empty() && !s.contains("**") => (s.to_string(), d.to_string()),
                    _ => (String::new(), text.to_string()),
                };
                let m = msgs.last_mut().unwrap();
                let th = m.rec.entry("thoughts").or_insert(json!([]));
                th.as_array_mut().unwrap().push(json!({"subject": subject, "description": text, "timestamp": ts_of(e)}));
                m.events.push(e.clone());
            }
            Kind::ToolCall => {
                if need_gemini(&msgs, e, false) {
                    new_msg(&mut msgs, e, "gemini");
                }
                let call_id = e.str("call_id").map(String::from).unwrap_or_else(|| util::short_id(&e.id, 12));
                let args = crate::formats::pi_session::tool_args_object(e, losses);
                let idx = msgs.len() - 1;
                let m = msgs.last_mut().unwrap();
                let tcs = m.rec.entry("toolCalls").or_insert(json!([]));
                let a = tcs.as_array_mut().unwrap();
                call_at.insert(call_id.clone(), (idx, a.len()));
                a.push(json!({
                    "id": call_id,
                    "name": e.str("name").unwrap_or("unknown"),
                    "args": args,
                    "result": null,
                    "status": "cancelled",
                    "timestamp": ts_of(e),
                }));
                m.has_calls = true;
                m.events.push(e.clone());
            }
            Kind::ToolResult => {
                let call_id = e.str("call_id").unwrap_or("").to_string();
                let (text, media) = output_text(e.body.get("output"));
                if media {
                    losses.add("tool-result-media-dropped", Some(k), "media in tool output is not recorded");
                }
                let is_err = e.body.get("is_error").and_then(Value::as_bool) == Some(true);
                let status = if e.str("status") == Some("cancelled") {
                    "cancelled"
                } else if is_err {
                    "error"
                } else {
                    "success"
                };
                match call_at.get(&call_id) {
                    Some(&(mi, ci)) => {
                        let m = &mut msgs[mi];
                        let tc = &mut m.rec.get_mut("toolCalls").unwrap()[ci];
                        let name = tc["name"].clone();
                        let resp = if is_err { json!({"error": text}) } else { json!({"output": text}) };
                        tc["result"] = json!([{"functionResponse": {"id": call_id, "name": name, "response": resp}}]);
                        tc["status"] = json!(status);
                        tc["timestamp"] = json!(ts_of(e));
                        // Carried by the next record, so a carry trip keeps its place.
                        orphan.push(e.clone());
                    }
                    None => {
                        losses.add(
                            "tool-result-unpaired",
                            Some(k),
                            "a result whose call is not in this ledger becomes a functionResponse part of a user message",
                        );
                        new_msg(&mut msgs, e, "user");
                        let m = msgs.last_mut().unwrap();
                        let resp = if is_err { json!({"error": text}) } else { json!({"output": text}) };
                        m.rec
                            .get_mut("content")
                            .unwrap()
                            .as_array_mut()
                            .unwrap()
                            .push(json!({"functionResponse": {"id": call_id, "name": "unknown", "response": resp}}));
                        m.events.push(e.clone());
                    }
                }
            }
            Kind::Usage => {
                if e.body.get("cumulative").and_then(Value::as_bool) == Some(true) {
                    losses.add("usage-cumulative-dropped", Some(k), "session totals have no record; per-message tokens only");
                    orphan.push(e.clone());
                    continue;
                }
                let target = msgs.iter_mut().rev().find(|m| m.is_gemini && (e.group.is_none() || m.events.first().is_some_and(|f| f.group == e.group)));
                match target {
                    Some(m) if m.rec.get("tokens").is_none() => {
                        // SPEC §3.1.1 back into Gemini's counts: `input` with
                        // the cached tokens, `output` without the thoughts.
                        let n = |key: &str| e.body.get(key).and_then(Value::as_f64).unwrap_or(0.0);
                        let (c, r) = (n("cache_read"), n("reasoning"));
                        let (i, o) = (n("input") + c, (n("output") - r).max(0.0));
                        let total = (i + o + r) as u64;
                        let (i, o, c, r) = (count(i), count(o), count(c), count(r));
                        let mut tok = json!({"input": i, "output": o, "cached": c});
                        if e.body.get("reasoning").is_some() {
                            tok["thoughts"] = r;
                        }
                        tok["tool"] = json!(0);
                        tok["total"] = json!(total);
                        m.rec.insert("tokens".into(), tok);
                        if e.body.get("cache_write").is_some() || e.body.get("cost_usd").is_some() {
                            losses.add("usage-fields-dropped", Some(k), "cache writes and cost have no Gemini CLI field");
                        }
                        if let Some(md) = e.str("model") {
                            m.rec.entry("model").or_insert(json!(md));
                        }
                        orphan.push(e.clone());
                    }
                    _ => {
                        losses.add("usage-unattached", Some(k), "usage with no message to attach to");
                        orphan.push(e.clone());
                    }
                }
            }
            Kind::Settings => {
                if let Some(md) = e.str("model") {
                    model = Some(md.to_string());
                }
                let other = e.body.keys().any(|k| !matches!(k.as_str(), "model" | "cwd" | "complete"));
                if other {
                    losses.add("settings-dropped", Some(k), "only model and cwd (as projectHash) have a place in a session");
                }
                orphan.push(e.clone());
            }
            Kind::Title => {
                summary = e.str("text").map(String::from);
                summary_at = msgs.len();
                title_events.push(e.clone());
            }
            Kind::Error => {
                if !matches!(e.str("kind"), None | Some("error") | Some("warning")) || e.body.get("retryable").is_some() || e.body.get("status").is_some() {
                    losses.add("error-fields-dropped", Some(k), "an error record holds only its message");
                }
                new_msg(&mut msgs, e, if e.str("kind") == Some("warning") { "warning" } else { "error" });
                let m = msgs.last_mut().unwrap();
                m.rec.insert("content".into(), json!(e.str("message").unwrap_or("")));
                m.events.push(e.clone());
            }
            Kind::Subagent => {
                let child = e.str("agent").and_then(|c| sid.get(c));
                let call = e.str("call_id").and_then(|c| call_at.get(c)).copied();
                match (child, call, e.str("phase")) {
                    (Some(c), Some((mi, ci)), Some("start")) => {
                        let tc = &mut msgs[mi].rec.get_mut("toolCalls").unwrap()[ci];
                        tc["agentId"] = json!(c);
                        orphan.push(e.clone());
                    }
                    (_, Some(_), _) => orphan.push(e.clone()),
                    _ => {
                        losses.add("subagent-event-dropped", Some(k), "a subagent event with no matching tool call");
                        orphan.push(e.clone());
                    }
                }
            }
            Kind::FileChange => {
                losses.add("file-change-dropped", Some(k), "file changes are only recorded as a tool's resultDisplay");
                orphan.push(e.clone());
            }
            Kind::Turn => {
                losses.add("turn-dropped", Some(k), "Gemini CLI records no turn boundaries");
                orphan.push(e.clone());
            }
            Kind::App | Kind::Native => {
                losses.add(if k == Kind::App { "app-dropped" } else { "foreign-native-dropped" }, Some(k), "no Gemini CLI record for it");
                orphan.push(e.clone());
            }
        }
        if let Some(m) = msgs.last_mut() {
            if m.is_gemini
                && m.rec.get("model").is_none()
                && let Some(md) = &model
            {
                m.rec.insert("model".into(), json!(md));
            }
            // Events that no record shows ride with the next record made.
            if !orphan.is_empty() && m.events.last().is_some_and(|x| x.id == e.id) {
                let tail = m.events.pop().unwrap();
                m.events.append(&mut orphan);
                m.events.push(tail);
            }
        }
    }
    // A call with no result would leave the resumed history with an
    // unanswered functionCall; close it the way Gemini CLI itself does.
    for m in msgs.iter_mut() {
        if let Some(tcs) = m.rec.get_mut("toolCalls").and_then(Value::as_array_mut) {
            for tc in tcs.iter_mut() {
                if tc["result"].is_null() {
                    losses.add(
                        "tool-result-synthesized",
                        Some(Kind::ToolResult),
                        "a call with no result is closed with Gemini CLI's own 'Response was lost or interrupted.' error",
                    );
                    tc["result"] = json!([{"functionResponse": {"id": tc["id"].clone(), "name": tc["name"].clone(), "response": {"error": INTERRUPTED}}}]);
                }
            }
        }
    }
    // Normalise each gemini message to the field order Gemini CLI writes.
    let mut out = Vec::new();
    let mut meta_events: Vec<Event> = Vec::new();
    if msgs.is_empty() {
        meta_events.append(&mut orphan);
    } else if !orphan.is_empty() {
        msgs.last_mut().unwrap().events.append(&mut orphan);
    }
    for m in msgs {
        let mut rec = Map::new();
        for k in ["id", "timestamp", "type", "content", "thoughts", "tokens", "model", "toolCalls"] {
            if let Some(v) = m.rec.get(k) {
                rec.insert(k.into(), v.clone());
            }
        }
        if m.is_gemini && !rec.contains_key("thoughts") {
            let mut r2 = Map::new();
            for (k, v) in rec {
                r2.insert(k.clone(), v);
                if k == "content" {
                    r2.insert("thoughts".into(), json!([]));
                }
            }
            rec = r2;
        }
        let mut v = Value::Object(rec);
        if opts.carry {
            carry::attach(&mut v, &[], &m.events);
        }
        out.push(v);
    }
    Rendered { meta: Value::Object(meta), messages: out, summary, meta_events, title_events, summary_at }
}

/// A carrier that also holds the foreign agent header (id, parent, links,
/// meta), so a carry trip restores the header too. Written even with no
/// events. Built by [`carry::attach_with`], so a carried native record whose
/// `Value` would re-spell it also has its exact text under `"records"`, and
/// [`carry::take`] restores it byte for byte.
pub(crate) fn attach_with_agent(v: &mut Value, events: &[Event], agent: &Agent) {
    carry::attach_with(v, &[], events, Some(agent));
    // The header as this carrier has always spelled it (every field present);
    // the source too: restoring it lets the source format write its own files
    // back exactly (X → P → here → P → X).
    let a = json!({"id": agent.id, "created_at": agent.created_at, "source": agent.source, "parent": agent.parent, "links": agent.links, "meta": agent.meta});
    if let Some(c) = v.get_mut(carry::KEY) {
        c["agent"] = a;
    }
}

/// The carried agent header of `v`, if its carrier is intact.
pub(crate) fn carried_agent(v: &Value) -> Option<Value> {
    let c = v.get(carry::KEY)?;
    let a = c.get("agent")?.clone();
    (carry::record_hash(v) == c.get("h")?.as_str()?).then_some(a)
}

pub(crate) fn apply_agent(agent: &mut Agent, a: &Value) {
    if let Some(id) = a.get("id").and_then(Value::as_str) {
        agent.id = id.to_string();
    }
    agent.created_at = a.get("created_at").and_then(Value::as_str).map(String::from);
    if let Some(src) = a.get("source") {
        agent.source = serde_json::from_value(src.clone()).ok();
    }
    agent.parent = a.get("parent").and_then(|p| serde_json::from_value(p.clone()).ok());
    agent.links = a.get("links").and_then(|p| serde_json::from_value(p.clone()).ok()).unwrap_or_default();
    agent.meta = a.get("meta").and_then(Value::as_object).cloned().unwrap_or_default();
}

fn media_part(e: &Event) -> Option<Value> {
    let mt = e.str("media_type").unwrap_or("application/octet-stream");
    if let Some(d) = e.str("data") {
        return Some(json!({"inlineData": {"mimeType": mt, "data": d}}));
    }
    if let Some(u) = e.str("url") {
        return Some(json!({"fileData": {"mimeType": mt, "fileUri": u}}));
    }
    None
}

/// Tool output as text; `true` if media parts were dropped.
fn output_text(v: Option<&Value>) -> (String, bool) {
    match v {
        None | Some(Value::Null) => (String::new(), false),
        Some(Value::String(s)) => (s.clone(), false),
        Some(Value::Array(a)) => {
            let mut media = false;
            let mut s = Vec::new();
            for p in a {
                match p.get("type").and_then(Value::as_str) {
                    Some("text") => s.push(p.get("text").and_then(Value::as_str).unwrap_or("").to_string()),
                    _ => media = true,
                }
            }
            (s.join("\n"), media)
        }
        Some(o) => (o.to_string(), false),
    }
}

fn export_checkpoint(l: &Ledger, same: bool, opts: &ExportOptions, losses: &mut Losses) -> Vec<u8> {
    let extra = l.agent().source.as_ref().filter(|_| same).map(|s| s.extra.clone()).unwrap_or_default();
    let all_own = same && l.events.iter().all(|e| own(e, V_CKPT).is_some());
    let mut history: Vec<Value> = Vec::new();
    let push_part = |history: &mut Vec<Value>, role: &str, part: Value| match history.last_mut() {
        Some(c) if c["role"] == role && c.get("protoledger_own").is_none() => c["parts"].as_array_mut().unwrap().push(part),
        _ => history.push(json!({"role": role, "parts": [part]})),
    };
    let mut names: HashMap<String, String> = HashMap::new();
    let mut texts: Vec<Option<String>> = Vec::new();
    for e in &l.events {
        if let Some(n) = own(e, V_CKPT).filter(|_| same) {
            if n.part == 0
                && let Some(r) = &n.record_json()
            {
                texts.push(own_record_text(n));
                let mut r = r.clone();
                // Mark so a following foreign part does not merge into it.
                r["protoledger_own"] = json!(true);
                history.push(r);
            }
            continue;
        }
        let k = e.kind;
        match k {
            Kind::Text => match e.str("role") {
                Some("assistant") => push_part(&mut history, "model", json!({"text": e.str("text").unwrap_or("")})),
                Some("user") | None => push_part(&mut history, "user", json!({"text": e.str("text").unwrap_or("")})),
                _ => losses.add("system-text-dropped", Some(k), "a checkpoint holds only user/model turns"),
            },
            Kind::Media => match (media_part(e), e.str("role")) {
                (Some(p), Some("assistant")) => push_part(&mut history, "model", p),
                (Some(p), _) => push_part(&mut history, "user", p),
                (None, _) => losses.add("media-dropped", Some(k), "media given only by path cannot be inlined"),
            },
            Kind::Reasoning => {
                let mut p = json!({"text": e.str("text").unwrap_or(""), "thought": true});
                match e.body.get("sealed") {
                    Some(s) if s.get("vendor").and_then(Value::as_str) == Some("google") && s.get("kind").and_then(Value::as_str) == Some("signature") => {
                        p["thoughtSignature"] = s["data"].clone();
                    }
                    Some(_) => losses.add("reasoning-sealed-dropped", Some(k), "only Google thought signatures can be replayed"),
                    None => {}
                }
                push_part(&mut history, "model", p);
            }
            Kind::ToolCall => {
                let id = e.str("call_id").unwrap_or("").to_string();
                let name = e.str("name").unwrap_or("unknown").to_string();
                names.insert(id.clone(), name.clone());
                let args = crate::formats::pi_session::tool_args_object(e, losses);
                push_part(&mut history, "model", json!({"functionCall": {"id": id, "name": name, "args": args}}));
            }
            Kind::ToolResult => {
                let id = e.str("call_id").unwrap_or("").to_string();
                let (text, media) = output_text(e.body.get("output"));
                if media {
                    losses.add("tool-result-media-dropped", Some(k), "media in tool output is not kept");
                }
                let resp = if e.body.get("is_error").and_then(Value::as_bool) == Some(true) { json!({"error": text}) } else { json!({"output": text}) };
                let name = names.get(&id).cloned().unwrap_or_else(|| "unknown".into());
                push_part(&mut history, "user", json!({"functionResponse": {"id": id, "name": name, "response": resp}}));
            }
            Kind::Compaction if e.str("summary").is_some() => {
                losses.add("compaction-as-user-text", Some(k), "a compaction summary becomes user text");
                push_part(&mut history, "user", json!({"text": e.str("summary").unwrap_or("")}));
            }
            _ => losses.add(&format!("{}-dropped", k.as_str().replace('_', "-")), Some(k), "a checkpoint holds only the model's Content history"),
        }
    }
    for c in history.iter_mut() {
        if let Some(o) = c.as_object_mut() {
            o.remove_ordered("protoledger_own");
        }
    }
    // Every functionCall needs a functionResponse in the next user turn.
    let mut i = 0;
    while i < history.len() {
        let calls: Vec<(Value, Value)> = history[i]["parts"]
            .as_array()
            .into_iter()
            .flatten()
            .filter_map(|p| {
                p.get("functionCall").map(|f| (f.get("id").cloned().unwrap_or(Value::Null), f.get("name").cloned().unwrap_or(json!("generic_tool"))))
            })
            .collect();
        if history[i]["role"] == "model" && !calls.is_empty() {
            if history.get(i + 1).is_none_or(|n| n["role"] != "user") {
                history.insert(i + 1, json!({"role": "user", "parts": []}));
            }
            // Answered in the next turn, or (out of order, as a transcript that
            // interleaves a subagent can be) anywhere later.
            let answered: Vec<Value> = history[i + 1..]
                .iter()
                .flat_map(|c| c["parts"].as_array().cloned().unwrap_or_default())
                .filter_map(|p| p.get("functionResponse").and_then(|f| f.get("id")).cloned())
                .collect();
            for (id, name) in calls {
                if !answered.contains(&id) {
                    losses.add(
                        "tool-result-synthesized",
                        Some(Kind::ToolResult),
                        "a call with no result is closed with Gemini CLI's own 'Response was lost or interrupted.' error",
                    );
                    history[i + 1]["parts"]
                        .as_array_mut()
                        .unwrap()
                        .push(json!({"functionResponse": {"id": id, "name": name, "response": {"error": INTERRUPTED}}}));
                }
            }
        }
        i += 1;
    }
    let doc = match extra.get("document") {
        Some(Value::Array(_)) => Value::Array(history),
        Some(d) => {
            let mut d = d.clone();
            d["history"] = Value::Array(history);
            d
        }
        // Before 0.16.0 a checkpoint is a bare Content[]; 0.16.0+ reads both.
        None if opts.target_version.as_deref().is_some_and(|v| version_lt(v, "0.16.0")) => Value::Array(history),
        None => json!({"history": history}),
    };
    let list_key = if doc.is_array() { None } else { Some("history") };
    // A document kept whole is written back as read only while its records
    // still say what it says.
    if all_own
        && let Some(raw) = extra.get("raw_file").and_then(Value::as_str)
        && util::same_json(raw, &serde_json::to_string(&doc).expect("json"))
    {
        return raw.as_bytes().to_vec();
    }
    if all_own {
        if let Some(s) = own_text(&extra, list_key, &texts, &doc) {
            return s.into_bytes();
        }
        losses.add(jsonfmt::EXACT_TEXT_FALLBACK, None, jsonfmt::EXACT_TEXT_FALLBACK_DETAIL);
    }
    let mut s = pretty(&doc);
    if extra.get("style").and_then(Value::as_str) == Some("pretty2+newline") {
        s.push('\n');
    }
    s.into_bytes()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn write(dir: &Path, rel: &str, text: &str) -> std::path::PathBuf {
        let p = dir.join(rel);
        std::fs::create_dir_all(p.parent().unwrap()).unwrap();
        std::fs::write(&p, text).unwrap();
        p
    }

    const JSONL: &str = concat!(
        r#"{"sessionId":"5b9c3c1e-2f0a-4c43-9a51-6f4f0e0b7a11","projectHash":"ab","startTime":"2026-01-01T00:00:00.000Z","lastUpdated":"2026-01-01T00:00:00.000Z","kind":"main"}"#,
        "\n",
        r#"{"id":"u1","timestamp":"2026-01-01T00:00:01.000Z","type":"user","content":[{"text":"hi"}]}"#,
        "\n",
        r#"{"id":"g1","timestamp":"2026-01-01T00:00:02.000Z","type":"gemini","content":"Reading.","thoughts":[{"subject":"Plan","description":"read it","timestamp":"2026-01-01T00:00:02.000Z"}],"tokens":{"input":10,"output":2,"cached":0,"thoughts":1,"tool":0,"total":13},"model":"gemini-2.5-pro"}"#,
        "\n",
        r#"{"id":"g1","timestamp":"2026-01-01T00:00:02.000Z","type":"gemini","content":"Reading.","thoughts":[{"subject":"Plan","description":"read it","timestamp":"2026-01-01T00:00:02.000Z"}],"tokens":{"input":10,"output":2,"cached":0,"thoughts":1,"tool":0,"total":13},"model":"gemini-2.5-pro","toolCalls":[{"id":"c1","name":"read_file","args":{"file_path":"a"},"result":[{"functionResponse":{"id":"c1","name":"read_file","response":{"output":"A"}}}],"status":"success","timestamp":"2026-01-01T00:00:03.000Z"}]}"#,
        "\n",
        r#"{"id":"u2","timestamp":"2026-01-01T00:00:03.000Z","type":"user","content":[{"functionResponse":{"id":"c1","name":"read_file","response":{"output":"A"}}}]}"#,
        "\n",
        r#"{"$patch":{"updates":[{"id":"g1","content":[{"text":"**Plan** read it","thought":true},{"text":"Reading."},{"functionCall":{"id":"c1","name":"read_file","args":{"file_path":"a"}}}]}]}}"#,
        "\n",
        r#"{"$set":{"summary":"Say hi"}}"#,
        "\n",
    );

    #[test]
    fn jsonl_dedups_rewritten_messages_and_self_trips() {
        let t = tempfile::tempdir().unwrap();
        let p = write(t.path(), "session-2026-01-01T00-00-5b9c3c1e.jsonl", JSONL);
        assert_eq!(GeminiCli.detect(&p).unwrap().variant, V_JSONL);
        let imp = GeminiCli.import(&p, &ImportOptions::standard()).unwrap();
        let l = imp.set.iter().next().unwrap();
        let kinds: Vec<&str> = l.events.iter().map(|e| e.kind.as_str()).collect();
        assert_eq!(kinds, ["text", "settings", "reasoning", "text", "usage", "tool_call", "tool_result", "native", "native", "title"]);
        let out = GeminiCli.export(&imp.set, &ExportOptions::standard()).unwrap();
        assert_eq!(out.output.files["session-2026-01-01T00-00-5b9c3c1e.jsonl"], JSONL.as_bytes());
    }

    #[test]
    fn foreign_ledger_renders_loadable_session_and_carries() {
        let mut l = Ledger::new(Agent {
            id: "t1".into(),
            created_at: Some("2026-01-01T00:00:00Z".into()),
            source: None,
            parent: None,
            links: vec![],
            meta: Obj::new(),
        });
        l.append(Event::new("e1", Kind::Text).with("role", "user").with("text", "hi").ts(Some("2026-01-01T00:00:01Z"))).unwrap();
        l.append(Event::new("e2", Kind::Turn).with("phase", "start")).unwrap();
        l.append(Event::new("e3", Kind::ToolCall).with("call_id", "k").with("name", "ls").with("input", json!({})).group(Some("m1"))).unwrap();
        l.append(Event::new("e4", Kind::ToolResult).with("call_id", "k").with("output", "x")).unwrap();
        l.append(Event::new("e5", Kind::Text).with("role", "assistant").with("text", "done").group(Some("m2"))).unwrap();
        let set = LedgerSet::single(l);
        let out = GeminiCli.export(&set, &ExportOptions::standard()).unwrap();
        let (name, bytes) = out.output.files.iter().next().unwrap();
        assert!(name.starts_with("session-2026-01-01T00-00-") && name.ends_with(".jsonl"));
        let text = String::from_utf8(bytes.clone()).unwrap();
        let first: Value = serde_json::from_str(text.lines().next().unwrap()).unwrap();
        assert!(first.get("sessionId").is_some() && first.get("projectHash").is_some());
        let t = tempfile::tempdir().unwrap();
        let p = write(t.path(), name, &text);
        let back = GeminiCli.import(&p, &ImportOptions::standard()).unwrap();
        let ids: Vec<&str> = back.set.iter().next().unwrap().events.iter().map(|e| e.id.as_str()).collect();
        assert_eq!(ids, ["e1", "e2", "e3", "e4", "e5"]);
        // Without carry: neutral facts only.
        let plain = GeminiCli.export(&set, &ExportOptions::no_carry()).unwrap();
        let p2 = write(t.path(), "plain.jsonl", std::str::from_utf8(plain.output.files.values().next().unwrap()).unwrap());
        let back = GeminiCli.import(&p2, &ImportOptions { variant: None, restore_carried: false }).unwrap();
        let kinds: Vec<&str> = back.set.iter().next().unwrap().events.iter().map(|e| e.kind.as_str()).collect();
        assert!(kinds.contains(&"tool_call") && kinds.contains(&"tool_result"));
    }

    #[test]
    fn pretty_json_and_checkpoint_round_trip() {
        let doc = "{\n  \"sessionId\": \"s\",\n  \"projectHash\": \"h\",\n  \"startTime\": \"2026-01-01T00:00:00.000Z\",\n  \"lastUpdated\": \"2026-01-01T00:00:00.000Z\",\n  \"messages\": [\n    {\n      \"id\": \"u\",\n      \"timestamp\": \"2026-01-01T00:00:00.000Z\",\n      \"type\": \"user\",\n      \"content\": \"hi\"\n    }\n  ],\n  \"summary\": \"x\"\n}";
        let t = tempfile::tempdir().unwrap();
        let p = write(t.path(), "session-a.json", doc);
        let imp = GeminiCli.import(&p, &ImportOptions::standard()).unwrap();
        let out = GeminiCli.export(&imp.set, &ExportOptions::standard()).unwrap();
        assert_eq!(out.output.files["session-a.json"], doc.as_bytes());
        let ck = "[\n  {\n    \"role\": \"user\",\n    \"parts\": [\n      {\n        \"text\": \"hi\"\n      }\n    ]\n  }\n]";
        let p = write(t.path(), "checkpoint-a%20b.json", ck);
        let imp = GeminiCli.import(&p, &ImportOptions::standard()).unwrap();
        assert_eq!(imp.set.iter().next().unwrap().id(), "checkpoint-a b");
        let out = GeminiCli.export(&imp.set, &ExportOptions::standard()).unwrap();
        assert_eq!(out.output.files["checkpoint-a%20b.json"], ck.as_bytes());
    }

    /// Records spelled the way a parsed `Value` does not re-spell them under
    /// any feature set (the escape `\u00e9`, `1e3`, `0.000005`, whitespace
    /// inside a line, keys out of order) are held as their own text and come
    /// back byte for byte, in all three variants.
    #[test]
    fn records_are_held_and_reemitted_as_spelled() {
        let t = tempfile::tempdir().unwrap();
        let held = |set: &LedgerSet| -> Vec<String> {
            let l = set.iter().next().unwrap();
            l.events.iter().filter_map(|e| e.native.as_ref()).filter(|n| n.part == 0).filter_map(|n| n.record_text().map(str::to_string)).collect()
        };
        // session-jsonl: every line after the metadata line is a record.
        let lines = [
            r#"{"sessionId":"5b9c3c1e-2f0a-4c43-9a51-6f4f0e0b7a11","projectHash":"ab","startTime":"2026-01-01T00:00:00.000Z","lastUpdated":"2026-01-01T00:00:00.000Z","kind":"main"}"#,
            r#"{"id":"u1", "timestamp":"2026-01-01T00:00:01.000Z","type":"user","content":[{"text":"caf\u00e9"}]}"#,
            r#"{"id":"g1","timestamp":"2026-01-01T00:00:02.000Z","type":"gemini","content":"ok","tokens":{"input":1e3,"output":2,"cached":0,"thoughts":0,"tool":0,"total":1002},"cost":0.000005,"model":"gemini-2.5-pro"}"#,
        ];
        let text = lines.join("\n") + "\n";
        let name = "session-2026-01-01T00-00-5b9c3c1e.jsonl";
        let set = GeminiCli.import(&write(t.path(), name, &text), &ImportOptions::standard()).unwrap().set;
        assert_eq!(held(&set), lines[1..]);
        // As imported, and from the records' own texts alone (no `raw`).
        for set in [set.clone(), jsonfmt::without_raw(&set)] {
            let out = GeminiCli.export(&set, &ExportOptions::standard()).unwrap();
            assert_eq!(std::str::from_utf8(&out.output.files[name]).unwrap(), text);
        }

        // session-json: one pretty document; each message is a record.
        let doc = "{\n  \"sessionId\": \"s\",\n  \"projectHash\": \"h\",\n  \"startTime\": \"2026-01-01T00:00:00.000Z\",\n  \"lastUpdated\": \"2026-01-01T00:00:00.000Z\",\n  \"messages\": [\n    {\n      \"id\": \"u\",\n      \"timestamp\": \"2026-01-01T00:00:00.000Z\",\n      \"type\": \"user\",\n      \"content\": \"caf\\u00e9\",\n      \"n\": 1e3,\n      \"x\": 0.000005\n    }\n  ]\n}\n";
        let set = GeminiCli.import(&write(t.path(), "session-b.json", doc), &ImportOptions::standard()).unwrap().set;
        assert_eq!(held(&set), [r#"{"id":"u","timestamp":"2026-01-01T00:00:00.000Z","type":"user","content":"caf\u00e9","n":1e3,"x":0.000005}"#]);
        let extra = &set.iter().next().unwrap().agent().source.as_ref().unwrap().extra;
        assert_eq!(
            extra["document_text"],
            json!(r#"{"sessionId":"s","projectHash":"h","startTime":"2026-01-01T00:00:00.000Z","lastUpdated":"2026-01-01T00:00:00.000Z","messages":[]}"#)
        );
        assert!(extra.get("raw_file").is_none(), "the layout is reproduced, not kept whole");
        // As imported, and from the records' own texts alone (no `raw`).
        for set in [set.clone(), jsonfmt::without_raw(&set)] {
            let out = GeminiCli.export(&set, &ExportOptions::standard()).unwrap();
            assert_eq!(std::str::from_utf8(&out.output.files["session-b.json"]).unwrap(), doc);
        }

        // checkpoint: a bare Content[]; each Content is a record.
        let ck = "[\n  {\n    \"role\": \"user\",\n    \"parts\": [\n      {\n        \"text\": \"caf\\u00e9\"\n      }\n    ],\n    \"n\": 1e3\n  }\n]";
        let set = GeminiCli.import(&write(t.path(), "checkpoint-odd.json", ck), &ImportOptions::standard()).unwrap().set;
        assert_eq!(held(&set), [r#"{"role":"user","parts":[{"text":"caf\u00e9"}],"n":1e3}"#]);
        // As imported, and from the records' own texts alone (no `raw`).
        for set in [set.clone(), jsonfmt::without_raw(&set)] {
            let out = GeminiCli.export(&set, &ExportOptions::standard()).unwrap();
            assert_eq!(std::str::from_utf8(&out.output.files["checkpoint-odd.json"]).unwrap(), ck);
        }
    }

    /// An unchanged record whose `raw` is a spelling within
    /// `util::NUMBER_ULPS` of its record (so `current_raw` writes it) cannot be
    /// laid back into the document it came from: the rebuilt text no longer
    /// parses to the document's value, the exporter falls back to the value,
    /// and that is named.
    #[test]
    fn an_unchanged_document_not_rebuilt_from_its_texts_names_the_fallback() {
        let t = tempfile::tempdir().unwrap();
        let has = |ex: &Exported| ex.losses.0.iter().any(|l| l.code == jsonfmt::EXACT_TEXT_FALLBACK);
        let doc = "{\n  \"sessionId\": \"s\",\n  \"projectHash\": \"h\",\n  \"startTime\": \"2026-01-01T00:00:00.000Z\",\n  \"lastUpdated\": \"2026-01-01T00:00:00.000Z\",\n  \"messages\": [\n    {\n      \"id\": \"u\",\n      \"timestamp\": \"2026-01-01T00:00:00.000Z\",\n      \"type\": \"user\",\n      \"content\": \"hi\",\n      \"x\": 0.000005\n    }\n  ]\n}\n";
        let ck = "[\n  {\n    \"role\": \"user\",\n    \"parts\": [\n      {\n        \"text\": \"hi\"\n      }\n    ],\n    \"x\": 0.000005\n  }\n]";
        for (name, text) in [("session-b.json", doc), ("checkpoint-odd.json", ck)] {
            let (from, to) = ("0.000005", "5.000000000000001e-06");
            let set = GeminiCli.import(&write(t.path(), name, text), &ImportOptions::standard()).unwrap().set;
            // Opposed: as imported, it is rebuilt from its texts and names nothing.
            let ex = GeminiCli.export(&set, &ExportOptions::standard()).unwrap();
            assert_eq!(std::str::from_utf8(&ex.output.files[name]).unwrap(), text);
            assert!(!has(&ex), "{name}: {:?}", ex.losses);
            let ex = GeminiCli.export(&nudged(&set, from, to), &ExportOptions::standard()).unwrap();
            assert!(has(&ex), "{name}: the fallback is named: {:?}", ex.losses);
        }
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

    /// The `raw` rule for a document kept whole (no layout reproduces it): its `raw`
    /// is written back only while its records still say what it says.
    #[test]
    fn a_document_kept_whole_is_written_back_only_while_its_records_say_it() {
        let t = tempfile::tempdir().unwrap();
        let doc = "{\n    \"sessionId\": \"s\",\n    \"projectHash\": \"h\",\n    \"startTime\": \"2026-01-01T00:00:00.000Z\",\n    \"lastUpdated\": \"2026-01-01T00:00:00.000Z\",\n    \"messages\": [\n        {\n            \"id\": \"u\",\n            \"timestamp\": \"2026-01-01T00:00:00.000Z\",\n            \"type\": \"user\",\n            \"content\": \"hello there\"\n        }\n    ]\n}";
        let ck = "[\n    {\n        \"role\": \"user\",\n        \"parts\": [\n            {\n                \"text\": \"hello there\"\n            }\n        ]\n    }\n]";
        for (name, text) in [("session-w.json", doc), ("checkpoint-w.json", ck)] {
            let set = GeminiCli.import(&write(t.path(), name, text), &ImportOptions::standard()).unwrap().set;
            let extra = &set.iter().next().unwrap().agent().source.as_ref().unwrap().extra;
            assert!(extra.get("raw_file").is_some(), "{name}: sentinel: kept whole");
            // Opposed: unchanged, the file as read.
            let out = GeminiCli.export(&set, &ExportOptions::standard()).unwrap();
            assert_eq!(std::str::from_utf8(&out.output.files[name]).unwrap(), text, "{name}");
            // Edited (the record's text), the stale whole file is not written.
            let mut edited = set.clone();
            let id = edited.iter().next().unwrap().id().to_string();
            let n = edited
                .ledgers
                .get_mut(&id)
                .unwrap()
                .events
                .iter_mut()
                .filter_map(|e| e.native.as_mut())
                .find(|n| n.part == 0 && n.record_text().is_some_and(|t| t.contains("hello there")))
                .expect("the record");
            let r = n.record_text().unwrap().replace("hello there", "EDITED");
            *n = n.clone().record_raw(&r).unwrap();
            let out = GeminiCli.export(&edited, &ExportOptions::standard()).unwrap();
            let got = std::str::from_utf8(&out.output.files[name]).unwrap();
            assert!(got.contains("EDITED") && !got.contains("hello there"), "{name}: the edit is written: {got}");
        }
    }

    /// SPEC §3.1.5: a thought imports as its whole text (`**subject**
    /// description`, as Gemini CLI shows it) and is written back split.
    #[test]
    fn a_thought_is_its_text_and_is_written_back_with_its_subject() {
        let t = tempfile::tempdir().unwrap();
        let line = r#"{"id":"g1","timestamp":"2026-01-01T00:00:02.000Z","type":"gemini","content":"ok","thoughts":[{"subject":"Plan","description":"read it","timestamp":"2026-01-01T00:00:02.000Z"}],"model":"m"}"#;
        let text = format!(
            "{}\n{line}\n",
            r#"{"sessionId":"5b9c3c1e-2f0a-4c43-9a51-6f4f0e0b7a11","projectHash":"ab","startTime":"2026-01-01T00:00:00.000Z","lastUpdated":"2026-01-01T00:00:00.000Z","kind":"main"}"#
        );
        let set = GeminiCli.import(&write(t.path(), "session-2026-01-01T00-00-5b9c3c1e.jsonl", &text), &ImportOptions::standard()).unwrap().set;
        let r = set.iter().next().unwrap().events.iter().find(|e| e.kind == Kind::Reasoning).expect("the thought");
        assert_eq!(r.str("text"), Some("**Plan** read it"));
        assert!(r.body.get("summary").is_none(), "the subject is not a summary");
        // Written back (as a foreign event) with its subject split off again.
        let mut l = Ledger::new(Agent {
            id: "f".into(),
            created_at: Some("2026-01-01T00:00:00Z".into()),
            source: None,
            parent: None,
            links: vec![],
            meta: Default::default(),
        });
        for (id, t) in [("a", "**Plan** read it"), ("b", "no subject here")] {
            l.append(Event::new(id, Kind::Reasoning).with("text", t).group(Some("g".to_string()))).unwrap();
        }
        l.append(Event::new("c", Kind::Text).with("role", "assistant").with("text", "ok").group(Some("g".to_string()))).unwrap();
        let out = GeminiCli.export(&LedgerSet::single(l), &ExportOptions::no_carry()).unwrap();
        let body: String = out.output.files.values().map(|b| String::from_utf8_lossy(b).to_string()).collect();
        let thoughts: Vec<(String, String)> = body
            .lines()
            .filter_map(|l| serde_json::from_str::<Value>(l).ok())
            .flat_map(|v| v.get("thoughts").and_then(Value::as_array).cloned().unwrap_or_default())
            .map(|t| (t["subject"].as_str().unwrap_or("").to_string(), t["description"].as_str().unwrap_or("").to_string()))
            .collect();
        // Opposed: a thought with no subject keeps its whole text.
        assert_eq!(thoughts, [("Plan".to_string(), "read it".to_string()), (String::new(), "no subject here".to_string())], "{body}");
    }

    /// SPEC §3.1.2: a custom tool's `input_text` reaches Gemini CLI's
    /// object `args` wrapped and named, in sessions and checkpoints alike
    /// (it was written as `{}`, silently).
    #[test]
    fn a_custom_tool_input_text_is_wrapped_not_dropped() {
        let mk = |body: Value| {
            let mut l = Ledger::new(Agent {
                id: "c".into(),
                created_at: Some("2026-01-01T00:00:00Z".into()),
                source: None,
                parent: None,
                links: vec![],
                meta: Default::default(),
            });
            l.append(Event::new("u", Kind::Text).with("role", "user").with("text", "go").ts(Some("2026-01-01T00:00:01Z"))).unwrap();
            let mut c = Event::new("c1", Kind::ToolCall).ts(Some("2026-01-01T00:00:02Z"));
            c.body = body.as_object().unwrap().clone().into_iter().collect();
            l.append(c).unwrap();
            LedgerSet::single(l)
        };
        for v in [V_JSONL, V_JSON, V_CKPT] {
            let opts = ExportOptions { variant: Some(v.into()), carry: false, target_version: None };
            let ex =
                GeminiCli.export(&mk(json!({"call_id": "k1", "name": "apply_patch", "input_text": "*** PATCH ***", "tool_type": "custom"})), &opts).unwrap();
            let text: String = ex.output.files.values().map(|b| String::from_utf8_lossy(b).to_string()).collect();
            assert!(text.contains("*** PATCH ***"), "{v}: the text is written: {text}");
            assert!(ex.losses.0.iter().any(|l| l.code == "tool-input-text-wrapped"), "{v}: and named: {:?}", ex.losses);
            // Opposed: an object input is written as it is, naming nothing.
            let ex = GeminiCli.export(&mk(json!({"call_id": "k1", "name": "read", "input": {"path": "a"}})), &opts).unwrap();
            assert!(!ex.losses.0.iter().any(|l| l.code.starts_with("tool-input")), "{v}: {:?}", ex.losses);
        }
    }

    #[test]
    fn encode_tag_like_js() {
        assert_eq!(percent_encode("demo tag/1"), "demo%20tag%2F1");
        assert_eq!(percent_decode("demo%20tag%2F1"), "demo tag/1");
    }
}

#[cfg(test)]
mod cross_tests {
    use super::*;
    use crate::loss::{CompareOptions, compare};
    use crate::roundtrip::materialise;

    /// A foreign ledger exercising most kinds.
    pub(crate) fn foreign() -> LedgerSet {
        let text = include_str!("../../fixtures/gemini-cli/_generators/foreign-ledger.jsonl");
        LedgerSet::single(Ledger::from_jsonl(text).unwrap().0)
    }

    fn trip(variant: &str, carry: bool) -> crate::loss::Diff {
        let p0 = foreign();
        let eo = ExportOptions { variant: Some(variant.into()), carry, target_version: None };
        let out = GeminiCli.export(&p0, &eo).unwrap();
        let t = tempfile::tempdir().unwrap();
        let path = materialise(&out.output, t.path()).unwrap();
        let p1 = GeminiCli.import(&path, &ImportOptions { variant: None, restore_carried: carry }).unwrap().set;
        compare(&p0, &p1, CompareOptions { ignore_native: true, ..Default::default() })
    }

    #[test]
    fn carry_comes_back_exact() {
        for v in [V_JSONL, V_JSON] {
            let d = trip(v, true);
            assert!(d.is_clean(), "{v}: {}", d.summary());
        }
    }

    /// A foreign native record spelled as no `Value` spells it: `1.50` and
    /// `0.000005` (serde writes `1.5` and `5e-6`).
    const ODD_RECORD: &str = r#"{"type":"assistant","cost":1.50,"tiny":0.000005}"#;
    /// `ODD_RECORD`'s numbers survive a `Value` under serde_json's
    /// `arbitrary_precision` (on in the removed `legacy-json` feature), which
    /// made the test blind there.
    /// An escape and inner whitespace survive a `Value` under no feature.
    const ODD_RECORD_ANY_BUILD: &str = r#"{"type":"assistant", "name":"caf\u00e9","cost":1.50}"#;

    #[test]
    fn carried_native_records_restore_their_source_text() {
        for odd_record in [ODD_RECORD, ODD_RECORD_ANY_BUILD] {
            carried_record_restores(odd_record);
        }
        // Sentinel: in every build a `Value` re-spells at least one of them,
        // so only its carried text can bring it back.
        assert_ne!(serde_json::to_string(&serde_json::from_str::<Value>(ODD_RECORD_ANY_BUILD).unwrap()).unwrap(), ODD_RECORD_ANY_BUILD);
    }

    fn carried_record_restores(odd_record: &'static str) {
        let odd = |id: &str, kind: Kind| {
            Event::new(id, kind).ts(Some("2026-05-01T10:00:01Z")).native(Native::new("claude-code", Some(id.into())).record_raw(odd_record).unwrap())
        };
        // On a message (carry::attach), and with no message at all: on the
        // session's metadata line or document (attach_with_agent).
        let mut on_msg = foreign();
        let first = on_msg.iter().next().unwrap().id().to_string();
        on_msg.ledgers.get_mut(&first).unwrap().append(odd("n0", Kind::Native)).unwrap();
        let mut lone = Ledger::new(Agent {
            id: "lone".into(),
            created_at: Some("2026-05-01T10:00:00Z".into()),
            source: None,
            parent: None,
            links: vec![],
            meta: Default::default(),
        });
        for id in ["n1", "n2"] {
            lone.append(odd(id, Kind::Native)).unwrap();
        }
        let lone = LedgerSet::single(lone);
        for v in [V_JSONL, V_JSON] {
            for (set, agent, ids) in [(&on_msg, first.as_str(), &["n0"][..]), (&lone, "lone", &["n1", "n2"][..])] {
                let out = GeminiCli.export(set, &ExportOptions { variant: Some(v.into()), carry: true, target_version: None }).unwrap();
                let t = tempfile::tempdir().unwrap();
                let path = materialise(&out.output, t.path()).unwrap();
                let back = GeminiCli.import(&path, &ImportOptions { variant: None, restore_carried: true }).unwrap().set;
                let l = back.get(agent).unwrap_or_else(|| panic!("{v}: {agent} came back"));
                for id in ids {
                    let text = l.events.iter().find(|e| e.id == *id).and_then(|e| e.native.as_ref()).and_then(|n| n.record_text());
                    // Compared with the source's bytes, not with a re-serialised `Value`.
                    assert_eq!(text, Some(odd_record), "{v}: {agent}/{id}");
                }
            }
        }
    }

    #[test]
    fn plain_trip_loses_only_what_gemini_cannot_say() {
        for v in [V_JSONL, V_JSON, V_CKPT] {
            let d = trip(v, false);
            eprintln!("{v}: {}", d.summary());
            // A checkpoint has no place for system text.
            let text_lost = if v == V_CKPT { 1 } else { 0 };
            assert!(d.missing.get("text").copied().unwrap_or(0) <= text_lost, "{v}: {}", d.summary());
            assert!(!d.missing.contains_key("tool_call") && !d.missing.contains_key("tool_result"), "{v}: {}", d.summary());
        }
    }
}
