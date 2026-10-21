//! `state.db`: hermes' SQLite session store.
//!
//! Import reads `sessions` and `messages` row by row; each row is kept as a
//! JSON object (`native.record`, column order preserved; REAL values keep a
//! decimal point, BLOBs are `{"$blob": base64}`). The schema text, every row
//! of the other tables and `sqlite_sequence` ride on the first session's
//! ledger, so export rebuilds a row-identical database. FTS tables are
//! rebuilt by hermes' own triggers.

use super::{FORMAT, JSON_PREFIX, json_col, message_events, pyjson, session_events};
use crate::carry;
use crate::formats::openclaw::jsonl;
use crate::formats::{ExportOptions, ImportOptions, Losses, Output};
use crate::ledger::{Ledger, LedgerSet};
use crate::model::{Agent, Audience, Event, Kind, Native, Obj, Relation};
use crate::util;
use crate::util::OrderedMap;
use anyhow::{Context, Result};
use base64::Engine;
use rusqlite::types::Value as Sql;
use rusqlite::{Connection, OpenFlags};
use serde_json::{Map, Value, json};
use std::collections::{HashMap, HashSet};
use std::path::Path;

/// hermes-agent v0.22.x `state.db` schema (schema_version 31), from
/// NousResearch/hermes-agent @ 774a49c5 (MIT, (c) 2025 Nous Research).
const SCHEMA_V31: &str = include_str!("schema_v31.sql");
const KEY: &str = carry::KEY;

/// Schema version of a hermes `state.db`, if it is one.
pub fn probe(p: &Path) -> Option<i64> {
    let c = Connection::open_with_flags(p, OpenFlags::SQLITE_OPEN_READ_ONLY).ok()?;
    let has = |t: &str| c.query_row("SELECT count(*) FROM sqlite_master WHERE type='table' AND name=?", [t], |r| r.get::<_, i64>(0)).ok() == Some(1);
    if !(has("sessions") && has("messages")) {
        return None;
    }
    c.query_row("SELECT max(version) FROM schema_version", [], |r| r.get::<_, i64>(0)).ok().or(Some(0))
}

pub(crate) fn to_json(v: Sql) -> Value {
    match v {
        Sql::Null => Value::Null,
        Sql::Integer(i) => json!(i),
        Sql::Real(f) => serde_json::Number::from_f64(f).map(Value::Number).unwrap_or(Value::Null),
        Sql::Text(s) => Value::String(s),
        Sql::Blob(b) => json!({"$blob": base64::engine::general_purpose::STANDARD.encode(b)}),
    }
}

pub(crate) fn to_sql(v: &Value) -> Sql {
    match v {
        Value::Null => Sql::Null,
        Value::Bool(b) => Sql::Integer(*b as i64),
        Value::Number(n) => {
            let s = n.to_string();
            if s.contains(['.', 'e', 'E']) {
                Sql::Real(n.as_f64().unwrap_or(0.0))
            } else {
                n.as_i64().map(Sql::Integer).unwrap_or_else(|| Sql::Real(n.as_f64().unwrap_or(0.0)))
            }
        }
        Value::String(s) => Sql::Text(s.clone()),
        Value::Object(m) if m.len() == 1 && m.contains_key("$blob") => {
            Sql::Blob(base64::engine::general_purpose::STANDARD.decode(m["$blob"].as_str().unwrap_or("")).unwrap_or_default())
        }
        other => Sql::Text(other.to_string()),
    }
}

pub(crate) fn rows(c: &Connection, sql: &str, params: &[&dyn rusqlite::ToSql]) -> Result<Vec<Map<String, Value>>> {
    crate::formats::sqlite_schema_is_utf8(c)?;
    let mut st = c.prepare(sql)?;
    let cols: Vec<String> = st.column_names().iter().map(|s| s.to_string()).collect();
    let mut q = st.query(params)?;
    let mut out = Vec::new();
    while let Some(r) = q.next()? {
        let mut m = Map::new();
        for (i, name) in cols.iter().enumerate() {
            // Through `get_ref`: rusqlite's own conversion to an owned value
            // panics on TEXT that is not UTF-8 (a damaged database).
            let v = match r.get_ref(i)? {
                rusqlite::types::ValueRef::Text(b) => {
                    Sql::Text(std::str::from_utf8(b).map_err(|e| anyhow::anyhow!("column {name}: text that is not UTF-8 ({e})"))?.to_string())
                }
                other => Sql::from(other),
            };
            m.insert(name.clone(), to_json(v));
        }
        out.push(m);
    }
    Ok(out)
}

/// A table SQLite creates for virtual table `vt` (FTS3/4/5 shadow tables).
pub(crate) fn is_shadow_of(vt: &str, name: &str) -> bool {
    name.strip_prefix(vt)
        .and_then(|r| r.strip_prefix('_'))
        .is_some_and(|suffix| matches!(suffix, "data" | "idx" | "content" | "docsize" | "config" | "segments" | "segdir" | "stat"))
}

/// sqlite_master statements to replay, minus what SQLite creates itself
/// (sqlite_sequence, FTS shadow tables, autoindexes).
pub(crate) fn schema_of(c: &Connection) -> Result<(Vec<String>, Vec<String>)> {
    let master = rows(c, "SELECT type, name, sql FROM sqlite_master WHERE sql IS NOT NULL ORDER BY rowid", &[])?;
    let virtuals: Vec<String> = master
        .iter()
        .filter(|r| r["sql"].as_str().is_some_and(|s| s.trim_start().to_ascii_uppercase().starts_with("CREATE VIRTUAL TABLE")))
        .filter_map(|r| r["name"].as_str().map(str::to_string))
        .collect();
    let mut stmts = Vec::new();
    let mut plain_tables = Vec::new();
    for r in &master {
        let name = r["name"].as_str().unwrap_or("");
        let shadow = r["type"] == "table" && !virtuals.iter().any(|v| v == name) && virtuals.iter().any(|v| is_shadow_of(v, name));
        if name.starts_with("sqlite_") || shadow {
            continue;
        }
        stmts.push(r["sql"].as_str().unwrap_or("").to_string());
        if r["type"] == "table" && !virtuals.iter().any(|v| v == name) {
            plain_tables.push(name.to_string());
        }
    }
    Ok((stmts, plain_tables))
}

