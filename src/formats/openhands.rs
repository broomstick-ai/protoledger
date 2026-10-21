//! OpenHands conversations.
//!
//! Two generations, both imported, see `docs/formats/openhands.md`:
//!
//! - **V0** (legacy OpenHands ≤ 1.0 event stream): `sessions/<sid>/events/<n>.json`,
//!   one Python `json.dumps(event_to_dict(e))` per file, actions and
//!   observations tagged by `action` / `observation`; also the trajectory
//!   exports (`save_trajectory_path`, `/api/conversations/{id}/trajectory`) and
//!   the events API page, which hold the same dicts.
//! - **V1** (`software-agent-sdk`, OpenHands ≥ 1.0, CLI, Agent Canvas):
//!   `<conversation hex>/base_state.json` + `events/event-<idx:05d>-<uuid>.json`,
//!   pydantic `model_dump_json(exclude_none=True)`, events tagged by `kind`;
//!   also the agent-server events API page (`{"items": [...]}`).
//!
//! Export writes V1 (what OpenHands loads today); V0 ledgers go back to V0.

use super::pi_session::{peer_wrap, text_or_peer};
use crate::carry;
use crate::formats::atif::jsonfmt::{self, Style};
use crate::formats::openclaw::jsonl;
use crate::formats::{Detection, ExportOptions, Exported, Format, ImportOptions, Imported, Losses, Output, Variant};
use crate::ledger::{Ledger, LedgerSet};
use crate::model::{Agent, Event, Kind, Native, Obj, Source};
use crate::util;
use crate::util::OrderedMap;
use anyhow::{Context, Result, bail};
use base64::Engine;
use serde_json::{Map, Value, json};
use std::collections::{HashMap, HashSet};
use std::path::{Path, PathBuf};

pub struct OpenHands;

const FORMAT: &str = "openhands";

const VARIANTS: &[Variant] = &[
    Variant {
        name: "v0-events",
        versions: "OpenHands <= 1.0 (V0 conversations; removed from OpenHands after 1.0)",
        import: true,
        export: true,
        notes: "sessions/<sid>/events/<n>.json + metadata.json; exported back only for ledgers imported from it",
    },
    Variant {
        name: "v0-trajectory",
        versions: "OpenHands <= 1.0",
        import: true,
        export: true,
        notes: "a JSON file of V0 event dicts: save_trajectory_path array, {\"trajectory\": [...]} or the events API {\"events\": [...], \"has_more\"}; exported back only for ledgers imported from it",
    },
    Variant {
        name: "v1-events",
        versions: "software-agent-sdk >= 1.0.0a1 (OpenHands >= 1.0, current 1.55)",
        import: true,
        export: true,
        notes: "<conversation hex>/base_state.json + events/event-<idx:05d>-<uuid>.json; parent_id from SDK 1.31",
    },
    Variant {
        name: "v1-api",
        versions: "software-agent-sdk agent server",
        import: true,
        export: true,
        notes: "a JSON file of V1 events: a bare array or the events search page {\"items\": [...], \"next_page_id\"}; also JSONL",
    },
];

