//! Nous Research Hermes Agent sessions.
//!
//! The live store is `~/.hermes/state.db` (SQLite, `sessions` + `messages`,
//! schema v4-v31); the legacy per-session files (`sessions/session_<id>.json`
//! from the CLI and `sessions/<id>.jsonl` from the gateway, both removed in
//! v2026.5.20) are imported and reproduced exactly. Foreign ledgers export to
//! a `state.db`. See `docs/formats/hermes.md`.

use crate::util::OrderedMap;
#[cfg(feature = "sqlite")]
pub(crate) mod db;
#[cfg_attr(not(feature = "sqlite"), allow(dead_code))] // used by the SQLite-backed paths only
pub(crate) mod pyjson;

use crate::formats::atif::jsonfmt;
use crate::formats::openclaw::jsonl;
use crate::formats::{Detection, ExportOptions, Exported, Format, ImportOptions, Imported, Losses, Output, Variant};
use crate::ledger::{Ledger, LedgerSet};
use crate::model::{Agent, Audience, Event, Kind, Link, Native, Obj, Parent, Relation, Source};
use crate::util;
use anyhow::{Context, Result, bail};
use serde_json::{Value, json};
use std::path::{Path, PathBuf};

pub(crate) const FORMAT: &str = "hermes";
/// Prefix hermes puts on multimodal (list) content stored in a TEXT column.
pub(crate) const JSON_PREFIX: &str = "\u{0}json:";

pub struct Hermes;

static VARIANTS: &[Variant] = &[
    Variant {
        name: "state-db",
        versions: "hermes v2026.3.12 (schema 4) - v0.22.x (schema 31)",
        import: true,
        export: true,
        notes: "~/.hermes/state.db; the only store resume reads. Export target",
    },
    Variant {
        name: "session-json",
        versions: "hermes <= v2026.5.16",
        import: true,
        export: false,
        notes: "CLI sessions/session_<id>.json (pretty JSON); reproduced exactly, foreign ledgers go to state.db",
    },
    Variant {
        name: "gateway-jsonl",
        versions: "hermes <= v2026.5.16",
        import: true,
        export: false,
        notes: "gateway sessions/<id>.jsonl; reproduced exactly, foreign ledgers go to state.db",
    },
];

impl Format for Hermes {
    fn name(&self) -> &'static str {
        FORMAT
    }
    fn describe(&self) -> &'static str {
        "Hermes Agent sessions (~/.hermes/state.db; legacy sessions/*.json, *.jsonl)"
    }
    fn variants(&self) -> &'static [Variant] {
        VARIANTS
    }
    fn detect(&self, input: &Path) -> Option<Detection> {
        let files: Vec<PathBuf> = if input.is_file() { vec![input.to_path_buf()] } else { source_files(input) };
        files.iter().find_map(|p| detect_file(p))
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

#[derive(Clone, Copy, PartialEq, Debug)]
enum FileKind {
    Db,
    SessionJson,
    GatewayJsonl,
}

fn kind_of(p: &Path) -> Option<FileKind> {
    let name = p.file_name()?.to_string_lossy().to_string();
    let mut head = vec![0u8; 16];
    {
        use std::io::Read;
        let mut f = std::fs::File::open(p).ok()?;
        let n = f.read(&mut head).ok()?;
        head.truncate(n);
    }
    if head.starts_with(b"SQLite format 3\0") {
        return Some(FileKind::Db);
    }
    if name.ends_with(".json") {
        let v: Value = serde_json::from_str(&std::fs::read_to_string(p).ok()?).ok()?;
        if v.get("session_id").is_some() && v.get("messages").is_some_and(Value::is_array) {
            return Some(FileKind::SessionJson);
        }
    }
    if name.ends_with(".jsonl") {
        let text = std::fs::read_to_string(p).ok()?;
        let first: Value = serde_json::from_str(text.lines().find(|l| !l.trim().is_empty())?).ok()?;
        if first.get("role").is_some_and(Value::is_string) && first.get("type").is_none() {
            return Some(FileKind::GatewayJsonl);
        }
    }
    None
}

fn detect_file(p: &Path) -> Option<Detection> {
    match kind_of(p)? {
        FileKind::Db => {
            #[cfg(feature = "sqlite")]
            {
                let v = db::probe(p)?;
                Some(Detection { variant: "state-db", confidence: 95, version: Some(format!("schema-v{v}")) })
            }
            #[cfg(not(feature = "sqlite"))]
            None
        }
        FileKind::SessionJson => Some(Detection { variant: "session-json", confidence: 85, version: None }),
        FileKind::GatewayJsonl => Some(Detection { variant: "gateway-jsonl", confidence: 60, version: None }),
    }
}

/// Native files under a hermes home (or any directory).
fn source_files(dir: &Path) -> Vec<PathBuf> {
    let mut v: Vec<PathBuf> = walkdir::WalkDir::new(dir)
        .sort_by_file_name()
        .into_iter()
        .filter_map(|e| e.ok())
        .filter(|e| e.file_type().is_file())
        .map(|e| e.path().to_path_buf())
        .filter(|p| !p.file_name().is_some_and(|n| n.to_string_lossy().starts_with('.')))
        .filter(|p| kind_of(p).is_some())
        .collect();
    v.sort();
    v
}

// ---------------------------------------------------------------- import

fn import(input: &Path, opts: &ImportOptions) -> Result<Imported> {
    let (root, files) = if input.is_file() {
        (input.parent().unwrap_or(Path::new(".")).to_path_buf(), vec![input.to_path_buf()])
    } else {
        (input.to_path_buf(), source_files(input))
    };
    if files.is_empty() {
        bail!("{}: no hermes state.db or session files", input.display());
    }
    let mut set = LedgerSet::new();
    let mut losses = Losses::default();
    let mut variant = None;
    for p in &files {
        let rel = p.strip_prefix(&root).unwrap_or(p).to_string_lossy().replace('\\', "/");
        // A file named on the command line is read whatever it holds: one that
        // is none of hermes's kinds (or no longer is) is an error, not a panic.
        let Some(kind) = kind_of(p) else {
            bail!("{}: not a hermes state.db, session JSON or gateway JSONL file", p.display());
        };
        let ledgers = match kind {
            FileKind::Db => {
                #[cfg(feature = "sqlite")]
                {
                    variant.get_or_insert("state-db");
                    db::import(p, &rel, opts, &mut losses)?
                }
                #[cfg(not(feature = "sqlite"))]
                {
                    let _ = opts;
                    bail!("{}: reading state.db needs the `sqlite` feature", p.display())
                }
            }
            FileKind::SessionJson => {
                variant.get_or_insert("session-json");
                vec![import_session_json(&std::fs::read_to_string(p)?, &rel).with_context(|| p.display().to_string())?]
            }
            FileKind::GatewayJsonl => {
                variant.get_or_insert("gateway-jsonl");
                vec![import_gateway_jsonl(&std::fs::read_to_string(p)?, &rel, &mut losses)]
            }
        };
        for mut l in ledgers {
            // The same session can sit in state.db and in a legacy file.
            if set.get(l.id()).is_some() {
                let id = format!("{}~{}", l.id(), rel);
                l.agent_mut().id = id;
            }
            set.insert(l);
        }
    }
    link_sessions(&mut set);
    Ok(Imported { set, losses, variant: variant.unwrap_or("state-db").to_string() })
}

/// `parent_session_id` + the child's `model_config` markers and `source`
/// decide the relation (hermes_state_sessions.py): a delegate/subagent/tool
/// child is a subagent, `_branched_from` a fork, anything else (compression,
/// reset) a continuation.
fn link_sessions(set: &mut LedgerSet) {
    let ids: Vec<String> = set.ledgers.keys().cloned().collect();
    let mut continued_in: Vec<(String, String)> = Vec::new();
    for id in &ids {
        let l = set.ledgers.get_mut(id).unwrap();
        let Some(extra) = l.agent().source.as_ref().map(|s| s.extra.clone()) else { continue };
        let Some(parent) = extra.get("parent_session_id").and_then(Value::as_str).map(str::to_string) else { continue };
        if l.agent().parent.is_some() || !l.agent().links.is_empty() {
            continue; // restored from a carrier
        }
        let mc = extra.get("model_config").cloned().unwrap_or(Value::Null);
        let source = extra.get("session_source").and_then(Value::as_str).unwrap_or("");
        let rel = if mc.get("_delegate_from").is_some() || matches!(source, "subagent" | "tool") {
            Some(Relation::Subagent)
        } else if mc.get("_branched_from").is_some() {
            Some(Relation::Fork)
        } else {
            None
        };
        match rel {
            Some(r) => l.agent_mut().parent = Some(Parent { agent: parent, event: None, relation: r }),
            None => {
                l.agent_mut().links.push(Link { rel: "continued-from".into(), agent: Some(parent.clone()), event: None, extra: Obj::new() });
                continued_in.push((parent, id.clone()));
            }
        }
    }
    for (parent, child) in continued_in {
        if let Some(p) = set.ledgers.get_mut(&parent)
            && !p.agent().links.iter().any(|k| k.rel == "continued-in" && k.agent.as_deref() == Some(&child))
        {
            p.agent_mut().links.push(Link { rel: "continued-in".into(), agent: Some(child), event: None, extra: Obj::new() });
        }
    }
}

/// A hermes timestamp: epoch seconds (REAL) or an ISO string.
pub(crate) fn ts_of(v: Option<&Value>) -> Option<String> {
    match v? {
        Value::Number(n) => n.as_f64().map(|s| util::ms_to_rfc3339((s * 1000.0).round() as i64)),
        Value::String(s) if !s.is_empty() => Some(s.clone()),
        _ => None,
    }
}

fn str_at<'a>(v: &'a Value, k: &str) -> Option<&'a str> {
    v.get(k).and_then(Value::as_str)
}

