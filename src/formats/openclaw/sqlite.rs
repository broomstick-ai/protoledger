//! OpenClaw's SQLite session store (v2026.8.1 on):
//! `~/.openclaw/agents/<agentId>/agent/openclaw-agent.sqlite`.
//!
//! `transcript_events` holds one row per transcript line (`event_json`, or the
//! same JSON zstd-compressed in `event_zstd`), so each session is imported
//! exactly as its JSONL would be. `session_nodes` / `session_windows` give
//! the session key, label, `spawned_by` and `previous_session_id`.
//!
//! For a row-identical rebuild the whole store is captured: every schema
//! object, every row of every ordinary table, the content of FTS tables and
//! the file's header values ([`sqlite_header`]).
//! On export, tables are created and filled first and indexes, triggers and
//! views come last, so OpenClaw's triggers do not fire on restored rows.

use super::{FORMAT, import_text};
use crate::formats::hermes::db::{columns, insert, rows};
use crate::formats::{ImportOptions, Losses, sqlite_header};
use crate::ledger::{Ledger, LedgerSet};
use crate::model::{Event, Kind, Link, Native, Obj, Parent, Relation};
use crate::util::OrderedMap;
use anyhow::{Context, Result};
use base64::Engine;
use rusqlite::{Connection, OpenFlags};
use serde_json::{Map, Value, json};
use std::collections::{HashMap, HashSet};
use std::path::Path;

/// OpenClaw's agent store, if `p` is one.
pub fn probe(p: &Path) -> bool {
    let Ok(mut f) = std::fs::File::open(p) else { return false };
    let mut head = [0u8; 16];
    use std::io::Read;
    if f.read_exact(&mut head).is_err() || &head != b"SQLite format 3\0" {
        return false;
    }
    let Ok(c) = Connection::open_with_flags(p, OpenFlags::SQLITE_OPEN_READ_ONLY) else { return false };
    c.query_row("SELECT count(*) FROM sqlite_master WHERE type='table' AND name IN ('transcript_events','session_windows')", [], |r| r.get::<_, i64>(0)).ok()
        == Some(2)
}

/// The store as JSON: schema objects in order, ordinary-table rows, FTS rows.
fn snapshot(c: &Connection, skip: &[&str]) -> Result<Value> {
    let master = rows(c, "SELECT type, name, tbl_name, sql FROM sqlite_master WHERE sql IS NOT NULL ORDER BY rowid", &[])?;
    let virtuals: Vec<(String, String)> = master
        .iter()
        .filter(|r| r["sql"].as_str().is_some_and(|s| s.trim_start().to_ascii_uppercase().starts_with("CREATE VIRTUAL TABLE")))
        .map(|r| (r["name"].as_str().unwrap_or("").to_string(), r["sql"].as_str().unwrap_or("").to_string()))
        .collect();
    let is_shadow = |name: &str| virtuals.iter().any(|(v, _)| v != name && crate::formats::hermes::db::is_shadow_of(v, name));
    let mut objects = Vec::new();
    let mut table_rows = Map::new();
    let mut fts_rows = Map::new();
    for r in &master {
        let (ty, name) = (r["type"].as_str().unwrap_or(""), r["name"].as_str().unwrap_or(""));
        if name.starts_with("sqlite_") || (ty == "table" && is_shadow(name)) {
            continue;
        }
        objects.push(json!({"type": ty, "name": name, "sql": r["sql"]}));
        if ty != "table" || skip.contains(&name) {
            continue;
        }
        if let Some((_, sql)) = virtuals.iter().find(|(v, _)| v == name) {
            if !external_content(sql) {
                let rs = rows(c, &format!("SELECT rowid AS \"$rowid\", * FROM \"{name}\""), &[])?;
                fts_rows.insert(name.to_string(), Value::Array(rs.into_iter().map(Value::Object).collect()));
            }
            continue;
        }
        let rs = rows(c, &format!("SELECT * FROM \"{name}\""), &[])?;
        table_rows.insert(name.to_string(), Value::Array(rs.into_iter().map(Value::Object).collect()));
    }
    let seq = rows(c, "SELECT name, seq FROM sqlite_sequence", &[]).unwrap_or_default();
    let mut store = json!({"objects": objects, "rows": table_rows, "fts": fts_rows, "sqlite_sequence": seq});
    if let Some(h) = sqlite_header::read(c)? {
        store[sqlite_header::KEY] = h;
    }
    Ok(store)
}

