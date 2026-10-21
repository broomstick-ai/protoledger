//! OpenClaw session transcripts.
//!
//! OpenClaw embeds pi-agent's `SessionManager`, so a transcript is a pi
//! session JSONL file: one `type:"session"` header line, then entries
//! (`message`, `model_change`, `compaction`, ...) linked into a tree by
//! `id`/`parentId`. The pi-session parsing lives here, independent of the
//! `pi` adapter. See `docs/formats/openclaw.md`.

pub(crate) mod jsonl;
#[cfg(feature = "sqlite")]
mod sqlite;

use crate::formats::{Detection, ExportOptions, Exported, Format, ImportOptions, Imported, Losses, Output, Variant};
use crate::ledger::{Ledger, LedgerSet};
use crate::model::{Agent, Audience, Event, Kind, Link, Obj, Parent, Relation, Source};
use crate::util;
use anyhow::{Context, Result, bail};
use jsonl::{Line, Taken};
use serde_json::{Map, Value, json};
use std::collections::{HashMap, HashSet};
use std::path::Path;

const FORMAT: &str = "openclaw";

/// customType of the `custom` entry protoledger writes for text that people
/// saw but the model did not.
const RECORD_TEXT: &str = "protoledger.record-text";

/// OpenClaw `CURRENT_SESSION_VERSION` (src/config/sessions/version.ts) written on export.
const EXPORT_VERSION: u64 = 4;

pub struct OpenClaw;

static VARIANTS: &[Variant] = &[
    Variant {
        name: "session-v1",
        versions: "pi <= 0.30 / clawdbot (header without version)",
        import: true,
        export: false,
        notes: "linear entries without id/parentId; anchored by byte offset",
    },
    Variant { name: "session-v2", versions: "pi 0.31-0.34", import: true, export: false, notes: "id/parentId tree; `hookMessage` role" },
    Variant {
        name: "session-v3",
        versions: "pi >= 0.35; OpenClaw <= 2026.9.6",
        import: true,
        export: false,
        notes: "pi writer (8-hex ids, base fields first) until 2026.5.27, then OpenClaw's fork (UUIDs, payload first)",
    },
    Variant {
        name: "session-v4",
        versions: "OpenClaw >= 2026.9.7 (JSONL via `doctor --session-sqlite import`)",
        import: true,
        export: true,
        notes: "same entries as v3; v4 only changes prompt escaping. Export target",
    },
    Variant {
        name: "sqlite",
        versions: "OpenClaw >= 2026.8.1 (agents/<id>/agent/openclaw-agent.sqlite)",
        import: true,
        export: true,
        notes: "transcript_events rows are transcript lines; rebuilt row-identically, foreign ledgers export as v4 JSONL",
    },
];

impl Format for OpenClaw {
    fn name(&self) -> &'static str {
        FORMAT
    }
    fn describe(&self) -> &'static str {
        "OpenClaw agent sessions (pi-agent session JSONL, ~/.openclaw/agents/<id>/sessions/)"
    }
    fn variants(&self) -> &'static [Variant] {
        VARIANTS
    }
    fn detect(&self, input: &Path) -> Option<Detection> {
        #[cfg(feature = "sqlite")]
        {
            let dbs: Vec<std::path::PathBuf> =
                if input.is_file() { vec![input.to_path_buf()] } else { jsonl::walk(input, ".sqlite").into_iter().map(|(_, p)| p).collect() };
            if dbs.iter().any(|p| sqlite::probe(p)) {
                return Some(Detection { variant: "sqlite", confidence: 92, version: None });
            }
        }
        let files: Vec<std::path::PathBuf> =
            if input.is_file() { vec![input.to_path_buf()] } else { jsonl::walk(input, ".jsonl").into_iter().map(|(_, p)| p).take(4).collect() };
        let mut best: Option<Detection> = None;
        for p in files {
            let text = std::fs::read_to_string(&p).ok()?;
            if let Some(d) = detect_text(&text, &p)
                && best.as_ref().is_none_or(|b| d.confidence > b.confidence)
            {
                best = Some(d);
            }
        }
        best
    }
    fn import(&self, input: &Path, opts: &ImportOptions) -> Result<Imported> {
        import(input, opts)
    }
    fn export(&self, set: &LedgerSet, opts: &ExportOptions) -> Result<Exported> {
        export(set, opts)
    }
}

// ---------------------------------------------------------------- detection

fn detect_text(text: &str, path: &Path) -> Option<Detection> {
    let first = jsonl::lines(text).into_iter().find_map(|l| l.value)?;
    if first.get("type").and_then(Value::as_str) != Some("session") || first.get("id").is_none() {
        return None;
    }
    let version = first.get("version").and_then(Value::as_u64);
    let p = path.to_string_lossy();
    // pi and OpenClaw write the same header; content markers and the storage
    // path tell them apart.
    let marked = p.contains(".openclaw")
        || p.contains(".clawdbot")
        || p.contains("/agents/") && p.contains("/sessions/")
        || text.contains("\"delivery-mirror\"")
        || text.contains("\"provider\":\"openclaw\"")
        || text.contains("\"customType\":\"openclaw")
        || text.contains("\"openclaw");
    Some(Detection { variant: variant_name(version), confidence: if marked { 90 } else { 55 }, version: version.map(|v| format!("session-v{v}")) })
}

fn variant_name(version: Option<u64>) -> &'static str {
    match version {
        None | Some(0) | Some(1) => "session-v1",
        Some(2) => "session-v2",
        Some(3) => "session-v3",
        _ => "session-v4",
    }
}

// ---------------------------------------------------------------- import

fn import(input: &Path, opts: &ImportOptions) -> Result<Imported> {
    let mut set = LedgerSet::new();
    let mut losses = Losses::default();
    let mut variant = None;
    let all: Vec<(String, std::path::PathBuf)> = if input.is_file() {
        vec![(input.file_name().unwrap().to_string_lossy().to_string(), input.to_path_buf())]
    } else {
        let mut v = jsonl::walk(input, ".jsonl");
        v.extend(jsonl::walk(input, ".sqlite"));
        v.sort();
        v
    };
    let mut files = Vec::new();
    for (rel, p) in all {
        #[cfg(feature = "sqlite")]
        if sqlite::probe(&p) {
            for l in sqlite::import(&p, &rel, opts, &mut losses)? {
                variant.get_or_insert("sqlite");
                set.insert(l);
            }
            continue;
        }
        files.push((rel, p));
    }
    if files.is_empty() && set.is_empty() {
        bail!("{}: no session .jsonl files", input.display());
    }
    // sessions.json index files, kept whole so the store round-trips.
    let mut index_files: Vec<(String, String)> = Vec::new();
    if input.is_dir() {
        for (rel, p) in jsonl::walk(input, "sessions.json") {
            index_files.push((rel, std::fs::read_to_string(&p)?));
        }
    }
    let index = parse_indexes(&index_files);
    for (rel, path) in &files {
        let text = std::fs::read_to_string(path).with_context(|| path.display().to_string())?;
        // A file with nothing in it yet names no session: its id would be the
        // file's, not the session's (SPEC §9.3, prefix-stable).
        if text.trim().is_empty() {
            continue;
        }
        let (l, v) = import_text(&text, rel, opts, &mut losses)?;
        variant.get_or_insert(v);
        set.insert(l);
    }
    if set.is_empty() {
        bail!("{}: no session record yet", input.display());
    }
    link_index(&mut set, &index, &index_files);
    // A fork whose parent session is in the set is a structural fork.
    let ids: HashSet<String> = set.ledgers.keys().cloned().collect();
    for l in set.ledgers.values_mut() {
        if l.header.agent.parent.is_some() {
            continue;
        }
        let fork = l.agent().links.iter().find(|k| k.rel == "forked-from").and_then(|k| k.agent.clone());
        if let Some(p) = fork.filter(|p| ids.contains(p) && p != l.id()) {
            l.header.agent.parent = Some(Parent { agent: p, event: None, relation: Relation::Fork });
        }
    }
    Ok(Imported { set, losses, variant: variant.unwrap_or("session-v4").to_string() })
}