/// A JSON column: stored as text in state.db, as a value in legacy files.
pub(crate) fn json_col(v: Option<&Value>) -> Option<Value> {
    match v? {
        Value::Null => None,
        Value::String(s) => serde_json::from_str(s).ok(),
        other => Some(other.clone()),
    }
}

/// Decoded content: a string, or the parts of multimodal content.
fn content_of(v: Option<&Value>) -> Option<Value> {
    match v? {
        Value::String(s) => match s.strip_prefix(JSON_PREFIX) {
            Some(j) => serde_json::from_str(j).ok().or_else(|| Some(Value::String(s.clone()))),
            None => Some(Value::String(s.clone())),
        },
        Value::Null => None,
        other => Some(other.clone()),
    }
}

fn parts_events(c: &Value, role: &str, mk: &mut dyn FnMut(Kind) -> Event, out: &mut Vec<Event>) {
    match c {
        Value::String(s) => out.push(mk(Kind::Text).with("role", role).with("text", s.as_str())),
        Value::Array(parts) => {
            for p in parts {
                match str_at(p, "type") {
                    Some("text") | Some("input_text") => {
                        out.push(mk(Kind::Text).with("role", role).with("text", p.get("text").cloned().unwrap_or(Value::Null)))
                    }
                    Some("image_url") | Some("input_image") => {
                        let url = p.pointer("/image_url/url").or(p.get("image_url")).and_then(Value::as_str).unwrap_or("");
                        let e = match url.strip_prefix("data:").and_then(|r| r.split_once(";base64,")) {
                            Some((mt, data)) => mk(Kind::Media).with("role", role).with("media_type", mt).with("data", data),
                            None => mk(Kind::Media).with("role", role).with("url", url),
                        };
                        out.push(e);
                    }
                    _ => {}
                }
            }
        }
        _ => {}
    }
}

/// SPEC §3.1.5: the vendor an OpenRouter-style `reasoning_details` `format`
/// (`anthropic-claude-v1`, `openai-responses-v1`, `google-gemini-v1`) names.
pub(crate) fn vendor_of_format(f: &str) -> &str {
    ["anthropic", "openai", "google"].into_iter().find(|v| f.starts_with(v)).unwrap_or(f)
}

/// The `reasoning_details` `format` for a vendor (the inverse of [`vendor_of_format`]).
#[cfg_attr(not(feature = "sqlite"), allow(dead_code))] // used by the SQLite-backed paths only
pub(crate) fn format_of_vendor(v: &str) -> &str {
    match v {
        "anthropic" => "anthropic-claude-v1",
        "openai" => "openai-responses-v1",
        "google" => "google-gemini-v1",
        other => other,
    }
}