impl Format for OpenHands {
    fn name(&self) -> &'static str {
        FORMAT
    }
    fn describe(&self) -> &'static str {
        "OpenHands conversations: V1 agent-sdk event dirs, V0 event streams and trajectories"
    }
    fn variants(&self) -> &'static [Variant] {
        VARIANTS
    }
    fn detect(&self, input: &Path) -> Option<Detection> {
        if input.is_dir() {
            let convs = conversation_dirs(input);
            let (dir, generation) = convs.first()?;
            let version = if *generation == Gen::V0 { v0_version_in_dir(dir) } else { None };
            return Some(Detection { variant: if *generation == Gen::V0 { "v0-events" } else { "v1-events" }, confidence: 85, version });
        }
        let text = std::fs::read_to_string(input).ok()?;
        let (items, _) = parse_file_items(&text)?;
        let generation = gen_of_items(&items)?;
        let version = items.iter().find_map(v0_version);
        Some(Detection { variant: if generation == Gen::V0 { "v0-trajectory" } else { "v1-api" }, confidence: 80, version })
    }
    fn import(&self, input: &Path, opts: &ImportOptions) -> Result<Imported> {
        import(input, opts)
    }
    fn export(&self, set: &LedgerSet, opts: &ExportOptions) -> Result<Exported> {
        export(set, opts)
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Gen {
    V0,
    V1,
}

// ---------------------------------------------------------------------------
// Python json.dumps, for V0 files
// ---------------------------------------------------------------------------

/// Python's `json.dumps(v)` (`indent=None`: separators `", "` / `": "`) or
/// `json.dumps(v, indent=n)`, with `ensure_ascii=True`.
pub(crate) fn py_dumps(v: &Value, indent: Option<usize>) -> String {
    let mut out = String::new();
    py_write(v, indent, 0, &mut out);
    out
}

fn py_write(v: &Value, indent: Option<usize>, depth: usize, out: &mut String) {
    let nl = |out: &mut String, d: usize| {
        if let Some(n) = indent {
            out.push('\n');
            out.push_str(&" ".repeat(n * d));
        }
    };
    match v {
        Value::Null => out.push_str("null"),
        Value::Bool(b) => out.push_str(if *b { "true" } else { "false" }),
        Value::Number(n) => out.push_str(&n.to_string()),
        Value::String(s) => py_str(s, out),
        Value::Array(a) => {
            if a.is_empty() {
                out.push_str("[]");
                return;
            }
            out.push('[');
            for (i, x) in a.iter().enumerate() {
                if i > 0 {
                    out.push(',');
                    if indent.is_none() {
                        out.push(' ');
                    }
                }
                nl(out, depth + 1);
                py_write(x, indent, depth + 1, out);
            }
            nl(out, depth);
            out.push(']');
        }
        Value::Object(m) => {
            if m.is_empty() {
                out.push_str("{}");
                return;
            }
            out.push('{');
            for (i, (k, x)) in m.iter().enumerate() {
                if i > 0 {
                    out.push(',');
                    if indent.is_none() {
                        out.push(' ');
                    }
                }
                nl(out, depth + 1);
                py_str(k, out);
                out.push_str(": ");
                py_write(x, indent, depth + 1, out);
            }
            nl(out, depth);
            out.push('}');
        }
    }
}

fn py_str(s: &str, out: &mut String) {
    out.push('"');
    for c in s.chars() {
        match c {
            '"' => out.push_str("\\\""),
            '\\' => out.push_str("\\\\"),
            '\n' => out.push_str("\\n"),
            '\r' => out.push_str("\\r"),
            '\t' => out.push_str("\\t"),
            '\u{8}' => out.push_str("\\b"),
            '\u{c}' => out.push_str("\\f"),
            c if (c as u32) < 0x20 || (c as u32) > 0x7e && (c as u32) != 0x7f => {
                let mut buf = [0u16; 2];
                for u in c.encode_utf16(&mut buf) {
                    out.push_str(&format!("\\u{:04x}", u));
                }
            }
            c => out.push(c),
        }
    }
    out.push('"');
}

// ---------------------------------------------------------------------------
// Locating conversations
// ---------------------------------------------------------------------------

fn is_v1_event_name(n: &str) -> bool {
    n.starts_with("event-") && n.ends_with(".json")
}

fn v1_idx(n: &str) -> Option<u64> {
    n.strip_prefix("event-")?.split('-').next()?.parse().ok()
}

fn v0_idx(n: &str) -> Option<u64> {
    n.strip_suffix(".json")?.parse().ok()
}

/// Conversation directories under `root`: V1 (`base_state.json` or
/// `events/event-*.json`) and V0 (`events/<n>.json`).
fn conversation_dirs(root: &Path) -> Vec<(PathBuf, Gen)> {
    let mut out = Vec::new();
    for e in walkdir::WalkDir::new(root).sort_by_file_name().into_iter().filter_map(|e| e.ok()) {
        if !e.file_type().is_dir() {
            continue;
        }
        let d = e.path();
        let events = d.join("events");
        let names: Vec<String> =
            std::fs::read_dir(&events).map(|rd| rd.filter_map(|x| x.ok()).map(|x| x.file_name().to_string_lossy().to_string()).collect()).unwrap_or_default();
        if d.join("base_state.json").is_file() || names.iter().any(|n| is_v1_event_name(n)) {
            out.push((d.to_path_buf(), Gen::V1));
        } else if names.iter().any(|n| v0_idx(n).is_some()) {
            out.push((d.to_path_buf(), Gen::V0));
        }
    }
    out
}

fn v0_version(v: &Value) -> Option<String> {
    (v.get("action").and_then(Value::as_str) == Some("system")).then(|| util::get_str(v, &["args", "openhands_version"]).map(String::from)).flatten()
}

fn v0_version_in_dir(dir: &Path) -> Option<String> {
    let t = std::fs::read_to_string(dir.join("events/0.json")).ok()?;
    v0_version(&serde_json::from_str(&t).ok()?)
}

/// The events in a JSON file: an array, an `{items|events|trajectory: [...]}`
/// envelope, or JSONL. Returns the items and the envelope key.
fn parse_file_items(text: &str) -> Option<(Vec<Value>, Option<String>)> {
    match serde_json::from_str::<Value>(text) {
        Ok(Value::Array(a)) => Some((a, None)),
        Ok(Value::Object(m)) => {
            for k in ["items", "events", "trajectory"] {
                if let Some(Value::Array(a)) = m.get(k) {
                    return Some((a.clone(), Some(k.to_string())));
                }
            }
            None
        }
        _ => {
            let mut items = Vec::new();
            for line in text.lines().filter(|l| !l.trim().is_empty()) {
                let v: Value = serde_json::from_str(line).ok()?;
                v.is_object().then_some(())?;
                items.push(v);
            }
            (!items.is_empty()).then_some((items, Some("jsonl".into())))
        }
    }
}

/// A whole-document file's compact text with its event list emptied (the
/// skeleton), and each event's own text, every token as written.
fn doc_items(text: &str, key: Option<&str>) -> Option<(String, Vec<String>)> {
    let compact = jsonfmt::minify(text);
    let (skeleton, list) = match key {
        None => ("[]".to_string(), compact.as_str()),
        Some(k) => (jsonfmt::with_member(&compact, k, "[]")?, jsonfmt::member(&compact, k)?),
    };
    Some((skeleton, jsonfmt::items(list)?.into_iter().map(String::from).collect()))
}

fn gen_of_items(items: &[Value]) -> Option<Gen> {
    let mut v0 = 0;
    let mut v1 = 0;
    for it in items {
        if it.get("action").is_some_and(Value::is_string) || it.get("observation").is_some_and(Value::is_string) {
            v0 += 1;
        } else if it.get("kind").is_some_and(Value::is_string) {
            v1 += 1;
        }
    }
    if v0 == 0 && v1 == 0 {
        None
    } else if v0 >= v1 {
        Some(Gen::V0)
    } else {
        Some(Gen::V1)
    }
}

// ---------------------------------------------------------------------------
// Import
// ---------------------------------------------------------------------------

fn import(input: &Path, opts: &ImportOptions) -> Result<Imported> {
    let mut set = LedgerSet::new();
    let mut losses = Losses::default();
    let mut variant = None;
    if input.is_dir() {
        let convs = conversation_dirs(input);
        if convs.is_empty() {
            bail!("{}: no OpenHands conversation directories", input.display());
        }
        for (dir, generation) in convs {
            let rel = dir.strip_prefix(input).unwrap_or(&dir).to_string_lossy().replace('\\', "/");
            let l = import_dir(&dir, &rel, generation, opts, &mut losses).with_context(|| format!("openhands: {}", dir.display()))?;
            variant.get_or_insert(if generation == Gen::V0 { "v0-events" } else { "v1-events" });
            set.insert(l);
        }
    } else {
        let text = std::fs::read_to_string(input).with_context(|| format!("reading {}", input.display()))?;
        let name = input.file_name().unwrap().to_string_lossy().to_string();
        let (l, v) = import_file(&text, &name, opts, &mut losses)?;
        variant = Some(v);
        set.insert(l);
    }
    Ok(Imported { set, losses, variant: variant.unwrap_or("v1-events").to_string() })
}

/// Text of a non-event file, or base64 when it is not UTF-8.
fn side_file(p: &Path) -> Option<(bool, String)> {
    let bytes = std::fs::read(p).ok()?;
    match String::from_utf8(bytes) {
        Ok(s) => Some((true, s)),
        Err(e) => Some((false, base64::engine::general_purpose::STANDARD.encode(e.into_bytes()))),
    }
}

fn import_dir(dir: &Path, rel: &str, generation: Gen, opts: &ImportOptions, losses: &mut Losses) -> Result<Ledger> {
    let conv_id = dir.file_name().unwrap().to_string_lossy().to_string();
    let prefix = if rel.is_empty() { String::new() } else { format!("{rel}/") };
    // Event files in order; everything else is kept verbatim.
    let mut events: Vec<(u64, String, String)> = Vec::new(); // (idx, rel-in-conv, text)
    let mut files = Map::new();
    let mut files_b64 = Map::new();
    let mut base_state: Option<String> = None;
    let mut metadata: Option<String> = None;
    for e in walkdir::WalkDir::new(dir).sort_by_file_name().into_iter().filter_map(|e| e.ok()) {
        if !e.file_type().is_file() {
            continue;
        }
        let r = e.path().strip_prefix(dir).unwrap().to_string_lossy().replace('\\', "/");
        let name = e.file_name().to_string_lossy().to_string();
        let in_events = r == format!("events/{name}");
        let idx = match generation {
            Gen::V1 if in_events && is_v1_event_name(&name) => v1_idx(&name),
            Gen::V0 if in_events => v0_idx(&name),
            _ => None,
        };
        if let Some(i) = idx {
            events.push((i, r, std::fs::read_to_string(e.path())?));
        } else if r == "base_state.json" && generation == Gen::V1 {
            base_state = std::fs::read_to_string(e.path()).ok();
        } else if r == "metadata.json" && generation == Gen::V0 {
            metadata = std::fs::read_to_string(e.path()).ok();
        } else if let Some((utf8, s)) = side_file(e.path()) {
            if utf8 {
                files.insert(r, json!(s))
            } else {
                files_b64.insert(r, json!(s))
            };
        }
    }
    events.sort_by_key(|x| x.0);
    let variant = if generation == Gen::V0 { "v0-events" } else { "v1-events" };
    let mut extra = Obj::new();
    extra.insert("dir".into(), json!(rel));
    if !files.is_empty() {
        extra.insert("files".into(), Value::Object(files));
    }
    if !files_b64.is_empty() {
        extra.insert("files_b64".into(), Value::Object(files_b64));
    }
    let mut records: Vec<(Value, Option<String>, String)> = Vec::new(); // (record, source text, file)
    let mut sidecar: Option<(Value, Option<String>, String)> = None;
    for (name, text) in [("base_state.json", &base_state), ("metadata.json", &metadata)] {
        if let Some(t) = text {
            match serde_json::from_str::<Value>(t) {
                Ok(v) => {
                    sidecar = Some((v, Some(t.clone()), format!("{prefix}{name}")));
                }
                Err(_) => {
                    extra.entry("files").or_insert_with(|| json!({}))[name] = json!(t);
                }
            }
        }
    }
    for (_, r, text) in events {
        match serde_json::from_str::<Value>(&text) {
            Ok(v) if v.is_object() => {
                records.push((v, Some(text), format!("{prefix}{r}")));
            }
            _ => {
                losses.add("malformed-event-file", None, "an event file that is not a JSON object; kept raw");
                records.push((Value::Null, Some(text), format!("{prefix}{r}")));
            }
        }
    }
    let version = records.iter().find_map(|r| v0_version(&r.0));
    let mut agent = Agent {
        id: conv_id.clone(),
        created_at: None,
        source: Some(Source { format: FORMAT.into(), variant: Some(variant.into()), version: version.clone(), native_id: Some(conv_id), extra }),
        parent: None,
        links: vec![],
        meta: Obj::new(),
    };
    let mut evs = Vec::new();
    // A conversation protoledger rendered from a foreign ledger carries the
    // ledger's agent (and events no event file could hold) in base_state.json,
    // which is then derived, not history.
    let mut pre = None;
    if opts.restore_carried
        && let Some((v, _, _)) = sidecar.as_mut()
    {
        pre = take_carrier_at(v, None);
    }
    let derived = pre.as_ref().is_some_and(|p| p.1.is_some());
    let (body, override_agent) = map_records(generation, records, variant, version.as_deref(), opts, losses, pre);
    if let (Some((v, text, file)), false) = (sidecar, derived) {
        evs.extend(sidecar_events(generation, &v, text.as_deref(), &file, version.as_deref()));
    }
    evs.extend(body);
    if let Some(a) = override_agent {
        agent = a;
    }
    let mut l = Ledger::new(agent);
    for e in evs {
        l.append_unique(e);
    }
    if l.agent().created_at.is_none() {
        let first = l.events.iter().find_map(|e| e.ts.clone());
        l.agent_mut().created_at = first;
    }
    Ok(l)
}

fn import_file(text: &str, name: &str, opts: &ImportOptions, losses: &mut Losses) -> Result<(Ledger, &'static str)> {
    let (items, key) = parse_file_items(text).ok_or_else(|| anyhow::anyhow!("{name}: not an OpenHands event list"))?;
    let generation = gen_of_items(&items).unwrap_or(Gen::V1);
    let variant = if generation == Gen::V0 { "v0-trajectory" } else { "v1-api" };
    let mut extra = Obj::new();
    extra.insert("file".into(), json!(name));
    if let Some(k) = &key {
        extra.insert("envelope".into(), json!(k));
    }
    // The document around its events (the envelope's other keys, `has_more`,
    // `next_page_id`, as written) and its layout, so it is rebuilt from the
    // records' own texts; a layout no style reproduces keeps the file whole.
    if key.as_deref() != Some("jsonl") {
        let whole: Value = serde_json::from_str(text)?;
        let skeleton = doc_items(text, key.as_deref()).map(|(sk, _)| sk);
        if let Some(sk) = &skeleton {
            extra.insert("skeleton".into(), json!(sk));
        }
        match Style::detect(text, &whole).filter(|_| skeleton.is_some()) {
            Some(st) => {
                extra.insert("layout".into(), st.to_value());
            }
            None => {
                extra.insert("raw".into(), json!(text));
            }
        }
    } else if !text.ends_with('\n') {
        extra.insert("eof_newline".into(), json!(false));
    }
    let stem = name.trim_end_matches(".json").trim_end_matches(".jsonl").to_string();
    let version = items.iter().find_map(v0_version);
    let mut agent = Agent {
        id: stem.clone(),
        created_at: None,
        source: Some(Source { format: FORMAT.into(), variant: Some(variant.into()), version: version.clone(), native_id: Some(stem), extra }),
        parent: None,
        links: vec![],
        meta: Obj::new(),
    };
    // Each item's own text: a JSONL line, or the item cut out of the document's text.
    let texts: Vec<String> = if key.as_deref() == Some("jsonl") {
        text.lines().filter(|l| !l.trim().is_empty()).map(String::from).collect()
    } else {
        doc_items(text, key.as_deref()).map(|(_, items)| items).unwrap_or_default()
    };
    let n_items = items.len();
    let records: Vec<(Value, Option<String>, String)> =
        items.into_iter().enumerate().map(|(i, v)| (v, texts.get(i).filter(|_| texts.len() == n_items).cloned(), String::new())).collect();
    let (evs, override_agent) = map_records(generation, records, variant, version.as_deref(), opts, losses, None);
    if let Some(a) = override_agent {
        agent = a;
    }
    let mut l = Ledger::new(agent);
    for e in evs {
        l.append_unique(e);
    }
    if l.agent().created_at.is_none() {
        let first = l.events.iter().find_map(|e| e.ts.clone());
        l.agent_mut().created_at = first;
    }
    Ok((l, variant))
}

/// `base_state.json` (V1) or `metadata.json` (V0) as events.
fn sidecar_events(generation: Gen, v: &Value, text: Option<&str>, file: &str, version: Option<&str>) -> Vec<Event> {
    let s = |p: &[&str]| util::get_str(v, p).map(|x| json!(x)).unwrap_or(Value::Null);
    let (id, mut evs) = match generation {
        Gen::V1 => {
            let mut evs = vec![Event::new("", Kind::Settings).with("model", s(&["agent", "llm", "model"])).with("cwd", s(&["workspace", "working_dir"]))];
            if let Some(u) = first_metrics(v, text, &["stats", "usage_to_metrics"]) {
                evs.push(metrics_usage(u));
            }
            ("base_state", evs)
        }
        Gen::V0 => {
            let mut evs = vec![Event::new("", Kind::Settings).with("model", s(&["llm_model"]))];
            if let Some(t) = v.get("title").and_then(Value::as_str) {
                evs.push(Event::new("", Kind::Title).with("text", t));
            }
            if v.get("accumulated_cost").is_some() || v.get("prompt_tokens").is_some() {
                evs.push(
                    Event::new("", Kind::Usage)
                        .with("model", s(&["llm_model"]))
                        .with("input", v.get("prompt_tokens").cloned().unwrap_or(Value::Null))
                        .with("output", v.get("completion_tokens").cloned().unwrap_or(Value::Null))
                        .with("cost_usd", v.get("accumulated_cost").cloned().unwrap_or(Value::Null))
                        .with("cumulative", true),
                );
            }
            ("metadata", evs)
        }
    };
    let ts = match generation {
        Gen::V0 => v.get("created_at").and_then(Value::as_str).map(utc_ts),
        Gen::V1 => None,
    };
    let mut n =
        Native::new(FORMAT, Some(id.into())).variant(Some(if generation == Gen::V0 { "v0-events" } else { "v1-events" })).version(version).file(Some(file));
    hold_text(&mut n, text, v);
    let count = evs.len();
    for (i, e) in evs.iter_mut().enumerate() {
        e.id = if i == 0 { id.to_string() } else { format!("{id}#{i}") };
        e.ts = ts.clone();
        e.native = Some(if i == 0 { n.clone() } else { n.part_of(i as u32) });
    }
    let _ = count;
    evs
}

/// Hold a native record as the text it was read from (SPEC §4): a one-line
/// text as itself, a text spanning lines (a pretty-printed file) as its
/// tokens with the whitespace between them removed, the file kept as `raw`.
fn hold_text(n: &mut Native, text: Option<&str>, v: &Value) {
    match text {
        Some(t) if !t.contains(['\n', '\r']) => jsonfmt::hold(n, Some(t), v),
        Some(t) => {
            if !jsonl::hold_file(n, t) {
                n.set_record(Some(v.clone()));
                n.raw = Some(t.to_string());
            }
        }
        None => n.set_record(Some(v.clone())),
    }
}

/// The first metrics snapshot in the object at `path` (keyed by usage id: the
/// agent's LLM, a condenser's, ...), first in the order the source wrote them.
/// protoledger builds serde_json without `preserve_order`, so a parsed `Value`
/// sorts its keys (`"condenser"` before `"default"`) and the order is read off
/// the record's text.
fn first_metrics<'a>(v: &'a Value, text: Option<&str>, path: &[&str]) -> Option<&'a Value> {
    let m = util::get(v, path)?.as_object()?;
    let first_key = text.and_then(|t| {
        let compact = jsonfmt::minify(t);
        let mut cur: &str = &compact;
        for k in path {
            cur = jsonfmt::member(cur, k)?;
        }
        jsonfmt::members(cur)?.into_iter().next().map(|(k, _)| k)
    });
    match first_key {
        Some(k) => m.get(&k),
        None => m.values().next(),
    }
}

/// A V1 `Metrics` snapshot (cumulative).
fn metrics_usage(m: &Value) -> Event {
    let t = |k: &str| util::get(m, &["accumulated_token_usage", k]).cloned().unwrap_or(Value::Null);
    let n = |k: &str| util::get(m, &["accumulated_token_usage", k]).and_then(Value::as_u64);
    // SPEC §3.1.1: input excludes cache reads and writes.
    let input =
        n("prompt_tokens").map(|p| json!(p.saturating_sub(n("cache_read_tokens").unwrap_or(0) + n("cache_write_tokens").unwrap_or(0)))).unwrap_or(Value::Null);
    Event::new("", Kind::Usage)
        .with("model", m.get("model_name").cloned().or_else(|| util::get(m, &["accumulated_token_usage", "model"]).cloned()).unwrap_or(Value::Null))
        .with("input", input)
        .with("output", t("completion_tokens"))
        .with("cache_read", t("cache_read_tokens"))
        .with("cache_write", t("cache_write_tokens"))
        .with("reasoning", t("reasoning_tokens"))
        .with("cost_usd", m.get("accumulated_cost").cloned().unwrap_or(Value::Null))
        .with("cumulative", true)
}

/// Carried events in a record, wherever this adapter puts them.
fn take_carrier(record: &mut Value) -> Option<(Vec<Event>, Option<Agent>, Vec<String>)> {
    let at = ["llm_message", "tool_call"].into_iter().find(|k| record.get(*k).is_some_and(|x| x.get(carry::KEY).is_some()))?;
    take_carrier_at(record, Some(at))
}

/// [`carry::take_with`] at `at` (each carried native record's exact text put
/// back), plus the ids of the records the carrier covers.
fn take_carrier_at(record: &mut Value, at: Option<&str>) -> Option<(Vec<Event>, Option<Agent>, Vec<String>)> {
    let path: Vec<&str> = at.into_iter().collect();
    let holder = match at {
        Some(k) => record.get(k)?,
        None => &*record,
    };
    let covers = holder
        .get(carry::KEY)?
        .get("covers")
        .and_then(Value::as_array)
        .map(|a| a.iter().filter_map(|x| x.as_str().map(String::from)).collect())
        .unwrap_or_default();
    let c = carry::take_with(record, &path)?;
    Some((c.events, c.agent, covers))
}

