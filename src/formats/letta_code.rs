//! Letta Code (`letta-ai/letta-code`) client transcripts:
//! `~/.letta/transcripts/<agent>/<conversation>/transcript.jsonl`.
//!
//! Letta Code appends one batch of rows per completed turn, each row
//! `JSON.stringify`'d on its own line (`src/cli/helpers/reflection-transcript.ts`).
//! Five row kinds exist: `user`, `assistant`, `reasoning`, `error`, `tool_call`
//! (a tool call and its result share one row). The file records no producer
//! version; the row shape tells the era (see `docs/formats/letta-code.md`).
//!
//! The local backend's store (`~/.letta/lc-local-backend/conversations/*/messages.jsonl`)
//! is pi session-entry JSONL and is imported by the shared pi-session module.

use super::pi_session::{self, Dialect};
use crate::carry;
use crate::formats::atif::jsonfmt;
use crate::formats::{Detection, ExportOptions, Exported, Format, ImportOptions, Imported, Losses, Output, Variant};
use crate::ledger::{Ledger, LedgerSet};
use crate::model::{Agent, Event, Kind, Native, Obj, Parent, Source};
use crate::util;
use anyhow::{Context, Result, bail};
use serde_json::{Map, Value, json};
use std::collections::HashMap;
use std::path::{Path, PathBuf};

pub struct LettaCode;

const FORMAT: &str = "letta-code";

const VARIANTS: &[Variant] = &[
    Variant {
        name: "transcript-v0",
        versions: ">= 0.18.3, < 0.22.0",
        import: true,
        export: false,
        notes: "rows carry no ids (kind, text | name/argsText/resultText/resultOk, captured_at)",
    },
    Variant { name: "transcript-v1", versions: ">= 0.22.0, < 0.23.11", import: true, export: false, notes: "source_line_id on every row" },
    Variant {
        name: "transcript",
        versions: ">= 0.23.11 (current, 0.34.x)",
        import: true,
        export: true,
        notes: "source_message_id on user/assistant/reasoning rows when the backend echoed one",
    },
    Variant {
        name: "transcript-external",
        versions: ">= 0.27.28, < 0.31.6",
        import: true,
        export: false,
        notes: "`letta dream --from` rows: source_message_id on every kind, no source_line_id",
    },
    Variant {
        name: "local-backend",
        versions: ">= 0.26.4 (manifest schema_version 2)",
        import: true,
        export: true,
        notes: "lc-local-backend conversations/<b64>/messages.jsonl: pi session v3 entries with message snapshots; exported back only for ledgers imported from it",
    },
];

const KINDS: [&str; 5] = ["user", "assistant", "reasoning", "error", "tool_call"];

impl Format for LettaCode {
    fn name(&self) -> &'static str {
        FORMAT
    }
    fn describe(&self) -> &'static str {
        "Letta Code client transcripts (~/.letta/transcripts/<agent>/<conversation>/transcript.jsonl)"
    }
    fn variants(&self) -> &'static [Variant] {
        VARIANTS
    }
    fn detect(&self, input: &Path) -> Option<Detection> {
        if input.is_dir() {
            if !local_backend_dirs(input).is_empty() {
                return Some(Detection { variant: "local-backend", confidence: 90, version: None });
            }
            let files = transcript_files(input);
            let f = files.first()?;
            return detect_file(f).or(Some(Detection { variant: "transcript", confidence: 50, version: None }));
        }
        detect_file(input)
    }
    fn import(&self, input: &Path, opts: &ImportOptions) -> Result<Imported> {
        if input.is_dir() && !local_backend_dirs(input).is_empty() {
            return import_local_backend(input, opts);
        }
        import_transcripts(input, opts)
    }
    fn export(&self, set: &LedgerSet, opts: &ExportOptions) -> Result<Exported> {
        export(set, opts)
    }
}

fn detect_file(p: &Path) -> Option<Detection> {
    let text = std::fs::read_to_string(p).ok()?;
    let named = p.file_name().is_some_and(|n| n == "transcript.jsonl");
    let mut hits = 0;
    let mut misses = 0;
    for line in text.lines().take(50) {
        match serde_json::from_str::<Value>(line) {
            Ok(v) if v.get("kind").and_then(Value::as_str).is_some_and(|k| KINDS.contains(&k)) && v.get("captured_at").is_some() => hits += 1,
            Ok(v) if v.get("type").is_some() => misses += 1,
            _ => {}
        }
    }
    if hits == 0 && !(named && text.is_empty()) {
        return None;
    }
    if misses > hits {
        return None;
    }
    Some(Detection { variant: variant_of(&text), confidence: if named { 90 } else { 75 }, version: None })
}