/// The neutral events of one hermes message (a state.db row or a legacy
/// message dict). `id` is the event id of part 0.
pub(crate) fn message_events(m: &Value, id: &str, model: Option<&str>) -> Vec<Event> {
    let ts = ts_of(m.get("timestamp"));
    let mut n = 0usize;
    let mut mk = |kind: Kind| {
        let eid = if n == 0 { id.to_string() } else { format!("{id}#{n}") };
        n += 1;
        Event::new(eid, kind).ts(ts.clone())
    };
    let mut out = Vec::new();
    let role = str_at(m, "role").unwrap_or("");
    let content = content_of(m.get("content"));
    match role {
        "user" | "system" => {
            if m.get("_compressed_summary").is_some_and(|v| v.as_i64() == Some(1) || v.as_bool() == Some(true)) {
                let text = content.as_ref().and_then(Value::as_str).unwrap_or("").to_string();
                out.push(mk(Kind::Compaction).with("phase", "summary").with("trigger", "auto").with("summary", text));
            } else if let Some(c) = &content {
                parts_events(c, role, &mut mk, &mut out);
            }
        }
        "assistant" => {
            let details = json_col(m.get("reasoning_details"));
            let text = str_at(m, "reasoning").or(str_at(m, "reasoning_content")).map(str::to_string);
            let mut sealed = None;
            let mut summary: Vec<String> = Vec::new();
            if let Some(Value::Array(ds)) = &details {
                for d in ds {
                    // SPEC §3.1.5: the vendor, not OpenRouter's format tag.
                    let vendor = vendor_of_format(str_at(d, "format").unwrap_or("unknown"));
                    match str_at(d, "type") {
                        Some("reasoning.text") if sealed.is_none() => {
                            if let Some(sig) = str_at(d, "signature").filter(|s| !s.is_empty()) {
                                sealed = Some(json!({"vendor": vendor, "kind": "signature", "data": sig}));
                            }
                        }
                        Some("reasoning.encrypted") if sealed.is_none() => {
                            sealed = Some(json!({"vendor": vendor, "kind": "encrypted", "data": d.get("data")}));
                        }
                        Some("reasoning.summary") => {
                            if let Some(s) = str_at(d, "summary") {
                                summary.push(s.to_string());
                            }
                        }
                        _ => {}
                    }
                }
            }
            if sealed.is_none()
                && let Some(Value::Array(items)) = json_col(m.get("codex_reasoning_items"))
                && let Some(enc) = items.iter().find_map(|i| str_at(i, "encrypted_content"))
            {
                sealed = Some(json!({"vendor": "openai", "kind": "encrypted", "data": enc}));
            }
            if text.is_some() || sealed.is_some() || !summary.is_empty() {
                let mut e = mk(Kind::Reasoning);
                if let Some(t) = &text {
                    e = e.with("text", t.as_str());
                }
                if !summary.is_empty() {
                    e = e.with("summary", json!(summary));
                }
                if let Some(s) = sealed {
                    e = e.with("sealed", s);
                }
                out.push(e);
            }
            match &content {
                Some(Value::String(s)) if !s.is_empty() => out.push(mk(Kind::Text).with("role", "assistant").with("text", s.as_str())),
                Some(c @ Value::Array(_)) => parts_events(c, "assistant", &mut mk, &mut out),
                _ => {}
            }
            if let Some(Value::Array(calls)) = json_col(m.get("tool_calls")) {
                for c in calls {
                    let f = c.get("function").unwrap_or(&c);
                    let mut e = mk(Kind::ToolCall)
                        .with("call_id", c.get("id").cloned().unwrap_or(Value::Null))
                        .with("name", f.get("name").cloned().unwrap_or(Value::Null))
                        .with("tool_type", "function");
                    match f.get("arguments") {
                        Some(Value::String(a)) => match serde_json::from_str::<Value>(a) {
                            Ok(v @ Value::Object(_)) => e = e.with("input", v),
                            _ => e = e.with("input_text", a.as_str()),
                        },
                        Some(v) => e = e.with("input", v.clone()),
                        None => e = e.with("input", json!({})),
                    }
                    out.push(e);
                }
            }
            if let Some(t) = m.get("token_count").and_then(Value::as_u64) {
                out.push(mk(Kind::Usage).with("model", model.map(str::to_string)).with("output", t));
            }
            for e in out.iter_mut() {
                e.group = Some(id.to_string());
            }
        }
        "tool" => {
            let output = match &content {
                Some(Value::String(s)) => json!(s),
                Some(Value::Array(parts)) => Value::Array(
                    parts
                        .iter()
                        .filter_map(|p| match str_at(p, "type") {
                            Some("text") => Some(json!({"type":"text","text": p.get("text")})),
                            _ => None,
                        })
                        .collect(),
                ),
                _ => json!(""),
            };
            out.push(mk(Kind::ToolResult).with("call_id", m.get("tool_call_id").cloned().unwrap_or(Value::Null)).with("output", output));
        }
        "session_meta" => {
            let tools: Vec<Value> =
                m.get("tools").and_then(Value::as_array).map(|a| a.iter().filter_map(|t| t.pointer("/function/name").cloned()).collect()).unwrap_or_default();
            out.push(mk(Kind::Settings).with("model", m.get("model").cloned().unwrap_or(Value::Null)).with("tools", json!(tools)));
        }
        _ => {}
    }
    if out.is_empty() {
        out.push(mk(Kind::Native));
    }
    // Rewound or compacted-away rows are no longer in the model's context.
    if m.get("active").and_then(Value::as_i64) == Some(0) {
        for e in out.iter_mut() {
            e.audience = Some(Audience::Record);
        }
    }
    out
}

/// Settings (+ title, + cumulative usage) from a session row or a legacy header.
pub(crate) fn session_events(s: &Value, id: &str, system_prompt: Option<&str>) -> Vec<Event> {
    let mut out = Vec::new();
    let mc = json_col(s.get("model_config")).unwrap_or(Value::Null);
    let tools: Option<Vec<Value>> = s.get("tools").and_then(Value::as_array).map(|a| a.iter().filter_map(|t| t.pointer("/function/name").cloned()).collect());
    let mut e = Event::new(id, Kind::Settings)
        .ts(ts_of(s.get("started_at")).or(ts_of(s.get("session_start"))))
        .with("model", s.get("model").cloned().unwrap_or(Value::Null))
        .with("provider", s.get("billing_provider").cloned().unwrap_or(Value::Null))
        .with("cwd", s.get("cwd").cloned().unwrap_or(Value::Null))
        .with("git_branch", s.get("git_branch").cloned().unwrap_or(Value::Null))
        .with("effort", mc.get("reasoning_effort").cloned().unwrap_or(Value::Null))
        .with("system_prompt", system_prompt.map(str::to_string).filter(|p| !p.is_empty()));
    if let Some(t) = tools {
        e = e.with("tools", json!(t));
    }
    out.push(e);
    if let Some(t) = str_at(s, "title").filter(|t| !t.is_empty()) {
        out.push(Event::new(format!("{id}#{}", out.len()), Kind::Title).with("text", t));
    }
    let n = |k: &str| s.get(k).and_then(Value::as_u64).unwrap_or(0);
    let total = n("input_tokens") + n("output_tokens") + n("cache_read_tokens") + n("cache_write_tokens") + n("reasoning_tokens");
    if total > 0 {
        let cost = s.get("actual_cost_usd").filter(|v| !v.is_null()).or(s.get("estimated_cost_usd")).cloned().unwrap_or(Value::Null);
        out.push(
            Event::new(format!("{id}#{}", out.len()), Kind::Usage)
                .ts(ts_of(s.get("ended_at")))
                .with("model", s.get("model").cloned().unwrap_or(Value::Null))
                .with("input", n("input_tokens"))
                .with("output", n("output_tokens"))
                .with("cache_read", n("cache_read_tokens"))
                .with("cache_write", n("cache_write_tokens"))
                .with("reasoning", n("reasoning_tokens"))
                .with("cost_usd", cost)
                .with("cumulative", true),
        );
    }
    out
}

fn agent_for(id: &str, created: Option<String>, variant: &str, version: Option<String>, extra: Obj) -> Agent {
    Agent {
        id: id.to_string(),
        created_at: created,
        source: Some(Source { format: FORMAT.into(), variant: Some(variant.into()), version, native_id: Some(id.to_string()), extra }),
        parent: None,
        links: vec![],
        meta: Obj::new(),
    }
}

// ---- legacy files