fn map_records(
    generation: Gen,
    records: Vec<(Value, Option<String>, String)>,
    variant: &str,
    version: Option<&str>,
    opts: &ImportOptions,
    losses: &mut Losses,
    pre: Option<(Vec<Event>, Option<Agent>, Vec<String>)>,
) -> (Vec<Event>, Option<Agent>) {
    let mut out = Vec::new();
    let mut agent = None;
    // Restore carried events first; records they cover are not re-imported.
    let mut restored: HashMap<usize, Vec<Event>> = HashMap::new();
    let mut covered: HashSet<String> = HashSet::new();
    if let Some((evs, a, cov)) = pre {
        out.extend(evs);
        agent = a;
        covered.extend(cov);
    }
    if opts.restore_carried {
        for (i, (v, _, _)) in records.iter().enumerate() {
            let mut v = v.clone();
            if let Some((evs, a, cov)) = take_carrier(&mut v) {
                if a.is_some() && agent.is_none() {
                    agent = a;
                }
                covered.extend(cov);
                restored.insert(i, evs);
            }
        }
    }
    let mut ctx = MapCtx::default();
    // V0: action id → (call id, tool name), for observations that arrive first.
    if generation == Gen::V0 {
        for (i, (v, _, _)) in records.iter().enumerate() {
            if let Some(r) = util::get_str(v, &["tool_call_metadata", "model_response", "id"]) {
                ctx.v0_last.insert(r.to_string(), i);
            }
        }
        for (v, _, _) in &records {
            if v.get("action").is_some()
                && let Some(id) = v.get("id").map(id_str)
                && let Some(c) = v0_call(v)
            {
                ctx.v0_calls.insert(id, c);
            }
        }
    }
    let mut prev_id: Option<String> = None;
    for (i, (v, text, file)) in records.into_iter().enumerate() {
        if let Some(evs) = restored.remove(&i) {
            out.extend(evs);
            continue;
        }
        let rid = v.get("id").map(id_str);
        if rid.as_ref().is_some_and(|r| covered.contains(r)) {
            continue;
        }
        let base = rid.clone().unwrap_or_else(|| format!("@{i}"));
        ctx.cur = i;
        ctx.cur_text = text.clone();
        let mut evs = if v.is_null() {
            vec![Event::new("", Kind::Native)]
        } else {
            match generation {
                Gen::V0 => map_v0(&mut ctx, &v, losses),
                Gen::V1 => map_v1(&mut ctx, &v, losses),
            }
        };
        if evs.is_empty() {
            let t = v.get("action").or(v.get("observation")).or(v.get("kind")).and_then(Value::as_str).unwrap_or("?");
            losses.add("record-native-only", Some(Kind::Native), &format!("openhands events with no neutral meaning (e.g. {t})"));
            evs.push(Event::new("", Kind::Native));
        }
        let ts = v.get("timestamp").and_then(Value::as_str).map(utc_ts);
        let mut n = Native::new(FORMAT, Some(base.clone())).variant(Some(variant)).version(version);
        if !file.is_empty() {
            n.file = Some(file);
        }
        if v.is_null() {
            // Not an event (a malformed file, a `null` item): kept only as its text.
            n.raw = Some(text.unwrap_or_else(|| "null".to_string()));
        } else {
            hold_text(&mut n, text.as_deref(), &v);
        }
        // V1 `parent_id` names the causal predecessor; keep it when it is not the previous event.
        let after = v.get("parent_id").and_then(Value::as_str).filter(|p| prev_id.as_deref() != Some(*p)).map(String::from);
        let count = evs.len();
        for (k, e) in evs.iter_mut().enumerate() {
            e.id = if k == 0 { base.clone() } else { format!("{base}#{k}") };
            if e.ts.is_none() {
                e.ts = ts.clone();
            }
            if k == 0 {
                e.after = after.clone();
            }
            e.native = Some(if k == 0 { n.clone() } else { n.part_of(k as u32) });
        }
        let _ = count;
        prev_id = rid;
        out.extend(evs);
    }
    (out, agent)
}

/// SPEC §3.1.7: times are RFC 3339 in UTC. OpenHands writes naive
/// `datetime.isoformat()` times (local clock, no zone); they are read as UTC.
fn utc_ts(t: &str) -> String {
    // By bytes: index 19 of a damaged time may fall inside a character.
    let has_zone = t.ends_with('Z') || t.as_bytes().get(19..).is_some_and(|r| !r.is_empty() && (r.contains(&b'+') || r.contains(&b'-')));
    if has_zone { t.to_string() } else { format!("{t}Z") }
}

fn id_str(v: &Value) -> String {
    match v {
        Value::String(s) => s.clone(),
        other => other.to_string(),
    }
}

#[derive(Default)]
struct MapCtx {
    /// V0 action id → (call id, tool name, is tool action).
    v0_calls: HashMap<String, (String, String)>,
    /// LLM responses already seen (usage/reasoning are emitted once per response).
    responses: HashSet<String>,
    /// V1 tool call id → tool name.
    v1_tools: HashMap<String, String>,
    /// V0: the last record of each LLM response, where its usage goes.
    v0_last: HashMap<String, usize>,
    cur: usize,
    /// The current record's text as written, for what only its spelling says
    /// (which metrics snapshot comes first).
    cur_text: Option<String>,
}

fn texts(content: Option<&Value>) -> String {
    match content {
        Some(Value::String(s)) => s.clone(),
        Some(Value::Array(a)) => a
            .iter()
            .filter(|&c| c.get("type").and_then(Value::as_str) == Some("text"))
            .map(|c| c.get("text").and_then(Value::as_str).unwrap_or("").to_string())
            .collect::<Vec<_>>()
            .join(""),
        _ => String::new(),
    }
}

/// `data:<mime>;base64,<data>` → (mime, data).
fn data_url(u: &str) -> Option<(String, String)> {
    let rest = u.strip_prefix("data:")?;
    let (mime, data) = rest.split_once(";base64,")?;
    Some((mime.to_string(), data.to_string()))
}

fn media_from_url(role: &str, u: &str) -> Event {
    match data_url(u) {
        Some((mime, data)) => Event::new("", Kind::Media).with("role", role).with("media_type", mime).with("data", data),
        None => Event::new("", Kind::Media).with("role", role).with("media_type", "image/*").with("url", u),
    }
}

// ---- V0 ---------------------------------------------------------------------

const V0_TOOL_ACTIONS: [&str; 12] =
    ["run", "run_ipython", "read", "write", "edit", "browse", "browse_interactive", "call_tool_mcp", "think", "delegate", "task_tracking", "finish"];

fn v0_call(v: &Value) -> Option<(String, String)> {
    let action = v.get("action")?.as_str()?;
    let meta = v.get("tool_call_metadata");
    let name = meta.and_then(|m| m.get("function_name")).and_then(Value::as_str);
    if meta.is_none() && (!V0_TOOL_ACTIONS.contains(&action) || action == "finish" || action == "think") {
        return None;
    }
    let call = meta
        .and_then(|m| m.get("tool_call_id"))
        .and_then(Value::as_str)
        .map(String::from)
        .unwrap_or_else(|| format!("oh_{}", v.get("id").map(id_str).unwrap_or_default()));
    Some((call, name.unwrap_or(action).to_string()))
}

fn v0_usage(ctx: &mut MapCtx, v: &Value, evs: &mut Vec<Event>) -> Option<String> {
    let mr = util::get(v, &["tool_call_metadata", "model_response"]).filter(|m| m.is_object());
    let rid = mr.and_then(|m| m.get("id")).and_then(Value::as_str).map(String::from);
    if let (Some(mr), Some(r)) = (mr, &rid) {
        if ctx.responses.insert(r.clone())
            && let Some(rc) = util::get_str(mr, &["choices", "0", "message", "reasoning_content"])
                .or_else(|| mr.get("choices").and_then(|c| c.get(0)).and_then(|c| util::get_str(c, &["message", "reasoning_content"])))
            && !rc.is_empty()
        {
            evs.insert(0, Event::new("", Kind::Reasoning).with("text", rc));
        }
        // SPEC §3.1.1: one usage per response, after that response's events.
        if ctx.v0_last.get(r) == Some(&ctx.cur)
            && let Some(u) = mr.get("usage").filter(|u| u.is_object())
        {
            let g = |p: &[&str]| util::get(u, p).cloned().unwrap_or(Value::Null);
            // SPEC §3.1.1: input excludes cache reads (OpenAI counts them in prompt_tokens).
            let n = |p: &[&str]| util::get(u, p).and_then(Value::as_u64);
            let cached = n(&["prompt_tokens_details", "cached_tokens"]).unwrap_or(0);
            let input = n(&["prompt_tokens"]).map(|p| json!(p.saturating_sub(cached))).unwrap_or(Value::Null);
            evs.push(
                Event::new("", Kind::Usage)
                    .with("model", mr.get("model").cloned().unwrap_or(Value::Null))
                    .with("input", input)
                    .with("output", g(&["completion_tokens"]))
                    .with("cache_read", g(&["prompt_tokens_details", "cached_tokens"]))
                    .with("reasoning", g(&["completion_tokens_details", "reasoning_tokens"]))
                    .with("cumulative", false),
            );
        }
    }
    if let Some(m) = v.get("llm_metrics").filter(|m| m.is_object()) {
        let t = |k: &str| util::get(m, &["accumulated_token_usage", k]).cloned().unwrap_or(Value::Null);
        let n = |k: &str| util::get(m, &["accumulated_token_usage", k]).and_then(Value::as_u64);
        let input = n("prompt_tokens")
            .map(|p| json!(p.saturating_sub(n("cache_read_tokens").unwrap_or(0) + n("cache_write_tokens").unwrap_or(0))))
            .unwrap_or(Value::Null);
        evs.push(
            Event::new("", Kind::Usage)
                .with("model", t("model"))
                .with("input", input)
                .with("output", t("completion_tokens"))
                .with("cache_read", t("cache_read_tokens"))
                .with("cache_write", t("cache_write_tokens"))
                .with("cost_usd", m.get("accumulated_cost").cloned().unwrap_or(Value::Null))
                .with("cumulative", true),
        );
    }
    rid
}