fn transcript_files(dir: &Path) -> Vec<PathBuf> {
    let mut v: Vec<PathBuf> = walkdir::WalkDir::new(dir)
        .sort_by_file_name()
        .into_iter()
        .filter_map(|e| e.ok())
        .filter(|e| e.file_type().is_file() && e.file_name() == "transcript.jsonl")
        .map(|e| e.into_path())
        .collect();
    v.sort();
    v
}

/// The newest row shape in the file (rows of several eras can mix: the file is append-only).
fn variant_of(text: &str) -> &'static str {
    let (mut line_ids, mut msg_ids, mut tool_msg_ids) = (false, false, false);
    for line in text.lines() {
        let Ok(v) = serde_json::from_str::<Value>(line) else { continue };
        line_ids |= v.get("source_line_id").is_some();
        msg_ids |= v.get("source_message_id").is_some();
        tool_msg_ids |= v.get("kind").and_then(Value::as_str) == Some("tool_call") && v.get("source_message_id").is_some();
    }
    if tool_msg_ids && !line_ids {
        "transcript-external"
    } else if msg_ids {
        "transcript"
    } else if line_ids {
        "transcript-v1"
    } else {
        "transcript-v0"
    }
}

// ---------------------------------------------------------------------------
// Import: client transcripts
// ---------------------------------------------------------------------------

fn import_transcripts(input: &Path, opts: &ImportOptions) -> Result<Imported> {
    let (root, files) = if input.is_dir() {
        (input.to_path_buf(), transcript_files(input))
    } else {
        (input.parent().unwrap_or(Path::new(".")).to_path_buf(), vec![input.to_path_buf()])
    };
    if files.is_empty() {
        bail!("{}: no transcript.jsonl", input.display());
    }
    let mut set = LedgerSet::new();
    let mut losses = Losses::default();
    let mut variant = None;
    for f in files {
        let rel = f.strip_prefix(&root).unwrap_or(&f).to_string_lossy().replace('\\', "/");
        let text = std::fs::read_to_string(&f).with_context(|| format!("reading {}", f.display()))?;
        let state = std::fs::read_to_string(f.with_file_name("state.json")).ok();
        let v = variant_of(&text);
        variant.get_or_insert(v);
        let l = import_text(&text, &rel, state, v, opts, &mut losses);
        set.insert(l);
    }
    Ok(Imported { set, losses, variant: variant.unwrap_or("transcript").to_string() })
}

/// `<agent>/<conversation>` from the path, when it has those segments.
fn ids_from_rel(rel: &str) -> (Option<String>, Option<String>) {
    let parts: Vec<&str> = rel.split('/').collect();
    if parts.len() >= 3 { (Some(parts[parts.len() - 3].to_string()), Some(parts[parts.len() - 2].to_string())) } else { (None, None) }
}

fn import_text(text: &str, rel: &str, state: Option<String>, variant: &str, opts: &ImportOptions, losses: &mut Losses) -> Ledger {
    let (agent_dir, conv_dir) = ids_from_rel(rel);
    let id = match (&agent_dir, &conv_dir) {
        (Some(a), Some(c)) => format!("{a}.{c}"),
        _ => rel.trim_end_matches(".jsonl").replace('/', "."),
    };
    let mut extra = Obj::new();
    extra.insert("file".into(), json!(rel));
    if let Some(s) = state {
        extra.insert("state_json".into(), json!(s));
    }
    if !text.is_empty() && !text.ends_with('\n') {
        extra.insert("eof_newline".into(), json!(false));
    }
    let mut agent = Agent {
        id,
        created_at: None,
        source: Some(Source { format: FORMAT.into(), variant: Some(variant.into()), version: None, native_id: conv_dir.clone(), extra }),
        parent: None,
        links: vec![],
        meta: Obj::new(),
    };
    if let Some(a) = &agent_dir {
        let mut m = Obj::new();
        m.insert("agent_id".into(), json!(a));
        if let Some(c) = &conv_dir {
            m.insert("conversation_id".into(), json!(c));
        }
        agent.source.as_mut().unwrap().extra.insert("letta".into(), Value::Object(m));
    }
    let mut l = Ledger::new(agent);
    let mut override_agent = None;
    let mut blank = Vec::new();
    let mut off = 0usize;
    let mut pieces: Vec<&str> = text.split('\n').collect();
    if text.ends_with('\n') || text.is_empty() {
        pieces.pop();
    }
    for (no, line) in pieces.into_iter().enumerate() {
        let start = off;
        off += line.len() + 1;
        if line.trim().is_empty() {
            blank.push(json!([no, line]));
            continue;
        }
        let parsed = serde_json::from_str::<Value>(line).ok().filter(Value::is_object);
        let Some(mut v) = parsed else {
            losses.add("malformed-line", None, "a line that is not a JSON object; Letta Code skips it");
            let nid = format!("@{start}");
            l.append_unique(Event::new(nid.clone(), Kind::Native).native(Native::new(FORMAT, Some(nid)).raw(line)));
            continue;
        };
        if opts.restore_carried
            && v.get(carry::KEY).is_some()
            && let Some(c) = carry::take_with(&mut v, &[])
        {
            if let Some(a) = c.agent {
                override_agent.get_or_insert(a);
            }
            for e in c.events {
                l.append_unique(e);
            }
            continue;
        }
        // The row as written (SPEC §4); a row a stale carrier was taken out of
        // is held as what is left, with the line as `raw`.
        let as_written = serde_json::from_str::<Value>(line).ok().as_ref() == Some(&v);
        let exact = serde_json::to_string(&v).map(|s| s == line).unwrap_or(false);
        for e in row_events(&v, start, no, variant, losses) {
            let mut e = e;
            if let Some(n) = e.native.as_mut().filter(|n| n.part == 0) {
                if as_written {
                    jsonfmt::hold(n, Some(line), &v);
                } else if !exact {
                    n.raw = Some(line.to_string());
                }
            }
            l.append_unique(e);
        }
    }
    if let Some(a) = override_agent {
        l.header.agent = a;
    } else if !blank.is_empty() {
        l.agent_mut().source.as_mut().unwrap().extra.insert("blank".into(), Value::Array(blank));
    }
    l.agent_mut().created_at = l.agent().created_at.clone().or_else(|| l.events.iter().find_map(|e| e.ts.clone()));
    l
}