/// The columns a carrier hash covers on a message row (defaults for absent ones).
fn msg_subset(row: &Map<String, Value>) -> Value {
    let mut m = Map::new();
    for k in [
        "role",
        "content",
        "tool_call_id",
        "tool_calls",
        "tool_name",
        "timestamp",
        "token_count",
        "finish_reason",
        "reasoning",
        "reasoning_content",
        "reasoning_details",
    ] {
        m.insert(k.into(), row.get(k).cloned().unwrap_or(Value::Null));
    }
    m.insert("active".into(), row.get("active").cloned().filter(|v| !v.is_null()).unwrap_or(json!(1)));
    m.insert("_compressed_summary".into(), row.get("_compressed_summary").cloned().filter(|v| !v.is_null()).unwrap_or(json!(0)));
    Value::Object(m)
}

fn session_subset(row: &Map<String, Value>, model_config: &Value) -> Value {
    let mut m = Map::new();
    for k in ["id", "source", "model", "parent_session_id", "started_at", "title", "cwd", "git_branch"] {
        m.insert(k.into(), row.get(k).cloned().unwrap_or(Value::Null));
    }
    m.insert("model_config".into(), model_config.clone());
    Value::Object(m)
}

/// A JSON-text column's object with the carrier taken out.
fn split_carrier(text: Option<&Value>) -> (Value, Option<Value>) {
    match json_col(text) {
        Some(Value::Object(mut m)) => {
            let c = m.remove_ordered(KEY);
            // A column that held only the carrier was empty before it.
            if m.is_empty() && c.is_some() { (Value::Null, c) } else { (Value::Object(m), c) }
        }
        Some(other) => (other, None),
        None => (Value::Null, None),
    }
}

/// What a carrier over `subject` holds, if `subject` is unchanged since
/// ([`jsonl::carried_over`], which restores carried records' exact text).
fn carried(c: &Value, subject: &Value) -> Option<(Vec<Event>, Option<Agent>)> {
    jsonl::carried_over(subject, c).map(|c| (c.events, c.agent))
}

pub fn import(p: &Path, rel: &str, opts: &ImportOptions, losses: &mut Losses) -> Result<Vec<Ledger>> {
    let c = Connection::open_with_flags(p, OpenFlags::SQLITE_OPEN_READ_ONLY).with_context(|| p.display().to_string())?;
    let version = c.query_row("SELECT max(version) FROM schema_version", [], |r| r.get::<_, i64>(0)).ok();
    let vtag = version.map(|v| format!("schema-v{v}"));
    let (schema, tables) = schema_of(&c)?;
    let mut table_rows = Map::new();
    for t in tables.iter().filter(|t| *t != "sessions" && *t != "messages") {
        let rs = rows(&c, &format!("SELECT * FROM \"{t}\""), &[])?;
        table_rows.insert(t.clone(), Value::Array(rs.into_iter().map(Value::Object).collect()));
    }
    let seq = rows(&c, "SELECT name, seq FROM sqlite_sequence", &[]).unwrap_or_default();
    let prompts: HashMap<String, String> = if tables.iter().any(|t| t == "system_prompts") {
        rows(&c, "SELECT hash, prompt FROM system_prompts", &[])?
            .into_iter()
            .filter_map(|r| Some((r["hash"].as_str()?.to_string(), r["prompt"].as_str()?.to_string())))
            .collect()
    } else {
        HashMap::new()
    };
    let has_order = rows(&c, "PRAGMA table_info(messages)", &[])?.iter().any(|r| r["name"] == "id");
    let mut out = Vec::new();
    for (si, srow) in rows(&c, "SELECT * FROM sessions ORDER BY rowid", &[])?.into_iter().enumerate() {
        let sid = srow.get("id").and_then(Value::as_str).unwrap_or("").to_string();
        let (mc, carrier) = split_carrier(srow.get("model_config"));
        let mut extra = Obj::new();
        extra.insert("file".into(), json!(rel));
        if let Some(p) = srow.get("parent_session_id").filter(|v| !v.is_null()) {
            extra.insert("parent_session_id".into(), p.clone());
        }
        if !mc.is_null() {
            extra.insert("model_config".into(), mc.clone());
        }
        extra.insert("session_source".into(), srow.get("source").cloned().unwrap_or(Value::Null));
        if si == 0 {
            extra.insert("store".into(), json!({"schema": schema, "tables": table_rows, "sqlite_sequence": seq, "schema_version": version}));
        }
        let agent = Agent {
            id: sid.clone(),
            created_at: super::ts_of(srow.get("started_at")),
            source: Some(crate::model::Source {
                format: FORMAT.into(),
                variant: Some("state-db".into()),
                version: vtag.clone(),
                native_id: Some(sid.clone()),
                extra,
            }),
            parent: None,
            links: vec![],
            meta: Obj::new(),
        };
        let mut l = Ledger::new(agent);
        let snative = Native::new(FORMAT, Some(sid.clone()))
            .variant(Some("state-db"))
            .version(vtag.clone())
            .file(Some(format!("{rel}#sessions")))
            .record(Value::Object(srow.clone()));
        let mut tail: Vec<Event> = Vec::new();
        let restored = if opts.restore_carried { carrier.as_ref().and_then(|c| carried(c, &session_subset(&srow, &mc))) } else { None };
        match restored {
            Some((evs, agent)) => {
                if let Some(mut a) = agent {
                    // Keep the store facts this file carries for a later self export.
                    if let (Some(src), Some(st)) = (a.source.as_mut(), l.agent().source.as_ref())
                        && si == 0
                        && src.format == FORMAT
                    {
                        src.extra.insert("store".into(), st.extra["store"].clone());
                    }
                    l.header.agent = a;
                }
                for e in evs {
                    // A renamed native row's own events: its usage goes last, as below.
                    let own_usage = e.kind == Kind::Usage
                        && e.native.as_ref().is_some_and(|n| n.format == FORMAT && n.file.as_deref().is_some_and(|f| f.ends_with("#sessions")));
                    if own_usage {
                        tail.push(e);
                    } else {
                        l.append_unique(e);
                    }
                }
            }
            None => {
                let prompt = srow
                    .get("system_prompt")
                    .and_then(Value::as_str)
                    .map(str::to_string)
                    .or_else(|| srow.get("system_prompt_hash").and_then(Value::as_str).and_then(|h| prompts.get(h).cloned()));
                let mut row_for_events = srow.clone();
                row_for_events.insert("model_config".into(), mc.clone());
                for (i, e) in session_events(&Value::Object(row_for_events), &sid, prompt.as_deref()).into_iter().enumerate() {
                    let e = e.native(if i == 0 { snative.clone() } else { snative.part_of(i as u32) });
                    if e.kind == Kind::Usage {
                        tail.push(e)
                    } else {
                        l.append_unique(e);
                    }
                }
            }
        }
        let order = if has_order { "ORDER BY id" } else { "ORDER BY rowid" };
        let model = srow.get("model").and_then(Value::as_str).map(str::to_string);
        for mrow in rows(&c, &format!("SELECT * FROM messages WHERE session_id = ? {order}"), &[&sid])? {
            let mid = mrow.get("id").map(|v| v.to_string()).unwrap_or_default();
            let (dm, mcarrier) = split_carrier(mrow.get("display_metadata"));
            if opts.restore_carried
                && let Some((evs, _)) = mcarrier.as_ref().and_then(|c| carried(c, &msg_subset(&mrow)))
            {
                for e in evs {
                    l.append_unique(e);
                }
                continue;
            }
            let _ = dm;
            let n = Native::new(FORMAT, Some(mid.clone()))
                .variant(Some("state-db"))
                .version(vtag.clone())
                .file(Some(format!("{rel}#messages")))
                .record(Value::Object(mrow.clone()));
            for (i, e) in message_events(&Value::Object(mrow.clone()), &mid, model.as_deref()).into_iter().enumerate() {
                l.append_unique(e.native(if i == 0 { n.clone() } else { n.part_of(i as u32) }));
            }
        }
        for e in tail {
            l.append_unique(e);
        }
        out.push(l);
    }
    let _ = losses;
    Ok(out)
}