fn external_content(sql: &str) -> bool {
    let s: String = sql.chars().filter(|c| !c.is_whitespace()).collect::<String>().to_ascii_lowercase();
    s.contains("content='") && !s.contains("content=''") || s.contains("content=\"")
}

/// One transcript row's JSON text.
fn event_text(r: &Map<String, Value>, losses: &mut Losses) -> Option<String> {
    if let Some(s) = r.get("event_json").and_then(Value::as_str) {
        return Some(s.to_string());
    }
    let b = r.get("event_zstd")?.get("$blob")?.as_str()?;
    let bytes = base64::engine::general_purpose::STANDARD.decode(b).ok()?;
    #[cfg(feature = "zstd")]
    {
        let _ = &losses;
        zstd::decode_all(&bytes[..]).ok().map(|v| String::from_utf8_lossy(&v).into_owned())
    }
    #[cfg(not(feature = "zstd"))]
    {
        let _ = bytes;
        losses.add("zstd-transcript-unread", None, "event_zstd rows need the `zstd` feature");
        None
    }
}

pub fn import(p: &Path, rel: &str, opts: &ImportOptions, losses: &mut Losses) -> Result<Vec<Ledger>> {
    let c = Connection::open_with_flags(p, OpenFlags::SQLITE_OPEN_READ_ONLY).with_context(|| p.display().to_string())?;
    let store = snapshot(&c, &["transcript_events"])?;
    let version = c
        .query_row("SELECT schema_version, app_version FROM schema_meta ORDER BY meta_key LIMIT 1", [], |r| {
            Ok((r.get::<_, i64>(0)?, r.get::<_, Option<String>>(1)?))
        })
        .ok();
    let agent_dir = rel.split('/').collect::<Vec<_>>();
    // agents/<agentId>/agent/openclaw-agent.sqlite → agentId
    let agent_name = agent_dir.iter().position(|s| *s == "agents").and_then(|i| agent_dir.get(i + 1)).copied().unwrap_or("main");
    let tcols = columns(&c, "transcript_events");
    let order = if tcols.contains("seq") { "ORDER BY session_id, seq" } else { "ORDER BY rowid" };
    let mut by_session: Vec<(String, Vec<Map<String, Value>>)> = Vec::new();
    for r in rows(&c, &format!("SELECT * FROM transcript_events {order}"), &[])? {
        let sid = r.get("session_id").and_then(Value::as_str).unwrap_or("").to_string();
        match by_session.last_mut() {
            Some((s, v)) if *s == sid => v.push(r),
            _ => by_session.push((sid, vec![r])),
        }
    }
    let windows: HashMap<String, Map<String, Value>> =
        rows(&c, "SELECT * FROM session_windows", &[])?.into_iter().filter_map(|r| Some((r.get("session_id")?.as_str()?.to_string(), r))).collect();
    let nodes: HashMap<String, Map<String, Value>> = if columns(&c, "session_nodes").is_empty() {
        HashMap::new()
    } else {
        rows(&c, "SELECT * FROM session_nodes", &[])?.into_iter().filter_map(|r| Some((r.get("session_key")?.as_str()?.to_string(), r))).collect()
    };
    let mut out = Vec::new();
    for (i, (sid, trows)) in by_session.into_iter().enumerate() {
        let mut text = String::new();
        for r in &trows {
            if let Some(t) = event_text(r, losses) {
                text.push_str(&t);
                text.push('\n');
            }
        }
        let jsonl_rel = format!("agents/{agent_name}/sessions/{sid}.jsonl");
        let (mut l, _) = import_text(&text, &jsonl_rel, opts, losses)?;
        let carried = l.agent().source.as_ref().is_none_or(|s| s.format != FORMAT);
        // The node's label is the session title OpenClaw shows.
        let key = windows.get(&sid).and_then(|w| w.get("session_key")).and_then(Value::as_str).map(str::to_string);
        let node = key.as_ref().and_then(|k| nodes.get(k));
        let mut title_count = 0;
        if !carried && let Some(n) = node {
            let label = n
                .get("label")
                .and_then(Value::as_str)
                .filter(|s| !s.trim().is_empty())
                .or(n.get("display_name").and_then(Value::as_str).filter(|s| !s.trim().is_empty()));
            if let Some(t) = label {
                let id = format!("session_nodes:{}", key.as_deref().unwrap_or(""));
                let e = Event::new(id.clone(), Kind::Title)
                    .with("text", t)
                    .native(Native::new(FORMAT, Some(id)).variant(Some("sqlite")).file(Some(format!("{rel}#session_nodes"))).record(Value::Object(n.clone())));
                l.append_unique(e);
                title_count = 1;
            }
        }
        let native_events = l.events.iter().filter(|e| e.native.as_ref().is_some_and(|n| n.format == FORMAT)).count();
        if let Some(src) = l.agent_mut().source.as_mut().filter(|s| s.format == FORMAT) {
            src.variant = Some("sqlite".into());
            if let Some((v, app)) = &version {
                src.version = Some(match app {
                    Some(a) => format!("{a} (agent schema {v})"),
                    None => format!("agent schema {v}"),
                });
            }
            let mut x = json!({"file": rel, "session_id": sid, "rows": trows, "event_count": native_events, "title_events": title_count});
            if let Some(k) = &key {
                x["session_key"] = json!(k);
            }
            if i == 0 {
                x["store"] = store.clone();
            }
            src.extra.insert("sqlite".into(), x);
            if let Some(k) = &key {
                src.extra.insert("session_key".into(), json!(k));
            }
        }
        out.push((l, windows.get(&sid).cloned(), node.cloned()));
    }
    link(&mut out, &windows, &nodes);
    Ok(out.into_iter().map(|(l, _, _)| l).collect())
}