fn row_events(v: &Value, off: usize, no: usize, variant: &str, losses: &mut Losses) -> Vec<Event> {
    let s = |k: &str| v.get(k).and_then(Value::as_str).filter(|x| !x.is_empty());
    let line_id = s("source_line_id");
    let msg_id = s("source_message_id");
    let base = line_id.or(msg_id).map(String::from).unwrap_or_else(|| format!("@{off}"));
    let ts = s("captured_at").map(String::from);
    let text = v.get("text").and_then(Value::as_str).unwrap_or("").to_string();
    let mut evs = match s("kind").unwrap_or("") {
        "user" => vec![pi_session::text_or_peer("user", text)],
        "assistant" => vec![pi_session::text_or_peer("assistant", text)],
        "reasoning" => vec![Event::new("", Kind::Reasoning).with("text", text)],
        "error" => vec![Event::new("", Kind::Error).with("kind", "client").with("message", text)],
        "tool_call" => {
            let call_id = line_id.or(msg_id).map(String::from).unwrap_or_else(|| format!("letta-code-tool-line-{}", no + 1));
            let shell = matches!(s("name"), Some("Bash" | "bash" | "run_shell_command" | "Shell"));
            let mut c = Event::new("", Kind::ToolCall).with("call_id", call_id.clone()).with("tool_type", if shell { "shell" } else { "function" });
            if let Some(n) = s("name") {
                c = c.with("name", n);
            }
            match v.get("argsText").and_then(Value::as_str) {
                Some(a) => match serde_json::from_str::<Value>(a) {
                    Ok(j) if j.is_object() => c = c.with("input", j),
                    // SPEC §3.1.2: a tool that takes free text is `custom`, its text in `input_text`.
                    _ if shell => c = c.with("input_text", a),
                    _ => c = c.with("input_text", a).with("tool_type", "custom"),
                },
                None => c = c.with("input", json!({})),
            }
            let mut out = vec![c];
            let rt = v.get("resultText").and_then(Value::as_str);
            let ok = v.get("resultOk").and_then(Value::as_bool);
            if rt.is_some() || ok.is_some() {
                let mut r = Event::new("", Kind::ToolResult).with("call_id", call_id).with("output", rt.unwrap_or(""));
                if let Some(ok) = ok {
                    r = r.with("is_error", !ok);
                }
                out.push(r);
            }
            out
        }
        other => {
            losses.add("row-native-only", Some(Kind::Native), &format!("a row kind Letta Code does not write (e.g. {other})"));
            vec![Event::new("", Kind::Native)]
        }
    };
    let nid = base.clone();
    // The era is the header's: decided by later rows (SPEC §9.3), so no
    // record carries it.
    let _ = variant;
    let native = Native::new(FORMAT, Some(nid)).record(v.clone());
    let n = evs.len();
    for (i, e) in evs.iter_mut().enumerate() {
        e.id = if i == 0 { base.clone() } else { format!("{base}#{i}") };
        e.ts = ts.clone();
        e.group = msg_id.map(String::from).filter(|_| n > 0 && matches!(e.kind, Kind::Text | Kind::Reasoning | Kind::PeerMessage));
        e.native = Some(if i == 0 { native.clone() } else { native.part_of(i as u32) });
    }
    evs
}