fn import_session_json(text: &str, rel: &str) -> Result<Ledger> {
    let v: Value = serde_json::from_str(text)?;
    let sid = str_at(&v, "session_id")
        .map(str::to_string)
        .unwrap_or_else(|| rel.rsplit('/').next().unwrap_or(rel).trim_start_matches("session_").trim_end_matches(".json").to_string());
    let mut extra = Obj::new();
    extra.insert("file".into(), json!(rel));
    let mut l = Ledger::new(agent_for(&sid, ts_of(v.get("session_start")), "session-json", None, extra));
    let mut header = v.clone();
    if let Value::Object(m) = &mut header {
        m.remove_ordered("messages");
    }
    // Each record is held as its own text cut out of the file's (SPEC §4): the
    // header as the file without `messages`, each message as its item; the
    // file itself is kept as the header's `raw` and written back from there.
    let compact = jsonfmt::minify(text);
    let header_text = without_member(&compact, "messages");
    let message_texts: Vec<&str> = jsonfmt::member(&compact, "messages").and_then(jsonfmt::items).unwrap_or_default();
    let mut n = Native::new(FORMAT, Some(sid.clone())).variant(Some("session-json")).file(Some(rel));
    jsonfmt::hold(&mut n, header_text.as_deref(), &header);
    n.raw = Some(text.to_string());
    let model = str_at(&v, "model").map(str::to_string);
    for (i, e) in session_events(&header, &sid, str_at(&v, "system_prompt")).into_iter().enumerate() {
        l.append_unique(e.native(if i == 0 { n.clone() } else { n.part_of(i as u32) }));
    }
    let messages = v.get("messages").and_then(Value::as_array).cloned().unwrap_or_default();
    for (i, m) in messages.iter().enumerate() {
        let nid = format!("{sid}/{i}");
        let mut mn = Native::new(FORMAT, Some(nid.clone())).variant(Some("session-json")).file(Some(rel));
        let mt = (message_texts.len() == messages.len()).then(|| message_texts[i]);
        jsonfmt::hold(&mut mn, mt, m);
        for (j, e) in message_events(m, &nid, model.as_deref()).into_iter().enumerate() {
            l.append_unique(e.native(if j == 0 { mn.clone() } else { mn.part_of(j as u32) }));
        }
    }
    Ok(l)
}

/// Whether a session file's text `raw` is its header record (`header`, the
/// file without `messages`) and its message records (`messages`, in order),
/// as [`crate::util::same_json`] compares them.
fn file_matches_records(raw: &str, header: Option<&str>, messages: &[Option<String>]) -> bool {
    use crate::util::same_json;
    let compact = jsonfmt::minify(raw);
    let head = without_member(&compact, "messages").or_else(|| {
        let mut v: Value = serde_json::from_str(raw).ok()?;
        v.as_object_mut()?.remove_ordered("messages");
        serde_json::to_string(&v).ok()
    });
    let head_same = matches!((head.as_deref(), header), (Some(a), Some(b)) if same_json(a, b));
    let items = jsonfmt::member(&compact, "messages").and_then(jsonfmt::items).unwrap_or_default();
    head_same && items.len() == messages.len() && items.iter().zip(messages).all(|(a, b)| b.as_deref().is_some_and(|b| same_json(a, b)))
}

/// A compact object's text without member `key` (every other byte as it was).
fn without_member(compact: &str, key: &str) -> Option<String> {
    let ms = jsonfmt::members(compact)?;
    let pos = ms.iter().position(|(k, _)| k == key)?;
    let off = |v: &str| v.as_ptr() as usize - compact.as_ptr() as usize;
    // A member runs from its key (just past the previous value's comma) to the end of its value.
    let start = if pos == 0 { 1 } else { off(ms[pos - 1].1) + ms[pos - 1].1.len() + 1 };
    let end = off(ms[pos].1) + ms[pos].1.len();
    Some(match (pos == 0, pos + 1 == ms.len()) {
        (_, false) => format!("{}{}", &compact[..start], &compact[end + 1..]),
        (true, true) => "{}".to_string(),
        (false, true) => format!("{}{}", &compact[..start - 1], &compact[end..]),
    })
}

fn import_gateway_jsonl(text: &str, rel: &str, losses: &mut Losses) -> Ledger {
    let sid = rel.rsplit('/').next().unwrap_or(rel).trim_end_matches(".jsonl").to_string();
    let lines = jsonl::lines(text);
    let created = lines.iter().find_map(|l| ts_of(l.value.as_ref().and_then(|v| v.get("timestamp"))));
    let mut extra = Obj::new();
    extra.insert("file".into(), json!(rel));
    if !text.is_empty() && !text.ends_with('\n') {
        extra.insert("final_newline".into(), json!(false));
    }
    let mut l = Ledger::new(agent_for(&sid, created, "gateway-jsonl", None, extra));
    let mut model: Option<String> = None;
    for line in &lines {
        let id = format!("@{}", line.off);
        let n = jsonl::native_of(FORMAT, "gateway-jsonl", None, &id, line, Some(rel));
        let Some(v) = &line.value else {
            losses.add("invalid-json-line", Some(Kind::Native), "a line that is not JSON is kept only verbatim");
            l.append_unique(Event::new(id, Kind::Native).native(n));
            continue;
        };
        if str_at(v, "role") == Some("session_meta") {
            model = str_at(v, "model").map(str::to_string);
        }
        for (j, e) in message_events(v, &id, model.as_deref()).into_iter().enumerate() {
            l.append_unique(e.native(if j == 0 { n.clone() } else { n.part_of(j as u32) }));
        }
    }
    l
}

// ---------------------------------------------------------------- export

/// The legacy file a hermes-native ledger came from, when every one of its
/// records is still intact (so the file can be written back verbatim).
fn legacy_intact(l: &Ledger) -> Option<(String, String)> {
    let src = l.agent().source.as_ref().filter(|s| s.format == FORMAT)?;
    let variant = src.variant.as_deref()?;
    let file = src.extra.get("file")?.as_str()?.to_string();
    let counts = jsonl::part_counts(l, FORMAT);
    match variant {
        "session-json" => {
            let mut raw = None;
            let mut ok = true;
            // The records' own texts, to check the file against.
            let mut header_text: Option<String> = None;
            let mut message_texts: Vec<Option<String>> = Vec::new();
            for (i, e) in l.events.iter().enumerate() {
                let Some(n) = e.native.as_ref().filter(|n| n.format == FORMAT && n.part == 0) else {
                    if e.native.as_ref().is_none_or(|n| n.format != FORMAT) {
                        ok = false;
                    }
                    continue;
                };
                let id = n.id.clone().unwrap_or_default();
                // The header record (native id = the session id) keeps the whole file as `raw`.
                let header = n.id.is_some() && n.id == src.native_id;
                let want = match (&n.raw, &n.record_json()) {
                    (Some(r), Some(h)) if header => {
                        raw = Some(r.clone());
                        header_text = n.record_text().map(str::to_string);
                        session_events(h, &id, str_at(h, "system_prompt")).len()
                    }
                    (_, Some(m)) if !header => {
                        message_texts.push(n.record_text().map(str::to_string));
                        message_events(m, &id, None).len()
                    }
                    _ => 1,
                };
                ok &= counts[i] == want;
            }
            // The file is written back whole only while it still says what its
            // records say (an edited record is exported anew).
            let raw = raw.filter(|r| file_matches_records(r, header_text.as_deref(), &message_texts));
            if ok { raw.map(|r| (file, r)) } else { None }
        }
        "gateway-jsonl" => {
            let mut text = String::new();
            for (i, e) in l.events.iter().enumerate() {
                match e.native.as_ref().filter(|n| n.format == FORMAT) {
                    Some(n) if n.part == 0 => {
                        let want = n.record_json().as_ref().map(|r| message_events(r, "x", None).len()).unwrap_or(1);
                        if counts[i] != want {
                            return None;
                        }
                        text.push_str(&jsonl::line_of(n)?);
                        text.push('\n');
                    }
                    Some(_) => {}
                    None => return None,
                }
            }
            if src.extra.get("final_newline").and_then(Value::as_bool) == Some(false) {
                text.pop();
            }
            Some((file, text))
        }
        _ => None,
    }
}