/// One `sessions.json` entry that names a session file.
struct IndexEntry {
    key: String,
    session_id: String,
    spawned_by: Option<String>,
}

fn parse_indexes(files: &[(String, String)]) -> Vec<IndexEntry> {
    let mut out = Vec::new();
    for (_, text) in files {
        let Ok(Value::Object(m)) = serde_json::from_str::<Value>(text) else { continue };
        for (key, v) in m {
            if let Some(sid) = v.get("sessionId").and_then(Value::as_str) {
                out.push(IndexEntry {
                    key: key.clone(),
                    session_id: sid.to_string(),
                    spawned_by: v.get("spawnedBy").and_then(Value::as_str).map(str::to_string),
                });
            }
        }
    }
    out
}

/// Attach index facts: the whole index files on the first ledger (for an exact
/// rebuild), the session key on each ledger, and `spawnedBy` as a structural
/// parent when the parent session is in the set.
fn link_index(set: &mut LedgerSet, index: &[IndexEntry], files: &[(String, String)]) {
    if files.is_empty() {
        return;
    }
    let by_key: HashMap<&str, &str> = index.iter().map(|e| (e.key.as_str(), e.session_id.as_str())).collect();
    let ids: Vec<String> = set.ledgers.keys().cloned().collect();
    if let Some(first) = ids.first() {
        let l = set.ledgers.get_mut(first).unwrap();
        let src =
            l.header.agent.source.get_or_insert_with(|| Source { format: FORMAT.into(), variant: None, version: None, native_id: None, extra: Obj::new() });
        let mut m = Map::new();
        for (rel, text) in files {
            m.insert(rel.clone(), Value::String(text.clone()));
        }
        src.extra.insert("index_files".into(), Value::Object(m));
    }
    let mut pending: Vec<(String, String, String)> = Vec::new();
    for e in index {
        let Some(l) = set.ledgers.get_mut(&e.session_id) else { continue };
        if let Some(src) = l.header.agent.source.as_mut() {
            src.extra.insert("session_key".into(), Value::String(e.key.clone()));
        }
        if l.header.agent.parent.is_some() {
            continue;
        }
        if let Some(parent_sid) = e.spawned_by.as_deref().and_then(|k| by_key.get(k))
            && *parent_sid != e.session_id
            && ids.iter().any(|i| i == parent_sid)
        {
            pending.push((e.session_id.clone(), parent_sid.to_string(), e.key.clone()));
        }
    }
    for (child, parent, key) in pending {
        // The spawning call: the tool call whose result names the child's key.
        let event = set.get(&parent).and_then(|pl| {
            let call =
                pl.events.iter().find(|x| x.kind == Kind::ToolResult && jsonl::output_text(x.body.get("output")).contains(&key))?.str("call_id")?.to_string();
            pl.events.iter().find(|x| x.kind == Kind::ToolCall && x.str("call_id") == Some(&call)).map(|x| x.id.clone())
        });
        set.ledgers.get_mut(&child).unwrap().header.agent.parent = Some(Parent { agent: parent, event, relation: Relation::Subagent });
    }
    // A parent must not name a child that names it back.
    let parents: Vec<(String, String)> =
        set.ledgers.values().filter_map(|l| l.agent().parent.as_ref().map(|p| (l.id().to_string(), p.agent.clone()))).collect();
    for (child, parent) in parents {
        if set.get(&parent).and_then(|p| p.agent().parent.as_ref()).is_some_and(|pp| pp.agent == child) {
            set.ledgers.get_mut(&child).unwrap().header.agent.parent = None;
        }
    }
}

fn import_text(text: &str, rel: &str, opts: &ImportOptions, losses: &mut Losses) -> Result<(Ledger, &'static str)> {
    // Some files start with a UTF-8 BOM; offsets are taken after it.
    let bom = text.starts_with('\u{feff}');
    let text = text.strip_prefix('\u{feff}').unwrap_or(text);
    let lines = jsonl::lines(text);
    let header = lines.iter().find_map(|l| l.value.as_ref().filter(|v| str_at(v, "type") == Some("session")));
    let version = header.and_then(|h| h.get("version")).and_then(Value::as_u64);
    let variant = variant_name(version);
    let vtag = version.map(|v| format!("session-v{v}"));
    let session_id = header
        .and_then(|h| str_at(h, "id"))
        .map(str::to_string)
        .unwrap_or_else(|| rel.rsplit('/').next().unwrap_or(rel).trim_end_matches(".jsonl").to_string());
    let mut extra = Obj::new();
    extra.insert("file".into(), Value::String(rel.to_string()));
    if !text.is_empty() && !text.ends_with('\n') {
        extra.insert("final_newline".into(), Value::Bool(false));
    }
    if bom {
        extra.insert("bom".into(), Value::Bool(true));
    }
    let mut agent = Agent {
        id: session_id.clone(),
        created_at: header.and_then(|h| str_at(h, "timestamp")).map(str::to_string),
        source: Some(Source { format: FORMAT.into(), variant: Some(variant.into()), version: vtag.clone(), native_id: Some(session_id.clone()), extra }),
        parent: None,
        links: vec![],
        meta: Obj::new(),
    };
    if let Some(ps) = header.and_then(|h| str_at(h, "parentSession")) {
        // A session file path (`.../<timestamp>_<id>.jsonl`) or an id.
        let base = ps.rsplit(['/', '\\']).next().unwrap_or(ps).trim_end_matches(".jsonl");
        let pid = base.rsplit('_').next().unwrap_or(base).to_string();
        let mut x = Obj::new();
        x.insert("path".into(), json!(ps));
        agent.links.push(Link { rel: "forked-from".into(), agent: Some(pid), event: None, extra: x });
    }
    let mut l = Ledger::new(agent);
    let mut prev_entry: Option<String> = None;
    let mut carried_agent: Option<Agent> = None;
    for line in &lines {
        let anchor = format!("@{}", line.off);
        let Some(value) = &line.value else {
            // A torn or corrupt line: kept verbatim, nothing neutral.
            let n = jsonl::native_of(FORMAT, variant, vtag.as_deref(), &anchor, line, Some(rel));
            l.append_unique(Event::new(anchor, Kind::Native).native(n));
            losses.add("invalid-json-line", Some(Kind::Native), "a line that is not JSON is kept only verbatim");
            continue;
        };
        let mut rec = value.clone();
        let mut native_line: Line = Line { off: line.off, text: line.text, value: Some(rec.clone()), exact: line.exact };
        match jsonl::take(&mut rec, opts.restore_carried) {
            Taken::Restored(evs, agent) => {
                for e in evs {
                    l.append_unique(e);
                }
                if let Some(a) = agent {
                    carried_agent = Some(a);
                }
                prev_entry = str_at(&rec, "id").map(str::to_string);
                continue;
            }
            Taken::Stale => {
                // Edited in the harness: import what is there now, carrier gone.
                if opts.restore_carried {
                    native_line.value = Some(rec.clone());
                    native_line.exact = false;
                }
            }
            Taken::None => {}
        }
        let base = str_at(&rec, "id").filter(|_| str_at(&rec, "type") != Some("session")).map(str::to_string);
        let id = match (&base, str_at(&rec, "type")) {
            (Some(i), _) => i.clone(),
            (None, Some("session")) => session_id.clone(),
            _ => anchor.clone(),
        };
        let mut evs = record_events(&rec, &id);
        evs.iter_mut().for_each(crate::formats::pi_session::unwrap_peer_in_place);
        let native = jsonl::native_of(FORMAT, variant, vtag.as_deref(), &id, &native_line, Some(rel));
        // Tree: `after` only when the parent is not simply the previous entry.
        let parent = rec.get("parentId").and_then(Value::as_str).map(str::to_string);
        let mut after = None;
        if let Some(p) = &parent
            && prev_entry.as_deref() != Some(p.as_str())
        {
            after = Some(p.clone());
        }
        if base.is_some() {
            prev_entry = base.clone();
        }
        for (i, e) in evs.iter_mut().enumerate() {
            e.native = Some(if i == 0 { native.clone() } else { native.part_of(i as u32) });
            if i == 0 {
                e.after = after.clone();
            }
        }
        for e in evs {
            l.append_unique(e);
        }
    }
    if let Some(a) = carried_agent {
        l.header.agent = a;
    }
    Ok((l, variant))
}