// ---------------------------------------------------------------- export

struct Target {
    file: String,
    schema: Vec<String>,
    tables: Map<String, Value>,
    seq: Vec<Value>,
    version: Option<i64>,
}

fn target(set: &LedgerSet) -> Target {
    for l in set.iter() {
        let Some(src) = l.agent().source.as_ref().filter(|s| s.format == FORMAT) else { continue };
        if let Some(st) = src.extra.get("store") {
            return Target {
                file: src.extra.get("file").and_then(Value::as_str).unwrap_or("state.db").to_string(),
                schema: st["schema"].as_array().map(|a| a.iter().filter_map(|s| s.as_str().map(str::to_string)).collect()).unwrap_or_default(),
                tables: st["tables"].as_object().cloned().unwrap_or_default(),
                seq: st["sqlite_sequence"].as_array().cloned().unwrap_or_default(),
                version: st["schema_version"].as_i64(),
            };
        }
    }
    Target { file: "state.db".into(), schema: vec![], tables: Map::new(), seq: vec![], version: Some(31) }
}

pub(crate) fn insert(c: &Connection, table: &str, row: &Map<String, Value>, cols: &HashSet<String>) -> Result<()> {
    let keys: Vec<&String> = row.keys().filter(|k| cols.contains(*k)).collect();
    let sql = format!(
        "INSERT INTO \"{table}\" ({}) VALUES ({})",
        keys.iter().map(|k| format!("\"{k}\"")).collect::<Vec<_>>().join(", "),
        vec!["?"; keys.len()].join(", ")
    );
    let vals: Vec<Sql> = keys.iter().map(|k| to_sql(&row[*k])).collect();
    c.execute(&sql, rusqlite::params_from_iter(vals.iter())).with_context(|| format!("insert into {table}"))?;
    Ok(())
}

pub(crate) fn columns(c: &Connection, table: &str) -> HashSet<String> {
    rows(c, &format!("PRAGMA table_info(\"{table}\")"), &[]).unwrap_or_default().into_iter().filter_map(|r| r["name"].as_str().map(str::to_string)).collect()
}

pub fn export(set: &LedgerSet, all: &LedgerSet, opts: &ExportOptions, out: &mut Output, losses: &mut Losses) -> Result<()> {
    let t = target(set);
    let dir = tempfile::tempdir()?;
    let path = dir.path().join("state.db");
    {
        let c = Connection::open(&path)?;
        // Sessions are written in set order; a child may precede its parent.
        c.execute_batch("PRAGMA foreign_keys = OFF;")?;
        if t.schema.is_empty() {
            // The fresh-database dump, minus objects SQLite makes itself.
            let mut stmts = Vec::new();
            for s in split_sql(SCHEMA_V31) {
                let up = s.to_ascii_uppercase();
                if up.contains("SQLITE_SEQUENCE") && up.starts_with("CREATE TABLE") || up.starts_with("CREATE TABLE IF NOT EXISTS 'MESSAGES_FTS") {
                    continue;
                }
                stmts.push(s);
            }
            for s in &stmts {
                c.execute_batch(s).with_context(|| format!("schema: {}", &s[..s.len().min(80)]))?;
            }
            c.execute("INSERT INTO schema_version (version) VALUES (?)", [t.version.unwrap_or(31)])?;
        } else {
            for s in &t.schema {
                c.execute_batch(s).with_context(|| format!("schema: {}", &s[..s.len().min(80)]))?;
            }
        }
        for (table, rs) in &t.tables {
            let cols = columns(&c, table);
            for r in rs.as_array().into_iter().flatten() {
                if let Some(m) = r.as_object() {
                    insert(&c, table, m, &cols)?;
                }
            }
        }
        let scols = columns(&c, "sessions");
        let mcols = columns(&c, "messages");
        let has_prompts = !columns(&c, "system_prompts").is_empty();
        // Message ids: native rows keep theirs, rendered rows follow the largest.
        let mut next_id = set
            .iter()
            .flat_map(|l| l.events.iter())
            .filter_map(|e| e.native.as_ref().filter(|n| n.format == FORMAT && n.file.as_deref().is_some_and(|f| f.ends_with("#messages"))))
            .filter_map(|n| n.record_json().as_ref()?.get("id")?.as_i64())
            .max()
            .unwrap_or(0)
            + 1;
        let sids: HashMap<String, String> = all.iter().map(|l| (l.id().to_string(), session_id(l))).collect();
        let mut msg_rows: Vec<(i64, Map<String, Value>)> = Vec::new();
        let mut seq_max = 0i64;
        for l in set.iter() {
            let (srow, mrows) = render_ledger(l, all, &sids, &scols, &mcols, has_prompts, &mut next_id, opts.carry, losses, &c)?;
            insert(&c, "sessions", &srow, &scols)?;
            for (id, r) in mrows {
                seq_max = seq_max.max(id);
                msg_rows.push((id, r));
            }
        }
        msg_rows.sort_by_key(|(id, _)| *id);
        for (_, r) in &msg_rows {
            insert(&c, "messages", r, &mcols)?;
        }
        // sqlite_sequence as it was, never below what was inserted.
        if !t.seq.is_empty() || seq_max > 0 {
            c.execute("DELETE FROM sqlite_sequence", [])?;
            let mut seen = false;
            for r in &t.seq {
                let name = r["name"].as_str().unwrap_or("");
                let mut s = r["seq"].as_i64().unwrap_or(0);
                if name == "messages" {
                    s = s.max(seq_max);
                    seen = true;
                }
                c.execute("INSERT INTO sqlite_sequence (name, seq) VALUES (?, ?)", rusqlite::params![name, s])?;
            }
            if !seen && seq_max > 0 {
                c.execute("INSERT INTO sqlite_sequence (name, seq) VALUES ('messages', ?)", [seq_max])?;
            }
        }
    }
    out.put(t.file, std::fs::read(&path)?);
    Ok(())
}