fn map_v0(ctx: &mut MapCtx, v: &Value, losses: &mut Losses) -> Vec<Event> {
    let s = |p: &[&str]| util::get_str(v, p);
    let source = s(&["source"]).unwrap_or("");
    let mut evs = Vec::new();
    if let Some(action) = s(&["action"]) {
        let args = v.get("args").cloned().unwrap_or(json!({}));
        let a = |k: &str| args.get(k).and_then(Value::as_str).unwrap_or("");
        match action {
            "system" => {
                let mut e = Event::new("", Kind::Settings).with("system_prompt", a("content"));
                if let Some(t) = args.get("tools") {
                    e = e.with("tools", t.clone());
                }
                if !a("agent_class").is_empty() {
                    e = e.with("agent", a("agent_class"));
                }
                evs.push(e);
            }
            "message" => {
                let role = match source {
                    "user" => "user",
                    "agent" => "assistant",
                    _ => "system",
                };
                evs.push(text_or_peer(role, a("content").to_string()));
                for u in args.get("image_urls").and_then(Value::as_array).map(|x| x.as_slice()).unwrap_or(&[]) {
                    if let Some(u) = u.as_str() {
                        evs.push(media_from_url(role, u));
                    }
                }
                let group = v0_usage(ctx, v, &mut evs);
                if let Some(g) = group {
                    for e in &mut evs {
                        e.group = Some(g.clone());
                    }
                }
            }
            "condensation" => {
                let summary = args.get("summary").and_then(Value::as_str);
                let mut e = Event::new("", Kind::Compaction).with("phase", if summary.is_some() { "summary" } else { "boundary" });
                if let Some(sm) = summary {
                    e = e.with("summary", sm);
                }
                if let Some(f) = args.get("forgotten_event_ids").filter(|f| !f.is_null()) {
                    e = e.with("forgotten", f.clone());
                }
                evs.push(e);
            }
            _ => {
                let thought = a("thought");
                if action == "finish" {
                    let ft = args.get("final_thought").and_then(Value::as_str).filter(|x| !x.is_empty()).unwrap_or(thought);
                    if !ft.is_empty() {
                        evs.push(Event::new("", Kind::Text).with("role", "assistant").with("text", ft));
                    }
                } else if !thought.is_empty() && source == "agent" {
                    evs.push(Event::new("", Kind::Text).with("role", "assistant").with("text", thought));
                }
                let call = v.get("id").map(id_str).and_then(|id| ctx.v0_calls.get(&id).cloned());
                if let Some((call_id, name)) = call {
                    // The arguments the model sent, if the response is kept.
                    let input = util::get(v, &["tool_call_metadata", "model_response", "choices"])
                        .and_then(Value::as_array)
                        .and_then(|c| c.first())
                        .and_then(|c| util::get(c, &["message", "tool_calls"]))
                        .and_then(Value::as_array)
                        .and_then(|tc| tc.iter().find(|t| t.get("id").and_then(Value::as_str) == Some(call_id.as_str())))
                        .and_then(|t| util::get_str(t, &["function", "arguments"]))
                        .and_then(|a| serde_json::from_str::<Value>(a).ok())
                        .unwrap_or_else(|| {
                            let mut m = args.as_object().cloned().unwrap_or_default();
                            m.remove_ordered("thought");
                            Value::Object(m)
                        });
                    let tool_type = if action == "call_tool_mcp" {
                        "mcp"
                    } else if action == "run" {
                        "shell"
                    } else {
                        "function"
                    };
                    evs.push(Event::new("", Kind::ToolCall).with("call_id", call_id).with("name", name).with("input", input).with("tool_type", tool_type));
                } else if !matches!(action, "finish" | "think") && evs.is_empty() {
                    return vec![];
                }
                if action == "delegate" {
                    evs.push(
                        Event::new("", Kind::Subagent)
                            .with("phase", "start")
                            .with("agent_type", a("agent"))
                            .with("description", util::get(&args, &["inputs", "task"]).cloned().unwrap_or(Value::Null)),
                    );
                }
                if action == "think" && !ctx.v0_calls.contains_key(&v.get("id").map(id_str).unwrap_or_default()) {
                    evs.push(Event::new("", Kind::Reasoning).with("text", thought));
                }
                let group = v0_usage(ctx, v, &mut evs);
                if let Some(g) = group {
                    for e in &mut evs {
                        if matches!(e.kind, Kind::Text | Kind::Reasoning | Kind::ToolCall | Kind::Usage) {
                            e.group = Some(g.clone());
                        }
                    }
                }
            }
        }
        return evs;
    }
    let Some(obs) = s(&["observation"]) else { return evs };
    let content = v.get("content").and_then(Value::as_str).unwrap_or("").to_string();
    let extras = v.get("extras").cloned().unwrap_or(json!({}));
    let cause = v.get("cause").map(id_str);
    let call = cause.as_ref().and_then(|c| ctx.v0_calls.get(c).cloned());
    match obs {
        "agent_state_changed" => {
            let st = extras.get("agent_state").and_then(Value::as_str).unwrap_or("");
            let turn = match st {
                "running" => Some(("start", None)),
                "awaiting_user_input" | "finished" => Some(("end", Some("ok"))),
                "error" => Some(("end", Some("error"))),
                "paused" | "stopped" | "user_rejected" | "rejected" => Some(("end", Some("interrupted"))),
                _ => None,
            };
            if let Some((phase, outcome)) = turn {
                let mut e = Event::new("", Kind::Turn).with("phase", phase).with("reason", st);
                if let Some(o) = outcome {
                    e = e.with("outcome", o);
                }
                evs.push(e);
            }
        }
        _ => {
            if let Some((call_id, _)) = call {
                let is_error = obs == "error" || obs == "user_rejected" || v.get("success").and_then(Value::as_bool) == Some(false);
                let mut e = Event::new("", Kind::ToolResult).with("call_id", call_id).with("output", content.clone()).with("is_error", is_error);
                if obs == "user_rejected" {
                    e = e.with("status", "rejected");
                }
                evs.push(e);
                let path = extras.get("path").and_then(Value::as_str);
                match (obs, path) {
                    ("edit", Some(p)) if !is_error => {
                        let created = extras.get("prev_exist").and_then(Value::as_bool) == Some(false);
                        let mut f = Event::new("", Kind::FileChange).with("path", p).with("op", if created { "create" } else { "edit" });
                        if let Some(d) = extras.get("diff").and_then(Value::as_str) {
                            f = f.with("diff", d);
                        }
                        if let Some(c) = extras.get("new_content").and_then(Value::as_str) {
                            f = f.with("content", c);
                        }
                        evs.push(f);
                    }
                    ("write", Some(p)) if !is_error => evs.push(Event::new("", Kind::FileChange).with("path", p).with("op", "edit")),
                    _ => {}
                }
                if obs == "delegate" {
                    evs.push(Event::new("", Kind::Subagent).with("phase", "end").with("result", extras.get("outputs").cloned().unwrap_or(Value::Null)));
                }
            } else if obs == "error" {
                evs.push(Event::new("", Kind::Error).with("kind", extras.get("error_id").and_then(Value::as_str).unwrap_or("error")).with("message", content));
            } else if obs == "user_rejected" {
                evs.push(Event::new("", Kind::Turn).with("phase", "end").with("outcome", "interrupted").with("reason", "user_rejected"));
            }
            let _ = losses;
        }
    }
    evs
}

// ---- V1 ---------------------------------------------------------------------

fn v1_reasoning(v: &Value, evs: &mut Vec<Event>, vendor_hint: &str) {
    if let Some(rc) = v.get("reasoning_content").and_then(Value::as_str).filter(|x| !x.is_empty()) {
        evs.push(Event::new("", Kind::Reasoning).with("text", rc));
    }
    for tb in v.get("thinking_blocks").and_then(Value::as_array).map(|x| x.as_slice()).unwrap_or(&[]) {
        match tb.get("type").and_then(Value::as_str) {
            Some("thinking") => {
                let mut e = Event::new("", Kind::Reasoning).with("text", tb.get("thinking").and_then(Value::as_str).unwrap_or(""));
                if let Some(sig) = tb.get("signature").and_then(Value::as_str) {
                    e = e.with("sealed", json!({"vendor": "anthropic", "kind": "signature", "data": sig}));
                }
                evs.push(e);
            }
            Some("redacted_thinking") => evs.push(
                Event::new("", Kind::Reasoning)
                    .with("text", "")
                    .with("redacted", true)
                    .with("sealed", json!({"vendor": "anthropic", "kind": "encrypted", "data": tb.get("data").cloned().unwrap_or(Value::Null)})),
            ),
            _ => {}
        }
    }
    if let Some(ri) = v.get("responses_reasoning_item").filter(|r| r.is_object()) {
        let summary: Vec<Value> = ri.get("summary").and_then(Value::as_array).cloned().unwrap_or_default();
        let content =
            ri.get("content").and_then(Value::as_array).map(|c| c.iter().filter_map(Value::as_str).collect::<Vec<_>>().join("\n")).unwrap_or_default();
        let mut e = Event::new("", Kind::Reasoning).with("text", content).with("summary", Value::Array(summary));
        if let Some(enc) = ri.get("encrypted_content").and_then(Value::as_str) {
            e = e.with("sealed", json!({"vendor": "openai", "kind": "encrypted", "data": enc}));
        }
        evs.push(e);
    }
    let _ = vendor_hint;
}

fn obs_output(content: Option<&Value>) -> Value {
    let parts: Vec<&Value> = content.and_then(Value::as_array).map(|a| a.iter().collect()).unwrap_or_default();
    if parts.iter().all(|p| p.get("type").and_then(Value::as_str) == Some("text")) && parts.len() <= 1 {
        return json!(texts(content));
    }
    let mut out = Vec::new();
    for p in parts {
        match p.get("type").and_then(Value::as_str) {
            Some("text") => out.push(json!({"type":"text","text": p.get("text").cloned().unwrap_or(json!(""))})),
            Some("image") => {
                for u in p.get("image_urls").and_then(Value::as_array).map(|x| x.as_slice()).unwrap_or(&[]) {
                    if let Some(u) = u.as_str() {
                        match data_url(u) {
                            Some((m, d)) => out.push(json!({"type":"media","media_type":m,"data":d})),
                            None => out.push(json!({"type":"media","media_type":"image/*","url":u})),
                        }
                    }
                }
            }
            _ => {}
        }
    }
    Value::Array(out)
}

fn map_v1(ctx: &mut MapCtx, v: &Value, losses: &mut Losses) -> Vec<Event> {
    let s = |k: &str| v.get(k).and_then(Value::as_str);
    let mut evs = Vec::new();
    let group = s("llm_response_id").map(String::from);
    match s("kind").unwrap_or("") {
        "SystemPromptEvent" => {
            let mut e = Event::new("", Kind::Settings).with(
                "system_prompt",
                texts(v.get("system_prompt").map(|p| if p.is_object() { std::slice::from_ref(p) } else { &[] }).map(|x| Value::Array(x.to_vec())).as_ref()),
            );
            if let Some(t) = v.get("tools") {
                e = e.with("tools", t.clone());
            }
            evs.push(e);
        }
        "MessageEvent" => {
            let m = v.get("llm_message").cloned().unwrap_or(json!({}));
            let role = m.get("role").and_then(Value::as_str).unwrap_or("user");
            let role = if role == "tool" { "user" } else { role };
            v1_reasoning(&m, &mut evs, "");
            let content = m.get("content");
            match content {
                Some(Value::String(t)) => evs.push(text_or_peer(role, t.clone())),
                Some(Value::Array(a)) => {
                    for c in a {
                        match c.get("type").and_then(Value::as_str) {
                            Some("text") => evs.push(text_or_peer(role, c.get("text").and_then(Value::as_str).unwrap_or("").to_string())),
                            Some("image") => {
                                for u in c.get("image_urls").and_then(Value::as_array).map(|x| x.as_slice()).unwrap_or(&[]) {
                                    if let Some(u) = u.as_str() {
                                        evs.push(media_from_url(role, u));
                                    }
                                }
                            }
                            _ => {}
                        }
                    }
                }
                _ => {}
            }
            for c in v.get("extended_content").and_then(Value::as_array).map(|x| x.as_slice()).unwrap_or(&[]) {
                if let Some(t) = c.get("text").and_then(Value::as_str) {
                    evs.push(Event::new("", Kind::Text).with("role", role).with("text", t));
                }
            }
            for e in &mut evs {
                e.group = group.clone();
            }
        }
        "ActionEvent" => {
            let first = group.as_ref().is_none_or(|g| ctx.responses.insert(g.clone()));
            if first {
                v1_reasoning(v, &mut evs, "");
            }
            let thought = texts(v.get("thought"));
            if !thought.is_empty() {
                evs.push(Event::new("", Kind::Text).with("role", "assistant").with("text", thought));
            }
            let call_id = s("tool_call_id").unwrap_or("").to_string();
            let name = s("tool_name").unwrap_or("").to_string();
            ctx.v1_tools.insert(call_id.clone(), name.clone());
            let args = util::get_str(v, &["tool_call", "arguments"]);
            let mut e = Event::new("", Kind::ToolCall).with("call_id", call_id).with("name", name);
            match args.map(serde_json::from_str::<Value>) {
                Some(Ok(j)) => e = e.with("input", j),
                Some(Err(_)) => e = e.with("input_text", args.unwrap()),
                None => e = e.with("input", v.get("action").cloned().unwrap_or(json!({}))),
            }
            let akind = util::get_str(v, &["action", "kind"]).unwrap_or("");
            e = e.with(
                "tool_type",
                if akind.starts_with("MCP") {
                    "mcp"
                } else if akind == "TerminalAction" {
                    "shell"
                } else {
                    "function"
                },
            );
            evs.push(e);
            for e in &mut evs {
                e.group = group.clone();
            }
        }
        "ObservationEvent" => {
            let o = v.get("observation").cloned().unwrap_or(json!({}));
            let is_error = o.get("is_error").and_then(Value::as_bool).unwrap_or(false);
            evs.push(
                Event::new("", Kind::ToolResult)
                    .with("call_id", s("tool_call_id").unwrap_or(""))
                    .with("output", obs_output(o.get("content")))
                    .with("is_error", is_error),
            );
            if o.get("kind").and_then(Value::as_str) == Some("FileEditorObservation") && !is_error {
                let cmd = o.get("command").and_then(Value::as_str).unwrap_or("");
                if let (Some(p), true) = (o.get("path").and_then(Value::as_str), matches!(cmd, "create" | "str_replace" | "insert" | "undo_edit")) {
                    let created = cmd == "create" && o.get("prev_exist").and_then(Value::as_bool) != Some(true);
                    let mut f = Event::new("", Kind::FileChange).with("path", p).with("op", if created { "create" } else { "edit" });
                    if let Some(c) = o.get("new_content").and_then(Value::as_str) {
                        f = f.with("content", c);
                    }
                    evs.push(f);
                }
            }
        }
        "UserRejectObservation" => evs.push(
            Event::new("", Kind::ToolResult)
                .with("call_id", s("tool_call_id").unwrap_or(""))
                .with("output", s("rejection_reason").unwrap_or(""))
                .with("is_error", true)
                .with("status", "rejected"),
        ),
        "AgentErrorEvent" => match s("tool_call_id").filter(|c| !c.is_empty()) {
            Some(c) => evs.push(Event::new("", Kind::ToolResult).with("call_id", c).with("output", s("error").unwrap_or("")).with("is_error", true)),
            None => evs.push(Event::new("", Kind::Error).with("kind", "agent").with("message", s("error").unwrap_or(""))),
        },
        "ConversationErrorEvent" => evs.push(Event::new("", Kind::Error).with("kind", s("code").unwrap_or("error")).with("message", s("detail").unwrap_or(""))),
        "Condensation" => {
            let summary = s("summary");
            let mut e = Event::new("", Kind::Compaction).with("phase", if summary.is_some() { "summary" } else { "boundary" });
            if let Some(sm) = summary {
                e = e.with("summary", sm);
            }
            if let Some(f) = v.get("forgotten_event_ids") {
                e = e.with("forgotten", f.clone());
            }
            evs.push(e);
        }
        "CondensationSummaryEvent" => evs.push(Event::new("", Kind::Compaction).with("phase", "summary").with("summary", s("summary").unwrap_or(""))),
        "PauseEvent" | "InterruptEvent" => {
            evs.push(Event::new("", Kind::Turn).with("phase", "end").with("outcome", "interrupted").with("reason", s("kind").unwrap_or("")))
        }
        "ACPToolCallEvent" => {
            let id = s("tool_call_id").unwrap_or("").to_string();
            evs.push(
                Event::new("", Kind::ToolCall)
                    .with("call_id", id.clone())
                    .with("name", s("title").unwrap_or(""))
                    .with("input", v.get("raw_input").cloned().unwrap_or(json!({})))
                    .with("tool_type", "function"),
            );
            if v.get("raw_output").is_some() || v.get("content").is_some() {
                let out = v.get("raw_output").cloned().filter(|x| !x.is_null()).unwrap_or_else(|| json!(texts(v.get("content"))));
                let out = if out.is_string() { out } else { json!(out.to_string()) };
                evs.push(
                    Event::new("", Kind::ToolResult)
                        .with("call_id", id)
                        .with("output", out)
                        .with("is_error", v.get("is_error").and_then(Value::as_bool).unwrap_or(false)),
                );
            }
        }
        "ConversationStateUpdateEvent" if s("key") == Some("stats") => {
            if let Some(u) = first_metrics(v, ctx.cur_text.as_deref(), &["value", "usage_to_metrics"]) {
                evs.push(metrics_usage(u));
            }
        }
        _ => {}
    }
    let _ = losses;
    evs
}

