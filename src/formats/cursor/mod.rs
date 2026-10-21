//! Cursor: the `cursor-agent` CLI and the Cursor IDE.
//!
//! Four native shapes (see `docs/formats/cursor.md`):
//!
//! - **`stream-json`**: what `cursor-agent --print --output-format stream-json`
//!   writes to stdout, one JSON event per line (`system/init`, `user`,
//!   `assistant`, `thinking`, `tool_call started/completed`, `result`).
//! - **`agent-transcript`**: the CLI's durable JSONL projection,
//!   `~/.cursor/projects/<slug>/agent-transcripts/<id>.jsonl`
//!   (`{"role":..,"message":{"content":[..]}}` lines plus `turn_ended` /
//!   `error` control lines).
//! - **`agent-store`**: the CLI's chat store `~/.cursor/chats/<md5(cwd)>/<agent
//!   id>/store.db`, a content-addressed SQLite blob DAG whose message blobs are
//!   AI SDK messages. This is what `cursor-agent --resume` loads.
//! - **`ide-state-vscdb`**: the IDE's `globalStorage/state.vscdb`,
//!   `cursorDiskKV` rows `composerData:<id>` and `bubbleId:<id>:<bubble>`.

use crate::formats::{Detection, ExportOptions, Exported, Format, ImportOptions, Imported, Variant};
use crate::ledger::{Ledger, LedgerSet};
use crate::model::{Agent, Event, Kind, Native, Obj, Source};
use anyhow::{Result, bail};
use serde_json::{Value, json};
use std::path::Path;

mod stream;
pub(crate) use stream::raw_lines;
#[cfg(feature = "sqlite")]
mod ide;
#[cfg(feature = "sqlite")]
mod store;
mod transcript;

pub const NAME: &str = "cursor";
pub(crate) const V_STREAM: &str = "stream-json";
pub(crate) const V_TRANSCRIPT: &str = "agent-transcript";
pub(crate) const V_STORE: &str = "agent-store";
pub(crate) const V_IDE: &str = "ide-state-vscdb";

pub struct Cursor;

static VARIANTS: [Variant; 4] = [
    Variant {
        name: V_STREAM,
        versions: "cursor-agent 2025.08 - 2026.09 (tool_call siblings toolCallId/startedAtMs/completedAtMs since ~2026.08)",
        import: true,
        export: true,
        notes: "`--print --output-format stream-json` stdout; Cursor writes it, never reads it",
    },
    Variant {
        name: V_TRANSCRIPT,
        versions: "cursor-agent 2025 - 2026.09",
        import: true,
        export: true,
        notes: "~/.cursor/projects/<slug>/agent-transcripts/<id>.jsonl (+ <id>/subagents/); a projection without tool results or ids",
    },
    Variant {
        name: V_STORE,
        versions: "cursor-agent chats store.db (meta + blobs); root field 1 JSON messages; field-8 turn index (~2026.09) read provisionally",
        import: true,
        export: true,
        notes: "~/.cursor/chats/<md5(cwd)>/<agentId>/store.db; the shape `cursor-agent --resume` loads",
    },
    Variant {
        name: V_IDE,
        versions: "Cursor IDE composerData _v 1-17 (inline conversation[] before _v 3; bubbleId rows since)",
        import: true,
        export: true,
        notes: "<userdata>/User/globalStorage/state.vscdb cursorDiskKV; export writes a fresh state.vscdb holding only these composers",
    },
];