// ---------------------------------------------------------------------------
// Import: local backend (pi session entries)
// ---------------------------------------------------------------------------

/// `conversations/<b64url key>/` directories under `dir` that hold a manifest.
fn local_backend_dirs(dir: &Path) -> Vec<PathBuf> {
    let mut v: Vec<PathBuf> = walkdir::WalkDir::new(dir)
        .max_depth(4)
        .into_iter()
        .filter_map(|e| e.ok())
        .filter(|e| e.file_type().is_file() && e.file_name() == "manifest.json")
        .filter(|e| {
            std::fs::read_to_string(e.path()).ok().and_then(|t| serde_json::from_str::<Value>(&t).ok()).is_some_and(|m| m.get("message_format").is_some())
        })
        .map(|e| e.path().parent().unwrap().to_path_buf())
        .collect();
    v.sort();
    v
}

fn import_local_backend(input: &Path, opts: &ImportOptions) -> Result<Imported> {
    let mut set = LedgerSet::new();
    let mut losses = Losses::default();
    for dir in local_backend_dirs(input) {
        let msgs = dir.join("messages.jsonl");
        if !msgs.exists() {
            continue;
        }
        let rel = dir.strip_prefix(input).unwrap_or(&dir).to_string_lossy().replace('\\', "/");
        let imp = pi_session::import(Dialect::LettaLocal, &msgs, opts)?;
        losses.extend(imp.losses);
        for mut l in imp.set.ledgers.into_values() {
            if let Some(src) = l.agent_mut().source.as_mut() {
                src.extra.insert("file".into(), json!(format!("{rel}/messages.jsonl")));
                for side in ["manifest.json", "conversation.json", "system-prompt.json"] {
                    if let Ok(t) = std::fs::read_to_string(dir.join(side)) {
                        src.extra.insert(side.into(), json!(t));
                    }
                }
            }
            set.insert(l);
        }
    }
    Ok(Imported { set, losses, variant: "local-backend".into() })
}

// ---------------------------------------------------------------------------
// Export: client transcripts
// ---------------------------------------------------------------------------

fn export(set: &LedgerSet, opts: &ExportOptions) -> Result<Exported> {
    if opts.variant.as_deref() == Some("local-backend") {
        bail!("letta-code: export to the local-backend store is not supported; export the transcript variant");
    }
    let mut out = Output::default();
    let mut losses = Losses::default();
    // Local-backend conversations go back to their own store, natively.
    let is_local = |l: &Ledger| l.agent().source.as_ref().is_some_and(|s| s.format == FORMAT && s.variant.as_deref() == Some("local-backend"));
    let mut local = LedgerSet::new();
    for l in set.iter().filter(|l| is_local(l)) {
        local.insert(l.clone());
        if let Some(src) = &l.agent().source {
            let dir = src.extra.get("file").and_then(Value::as_str).and_then(|f| f.rsplit_once('/')).map(|(d, _)| d.to_string());
            for side in ["manifest.json", "conversation.json", "system-prompt.json"] {
                if let (Some(d), Some(t)) = (&dir, src.extra.get(side).and_then(Value::as_str)) {
                    out.put(format!("{d}/{side}"), t.as_bytes().to_vec());
                }
            }
        }
    }
    if !local.is_empty() {
        let e = pi_session::export(Dialect::LettaLocal, &local, opts)?;
        losses.extend(e.losses);
        for (k, v) in e.output.files {
            out.put(k, v);
        }
    }
    for l in set.iter().filter(|l| !is_local(l)) {
        let native_rel = l
            .agent()
            .source
            .as_ref()
            .filter(|s| s.format == FORMAT && s.variant.as_deref().is_some_and(|v| v.starts_with("transcript")))
            .and_then(|s| s.extra.get("file"))
            .and_then(Value::as_str)
            .map(String::from);
        let rel = native_rel.clone().unwrap_or_else(|| {
            let root = root_of(set, l);
            if l.agent().parent.is_some() {
                losses.add("subagent-separate-conversation", Some(Kind::Subagent), "a child agent becomes its own conversation transcript");
            }
            format!("agent-{}/conv-{}/transcript.jsonl", util::uuid_for(root.id()), util::uuid_for(l.id()))
        });
        let text = render(l, native_rel.is_some(), opts, &mut losses);
        if let Some(st) = l.agent().source.as_ref().filter(|s| s.format == FORMAT).and_then(|s| s.extra.get("state_json")).and_then(Value::as_str) {
            let state_rel = match rel.rsplit_once('/') {
                Some((d, _)) => format!("{d}/state.json"),
                None => "state.json".into(),
            };
            out.put(state_rel, st.as_bytes().to_vec());
        }
        out.put(rel, text.into_bytes());
    }
    Ok(Exported { output: out, losses: losses.sorted(), variant: "transcript".into() })
}