// ---------------------------------------------------------------------------
// Export
// ---------------------------------------------------------------------------

fn export(set: &LedgerSet, opts: &ExportOptions) -> Result<Exported> {
    let mut out = Output::default();
    let mut losses = Losses::default();
    let mut variant = "v1-events";
    for l in set.iter() {
        let src = l.agent().source.as_ref().filter(|s| s.format == FORMAT);
        let v = src.and_then(|s| s.variant.clone()).unwrap_or_default();
        match v.as_str() {
            "v0-events" | "v1-events" if src.and_then(|s| s.extra.get("dir")).is_some() => {
                variant = if v == "v0-events" { "v0-events" } else { "v1-events" };
                export_native_dir(l, &v, opts, &mut out, &mut losses);
            }
            "v0-trajectory" | "v1-api" if src.and_then(|s| s.extra.get("file")).is_some() => {
                variant = if v == "v0-trajectory" { "v0-trajectory" } else { "v1-api" };
                export_native_file(l, &v, opts, &mut out, &mut losses);
            }
            _ => {
                if l.agent().parent.is_some() {
                    losses.add("subagent-separate-conversation", Some(Kind::Subagent), "a child agent becomes its own OpenHands conversation");
                }
                let hex = util::uuid_for(&format!("openhands/{}", l.id())).replace('-', "");
                let files = render_v1(l, opts, &mut losses, None);
                for (name, bytes) in files {
                    out.put(format!("{hex}/{name}"), bytes);
                }
            }
        }
    }
    Ok(Exported { output: out, losses: losses.sorted(), variant: variant.into() })
}

/// Records of this format found intact, in order, and the foreign events.
fn split_native<'a>(l: &'a Ledger, gen_variant: &str) -> (Vec<&'a crate::model::Native>, Vec<&'a Event>) {
    let mut natives = Vec::new();
    let mut foreign = Vec::new();
    let mut last: Option<&str> = None;
    for e in &l.events {
        if let Some(n) = e.native.as_ref().filter(|n| n.format == FORMAT && n.variant.as_deref() == Some(gen_variant)) {
            if n.part == 0 && (n.record.is_some() || n.raw.is_some()) {
                natives.push(n);
                last = n.id.as_deref();
                continue;
            }
            if n.part > 0 && last.is_some() && last == n.id.as_deref() {
                continue;
            }
        }
        last = None;
        foreign.push(e);
    }
    (natives, foreign)
}

fn export_native_dir(l: &Ledger, v: &str, opts: &ExportOptions, out: &mut Output, losses: &mut Losses) {
    let src = l.agent().source.as_ref().unwrap();
    let dir = src.extra.get("dir").and_then(Value::as_str).unwrap_or("");
    let prefix = if dir.is_empty() { String::new() } else { format!("{dir}/") };
    let generation = if v == "v0-events" { Gen::V0 } else { Gen::V1 };
    let (natives, foreign) = split_native(l, v);
    let mut max_idx: Option<u64> = None;
    let mut ids: Vec<String> = Vec::new();
    for n in &natives {
        // Each file exactly as held (SPEC §4). Only a record that is no longer
        // the source's (scrubbed) is written anew, a V0 event as Python writes it.
        let sidecar = matches!(n.id.as_deref(), Some("base_state" | "metadata"));
        let text = match (crate::util::current_raw(n), n.record_text()) {
            (Some(r), _) => r.to_string(),
            (None, Some(_)) if n.altered && generation == Gen::V0 && !sidecar => py_dumps(&n.record_json().expect("a record is present"), None),
            (None, Some(t)) => t.to_string(),
            _ => continue,
        };
        let file = n.file.clone().unwrap_or_else(|| format!("{prefix}events/{}.json", n.id.clone().unwrap_or_default()));
        if let Some(name) = file.rsplit('/').next()
            && let Some(i) = if generation == Gen::V1 { v1_idx(name) } else { v0_idx(name) }
        {
            max_idx = Some(max_idx.map_or(i, |m| m.max(i)));
            if let Some(id) = n.record_json().as_ref().and_then(|r| r.get("id")).and_then(Value::as_str) {
                ids.push(id.to_string());
            }
        }
        out.put(file, text.into_bytes());
    }
    for (k, key) in [("files", false), ("files_b64", true)] {
        if let Some(Value::Object(m)) = src.extra.get(k) {
            for (r, t) in m {
                let t = t.as_str().unwrap_or("");
                let bytes = if key { base64::engine::general_purpose::STANDARD.decode(t).unwrap_or_default() } else { t.as_bytes().to_vec() };
                out.put(format!("{prefix}{r}"), bytes);
            }
        }
    }
    if !foreign.is_empty() {
        match generation {
            Gen::V1 => {
                let start = max_idx.map_or(0, |m| m + 1);
                let mut sub = Ledger::new(l.agent().clone());
                for e in foreign {
                    sub.append_unique(e.clone());
                }
                let files = render_v1(&sub, opts, losses, Some((start, ids.last().cloned())));
                for (name, bytes) in files {
                    if name != "base_state.json" {
                        out.put(format!("{prefix}{name}"), bytes);
                    }
                }
            }
            Gen::V0 => {
                losses.add_n("v0-append-unsupported", None, foreign.len() as u64, "events added to a V0 conversation are not written; OpenHands V0 is retired")
            }
        }
    }
}

fn export_native_file(l: &Ledger, v: &str, opts: &ExportOptions, out: &mut Output, losses: &mut Losses) {
    let src = l.agent().source.as_ref().unwrap();
    let name = src.extra.get("file").and_then(Value::as_str).unwrap_or("events.json").to_string();
    let (natives, foreign) = split_native(l, v);
    let none_foreign = foreign.is_empty();
    let layout = src.extra.get("layout").and_then(Style::from_value);
    // Each native record as held (SPEC §4), never re-serialised: its `raw`
    // while that still says what the record says (it is what survives a
    // carrier that holds records as values, which re-spells the record), else
    // the record's own text, so an edit to the record is never lost.
    let mut texts: Vec<String> = natives.iter().filter_map(|n| crate::util::native_text(n)).collect();
    let mut fresh: Vec<Value> = Vec::new();
    if !foreign.is_empty() {
        if v == "v1-api" {
            let mut sub = Ledger::new(l.agent().clone());
            for e in foreign {
                sub.append_unique(e.clone());
            }
            for (fname, bytes) in render_v1(&sub, opts, losses, Some((0, None))) {
                if fname.starts_with("events/event-")
                    && let Ok(v) = serde_json::from_slice::<Value>(&bytes)
                {
                    fresh.push(v);
                }
            }
        } else {
            losses.add_n("v0-append-unsupported", None, foreign.len() as u64, "events added to a V0 trajectory are not written");
        }
    }
    let envelope = src.extra.get("envelope").and_then(Value::as_str);
    if envelope == Some("jsonl") {
        let mut text = String::new();
        for line in texts.iter().cloned().chain(fresh.iter().map(|v| serde_json::to_string(v).unwrap())) {
            text.push_str(&line);
            text.push('\n');
        }
        if src.extra.get("eof_newline").and_then(Value::as_bool) == Some(false) {
            text.pop();
        }
        out.put(name, text.into_bytes());
        return;
    }
    // Rebuilt from the records' texts and laid out in the file's own style;
    // events added here are written as that style writes strings.
    let style = layout.unwrap_or(Style { indent: Some(4), spaced: false, ascii: true, slash: false, newline: false, numbers_inline: false });
    let one_line = Style { indent: None, spaced: false, newline: false, numbers_inline: false, ..style };
    texts.extend(fresh.iter().map(|v| one_line.render(v)));
    let list = jsonfmt::array_text(texts.iter().map(String::as_str));
    let skeleton = src.extra.get("skeleton").and_then(Value::as_str).map(str::to_string).unwrap_or_else(|| match envelope {
        Some(k) => format!("{{{}:[]}}", serde_json::to_string(k).unwrap()),
        None => "[]".into(),
    });
    let doc = match envelope {
        Some(k) => jsonfmt::with_member(&skeleton, k, &list).unwrap_or_else(|| format!("{{{}:{list}}}", serde_json::to_string(k).unwrap())),
        None => list,
    };
    // A file no layout reproduces is kept whole, and written back whole while
    // its events are still the records' (an edited record is written anew).
    if none_foreign
        && let Some(raw) = src.extra.get("raw").and_then(Value::as_str)
        && match doc_items(raw, envelope) {
            Some((_, items)) => items.len() == texts.len() && items.iter().zip(&texts).all(|(a, b)| crate::util::same_json(a, b)),
            None => crate::util::same_json(raw, &doc),
        }
    {
        out.put(name, raw.as_bytes().to_vec());
        return;
    }
    out.put(name, style.relay(&doc).into_bytes());
}

// ---- rendering V1 from neutral events -------------------------------------

struct V1Rec {
    value: Value,
    /// Where a carrier may sit in this record (`None`: it cannot hold one).
    carry_at: Option<&'static str>,
    events: Vec<Event>,
    /// Records whose own events ride in this record's carrier.
    covers: Vec<String>,
}

struct V1Render<'a> {
    l: &'a Ledger,
    losses: &'a mut Losses,
    recs: Vec<V1Rec>,
    /// Events waiting for a record that can carry them.
    pending: Vec<Event>,
    pending_covers: Vec<String>,
    call_names: HashMap<String, String>,
    call_action: HashMap<String, String>,
    acc: Option<Acc>,
    user: Option<UserAcc>,
    last_ts: String,
}

struct Acc {
    group: Option<String>,
    ts: String,
    seed: String,
    text: Vec<String>,
    reasoning: Vec<String>,
    thinking: Vec<Value>,
    /// SPEC §3.1.5: an OpenAI reasoning item (summary, encrypted content).
    item: Option<Value>,
    calls: Vec<Event>,
    events: Vec<Event>,
}

struct UserAcc {
    group: Option<String>,
    ts: String,
    seed: String,
    role: String,
    content: Vec<Value>,
    events: Vec<Event>,
}

/// pydantic `datetime.isoformat()` of a naive local time: microseconds, no zone.
fn oh_ts(ts: &str) -> String {
    match util::parse_rfc3339_ms(ts) {
        Some(ms) => {
            let s = util::ms_to_rfc3339(ms);
            format!("{}000", s.trim_end_matches('Z'))
        }
        None => ts.to_string(),
    }
}

fn text_content(t: &str) -> Value {
    json!({"cache_prompt": false, "type": "text", "text": t})
}