impl Format for Cursor {
    fn name(&self) -> &'static str {
        NAME
    }
    fn describe(&self) -> &'static str {
        "Cursor: cursor-agent stream-json, agent transcripts, agent chat store.db, IDE state.vscdb composers"
    }
    fn variants(&self) -> &'static [Variant] {
        &VARIANTS
    }
    fn detect(&self, input: &Path) -> Option<Detection> {
        detect(input)
    }
    fn import(&self, input: &Path, opts: &ImportOptions) -> Result<Imported> {
        // SPEC §3.1.6: user or assistant text holding the peer-message wrapper is that peer message.
        let mut im = import_records(input, opts)?;
        crate::formats::pi_session::unwrap_peers(&mut im.set, self.name());
        Ok(im)
    }
    fn export(&self, set: &LedgerSet, opts: &ExportOptions) -> Result<Exported> {
        // One variant per export: the requested one, else the source's, else stream-json.
        let variant = opts.variant.clone().unwrap_or_else(|| {
            set.iter()
                .find_map(|l| l.agent().source.as_ref().filter(|s| s.format == NAME).and_then(|s| s.variant.clone()))
                .unwrap_or_else(|| V_STREAM.to_string())
        });
        match variant.as_str() {
            V_STREAM => stream::export(set, opts),
            V_TRANSCRIPT => transcript::export(set, opts),
            #[cfg(feature = "sqlite")]
            V_STORE => store::export(set, opts),
            #[cfg(feature = "sqlite")]
            V_IDE => ide::export(set, opts),
            v => bail!("cursor: variant {v} is not available"),
        }
    }
}

/// What [`Format::import`] reads before peer messages are unwrapped.
fn import_records(input: &Path, opts: &ImportOptions) -> Result<Imported> {
    let variant = match opts.variant.as_deref() {
        Some(v) => v.to_string(),
        None => match detect(input) {
            Some(d) => d.variant.to_string(),
            None => bail!("{}: not a Cursor transcript, store or state.vscdb", input.display()),
        },
    };
    match variant.as_str() {
        V_STREAM => stream::import(input, opts),
        V_TRANSCRIPT => transcript::import(input, opts),
        #[cfg(feature = "sqlite")]
        V_STORE => store::import(input, opts),
        #[cfg(feature = "sqlite")]
        V_IDE => ide::import(input, opts),
        v => bail!("cursor: variant {v} is not available"),
    }
}

fn detect(input: &Path) -> Option<Detection> {
    if input.is_dir() {
        // A chat directory, a chats tree, or a transcripts tree.
        let mut best: Option<Detection> = None;
        for e in walkdir::WalkDir::new(input).max_depth(4).sort_by_file_name().into_iter().flatten() {
            if !e.file_type().is_file() {
                continue;
            }
            if let Some(d) = detect_file(e.path())
                && best.as_ref().is_none_or(|b| d.confidence > b.confidence)
            {
                best = Some(d);
            }
        }
        return best;
    }
    detect_file(input)
}

fn detect_file(p: &Path) -> Option<Detection> {
    let bytes = std::fs::read(p).ok()?;
    if bytes.starts_with(b"SQLite format 3\0") {
        #[cfg(feature = "sqlite")]
        {
            return sqlite_kind(p).map(|v| Detection { variant: v, confidence: 95, version: None });
        }
        #[cfg(not(feature = "sqlite"))]
        return None;
    }
    let text = std::str::from_utf8(&bytes).ok()?;
    for line in text.lines().filter(|l| l.trim_start().starts_with('{')).take(5) {
        let Ok(v) = serde_json::from_str::<Value>(line) else { continue };
        let ty = v.get("type").and_then(Value::as_str);
        if v.get("session_id").is_some() && matches!(ty, Some("system" | "user" | "assistant" | "thinking" | "tool_call" | "result" | "interaction_query")) {
            return Some(Detection { variant: V_STREAM, confidence: 85, version: None });
        }
        if v.get("role").is_some_and(Value::is_string) && v.get("message").is_some_and(Value::is_object) && ty.is_none() {
            return Some(Detection { variant: V_TRANSCRIPT, confidence: 70, version: None });
        }
    }
    None
}

