//! Reading and writing the LangGraph SQLite checkpoint store.

use super::FORMAT;
use super::msgpack;
use crate::carry;
use crate::formats::hermes::db::{columns, insert, rows, schema_of};
use crate::formats::hermes::pyjson;
use crate::formats::openclaw::jsonl;
use crate::formats::{Detection, ExportOptions, Exported, ImportOptions, Imported, Losses, Output};
use crate::ledger::{Ledger, LedgerSet};
use crate::model::{Agent, Audience, Event, Kind, Native, Obj, Parent, Relation, Source};
use crate::util;
use anyhow::{Context, Result, bail};
use base64::Engine;
use rusqlite::{Connection, OpenFlags};
use serde_json::{Map, Value, json};
use std::collections::{BTreeMap, HashMap, HashSet};
use std::path::{Path, PathBuf};

const B64: base64::engine::GeneralPurpose = base64::engine::general_purpose::STANDARD;

/// langgraph-checkpoint-sqlite's DDL (`SqliteSaver.setup`, 3.1.2).
const DDL: &[&str] = &[
    "CREATE TABLE IF NOT EXISTS checkpoints (\n    thread_id TEXT NOT NULL,\n    checkpoint_ns TEXT NOT NULL DEFAULT '',\n    checkpoint_id TEXT NOT NULL,\n    parent_checkpoint_id TEXT,\n    type TEXT,\n    checkpoint BLOB,\n    metadata BLOB,\n    PRIMARY KEY (thread_id, checkpoint_ns, checkpoint_id)\n)",
    "CREATE TABLE IF NOT EXISTS writes (\n    thread_id TEXT NOT NULL,\n    checkpoint_ns TEXT NOT NULL DEFAULT '',\n    checkpoint_id TEXT NOT NULL,\n    task_id TEXT NOT NULL,\n    task_path TEXT NOT NULL DEFAULT '',\n    idx INTEGER NOT NULL,\n    channel TEXT NOT NULL,\n    type TEXT,\n    value BLOB,\n    PRIMARY KEY (thread_id, checkpoint_ns, checkpoint_id, task_id, idx)\n)",
];
/// deepagents-code's thread-name table.
const NAMES_DDL: &str = "CREATE TABLE IF NOT EXISTS dcode_thread_names (thread_id TEXT PRIMARY KEY NOT NULL, name TEXT NOT NULL)";

fn is_store(p: &Path) -> bool {
    let Ok(mut f) = std::fs::File::open(p) else { return false };
    let mut head = [0u8; 16];
    use std::io::Read;
    if f.read_exact(&mut head).is_err() || &head != b"SQLite format 3\0" {
        return false;
    }
    let Ok(c) = Connection::open_with_flags(p, OpenFlags::SQLITE_OPEN_READ_ONLY) else { return false };
    let n: i64 = c.query_row("SELECT count(*) FROM sqlite_master WHERE type='table' AND name IN ('checkpoints','writes')", [], |r| r.get(0)).unwrap_or(0);
    n == 2
}

fn stores(input: &Path) -> Vec<PathBuf> {
    if input.is_file() {
        return if is_store(input) { vec![input.to_path_buf()] } else { vec![] };
    }
    let mut v: Vec<PathBuf> = walkdir::WalkDir::new(input)
        .sort_by_file_name()
        .into_iter()
        .filter_map(|e| e.ok())
        .filter(|e| e.file_type().is_file())
        .map(|e| e.path().to_path_buf())
        .filter(|p| is_store(p))
        .collect();
    v.sort();
    v
}

pub fn detect(input: &Path) -> Option<Detection> {
    let p = stores(input).into_iter().next()?;
    let c = Connection::open_with_flags(&p, OpenFlags::SQLITE_OPEN_READ_ONLY).ok()?;
    let delta = c
        .query_row("SELECT count(*) FROM checkpoints WHERE CAST(metadata AS TEXT) LIKE '%counters_since_delta_snapshot%'", [], |r| r.get::<_, i64>(0))
        .unwrap_or(0)
        > 0;
    Some(Detection { variant: if delta { "checkpoint-delta" } else { "checkpoint-full" }, confidence: 90, version: None })
}

// ---------------------------------------------------------------- typed values

/// `serde.loads_typed((type, bytes))` as JSON.
fn loads(ty: Option<&str>, v: &Value) -> Option<Value> {
    let bytes = blob(v)?;
    match ty.unwrap_or("msgpack") {
        "msgpack" => msgpack::decode(&bytes).ok(),
        "json" => serde_json::from_slice(&bytes).ok(),
        "null" | "empty" => Some(Value::Null),
        "bytes" | "bytearray" => Some(json!({"__bin__": B64.encode(&bytes)})),
        _ => None,
    }
}

/// The bytes of a BLOB or TEXT column value.
fn blob(v: &Value) -> Option<Vec<u8>> {
    match v {
        Value::Object(m) => B64.decode(m.get("$blob")?.as_str()?).ok(),
        Value::String(s) => Some(s.as_bytes().to_vec()),
        Value::Null => Some(vec![]),
        _ => None,
    }
}

fn blob_value(b: &[u8]) -> Value {
    json!({"$blob": B64.encode(b)})
}

/// A LangChain message: (class name, field dict), from the msgpack ext
/// (5 pydantic v2, 4 pydantic v1) or the old LC-JSON constructor form.
fn message_of(v: &Value) -> Option<(String, Map<String, Value>)> {
    if let Some(code) = v.get("__ext__").and_then(Value::as_i64) {
        let d = v.get("data")?.as_array()?;
        if code == 5 || code == 4 {
            let cls = d.get(1)?.as_str()?.to_string();
            return Some((cls, d.get(2)?.as_object()?.clone()));
        }
        return None;
    }
    if v.get("lc").is_some() && v.get("type").and_then(Value::as_str) == Some("constructor") {
        let cls = v.get("id")?.as_array()?.last()?.as_str()?.to_string();
        let mut kw = v.get("kwargs")?.as_object()?.clone();
        if !kw.contains_key("type") {
            let t = match cls.as_str() {
                "HumanMessage" => "human",
                "AIMessage" => "ai",
                "ToolMessage" => "tool",
                "SystemMessage" => "system",
                "RemoveMessage" => "remove",
                _ => "",
            };
            kw.insert("type".into(), json!(t));
        }
        return Some((cls, kw));
    }
    None
}

/// `langgraph.types.Overwrite(value)` → its value.
fn overwrite_of(v: &Value) -> Option<Value> {
    let d = v.get("data")?.as_array()?;
    if v.get("__ext__")?.as_i64()? == 2 && d.get(1)?.as_str()? == "Overwrite" {
        return d.get(2)?.get("value").cloned();
    }
    None
}

/// A channel value as a list of items (a delta write, a full list, a snapshot).
fn items_of(v: &Value) -> Vec<Value> {
    match v {
        Value::Array(a) => a.clone(),
        Value::Null => vec![],
        o if o.get("__ext__").and_then(Value::as_i64) == Some(7) => items_of(o.get("data").unwrap_or(&Value::Null)),
        o => vec![o.clone()],
    }
}

fn str_at<'a>(v: &'a Map<String, Value>, k: &str) -> Option<&'a str> {
    v.get(k).and_then(Value::as_str)
}

// ---------------------------------------------------------------- neutral mapping

fn content_events(c: &Value, role: &str, mk: &mut dyn FnMut(Kind) -> Event, out: &mut Vec<Event>) {
    match c {
        Value::String(s) if !s.is_empty() => out.push(mk(Kind::Text).with("role", role).with("text", s.as_str())),
        Value::Array(blocks) => {
            for b in blocks {
                let t = b.get("type").and_then(Value::as_str);
                match t {
                    Some("text") => out.push(mk(Kind::Text).with("role", role).with("text", b.get("text").cloned().unwrap_or(Value::Null))),
                    Some("image") | Some("image_url") => {
                        let (data, mt, url) = (
                            b.get("data").or(b.get("base64")).or(b.pointer("/source/data")).and_then(Value::as_str),
                            b.get("mime_type").or(b.pointer("/source/media_type")).and_then(Value::as_str),
                            b.pointer("/image_url/url").or(b.get("url")).and_then(Value::as_str),
                        );
                        let e = match (data, url) {
                            (Some(d), _) => mk(Kind::Media).with("role", role).with("media_type", mt.unwrap_or("image/png")).with("data", d),
                            (None, Some(u)) => match u.strip_prefix("data:").and_then(|r| r.split_once(";base64,")) {
                                Some((m, d)) => mk(Kind::Media).with("role", role).with("media_type", m).with("data", d),
                                None => mk(Kind::Media).with("role", role).with("url", u),
                            },
                            _ => continue,
                        };
                        out.push(e);
                    }
                    Some(s) if s.starts_with("text") => out.push(mk(Kind::Text).with("role", role).with("text", b.get("text").cloned().unwrap_or(Value::Null))),
                    _ => {}
                }
            }
        }
        Value::Object(_) => {}
        _ => {}
    }
}