/// Split a schema dump into statements, keeping trigger bodies whole.
fn split_sql(s: &str) -> Vec<String> {
    let mut out = Vec::new();
    let mut cur = String::new();
    let mut in_trigger = false;
    for line in s.lines() {
        if cur.is_empty() && line.trim().is_empty() {
            continue;
        }
        cur.push_str(line);
        cur.push('\n');
        let up = line.trim_start().to_ascii_uppercase();
        if cur.trim_start().to_ascii_uppercase().starts_with("CREATE TRIGGER") {
            in_trigger = true;
        }
        let done = if in_trigger { up.starts_with("END;") || up == "END;" } else { line.trim_end().ends_with(';') };
        if done {
            out.push(std::mem::take(&mut cur));
            in_trigger = false;
        }
    }
    if !cur.trim().is_empty() {
        out.push(cur);
    }
    out
}

/// The hermes session id of a ledger: its own, or `YYYYMMDD_HHMMSS_<hex6>`.
fn session_id(l: &Ledger) -> String {
    if let Some(id) = l.agent().source.as_ref().filter(|s| s.format == FORMAT).and_then(|s| s.native_id.clone()) {
        return id;
    }
    let ms = l.agent().created_at.as_deref().or_else(|| l.events.iter().find_map(|e| e.ts.as_deref())).and_then(util::parse_rfc3339_ms).unwrap_or(0);
    let t = util::ms_to_rfc3339(ms);
    format!("{}{}{}_{}{}{}_{}", &t[0..4], &t[5..7], &t[8..10], &t[11..13], &t[14..16], &t[17..19], util::short_id(l.id(), 6))
}

fn secs(ts: Option<&str>) -> f64 {
    ts.and_then(util::parse_rfc3339_ms).map(|ms| ms as f64 / 1000.0).unwrap_or(0.0)
}