fn root_of<'a>(set: &'a LedgerSet, l: &'a Ledger) -> &'a Ledger {
    let mut cur = l;
    let mut guard = 0;
    while let Some(Parent { agent, .. }) = &cur.agent().parent {
        guard += 1;
        match set.get(agent) {
            Some(p) if guard < 10_000 => cur = p,
            _ => break,
        }
    }
    cur
}

struct Row {
    value: Value,
    raw: Option<String>,
    events: Vec<Event>,
    native: bool,
}

fn render(l: &Ledger, native_file: bool, opts: &ExportOptions, losses: &mut Losses) -> String {
    // Results by call id, so a tool row can carry its result like Letta's does.
    let mut results: HashMap<String, &Event> = HashMap::new();
    for e in &l.events {
        if e.kind == Kind::ToolResult
            && e.native.as_ref().is_none_or(|n| n.format != FORMAT)
            && let Some(c) = e.str("call_id")
        {
            results.entry(c.to_string()).or_insert(e);
        }
    }
    let mut called: HashMap<String, usize> = HashMap::new();
    let mut rows: Vec<Row> = Vec::new();
    let mut pending: Vec<Event> = Vec::new();
    let mut last_native: Option<String> = None;
    let mut last_ts = l.agent().created_at.clone().map(|t| util::norm_ts(&t)).unwrap_or_else(|| "1970-01-01T00:00:00.000Z".into());
    let push = |rows: &mut Vec<Row>, pending: &mut Vec<Event>, value: Value, raw: Option<String>, mut evs: Vec<Event>, native: bool| {
        if !native {
            let mut all = std::mem::take(pending);
            all.append(&mut evs);
            evs = all;
        }
        rows.push(Row { value, raw, events: evs, native });
    };
    // SPEC §2.3: a transcript is one list of rows; the active path is written,
    // and an abandoned event rides on the next row.
    let own = |e: &Event| e.native.as_ref().is_some_and(|n| n.format == FORMAT);
    let on = crate::formats::active_path_only(l, losses, "with carry they ride on the next row", own);
    for (e, on) in l.events.iter().zip(on) {
        if !on && !own(e) {
            pending.push(e.clone());
            continue;
        }
        if let Some(n) = e.native.as_ref().filter(|n| n.format == FORMAT && n.variant.as_deref().is_none_or(|v| v.starts_with("transcript"))) {
            if n.part == 0 && (n.record.is_some() || n.raw.is_some()) {
                last_native = n.id.clone();
                if !pending.is_empty() && opts.carry {
                    // Ride on the previous fresh row, if any.
                    if let Some(r) = rows.iter_mut().rev().find(|r| !r.native) {
                        r.events.append(&mut pending);
                    }
                }
                pending.clear();
                // Emitted as held, never re-serialised (SPEC §4).
                let text = crate::util::native_text(n);
                push(&mut rows, &mut pending, n.record_json().unwrap_or(Value::Null), text, vec![], true);
                continue;
            }
            if n.part > 0 && last_native.is_some() && last_native == n.id {
                continue;
            }
        }
        last_native = None;
        let ts = e.ts.as_deref().map(util::norm_ts).unwrap_or_else(|| last_ts.clone());
        last_ts = ts.clone();
        let mid = |e: &Event| format!("message-{}", util::uuid_for(e.group.as_deref().unwrap_or(&e.id)));
        match e.kind {
            Kind::Text => {
                let role = e.str("role").unwrap_or("user");
                let text = e.str("text").unwrap_or("");
                let (kind, line_id) = match role {
                    "assistant" => ("assistant", mid(e)),
                    "user" => ("user", format!("user-{}", util::uuid_for(&e.id))),
                    _ => {
                        losses.add("system-text-as-user", Some(Kind::Text), "Letta transcripts have no system rows; written as a user row");
                        ("user", format!("user-{}", util::uuid_for(&e.id)))
                    }
                };
                // Rows that share a message id need distinct line ids (Letta's `-split-N`).
                let line_id = if kind == "assistant" && rows.iter().any(|r| r.value.get("source_line_id").and_then(Value::as_str) == Some(line_id.as_str())) {
                    format!("{line_id}-split-{}", util::short_id(&e.id, 6))
                } else {
                    line_id
                };
                let v = json!({"kind": kind, "text": text, "captured_at": ts, "source_line_id": line_id, "source_message_id": mid(e)});
                push(&mut rows, &mut pending, v, None, vec![e.clone()], false);
            }
            Kind::Reasoning => {
                if e.body.get("sealed").is_some() {
                    losses.add("reasoning-sealed-dropped", Some(Kind::Reasoning), "transcript rows keep reasoning text only");
                }
                let v =
                    json!({"kind":"reasoning","text": e.str("text").unwrap_or(""), "captured_at": ts, "source_line_id": mid(e), "source_message_id": mid(e)});
                push(&mut rows, &mut pending, v, None, vec![e.clone()], false);
            }
            Kind::PeerMessage => {
                // SPEC §3.1.6: wrapped user (incoming) or assistant (outgoing) text.
                let out = e.str("direction") == Some("out");
                let (kind, line_id) = if out { ("assistant", mid(e)) } else { ("user", format!("user-{}", util::uuid_for(&e.id))) };
                let v = json!({"kind": kind, "text": pi_session::peer_wrap(e), "captured_at": ts, "source_line_id": line_id, "source_message_id": mid(e)});
                push(&mut rows, &mut pending, v, None, vec![e.clone()], false);
            }
            Kind::Error => {
                let msg = e.str("message").or(e.str("kind")).unwrap_or("error");
                let v = json!({"kind":"error","text": msg, "captured_at": ts, "source_line_id": format!("err-{}", util::short_id(&e.id, 12))});
                push(&mut rows, &mut pending, v, None, vec![e.clone()], false);
            }
            Kind::ToolCall => {
                let call_id = e.str("call_id").unwrap_or(&e.id).to_string();
                let mut m = Map::new();
                m.insert("kind".into(), json!("tool_call"));
                if let Some(n) = e.str("name") {
                    m.insert("name".into(), json!(n));
                }
                let args = match (e.body.get("input"), e.str("input_text")) {
                    (_, Some(t)) => Some(t.to_string()),
                    (Some(v), _) if !v.is_null() => Some(serde_json::to_string(v).unwrap()),
                    _ => None,
                };
                if let Some(a) = args {
                    m.insert("argsText".into(), json!(a));
                }
                let evs = vec![e.clone()];
                if let Some(r) = results.get(&call_id) {
                    m.insert("resultText".into(), json!(result_text(r, losses)));
                    m.insert("resultOk".into(), json!(!r.body.get("is_error").and_then(Value::as_bool).unwrap_or(false)));
                }
                m.insert("captured_at".into(), json!(ts));
                m.insert("source_line_id".into(), json!(call_id));
                called.insert(call_id, rows.len());
                push(&mut rows, &mut pending, Value::Object(m), None, evs, false);
            }
            Kind::ToolResult => {
                let call_id = e.str("call_id").unwrap_or("").to_string();
                if called.contains_key(&call_id) {
                    // Its text is on the call's row; the event itself rides
                    // with the next row so a carried trip keeps its place.
                    pending.push(e.clone());
                    continue;
                }
                let ok = !e.body.get("is_error").and_then(Value::as_bool).unwrap_or(false);
                let v = json!({"kind":"tool_call","resultText": result_text(e, losses),"resultOk": ok,"captured_at": ts,"source_line_id": if call_id.is_empty() { e.id.clone() } else { call_id }});
                losses.add("tool-result-orphan", Some(Kind::ToolResult), "a result with no call becomes a tool row without a name");
                push(&mut rows, &mut pending, v, None, vec![e.clone()], false);
            }
            k => {
                let code = match k {
                    Kind::Media => "media-dropped",
                    Kind::Compaction => "compaction-dropped",
                    Kind::Settings => "settings-dropped",
                    Kind::Usage => "usage-dropped",
                    Kind::Title => "title-dropped",
                    Kind::Subagent => "subagent-event-dropped",
                    Kind::FileChange => "file-change-dropped",
                    Kind::Turn => "turn-dropped",
                    Kind::App => "app-dropped",
                    _ => "native-dropped",
                };
                losses.add(code, Some(k), "Letta transcript rows have no place for this");
                pending.push(e.clone());
            }
        }
    }
    if opts.carry && !pending.is_empty() {
        match rows.iter_mut().rev().find(|r| !r.native) {
            Some(r) => r.events.append(&mut pending),
            None => {
                // Nothing to ride on: a row of a kind Letta Code skips.
                let v = json!({"kind": "protoledger.carry", "captured_at": last_ts});
                rows.push(Row { value: v, raw: None, events: std::mem::take(&mut pending), native: false });
            }
        }
    }
    if opts.carry && !native_file && !rows.iter().any(|r| !r.native) {
        let v = json!({"kind": "protoledger.carry", "captured_at": last_ts});
        rows.push(Row { value: v, raw: None, events: vec![], native: false });
    }
    let foreign_header = !native_file;
    let mut text = String::new();
    let mut first_fresh = true;
    for r in rows.iter_mut() {
        if opts.carry && !r.native {
            carry::attach_with(&mut r.value, &[], &r.events, (foreign_header && first_fresh).then(|| l.agent()));
            first_fresh = false;
        }
        text.push_str(&r.raw.clone().unwrap_or_else(|| serde_json::to_string(&r.value).unwrap()));
        text.push('\n');
    }
    // Blank lines and a missing final newline, for an exact self trip.
    if native_file && rows.iter().all(|r| r.native) {
        let ex = l.agent().source.as_ref().map(|s| &s.extra);
        if let Some(Value::Array(blank)) = ex.and_then(|x| x.get("blank")) {
            let mut lines: Vec<String> = text.lines().map(String::from).collect();
            for b in blank {
                if let (Some(n), Some(t)) = (b.get(0).and_then(Value::as_u64), b.get(1).and_then(Value::as_str)) {
                    let n = (n as usize).min(lines.len());
                    lines.insert(n, t.to_string());
                }
            }
            text = lines.join("\n") + "\n";
        }
        if ex.and_then(|x| x.get("eof_newline")).and_then(Value::as_bool) == Some(false) {
            text.pop();
        }
    }
    text
}