/// The neutral events of one LangChain message.
fn message_events(cls: &str, d: &Map<String, Value>, id: &str, ts: Option<&str>) -> Vec<Event> {
    let mut n = 0usize;
    let mut mk = |kind: Kind| {
        let eid = if n == 0 { id.to_string() } else { format!("{id}#{n}") };
        n += 1;
        Event::new(eid, kind).ts(ts.map(str::to_string))
    };
    let mut out = Vec::new();
    let ty = str_at(d, "type").unwrap_or(match cls {
        "HumanMessage" => "human",
        "AIMessage" => "ai",
        "ToolMessage" => "tool",
        "SystemMessage" => "system",
        _ => "",
    });
    let content = d.get("content").cloned().unwrap_or(Value::Null);
    match ty {
        "human" => content_events(&content, "user", &mut mk, &mut out),
        "system" => content_events(&content, "system", &mut mk, &mut out),
        "ai" | "AIMessageChunk" => {
            let mut had_reasoning = false;
            if let Value::Array(blocks) = &content {
                for b in blocks {
                    match b.get("type").and_then(Value::as_str) {
                        Some("thinking") => {
                            had_reasoning = true;
                            let mut e = mk(Kind::Reasoning).with("text", b.get("thinking").cloned().unwrap_or(Value::Null));
                            // SPEC §3.1.5: thinking blocks and their signatures are Anthropic's.
                            if let Some(sig) = b.get("signature").and_then(Value::as_str).filter(|s| !s.is_empty()) {
                                e = e.with("sealed", json!({"vendor": "anthropic", "kind": "signature", "data": sig}));
                            }
                            out.push(e);
                        }
                        Some("redacted_thinking") => {
                            had_reasoning = true;
                            out.push(
                                mk(Kind::Reasoning)
                                    .with("redacted", true)
                                    .with("sealed", json!({"vendor": "anthropic", "kind": "encrypted", "data": b.get("data")})),
                            );
                        }
                        Some("reasoning") => {
                            had_reasoning = true;
                            let text = b.get("reasoning").or(b.get("text")).cloned();
                            let mut e = mk(Kind::Reasoning);
                            if let Some(t) = text {
                                e = e.with("text", t);
                            }
                            if let Some(s) = b.get("summary").and_then(Value::as_array) {
                                let sum: Vec<Value> = s.iter().filter_map(|x| x.get("text").or(Some(x)).cloned()).filter(Value::is_string).collect();
                                e = e.with("summary", json!(sum));
                            }
                            // SPEC §3.1.5: an OpenAI reasoning item's encrypted content.
                            if let Some(enc) = b.get("encrypted_content").and_then(Value::as_str).filter(|s| !s.is_empty()) {
                                e = e.with("sealed", json!({"vendor": "openai", "kind": "encrypted", "data": enc}));
                            }
                            out.push(e);
                        }
                        Some("text") => out.push(mk(Kind::Text).with("role", "assistant").with("text", b.get("text").cloned().unwrap_or(Value::Null))),
                        Some("image") | Some("image_url") => content_events(&json!([b]), "assistant", &mut mk, &mut out),
                        _ => {}
                    }
                }
            }
            if !had_reasoning && let Some(r) = d.get("additional_kwargs").and_then(|k| k.get("reasoning_content")).and_then(Value::as_str) {
                out.insert(0, mk(Kind::Reasoning).with("text", r));
                // keep ids in order: renumber below
            }
            if let Value::String(s) = &content
                && !s.is_empty()
            {
                out.push(mk(Kind::Text).with("role", "assistant").with("text", s.as_str()));
            }
            for c in d.get("tool_calls").and_then(Value::as_array).cloned().unwrap_or_default() {
                out.push(
                    mk(Kind::ToolCall)
                        .with("call_id", c.get("id").cloned().unwrap_or(Value::Null))
                        .with("name", c.get("name").cloned().unwrap_or(Value::Null))
                        .with("input", c.get("args").cloned().unwrap_or(json!({})))
                        .with("tool_type", "function"),
                );
            }
            if let Some(u) = d.get("usage_metadata").filter(|u| u.is_object()) {
                let g = |p: &str| u.pointer(p).cloned().unwrap_or(Value::Null);
                // SPEC §3.1.1: LangChain's input_tokens is every input token,
                // cache reads and creations included; neutral `input` is not.
                let n = |p: &str| u.pointer(p).and_then(Value::as_u64);
                let input =
                    match (n("/input_tokens"), n("/input_token_details/cache_read").unwrap_or(0) + n("/input_token_details/cache_creation").unwrap_or(0)) {
                        (Some(i), c) if c <= i => json!(i - c),
                        _ => g("/input_tokens"),
                    };
                out.push(
                    mk(Kind::Usage)
                        .with("model", d.get("response_metadata").and_then(|r| r.get("model_name")).cloned().unwrap_or(Value::Null))
                        .with("input", input)
                        .with("output", g("/output_tokens"))
                        .with("cache_read", g("/input_token_details/cache_read"))
                        .with("cache_write", g("/input_token_details/cache_creation"))
                        .with("reasoning", g("/output_token_details/reasoning")),
                );
            }
            for e in out.iter_mut() {
                e.group = Some(id.to_string());
            }
        }
        "tool" => {
            let output = match &content {
                Value::String(s) => json!(s),
                Value::Array(blocks) => Value::Array(
                    blocks
                        .iter()
                        .filter_map(|b| match b.get("type").and_then(Value::as_str) {
                            Some("text") => Some(json!({"type":"text","text": b.get("text")})),
                            _ => b.as_str().map(|s| json!({"type":"text","text": s})),
                        })
                        .collect(),
                ),
                other => json!(other.to_string()),
            };
            let mut e = mk(Kind::ToolResult).with("call_id", d.get("tool_call_id").cloned().unwrap_or(Value::Null)).with("output", output);
            if let Some(s) = str_at(d, "status") {
                e = e.with("is_error", s == "error");
            }
            out.push(e);
        }
        _ => {}
    }
    if out.is_empty() {
        out.push(mk(Kind::Native));
    }
    // ids follow order (a reasoning_content event may have been inserted first)
    for (i, e) in out.iter_mut().enumerate() {
        e.id = if i == 0 { id.to_string() } else { format!("{id}#{i}") };
    }
    out
}

/// Python's `datetime.isoformat()` for an RFC 3339 timestamp (UTC, microseconds).
fn py_iso(ts: &str) -> String {
    let ms = util::parse_rfc3339_ms(ts).unwrap_or(0);
    let t = util::ms_to_rfc3339(ms);
    format!("{}{}000+00:00", &t[..19], &t[19..23])
}

// ---------------------------------------------------------------- import

struct Ck {
    id: String,
    parent: Option<String>,
    row: Map<String, Value>,
    value: Value,
    meta: Value,
}

struct Wr {
    checkpoint_id: String,
    task_id: String,
    task_path: String,
    idx: i64,
    channel: String,
    row: Map<String, Value>,
}

pub fn import(input: &Path, opts: &ImportOptions) -> Result<Imported> {
    let files = stores(input);
    if files.is_empty() {
        bail!("{}: no LangGraph checkpoint store (tables checkpoints + writes)", input.display());
    }
    let root = if input.is_file() { input.parent().unwrap_or(Path::new(".")).to_path_buf() } else { input.to_path_buf() };
    let mut set = LedgerSet::new();
    let mut losses = Losses::default();
    let mut variant = "checkpoint-full";
    for p in &files {
        let rel = p.strip_prefix(&root).unwrap_or(p).to_string_lossy().replace('\\', "/");
        let rel = if input.is_file() { p.file_name().unwrap().to_string_lossy().to_string() } else { rel };
        if import_store(p, &rel, opts, &mut set, &mut losses)? {
            variant = "checkpoint-delta";
        }
    }
    link_subagents(&mut set);
    Ok(Imported { set, losses: losses.sorted(), variant: variant.into() })
}