#[allow(clippy::too_many_arguments, clippy::type_complexity)] // the session row and its message rows, as built
fn render_ledger(
    l: &Ledger,
    all: &LedgerSet,
    sids: &HashMap<String, String>,
    scols: &HashSet<String>,
    mcols: &HashSet<String>,
    has_prompts: bool,
    next_id: &mut i64,
    carry: bool,
    losses: &mut Losses,
    c: &Connection,
) -> Result<(Map<String, Value>, Vec<(i64, Map<String, Value>)>)> {
    let sid = sids[l.id()].clone();
    let counts = crate::formats::openclaw::jsonl::part_counts(l, FORMAT);
    // Parts per native id across the whole ledger (session rows split their parts).
    let mut by_id: HashMap<String, usize> = HashMap::new();
    for e in &l.events {
        if let Some(n) = e.native.as_ref().filter(|n| n.format == FORMAT) {
            *by_id.entry(n.id.clone().unwrap_or_default()).or_default() += 1;
        }
    }
    let _ = counts;
    let mut native_session: Option<Map<String, Value>> = None;
    // SPEC §2.3: hermes cannot say where a rewound conversation continued
    // from, so the active path is written and an abandoned event rides on the
    // next row.
    let on = crate::formats::active_path_only(l, losses, "with carry they ride on the next row", |e| e.native.as_ref().is_some_and(|n| n.format == FORMAT));
    let mut b = Rows::new(l, &sid, mcols, losses);
    for (e, on) in l.events.iter().zip(on) {
        if !on && e.native.as_ref().is_none_or(|n| n.format != FORMAT) {
            b.pending.push(e.clone());
            continue;
        }
        if let Some(n) = e.native.as_ref().filter(|n| n.format == FORMAT) {
            let id = n.id.clone().unwrap_or_default();
            let file = n.file.as_deref().unwrap_or("");
            if file.ends_with("#sessions") {
                if n.part == 0 {
                    if let Some(Value::Object(r)) = &n.record_json() {
                        let (mc, _) = split_carrier(r.get("model_config"));
                        let mut rr = r.clone();
                        rr.insert("model_config".into(), mc);
                        let want = session_events(&Value::Object(rr), &id, None).len();
                        // system_prompt does not change the part count.
                        if by_id.get(&id).copied() == Some(want) {
                            native_session = Some(r.clone());
                            continue;
                        }
                    }
                } else if native_session.is_some() {
                    continue;
                }
            } else if file.ends_with("#messages") {
                if n.part == 0 {
                    if let Some(Value::Object(r)) = &n.record_json() {
                        let want = message_events(&Value::Object(r.clone()), &id, None).len();
                        if by_id.get(&id).copied() == Some(want) {
                            b.native_row(r.clone());
                            b.done.insert(id);
                            continue;
                        }
                    }
                } else if b.done.contains(&id) {
                    continue;
                }
            }
        }
        b.push(e);
    }
    b.finish();
    // Rendered rows get ids now, in order.
    let mut mrows: Vec<(i64, Map<String, Value>)> = Vec::new();
    for (row, evs, native) in std::mem::take(&mut b.rows) {
        if native {
            let id = row.get("id").and_then(Value::as_i64).unwrap_or(0);
            mrows.push((id, row));
            continue;
        }
        let mut row = row;
        let id = *next_id;
        *next_id += 1;
        row.insert("id".into(), json!(id));
        if mcols.contains("message_uid") {
            let anchor = evs.first().map(|e| e.id.clone()).unwrap_or_default();
            row.insert("message_uid".into(), json!(util::short_id(&format!("{}\u{0}{anchor}", l.id()), 32)));
        }
        if carry && !evs.is_empty() && mcols.contains("display_metadata") {
            let dm = json!({KEY: jsonl::carrier_over(&msg_subset(&row), &evs, None)});
            row.insert("display_metadata".into(), json!(pyjson::dumps(&dm, pyjson::Opts::DEFAULT)));
        } else if !evs.is_empty() && carry {
            b.losses.add("carry-unavailable", None, "this schema has no display_metadata column to carry events");
        }
        mrows.push((id, row));
    }
    let srow = match native_session {
        Some(mut r) => {
            if let Some(t) = r.get("title").and_then(Value::as_str).map(str::to_string)
                && let Some(n) = unique_title(c, &t, b.losses)?
            {
                r.insert("title".into(), json!(n));
                if carry {
                    carry_native_session(l, &mut r, scols, b.losses);
                }
            }
            r
        }
        None => {
            let mut s = Map::new();
            s.insert("id".into(), json!(sid));
            let parent = l.agent().parent.as_ref();
            let continued = l.agent().links.iter().find(|k| k.rel == "continued-from").and_then(|k| k.agent.clone());
            let mut mc = Map::new();
            let source = match parent.map(|p| p.relation) {
                Some(Relation::Subagent) => "subagent",
                _ => "cli",
            };
            s.insert("source".into(), json!(source));
            if let Some(m) = &b.model {
                s.insert("model".into(), json!(m));
            }
            if let Some(p) = b.provider.as_ref().filter(|_| scols.contains("billing_provider")) {
                s.insert("billing_provider".into(), json!(p));
            }
            if let Some(e) = &b.effort {
                mc.insert("reasoning_effort".into(), json!(e));
            }
            let psid = parent.map(|p| sids.get(&p.agent).cloned().unwrap_or(p.agent.clone())).or(continued.map(|c| sids.get(&c).cloned().unwrap_or(c)));
            if let (Some(p), Some(ps)) = (parent, &psid) {
                match p.relation {
                    Relation::Subagent => mc.insert("_delegate_from".into(), json!(ps)),
                    Relation::Fork => mc.insert("_branched_from".into(), json!(ps)),
                };
            }
            if let Some(ps) = &psid {
                s.insert("parent_session_id".into(), json!(ps));
            }
            if let Some(prompt) = &b.system_prompt {
                if has_prompts && scols.contains("system_prompt_hash") {
                    use sha2::{Digest, Sha256};
                    let h = hex::encode(Sha256::digest(prompt.as_bytes()));
                    c.execute("INSERT OR IGNORE INTO system_prompts (hash, prompt) VALUES (?, ?)", rusqlite::params![h, prompt])?;
                    s.insert("system_prompt_hash".into(), json!(h));
                } else {
                    s.insert("system_prompt".into(), json!(prompt));
                }
            }
            let started = l.agent().created_at.clone().or_else(|| l.events.iter().find_map(|e| e.ts.clone()));
            s.insert("started_at".into(), json!(secs(started.as_deref())));
            s.insert("message_count".into(), json!(mrows.len()));
            s.insert("tool_call_count".into(), json!(b.tool_calls));
            let u = b.usage;
            for (i, k) in ["input_tokens", "output_tokens", "cache_read_tokens", "cache_write_tokens", "reasoning_tokens"].iter().enumerate() {
                s.insert((*k).into(), json!(u[i]));
            }
            if let Some(cost) = b.cost {
                s.insert("estimated_cost_usd".into(), json!(cost));
            }
            if let Some(t) = b.title.clone()
                && let Some(n) = unique_title(c, &t, b.losses)?
            {
                b.title = Some(n);
            }
            for (k, v) in [("cwd", &b.cwd), ("git_branch", &b.git_branch), ("title", &b.title)] {
                if let Some(v) = v {
                    s.insert(k.into(), json!(v));
                }
            }
            let mc_value = Value::Object(mc.clone());
            if carry {
                let subject = session_subset(&s, &if mc.is_empty() { Value::Null } else { mc_value.clone() });
                if let Some(c) = jsonl::carrier_over(&subject, &b.lead, Some(l.agent())) {
                    mc.insert(KEY.into(), c);
                }
            } else if !b.lead.is_empty() {
                // nothing: the lead events are simply not carried
            }
            if !mc.is_empty() {
                s.insert("model_config".into(), json!(pyjson::dumps(&Value::Object(mc), pyjson::Opts::DEFAULT)));
            }
            s
        }
    };
    let _ = all;
    Ok((srow, mrows))
}

/// `sessions.title` is UNIQUE. A title already held by a session written
/// before this one is renamed the way hermes renames a taken title
/// (`hermes_state_titles.next_title_in_lineage`, used by its auto-titler and
/// `hermes sessions retitle`): strip a ` #N` suffix, then `<base> #<max+1>`,
/// the unnumbered original counting as #1. `None` when the title is free.
fn unique_title(c: &Connection, title: &str, losses: &mut Losses) -> Result<Option<String>> {
    let taken: bool = c.query_row("SELECT EXISTS (SELECT 1 FROM sessions WHERE title = ?)", [title], |r| r.get(0))?;
    if !taken {
        return Ok(None);
    }
    let base = numbered(title).map_or(title, |(b, _)| b);
    let like = format!("{} #%", base.replace('\\', "\\\\").replace('%', "\\%").replace('_', "\\_"));
    let mut st = c.prepare("SELECT title FROM sessions WHERE title = ? OR title LIKE ? ESCAPE '\\'")?;
    let mut max = String::from("1");
    let mut any = false;
    for t in st.query_map(rusqlite::params![base, like], |r| r.get::<_, String>(0))? {
        any = true;
        let t = t?;
        if let Some((_, n)) = numbered(&t)
            && (n.len(), n) > (max.len(), max.as_str())
        {
            max = n.to_string();
        }
    }
    let next = if any { format!("{base} #{}", increment(&max)) } else { base.to_string() };
    losses.add(
        "title-renamed",
        Some(Kind::Title),
        "sessions.title is unique: a title another exported session holds is numbered `<title> #N`, as hermes numbers a taken title",
    );
    Ok(Some(next))
}