fn export(set: &LedgerSet, opts: &ExportOptions) -> Result<Exported> {
    let mut output = Output::default();
    #[cfg_attr(not(feature = "sqlite"), allow(unused_mut))] // written by db::export only
    let mut losses = Losses::default();
    let mut to_db = LedgerSet::new();
    for l in set.iter() {
        match legacy_intact(l) {
            Some((file, text)) => output.put(file, text.into_bytes()),
            None => to_db.insert(l.clone()),
        }
    }
    if !to_db.is_empty() {
        #[cfg(feature = "sqlite")]
        db::export(&to_db, set, opts, &mut output, &mut losses)?;
        #[cfg(not(feature = "sqlite"))]
        {
            let _ = opts;
            bail!("hermes: writing state.db needs the `sqlite` feature");
        }
    }
    Ok(Exported { output, losses: losses.sorted(), variant: "state-db".into() })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn maps_db_style_rows() {
        let row = json!({"id": 2, "role": "assistant", "content": null,
            "tool_calls": "[{\"id\": \"c1\", \"type\": \"function\", \"function\": {\"name\": \"terminal\", \"arguments\": \"{\\\"command\\\": \\\"ls\\\"}\"}}]",
            "timestamp": 1792152002.0, "token_count": 12, "reasoning": "Run ls.",
            "reasoning_details": "[{\"type\": \"reasoning.text\", \"text\": \"Run ls.\", \"signature\": \"sig\", \"format\": \"anthropic-claude-v1\", \"index\": 0}]"});
        let evs = message_events(&row, "2", Some("m"));
        let kinds: Vec<Kind> = evs.iter().map(|e| e.kind).collect();
        assert_eq!(kinds, vec![Kind::Reasoning, Kind::ToolCall, Kind::Usage]);
        assert_eq!(evs[1].body["input"]["command"], "ls");
        assert_eq!(evs[0].body["sealed"]["data"], "sig");
        assert_eq!(evs[0].ts.as_deref(), Some("2026-10-16T12:00:02.000Z"));
        let mm = json!({"role":"user","content": format!("{JSON_PREFIX}[{{\"type\": \"text\", \"text\": \"hi\"}}, {{\"type\": \"image_url\", \"image_url\": {{\"url\": \"data:image/png;base64,AAAA\"}}}}]")});
        let evs = message_events(&mm, "6", None);
        assert_eq!(evs[1].body["media_type"], "image/png");
    }

    #[test]
    fn legacy_records_are_held_and_reemitted_as_written() {
        use crate::formats::atif::jsonfmt::without_raw;
        use crate::formats::openclaw::jsonl::spelling::{ODD, assert_value_loses, held};
        // gateway-jsonl: each line as written.
        let line = format!(r#"{{"role":"user","content":"hi","timestamp":"2026-10-16T12:00:00",{ODD}}}"#);
        assert_value_loses(&line);
        let src = format!("{line}\n");
        let mut losses = Losses::default();
        let set = LedgerSet::single(import_gateway_jsonl(&src, "s1.jsonl", &mut losses));
        assert_eq!(held(&set, FORMAT), [line.as_str()], "the ledger holds the line as written");
        for s in [set.clone(), without_raw(&set)] {
            let out = export(&s, &ExportOptions::standard()).unwrap();
            assert_eq!(String::from_utf8(out.output.files["s1.jsonl"].clone()).unwrap(), src);
        }
    }

    #[test]
    fn session_json_records_are_held_and_reemitted_as_written() {
        use crate::formats::openclaw::jsonl::spelling::{assert_value_loses, held};
        // The header (the file without its messages) and each message, held
        // as cut out of the file; the file is written back whole. Both carry
        // a spelling a `Value` changes under every build (`\u00e9`).
        let msg = "{\n      \"role\": \"user\",\n      \"content\": \"hi\",\n      \"b\": 1,\n      \"a\": 1.0,\n      \"c\": 0.000005,\n      \"d\": 1e3,\n      \"g\": \"\\u00e9\"\n    }";
        let file = format!(
            "{{\n  \"session_id\": \"s2\",\n  \"model\": \"m\",\n  \"messages\": [\n    {msg}\n  ],\n  \"z\": 1.0,\n  \"y\": 1e3,\n  \"t\": \"\\u00e9\"\n}}"
        );
        let header = r#"{"session_id":"s2","model":"m","z":1.0,"y":1e3,"t":"\u00e9"}"#;
        let message = r#"{"role":"user","content":"hi","b":1,"a":1.0,"c":0.000005,"d":1e3,"g":"\u00e9"}"#;
        assert_value_loses(header);
        assert_value_loses(message);
        let set = LedgerSet::single(import_session_json(&file, "session_s2.json").unwrap());
        let got = held(&set, FORMAT);
        assert_eq!(got[0], header, "the ledger holds the header as cut from the file");
        assert_eq!(got[1..], [message], "the ledger holds the message as cut from the file");
        let out = export(&set, &ExportOptions::standard()).unwrap();
        assert_eq!(String::from_utf8(out.output.files["session_s2.json"].clone()).unwrap(), file);
    }

    /// A session file is written back whole only while
    /// its records still say what it says. An edited one goes to state.db,
    /// the only store hermes reads (its session-file writer was removed in
    /// v2026.5.20), so the edit lands where hermes can resume it.
    #[cfg(feature = "sqlite")]
    #[test]
    fn an_edited_session_file_is_exported_to_state_db_with_its_edit() {
        let file = "{\n  \"session_id\": \"s2\",\n  \"model\": \"m\",\n  \"messages\": [\n    {\n      \"role\": \"user\",\n      \"content\": \"hi\"\n    }\n  ],\n  \"t\": \"\\u00e9\"\n}";
        let set = LedgerSet::single(import_session_json(file, "session_s2.json").unwrap());
        // Opposed: unchanged, the session file comes back byte for byte and no database is written.
        let out = export(&set, &ExportOptions::standard()).unwrap().output.files;
        assert_eq!(out.keys().collect::<Vec<_>>(), ["session_s2.json"]);
        assert_eq!(std::str::from_utf8(&out["session_s2.json"]).unwrap(), file);
        // Edited: the message (its text and its record) says something else;
        // the stale whole file stays as the header's `raw`.
        let mut edited = set.clone();
        let l = edited.ledgers.values_mut().next().unwrap();
        let e = l.events.iter_mut().find(|e| e.native.as_ref().and_then(|n| n.record_text()).is_some_and(|t| t.contains("\"hi\""))).expect("the message");
        e.body.insert("text".into(), json!("EDITED"));
        let n = e.native.as_mut().unwrap();
        let t = n.record_text().unwrap().replace("\"hi\"", "\"EDITED\"");
        *n = n.clone().record_raw(&t).unwrap();
        let out = export(&edited, &ExportOptions::standard()).unwrap();
        let names: Vec<&String> = out.output.files.keys().collect();
        assert!(!names.iter().any(|k| k.as_str() == "session_s2.json"), "the stale session file is not written: {names:?}");
        assert!(names.iter().any(|k| k.ends_with("state.db")), "the edited session goes to state.db: {names:?}");
        let dir = tempfile::tempdir().unwrap();
        out.output.write_to(dir.path(), true).unwrap();
        let back = import(dir.path(), &ImportOptions::standard()).unwrap().set;
        let texts: Vec<&str> = back.iter().flat_map(|l| l.events.iter()).filter(|e| e.kind == Kind::Text).filter_map(|e| e.str("text")).collect();
        assert!(texts.contains(&"EDITED") && !texts.contains(&"hi"), "the database holds the edit: {texts:?}");
    }

    #[test]
    fn without_member_cuts_one_member_exactly() {
        assert_eq!(without_member(r#"{"a":1,"m":[2],"b":"\u00e9"}"#, "m").as_deref(), Some(r#"{"a":1,"b":"\u00e9"}"#));
        assert_eq!(without_member(r#"{"m":[2],"b":1e3}"#, "m").as_deref(), Some(r#"{"b":1e3}"#));
        assert_eq!(without_member(r#"{"a":1.0,"m":{"x":[1]}}"#, "m").as_deref(), Some(r#"{"a":1.0}"#));
        assert_eq!(without_member(r#"{"m":1}"#, "m").as_deref(), Some("{}"));
        assert_eq!(without_member(r#"{"a":1}"#, "m"), None);
    }

    /// Title texts per ledger id, in the order the ledger holds them.
    #[cfg(feature = "sqlite")]
    fn titles_by_ledger(set: &LedgerSet) -> std::collections::BTreeMap<String, Vec<String>> {
        set.iter()
            .map(|l| (l.id().to_string(), l.events.iter().filter(|e| e.kind == Kind::Title).filter_map(|e| e.str("text")).map(str::to_string).collect()))
            .collect()
    }

    /// The `sessions.title` column of an export, in session-row order.
    #[cfg(feature = "sqlite")]
    fn db_titles(out: &Output) -> Vec<Option<String>> {
        let dir = tempfile::tempdir().unwrap();
        out.write_to(dir.path(), true).unwrap();
        let db = out.files.keys().find(|k| k.ends_with("state.db")).expect("a state.db");
        let c = rusqlite::Connection::open(dir.path().join(db)).unwrap();
        let mut st = c.prepare("SELECT title FROM sessions ORDER BY rowid").unwrap();
        st.query_map([], |r| r.get(0)).unwrap().map(Result::unwrap).collect()
    }

    /// Session ids whose `model_config` holds a carrier.
    #[cfg(feature = "sqlite")]
    fn carrying_rows(out: &Output) -> Vec<String> {
        let dir = tempfile::tempdir().unwrap();
        out.write_to(dir.path(), true).unwrap();
        let db = out.files.keys().find(|k| k.ends_with("state.db")).expect("a state.db");
        let c = rusqlite::Connection::open(dir.path().join(db)).unwrap();
        let mut st = c.prepare("SELECT id, model_config FROM sessions ORDER BY rowid").unwrap();
        let rows: Vec<(String, Option<String>)> = st.query_map([], |r| Ok((r.get(0)?, r.get(1)?))).unwrap().map(Result::unwrap).collect();
        rows.into_iter()
            .filter(|(_, mc)| json_col(mc.as_ref().map(|t| json!(t)).as_ref()).is_some_and(|v| v.get(crate::carry::KEY).is_some()))
            .map(|(id, _)| id)
            .collect()
    }

    /// Every `sessions` row of a state.db, by id, column by column.
    #[cfg(feature = "sqlite")]
    fn session_rows(db: &std::path::Path) -> std::collections::BTreeMap<String, std::collections::BTreeMap<String, rusqlite::types::Value>> {
        let c = rusqlite::Connection::open(db).unwrap();
        let mut st = c.prepare("SELECT * FROM sessions").unwrap();
        let names: Vec<String> = st.column_names().into_iter().map(str::to_string).collect();
        st.query_map([], |r| {
            let row: std::collections::BTreeMap<String, rusqlite::types::Value> =
                names.iter().enumerate().map(|(i, n)| Ok((n.clone(), r.get(i)?))).collect::<rusqlite::Result<_>>()?;
            Ok((r.get::<_, String>("id")?, row))
        })
        .unwrap()
        .map(Result::unwrap)
        .collect()
    }

    /// The `sessions` rows of an export's state.db.
    #[cfg(feature = "sqlite")]
    fn exported_session_rows(out: &Output) -> std::collections::BTreeMap<String, std::collections::BTreeMap<String, rusqlite::types::Value>> {
        let dir = tempfile::tempdir().unwrap();
        out.write_to(dir.path(), true).unwrap();
        let db = out.files.keys().find(|k| k.ends_with("state.db")).expect("a state.db");
        session_rows(&dir.path().join(db))
    }

    #[cfg(feature = "sqlite")]
    fn renamed(losses: &Losses) -> u64 {
        losses.0.iter().filter(|l| l.code == "title-renamed").map(|l| l.count).sum()
    }

    /// Ledgers `id -> title`, each holding one title event and one message.
    #[cfg(feature = "sqlite")]
    fn titled(pairs: &[(&str, &str)]) -> LedgerSet {
        let mut set = LedgerSet::new();
        for (id, title) in pairs {
            let a = crate::model::Agent {
                id: (*id).into(),
                created_at: Some("2026-10-01T00:00:00.000Z".into()),
                source: None,
                parent: None,
                links: vec![],
                meta: crate::model::Obj::new(),
            };
            let mut l = crate::ledger::Ledger::new(a);
            l.append(Event::new("title", Kind::Title).with("text", *title)).unwrap();
            l.append(Event::new("m", Kind::Text).ts(Some("2026-10-01T00:00:01.000Z")).with("role", "user").with("text", "hi")).unwrap();
            set.insert(l);
        }
        set
    }

    /// `sessions.title` is UNIQUE: a taken title is numbered as hermes
    /// numbers one, its ` #N` stripped, then `<base> #<highest + 1>`, the
    /// unnumbered original counting as #1. Sessions are written in ledger-id
    /// order, so the earliest id keeps the bare title. One `title-renamed`
    /// per renamed session; none when every title is free.
    #[cfg(feature = "sqlite")]
    #[test]
    fn taken_titles_are_numbered_from_the_highest_holder() {
        let set = titled(&[("a", "T"), ("b", "T"), ("c", "T"), ("d", "T #2"), ("e", "U #7"), ("f", "U #7"), ("g", "V"), ("h", "t")]);
        for opts in [ExportOptions::standard(), ExportOptions::no_carry()] {
            let out = export(&set, &opts).expect("hermes exports a ledger set whose titles repeat");
            let want = ["T", "T #2", "T #3", "T #4", "U #7", "U #8", "V", "t"];
            assert_eq!(db_titles(&out.output), want.map(|t| Some(t.to_string())), "numbered by the highest holder");
            assert_eq!(renamed(&out.losses), 4, "one per renamed session (b, c, d, f): {:?}", out.losses);
        }
        // Opposed: titles all free, nothing renamed and nothing reported.
        let free = titled(&[("a", "One"), ("b", "Two"), ("c", "T #2")]);
        let out = export(&free, &ExportOptions::standard()).unwrap();
        assert_eq!(db_titles(&out.output), ["One", "Two", "T #2"].map(|t| Some(t.to_string())));
        assert_eq!(renamed(&out.losses), 0, "no title is renamed: {:?}", out.losses);
    }

    /// A title's number is an integer of any size, as in hermes: the highest
    /// a u64 holds, one past it, and one with leading zeros are each numbered
    /// one higher, with no overflow.
    #[cfg(feature = "sqlite")]
    #[test]
    fn a_title_numbered_past_any_fixed_width_is_numbered_one_higher() {
        let max = "T #18446744073709551615";
        let wide = "W #99999999999999999999999";
        let set = titled(&[("a", max), ("b", max), ("c", wide), ("d", wide), ("e", "Z #009"), ("f", "Z #009")]);
        let out = export(&set, &ExportOptions::standard()).expect("hermes exports titles with large numbers");
        let want = [max, "T #18446744073709551616", wide, "W #100000000000000000000000", "Z #009", "Z #10"];
        assert_eq!(db_titles(&out.output), want.map(|t| Some(t.to_string())), "numbered one past the highest holder");
        assert_eq!(renamed(&out.losses), 3, "{:?}", out.losses);
        // Holders straddling a digit-count boundary: 10 is above 9. Eleven
        // sessions share `S`; `R` is held as `R`, `R #9` and `R #10`.
        let ids: Vec<String> = (0..11).map(|i| format!("g{i:02}")).collect();
        let mut pairs: Vec<(&str, &str)> = ids.iter().map(|id| (id.as_str(), "S")).collect();
        pairs.extend([("h0", "R"), ("h1", "R #9"), ("h2", "R #10"), ("h3", "R")]);
        let out = export(&titled(&pairs), &ExportOptions::standard()).expect("hermes numbers past #9 without a duplicate");
        let mut want: Vec<String> = vec!["S".into()];
        want.extend((2..=11).map(|n| format!("S #{n}")));
        want.extend(["R", "R #9", "R #10", "R #11"].map(str::to_string));
        assert_eq!(db_titles(&out.output), want.into_iter().map(Some).collect::<Vec<_>>(), "numbered one past the highest holder, by value");
        assert_eq!(renamed(&out.losses), 11, "{:?}", out.losses);
    }

    /// omp's subagent session repeats its parent's title, which state.db
    /// refused. The later holder becomes `<title> #2`, and a carried trip
    /// brings the original back.
    #[cfg(feature = "sqlite")]
    #[test]
    fn omp_subagent_branches_exports_with_duplicate_titles_numbered() {
        let input = std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("fixtures/omp/subagent-branches/native");
        let omp = crate::formats::by_name("omp").unwrap();
        let set = omp.import(&input, &ImportOptions::standard()).unwrap().set;
        let original = titles_by_ledger(&set);
        let title = original.values().flatten().next().expect("a titled session").clone();
        assert_eq!(original.values().flatten().filter(|t| **t == title).count(), 2, "two sessions share {title:?}: {original:?}");
        for carry in [true, false] {
            let opts = if carry { ExportOptions::standard() } else { ExportOptions::no_carry() };
            let out = export(&set, &opts).expect("hermes exports a ledger set whose titles repeat");
            assert_eq!(renamed(&out.losses), 1, "the one renamed session is named once: {:?}", out.losses);
            let mut titles: Vec<String> = db_titles(&out.output).into_iter().flatten().collect();
            titles.sort();
            assert_eq!(titles, [title.clone(), format!("{title} #2")], "the second holder is numbered");
            let dir = tempfile::tempdir().unwrap();
            out.output.write_to(dir.path(), true).unwrap();
            let back = import(dir.path(), &ImportOptions { variant: None, restore_carried: carry }).unwrap().set;
            let back = titles_by_ledger(&back);
            if carry {
                assert_eq!(back, original, "a carried trip restores each original title");
            } else {
                assert!(back.values().flatten().any(|t| *t == format!("{title} #2")), "opposed: without carry the number stays: {back:?}");
            }
            let ct = crate::roundtrip::cross_trip(omp, &input, crate::formats::by_name(FORMAT).unwrap(), carry).expect("omp -> hermes -> omp");
            if carry {
                assert_eq!(ct.there.score(), 0, "a carried trip loses nothing on the way there: {}", ct.there.summary());
            } else {
                assert!(ct.there.missing.contains_key("title"), "opposed: the comparison sees titles: {}", ct.there.summary());
            }
        }
    }

    /// A copy of a hermes fixture's state.db, changed by `sql` (none when
    /// empty), imported, with a foreign ledger that sorts first taking its
    /// first title, exported with carry. Returns the hermes session's ledger
    /// id, its title and the export.
    #[cfg(feature = "sqlite")]
    fn renamed_from_copy(fixture: &str, sql: &str) -> (String, String, Exported) {
        let src = std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("fixtures/hermes").join(fixture).join("native/state.db");
        let dir = tempfile::tempdir().unwrap();
        std::fs::copy(&src, dir.path().join("state.db")).unwrap();
        if !sql.is_empty() {
            rusqlite::Connection::open(dir.path().join("state.db")).unwrap().execute_batch(sql).unwrap();
        }
        let mut set = import(dir.path(), &ImportOptions::standard()).unwrap().set;
        let (hid, title) = titles_by_ledger(&set).into_iter().find_map(|(id, ts)| Some((id, ts.first()?.clone()))).expect("a titled hermes session");
        for (id, l) in titled(&[("!first", title.as_str())]).ledgers {
            assert!(id.as_str() < hid.as_str());
            set.insert(l);
        }
        (hid, title, export(&set, &ExportOptions::standard()).unwrap())
    }

    /// `carry-unavailable` for a renamed title: (count, detail) per entry.
    #[cfg(feature = "sqlite")]
    fn title_carry_unavailable(losses: &Losses) -> Vec<(u64, String)> {
        losses.0.iter().filter(|l| l.code == "carry-unavailable" && l.kind == Some(Kind::Title)).map(|l| (l.count, l.detail.clone())).collect()
    }

    /// A renamed hermes row whose `model_config` cannot hold a carrier (not
    /// a JSON object, or no such column) is renamed all the same and reports
    /// one `carry-unavailable` saying which. Opposed: the same store
    /// unchanged carries the renamed row and reports none.
    #[cfg(feature = "sqlite")]
    #[test]
    fn a_renamed_hermes_row_that_cannot_carry_reports_carry_unavailable() {
        let cases = [
            (
                "state-db-v31-lineage",
                "UPDATE sessions SET model_config = '[1]';",
                "a model_config that is not an object cannot hold the carrier of a renamed title",
            ),
            ("state-db-v4", "ALTER TABLE sessions DROP COLUMN model_config;", "this schema has no model_config column to carry a renamed title"),
        ];
        for (fixture, sql, detail) in cases {
            let (hid, title, out) = renamed_from_copy(fixture, sql);
            assert_eq!(renamed(&out.losses), 1, "{fixture}: {:?}", out.losses);
            assert!(db_titles(&out.output).contains(&Some(format!("{title} #2"))), "{fixture}: the row is renamed: {:?}", db_titles(&out.output));
            assert_eq!(title_carry_unavailable(&out.losses), vec![(1, detail.to_string())], "{fixture}: {:?}", out.losses);
            if fixture == "state-db-v31-lineage" {
                assert!(!carrying_rows(&out.output).contains(&hid), "{fixture}: no carrier is written");
            }
            // Opposed: unchanged, the renamed row carries and nothing is reported.
            let (hid, title, out) = renamed_from_copy(fixture, "");
            assert_eq!(renamed(&out.losses), 1, "{fixture} unchanged: {:?}", out.losses);
            assert!(db_titles(&out.output).contains(&Some(format!("{title} #2"))), "{fixture} unchanged: {:?}", db_titles(&out.output));
            assert_eq!(title_carry_unavailable(&out.losses), Vec::<(u64, String)>::new(), "{fixture} unchanged: {:?}", out.losses);
            assert!(carrying_rows(&out.output).contains(&hid), "{fixture} unchanged: the renamed row carries");
        }
    }

    /// A hermes session renamed by the export (another session holding its
    /// title is written first) comes back with its own title on a carried
    /// trip: its row carries its events. Without a rename the row is written
    /// as read, with no carrier.
    #[cfg(feature = "sqlite")]
    #[test]
    fn a_renamed_hermes_session_is_restored_by_a_carried_trip() {
        let input = std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("fixtures/hermes/state-db-v31-lineage/native");
        let mut set = import(&input, &ImportOptions::standard()).unwrap().set;
        let own = titles_by_ledger(&set);
        let (hid, title) = own.iter().find_map(|(id, ts)| Some((id.clone(), ts.first()?.clone()))).expect("a titled hermes session");
        let hermes_only = set.clone();
        // Opposed: alone, no title is taken and the rows carry nothing new.
        let alone = export(&hermes_only, &ExportOptions::standard()).unwrap();
        assert_eq!(renamed(&alone.losses), 0, "{:?}", alone.losses);
        let source = session_rows(&input.join("state.db"));
        assert!(source.contains_key(&hid) && source.len() > 1, "the source holds the hermes session and others: {:?}", source.keys());
        assert_eq!(exported_session_rows(&alone.output), source, "alone, every row is written as read, column for column");
        assert_eq!(carrying_rows(&alone.output), Vec::<String>::new(), "a row that is not renamed is written as read");
        // A foreign ledger whose id sorts first takes the title.
        for (id, l) in titled(&[("!first", title.as_str())]).ledgers {
            assert!(id.as_str() < hid.as_str());
            set.insert(l);
        }
        for carry in [true, false] {
            let opts = if carry { ExportOptions::standard() } else { ExportOptions::no_carry() };
            let out = export(&set, &opts).unwrap();
            assert_eq!(renamed(&out.losses), 1, "{:?}", out.losses);
            assert!(db_titles(&out.output).contains(&Some(format!("{title} #2"))), "the hermes row is numbered: {:?}", db_titles(&out.output));
            let rows = exported_session_rows(&out.output);
            for (id, row) in source.iter().filter(|(id, _)| **id != hid) {
                assert_eq!(rows.get(id), Some(row), "carry {carry}: a row that is not renamed is written as read, column for column");
            }
            assert_ne!(rows.get(&hid), source.get(&hid), "opposed: the renamed row is not as read");
            assert_eq!(carrying_rows(&out.output).contains(&hid), carry, "the renamed hermes row carries its events only with carry");
            let dir = tempfile::tempdir().unwrap();
            out.output.write_to(dir.path(), true).unwrap();
            let back = import(dir.path(), &ImportOptions { variant: None, restore_carried: carry }).unwrap().set;
            let got = titles_by_ledger(&back).get(&hid).cloned();
            if carry {
                assert_eq!(got, Some(vec![title.clone()]), "a carried trip restores the hermes session's own title");
                let mut mine = LedgerSet::new();
                mine.insert(back.get(&hid).unwrap().clone());
                let mut was = LedgerSet::new();
                was.insert(hermes_only.get(&hid).unwrap().clone());
                let d = crate::loss::compare(&was, &mine, Default::default());
                assert_eq!(d.score(), 0, "the session comes back as it was: {}", d.summary());
                let ids = |l: &Ledger| l.events.iter().map(|e| e.id.clone()).collect::<Vec<_>>();
                assert_eq!(ids(back.get(&hid).unwrap()), ids(hermes_only.get(&hid).unwrap()), "in the order it had, its usage last");
            } else {
                assert_eq!(got, Some(vec![format!("{title} #2")]), "opposed: without carry the number stays");
            }
        }
    }

    /// A changed legacy session goes into `state.db` as a rendered row that
    /// carries its agent; a carried trip restores that agent in place of the
    /// one built from the row, and it keeps the header values of the file it
    /// was read from. Values the carried agent was never read with, so the
    /// carrier cannot supply them.
    #[cfg(feature = "sqlite")]
    #[test]
    fn a_restored_hermes_agent_keeps_its_file_header() {
        let input = std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("fixtures/hermes/legacy-session-json/native");
        let mut set = import(&input, &ImportOptions::standard()).unwrap().set;
        let mut l = set.iter().next().unwrap().clone();
        let id = l.id().to_string();
        l.append_unique(Event::new("appended", Kind::Text).with("role", "user").with("text", "one more"));
        set.insert(l);
        let out = export(&set, &ExportOptions::standard()).unwrap();
        let dir = tempfile::tempdir().unwrap();
        out.output.write_to(dir.path(), true).unwrap();
        let c = rusqlite::Connection::open(dir.path().join("state.db")).unwrap();
        c.pragma_update(None, "application_id", 1_234_567).unwrap();
        c.pragma_update(None, "user_version", 7).unwrap();
        drop(c);
        let back = import(dir.path(), &ImportOptions { variant: None, restore_carried: true }).unwrap().set;
        let src = back.get(&id).unwrap().agent().source.clone().unwrap();
        assert_eq!(src.variant.as_deref(), Some("session-json"), "the carried agent is restored, not the one built from the row");
        assert_eq!(src.extra.get(crate::formats::sqlite_header::KEY), Some(&serde_json::json!({"application_id": 1_234_567, "user_version": 7})));
    }
}