#[cfg(feature = "sqlite")]
fn sqlite_kind(p: &Path) -> Option<&'static str> {
    let c = rusqlite::Connection::open_with_flags(p, rusqlite::OpenFlags::SQLITE_OPEN_READ_ONLY).ok()?;
    let has = |t: &str| c.query_row("SELECT count(*) FROM sqlite_master WHERE type='table' AND name=?1", [t], |r| r.get::<_, i64>(0)).ok() == Some(1);
    if has("cursorDiskKV") {
        Some(V_IDE)
    } else if has("blobs") && has("meta") {
        Some(V_STORE)
    } else {
        None
    }
}

// ---------------------------------------------------------------------------
// shared helpers

pub(crate) fn agent(id: &str, variant: &str, native_id: Option<String>, extra: Obj) -> Agent {
    Agent {
        id: id.to_string(),
        created_at: None,
        source: Some(Source { format: NAME.into(), variant: Some(variant.into()), version: None, native_id, extra }),
        parent: None,
        links: vec![],
        meta: Obj::new(),
    }
}

pub(crate) fn native(variant: &str, id: &str) -> Native {
    Native::new(NAME, Some(id.to_string())).variant(Some(variant))
}

/// Append the events of one native record: part 0 carries the record, the
/// rest share its id. A record with no neutral meaning is one `native` event.
pub(crate) fn push_record(l: &mut Ledger, nid: &str, n: Native, mut evs: Vec<Event>, ts: Option<String>) {
    if evs.is_empty() {
        evs.push(Event::new("", Kind::Native).ts(ts));
    }
    for (i, mut e) in evs.into_iter().enumerate() {
        e.id = if i == 0 { nid.to_string() } else { format!("{nid}#{i}") };
        e.native = Some(if i == 0 { n.clone() } else { n.part_of(i as u32) });
        l.append_unique(e);
    }
}

/// Is `e` a record of this format and variant, to be written back as is?
pub(crate) fn own<'a>(e: &'a Event, variant: &str) -> Option<&'a Native> {
    e.native.as_ref().filter(|n| n.format == NAME && n.variant.as_deref() == Some(variant))
}

pub(crate) fn is_own_ledger(l: &Ledger, variant: &str) -> bool {
    l.agent().source.as_ref().is_some_and(|s| s.format == NAME && s.variant.as_deref() == Some(variant))
}

pub(crate) fn ms_ts(v: Option<&Value>) -> Option<String> {
    let ms = match v? {
        Value::Number(n) => n.as_f64()? as i64,
        Value::String(s) => s.parse::<f64>().ok()? as i64,
        _ => return None,
    };
    (ms > 100_000_000_000).then(|| crate::util::ms_to_rfc3339(ms))
}

pub(crate) fn ts_ms(ts: Option<&str>) -> Option<i64> {
    crate::util::parse_rfc3339_ms(ts?)
}

/// Text of a tool output (string or neutral parts); `true` if media was dropped.
pub(crate) fn output_text(v: Option<&Value>) -> (String, bool) {
    match v {
        None | Some(Value::Null) => (String::new(), false),
        Some(Value::String(s)) => (s.clone(), false),
        Some(Value::Array(a)) => {
            let mut media = false;
            let mut out = Vec::new();
            for p in a {
                match p.get("type").and_then(Value::as_str) {
                    Some("text") => out.push(p.get("text").and_then(Value::as_str).unwrap_or("").to_string()),
                    _ => media = true,
                }
            }
            (out.join("\n"), media)
        }
        Some(o) => (o.to_string(), false),
    }
}