fn import_store(p: &Path, rel: &str, opts: &ImportOptions, set: &mut LedgerSet, losses: &mut Losses) -> Result<bool> {
    let c = Connection::open_with_flags(p, OpenFlags::SQLITE_OPEN_READ_ONLY).with_context(|| p.display().to_string())?;
    let (schema, tables) = schema_of(&c)?;
    let mut other = Map::new();
    for t in tables.iter().filter(|t| *t != "checkpoints" && *t != "writes") {
        let rs = rows(&c, &format!("SELECT * FROM \"{t}\""), &[])?;
        other.insert(t.clone(), Value::Array(rs.into_iter().map(Value::Object).collect()));
    }
    let names: HashMap<String, String> = other
        .get("dcode_thread_names")
        .and_then(Value::as_array)
        .map(|a| a.iter().filter_map(|r| Some((r.get("thread_id")?.as_str()?.to_string(), r.get("name")?.as_str()?.to_string()))).collect())
        .unwrap_or_default();
    let has_path = columns(&c, "writes").contains("task_path");
    let ck_rows = rows(&c, "SELECT * FROM checkpoints ORDER BY thread_id, checkpoint_ns, checkpoint_id", &[])?;
    let wr_rows = rows(&c, "SELECT * FROM writes ORDER BY thread_id, checkpoint_ns, checkpoint_id, task_id, idx", &[])?;
    let mut groups: BTreeMap<(String, String), (Vec<Ck>, Vec<Wr>)> = BTreeMap::new();
    let mut delta = false;
    for r in ck_rows {
        let key = (r["thread_id"].as_str().unwrap_or("").to_string(), r["checkpoint_ns"].as_str().unwrap_or("").to_string());
        let value = loads(r.get("type").and_then(Value::as_str), &r["checkpoint"]).unwrap_or(Value::Null);
        let meta = blob(&r["metadata"])
            .and_then(|b| serde_json::from_slice(&b).ok())
            .or_else(|| loads(r.get("type").and_then(Value::as_str), &r["metadata"]))
            .unwrap_or(Value::Null);
        delta |= meta.get("counters_since_delta_snapshot").is_some();
        let ck = Ck {
            id: r["checkpoint_id"].as_str().unwrap_or("").to_string(),
            parent: r.get("parent_checkpoint_id").and_then(Value::as_str).map(str::to_string),
            row: r,
            value,
            meta,
        };
        groups.entry(key).or_default().0.push(ck);
    }
    for r in wr_rows {
        let key = (r["thread_id"].as_str().unwrap_or("").to_string(), r["checkpoint_ns"].as_str().unwrap_or("").to_string());
        let w = Wr {
            checkpoint_id: r["checkpoint_id"].as_str().unwrap_or("").to_string(),
            task_id: r["task_id"].as_str().unwrap_or("").to_string(),
            task_path: r.get("task_path").and_then(Value::as_str).unwrap_or("").to_string(),
            idx: r["idx"].as_i64().unwrap_or(0),
            channel: r["channel"].as_str().unwrap_or("").to_string(),
            row: r,
        };
        groups.entry(key).or_default().1.push(w);
    }
    let mut first = true;
    for ((thread, ns), (cks, wrs)) in groups {
        let mut l = import_thread(&thread, &ns, cks, wrs, rel, opts, losses, names.get(&thread).filter(|_| ns.is_empty()))?;
        if first {
            if let Some(src) = l.agent_mut().source.as_mut()
                && src.format == FORMAT
            {
                src.extra.insert("store".into(), json!({"schema": schema, "tables": other, "task_path": has_path}));
            }
            first = false;
        }
        set.insert(l);
    }
    Ok(delta)
}

fn agent_id(thread: &str, ns: &str) -> String {
    if ns.is_empty() { thread.to_string() } else { format!("{thread}/{ns}") }
}