fn str_at<'a>(v: &'a Value, k: &str) -> Option<&'a str> {
    v.get(k).and_then(Value::as_str)
}

fn ts_of(rec: &Value) -> Option<String> {
    if let Some(s) = str_at(rec, "timestamp") {
        return Some(s.to_string());
    }
    rec.get("message").and_then(|m| m.get("timestamp")).and_then(Value::as_i64).map(util::ms_to_rfc3339)
}

/// The neutral events one pi entry stands for (ids, ts, group; no native).
/// Pure, so the exporter can recount a record's parts.
fn record_events(rec: &Value, id: &str) -> Vec<Event> {
    let ts = ts_of(rec);
    let mut out: Vec<Event> = Vec::new();
    let push = |out: &mut Vec<Event>, kind: Kind| -> usize {
        let n = out.len();
        let eid = if n == 0 { id.to_string() } else { format!("{id}#{n}") };
        out.push(Event::new(eid, kind).ts(ts.clone()));
        n
    };
    match str_at(rec, "type").unwrap_or("") {
        "session" => {
            let i = push(&mut out, Kind::Settings);
            out[i] = out[i].clone().with("cwd", rec.get("cwd").cloned().unwrap_or(Value::Null));
        }
        "message" => {
            let m = rec.get("message").cloned().unwrap_or(Value::Null);
            message_events(&m, id, &mut out, &push);
        }
        "model_change" => {
            let i = push(&mut out, Kind::Settings);
            out[i] = out[i]
                .clone()
                .with("provider", rec.get("provider").cloned().unwrap_or(Value::Null))
                .with("model", rec.get("modelId").cloned().unwrap_or(Value::Null));
        }
        "thinking_level_change" => {
            let i = push(&mut out, Kind::Settings);
            out[i] = out[i].clone().with("effort", rec.get("thinkingLevel").cloned().unwrap_or(Value::Null));
        }
        "usage" => {
            let i = push(&mut out, Kind::Usage);
            out[i] = usage_event(out[i].clone(), rec.get("usage"), rec.get("model"));
        }
        "compaction" => {
            let i = push(&mut out, Kind::Compaction);
            out[i] = out[i]
                .clone()
                .with("phase", "summary")
                .with("trigger", if rec.get("fromHook").and_then(Value::as_bool) == Some(true) { "manual" } else { "auto" })
                .with("summary", rec.get("summary").cloned().unwrap_or(Value::Null))
                .with("tokens_before", rec.get("tokensBefore").cloned().unwrap_or(Value::Null));
        }
        "branch_summary" => {
            let i = push(&mut out, Kind::App);
            out[i] = out[i]
                .clone()
                .audience(Audience::Model)
                .with("type", "pi/branch_summary")
                .with("data", json!({"summary": rec.get("summary"), "from": rec.get("fromId")}));
        }
        "custom_message" => {
            content_events(rec.get("content"), "user", &mut out, &push);
            if out.is_empty() {
                push(&mut out, Kind::Native);
            }
        }
        "session_info" => {
            let i = push(&mut out, Kind::Title);
            out[i] = out[i].clone().with("text", rec.get("name").cloned().unwrap_or(Value::Null));
        }
        // OpenClaw: the conversation was reset; context before it is gone.
        "reset" => {
            let i = push(&mut out, Kind::Compaction);
            let manual = matches!(str_at(rec, "reason"), Some("new") | Some("reset"));
            out[i] = out[i]
                .clone()
                .with("phase", "boundary")
                .with("trigger", if manual { "manual" } else { "auto" })
                .with("reason", rec.get("reason").cloned().unwrap_or(Value::Null));
        }
        "custom" if str_at(rec, "customType") == Some(RECORD_TEXT) => {
            let d = rec.get("data").cloned().unwrap_or(Value::Null);
            let i = push(&mut out, Kind::Text);
            out[i] = out[i]
                .clone()
                .audience(Audience::Record)
                .with("role", d.get("role").cloned().unwrap_or(json!("user")))
                .with("text", d.get("text").cloned().unwrap_or(Value::Null));
        }
        // custom (extension state), label, context_edit, and anything newer.
        _ => {
            push(&mut out, Kind::Native);
        }
    }
    if out.is_empty() {
        push(&mut out, Kind::Native);
    }
    out
}

/// A usage block whose counters and cost are all zero says nothing.
fn zero_usage(u: &Value) -> bool {
    let z = |v: Option<&Value>| v.is_none_or(|v| v.as_f64() == Some(0.0));
    ["input", "output", "cacheRead", "cacheWrite", "totalTokens"].iter().all(|k| z(u.get(*k))) && z(u.get("cost").and_then(|c| c.get("total")))
}

fn usage_event(e: Event, u: Option<&Value>, model: Option<&Value>) -> Event {
    let g = |k: &str| u.and_then(|u| u.get(k)).cloned().unwrap_or(Value::Null);
    e.with("model", model.cloned().unwrap_or(Value::Null))
        .with("input", g("input"))
        .with("output", g("output"))
        .with("cache_read", g("cacheRead"))
        .with("cache_write", g("cacheWrite"))
        .with("reasoning", g("reasoning"))
        .with("cost_usd", u.and_then(|u| u.get("cost")).and_then(|c| c.get("total")).cloned().unwrap_or(Value::Null))
}

/// SPEC §3.1.5: what pi-ai's `thinkingSignature` seals, by the message's API.
/// Behind a chat-completions API it only names the field the reasoning came
/// in (`reasoning_content`), so it seals nothing; behind OpenAI's Responses
/// API it is the encrypted reasoning item; behind Anthropic's (or an unknown
/// API, whose provider is kept) a signature.
fn seal_of<'a>(api: &str, provider: &'a str) -> Option<(&'a str, &'static str)> {
    if api == "openai-completions" {
        None
    } else if api.starts_with("openai") || provider.starts_with("openai") {
        Some(("openai", "encrypted"))
    } else if api.starts_with("anthropic") || provider == "anthropic" {
        Some(("anthropic", "signature"))
    } else if api.starts_with("google") {
        Some(("google", "signature"))
    } else {
        Some((provider, "signature"))
    }
}

/// User-side content: text and images.
fn content_events(content: Option<&Value>, role: &str, out: &mut Vec<Event>, push: &dyn Fn(&mut Vec<Event>, Kind) -> usize) {
    match content {
        Some(Value::String(s)) => {
            let i = push(out, Kind::Text);
            out[i] = out[i].clone().with("role", role).with("text", s.as_str());
        }
        Some(Value::Array(blocks)) => {
            for b in blocks {
                match str_at(b, "type") {
                    Some("text") => {
                        let i = push(out, Kind::Text);
                        out[i] = out[i].clone().with("role", role).with("text", b.get("text").cloned().unwrap_or(Value::Null));
                    }
                    Some("image") => {
                        let i = push(out, Kind::Media);
                        out[i] = out[i]
                            .clone()
                            .with("role", role)
                            .with("media_type", b.get("mimeType").cloned().unwrap_or(Value::Null))
                            .with("data", b.get("data").cloned().unwrap_or(Value::Null));
                    }
                    _ => {}
                }
            }
        }
        _ => {}
    }
}