/// A native session row renamed by [`unique_title`] no longer says what its
/// events say: carry those events over the renamed row (in `model_config`, as
/// a rendered row carries its lead), so a carried trip restores the title. A
/// row that is not renamed is written as it was read and gets no carrier.
fn carry_native_session(l: &Ledger, r: &mut Map<String, Value>, scols: &HashSet<String>, losses: &mut Losses) {
    let id = r.get("id").and_then(Value::as_str).map(str::to_string);
    let (mc, _) = split_carrier(r.get("model_config"));
    let mut m = match &mc {
        Value::Object(m) => m.clone(),
        Value::Null => Map::new(),
        _ => return losses.add("carry-unavailable", Some(Kind::Title), "a model_config that is not an object cannot hold the carrier of a renamed title"),
    };
    if !scols.contains("model_config") {
        return losses.add("carry-unavailable", Some(Kind::Title), "this schema has no model_config column to carry a renamed title");
    }
    let evs: Vec<Event> = l
        .events
        .iter()
        .filter(|e| e.native.as_ref().is_some_and(|n| n.format == FORMAT && n.id == id && n.file.as_deref().is_some_and(|f| f.ends_with("#sessions"))))
        .cloned()
        .collect();
    if let Some(cv) = jsonl::carrier_over(&session_subset(r, &mc), &evs, None) {
        m.insert(KEY.into(), cv);
        r.insert("model_config".into(), json!(pyjson::dumps(&Value::Object(m), pyjson::Opts::DEFAULT)));
    }
}

/// `^(.*?) #(\d+)$`: the base and the number of a numbered title, the
/// number as its decimal digits without leading zeros. Hermes's numbers are
/// unbounded integers, so they are compared and incremented as digit strings
/// (longer is larger, then by digit): no suffix overflows or is skipped.
fn numbered(t: &str) -> Option<(&str, &str)> {
    let i = t.rfind(" #")?;
    let d = &t[i + 2..];
    if d.is_empty() || !d.bytes().all(|b| b.is_ascii_digit()) {
        return None;
    }
    let n = d.trim_start_matches('0');
    Some((&t[..i], if n.is_empty() { "0" } else { n }))
}

/// `n + 1` for a decimal digit string without leading zeros.
fn increment(n: &str) -> String {
    let mut d = n.as_bytes().to_vec();
    for b in d.iter_mut().rev() {
        if *b == b'9' {
            *b = b'0';
        } else {
            *b += 1;
            return String::from_utf8(d).expect("ascii digits");
        }
    }
    format!("1{}", String::from_utf8(d).expect("ascii digits"))
}

/// Builds message rows for one ledger.
struct Rows<'a> {
    l: &'a Ledger,
    sid: String,
    mcols: &'a HashSet<String>,
    /// (row, events it stands for, is a native row)
    rows: Vec<(Map<String, Value>, Vec<Event>, bool)>,
    done: HashSet<String>,
    open_asst: Option<(usize, Option<String>)>,
    open_user: Option<(usize, Option<String>)>,
    pending: Vec<Event>,
    /// events before the first rendered row: carried on the session row.
    lead: Vec<Event>,
    model: Option<String>,
    /// SPEC §3.1.3: the provider a settings event names (`billing_provider`).
    provider: Option<String>,
    effort: Option<String>,
    cwd: Option<String>,
    git_branch: Option<String>,
    title: Option<String>,
    system_prompt: Option<String>,
    usage: [u64; 5],
    cost: Option<f64>,
    tool_calls: usize,
    call_names: HashMap<String, String>,
    losses: &'a mut Losses,
}

impl<'a> Rows<'a> {
    fn new(l: &'a Ledger, sid: &str, mcols: &'a HashSet<String>, losses: &'a mut Losses) -> Self {
        Rows {
            l,
            sid: sid.to_string(),
            mcols,
            rows: vec![],
            done: HashSet::new(),
            open_asst: None,
            open_user: None,
            pending: vec![],
            lead: vec![],
            model: None,
            provider: None,
            effort: None,
            cwd: None,
            git_branch: None,
            title: None,
            system_prompt: None,
            usage: [0; 5],
            cost: None,
            tool_calls: 0,
            call_names: HashMap::new(),
            losses,
        }
    }

    fn flush(&mut self) {
        self.open_asst = None;
        self.open_user = None;
    }

    fn native_row(&mut self, r: Map<String, Value>) {
        self.flush();
        let p = std::mem::take(&mut self.pending);
        if self.rows.is_empty() {
            self.lead.extend(p);
        } else {
            // A native row cannot carry: keep them on the previous rendered row.
            match self.rows.iter_mut().rev().find(|x| !x.2) {
                Some(prev) => prev.1.extend(p),
                None => self.lead.extend(p),
            }
        }
        self.rows.push((r, vec![], true));
    }

    fn ts(&self, e: &Event) -> f64 {
        let t = e.ts.clone().or_else(|| {
            let pos = self.l.events.iter().position(|x| x.id == e.id).unwrap_or(0);
            self.l.events[..pos].iter().rev().find_map(|x| x.ts.clone())
        });
        secs(t.as_deref().or(self.l.agent().created_at.as_deref()))
    }