#[allow(clippy::too_many_arguments)]
fn import_thread(
    thread: &str,
    ns: &str,
    cks: Vec<Ck>,
    mut wrs: Vec<Wr>,
    rel: &str,
    opts: &ImportOptions,
    losses: &mut Losses,
    title: Option<&String>,
) -> Result<Ledger> {
    let by_id: HashMap<String, usize> = cks.iter().enumerate().map(|(i, c)| (c.id.clone(), i)).collect();
    // The latest checkpoint (LangGraph: ORDER BY checkpoint_id DESC) and its chain.
    let mut chain: Vec<usize> = Vec::new();
    if let Some(target) = cks.iter().map(|c| c.id.clone()).max() {
        let mut cur = by_id.get(&target).copied();
        let mut seen = HashSet::new();
        while let Some(i) = cur {
            if !seen.insert(i) {
                break;
            }
            chain.push(i);
            cur = cks[i].parent.as_ref().and_then(|p| by_id.get(p)).copied();
        }
        chain.reverse();
    }
    if chain.len() < cks.len() {
        losses.add_n("branch-dropped", None, (cks.len() - chain.len()) as u64, "checkpoints off the latest chain (time travel, forks) are kept as rows only");
    }
    let id = agent_id(thread, ns);
    let first_meta = chain.first().map(|&i| cks[i].meta.clone()).unwrap_or(Value::Null);
    let ts_of = |i: usize| cks[i].value.get("ts").and_then(Value::as_str).map(util::norm_ts);
    let mut extra = Obj::new();
    extra.insert("file".into(), json!(rel));
    extra.insert("thread_id".into(), json!(thread));
    extra.insert("checkpoint_ns".into(), json!(ns));
    let mut l = Ledger::new(Agent {
        id: id.clone(),
        created_at: chain.first().and_then(|&i| ts_of(i)),
        source: Some(Source { format: FORMAT.into(), variant: None, version: None, native_id: Some(id.clone()), extra }),
        parent: None,
        links: vec![],
        meta: Obj::new(),
    });
    let mut restored_any = false;
    let mut carried_agent: Option<Agent> = None;
    let mut fresh: Vec<Event> = Vec::new();
    // settings from the run metadata deepagents-code copies into every checkpoint
    let meta_native = Native::new(FORMAT, Some(format!("{id}:metadata"))).record(first_meta.clone());
    let mut settings = Event::new(format!("{id}:metadata"), Kind::Settings)
        .ts(chain.first().and_then(|&i| ts_of(i)))
        .with("cwd", first_meta.get("cwd").cloned().unwrap_or(Value::Null))
        .with("git_branch", first_meta.get("git_branch").cloned().unwrap_or(Value::Null))
        .with("agent", first_meta.get("lc_agent_name").or(first_meta.get("agent_name")).cloned().unwrap_or(Value::Null));
    settings.native = Some(meta_native.clone());
    let mut lead = vec![settings];
    if let Some(t) = title {
        let mut e = Event::new(format!("{id}:title"), Kind::Title).with("text", t.as_str());
        e.native = Some(Native::new(FORMAT, Some(format!("{id}:title"))).record(json!({"thread_id": thread, "name": t})));
        lead.push(e);
    }
    wrs.sort_by(|a, b| (&a.checkpoint_id, &a.task_path, &a.task_id, a.idx).cmp(&(&b.checkpoint_id, &b.task_path, &b.task_id, b.idx)));
    let mut by_ck: HashMap<String, Vec<usize>> = HashMap::new();
    for (i, w) in wrs.iter().enumerate() {
        by_ck.entry(w.checkpoint_id.clone()).or_default().push(i);
    }
    let mut seen_msgs: HashSet<String> = HashSet::new();
    // Checkpoints protoledger rendered (restored from their carriers): their
    // rows are not native data and are rendered again on export.
    let mut rendered: HashSet<String> = HashSet::new();
    let mut file_seen: HashSet<String> = HashSet::new();
    for (pos, &ci) in chain.iter().enumerate() {
        let ck = &cks[ci];
        let commit_ts = chain.get(pos + 1).and_then(|&n| ts_of(n)).or_else(|| ts_of(ci));
        let ws: Vec<&Wr> = by_ck.get(&ck.id).map(|v| v.iter().map(|&i| &wrs[i]).collect()).unwrap_or_default();
        let values: Vec<Value> = ws.iter().map(|w| loads(w.row.get("type").and_then(Value::as_str), &w.row["value"]).unwrap_or(Value::Null)).collect();
        // A protoledger carrier for this checkpoint's writes.
        if opts.restore_carried
            && let Some(c) = ck.meta.get(carry::KEY)
            && let Some(carry::Carried { events: evs, agent }) = jsonl::carried_over(&json!(values), c)
        {
            if let Some(a) = agent {
                carried_agent = Some(a);
            }
            restored_any = true;
            rendered.insert(ck.id.clone());
            fresh.extend(evs);
            for v in &values {
                for it in items_of(v) {
                    if let Some((_, d)) = message_of(&it)
                        && let Some(mid) = str_at(&d, "id")
                    {
                        seen_msgs.insert(mid.to_string());
                    }
                }
            }
            continue;
        }
        // Seeds: a messages value held in the checkpoint itself.
        if let Some(seed) = ck.value.get("channel_values").and_then(|cv| cv.get("messages")) {
            for (n, it) in items_of(seed).iter().enumerate() {
                push_message(
                    &mut fresh,
                    &mut seen_msgs,
                    it,
                    &format!("{}/seed/{n}", ck.id),
                    ts_of(ci).as_deref(),
                    json!({"checkpoint_id": ck.id, "seed": n}),
                    &id,
                );
            }
        }
        for (w, v) in ws.iter().zip(values.iter()) {
            let loc = json!({"checkpoint_id": w.checkpoint_id, "task_id": w.task_id, "idx": w.idx, "channel": w.channel});
            match w.channel.as_str() {
                "messages" => {
                    let list = match overwrite_of(v) {
                        Some(ov) => {
                            fresh.push(
                                Event::new(format!("{}/{}/{}", w.checkpoint_id, w.task_id, w.idx), Kind::Compaction)
                                    .ts(commit_ts.clone())
                                    .with("phase", "boundary")
                                    .with("trigger", "auto")
                                    .native(
                                        Native::new(FORMAT, Some(format!("{}/{}/{}", w.checkpoint_id, w.task_id, w.idx)))
                                            .record(json!({"write": loc, "overwrite": true})),
                                    ),
                            );
                            items_of(&ov)
                        }
                        None => items_of(v),
                    };
                    for (n, it) in list.iter().enumerate() {
                        if let Some((_, d)) = message_of(it)
                            && str_at(&d, "type") == Some("remove")
                        {
                            if str_at(&d, "id") == Some("__remove_all__") {
                                let eid = format!("{}/{}/{}/{n}", w.checkpoint_id, w.task_id, w.idx);
                                fresh.push(
                                    Event::new(eid.clone(), Kind::Compaction)
                                        .ts(commit_ts.clone())
                                        .with("phase", "boundary")
                                        .with("trigger", "auto")
                                        .native(Native::new(FORMAT, Some(eid)).record(json!({"write": loc, "item": n, "message": it}))),
                                );
                            } else {
                                losses.add("message-removal-not-represented", None, "RemoveMessage of one message has no neutral form; the row is kept");
                            }
                            continue;
                        }
                        let mut l2 = loc.clone();
                        l2["item"] = json!(n);
                        push_message(
                            &mut fresh,
                            &mut seen_msgs,
                            it,
                            &format!("{}/{}/{}/{n}", w.checkpoint_id, w.task_id, w.idx),
                            commit_ts.as_deref(),
                            l2,
                            &id,
                        );
                    }
                }
                "files" => {
                    for (path, fd) in v.as_object().cloned().unwrap_or_default() {
                        let eid = format!("{}/{}/{}:{path}", w.checkpoint_id, w.task_id, w.idx);
                        let op = if fd.is_null() {
                            "delete"
                        } else if file_seen.contains(&path) {
                            "edit"
                        } else {
                            "create"
                        };
                        file_seen.insert(path.clone());
                        let content = match fd.get("content") {
                            Some(Value::Array(lines)) => json!(lines.iter().filter_map(Value::as_str).collect::<Vec<_>>().join("\n")),
                            Some(c) => c.clone(),
                            None => Value::Null,
                        };
                        let mut e = Event::new(eid.clone(), Kind::FileChange).ts(commit_ts.clone()).with("path", path.as_str()).with("op", op);
                        if !content.is_null() {
                            e = e.with("content", content);
                        }
                        e.native = Some(Native::new(FORMAT, Some(eid)).record(json!({"write": loc, "path": path, "file": fd})));
                        fresh.push(e);
                    }
                }
                _ => {}
            }
        }
    }
    let _ = restored_any;
    // Settings and title come from native rows, unless the root checkpoint
    // carried the original agent (then they are among the carried events).
    let lead = if carried_agent.is_some() { vec![] } else { lead };
    let native_count = lead.len() + fresh.iter().filter(|e| e.native.as_ref().is_some_and(|n| n.format == FORMAT)).count();
    for e in lead {
        l.append_unique(e);
    }
    for e in fresh {
        l.append_unique(e);
    }
    let rows_json = json!({
        "checkpoints": cks.iter().filter(|c| !rendered.contains(&c.id)).map(|c| Value::Object(c.row.clone())).collect::<Vec<_>>(),
        "writes": wrs.iter().filter(|w| !rendered.contains(&w.checkpoint_id)).map(|w| Value::Object(w.row.clone())).collect::<Vec<_>>(),
    });
    if let Some(a) = carried_agent {
        // Rendered by protoledger: the original agent header comes back.
        l.header.agent = a;
    } else {
        let src = l.agent_mut().source.as_mut().unwrap();
        src.extra.insert("rows".into(), rows_json);
        src.extra.insert("event_count".into(), json!(native_count));
    }
    Ok(l)
}

#[allow(clippy::too_many_arguments)]
fn push_message(out: &mut Vec<Event>, seen: &mut HashSet<String>, it: &Value, anchor: &str, ts: Option<&str>, loc: Value, _agent: &str) {
    let Some((cls, d)) = message_of(it) else { return };
    let mid = str_at(&d, "id").map(str::to_string);
    if let Some(m) = &mid
        && !seen.insert(m.clone())
    {
        return; // already in the ledger (seed and write, or a re-write)
    }
    let id = mid.unwrap_or_else(|| anchor.to_string());
    let n = Native::new(FORMAT, Some(id.clone())).record(json!({"write": loc, "message": it}));
    for (i, e) in message_events(&cls, &d, &id, ts).into_iter().enumerate() {
        out.push(e.native(if i == 0 { n.clone() } else { n.part_of(i as u32) }));
    }
}

/// `tools:<task_id>` namespaces are subagents run by the parent's tool task
/// whose write has that task id; the spawning call is that write's
/// ToolMessage `tool_call_id`.
fn link_subagents(set: &mut LedgerSet) {
    let ids: Vec<String> = set.ledgers.keys().cloned().collect();
    let mut updates: Vec<(String, Parent)> = Vec::new();
    for id in &ids {
        let l = &set.ledgers[id];
        if l.agent().parent.is_some() {
            continue;
        }
        let Some(extra) = l.agent().source.as_ref().filter(|s| s.format == FORMAT).map(|s| &s.extra) else { continue };
        let (Some(thread), Some(ns)) = (extra.get("thread_id").and_then(Value::as_str), extra.get("checkpoint_ns").and_then(Value::as_str)) else {
            continue;
        };
        if ns.is_empty() {
            continue;
        }
        let (parent_ns, last) = match ns.rsplit_once('|') {
            Some((p, l)) => (p.to_string(), l.to_string()),
            None => (String::new(), ns.to_string()),
        };
        let task = last.split_once(':').map(|x| x.1).unwrap_or(&last).to_string();
        let pid = agent_id(thread, &parent_ns);
        let Some(pl) = set.get(&pid) else { continue };
        let call = pl.events.iter().find_map(|e| {
            let r = e.native.as_ref()?.record_json()?;
            if e.kind == Kind::ToolResult && r.pointer("/write/task_id").and_then(Value::as_str) == Some(task.as_str()) {
                e.str("call_id").map(str::to_string)
            } else {
                None
            }
        });
        let event = call.and_then(|c| pl.events.iter().find(|e| e.kind == Kind::ToolCall && e.str("call_id") == Some(&c)).map(|e| e.id.clone()));
        updates.push((id.clone(), Parent { agent: pid, event, relation: Relation::Subagent }));
    }
    for (id, p) in updates {
        set.ledgers.get_mut(&id).unwrap().agent_mut().parent = Some(p);
    }
}

// ---------------------------------------------------------------- export

/// Where a ledger goes in the store.
#[derive(Clone)]
struct Slot {
    thread: String,
    ns: String,
}