/// `spawned_by` (a parent session key) → structural subagent;
/// `previous_session_id` (reset, rollover) → continued-from / continued-in.
#[allow(clippy::type_complexity)] // a one-off tuple of rows, named by the function that builds it
fn link(
    out: &mut [(Ledger, Option<Map<String, Value>>, Option<Map<String, Value>>)],
    windows: &HashMap<String, Map<String, Value>>,
    nodes: &HashMap<String, Map<String, Value>>,
) {
    let ids: HashSet<String> = out.iter().map(|(l, _, _)| l.id().to_string()).collect();
    let key_to_sid: HashMap<String, String> = nodes
        .iter()
        .filter_map(|(k, n)| Some((k.clone(), n.get("current_session_id")?.as_str()?.to_string())))
        .chain(windows.iter().filter_map(|(s, w)| Some((w.get("session_key")?.as_str()?.to_string(), s.clone()))))
        .collect();
    let mut continued: Vec<(String, String)> = Vec::new();
    let mut parents: Vec<(usize, String, String)> = Vec::new();
    for (i, (l, w, n)) in out.iter().enumerate() {
        if l.agent().parent.is_some() || l.agent().source.as_ref().is_none_or(|s| s.format != FORMAT) {
            continue;
        }
        let spawned = n.as_ref().and_then(|n| n.get("spawned_by")).or(w.as_ref().and_then(|w| w.get("spawned_by"))).and_then(Value::as_str);
        if let Some(psid) = spawned.and_then(|k| key_to_sid.get(k)).filter(|p| *p != l.id() && ids.contains(*p)) {
            let key = l.agent().source.as_ref().and_then(|s| s.extra.get("session_key")).and_then(Value::as_str).unwrap_or("").to_string();
            parents.push((i, psid.clone(), key));
        }
        if let Some(prev) = w.as_ref().and_then(|w| w.get("previous_session_id")).and_then(Value::as_str) {
            continued.push((l.id().to_string(), prev.to_string()));
        }
    }
    for (i, psid, key) in parents {
        let event = out.iter().find(|(l, _, _)| l.id() == psid).and_then(|(pl, _, _)| {
            let call = pl
                .events
                .iter()
                .find(|x| x.kind == Kind::ToolResult && !key.is_empty() && super::jsonl::output_text(x.body.get("output")).contains(&key))?
                .str("call_id")?
                .to_string();
            pl.events.iter().find(|x| x.kind == Kind::ToolCall && x.str("call_id") == Some(&call)).map(|x| x.id.clone())
        });
        out[i].0.agent_mut().parent = Some(Parent { agent: psid, event, relation: Relation::Subagent });
    }
    for (child, prev) in continued {
        for (l, _, _) in out.iter_mut() {
            if l.id() == child && !l.agent().links.iter().any(|k| k.rel == "continued-from") {
                l.agent_mut().links.push(Link { rel: "continued-from".into(), agent: Some(prev.clone()), event: None, extra: Obj::new() });
            }
            if l.id() == prev && !l.agent().links.iter().any(|k| k.rel == "continued-in" && k.agent.as_deref() == Some(&child)) {
                l.agent_mut().links.push(Link { rel: "continued-in".into(), agent: Some(child.clone()), event: None, extra: Obj::new() });
            }
        }
    }
}