fn result_text(r: &Event, losses: &mut Losses) -> String {
    match r.body.get("output") {
        Some(Value::String(s)) => s.clone(),
        Some(Value::Array(parts)) => {
            let mut out = String::new();
            for p in parts {
                match p.get("type").and_then(Value::as_str) {
                    Some("text") => out.push_str(p.get("text").and_then(Value::as_str).unwrap_or("")),
                    _ => losses.add("media-dropped", Some(Kind::ToolResult), "tool output media has no place in resultText"),
                }
            }
            out
        }
        Some(v) if !v.is_null() => serde_json::to_string(v).unwrap(),
        _ => String::new(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const ROWS: &str = concat!(
        r#"{"kind":"user","text":"Inspect it.","captured_at":"2026-10-01T12:00:05.123Z","source_line_id":"user-1","source_message_id":"message-1"}"#,
        "\n",
        r#"{"kind":"reasoning","text":"Read first.","captured_at":"2026-10-01T12:00:05.123Z","source_line_id":"message-2","source_message_id":"message-2"}"#,
        "\n",
        r#"{"kind":"assistant","text":"Reading.","captured_at":"2026-10-01T12:00:05.123Z","source_line_id":"message-2-split-0","source_message_id":"message-2"}"#,
        "\n",
        r#"{"kind":"tool_call","name":"Read","argsText":"{\"file_path\":\"a.ts\"}","resultText":"x","resultOk":false,"captured_at":"2026-10-01T12:00:05.123Z","source_line_id":"toolu_1"}"#,
        "\n",
        r#"{"kind":"tool_call","name":"Glob","captured_at":"2026-10-01T12:00:05.123Z"}"#,
        "\n",
        "not json\n",
    );

    fn imp(text: &str) -> Ledger {
        imp_at(text, "agent-a/conv-b/transcript.jsonl")
    }

    fn imp_at(text: &str, rel: &str) -> Ledger {
        let mut losses = Losses::default();
        import_text(text, rel, None, variant_of(text), &ImportOptions::standard(), &mut losses)
    }

    #[test]
    fn rows_map_to_neutral_events() {
        let l = imp(ROWS);
        assert_eq!(l.id(), "agent-a.conv-b");
        let kinds: Vec<_> = l.events.iter().map(|e| e.kind.as_str()).collect();
        assert_eq!(kinds, ["text", "reasoning", "text", "tool_call", "tool_result", "tool_call", "native"]);
        assert_eq!(l.events[1].group, l.events[2].group);
        assert_eq!(l.events[3].body["input"], json!({"file_path":"a.ts"}));
        assert_eq!(l.events[4].body["is_error"], json!(true));
        assert_eq!(l.events[5].str("call_id"), Some("letta-code-tool-line-5"));
    }

    #[test]
    fn self_trip_is_exact() {
        let l = imp(ROWS);
        let out = export(&LedgerSet::single(l), &ExportOptions::standard()).unwrap();
        assert_eq!(String::from_utf8(out.output.files["agent-a/conv-b/transcript.jsonl"].clone()).unwrap(), ROWS);
    }

    #[test]
    fn rows_are_held_and_reemitted_as_written() {
        use crate::formats::atif::jsonfmt::without_raw;
        use crate::formats::openclaw::jsonl::spelling::{ODD, assert_value_loses, held};
        let row = format!(r#"{{"kind":"user","text":"hi","captured_at":"2026-10-01T12:00:05.123Z","source_line_id":"m1",{ODD}}}"#);
        assert_value_loses(&row);
        let src = format!("{row}\n");
        let set = LedgerSet::single(imp(&src));
        assert_eq!(held(&set, FORMAT), [row.as_str()], "the ledger holds the row as written");
        for s in [set.clone(), without_raw(&set)] {
            let out = export(&s, &ExportOptions::standard()).unwrap();
            assert_eq!(String::from_utf8(out.output.files["agent-a/conv-b/transcript.jsonl"].clone()).unwrap(), src);
        }
    }

    #[test]
    fn carried_records_keep_their_text() {
        use crate::formats::openclaw::jsonl::spelling::{ODD, assert_value_loses};
        let text = format!("{{{ODD}}}");
        assert_value_loses(&text);
        let mut l =
            Ledger::new(Agent { id: "f".into(), created_at: Some("2026-01-01T00:00:00Z".into()), source: None, parent: None, links: vec![], meta: Obj::new() });
        l.append(Event::new("u", Kind::Text).with("role", "user").with("text", "hi").native(Native::new("x", Some("1".into())).record_raw(&text).unwrap()))
            .unwrap();
        let out = export(&LedgerSet::single(l), &ExportOptions::standard()).unwrap();
        let (name, bytes) = out.output.files.iter().next().unwrap();
        let back = imp_at(std::str::from_utf8(bytes).unwrap(), name);
        let e = back.events.iter().find(|e| e.id == "u").expect("carried event restored");
        assert_eq!(e.native.as_ref().unwrap().record_text(), Some(text.as_str()));
    }

    #[test]
    fn foreign_ledger_renders_rows_and_carries_back() {
        let mut l = Ledger::new(Agent {
            id: "f1".into(),
            created_at: Some("2026-01-01T00:00:00Z".into()),
            source: None,
            parent: None,
            links: vec![],
            meta: Obj::new(),
        });
        l.append(Event::new("u", Kind::Text).with("role", "user").with("text", "hi")).unwrap();
        l.append(Event::new("s", Kind::Settings).with("model", "m")).unwrap();
        l.append(Event::new("c", Kind::ToolCall).with("call_id", "t1").with("name", "Bash").with("input", json!({"command":"ls"}))).unwrap();
        l.append(Event::new("a", Kind::Text).with("role", "assistant").with("text", "ok").group(Some("g"))).unwrap();
        l.append(Event::new("r", Kind::ToolResult).with("call_id", "t1").with("output", "a\n").with("is_error", false)).unwrap();
        let set = LedgerSet::single(l);
        let plain = export(&set, &ExportOptions::no_carry()).unwrap();
        let text = String::from_utf8(plain.output.files.values().next().unwrap().clone()).unwrap();
        let rows: Vec<Value> = text.lines().map(|x| serde_json::from_str(x).unwrap()).collect();
        assert_eq!(rows.len(), 3);
        assert_eq!(rows[1]["kind"], "tool_call");
        assert_eq!(rows[1]["argsText"], r#"{"command":"ls"}"#);
        assert_eq!(rows[1]["resultText"], "a\n");
        assert!(rows[0]["source_message_id"].as_str().unwrap().starts_with("message-"));
        let carried = export(&set, &ExportOptions::standard()).unwrap();
        let text = String::from_utf8(carried.output.files.values().next().unwrap().clone()).unwrap();
        let mut losses = Losses::default();
        let back = import_text(&text, "x/y/transcript.jsonl", None, "transcript", &ImportOptions::standard(), &mut losses);
        assert_eq!(back.id(), "f1");
        let ids: Vec<_> = back.events.iter().map(|e| e.id.as_str()).collect();
        assert_eq!(ids, ["u", "s", "c", "a", "r"]);
    }

    #[test]
    fn detects_variants() {
        assert_eq!(variant_of(ROWS), "transcript");
        assert_eq!(variant_of(r#"{"kind":"user","text":"x","captured_at":"t"}"#), "transcript-v0");
        assert_eq!(variant_of(r#"{"kind":"user","text":"x","captured_at":"t","source_line_id":"l"}"#), "transcript-v1");
        assert_eq!(variant_of(r#"{"kind":"tool_call","captured_at":"t","source_message_id":"13"}"#), "transcript-external");
    }
}