fn slot_of(l: &Ledger, set: &LedgerSet, memo: &mut HashMap<String, Slot>) -> Slot {
    if let Some(s) = memo.get(l.id()) {
        return s.clone();
    }
    let s = match l.agent().source.as_ref().filter(|s| s.format == FORMAT).map(|s| &s.extra) {
        Some(x) if x.get("thread_id").is_some() => {
            Slot { thread: x["thread_id"].as_str().unwrap_or("").into(), ns: x.get("checkpoint_ns").and_then(Value::as_str).unwrap_or("").into() }
        }
        _ => match l.agent().parent.as_ref().filter(|p| p.relation == Relation::Subagent).and_then(|p| set.get(&p.agent)) {
            Some(pl) => {
                let ps = slot_of(pl, set, memo);
                let task = util::uuid_for(&format!("task\u{0}{}", l.id()));
                let ns = if ps.ns.is_empty() { format!("tools:{task}") } else { format!("{}|tools:{task}", ps.ns) };
                Slot { thread: ps.thread, ns }
            }
            None => Slot { thread: if jsonl::is_safe_id(l.id()) { l.id().to_string() } else { util::uuid_for(l.id()) }, ns: String::new() },
        },
    };
    memo.insert(l.id().to_string(), s.clone());
    s
}

/// The rows a deepagents-native ledger still stands for exactly.
fn native_rows(l: &Ledger) -> Option<(Vec<Value>, Vec<Value>, usize)> {
    let src = l.agent().source.as_ref().filter(|s| s.format == FORMAT)?;
    let rows = src.extra.get("rows")?;
    let want = src.extra.get("event_count")?.as_u64()? as usize;
    // Native events must come first and be complete; anything after is new.
    let n = l.events.iter().take_while(|e| e.native.as_ref().is_some_and(|n| n.format == FORMAT)).count();
    if n != want || l.events.iter().skip(n).any(|e| e.native.as_ref().is_some_and(|n| n.format == FORMAT)) {
        return None;
    }
    Some((rows["checkpoints"].as_array()?.clone(), rows["writes"].as_array()?.clone(), n))
}

pub fn export(set: &LedgerSet, opts: &ExportOptions) -> Result<Exported> {
    let mut losses = Losses::default();
    let store = set.iter().filter_map(|l| l.agent().source.as_ref().filter(|s| s.format == FORMAT)?.extra.get("store").cloned()).next();
    let file = set
        .iter()
        .filter_map(|l| l.agent().source.as_ref().filter(|s| s.format == FORMAT)?.extra.get("file")?.as_str().map(str::to_string))
        .next()
        .unwrap_or_else(|| "sessions.db".into());
    let dir = tempfile::tempdir()?;
    let path = dir.path().join("store.db");
    {
        let c = Connection::open(&path)?;
        match store.as_ref().and_then(|s| s.get("schema")).and_then(Value::as_array) {
            Some(stmts) => {
                for s in stmts.iter().filter_map(Value::as_str) {
                    c.execute_batch(s).with_context(|| format!("schema: {s}"))?;
                }
            }
            None => {
                for s in DDL {
                    c.execute_batch(s)?;
                }
            }
        }
        if let Some(tables) = store.as_ref().and_then(|s| s.get("tables")).and_then(Value::as_object) {
            for (t, rs) in tables {
                let cols = columns(&c, t);
                for r in rs.as_array().into_iter().flatten().filter_map(Value::as_object) {
                    insert(&c, t, r, &cols)?;
                }
            }
        }
        let ccols = columns(&c, "checkpoints");
        let wcols = columns(&c, "writes");
        let mut memo = HashMap::new();
        // Parents before children, so a subagent can name the parent's checkpoint.
        let mut order: Vec<&Ledger> = set.iter().collect();
        order.sort_by_key(|l| depth(l, set));
        let mut last_ck: HashMap<String, String> = HashMap::new();
        let mut names: Vec<(String, String)> = Vec::new();
        for l in order {
            let slot = slot_of(l, set, &mut memo);
            let (cks, wrs, skip) = match native_rows(l) {
                Some((cks, wrs, n)) => (cks, wrs, n),
                None => (vec![], vec![], 0),
            };
            for r in cks.iter().filter_map(Value::as_object) {
                insert(&c, "checkpoints", r, &ccols)?;
            }
            for r in wrs.iter().filter_map(Value::as_object) {
                insert(&c, "writes", r, &wcols)?;
            }
            let base = cks.iter().filter_map(|r| r.get("checkpoint_id")?.as_str().map(str::to_string)).max();
            let rest: Vec<&Event> = l.events.iter().skip(skip).collect();
            if rest.is_empty() {
                if let Some(b) = base {
                    last_ck.insert(l.id().to_string(), b);
                }
                continue;
            }
            let parent_ck = l.agent().parent.as_ref().and_then(|p| last_ck.get(&p.agent).cloned());
            let mut r = Render::new(l, &slot, set, &mut memo, &mut losses);
            let steps = r.steps(&rest);
            let (ckrows, wrrows, last, title) = r.rows(steps, base, parent_ck, opts.carry, skip == 0, wcols.contains("task_path"));
            for row in &ckrows {
                insert(&c, "checkpoints", row, &ccols)?;
            }
            for row in &wrrows {
                insert(&c, "writes", row, &wcols)?;
            }
            last_ck.insert(l.id().to_string(), last);
            if let Some(t) = title.filter(|_| slot.ns.is_empty() && skip == 0) {
                names.push((slot.thread.clone(), t));
            }
        }
        if !names.is_empty() {
            c.execute_batch(NAMES_DDL)?;
            for (t, n) in names {
                c.execute("INSERT OR REPLACE INTO dcode_thread_names (thread_id, name) VALUES (?, ?)", rusqlite::params![t, n])?;
            }
        }
    }
    let mut output = Output::default();
    output.put(file, std::fs::read(&path)?);
    Ok(Exported { output, losses: losses.sorted(), variant: "checkpoint-delta".into() })
}

fn depth(l: &Ledger, set: &LedgerSet) -> usize {
    let mut d = 0;
    let mut cur = l;
    let mut seen = HashSet::new();
    while let Some(p) = cur.agent().parent.as_ref().and_then(|p| set.get(&p.agent)) {
        if !seen.insert(p.id().to_string()) || d > 64 {
            break;
        }
        d += 1;
        cur = p;
    }
    d
}

/// One rendered step: the writes one task made, and the events they stand for.
struct Step {
    writes: Vec<(String, Value)>,
    events: Vec<Event>,
    ts: String,
}

struct Render<'a> {
    l: &'a Ledger,
    slot: Slot,
    set: &'a LedgerSet,
    /// task ids of child namespaces, by the call id that spawned them
    child_tasks: HashMap<String, String>,
    losses: &'a mut Losses,
}

impl<'a> Render<'a> {
    fn new(l: &'a Ledger, slot: &Slot, set: &'a LedgerSet, memo: &mut HashMap<String, Slot>, losses: &'a mut Losses) -> Self {
        let mut child_tasks = HashMap::new();
        for ch in set.children_of(l.id()) {
            let call = ch
                .agent()
                .parent
                .as_ref()
                .and_then(|p| p.event.as_ref())
                .and_then(|ev| l.events.iter().find(|e| &e.id == ev))
                .and_then(|e| e.str("call_id").map(str::to_string));
            let cs = slot_of(ch, set, memo);
            if let (Some(call), Some(task)) = (call, cs.ns.rsplit(':').next()) {
                child_tasks.insert(call, task.to_string());
            }
        }
        Render { l, slot: slot.clone(), set, child_tasks, losses }
    }

    fn ts(&self, e: &Event) -> String {
        e.ts.clone()
            .or_else(|| {
                let pos = self.l.events.iter().position(|x| x.id == e.id).unwrap_or(0);
                self.l.events[..pos].iter().rev().find_map(|x| x.ts.clone())
            })
            .or_else(|| self.l.agent().created_at.clone())
            .unwrap_or_else(|| util::ms_to_rfc3339(0))
    }

    fn msg_id(&self, e: &Event) -> String {
        util::uuid_for(&format!("{}\u{0}{}", self.l.id(), e.id))
    }