fn message_events(m: &Value, id: &str, out: &mut Vec<Event>, push: &dyn Fn(&mut Vec<Event>, Kind) -> usize) {
    let role = str_at(m, "role").unwrap_or("");
    match role {
        "user" => content_events(m.get("content"), "user", out, push),
        "custom" | "hookMessage" => content_events(m.get("content"), "user", out, push),
        "system" => content_events(m.get("content"), "system", out, push),
        "assistant" => {
            let provider = str_at(m, "provider").or(str_at(m, "api")).unwrap_or("unknown").to_string();
            match m.get("content") {
                Some(Value::String(s)) => {
                    let i = push(out, Kind::Text);
                    out[i] = out[i].clone().with("role", "assistant").with("text", s.as_str());
                }
                Some(Value::Array(blocks)) => {
                    for b in blocks {
                        match str_at(b, "type") {
                            Some("text") => {
                                let i = push(out, Kind::Text);
                                out[i] = out[i].clone().with("role", "assistant").with("text", b.get("text").cloned().unwrap_or(Value::Null));
                            }
                            Some("thinking") => {
                                let i = push(out, Kind::Reasoning);
                                let mut e = out[i].clone().with("text", b.get("thinking").cloned().unwrap_or(Value::Null));
                                if b.get("redacted").and_then(Value::as_bool) == Some(true) {
                                    e = e.with("redacted", true);
                                }
                                if let Some(sig) = str_at(b, "thinkingSignature").filter(|s| !s.is_empty())
                                    && let Some((vendor, kind)) = seal_of(str_at(m, "api").unwrap_or(""), &provider)
                                {
                                    e = e.with("sealed", json!({"vendor": vendor, "kind": kind, "data": sig}));
                                }
                                out[i] = e;
                            }
                            Some("toolCall") => {
                                let i = push(out, Kind::ToolCall);
                                out[i] = out[i]
                                    .clone()
                                    .with("call_id", b.get("id").cloned().unwrap_or(Value::Null))
                                    .with("name", b.get("name").cloned().unwrap_or(Value::Null))
                                    .with("input", b.get("arguments").cloned().unwrap_or(json!({})))
                                    .with("tool_type", "function");
                            }
                            _ => {}
                        }
                    }
                }
                _ => {}
            }
            if let Some(u) = m.get("usage").filter(|u| u.is_object() && !zero_usage(u)) {
                let i = push(out, Kind::Usage);
                out[i] = usage_event(out[i].clone(), Some(u), m.get("model"));
            }
            if let Some(err) = str_at(m, "errorMessage") {
                let i = push(out, Kind::Error);
                out[i] = out[i].clone().with("kind", "provider").with("message", err);
            } else if str_at(m, "stopReason") == Some("aborted") {
                let i = push(out, Kind::Turn);
                out[i] = out[i].clone().with("phase", "end").with("outcome", "aborted");
            }
            for e in out.iter_mut() {
                e.group = Some(id.to_string());
            }
        }
        "toolResult" | "tool" => {
            let i = push(out, Kind::ToolResult);
            out[i] = out[i]
                .clone()
                .with("call_id", m.get("toolCallId").cloned().unwrap_or(Value::Null))
                .with("output", tool_output(m.get("content")))
                .with("is_error", m.get("isError").cloned().unwrap_or(Value::Null));
        }
        "bashExecution" => {
            let hidden = m.get("excludeFromContext").and_then(Value::as_bool) == Some(true);
            let i = push(out, Kind::ToolCall);
            out[i] = out[i].clone().with("call_id", id).with("name", "bash").with("input", json!({"command": m.get("command")})).with("tool_type", "shell");
            let j = push(out, Kind::ToolResult);
            let failed = m.get("cancelled").and_then(Value::as_bool) == Some(true) || m.get("exitCode").and_then(Value::as_i64).is_some_and(|c| c != 0);
            out[j] = out[j].clone().with("call_id", id).with("output", m.get("output").cloned().unwrap_or(Value::Null)).with("is_error", failed);
            if hidden {
                out[i].audience = Some(Audience::Record);
                out[j].audience = Some(Audience::Record);
            }
        }
        "compactionSummary" => {
            let i = push(out, Kind::Compaction);
            out[i] = out[i]
                .clone()
                .with("phase", "summary")
                .with("summary", m.get("summary").cloned().unwrap_or(Value::Null))
                .with("tokens_before", m.get("tokensBefore").cloned().unwrap_or(Value::Null));
        }
        "branchSummary" => {
            let i = push(out, Kind::App);
            out[i] = out[i]
                .clone()
                .audience(Audience::Model)
                .with("type", "pi/branch_summary")
                .with("data", json!({"summary": m.get("summary"), "from": m.get("fromId")}));
        }
        _ => {}
    }
}

fn tool_output(content: Option<&Value>) -> Value {
    match content {
        Some(Value::String(s)) => Value::String(s.clone()),
        Some(Value::Array(blocks)) => {
            let texts: Vec<&Value> = blocks.iter().collect();
            if texts.len() == 1 && str_at(texts[0], "type") == Some("text") {
                return texts[0].get("text").cloned().unwrap_or(Value::Null);
            }
            Value::Array(
                blocks
                    .iter()
                    .filter_map(|b| match str_at(b, "type") {
                        Some("text") => Some(json!({"type":"text","text": b.get("text")})),
                        Some("image") => Some(json!({"type":"media","media_type": b.get("mimeType"),"data": b.get("data")})),
                        _ => None,
                    })
                    .collect(),
            )
        }
        _ => Value::String(String::new()),
    }
}

// ---------------------------------------------------------------- export

fn export(set: &LedgerSet, opts: &ExportOptions) -> Result<Exported> {
    let mut output = Output::default();
    let mut losses = Losses::default();
    let mut index_written: HashSet<String> = HashSet::new();
    #[cfg(feature = "sqlite")]
    let in_db = sqlite::export(set, &mut output)?;
    #[cfg(not(feature = "sqlite"))]
    let in_db: HashSet<String> = HashSet::new();
    for l in set.iter().filter(|l| !in_db.contains(l.id())) {
        let (path, text) = export_ledger(l, set, opts, &mut losses);
        output.put(path, text);
        if let Some(files) = l.agent().source.as_ref().filter(|s| s.format == FORMAT).and_then(|s| s.extra.get("index_files")).and_then(Value::as_object) {
            for (rel, text) in files {
                if index_written.insert(rel.clone()) {
                    output.put(rel.clone(), text.as_str().unwrap_or("").as_bytes().to_vec());
                }
            }
        }
    }
    // Foreign sessions: a sessions.json index (`spawnedBy` names a subagent's parent).
    let has_index = set.iter().any(|l| native_source(l) && l.agent().source.as_ref().is_some_and(|s| s.extra.contains_key("index_files")));
    // OpenClaw only imports transcripts its sessions.json index references.
    if !has_index && set.iter().any(|l| !native_source(l)) {
        let key = |l: &Ledger| -> String {
            if l.agent().parent.is_some() {
                format!("agent:main:subagent:{}", session_id_of(l))
            } else {
                format!("agent:main:protoledger:{}", session_id_of(l))
            }
        };
        let mut idx = Map::new();
        for l in set.iter() {
            let mut v = json!({"sessionId": session_id_of(l), "updatedAt": l.events.iter().rev().find_map(|e| e.ts.as_deref()).and_then(util::parse_rfc3339_ms).unwrap_or(0)});
            if let Some(p) = l.agent().parent.as_ref().filter(|p| p.relation == Relation::Subagent) {
                match set.get(&p.agent) {
                    Some(pl) => {
                        v["spawnedBy"] = json!(key(pl));
                        v["spawnedBySessionId"] = json!(session_id_of(pl));
                    }
                    None => losses.add("subagent-flattened", Some(Kind::Subagent), "the parent session is not in the set"),
                }
            }
            idx.insert(key(l), v);
        }
        output.put("sessions.json", jsonl::pretty(&Value::Object(idx)).into_bytes());
    }
    Ok(Exported { output, losses: losses.sorted(), variant: "session-v4".into() })
}

fn native_source(l: &Ledger) -> bool {
    l.agent().source.as_ref().is_some_and(|s| s.format == FORMAT)
}

enum Item {
    Raw(String),
    Entry(Value, Vec<Event>),
}

struct Builder<'a> {
    ledger: &'a Ledger,
    items: Vec<Item>,
    /// id of the last entry written, for `parentId`.
    last_id: Option<String>,
    used_ids: HashSet<String>,
    /// The entry id each rendered event went into, so an event that continues
    /// from an earlier one (SPEC §2.3 `after`) can name that entry as its parent.
    entry_of: HashMap<String, String>,
    /// open assistant message: item index and group.
    open_asst: Option<(usize, Option<String>)>,
    open_user: Option<(usize, Option<String>)>,
    last_asst: Option<usize>,
    pending: Vec<Event>,
    model: String,
    provider: String,
    call_names: HashMap<String, String>,
    /// compaction entries waiting for the next entry's id as firstKeptEntryId.
    kept_fixups: Vec<usize>,
    losses: &'a mut Losses,
}