    fn row(&mut self, e: &Event, role: &str) -> usize {
        self.flush();
        let mut r = Map::new();
        r.insert("session_id".into(), json!(self.sid));
        r.insert("role".into(), json!(role));
        r.insert("timestamp".into(), json!(self.ts(e)));
        let mut evs = std::mem::take(&mut self.pending);
        if self.rows.is_empty() {
            self.lead.append(&mut evs);
        }
        evs.push(e.clone());
        self.rows.push((r, evs, false));
        self.rows.len() - 1
    }

    fn add(&mut self, idx: usize, e: &Event) {
        let p = std::mem::take(&mut self.pending);
        self.rows[idx].1.extend(p);
        self.rows[idx].1.push(e.clone());
    }

    fn defer(&mut self, e: &Event, code: &str, detail: &str) {
        self.losses.add(code, Some(e.kind), detail);
        self.pending.push(e.clone());
    }

    fn assistant(&mut self, e: &Event) -> usize {
        if let Some((i, g)) = &self.open_asst
            && *g == e.group
        {
            let i = *i;
            self.add(i, e);
            return i;
        }
        let i = self.row(e, "assistant");
        self.open_asst = Some((i, e.group.clone()));
        i
    }

    fn set(&mut self, idx: usize, k: &str, v: Value) {
        if self.mcols.contains(k) {
            self.rows[idx].0.insert(k.into(), v);
        }
    }

    fn user_part(&mut self, e: &Event, part: Value) {
        let idx = match &self.open_user {
            Some((i, g)) if g.is_some() && *g == e.group => {
                let i = *i;
                self.add(i, e);
                i
            }
            _ => {
                let i = self.row(e, "user");
                self.open_user = Some((i, e.group.clone()));
                self.rows[i].0.insert("_parts".into(), json!([]));
                i
            }
        };
        self.rows[idx].0.get_mut("_parts").unwrap().as_array_mut().unwrap().push(part);
    }