    fn steps(&mut self, events: &[&Event]) -> Vec<Step> {
        let _ = self.set;
        let mut steps: Vec<Step> = Vec::new();
        let mut pending: Vec<Event> = Vec::new();
        // the open message: (step index, group, role)
        let mut open: Option<(usize, Option<String>, &'static str)> = None;
        let mut call_names: HashMap<String, String> = HashMap::new();
        let mut losses: Vec<(&'static str, Kind, &'static str)> = Vec::new();
        // SPEC §2.3: a checkpoint holds one message list; the active path is
        // written, and an abandoned event rides on the next step.
        let l = self.l;
        let on = crate::formats::active_path_only(l, self.losses, "with carry they ride on the next step", |e| {
            e.native.as_ref().is_some_and(|n| n.format == FORMAT)
        });
        let off: HashSet<&str> = l.events.iter().zip(&on).filter(|(_, o)| !**o).map(|(e, _)| e.id.as_str()).collect();
        for e in events {
            let e: &Event = e;
            if off.contains(e.id.as_str()) {
                pending.push(e.clone());
                continue;
            }
            let b = &e.body;
            let s = |k: &str| b.get(k).and_then(Value::as_str).map(str::to_string);
            let role: Option<&'static str> = match e.kind {
                Kind::Text => Some(match s("role").as_deref() {
                    Some("assistant") => "ai",
                    Some("system") | Some("developer") => "system",
                    _ => "human",
                }),
                Kind::Media => Some(if s("role").as_deref() == Some("assistant") { "ai" } else { "human" }),
                Kind::Reasoning | Kind::ToolCall => Some("ai"),
                Kind::Usage => None,
                _ => None,
            };
            match (e.kind, role) {
                (Kind::Usage, _) => {
                    // usage_metadata on the open (or last) AI message
                    let target = open
                        .as_ref()
                        .filter(|o| o.2 == "ai")
                        .map(|o| o.0)
                        .or_else(|| steps.iter().rposition(|st| st.writes.first().is_some_and(|w| w.1.get("type") == Some(&json!("ai")))));
                    match target {
                        Some(i) if steps[i].writes[0].1.get("usage_metadata").is_none_or(Value::is_null) => {
                            let n = |k: &str| b.get(k).and_then(Value::as_u64).unwrap_or(0);
                            // SPEC §3.1.1 back into LangChain's counts: input_tokens includes the cache.
                            let input = n("input") + n("cache_read") + n("cache_write");
                            let mut u = json!({"input_tokens": input, "output_tokens": n("output"), "total_tokens": input + n("output")});
                            if b.get("cache_read").is_some() || b.get("cache_write").is_some() {
                                u["input_token_details"] = json!({"cache_read": n("cache_read"), "cache_creation": n("cache_write")});
                            }
                            if b.get("reasoning").is_some() {
                                u["output_token_details"] = json!({"reasoning": n("reasoning")});
                            }
                            let w = &mut steps[i].writes[0].1;
                            w["usage_metadata"] = u;
                            if let Some(m) = s("model") {
                                w["response_metadata"]["model_name"] = json!(m);
                            }
                            steps[i].events.append(&mut pending);
                            steps[i].events.push(e.clone());
                        }
                        _ => pending.push(e.clone()),
                    }
                }
                (Kind::Text | Kind::Media | Kind::Reasoning | Kind::ToolCall, Some(r)) => {
                    let same = open.as_ref().is_some_and(|o| o.2 == r && (r != "human" || e.group.is_some()) && o.1 == e.group);
                    let i = if same {
                        open.as_ref().unwrap().0
                    } else {
                        let ts = self.ts(e);
                        let msg = match r {
                            "ai" => json!({"content": "", "additional_kwargs": {}, "response_metadata": {}, "type": "ai", "name": null,
                                "id": format!("lc_run--{}-0", self.msg_id(e)), "tool_calls": [], "invalid_tool_calls": [], "usage_metadata": null}),
                            t => json!({"content": "", "additional_kwargs": {}, "response_metadata": {}, "type": t, "name": null, "id": self.msg_id(e)}),
                        };
                        steps.push(Step { writes: vec![("messages".into(), msg)], events: std::mem::take(&mut pending), ts });
                        open = Some((steps.len() - 1, e.group.clone(), r));
                        steps.len() - 1
                    };
                    let st = &mut steps[i];
                    st.events.append(&mut pending);
                    st.events.push(e.clone());
                    let m = &mut st.writes[0].1;
                    let block = match e.kind {
                        Kind::Text => json!({"type": "text", "text": s("text").unwrap_or_default()}),
                        Kind::Media => match (s("data"), s("media_type"), s("url")) {
                            (Some(d), mt, _) => {
                                json!({"type": "image", "source_type": "base64", "data": d, "mime_type": mt.unwrap_or_else(|| "image/png".into())})
                            }
                            (None, _, Some(u)) => json!({"type": "image", "source_type": "url", "url": u}),
                            _ => {
                                losses.push(("media-dropped", Kind::Media, "media without data or url"));
                                continue;
                            }
                        },
                        Kind::Reasoning => {
                            // SPEC §3.1.5: `text` is the reasoning, `summary` its summary texts.
                            let summary: Option<Vec<Value>> = b
                                .get("summary")
                                .and_then(Value::as_array)
                                .map(|a| a.iter().filter(|x| x.is_string()).map(|x| json!({"type": "summary_text", "text": x})).collect());
                            let reasoning = |text: Option<String>| {
                                let mut r = json!({"type": "reasoning"});
                                match (text, &summary) {
                                    (Some(t), _) => r["reasoning"] = json!(t),
                                    (None, None) => r["reasoning"] = json!(""),
                                    (None, Some(_)) => {}
                                }
                                if let Some(sm) = &summary {
                                    r["summary"] = json!(sm);
                                }
                                r
                            };
                            match b.get("sealed") {
                                Some(sl) if sl.get("kind").and_then(Value::as_str) == Some("signature") => {
                                    json!({"type": "thinking", "thinking": s("text").unwrap_or_default(), "signature": sl.get("data")})
                                }
                                Some(sl) if sl.get("vendor").and_then(Value::as_str) == Some("openai") && sl.get("data").is_some_and(Value::is_string) => {
                                    let mut r = reasoning(s("text"));
                                    r["encrypted_content"] = sl["data"].clone();
                                    r
                                }
                                Some(_) => {
                                    losses.push(("reasoning-sealed-dropped", Kind::Reasoning, "encrypted reasoning has no LangChain block"));
                                    reasoning(s("text"))
                                }
                                None => reasoning(s("text")),
                            }
                        }
                        _ => {
                            let call_id = s("call_id").unwrap_or_else(|| e.id.clone());
                            let name = s("name").unwrap_or_default();
                            call_names.insert(call_id.clone(), name.clone());
                            let args = match (b.get("input"), s("input_text")) {
                                (Some(v @ Value::Object(_)), _) => v.clone(),
                                (_, Some(t)) => {
                                    losses.push(("tool-input-text-wrapped", Kind::ToolCall, "raw tool input becomes {\"input\": text}"));
                                    json!({"input": t})
                                }
                                (Some(Value::Null) | None, None) => json!({}),
                                (Some(v), None) => {
                                    losses.push(("tool-input-text-wrapped", Kind::ToolCall, "non-object tool input becomes {\"input\": value}"));
                                    json!({"input": v})
                                }
                            };
                            m["tool_calls"].as_array_mut().unwrap().push(json!({"name": name, "args": args, "id": call_id, "type": "tool_call"}));
                            continue;
                        }
                    };
                    match &mut m["content"] {
                        Value::String(c) if c.is_empty() => {
                            m["content"] = if block["type"] == "text" { block["text"].clone() } else { json!([block]) };
                        }
                        Value::String(c) => {
                            let prev = json!({"type": "text", "text": c.clone()});
                            m["content"] = json!([prev, block]);
                        }
                        Value::Array(a) => a.push(block),
                        _ => {}
                    }
                }
                (Kind::ToolResult, _) => {
                    open = None;
                    let call = s("call_id").unwrap_or_default();
                    let content = match b.get("output") {
                        Some(Value::Array(parts)) => {
                            if parts.iter().any(|p| p.get("type").and_then(Value::as_str) != Some("text")) {
                                losses.push(("media-dropped", Kind::ToolResult, "tool output media becomes text-only content"));
                            }
                            json!(
                                parts
                                    .iter()
                                    .filter(|p| p.get("type").and_then(Value::as_str) == Some("text"))
                                    .map(|p| json!({"type": "text", "text": p.get("text")}))
                                    .collect::<Vec<_>>()
                            )
                        }
                        other => json!(jsonl::output_text(other)),
                    };
                    let status = if b.get("is_error").and_then(Value::as_bool) == Some(true) { "error" } else { "success" };
                    let msg = json!({"content": content, "additional_kwargs": {}, "response_metadata": {}, "type": "tool",
                        "name": call_names.get(&call).cloned(), "id": self.msg_id(e), "tool_call_id": call, "artifact": null, "status": status});
                    let mut evs = std::mem::take(&mut pending);
                    evs.push(e.clone());
                    steps.push(Step { writes: vec![("messages".into(), msg)], events: evs, ts: self.ts(e) });
                }
                (Kind::FileChange, _) => {
                    let path = s("path").unwrap_or_default();
                    let ts = py_iso(&self.ts(e));
                    let fd = match s("op").as_deref() {
                        Some("delete") => Value::Null,
                        _ => match b.get("content").and_then(Value::as_str) {
                            Some(c) => json!({"content": c, "encoding": "utf-8", "created_at": ts, "modified_at": ts}),
                            None => {
                                losses.push(("file-change-without-content", Kind::FileChange, "a diff-only file change cannot set file content"));
                                pending.push(e.clone());
                                continue;
                            }
                        },
                    };
                    let mut evs = std::mem::take(&mut pending);
                    evs.push(e.clone());
                    steps.push(Step { writes: vec![("files".into(), json!({path: fd}))], events: evs, ts: self.ts(e) });
                    open = None;
                }
                (Kind::Compaction, _) if b.get("summary").is_some() => {
                    open = None;
                    let msg = json!({"content": s("summary").unwrap_or_default(), "additional_kwargs": {}, "response_metadata": {},
                        "type": "human", "name": null, "id": self.msg_id(e)});
                    losses.push(("compaction-summary-as-human", Kind::Compaction, "a compaction summary becomes a human message"));
                    let mut evs = std::mem::take(&mut pending);
                    evs.push(e.clone());
                    steps.push(Step { writes: vec![("messages".into(), msg)], events: evs, ts: self.ts(e) });
                }
                (Kind::PeerMessage, _) => {
                    open = None;
                    // SPEC §3.1.6: wrapped text, which the importer unwraps.
                    if crate::formats::pi_session::peer_has_more(e) {
                        losses.push((
                            crate::formats::pi_session::PEER_FIELDS_DROPPED,
                            Kind::PeerMessage,
                            crate::formats::pi_session::PEER_FIELDS_DROPPED_DETAIL,
                        ));
                    }
                    let msg = match crate::formats::pi_session::peer_as_text(e) {
                        ("assistant", text) => json!({"content": text, "additional_kwargs": {}, "response_metadata": {}, "type": "ai", "name": null,
                            "id": format!("lc_run--{}-0", self.msg_id(e)), "tool_calls": [], "invalid_tool_calls": [], "usage_metadata": null}),
                        (_, text) => json!({"content": text, "additional_kwargs": {}, "response_metadata": {},
                            "type": "human", "name": null, "id": self.msg_id(e)}),
                    };
                    let mut evs = std::mem::take(&mut pending);
                    evs.push(e.clone());
                    steps.push(Step { writes: vec![("messages".into(), msg)], events: evs, ts: self.ts(e) });
                }
                (Kind::Settings | Kind::Title | Kind::Turn, _) => pending.push(e.clone()),
                (k, _) => {
                    let code = match k {
                        Kind::Compaction => "compaction-boundary-dropped",
                        Kind::Subagent => "subagent-marker-dropped",
                        Kind::Error => "error-dropped",
                        Kind::App => "app-dropped",
                        _ => "foreign-native-dropped",
                    };
                    losses.push((code, k, "no LangGraph state holds it"));
                    pending.push(e.clone());
                }
            }
            if e.effective_audience() == Audience::Record && matches!(e.kind, Kind::Text) {
                losses.push(("record-only-text-shown", Kind::Text, "text people saw but the model did not becomes a message"));
            }
        }
        for (c, k, d) in losses {
            self.losses.add(c, Some(k), d);
        }
        if !pending.is_empty() {
            match steps.last_mut() {
                Some(st) => st.events.append(&mut pending),
                None => steps.push(Step { writes: vec![], events: pending, ts: self.l.agent().created_at.clone().unwrap_or_else(|| util::ms_to_rfc3339(0)) }),
            }
        }
        steps
    }