fn session_id_of(l: &Ledger) -> String {
    l.agent()
        .source
        .as_ref()
        .filter(|s| s.format == FORMAT)
        .and_then(|s| s.native_id.clone())
        .unwrap_or_else(|| if jsonl::is_safe_id(l.id()) { l.id().to_string() } else { util::uuid_for(l.id()) })
}

fn export_ledger(l: &Ledger, set: &LedgerSet, opts: &ExportOptions, losses: &mut Losses) -> (String, Vec<u8>) {
    let src = l.agent().source.as_ref().filter(|s| s.format == FORMAT);
    let session_id = session_id_of(l);
    let path =
        src.and_then(|s| s.extra.get("file")).and_then(Value::as_str).map(str::to_string).unwrap_or_else(|| format!("{}.jsonl", ledger_file(&session_id)));
    let final_newline = src.and_then(|s| s.extra.get("final_newline")).and_then(Value::as_bool).unwrap_or(true);
    let counts = jsonl::part_counts(l, FORMAT);
    let mut b = Builder {
        ledger: l,
        items: Vec::new(),
        last_id: None,
        used_ids: HashSet::new(),
        entry_of: HashMap::new(),
        open_asst: None,
        open_user: None,
        last_asst: None,
        pending: Vec::new(),
        model: "unknown".into(),
        provider: "unknown".into(),
        call_names: HashMap::new(),
        kept_fixups: Vec::new(),
        losses,
    };
    let has_native_header = l.events.iter().any(|e| {
        e.native.as_ref().is_some_and(|n| n.format == FORMAT && n.part == 0)
            && e.native.as_ref().and_then(|n| n.record_json()).is_some_and(|r| str_at(&r, "type") == Some("session"))
    });
    // A ledger imported from OpenClaw keeps its own first line, even a corrupt one.
    if !has_native_header && src.is_none() {
        let ts = l.agent().created_at.clone().or_else(|| l.events.iter().find_map(|e| e.ts.clone())).unwrap_or_else(|| util::ms_to_rfc3339(0));
        let cwd = l.events.iter().filter(|e| e.kind == Kind::Settings).find_map(|e| e.str("cwd").map(str::to_string));
        let mut h = json!({"type":"session","version": EXPORT_VERSION,"id": session_id,"timestamp": ts,"cwd": cwd.unwrap_or_default()});
        let fork = l.agent().parent.as_ref().filter(|p| p.relation == Relation::Fork).map(|p| p.agent.clone());
        let link = l.agent().links.iter().find(|k| k.rel == "forked-from");
        if let Some(path) = link.and_then(|k| k.extra.get("path")).and_then(Value::as_str) {
            h["parentSession"] = json!(path);
        } else if let Some(p) = fork.or_else(|| link.and_then(|k| k.agent.clone())) {
            let psid = set.get(&p).map(session_id_of).unwrap_or(p);
            h["parentSession"] = json!(format!("{}.jsonl", ledger_file(&psid)));
        }
        if opts.carry {
            jsonl::attach(&mut h, &[], Some(l.agent()));
        }
        b.items.push(Item::Raw(serde_json::to_string(&h).unwrap()));
    }
    let mut done: HashSet<String> = HashSet::new();
    for (i, e) in l.events.iter().enumerate() {
        if let Some(n) = e.native.as_ref().filter(|n| n.format == FORMAT) {
            let nid = n.id.clone().unwrap_or_default();
            if n.part == 0 {
                done.remove(&nid);
                let want = n.record_json().as_ref().map(|r| record_events(r, &e.id).len()).unwrap_or(1);
                if counts[i] == want
                    && let Some(line) = jsonl::line_of(n)
                {
                    b.flush();
                    b.items.push(Item::Raw(line));
                    if let Some(id) = n.record_json().as_ref().and_then(|r| str_at(r, "id"))
                        && n.record_json().as_ref().and_then(|r| str_at(r, "type")) != Some("session")
                    {
                        b.set_last(id.to_string());
                        b.entry_of.insert(e.id.clone(), id.to_string());
                    }
                    done.insert(nid.clone());
                    continue;
                }
            } else if done.contains(&nid) {
                if let Some(id) = b.last_id.clone() {
                    b.entry_of.insert(e.id.clone(), id);
                }
                continue;
            }
        }
        b.push(e);
    }
    b.finish(opts.carry);
    let mut text = String::new();
    for it in &b.items {
        match it {
            Item::Raw(s) => text.push_str(s),
            Item::Entry(v, _) => text.push_str(&serde_json::to_string(v).unwrap()),
        }
        text.push('\n');
    }
    if !final_newline && text.ends_with('\n') {
        text.pop();
    }
    if src.and_then(|s| s.extra.get("bom")).and_then(Value::as_bool) == Some(true) {
        text.insert(0, '\u{feff}');
    }
    (path, text.into_bytes())
}

fn ledger_file(id: &str) -> String {
    crate::ledger::file_safe(id)
}

fn ms_of(ts: Option<&str>) -> i64 {
    ts.and_then(util::parse_rfc3339_ms).unwrap_or(0)
}