impl<'a> V1Render<'a> {
    fn id_for(&self, seed: &str) -> String {
        util::uuid_for(&format!("openhands/{}/{seed}", self.l.id()))
    }

    fn push(&mut self, value: Value, carry_at: Option<&'static str>, mut events: Vec<Event>) {
        if carry_at.is_some() {
            let mut evs = std::mem::take(&mut self.pending);
            evs.append(&mut events);
            let covers = std::mem::take(&mut self.pending_covers);
            self.recs.push(V1Rec { value, carry_at, events: evs, covers });
        } else {
            // It cannot carry: its events ride with the next record that can.
            if let Some(id) = value.get("id").and_then(Value::as_str) {
                self.pending_covers.push(id.to_string());
            }
            self.pending.extend(events);
            self.recs.push(V1Rec { value, carry_at, events: vec![], covers: vec![] });
        }
    }

    fn ts(&mut self, e: &Event) -> String {
        if let Some(t) = &e.ts {
            self.last_ts = oh_ts(t);
        }
        self.last_ts.clone()
    }

    fn flush(&mut self) {
        if let Some(u) = self.user.take() {
            let id = self.id_for(&u.seed);
            let v = json!({"id": id, "timestamp": u.ts, "source": "user",
                "llm_message": {"role": u.role, "content": u.content, "thinking_blocks": []},
                "activated_skills": [], "extended_content": [], "kind": "MessageEvent"});
            self.push(v, Some("llm_message"), u.events);
        }
        if let Some(a) = self.acc.take() {
            let resp = a.group.clone().unwrap_or_else(|| a.seed.clone());
            let rid = format!("protoledger-{}", util::short_id(&format!("{}/{resp}", self.l.id()), 16));
            let reasoning = (!a.reasoning.is_empty()).then(|| a.reasoning.join("\n\n"));
            if a.calls.is_empty() {
                let id = self.id_for(&a.seed);
                let mut msg = json!({"role": "assistant", "content": [text_content(&a.text.join("\n\n"))], "thinking_blocks": a.thinking});
                if let Some(r) = &reasoning {
                    msg["reasoning_content"] = json!(r);
                }
                if let Some(ri) = &a.item {
                    msg["responses_reasoning_item"] = ri.clone();
                }
                let v = json!({"id": id, "timestamp": a.ts, "source": "agent", "llm_message": msg,
                    "llm_response_id": rid, "activated_skills": [], "extended_content": [], "kind": "MessageEvent"});
                self.push(v, Some("llm_message"), a.events);
                return;
            }
            // One ActionEvent per call; the first carries the thought and reasoning.
            let n = a.calls.len();
            let mut evs = Some(a.events);
            let first_rec = self.recs.len();
            for (i, c) in a.calls.iter().enumerate() {
                let call_id = c.str("call_id").unwrap_or(&c.id).to_string();
                let name = c.str("name").unwrap_or("tool").to_string();
                let args = match (c.body.get("input"), c.str("input_text")) {
                    (_, Some(t)) => t.to_string(),
                    (Some(x), _) if !x.is_null() => serde_json::to_string(x).unwrap(),
                    _ => "{}".into(),
                };
                let id = self.id_for(&c.id);
                self.call_action.insert(call_id.clone(), id.clone());
                self.call_names.insert(call_id.clone(), name.clone());
                let thought: Vec<Value> = if i == 0 && !a.text.is_empty() { vec![text_content(&a.text.join("\n\n"))] } else { vec![] };
                let mut v = json!({"id": id, "timestamp": a.ts, "source": "agent", "thought": thought});
                if i == 0 {
                    if let Some(r) = &reasoning {
                        v["reasoning_content"] = json!(r);
                    }
                    v["thinking_blocks"] = Value::Array(a.thinking.clone());
                    if let Some(ri) = &a.item {
                        v["responses_reasoning_item"] = ri.clone();
                    }
                } else {
                    v["thinking_blocks"] = json!([]);
                }
                v["tool_name"] = json!(name);
                v["tool_call_id"] = json!(call_id);
                v["tool_call"] = json!({"id": call_id, "name": name, "arguments": args, "origin": "completion"});
                v["llm_response_id"] = json!(rid);
                v["security_risk"] = json!("UNKNOWN");
                v["kind"] = json!("ActionEvent");
                let e = if i == 0 { evs.take().unwrap() } else { vec![] };
                let _ = n;
                if i > 0 {
                    // Its events ride with the first call of the response.
                    self.recs.push(V1Rec { value: v, carry_at: None, events: vec![], covers: vec![] });
                    let id = self.recs.last().unwrap().value["id"].as_str().unwrap_or("").to_string();
                    self.recs[first_rec].covers.push(id);
                } else {
                    self.push(v, Some("tool_call"), e);
                }
            }
        }
    }

    fn asst(&mut self, e: &Event) -> &mut Acc {
        let joins = self.acc.as_ref().is_some_and(|a| a.group == e.group && (e.group.is_some() || a.calls.is_empty() || e.kind == Kind::ToolCall));
        if !joins {
            self.flush();
            let ts = self.ts(e);
            self.acc = Some(Acc {
                group: e.group.clone(),
                ts,
                seed: e.id.clone(),
                text: vec![],
                reasoning: vec![],
                thinking: vec![],
                item: None,
                calls: vec![],
                events: vec![],
            });
        } else if self.user.is_some() {
            let a = self.acc.take();
            self.flush();
            self.acc = a;
        }
        self.acc.as_mut().unwrap()
    }

    fn user_part(&mut self, e: &Event, role: &str, part: Value) {
        let joins = self.user.as_ref().is_some_and(|u| u.group.is_some() && u.group == e.group && u.role == role);
        if !joins || self.acc.is_some() {
            self.flush();
            let ts = self.ts(e);
            self.user = Some(UserAcc { group: e.group.clone(), ts, seed: e.id.clone(), role: role.into(), content: vec![], events: vec![] });
        }
        let u = self.user.as_mut().unwrap();
        u.content.push(part);
        u.events.push(e.clone());
    }

    fn defer(&mut self, e: &Event, code: &str, detail: &str) {
        self.losses.add(code, Some(e.kind), detail);
        if let Some(a) = self.acc.as_mut() {
            a.events.push(e.clone());
        } else if let Some(u) = self.user.as_mut() {
            u.events.push(e.clone());
        } else {
            self.pending.push(e.clone());
        }
    }

    fn render(&mut self, e: &Event) {
        match e.kind {
            Kind::Text => {
                let role = e.str("role").unwrap_or("user");
                let t = e.str("text").unwrap_or("");
                match role {
                    "assistant" => {
                        let a = self.asst(e);
                        a.text.push(t.to_string());
                        a.events.push(e.clone());
                    }
                    "system" | "developer" => self.user_part(e, "system", text_content(t)),
                    _ => self.user_part(e, "user", text_content(t)),
                }
            }
            Kind::Media => match (e.str("data"), e.str("url")) {
                (Some(d), _) => {
                    let u = format!("data:{};base64,{d}", e.str("media_type").unwrap_or("image/png"));
                    self.user_part(
                        e,
                        if e.str("role") == Some("assistant") { "assistant" } else { "user" },
                        json!({"cache_prompt": false, "type": "image", "image_urls": [u]}),
                    );
                }
                (None, Some(u)) => {
                    let u = u.to_string();
                    self.user_part(e, "user", json!({"cache_prompt": false, "type": "image", "image_urls": [u]}));
                }
                _ => self.defer(e, "media-dropped", "an image with neither data nor url"),
            },
            Kind::Reasoning => {
                let sealed = e.body.get("sealed").cloned();
                let text = e.str("text").unwrap_or("").to_string();
                let summary = e.body.get("summary").and_then(Value::as_array).filter(|s| !s.is_empty()).cloned();
                let a = self.asst(e);
                // SPEC §3.1.5: OpenAI's encrypted content, or summaries, go in
                // a `responses_reasoning_item` (one per response), which the
                // importer reads back.
                let openai =
                    sealed.as_ref().is_some_and(|s| s.get("vendor").and_then(Value::as_str) == Some("openai") && s.get("data").is_some_and(Value::is_string));
                if a.item.is_none() && (openai || (sealed.is_none() && summary.is_some())) {
                    let mut ri = json!({"type": "reasoning", "summary": summary.unwrap_or_default()});
                    if !text.is_empty() {
                        ri["content"] = json!([text]);
                    }
                    if openai {
                        ri["encrypted_content"] = sealed.as_ref().unwrap()["data"].clone();
                    }
                    a.item = Some(ri);
                    a.events.push(e.clone());
                    return;
                }
                match sealed {
                    Some(s) if s.get("vendor").and_then(Value::as_str) == Some("anthropic") && s.get("kind").and_then(Value::as_str) == Some("signature") => {
                        a.thinking.push(json!({"type": "thinking", "thinking": text, "signature": s.get("data").cloned().unwrap_or(Value::Null)}));
                    }
                    Some(s) if s.get("vendor").and_then(Value::as_str) == Some("anthropic") => {
                        a.thinking.push(json!({"type": "redacted_thinking", "data": s.get("data").cloned().unwrap_or(Value::Null)}));
                    }
                    Some(_) => {
                        a.reasoning.push(text);
                        a.events.push(e.clone());
                        self.losses.add(
                            "reasoning-sealed-dropped",
                            Some(Kind::Reasoning),
                            "encrypted reasoning from another API is kept as plain reasoning_content",
                        );
                        return;
                    }
                    None => a.reasoning.push(text),
                }
                self.acc.as_mut().unwrap().events.push(e.clone());
            }
            Kind::ToolCall => {
                let a = self.asst(e);
                a.calls.push(e.clone());
                a.events.push(e.clone());
            }
            Kind::ToolResult => {
                self.flush();
                let call_id = e.str("call_id").unwrap_or("").to_string();
                let name = self.call_names.get(&call_id).cloned().unwrap_or_else(|| "tool".into());
                let action_id = self.call_action.get(&call_id).cloned().unwrap_or_else(|| self.id_for(&format!("{call_id}#action")));
                let mut content = Vec::new();
                match e.body.get("output") {
                    Some(Value::String(s)) => content.push(text_content(s)),
                    Some(Value::Array(parts)) => {
                        for p in parts {
                            match p.get("type").and_then(Value::as_str) {
                                Some("text") => content.push(text_content(p.get("text").and_then(Value::as_str).unwrap_or(""))),
                                Some("media") => {
                                    if let Some(d) = p.get("data").and_then(Value::as_str) {
                                        let u = format!("data:{};base64,{d}", p.get("media_type").and_then(Value::as_str).unwrap_or("image/png"));
                                        content.push(json!({"cache_prompt": false, "type": "image", "image_urls": [u]}));
                                    } else if let Some(u) = p.get("url").and_then(Value::as_str) {
                                        content.push(json!({"cache_prompt": false, "type": "image", "image_urls": [u]}));
                                    }
                                }
                                _ => {}
                            }
                        }
                    }
                    Some(x) if !x.is_null() => content.push(text_content(&x.to_string())),
                    _ => {}
                }
                let is_error = e.body.get("is_error").and_then(Value::as_bool).unwrap_or(false);
                let ts = self.ts(e);
                let id = self.id_for(&e.id);
                let v = json!({"id": id, "timestamp": ts, "source": "environment", "tool_name": name, "tool_call_id": call_id,
                    "observation": {"content": content, "is_error": is_error, "tool_name": name, "kind": "MCPToolObservation"},
                    "action_id": action_id, "extended_content": [], "kind": "ObservationEvent"});
                self.push(v, None, vec![e.clone()]);
            }
            Kind::Settings => {
                if let Some(sp) = e.str("system_prompt") {
                    self.flush();
                    let ts = self.ts(e);
                    let id = self.id_for(&e.id);
                    let v = json!({"id": id, "timestamp": ts, "source": "agent", "system_prompt": text_content(sp), "tools": [], "kind": "SystemPromptEvent"});
                    self.push(v, None, vec![e.clone()]);
                } else {
                    // Model and cwd go to base_state.json; the event itself rides along.
                    self.defer(e, "settings-in-base-state", "settings other than the system prompt live in base_state.json, not in events");
                }
            }
            Kind::Compaction if e.str("summary").is_some() => {
                self.flush();
                let ts = self.ts(e);
                let id = self.id_for(&e.id);
                let v =
                    json!({"id": id, "timestamp": ts, "source": "environment", "summary": e.str("summary").unwrap_or(""), "kind": "CondensationSummaryEvent"});
                self.push(v, None, vec![e.clone()]);
            }
            Kind::PeerMessage => {
                // SPEC §3.1.6: wrapped user (incoming) or assistant (outgoing) text.
                let t = peer_wrap(e);
                if e.str("direction") == Some("out") {
                    let a = self.asst(e);
                    a.text.push(t);
                    a.events.push(e.clone());
                } else {
                    self.user_part(e, "user", text_content(&t));
                }
            }
            Kind::Error => {
                self.flush();
                let ts = self.ts(e);
                let id = self.id_for(&e.id);
                let v = json!({"id": id, "timestamp": ts, "source": "environment", "code": e.str("kind").unwrap_or("error"), "detail": e.str("message").unwrap_or(""), "kind": "ConversationErrorEvent"});
                self.push(v, None, vec![e.clone()]);
            }
            Kind::Turn if e.str("phase") == Some("end") && matches!(e.str("outcome"), Some("interrupted" | "aborted")) => {
                self.flush();
                let ts = self.ts(e);
                let id = self.id_for(&e.id);
                let v = json!({"id": id, "timestamp": ts, "source": "user", "kind": "InterruptEvent"});
                self.push(v, None, vec![e.clone()]);
            }
            k => {
                let code = match k {
                    Kind::Compaction => "compaction-boundary-dropped",
                    Kind::Usage => "usage-dropped",
                    Kind::Title => "title-dropped",
                    Kind::Turn => "turn-dropped",
                    Kind::Subagent => "subagent-event-dropped",
                    Kind::FileChange => "file-change-dropped",
                    Kind::App => "app-dropped",
                    _ => "native-dropped",
                };
                self.defer(e, code, "no OpenHands V1 event for this");
            }
        }
    }
}