    /// Checkpoint and write rows for `steps`, chained after `base` (a native
    /// checkpoint being continued) or from a fresh root.
    #[allow(clippy::too_many_arguments)]
    #[allow(clippy::type_complexity)] // a one-off tuple of rows, named by the function that builds it
    fn rows(
        &self,
        steps: Vec<Step>,
        base: Option<String>,
        parent_ck: Option<String>,
        carry: bool,
        fresh_thread: bool,
        task_path: bool,
    ) -> (Vec<Map<String, Value>>, Vec<Map<String, Value>>, String, Option<String>) {
        let mut cks = Vec::new();
        let mut wrs = Vec::new();
        let cwd = self.l.events.iter().filter(|e| e.kind == Kind::Settings).find_map(|e| e.str("cwd").map(str::to_string));
        let branch = self.l.events.iter().filter(|e| e.kind == Kind::Settings).find_map(|e| e.str("git_branch").map(str::to_string));
        let agent_name = self.l.events.iter().filter(|e| e.kind == Kind::Settings).find_map(|e| e.str("agent").map(str::to_string));
        let title = self.l.events.iter().find(|e| e.kind == Kind::Title).and_then(|e| e.str("text").map(str::to_string));
        let mut prev = base.clone();
        let mut last_time = base.as_deref().and_then(uuid6_time).unwrap_or(0);
        let mut versions: Map<String, Value> = Map::new();
        let mut counts: HashMap<String, u64> = HashMap::new();
        let n = steps.len();
        // n steps → n checkpoints carrying writes, plus the final (target) one.
        for i in 0..=n {
            let step = steps.get(i);
            let ts = step.map(|s| s.ts.clone()).or_else(|| steps.last().map(|s| s.ts.clone())).unwrap_or_else(|| util::ms_to_rfc3339(0));
            let mut t = ts_100ns(&ts);
            if t <= last_time {
                t = last_time + 1;
            }
            last_time = t;
            let id = uuid6(t, &format!("{}\u{0}{}\u{0}{i}", self.l.id(), self.slot.ns));
            let updated: Vec<Value> = if i == 0 { vec![] } else { steps[i - 1].writes.iter().map(|w| json!(w.0)).collect() };
            for u in &updated {
                let ch = u.as_str().unwrap().to_string();
                let c = counts.entry(ch.clone()).or_insert(0);
                *c += 1;
                versions.insert(ch.clone(), json!(format!("{:032}.0.{}", *c + 1, frac(&format!("{id}{ch}")))));
            }
            let ckpt = json!({"v": 4, "ts": py_iso(&ts), "id": id, "channel_values": {}, "channel_versions": versions.clone(),
                "versions_seen": {"__input__": {}}, "updated_channels": updated});
            let step_no = if fresh_thread { i as i64 - 1 } else { i as i64 };
            let mut meta = Map::new();
            meta.insert("source".into(), json!(if fresh_thread && i == 0 { "input" } else { "loop" }));
            meta.insert("step".into(), json!(step_no));
            meta.insert(
                "parents".into(),
                match &parent_ck {
                    Some(p) => json!({"": p}),
                    None => json!({}),
                },
            );
            let mut counters = Map::new();
            for (ch, c) in &counts {
                counters.insert(ch.clone(), json!([c, i + 1]));
            }
            meta.insert("counters_since_delta_snapshot".into(), Value::Object(counters));
            if self.slot.ns.is_empty() {
                meta.insert("ls_integration".into(), json!("deepagents"));
                meta.insert("agent_name".into(), json!(agent_name.clone().unwrap_or_else(|| "agent".into())));
                meta.insert("dcode_agent_name".into(), json!(agent_name.clone().unwrap_or_else(|| "agent".into())));
            } else {
                meta.insert("ls_integration".into(), json!("langchain_create_agent"));
                meta.insert("lc_agent_name".into(), json!(agent_name.clone().unwrap_or_else(|| "general-purpose".into())));
                meta.insert("ls_agent_type".into(), json!("subagent"));
            }
            meta.insert("updated_at".into(), json!(py_iso(&ts)));
            if let Some(c) = &cwd {
                meta.insert("cwd".into(), json!(c));
            }
            if let Some(b) = &branch {
                meta.insert("git_branch".into(), json!(b));
            }
            // this checkpoint's writes: step i
            let mut values = Vec::new();
            if let Some(st) = step {
                for (idx, (ch, v)) in st.writes.iter().enumerate() {
                    let val = match ch.as_str() {
                        "messages" => json!([lc_ext(v)]),
                        _ => v.clone(),
                    };
                    let task = match (ch.as_str(), v.get("type").and_then(Value::as_str), v.get("tool_call_id").and_then(Value::as_str)) {
                        ("messages", Some("tool"), Some(call)) if self.child_tasks.contains_key(call) => self.child_tasks[call].clone(),
                        _ => util::uuid_for(&format!("{id}\u{0}task")),
                    };
                    let path = match v.get("type").and_then(Value::as_str) {
                        _ if i == 0 && fresh_thread => "~__pregel_pull, __start__",
                        Some("ai") => "~__pregel_pull, model",
                        _ => "~__pregel_push, 0000000000, 0000000000",
                    };
                    let mut w = Map::new();
                    w.insert("thread_id".into(), json!(self.slot.thread));
                    w.insert("checkpoint_ns".into(), json!(self.slot.ns));
                    w.insert("checkpoint_id".into(), json!(id));
                    w.insert("task_id".into(), json!(task));
                    if task_path {
                        w.insert("task_path".into(), json!(path));
                    }
                    w.insert("idx".into(), json!(idx));
                    w.insert("channel".into(), json!(ch));
                    w.insert("type".into(), json!("msgpack"));
                    w.insert("value".into(), blob_value(&msgpack::encode(&val)));
                    values.push(val);
                    wrs.push(w);
                }
            }
            let events: &[Event] = step.map(|s| s.events.as_slice()).unwrap_or(&[]);
            if carry && (!events.is_empty() || (i == 0 && fresh_thread)) {
                let agent = (i == 0 && fresh_thread).then_some(self.l.agent());
                if let Some(c) = jsonl::carrier_over(&json!(values), events, agent) {
                    meta.insert(carry::KEY.into(), c);
                }
            }
            let mut r = Map::new();
            r.insert("thread_id".into(), json!(self.slot.thread));
            r.insert("checkpoint_ns".into(), json!(self.slot.ns));
            r.insert("checkpoint_id".into(), json!(id));
            r.insert("parent_checkpoint_id".into(), prev.clone().map(Value::String).unwrap_or(Value::Null));
            r.insert("type".into(), json!("msgpack"));
            r.insert("checkpoint".into(), blob_value(&msgpack::encode(&ckpt)));
            r.insert("metadata".into(), blob_value(pyjson::dumps(&Value::Object(meta), pyjson::Opts { indent: None, ensure_ascii: false }).as_bytes()));
            cks.push(r);
            prev = Some(id);
        }
        (cks, wrs, prev.unwrap_or_default(), title)
    }
}