/// The transcript rows an OpenClaw-SQLite ledger still stands for exactly.
pub fn intact_rows(l: &Ledger) -> Option<(String, Vec<Value>)> {
    let src = l.agent().source.as_ref().filter(|s| s.format == FORMAT)?;
    let x = src.extra.get("sqlite")?;
    let want = x.get("event_count")?.as_u64()? as usize;
    let mut n = 0;
    for e in &l.events {
        match e.native.as_ref() {
            Some(nv) if nv.format == FORMAT => n += 1,
            _ => return None,
        }
    }
    if n != want {
        return None;
    }
    Some((x.get("file")?.as_str()?.to_string(), x.get("rows")?.as_array()?.clone()))
}

/// Rebuild one store file from its snapshot and the transcript rows.
pub fn rebuild(store: &Value, transcripts: &[Value]) -> Result<Vec<u8>> {
    let dir = tempfile::tempdir()?;
    let path = dir.path().join("openclaw-agent.sqlite");
    {
        let c = Connection::open(&path)?;
        sqlite_header::write(&c, store.get(sqlite_header::KEY))?;
        c.execute_batch("PRAGMA foreign_keys = OFF;")?;
        let objects = store["objects"].as_array().cloned().unwrap_or_default();
        for o in objects.iter().filter(|o| o["type"] == "table") {
            c.execute_batch(o["sql"].as_str().unwrap_or("")).with_context(|| format!("schema {}", o["name"]))?;
        }
        for (t, rs) in store["rows"].as_object().cloned().unwrap_or_default() {
            let cols = columns(&c, &t);
            for r in rs.as_array().into_iter().flatten().filter_map(Value::as_object) {
                insert(&c, &t, r, &cols)?;
            }
        }
        let tcols = columns(&c, "transcript_events");
        for r in transcripts.iter().filter_map(Value::as_object) {
            insert(&c, "transcript_events", r, &tcols)?;
        }
        for o in objects.iter().filter(|o| o["type"] == "table") {
            let (name, sql) = (o["name"].as_str().unwrap_or(""), o["sql"].as_str().unwrap_or(""));
            if !sql.trim_start().to_ascii_uppercase().starts_with("CREATE VIRTUAL TABLE") {
                continue;
            }
            if external_content(sql) {
                c.execute(&format!("INSERT INTO \"{name}\"(\"{name}\") VALUES('rebuild')"), [])?;
            } else if let Some(rs) = store["fts"].get(name).and_then(Value::as_array) {
                for r in rs.iter().filter_map(Value::as_object) {
                    let mut row = r.clone();
                    let rowid = row.remove_ordered("$rowid").unwrap_or(Value::Null);
                    let keys: Vec<String> = row.keys().cloned().collect();
                    let sql = format!(
                        "INSERT INTO \"{name}\" (rowid, {}) VALUES (?, {})",
                        keys.iter().map(|k| format!("\"{k}\"")).collect::<Vec<_>>().join(", "),
                        vec!["?"; keys.len()].join(", ")
                    );
                    let mut vals = vec![crate::formats::hermes::db::to_sql(&rowid)];
                    vals.extend(keys.iter().map(|k| crate::formats::hermes::db::to_sql(&row[k])));
                    c.execute(&sql, rusqlite::params_from_iter(vals.iter()))?;
                }
            }
        }
        if let Some(seq) = store["sqlite_sequence"].as_array().filter(|s| !s.is_empty()) {
            c.execute("DELETE FROM sqlite_sequence", [])?;
            for r in seq {
                c.execute("INSERT INTO sqlite_sequence (name, seq) VALUES (?, ?)", rusqlite::params![r["name"].as_str(), r["seq"].as_i64()])?;
            }
        }
        for o in objects.iter().filter(|o| o["type"] != "table") {
            c.execute_batch(o["sql"].as_str().unwrap_or("")).with_context(|| format!("schema {}", o["name"]))?;
        }
    }
    Ok(std::fs::read(&path)?)
}