/// Neutral events for one AI SDK message (`{role, content}`), the message
/// blob shape of the agent store and the IDE's `agentKv:blob` rows.
#[cfg_attr(not(feature = "sqlite"), allow(dead_code))] // used by the store.db reader only
pub(crate) fn ai_sdk_events(m: &Value, ts: Option<String>, group: Option<String>, turn: Option<String>) -> Vec<Event> {
    let role = m.get("role").and_then(Value::as_str).unwrap_or("");
    let mk = |k: Kind| Event::new("", k).ts(ts.clone()).group(group.clone()).turn(turn.clone());
    let parts: Vec<Value> = match m.get("content") {
        Some(Value::String(s)) => vec![json!({"type": "text", "text": s})],
        Some(Value::Array(a)) => a.clone(),
        _ => vec![],
    };
    let mut out = Vec::new();
    for p in &parts {
        let ty = p.get("type").and_then(Value::as_str).unwrap_or("");
        match (role, ty) {
            (_, "text") => {
                let r = match role {
                    "assistant" => "assistant",
                    "system" => "system",
                    _ => "user",
                };
                out.push(mk(Kind::Text).with("role", r).with("text", p.get("text").cloned().unwrap_or(json!(""))));
            }
            (_, "reasoning") | (_, "redacted-reasoning") => {
                let mut e = mk(Kind::Reasoning).with("text", p.get("text").cloned().unwrap_or(json!("")));
                if let Some(s) = p.get("signature").and_then(Value::as_str) {
                    e = e.with("sealed", json!({"vendor": "cursor", "kind": "signature", "data": s}));
                }
                if let Some(d) = p.get("data").and_then(Value::as_str) {
                    e = e.with("redacted", true).with("sealed", json!({"vendor": "cursor", "kind": "encrypted", "data": d}));
                }
                out.push(e);
            }
            (_, "tool-call") => out.push(
                mk(Kind::ToolCall)
                    .with("call_id", p.get("toolCallId").cloned().unwrap_or(json!("")))
                    .with("name", p.get("toolName").cloned().unwrap_or(json!("")))
                    .with("input", p.get("args").or(p.get("input")).cloned().unwrap_or(json!({}))),
            ),
            (_, "tool-result") => {
                let r = p.get("result").or(p.get("output")).cloned().unwrap_or(json!(""));
                let output = match r {
                    Value::String(_) => r,
                    // [{type: text, text}, {type: image, data, mimeType}]
                    Value::Array(a) if a.iter().all(|x| x.get("type").and_then(Value::as_str).is_some()) => Value::Array(
                        a.iter()
                            .map(|x| match x.get("type").and_then(Value::as_str) {
                                Some("text") => json!({"type": "text", "text": x.get("text").cloned().unwrap_or(json!(""))}),
                                _ => json!({"type": "media", "media_type": x.get("mimeType").or(x.get("mediaType")).cloned().unwrap_or(json!("application/octet-stream")), "data": x.get("data").cloned().unwrap_or(json!(""))}),
                            })
                            .collect(),
                    ),
                    other => Value::String(other.to_string()),
                };
                let mut e = mk(Kind::ToolResult).with("call_id", p.get("toolCallId").cloned().unwrap_or(json!(""))).with("output", output);
                if p.get("isError").and_then(Value::as_bool) == Some(true) {
                    e = e.with("is_error", true);
                }
                out.push(e);
            }
            (_, "image") | (_, "file") => {
                let r = if role == "assistant" { "assistant" } else { "user" };
                let mt = p.get("mimeType").or(p.get("mediaType")).cloned().unwrap_or(json!("application/octet-stream"));
                let mut e = mk(Kind::Media).with("role", r).with("media_type", mt);
                match p.get("image").or(p.get("data")) {
                    Some(Value::String(s)) if s.starts_with("http") => e = e.with("url", s.as_str()),
                    Some(Value::String(s)) => e = e.with("data", s.as_str()),
                    _ => {}
                }
                out.push(e);
            }
            _ => {}
        }
    }
    out
}

/// Cursor stream / store tool keys (`readToolCall`) ↔ neutral names (`read`).
pub(crate) const KNOWN_TOOLS: [&str; 18] = [
    "read",
    "write",
    "edit",
    "shell",
    "grep",
    "glob",
    "readLints",
    "mcp",
    "task",
    "webFetch",
    "createPlan",
    "ls",
    "delete",
    "updateTodos",
    "semSearch",
    "getMcpTools",
    "todoWrite",
    "webSearch",
];

#[cfg(test)]
mod tests;