    fn push(&mut self, e: &Event) {
        let b = &e.body;
        let s = |k: &str| b.get(k).and_then(Value::as_str).map(str::to_string);
        match e.kind {
            Kind::Text => match s("role").as_deref() {
                Some("assistant") => {
                    let i = self.assistant(e);
                    let t = s("text").unwrap_or_default();
                    let prev = self.rows[i].0.get("content").and_then(Value::as_str).map(str::to_string);
                    let joined = match prev {
                        Some(p) => {
                            self.losses.add("assistant-text-joined", Some(Kind::Text), "several assistant text parts become one content string");
                            format!("{p}\n\n{t}")
                        }
                        None => t,
                    };
                    self.rows[i].0.insert("content".into(), json!(joined));
                    self.rows[i].0.insert("finish_reason".into(), json!("stop"));
                }
                Some("system") | Some("developer") => {
                    if self.system_prompt.is_none() && self.rows.is_empty() {
                        self.system_prompt = s("text");
                        self.pending.push(e.clone());
                    } else {
                        self.defer(e, "system-text-dropped", "hermes keeps one system prompt per session, not system messages");
                    }
                }
                _ => self.user_part(e, json!({"type":"text","text": s("text").unwrap_or_default()})),
            },
            Kind::Media => match (s("role").as_deref(), s("data"), s("media_type"), s("url")) {
                (Some("assistant"), ..) => self.defer(e, "media-dropped", "hermes assistant rows hold no images"),
                (_, Some(d), Some(mt), _) => self.user_part(e, json!({"type":"image_url","image_url":{"url": format!("data:{mt};base64,{d}")}})),
                (_, _, _, Some(u)) => self.user_part(e, json!({"type":"image_url","image_url":{"url": u}})),
                _ => self.defer(e, "media-dropped", "media without data or url"),
            },
            Kind::Reasoning => {
                let i = self.assistant(e);
                let summary: Vec<&str> = b.get("summary").and_then(Value::as_array).map(|a| a.iter().filter_map(Value::as_str).collect()).unwrap_or_default();
                let details_col = self.mcols.contains("reasoning_details");
                // SPEC §3.1.5: `text` is the reasoning; summaries go in
                // `reasoning_details` as `reasoning.summary` items (folded
                // into the text only where that column does not exist).
                let text = match s("text") {
                    Some(t) => Some(t),
                    None if !details_col || summary.is_empty() => Some(summary.join("\n")),
                    None => None,
                };
                if let Some(t) = &text {
                    self.set(i, "reasoning", json!(t));
                    self.set(i, "reasoning_content", json!(t));
                }
                let mut details: Vec<Value> = Vec::new();
                let mut vendor = json!("unknown");
                if let Some(sealed) = b.get("sealed") {
                    vendor = json!(super::format_of_vendor(sealed.get("vendor").and_then(Value::as_str).unwrap_or("unknown")));
                    details.push(if sealed.get("kind").and_then(Value::as_str) == Some("signature") {
                        json!({"type":"reasoning.text","text": text.clone().unwrap_or_default(),"signature": sealed.get("data"),"format": vendor,"index":0})
                    } else {
                        json!({"type":"reasoning.encrypted","data": sealed.get("data"),"format": vendor,"index":0})
                    });
                }
                if details_col {
                    for s in &summary {
                        details.push(json!({"type":"reasoning.summary","summary": s,"format": vendor,"index": details.len()}));
                    }
                }
                if !details.is_empty() {
                    if details_col {
                        self.set(i, "reasoning_details", json!(pyjson::dumps(&Value::Array(details), pyjson::Opts::DEFAULT)));
                    } else {
                        self.losses.add("reasoning-sealed-dropped", Some(Kind::Reasoning), "this schema has no reasoning_details column");
                    }
                }
            }
            Kind::ToolCall => {
                let i = self.assistant(e);
                let call_id = s("call_id").unwrap_or_else(|| e.id.clone());
                let name = s("name").unwrap_or_default();
                self.call_names.insert(call_id.clone(), name.clone());
                let args = match (b.get("input"), s("input_text")) {
                    (_, Some(t)) => t,
                    (Some(v), None) if !v.is_null() => pyjson::dumps(v, pyjson::Opts::DEFAULT),
                    _ => "{}".into(),
                };
                let call = json!({"id": call_id,"type":"function","function":{"name": name,"arguments": args}});
                let mut list = self.rows[i].0.get("_calls").cloned().unwrap_or(json!([]));
                list.as_array_mut().unwrap().push(call);
                self.rows[i].0.insert("_calls".into(), list);
                self.rows[i].0.insert("finish_reason".into(), json!("tool_calls"));
                self.tool_calls += 1;
            }
            Kind::ToolResult => {
                let i = self.row(e, "tool");
                let call = s("call_id").unwrap_or_default();
                if let Some(Value::Array(parts)) = b.get("output")
                    && parts.iter().any(|p| p.get("type").and_then(Value::as_str) != Some("text"))
                {
                    self.losses.add("media-dropped", Some(Kind::ToolResult), "tool rows hold text only");
                }
                let text = crate::formats::openclaw::jsonl::output_text(b.get("output"));
                let name = self.call_names.get(&call).cloned();
                self.rows[i].0.insert("content".into(), json!(text));
                self.rows[i].0.insert("tool_call_id".into(), json!(call));
                if let Some(n) = name {
                    self.rows[i].0.insert("tool_name".into(), json!(n));
                }
            }
            Kind::Compaction if b.get("summary").is_some() => {
                let i = self.row(e, "user");
                self.rows[i].0.insert("content".into(), json!(s("summary").unwrap_or_default()));
                self.set(i, "_compressed_summary", json!(1));
            }
            Kind::Compaction => self.defer(e, "compaction-boundary-dropped", "a boundary without a summary has no hermes row"),
            Kind::Settings => {
                for (k, slot) in [("model", 0), ("effort", 1), ("cwd", 2), ("git_branch", 3), ("system_prompt", 4), ("provider", 5)] {
                    if let Some(v) = s(k) {
                        let target = match slot {
                            0 => &mut self.model,
                            1 => &mut self.effort,
                            2 => &mut self.cwd,
                            3 => &mut self.git_branch,
                            5 => &mut self.provider,
                            _ => &mut self.system_prompt,
                        };
                        if target.is_none() {
                            *target = Some(v);
                        }
                    }
                }
                self.pending.push(e.clone());
            }
            Kind::Title => {
                if self.title.is_none() {
                    self.title = s("text");
                }
                self.pending.push(e.clone());
            }
            Kind::Usage => {
                let n = |k: &str| b.get(k).and_then(Value::as_u64).unwrap_or(0);
                let v = [n("input"), n("output"), n("cache_read"), n("cache_write"), n("reasoning")];
                if b.get("cumulative").and_then(Value::as_bool) == Some(true) {
                    self.usage = v;
                    self.cost = b.get("cost_usd").and_then(Value::as_f64).or(self.cost);
                } else {
                    for (u, x) in self.usage.iter_mut().zip(v) {
                        *u += x;
                    }
                    if let Some(c) = b.get("cost_usd").and_then(Value::as_f64) {
                        self.cost = Some(self.cost.unwrap_or(0.0) + c);
                    }
                    if let Some(i) = self.rows.iter().rposition(|r| !r.2 && r.0.get("role") == Some(&json!("assistant")))
                        && !self.rows[i].0.contains_key("token_count")
                        && v[1] > 0
                    {
                        self.rows[i].0.insert("token_count".into(), json!(v[1]));
                        self.rows[i].1.push(e.clone());
                        return;
                    }
                }
                self.pending.push(e.clone());
            }
            Kind::PeerMessage => {
                // SPEC §3.1.6: wrapped text, which the importer unwraps.
                if crate::formats::pi_session::peer_has_more(e) {
                    self.losses.add(
                        crate::formats::pi_session::PEER_FIELDS_DROPPED,
                        Some(Kind::PeerMessage),
                        crate::formats::pi_session::PEER_FIELDS_DROPPED_DETAIL,
                    );
                }
                let (role, text) = crate::formats::pi_session::peer_as_text(e);
                let i = self.row(e, role);
                self.rows[i].0.insert("content".into(), json!(text));
            }
            Kind::Turn => self.pending.push(e.clone()),
            Kind::Subagent => self.defer(e, "subagent-marker-dropped", "hermes links subagents by parent_session_id only"),
            Kind::Error => self.defer(e, "error-dropped", "hermes has no error rows"),
            Kind::FileChange => self.defer(e, "file-change-dropped", "hermes has no file-change rows"),
            Kind::App => self.defer(e, "app-dropped", "application records have no hermes row"),
            Kind::Native => self.defer(e, "foreign-native-dropped", "another harness's native record"),
        }
        if e.effective_audience() == Audience::Record
            && matches!(e.kind, Kind::Text | Kind::ToolCall | Kind::ToolResult | Kind::Reasoning)
            && let Some((r, _, false)) = self.rows.last_mut()
            && self.mcols.contains("active")
        {
            r.insert("active".into(), json!(0));
        }
    }

    fn finish(&mut self) {
        let p = std::mem::take(&mut self.pending);
        if !p.is_empty() {
            match self.rows.iter_mut().rev().find(|x| !x.2) {
                Some(r) => r.1.extend(p),
                None => self.lead.extend(p),
            }
        }
        // Materialise user parts and tool-call lists into hermes columns.
        for (r, _, native) in self.rows.iter_mut() {
            if *native {
                continue;
            }
            if let Some(Value::Array(parts)) = r.remove_ordered("_parts") {
                let content = if parts.len() == 1 && parts[0]["type"] == "text" {
                    parts[0]["text"].clone()
                } else {
                    json!(format!("{JSON_PREFIX}{}", pyjson::dumps(&Value::Array(parts), pyjson::Opts::DEFAULT)))
                };
                r.insert("content".into(), content);
            }
            if let Some(calls) = r.remove_ordered("_calls") {
                r.insert("tool_calls".into(), json!(pyjson::dumps(&calls, pyjson::Opts::DEFAULT)));
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn sql_json_round_trip() {
        for v in [Sql::Null, Sql::Integer(3), Sql::Real(1792152001.0), Sql::Real(0.25), Sql::Text("x".into()), Sql::Blob(vec![0, 1, 255])] {
            assert_eq!(to_sql(&to_json(v.clone())), v);
        }
    }

    #[test]
    fn schema_splits_into_statements() {
        let st = split_sql(SCHEMA_V31);
        assert!(st.iter().any(|s| s.starts_with("CREATE TRIGGER messages_fts_insert")));
        assert!(st.iter().all(|s| s.trim_end().ends_with(';')));
    }
}