/// Export loss: a store file not written because the session that carries
/// its snapshot (its first) is not in the set, or no longer stands for its
/// rows exactly (events added, removed or not OpenClaw's own).
pub const STORE_NOT_REBUILT: &str = "sqlite-store-not-rebuilt";

/// Export every intact OpenClaw-SQLite ledger back into its store file.
/// Returns the ids of the ledgers it wrote. A store whose snapshot rides on a
/// ledger that is not in the set or not intact is not written (its sessions
/// become JSONL), and is named [`STORE_NOT_REBUILT`].
pub fn export(set: &LedgerSet, out: &mut crate::formats::Output, losses: &mut Losses) -> Result<HashSet<String>> {
    #[allow(clippy::type_complexity)] // a one-off tuple of rows, named by the function that builds it
    let mut by_file: HashMap<String, (Option<Value>, Vec<Value>, Vec<String>)> = HashMap::new();
    // Every store file a ledger of the set was read from.
    let mut files = HashSet::new();
    for l in set.iter() {
        let x = l.agent().source.as_ref().filter(|s| s.format == FORMAT).and_then(|s| s.extra.get("sqlite"));
        if let Some(f) = x.and_then(|x| x.get("file")).and_then(Value::as_str) {
            files.insert(f.to_string());
        }
        let Some((file, rs)) = intact_rows(l) else { continue };
        let entry = by_file.entry(file).or_default();
        if let Some(st) = x.and_then(|x| x.get("store")) {
            entry.0 = Some(st.clone());
        }
        entry.1.extend(rs);
        entry.2.push(l.id().to_string());
    }
    let mut done = HashSet::new();
    for (file, (store, rs, ids)) in by_file {
        let Some(store) = store else { continue }; // the store's schema travels with its first session
        out.put(file.clone(), rebuild(&store, &rs)?);
        files.remove(&file);
        done.extend(ids);
    }
    losses.add_n(
        STORE_NOT_REBUILT,
        None,
        files.len() as u64,
        "an OpenClaw SQLite store whose first session, which carries the store, is not exported or has events added, removed or not OpenClaw's own since import: its sessions are written as JSONL and the store file is not written, with its other tables' rows and its header values (PRAGMA application_id, user_version, encoding)",
    );
    Ok(done)
}