/// A LangChain message dict as JsonPlusSerializer writes it (ext 5).
fn lc_ext(m: &Value) -> Value {
    let (module, cls) = match m.get("type").and_then(Value::as_str) {
        Some("ai") => ("langchain_core.messages.ai", "AIMessage"),
        Some("tool") => ("langchain_core.messages.tool", "ToolMessage"),
        Some("system") => ("langchain_core.messages.system", "SystemMessage"),
        _ => ("langchain_core.messages.human", "HumanMessage"),
    };
    json!({"__ext__": 5, "data": [module, cls, m, "model_validate_json"]})
}

/// 100-ns intervals since 1582-10-15 (UUID v1/v6 time) for a timestamp.
fn ts_100ns(ts: &str) -> u64 {
    let ms = util::parse_rfc3339_ms(ts).unwrap_or(0).max(0) as u64;
    ms * 10_000 + 0x01B2_1DD2_1381_4000
}

/// A UUID v6 (time-ordered, as LangGraph's checkpoint ids) for time `t`.
fn uuid6(t: u64, seed: &str) -> String {
    let h = util::short_id(seed, 16);
    let hi = (t >> 12) & 0xffff_ffff_ffff;
    let lo = t & 0x0fff;
    let s = format!("{hi:012x}6{lo:03x}");
    let clock = (u16::from_str_radix(&h[0..4], 16).unwrap_or(0) & 0x3fff) | 0x8000;
    format!("{}-{}-{}-{clock:04x}-{}", &s[0..8], &s[8..12], &s[12..16], &h[4..16])
}

fn uuid6_time(id: &str) -> Option<u64> {
    let hex: String = id.chars().filter(|c| *c != '-').collect();
    if hex.len() != 32 || &hex[12..13] != "6" {
        return None;
    }
    let hi = u64::from_str_radix(&hex[0..12], 16).ok()?;
    let lo = u64::from_str_radix(&hex[13..16], 16).ok()?;
    Some((hi << 12) | lo)
}

/// The random fraction LangGraph appends to channel versions, made deterministic.
fn frac(seed: &str) -> String {
    let h = util::short_id(seed, 16);
    let n = u64::from_str_radix(&h, 16).unwrap_or(0);
    format!("{}", n % 10_000_000_000_000_000)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn uuid6_is_time_ordered_and_parses() {
        let a = uuid6(ts_100ns("2026-10-16T23:57:00.010Z"), "x");
        let b = uuid6(ts_100ns("2026-10-16T23:57:00.011Z"), "y");
        assert!(a < b, "{a} {b}");
        assert_eq!(&a[14..15], "6");
        assert_eq!(uuid6_time(&a), Some(ts_100ns("2026-10-16T23:57:00.010Z")));
        // a real LangGraph id
        assert!(uuid6_time("1f1c9bd4-6dd4-67ba-bfff-31aca60a0345").is_some());
    }

    /// SPEC §3.1.1: LangChain's `input_tokens` counts the cache; neutral
    /// `input` does not. Written as LangChain counts, read back as neutral.
    #[test]
    fn usage_input_excludes_the_cache_both_ways() {
        use crate::formats::{ExportOptions, Format, ImportOptions};
        let mut l = Ledger::new(crate::model::Agent {
            id: "u".into(),
            created_at: Some("2026-01-01T00:00:00Z".into()),
            source: None,
            parent: None,
            links: vec![],
            meta: Default::default(),
        });
        l.append(Event::new("q", Kind::Text).with("role", "user").with("text", "go").ts(Some("2026-01-01T00:00:01Z"))).unwrap();
        l.append(Event::new("a", Kind::Text).with("role", "assistant").with("text", "hi").group(Some("g".to_string())).ts(Some("2026-01-01T00:00:02Z")))
            .unwrap();
        l.append(
            Event::new("n", Kind::Usage)
                .with("input", 10)
                .with("output", 3)
                .with("cache_read", 100)
                .with("cache_write", 5)
                .group(Some("g".to_string()))
                .ts(Some("2026-01-01T00:00:02Z")),
        )
        .unwrap();
        let out = super::super::DeepAgents.export(&LedgerSet::single(l), &ExportOptions::no_carry()).unwrap();
        let t = tempfile::tempdir().unwrap();
        let path = crate::roundtrip::materialise(&out.output, t.path()).unwrap();
        let back = super::super::DeepAgents.import(&path, &ImportOptions::standard()).unwrap().set;
        let evs: Vec<&Event> = back.iter().flat_map(|l| l.events.iter()).collect();
        let u = evs.iter().find(|e| e.kind == Kind::Usage).expect("usage back");
        assert_eq!((u.body["input"].clone(), u.body["cache_read"].clone(), u.body["cache_write"].clone()), (json!(10), json!(100), json!(5)));
        // What was written: LangChain's count, the cache included.
        let written =
            evs.iter().filter_map(|e| e.native.as_ref()?.record_json()).find_map(|r| r.pointer("/message/data/2/usage_metadata/input_tokens").cloned());
        assert_eq!(written, Some(json!(115)), "input_tokens as written");
    }

    #[test]
    fn maps_an_ai_message() {
        let d: Map<String, Value> = serde_json::from_value(json!({"content": [{"type":"thinking","thinking":"t","signature":"s"},{"type":"text","text":"hi"}],
            "additional_kwargs": {}, "response_metadata": {"model_name": "m", "model_provider": "anthropic"}, "type": "ai", "id": "a1",
            "tool_calls": [{"name":"ls","args":{},"id":"c1","type":"tool_call"}], "usage_metadata": {"input_tokens": 3, "output_tokens": 4, "total_tokens": 7}}))
        .unwrap();
        let evs = message_events("AIMessage", &d, "a1", None);
        let kinds: Vec<Kind> = evs.iter().map(|e| e.kind).collect();
        assert_eq!(kinds, vec![Kind::Reasoning, Kind::Text, Kind::ToolCall, Kind::Usage]);
        assert_eq!(evs[0].body["sealed"]["vendor"], "anthropic");
        assert_eq!(evs[3].id, "a1#3");
    }
}