impl Builder<'_> {
    fn set_last(&mut self, id: String) {
        for i in self.kept_fixups.drain(..) {
            if let Item::Entry(v, _) = &mut self.items[i] {
                v["firstKeptEntryId"] = Value::String(id.clone());
            }
        }
        self.used_ids.insert(id.clone());
        self.last_id = Some(id);
    }

    /// OpenClaw's own writer uses `randomUUID()` entry ids; ours are v5.
    fn mint(&mut self, e: &Event) -> String {
        let mut name = format!("{}\u{0}{}", self.ledger.id(), e.id);
        loop {
            let id = util::uuid_for(&name);
            if !self.used_ids.contains(&id) {
                return id;
            }
            name.push('~');
        }
    }

    /// An entry in OpenClaw's writer order: payload first, then id, parentId, timestamp.
    fn base_entry(&self, ty: &str, body: Vec<(&str, Value)>, id: &str, ts: String) -> Value {
        let mut m = Map::new();
        m.insert("type".into(), json!(ty));
        for (k, v) in body {
            m.insert(k.into(), v);
        }
        m.insert("id".into(), json!(id));
        m.insert("parentId".into(), self.last_id.clone().map(Value::String).unwrap_or(Value::Null));
        m.insert("timestamp".into(), json!(ts));
        Value::Object(m)
    }

    /// Start a new entry for event `e`; returns its item index.
    fn entry(&mut self, e: &Event, ty: &str, body: Vec<(&str, Value)>) -> usize {
        let id = self.mint(e);
        let mut v = self.base_entry(ty, body, &id, self.ts(e));
        // SPEC §2.3: an event that continues from an earlier one than the
        // previous entry hangs off that event's entry. The entries between
        // stay in the file as a branch; OpenClaw (pi's session tree) resumes
        // from the last entry, so it follows the active path.
        if let Some(p) = e.after.as_deref().and_then(|a| self.entry_of.get(a)).filter(|p| self.last_id.as_ref() != Some(*p)) {
            v["parentId"] = json!(p);
        }
        let mut evs = std::mem::take(&mut self.pending);
        evs.push(e.clone());
        for x in &evs {
            self.entry_of.insert(x.id.clone(), id.clone());
        }
        self.items.push(Item::Entry(v, evs));
        self.set_last(id);
        self.items.len() - 1
    }

    fn ts(&self, e: &Event) -> String {
        e.ts.clone()
            .or_else(|| {
                let pos = self.ledger.events.iter().position(|x| x.id == e.id).unwrap_or(0);
                self.ledger.events[..pos].iter().rev().find_map(|x| x.ts.clone())
            })
            .or_else(|| self.ledger.agent().created_at.clone())
            .unwrap_or_else(|| util::ms_to_rfc3339(0))
    }

    fn flush(&mut self) {
        self.open_asst = None;
        self.open_user = None;
    }

    fn add_events(&mut self, idx: usize, e: &Event) {
        let pending = std::mem::take(&mut self.pending);
        if let Item::Entry(v, evs) = &mut self.items[idx] {
            if let Some(id) = v.get("id").and_then(Value::as_str) {
                for x in pending.iter().chain([e]) {
                    self.entry_of.insert(x.id.clone(), id.to_string());
                }
            }
            evs.extend(pending);
            evs.push(e.clone());
        }
    }

    fn msg(&mut self, idx: usize) -> &mut Value {
        match &mut self.items[idx] {
            Item::Entry(v, _) => &mut v["message"],
            Item::Raw(_) => unreachable!(),
        }
    }

    fn defer(&mut self, e: &Event, code: &str, detail: &str) {
        self.losses.add(code, Some(e.kind), detail);
        self.pending.push(e.clone());
    }

    fn assistant(&mut self, e: &Event, block: Value) {
        let same = self.open_asst.as_ref().is_some_and(|(_, g)| g == &e.group);
        if same {
            let idx = self.open_asst.as_ref().unwrap().0;
            self.add_events(idx, e);
            self.msg(idx)["content"].as_array_mut().unwrap().push(block);
            return;
        }
        self.flush();
        let ms = ms_of(Some(&self.ts(e)));
        let message = json!({
            "role":"assistant","content":[block],"api":"unknown","provider": self.provider,"model": self.model,
            "usage": {"input":0,"output":0,"cacheRead":0,"cacheWrite":0,"totalTokens":0,
                      "cost":{"input":0,"output":0,"cacheRead":0,"cacheWrite":0,"total":0}},
            "stopReason":"stop","timestamp": ms
        });
        let idx = self.entry(e, "message", vec![("message", message)]);
        self.open_asst = Some((idx, e.group.clone()));
        self.last_asst = Some(idx);
    }

    fn user(&mut self, e: &Event, block: Value) {
        let same = self.open_user.as_ref().is_some_and(|(_, g)| g == &e.group && g.is_some());
        if same {
            let idx = self.open_user.as_ref().unwrap().0;
            self.add_events(idx, e);
            self.msg(idx)["content"].as_array_mut().unwrap().push(block);
            return;
        }
        self.flush();
        let ms = ms_of(Some(&self.ts(e)));
        let idx = self.entry(e, "message", vec![("message", json!({"role":"user","content":[block],"timestamp": ms}))]);
        self.open_user = Some((idx, e.group.clone()));
    }

    fn push(&mut self, e: &Event) {
        let b = &e.body;
        let s = |k: &str| b.get(k).and_then(Value::as_str).map(str::to_string);
        match e.kind {
            Kind::Text if e.effective_audience() == Audience::Record => {
                // Shown to people only: a custom entry, which pi keeps out of context.
                self.flush();
                self.entry(e, "custom", vec![("customType", json!(RECORD_TEXT)), ("data", json!({"role": s("role"), "text": s("text")}))]);
            }
            Kind::Text => match s("role").as_deref() {
                Some("assistant") => self.assistant(e, json!({"type":"text","text": s("text").unwrap_or_default()})),
                Some("user") | None => self.user(e, json!({"type":"text","text": s("text").unwrap_or_default()})),
                Some(other) => {
                    // pi sessions hold no system/developer messages; a custom
                    // message is the context-visible carrier pi offers.
                    self.losses.add("system-text-as-custom-message", Some(Kind::Text), "system/developer text becomes a custom_message");
                    self.flush();
                    let ms = ms_of(Some(&self.ts(e)));
                    let _ = ms;
                    self.entry(
                        e,
                        "custom_message",
                        vec![
                            ("customType", json!(format!("protoledger.{other}"))),
                            ("content", json!(s("text").unwrap_or_default())),
                            ("display", json!(false)),
                        ],
                    );
                }
            },
            Kind::Media => {
                let block = match (b.get("data").and_then(Value::as_str), s("media_type")) {
                    (Some(d), Some(mt)) if mt.starts_with("image/") => json!({"type":"image","data": d,"mimeType": mt}),
                    _ => {
                        self.defer(e, "media-dropped", "pi holds only inline base64 images");
                        return;
                    }
                };
                if s("role").as_deref() == Some("assistant") {
                    self.defer(e, "media-dropped", "pi assistant messages cannot hold images");
                } else {
                    self.user(e, block);
                }
            }
            Kind::Reasoning => {
                let mut block = json!({"type":"thinking","thinking": s("text").unwrap_or_default()});
                if b.get("redacted").and_then(Value::as_bool) == Some(true) {
                    block["redacted"] = json!(true);
                }
                if let Some(sealed) = b.get("sealed") {
                    match sealed.get("data").and_then(Value::as_str) {
                        Some(d) if sealed.get("kind").and_then(Value::as_str) == Some("signature") => {
                            block["thinkingSignature"] = json!(d);
                            if self.provider == "unknown"
                                && let Some(v) = sealed.get("vendor").and_then(Value::as_str)
                            {
                                self.provider = v.to_string();
                            }
                        }
                        _ => self.losses.add("reasoning-sealed-dropped", Some(Kind::Reasoning), "encrypted reasoning has no pi slot"),
                    }
                }
                if s("text").is_none() && b.get("summary").is_some() {
                    let sum: Vec<String> =
                        b["summary"].as_array().map(|a| a.iter().filter_map(|x| x.as_str().map(str::to_string)).collect()).unwrap_or_default();
                    block["thinking"] = json!(sum.join("\n"));
                }
                self.assistant(e, block);
            }
            Kind::ToolCall => {
                let call_id = s("call_id").unwrap_or_else(|| e.id.clone());
                let name = s("name").unwrap_or_else(|| "tool".into());
                self.call_names.insert(call_id.clone(), name.clone());
                let args = match (b.get("input"), s("input_text")) {
                    (Some(v @ Value::Object(_)), _) => v.clone(),
                    (_, Some(t)) => {
                        self.losses.add("tool-input-text-wrapped", Some(Kind::ToolCall), "raw tool input becomes {\"input\": text}");
                        json!({"input": t})
                    }
                    (Some(Value::Null) | None, None) => json!({}),
                    (Some(other), None) => {
                        self.losses.add("tool-input-text-wrapped", Some(Kind::ToolCall), "non-object tool input becomes {\"input\": value}");
                        json!({"input": other})
                    }
                };
                self.assistant(e, json!({"type":"toolCall","id": call_id,"name": name,"arguments": args}));
                if let Some(idx) = self.open_asst.as_ref().map(|x| x.0) {
                    self.msg(idx)["stopReason"] = json!("toolUse");
                }
            }
            Kind::ToolResult => {
                self.flush();
                let call_id = s("call_id").unwrap_or_default();
                let name = self.call_names.get(&call_id).cloned().unwrap_or_else(|| "tool".into());
                let content = match b.get("output") {
                    Some(Value::Array(parts)) => Value::Array(
                        parts
                            .iter()
                            .filter_map(|p| match p.get("type").and_then(Value::as_str) {
                                Some("text") => Some(json!({"type":"text","text": p.get("text")})),
                                Some("media") if p.get("data").is_some() => Some(json!({"type":"image","data": p.get("data"),"mimeType": p.get("media_type")})),
                                _ => None,
                            })
                            .collect(),
                    ),
                    other => json!([{"type":"text","text": jsonl::output_text(other)}]),
                };
                let ms = ms_of(Some(&self.ts(e)));
                let message = json!({"role":"toolResult","toolCallId": call_id,"toolName": name,"content": content,
                    "isError": b.get("is_error").and_then(Value::as_bool).unwrap_or(false),"timestamp": ms});
                self.entry(e, "message", vec![("message", message)]);
            }
            // SPEC §3.1.1: running totals are never written as one response's usage.
            Kind::Usage if e.body.get("cumulative").and_then(Value::as_bool) == Some(true) => {
                self.defer(e, "usage-cumulative-dropped", "running totals have no OpenClaw field; messages hold per-response usage only")
            }
            Kind::Usage => {
                let target = self.last_asst.filter(|i| match &self.items[*i] {
                    Item::Entry(v, evs) => v["message"]["usage"]["totalTokens"] == json!(0) && !evs.iter().any(|x| x.kind == Kind::Usage),
                    _ => false,
                });
                match target {
                    Some(idx) => {
                        let n = |k: &str| b.get(k).cloned().filter(|v| v.is_number()).unwrap_or(json!(0));
                        let total =
                            ["input", "output", "cache_read", "cache_write"].iter().map(|k| b.get(*k).and_then(Value::as_u64).unwrap_or(0)).sum::<u64>();
                        let mut u = json!({"input": n("input"),"output": n("output"),"cacheRead": n("cache_read"),
                            "cacheWrite": n("cache_write"),"totalTokens": total,
                            "cost":{"input":0,"output":0,"cacheRead":0,"cacheWrite":0,"total": n("cost_usd")}});
                        if let Some(r) = b.get("reasoning").filter(|v| v.is_number()) {
                            u["reasoning"] = r.clone();
                        }
                        if let Some(m) = s("model") {
                            self.msg(idx)["model"] = json!(m);
                        }
                        self.msg(idx)["usage"] = u;
                        self.add_events(idx, e);
                    }
                    None => self.defer(e, "usage-unattached", "usage with no assistant message to hold it"),
                }
            }
            Kind::Settings => {
                let mut wrote = false;
                if let Some(m) = s("model") {
                    self.model = m.clone();
                    if let Some(p) = s("provider") {
                        self.provider = p;
                    }
                    self.flush();
                    let provider = self.provider.clone();
                    self.entry(e, "model_change", vec![("provider", json!(provider)), ("modelId", json!(m))]);
                    wrote = true;
                }
                if let Some(eff) = s("effort") {
                    self.flush();
                    if wrote {
                        // second entry for the same event: carry it on the first only.
                        let id = self.mint(&Event::new(format!("{}#effort", e.id), Kind::Settings));
                        let v = self.base_entry("thinking_level_change", vec![("thinkingLevel", json!(eff))], &id, self.ts(e));
                        self.items.push(Item::Entry(v, vec![]));
                        self.set_last(id);
                    } else {
                        self.entry(e, "thinking_level_change", vec![("thinkingLevel", json!(eff))]);
                        wrote = true;
                    }
                }
                if !wrote {
                    // cwd lives in the header; the rest has no pi slot.
                    let only_cwd = b.keys().all(|k| k == "cwd" || k == "complete");
                    if only_cwd {
                        self.pending.push(e.clone());
                    } else {
                        self.defer(e, "settings-dropped", "settings other than model/effort/cwd have no pi entry");
                    }
                }
            }
            Kind::Compaction => {
                if s("phase").as_deref() == Some("summary") || b.get("summary").is_some() {
                    self.flush();
                    let idx = self.entry(
                        e,
                        "compaction",
                        vec![
                            ("summary", json!(s("summary").unwrap_or_default())),
                            ("firstKeptEntryId", Value::Null),
                            ("tokensBefore", b.get("tokens_before").cloned().filter(|v| v.is_number()).unwrap_or(json!(0))),
                        ],
                    );
                    // firstKeptEntryId: the entry after it, or itself if none.
                    let own = self.last_id.clone();
                    if let Item::Entry(v, _) = &mut self.items[idx] {
                        v["firstKeptEntryId"] = json!(own);
                    }
                    self.kept_fixups.push(idx);
                } else {
                    self.defer(e, "compaction-boundary-dropped", "a compaction boundary without a summary has no pi entry");
                }
            }
            Kind::Title => {
                self.flush();
                self.entry(e, "session_info", vec![("name", json!(s("text").unwrap_or_default()))]);
            }
            Kind::Error => {
                let target = self.open_asst.as_ref().map(|x| x.0).or(self.last_asst);
                match target {
                    Some(idx) if self.msg(idx).get("errorMessage").is_none() => {
                        let m = s("message").unwrap_or_default();
                        let v = self.msg(idx);
                        v["stopReason"] = json!("error");
                        v["errorMessage"] = json!(m);
                        self.add_events(idx, e);
                    }
                    _ => self.defer(e, "error-dropped", "an error with no assistant message to hold it"),
                }
            }
            Kind::Turn => {
                if s("outcome").as_deref() == Some("aborted")
                    && let Some(idx) = self.open_asst.as_ref().map(|x| x.0).or(self.last_asst)
                {
                    self.msg(idx)["stopReason"] = json!("aborted");
                    self.add_events(idx, e);
                    return;
                }
                // Turn markers are implicit in pi.
                self.pending.push(e.clone());
            }
            Kind::PeerMessage => {
                // SPEC §3.1.6: wrapped text, a user message when incoming,
                // assistant text when outgoing; the importer unwraps it.
                let block = json!({"type":"text","text": crate::formats::pi_session::peer_wrap(e)});
                if s("direction").as_deref() == Some("out") { self.assistant(e, block) } else { self.user(e, block) }
            }
            Kind::App if s("type").as_deref() == Some("pi/branch_summary") => {
                self.flush();
                let d = b.get("data").cloned().unwrap_or(Value::Null);
                self.entry(
                    e,
                    "branch_summary",
                    vec![("fromId", d.get("from").cloned().unwrap_or(Value::Null)), ("summary", d.get("summary").cloned().unwrap_or(json!("")))],
                );
            }
            Kind::Subagent => self.defer(e, "subagent-marker-dropped", "pi has no subagent markers"),
            Kind::FileChange => self.defer(e, "file-change-dropped", "pi has no file-change records"),
            Kind::App => self.defer(e, "app-dropped", "application records have no pi entry"),
            Kind::Native => self.defer(e, "foreign-native-dropped", "another harness's native record"),
        }
    }

    fn finish(&mut self, carry: bool) {
        if !self.pending.is_empty() {
            let last_entry = self.items.iter().rposition(|i| matches!(i, Item::Entry(..)));
            match last_entry {
                Some(i) => {
                    let p = std::mem::take(&mut self.pending);
                    if let Item::Entry(_, evs) = &mut self.items[i] {
                        evs.extend(p);
                    }
                }
                None if carry => {
                    // Nothing foreign was rendered: a custom entry holds them.
                    let e = Event::new("protoledger-carrier", Kind::Native);
                    let id = self.mint(&e);
                    let p = std::mem::take(&mut self.pending);
                    let ts = p.iter().find_map(|x| x.ts.clone()).unwrap_or_else(|| self.ts(&p[0]));
                    let v = self.base_entry("custom", vec![("customType", json!("protoledger"))], &id, ts);
                    self.items.push(Item::Entry(v, p));
                }
                None => {}
            }
        }
        if carry {
            for it in &mut self.items {
                if let Item::Entry(v, evs) = it {
                    if evs.is_empty() {
                        jsonl::mark(v);
                    } else {
                        jsonl::attach(v, evs, None);
                    }
                }
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// SPEC §3.1.1: a running total is not written as a response's usage;
    /// a per-response usage is.
    #[test]
    fn cumulative_usage_is_not_written_as_a_response() {
        let run = |cumulative: bool| {
            let mut l = Ledger::new(Agent {
                id: "u".into(),
                created_at: Some("2026-01-01T00:00:00Z".into()),
                source: None,
                parent: None,
                links: vec![],
                meta: Obj::new(),
            });
            l.append(Event::new("a", Kind::Text).with("role", "assistant").with("text", "hi").group(Some("g".to_string()))).unwrap();
            l.append(Event::new("n", Kind::Usage).with("input", 7).with("output", 5).with("cumulative", cumulative).group(Some("g".to_string()))).unwrap();
            let out = OpenClaw.export(&LedgerSet::single(l), &ExportOptions::no_carry()).unwrap();
            let text: String = out.output.files.iter().filter(|(k, _)| k.ends_with(".jsonl")).map(|(_, b)| String::from_utf8_lossy(b).to_string()).collect();
            let named = out.losses.0.iter().any(|x| x.code == "usage-cumulative-dropped");
            (text.contains("\"input\":7"), named)
        };
        assert_eq!(run(true), (false, true), "a running total is dropped, and named");
        assert_eq!(run(false), (true, false), "opposed: a response's usage is written");
    }

    /// SPEC §2.3, decision "the target expresses the branch": a rewound
    /// ledger is written as an entry tree whose last entry continues from the
    /// event `after` names, so OpenClaw resumes on the active path, and the
    /// abandoned branch comes back as `after` (nothing is dropped).
    #[test]
    fn a_rewound_ledger_is_written_as_a_branch_of_the_entry_tree() {
        let fx = |c: &str| {
            let p = concat!(env!("CARGO_MANIFEST_DIR"), "/fixtures/protoledger/").to_string() + c + "/native";
            crate::formats::by_name("protoledger").unwrap().import(Path::new(&p), &ImportOptions::standard()).unwrap().set
        };
        let texts = |l: &Ledger, evs: Vec<&Event>| -> Vec<String> {
            let _ = l;
            evs.iter().filter_map(|e| e.str("text").map(String::from)).collect()
        };
        let trip = |set: &LedgerSet| -> Ledger {
            let out = OpenClaw.export(set, &ExportOptions::no_carry()).unwrap();
            let t = tempfile::tempdir().unwrap();
            let path = crate::roundtrip::materialise(&out.output, t.path()).unwrap();
            let back = OpenClaw.import(&path, &ImportOptions::standard()).unwrap().set;
            back.iter().next().unwrap().clone()
        };
        let src = fx("branch-rewind");
        let l0 = src.iter().next().unwrap();
        assert!(l0.is_branched(), "sentinel: the source is rewound");
        let l1 = trip(&src);
        let said = |l: &Ledger| l.events.iter().filter(|e| e.kind != Kind::Settings).count();
        assert_eq!(said(&l1), said(l0), "every event is written, the abandoned branch too");
        assert!(l1.is_branched(), "the branch comes back");
        assert_eq!(texts(&l1, l1.active_path()), texts(l0, l0.active_path()), "the active path is the source's");
        // Opposed: an unbranched ledger stays one chain.
        let flat = fx("minimal");
        assert!(!trip(&flat).is_branched(), "no branch where the source has none");
    }

    const SAMPLE: &str = concat!(
        r#"{"type":"session","version":3,"id":"s1","timestamp":"2026-01-01T00:00:00.000Z","cwd":"/home/user/p"}"#,
        "\n",
        r#"{"type":"message","id":"a1","parentId":null,"timestamp":"2026-01-01T00:00:01.000Z","message":{"role":"user","content":[{"type":"text","text":"hi"}],"timestamp":1767225601000}}"#,
        "\n",
        r#"{"type":"message","id":"a2","parentId":"a1","timestamp":"2026-01-01T00:00:02.000Z","message":{"role":"assistant","content":[{"type":"thinking","thinking":"t","thinkingSignature":"sig"},{"type":"toolCall","id":"c1","name":"bash","arguments":{"command":"ls"}}],"api":"anthropic-messages","provider":"anthropic","model":"m","usage":{"input":1,"output":2,"cacheRead":0,"cacheWrite":0,"totalTokens":3,"cost":{"input":0,"output":0,"cacheRead":0,"cacheWrite":0,"total":0.01}},"stopReason":"toolUse","timestamp":1767225602000}}"#,
        "\n",
        r#"{"type":"message","id":"a3","parentId":"a1","timestamp":"2026-01-01T00:00:03.000Z","message":{"role":"toolResult","toolCallId":"c1","toolName":"bash","content":[{"type":"text","text":"x"}],"isError":false,"timestamp":1767225603000}}"#,
        "\n",
        "{\"type\":\"message\"\n",
    );

    fn imp(text: &str) -> Ledger {
        let mut losses = Losses::default();
        import_text(text, "s1.jsonl", &ImportOptions::standard(), &mut losses).unwrap().0
    }

    #[test]
    fn import_maps_kinds_tree_and_parts() {
        let l = imp(SAMPLE);
        let kinds: Vec<Kind> = l.events.iter().map(|e| e.kind).collect();
        assert_eq!(kinds, vec![Kind::Settings, Kind::Text, Kind::Reasoning, Kind::ToolCall, Kind::Usage, Kind::ToolResult, Kind::Native]);
        assert_eq!(l.events[3].id, "a2#1");
        assert_eq!(l.events[2].body["sealed"]["data"], "sig");
        // a3's parent is a1, not the previous entry a2.
        assert_eq!(l.events[5].after.as_deref(), Some("a1"));
        assert_eq!(l.events[6].id, format!("@{}", SAMPLE.rfind("{\"type\":\"message\"\n").unwrap()));
    }

    #[test]
    fn self_export_is_exact() {
        let l = imp(SAMPLE);
        let set = LedgerSet::single(l);
        let out = export(&set, &ExportOptions::standard()).unwrap();
        assert_eq!(String::from_utf8(out.output.files["s1.jsonl"].clone()).unwrap(), SAMPLE);
    }

    #[test]
    fn records_are_held_and_reemitted_as_written() {
        use super::jsonl::spelling::{ODD, assert_value_loses, held};
        use crate::formats::atif::jsonfmt::without_raw;
        let header = r#"{"type":"session","version":3,"id":"s1","timestamp":"2026-01-01T00:00:00.000Z","cwd":"/home/user/p"}"#;
        let line = format!(
            r#"{{"type":"message","id":"a1","parentId":null,"timestamp":"2026-01-01T00:00:01.000Z","message":{{"role":"user","content":[{{"type":"text","text":"hi"}}],"timestamp":1767225601000}},{ODD}}}"#
        );
        assert_value_loses(&line);
        let src = format!("{header}\n{line}\n");
        let set = LedgerSet::single(imp(&src));
        assert_eq!(held(&set, FORMAT), [header, line.as_str()], "the ledger holds each line as written");
        for s in [set.clone(), without_raw(&set)] {
            let out = export(&s, &ExportOptions::standard()).unwrap();
            assert_eq!(String::from_utf8(out.output.files["s1.jsonl"].clone()).unwrap(), src);
        }
    }

    #[test]
    fn foreign_export_reimports_and_carry_restores() {
        let mut l = Ledger::new(Agent {
            id: "foreign-1".into(),
            created_at: Some("2026-01-01T00:00:00.000Z".into()),
            source: None,
            parent: None,
            links: vec![],
            meta: Obj::new(),
        });
        l.append(Event::new("u1", Kind::Text).with("role", "user").with("text", "hello").ts(Some("2026-01-01T00:00:01.000Z"))).unwrap();
        l.append(Event::new("r1", Kind::Reasoning).with("text", "hm").group(Some("g"))).unwrap();
        l.append(Event::new("c1", Kind::ToolCall).with("call_id", "k").with("name", "ls").with("input", json!({})).group(Some("g"))).unwrap();
        l.append(Event::new("x1", Kind::FileChange).with("path", "a").with("op", "edit")).unwrap();
        l.append(Event::new("t1", Kind::ToolResult).with("call_id", "k").with("output", "ok")).unwrap();
        let set = LedgerSet::single(l.clone());
        for carry in [true, false] {
            let opts = if carry { ExportOptions::standard() } else { ExportOptions::no_carry() };
            let out = export(&set, &opts).unwrap();
            let text = String::from_utf8(out.output.files["foreign-1.jsonl"].clone()).unwrap();
            let mut losses = Losses::default();
            let iopts = ImportOptions { variant: None, restore_carried: carry };
            let back = import_text(&text, "foreign-1.jsonl", &iopts, &mut losses).unwrap().0;
            let d = crate::loss::compare(&set, &LedgerSet::single(back), crate::loss::CompareOptions { ignore_native: true, ..Default::default() });
            if carry {
                assert!(d.is_clean(), "{}", d.summary());
            } else {
                assert!(d.missing.get("file_change") == Some(&1), "{}", d.summary());
            }
        }
    }
}