/// Render a ledger as V1 files: `base_state.json` (when `append` is None) and
/// `events/event-<idx>-<uuid>.json` plus the SDK's lock and length markers.
fn render_v1(l: &Ledger, opts: &ExportOptions, losses: &mut Losses, append: Option<(u64, Option<String>)>) -> Vec<(String, Vec<u8>)> {
    let created = l
        .agent()
        .created_at
        .clone()
        .or_else(|| l.events.iter().find_map(|e| e.ts.clone()))
        .map(|t| oh_ts(&t))
        .unwrap_or_else(|| "1970-01-01T00:00:00.000000".into());
    // SPEC §2.3: an event stream has no branch; the active path is written,
    // and an abandoned event rides on the next record.
    let own = |e: &Event| e.native.as_ref().is_some_and(|n| n.format == FORMAT);
    let on = crate::formats::active_path_only(l, losses, "with carry they ride on the next record", own);
    let mut r = V1Render {
        l,
        losses,
        recs: vec![],
        pending: vec![],
        pending_covers: vec![],
        call_names: HashMap::new(),
        call_action: HashMap::new(),
        acc: None,
        user: None,
        last_ts: created.clone(),
    };
    for (e, on) in l.events.iter().zip(on) {
        if !on && !own(e) {
            // What is open is written first, so the abandoned events ride on
            // the record after them, in their order.
            r.flush();
            r.pending.push(e.clone());
            continue;
        }
        r.render(e);
    }
    r.flush();
    // The SDK refuses to run a conversation whose first three events hold a
    // user message but no system prompt.
    if append.is_none() && !r.recs.iter().take(3).any(|x| x.value["kind"] == "SystemPromptEvent") && !r.recs.is_empty() {
        let id = r.id_for("system-prompt");
        let v = json!({"id": id, "timestamp": created, "source": "agent", "system_prompt": text_content(""), "tools": [], "kind": "SystemPromptEvent"});
        r.recs.insert(0, V1Rec { value: v, carry_at: None, events: vec![], covers: vec![] });
        // It is derived: a carried trip must not import it as history.
        if let Some(x) = r.recs.iter_mut().find(|x| x.carry_at.is_some()) {
            x.covers.push(id);
        }
    }
    // Leftovers ride on the last record that can carry, else in base_state.json.
    let mut state_carry: (Vec<Event>, Vec<String>) = (vec![], vec![]);
    if !r.pending.is_empty() || !r.pending_covers.is_empty() {
        let p = std::mem::take(&mut r.pending);
        let c = std::mem::take(&mut r.pending_covers);
        match r.recs.iter_mut().rev().find(|x| x.carry_at.is_some()) {
            Some(x) => {
                x.events.extend(p);
                x.covers.extend(c);
            }
            None if append.is_none() => state_carry = (p, c),
            None => r.losses.add_n("carry-unplaced", None, p.len() as u64, "no event in this conversation can hold a carrier"),
        }
    }
    if append.is_none() && !r.recs.iter().any(|x| x.carry_at.is_some()) {
        // Records nothing can carry are covered from base_state.json.
        for x in &r.recs {
            if let Some(id) = x.value["id"].as_str() {
                state_carry.1.push(id.to_string());
            }
        }
    }
    let mut files = Vec::new();
    let (start, mut prev) = append.clone().unwrap_or((0, None));
    // The agent rides in base_state.json; appended events need none.
    let agent_carried = true;
    let n = r.recs.len();
    for (i, mut rec) in r.recs.into_iter().enumerate() {
        // parent_id chains each event to the previous one (SDK >= 1.31).
        if let Some(p) = &prev {
            let mut m = Map::new();
            for (k, v) in rec.value.as_object().unwrap() {
                m.insert(k.clone(), v.clone());
                if k == "source" {
                    m.insert("parent_id".into(), json!(p));
                }
            }
            rec.value = Value::Object(m);
        }
        if opts.carry
            && let Some(at) = rec.carry_at
        {
            let path: &[&str] = &[at];
            if rec.events.is_empty() && agent_carried {
                // nothing to carry
            } else {
                carry::attach(&mut rec.value, path, &rec.events);
                if !rec.covers.is_empty() {
                    rec.value[path[0]][carry::KEY]["covers"] = json!(rec.covers);
                }
            }
        }
        let id = rec.value["id"].as_str().unwrap_or("").to_string();
        files.push((format!("events/event-{:05}-{id}.json", start + i as u64), serde_json::to_string(&rec.value).unwrap().into_bytes()));
        prev = Some(id);
    }
    if append.is_none() {
        let model = l.events.iter().rev().find_map(|e| if e.kind == Kind::Settings { e.str("model").map(String::from) } else { None });
        let model = model.or_else(|| l.events.iter().rev().find_map(|e| if e.kind == Kind::Usage { e.str("model").map(String::from) } else { None }));
        let cwd = l.events.iter().find_map(|e| if e.kind == Kind::Settings { e.str("cwd").map(String::from) } else { None });
        let conv = util::uuid_for(&format!("openhands/{}", l.id()));
        let mut bs = json!({"id": conv, "agent": {"llm": {"model": model.unwrap_or_else(|| "unknown".into())}, "tools": [], "kind": "Agent"},
            "workspace": {"working_dir": cwd.unwrap_or_else(|| "/workspace".into()), "kind": "LocalWorkspace"}});
        if opts.carry {
            // ConversationState ignores unknown keys (it drops them when it rewrites the file).
            carry::attach_with(&mut bs, &[], &state_carry.0, Some(l.agent()));
            if !state_carry.1.is_empty() {
                bs[carry::KEY]["covers"] = json!(state_carry.1);
            }
        }
        files.push(("base_state.json".into(), serde_json::to_string(&bs).unwrap().into_bytes()));
        files.push(("events/.eventlog.lock".into(), vec![]));
        files.push((format!("events/.eventlog-len-{n}.marker"), vec![]));
    }
    files
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Found by the mutation harness (seed 0xabcdef0123, openhands/api-tool-calls):
    /// a time with a multi-byte character across byte 19 panicked. Opposed:
    /// zoned and naive times are read as before.
    #[test]
    fn utc_ts_takes_any_text() {
        assert_eq!(
            utc_ts("\u{e9}\u{e9}\u{e9}\u{e9}\u{e9}\u{e9}\u{e9}\u{e9}\u{e9}\u{e9}\u{e9}"),
            "\u{e9}\u{e9}\u{e9}\u{e9}\u{e9}\u{e9}\u{e9}\u{e9}\u{e9}\u{e9}\u{e9}Z"
        );
        assert_eq!(utc_ts("2026-01-01T00:00:00"), "2026-01-01T00:00:00Z");
        assert_eq!(utc_ts("2026-01-01T00:00:00+02:00"), "2026-01-01T00:00:00+02:00");
        assert_eq!(utc_ts("2026-01-01T00:00:00Z"), "2026-01-01T00:00:00Z");
    }

    #[test]
    fn py_dumps_matches_python() {
        // `py_dumps` writes a parsed value, so only spellings every build's
        // `Value` keeps (`2.5`, not `2.50`); a record's own spelling is kept by
        // holding its text (`records_are_held_and_reemitted_as_written`).
        let v: Value = serde_json::from_str(r#"{"a": [1, 2.5, {"b": "é\n\u0001😀"}], "c": {}, "d": []}"#).unwrap();
        assert_eq!(py_dumps(&v, None), "{\"a\": [1, 2.5, {\"b\": \"\\u00e9\\n\\u0001\\ud83d\\ude00\"}], \"c\": {}, \"d\": []}");
        assert_eq!(py_dumps(&json!({"a": [1], "b": {}}), Some(4)), "{\n    \"a\": [\n        1\n    ],\n    \"b\": {}\n}");
    }

    fn v0_stream() -> Vec<Value> {
        vec![
            json!({"id": 0, "timestamp": "2025-10-10T06:10:15.158090", "source": "agent", "message": "sys", "action": "system", "args": {"content": "You are OpenHands", "tools": [], "openhands_version": "0.58.0", "agent_class": "CodeActAgent"}}),
            json!({"id": 1, "timestamp": "2025-10-10T06:10:16", "source": "user", "message": "hi", "action": "message", "args": {"content": "make hello.txt", "image_urls": [], "wait_for_response": false}}),
            json!({"id": 2, "timestamp": "2025-10-10T06:10:17", "source": "agent", "message": "run", "action": "run",
                "tool_call_metadata": {"function_name": "execute_bash", "tool_call_id": "call_1", "model_response": {"id": "resp-1", "model": "gpt", "choices": [{"message": {"content": "", "role": "assistant", "tool_calls": [{"function": {"arguments": "{\"command\": \"touch hello.txt\"}", "name": "execute_bash"}, "id": "call_1", "type": "function"}]}}], "usage": {"prompt_tokens": 10, "completion_tokens": 5}}, "total_calls_in_response": 1},
                "args": {"command": "touch hello.txt", "thought": "Creating it."}}),
            json!({"id": 3, "timestamp": "2025-10-10T06:10:18", "source": "agent", "message": "ok", "cause": 2, "observation": "run", "content": "", "extras": {"command": "touch hello.txt"}, "success": true}),
            json!({"id": 4, "timestamp": "2025-10-10T06:10:19", "source": "environment", "message": "", "observation": "agent_state_changed", "content": "", "extras": {"agent_state": "awaiting_user_input"}}),
        ]
    }

    #[test]
    fn v0_events_map_and_round_trip() {
        let dir = tempfile::tempdir().unwrap();
        let ev = dir.path().join("sessions/abc/events");
        std::fs::create_dir_all(&ev).unwrap();
        for v in v0_stream() {
            std::fs::write(ev.join(format!("{}.json", v["id"])), py_dumps(&v, None)).unwrap();
        }
        let imp = import(dir.path(), &ImportOptions::standard()).unwrap();
        let l = imp.set.get("abc").unwrap();
        assert_eq!(l.agent().source.as_ref().unwrap().version.as_deref(), Some("0.58.0"));
        let kinds: Vec<_> = l.events.iter().map(|e| e.kind.as_str()).collect();
        assert_eq!(kinds, ["settings", "text", "text", "tool_call", "usage", "tool_result", "turn"]);
        assert_eq!(l.events[3].body["input"], json!({"command": "touch hello.txt"}));
        assert_eq!(l.events[5].str("call_id"), Some("call_1"));
        let out = export(&imp.set, &ExportOptions::standard()).unwrap();
        for v in v0_stream() {
            let k = format!("sessions/abc/events/{}.json", v["id"]);
            assert_eq!(String::from_utf8(out.output.files[&k].clone()).unwrap(), py_dumps(&v, None));
        }
    }

    #[test]
    fn foreign_ledger_renders_v1_and_carries_back() {
        let mut l =
            Ledger::new(Agent { id: "f".into(), created_at: Some("2026-01-01T00:00:00Z".into()), source: None, parent: None, links: vec![], meta: Obj::new() });
        l.append(Event::new("u", Kind::Text).with("role", "user").with("text", "hi").ts(Some("2026-01-01T00:00:01Z"))).unwrap();
        l.append(Event::new("t", Kind::Title).with("text", "greeting")).unwrap();
        l.append(Event::new("c", Kind::ToolCall).with("call_id", "k1").with("name", "Bash").with("input", json!({"command": "ls"})).group(Some("g"))).unwrap();
        l.append(Event::new("r", Kind::ToolResult).with("call_id", "k1").with("output", "a\n")).unwrap();
        l.append(Event::new("a", Kind::Text).with("role", "assistant").with("text", "done").group(Some("g2"))).unwrap();
        let set = LedgerSet::single(l);
        let out = export(&set, &ExportOptions::no_carry()).unwrap();
        let files: Vec<&String> = out.output.files.keys().collect();
        assert!(files.iter().any(|f| f.ends_with("/base_state.json")));
        let evs: Vec<Value> = out.output.files.iter().filter(|(k, _)| k.contains("/events/event-")).map(|(_, b)| serde_json::from_slice(b).unwrap()).collect();
        let kinds: Vec<&str> = evs.iter().map(|v| v["kind"].as_str().unwrap()).collect();
        assert_eq!(kinds, ["SystemPromptEvent", "MessageEvent", "ActionEvent", "ObservationEvent", "MessageEvent"]);
        assert_eq!(evs[3]["action_id"], evs[2]["id"]);
        assert_eq!(evs[2]["parent_id"], evs[1]["id"]);
        // With carry the original events come back, the observation once.
        let out = export(&set, &ExportOptions::standard()).unwrap();
        let dir = tempfile::tempdir().unwrap();
        out.output.write_to(dir.path(), true).unwrap();
        let back = import(dir.path(), &ImportOptions::standard()).unwrap();
        let l = back.set.iter().next().unwrap();
        assert_eq!(l.id(), "f");
        let ids: Vec<_> = l.events.iter().filter(|e| e.native.as_ref().is_none_or(|n| n.id.as_deref() != Some("base_state"))).map(|e| e.id.as_str()).collect();
        assert_eq!(ids, ["u", "t", "c", "r", "a"]);
    }

    #[test]
    fn records_are_held_and_reemitted_as_written() {
        use crate::formats::atif::jsonfmt::without_raw;
        use crate::formats::openclaw::jsonl::spelling::{assert_value_loses, held};
        // V0 directory: one event file as Python writes it (", " and ": ",
        // `\u00e9`), plus spellings Python does not write but a file may hold.
        let ev = r#"{"id": 0, "timestamp": "2025-10-10T06:10:16", "source": "user", "message": "hi", "action": "message", "args": {"content": "h\u00e9", "image_urls": [], "wait_for_response": false}, "b": 1, "a": 1.0, "c": 0.000005, "d": 1e3, "f": [1 ,2]}"#;
        assert_value_loses(ev);
        let dir = tempfile::tempdir().unwrap();
        let evdir = dir.path().join("sessions/abc/events");
        std::fs::create_dir_all(&evdir).unwrap();
        std::fs::write(evdir.join("0.json"), ev).unwrap();
        let meta = "{\n  \"title\": \"t\\u00e9\",\n  \"accumulated_cost\": 0.000005,\n  \"b\": 1e3\n}\n";
        std::fs::write(dir.path().join("sessions/abc/metadata.json"), meta).unwrap();
        let set = import(dir.path(), &ImportOptions::standard()).unwrap().set;
        assert_eq!(held(&set, FORMAT), [r#"{"title":"t\u00e9","accumulated_cost":0.000005,"b":1e3}"#, ev], "the ledger holds each file as written");
        for s in [set.clone(), without_raw(&set)] {
            let out = export(&s, &ExportOptions::standard()).unwrap().output.files;
            assert_eq!(std::str::from_utf8(&out["sessions/abc/events/0.json"]).unwrap(), ev);
        }
        let out = export(&set, &ExportOptions::standard()).unwrap().output.files;
        assert_eq!(std::str::from_utf8(&out["sessions/abc/metadata.json"]).unwrap(), meta);
        // A whole-document file: rebuilt from each event's text, laid out again.
        let doc = "[\n  {\n    \"kind\": \"MessageEvent\",\n    \"id\": \"m1\",\n    \"timestamp\": \"2026-07-03T10:00:00\",\n    \"source\": \"user\",\n    \"llm_message\": {\n      \"role\": \"user\",\n      \"content\": [\n        {\n          \"type\": \"text\",\n          \"text\": \"h\\u00e9\"\n        }\n      ]\n    },\n    \"b\": 1,\n    \"a\": 1.0,\n    \"c\": 0.000005,\n    \"d\": 1e3\n  }\n]\n";
        let doc_event = r#"{"kind":"MessageEvent","id":"m1","timestamp":"2026-07-03T10:00:00","source":"user","llm_message":{"role":"user","content":[{"type":"text","text":"h\u00e9"}]},"b":1,"a":1.0,"c":0.000005,"d":1e3}"#;
        assert_value_loses(doc_event);
        let mut losses = Losses::default();
        let (l, _) = import_file(doc, "events.json", &ImportOptions::standard(), &mut losses).unwrap();
        assert!(l.agent().source.as_ref().unwrap().extra.get("raw").is_none(), "the layout is reproduced, not the file kept whole");
        let set = LedgerSet::single(l);
        assert_eq!(held(&set, FORMAT), [doc_event]);
        for s in [set.clone(), without_raw(&set)] {
            let out = export(&s, &ExportOptions::standard()).unwrap().output.files;
            assert_eq!(std::str::from_utf8(&out["events.json"]).unwrap(), doc);
        }
    }

    #[test]
    fn carried_records_keep_their_text() {
        use crate::formats::openclaw::jsonl::spelling::{ODD, assert_value_loses};
        let text = format!("{{{ODD}}}");
        assert_value_loses(&text);
        let mut l =
            Ledger::new(Agent { id: "f".into(), created_at: Some("2026-01-01T00:00:00Z".into()), source: None, parent: None, links: vec![], meta: Obj::new() });
        l.append(
            Event::new("u", Kind::Text)
                .with("role", "user")
                .with("text", "hi")
                .ts(Some("2026-01-01T00:00:01Z"))
                .native(Native::new("x", Some("1".into())).record_raw(&text).unwrap()),
        )
        .unwrap();
        l.append(Event::new("t", Kind::Title).with("text", "greeting").native(Native::new("x", Some("2".into())).record_raw(&text).unwrap())).unwrap();
        let out = export(&LedgerSet::single(l), &ExportOptions::standard()).unwrap();
        let dir = tempfile::tempdir().unwrap();
        out.output.write_to(dir.path(), true).unwrap();
        let back = import(dir.path(), &ImportOptions::standard()).unwrap();
        let l = back.set.iter().next().unwrap();
        // Both ride in "u"'s message event's carrier: "t" renders no event of
        // its own and is deferred onto the user message still open, not into
        // base_state.json (which holds only what no event can carry).
        let bs = out.output.files.iter().find(|(k, _)| k.ends_with("/base_state.json")).map(|(_, b)| std::str::from_utf8(b).unwrap()).unwrap();
        assert!(!bs.contains("greeting"), "the title is not in base_state.json's carrier: {bs}");
        for id in ["u", "t"] {
            let e = l.events.iter().find(|e| e.id == id).unwrap_or_else(|| panic!("{id} restored"));
            assert_eq!(e.native.as_ref().unwrap().record_text(), Some(text.as_str()), "{id}");
        }
    }

    fn cumulative_inputs(set: &LedgerSet) -> Vec<Value> {
        set.iter()
            .flat_map(|l| l.events.iter())
            .filter(|e| e.kind == Kind::Usage && e.body.get("cumulative") == Some(&json!(true)))
            .map(|e| e.body["input"].clone())
            .collect()
    }

    #[test]
    fn cumulative_usage_is_the_first_metrics_snapshot_as_written() {
        // v1-goal-slice writes `default` (the agent's LLM) before `condenser`
        // (all zero); a sorted map would put `condenser` first.
        let dir = concat!(env!("CARGO_MANIFEST_DIR"), "/fixtures/openhands/v1-goal-slice/native");
        let set = import(Path::new(dir), &ImportOptions::standard()).unwrap().set;
        assert_eq!(cumulative_inputs(&set), [json!(596), json!(34780), json!(36938), json!(39265)]);
        // Opposed: a source whose first snapshot is genuinely zero imports as zero.
        let zero = r#"{"id":"s1","timestamp":"2026-06-16T05:39:40.828199","source":"environment","key":"stats","value":{"usage_to_metrics":{"default":{"model_name":"m","accumulated_cost":0.0,"accumulated_token_usage":{"model":"m","prompt_tokens":0,"completion_tokens":0,"cache_read_tokens":0,"cache_write_tokens":0,"reasoning_tokens":0}},"agent":{"model_name":"m","accumulated_cost":1.0,"accumulated_token_usage":{"model":"m","prompt_tokens":9,"completion_tokens":9,"cache_read_tokens":0,"cache_write_tokens":0,"reasoning_tokens":0}}}},"kind":"ConversationStateUpdateEvent"}"#;
        let tmp = tempfile::tempdir().unwrap();
        std::fs::create_dir_all(tmp.path().join("c1/events")).unwrap();
        std::fs::write(tmp.path().join("c1/events/event-00000-s1.json"), zero).unwrap();
        let set = import(tmp.path(), &ImportOptions::standard()).unwrap().set;
        assert_eq!(cumulative_inputs(&set), [json!(0)]);
    }

    #[test]
    fn base_state_carrier_keeps_carried_record_text() {
        use crate::formats::openclaw::jsonl::spelling::{ODD, assert_value_loses};
        // A lone title renders no event file: it can only ride in base_state.json.
        let text = format!("{{{ODD}}}");
        assert_value_loses(&text);
        let mut l =
            Ledger::new(Agent { id: "f".into(), created_at: Some("2026-01-01T00:00:00Z".into()), source: None, parent: None, links: vec![], meta: Obj::new() });
        l.append(Event::new("t", Kind::Title).with("text", "greeting").native(Native::new("x", Some("2".into())).record_raw(&text).unwrap())).unwrap();
        let out = export(&LedgerSet::single(l), &ExportOptions::standard()).unwrap();
        let events: Vec<&String> = out.output.files.keys().filter(|k| k.contains("/events/event-")).collect();
        assert!(events.is_empty(), "no event file could carry it: {events:?}");
        let bs = out.output.files.iter().find(|(k, _)| k.ends_with("/base_state.json")).map(|(_, b)| b).unwrap();
        assert!(std::str::from_utf8(bs).unwrap().contains("\"t\""), "the title rides in base_state.json");
        let dir = tempfile::tempdir().unwrap();
        out.output.write_to(dir.path(), true).unwrap();
        let back = import(dir.path(), &ImportOptions::standard()).unwrap();
        let l = back.set.iter().next().unwrap();
        let e = l.events.iter().find(|e| e.id == "t").expect("t restored from base_state.json");
        assert_eq!(e.native.as_ref().unwrap().record_text(), Some(text.as_str()));
    }

    #[test]
    fn an_edited_record_is_exported_with_its_edit() {
        let doc = "[\n  {\n    \"kind\": \"MessageEvent\",\n    \"id\": \"m1\",\n    \"timestamp\": \"2026-07-03T10:00:00\",\n    \"source\": \"user\",\n    \"llm_message\": {\n      \"role\": \"user\",\n      \"content\": \"h\\u00e9\"\n    },\n    \"d\": 1e3\n  }\n]\n";
        let mut losses = Losses::default();
        let (l, _) = import_file(doc, "events.json", &ImportOptions::standard(), &mut losses).unwrap();
        let mut set = LedgerSet::single(l.clone());
        // Opposed: unchanged, the file comes back byte for byte (from `raw` where it is kept).
        let out = export(&set, &ExportOptions::standard()).unwrap().output.files;
        assert_eq!(std::str::from_utf8(&out["events.json"]).unwrap(), doc);
        // Opposed, and only `raw` can pass it: an unchanged record a carrier
        // brought back re-spelled (`\u00e9` as `é`) beside its source text.
        let mut respelled = l.clone();
        let n = respelled.events[0].native.as_mut().unwrap();
        let source = n.record_text().unwrap().to_string();
        n.set_record(n.record_json());
        n.raw = Some(source.clone());
        assert_ne!(n.record_text(), Some(source.as_str()), "sentinel: the record no longer spells the source");
        let out = export(&LedgerSet::single(respelled), &ExportOptions::standard()).unwrap().output.files;
        assert_eq!(std::str::from_utf8(&out["events.json"]).unwrap(), doc, "an unchanged record is written as its raw");
        // Edited in place, with the record's stale `raw` kept beside it.
        let mut l = l;
        let n = l.events[0].native.as_mut().unwrap();
        n.raw = Some(
            r#"{"kind":"MessageEvent","id":"m1","timestamp":"2026-07-03T10:00:00","source":"user","llm_message":{"role":"user","content":"h\u00e9"},"d":1e3}"#
                .into(),
        );
        let edited = n.record_text().unwrap().replace("h\\u00e9", "EDITED");
        *n = n.clone().record_raw(&edited).unwrap();
        set.insert(l);
        let out = export(&set, &ExportOptions::standard()).unwrap().output.files;
        let got = std::str::from_utf8(&out["events.json"]).unwrap();
        assert!(got.contains("\"EDITED\""), "the edit is exported: {got}");
        assert!(!got.contains("u00e9"), "the stale raw is not: {got}");
    }
}
