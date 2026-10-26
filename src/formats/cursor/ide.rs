//! Cursor IDE: `<userdata>/User/globalStorage/state.vscdb`, table
//! `cursorDiskKV`: `composerData:<composerId>` (the conversation: name, times,
//! `fullConversationHeadersOnly`, or an inline `conversation[]` before `_v` 3)
//! and `bubbleId:<composerId>:<bubbleId>` (one message: `type` 1 user / 2
//! assistant, `text`, `thinking`, `toolFormerData`, `tokenCount`, ...).
//! Each row the importer reads (every row whose key names an imported
//! composer) is kept as a native record holding its key, SQLite storage class
//! and bytes, so the exporter writes the same row back. The rest of the file is
//! the IDE's own state, not threads: it is counted as skipped, by name
//! (`unlinked-row-skipped`, `ide-state-skipped`), never carried.

use super::{V_IDE, agent, ms_ts, native, output_text, own, push_record};
use crate::carry;
use crate::formats::atif::jsonfmt;
use crate::formats::gemini_cli::{apply_agent, attach_with_agent, carried_agent};
use crate::formats::{ExportOptions, Exported, ImportOptions, Imported, Losses, Output, sqlite_header};
use crate::ledger::{Ledger, LedgerSet};
use crate::model::{Event, Kind, Obj};
use crate::util::{self, get_str};
use anyhow::{Context, Result, bail};
use rusqlite::types::ValueRef;
use serde_json::{Value, json};
use std::path::Path;

/// A `cursorDiskKV` value as SQLite holds it: its storage class and bytes.
/// agentsview's fixture holds BLOBs, cursor-history's TEXT; each comes back as read.
#[derive(Debug, Clone, PartialEq)]
enum Cell {
    Null,
    Integer(i64),
    Real(f64),
    Text(Vec<u8>),
    Blob(Vec<u8>),
}

impl Cell {
    fn of(v: ValueRef<'_>) -> Cell {
        match v {
            ValueRef::Null => Cell::Null,
            ValueRef::Integer(i) => Cell::Integer(i),
            ValueRef::Real(f) => Cell::Real(f),
            ValueRef::Text(t) => Cell::Text(t.to_vec()),
            ValueRef::Blob(b) => Cell::Blob(b.to_vec()),
        }
    }
    fn class(&self) -> &'static str {
        match self {
            Cell::Null => "null",
            Cell::Integer(_) => "integer",
            Cell::Real(_) => "real",
            Cell::Text(_) => "text",
            Cell::Blob(_) => "blob",
        }
    }
    /// The value's text, when it is a TEXT or BLOB of UTF-8.
    fn text(&self) -> Option<&str> {
        match self {
            Cell::Text(b) | Cell::Blob(b) => std::str::from_utf8(b).ok(),
            _ => None,
        }
    }
    fn json(&self) -> Option<Value> {
        serde_json::from_str(self.text()?).ok()
    }
    /// `text` or `blob` (the default) holding `bytes`.
    fn bytes(class: Option<&str>, bytes: Vec<u8>) -> Cell {
        if class == Some("blob") { Cell::Blob(bytes) } else { Cell::Text(bytes) }
    }
    /// The value bound as SQLite holds it: a TEXT keeps its own bytes, even
    /// when they are not UTF-8.
    fn sql(&self) -> rusqlite::types::ToSqlOutput<'_> {
        use rusqlite::types::ToSqlOutput as Out;
        Out::Borrowed(match self {
            Cell::Null => ValueRef::Null,
            Cell::Integer(i) => ValueRef::Integer(*i),
            Cell::Real(f) => ValueRef::Real(*f),
            Cell::Text(b) => ValueRef::Text(b),
            Cell::Blob(b) => ValueRef::Blob(b),
        })
    }
}

/// The native record of one `cursorDiskKV` row (SPEC §4): `{"key", "storage"}`
/// (the SQLite storage class) and the value, as `value` (an object or array
/// value, embedded as its own text), `text` (any other UTF-8 value), `base64`
/// (a BLOB that is not UTF-8), or a number. `raw` is the value's own text when
/// a parsed `Value` would not spell it back.
fn row_record(key: &str, cell: &Cell) -> (String, Option<String>) {
    let head = format!("{{\"key\":{},\"storage\":\"{}\"", json!(key), cell.class());
    let mut raw = None;
    let tail = match cell {
        Cell::Null => String::new(),
        Cell::Integer(i) => format!(",\"value\":{i}"),
        Cell::Real(f) if f.is_finite() => format!(",\"value\":{f}"),
        Cell::Real(f) => format!(",\"text\":{}", json!(f.to_string())),
        Cell::Text(_) | Cell::Blob(_) => match cell.text() {
            Some(t) if t.starts_with(['{', '[']) && t.trim_end() == t && serde_json::from_str::<&serde_json::value::RawValue>(t).is_ok() => {
                if !jsonfmt::value_round_trips(t) {
                    raw = Some(t.to_string());
                }
                format!(",\"value\":{t}")
            }
            Some(t) => format!(",\"text\":{}", json!(t)),
            None => {
                use base64::Engine;
                let (Cell::Text(b) | Cell::Blob(b)) = cell else { unreachable!() };
                format!(",\"base64\":\"{}\"", base64::engine::general_purpose::STANDARD.encode(b))
            }
        },
    };
    (format!("{head}{tail}}}"), raw)
}

#[derive(serde::Deserialize)]
struct RowRec<'a> {
    key: String,
    storage: String,
    #[serde(borrow, default)]
    value: Option<&'a serde_json::value::RawValue>,
    #[serde(default)]
    text: Option<String>,
    #[serde(default)]
    base64: Option<String>,
}

/// The row a native record holds (its key and value), or `None` for a
/// record that is not a row (a ledger written before rows were kept: its
/// record is the bubble's value alone, a TEXT).
fn row_of(n: &crate::model::Native) -> Option<(String, Cell)> {
    let r: RowRec = serde_json::from_str(n.record_text()?).ok()?;
    let class = r.storage.as_str();
    let cell = match (class, r.value, r.text, r.base64) {
        ("null", ..) => Cell::Null,
        ("integer", Some(v), ..) => Cell::Integer(v.get().parse().ok()?),
        ("real", Some(v), ..) => Cell::Real(v.get().parse().ok()?),
        ("real", None, Some(t), _) => Cell::Real(t.parse().ok()?),
        (_, Some(v), ..) => {
            let t = n.raw.as_deref().filter(|raw| crate::util::raw_matches(raw, v.get())).unwrap_or(v.get());
            Cell::bytes(Some(class), t.as_bytes().to_vec())
        }
        (_, None, Some(t), _) => Cell::bytes(Some(class), t.into_bytes()),
        (_, None, None, Some(b)) => {
            use base64::Engine;
            Cell::bytes(Some(class), base64::engine::general_purpose::STANDARD.decode(b).ok()?)
        }
        _ => return None,
    };
    Some((r.key, cell))
}

fn db_files(input: &Path) -> Result<Vec<(String, std::path::PathBuf)>> {
    if input.is_file() {
        return Ok(vec![(input.file_name().unwrap().to_string_lossy().to_string(), input.to_path_buf())]);
    }
    let mut out = Vec::new();
    for e in walkdir::WalkDir::new(input).sort_by_file_name() {
        let e = e?;
        if e.file_type().is_file() && e.file_name().to_string_lossy().ends_with(".vscdb") {
            let rel = e.path().strip_prefix(input)?.to_string_lossy().replace('\\', "/");
            out.push((rel, e.path().to_path_buf()));
        }
    }
    Ok(out)
}

/// The rows of one database, by key, and which of them a ledger has taken.
struct Rows {
    rows: Vec<(String, Cell)>,
    taken: std::collections::HashSet<String>,
}

impl Rows {
    fn get(&self, key: &str) -> Option<&Cell> {
        self.rows.binary_search_by(|(k, _)| k.as_str().cmp(key)).ok().map(|i| &self.rows[i].1)
    }
}

pub(super) fn import(input: &Path, opts: &ImportOptions) -> Result<Imported> {
    let mut set = LedgerSet::new();
    let mut losses = Losses::default();
    for (rel, path) in db_files(input)? {
        let tmp = tempfile::tempdir()?;
        let copy = tmp.path().join("state.vscdb");
        std::fs::copy(&path, &copy)?;
        for side in ["-wal", "-shm"] {
            let p = std::path::PathBuf::from(format!("{}{side}", path.display()));
            if p.is_file() {
                std::fs::copy(&p, tmp.path().join(format!("state.vscdb{side}")))?;
            }
        }
        let c = rusqlite::Connection::open(&copy).with_context(|| path.display().to_string())?;
        let header = sqlite_header::read(&c)?;
        let mut rows = Rows { rows: Vec::new(), taken: Default::default() };
        let mut odd_keys = 0u64;
        {
            let mut st = c.prepare("SELECT key, value FROM cursorDiskKV")?;
            let it = st.query_map([], |r| {
                // A key that is not UTF-8 TEXT (a BLOB, a number, NULL) names
                // no composer: the row is counted as unlinked, never fatal.
                let key = match r.get_ref(0)? {
                    ValueRef::Text(t) => std::str::from_utf8(t).ok().map(str::to_string),
                    _ => None,
                };
                Ok(match key {
                    Some(k) => Some((k, Cell::of(r.get_ref(1)?))),
                    None => None,
                })
            })?;
            for r in it {
                match r? {
                    Some(row) => rows.rows.push(row),
                    None => odd_keys += 1,
                }
            }
        }
        rows.rows.sort_by(|a, b| a.0.cmp(&b.0));
        let composers: Vec<(String, Cell)> = rows.rows.iter().filter(|(k, _)| k.starts_with("composerData:")).cloned().collect();
        for (key, cell) in composers {
            rows.taken.insert(key.clone());
            let cid = key.trim_start_matches("composerData:").to_string();
            if cell == Cell::Null {
                losses.add("null-value-skipped", None, "a composerData row with a NULL value");
                continue;
            }
            let Some((raw, cd)) = cell.text().and_then(|t| Some((t.to_string(), serde_json::from_str::<Value>(t).ok()?))) else {
                losses.add("unparseable-composer-skipped", None, "a composerData row that is not JSON");
                continue;
            };
            let mut l = import_composer(&mut rows, &rel, &cid, &raw, cell.class(), &cd, opts, &mut losses)?;
            if let Some(h) = &header
                && let Some(src) = l.agent_mut().source.as_mut().filter(|s| s.format == super::NAME && s.variant.as_deref() == Some(V_IDE))
            {
                src.extra.insert(sqlite_header::KEY.into(), h.clone());
            }
            set.insert(l);
        }
        let left = rows.rows.iter().filter(|(k, _)| !rows.taken.contains(k)).count();
        losses.add_n("unlinked-row-skipped", None, left as u64 + odd_keys, "a cursorDiskKV row whose key names no imported composer");
        losses.add_n("ide-state-skipped", None, ide_state_rows(&c)?, "a row of the IDE's own state (ItemTable, or any table but cursorDiskKV): not a thread");
    }
    if set.is_empty() {
        bail!("{}: no Cursor composers found", input.display());
    }
    Ok(Imported { set, losses: losses.sorted(), variant: V_IDE.into() })
}

/// The rows of every table but `cursorDiskKV` (and SQLite's own `sqlite_*`
/// bookkeeping): the IDE's UI and settings state (`ItemTable`, ...), which
/// holds no thread and is counted as skipped.
fn ide_state_rows(c: &rusqlite::Connection) -> Result<u64> {
    let tables: Vec<String> = c
        .prepare("SELECT name FROM sqlite_master WHERE type = 'table' AND name <> 'cursorDiskKV' AND name NOT LIKE 'sqlite\\_%' ESCAPE '\\'")?
        .query_map([], |r| r.get(0))?
        .collect::<rusqlite::Result<_>>()?;
    let mut n = 0u64;
    for t in tables {
        let count: i64 = c.query_row(&format!("SELECT COUNT(*) FROM \"{}\"", t.replace('"', "\"\"")), [], |r| r.get(0))?;
        n += count as u64;
    }
    Ok(n)
}

/// The composer a `cursorDiskKV` key names: its second `:`-separated field
/// (`bubbleId:<composer>:<bubble>`, `checkpointId:<composer>:...`). The one
/// test of whether a row belongs to a composer.
fn composer_of(key: &str) -> Option<&str> {
    key.split(':').nth(1)
}

#[allow(clippy::too_many_arguments)]
fn import_composer(rows: &mut Rows, rel: &str, cid: &str, raw: &str, storage: &str, cd: &Value, opts: &ImportOptions, losses: &mut Losses) -> Result<Ledger> {
    let mut extra = Obj::new();
    extra.insert("file".into(), json!(rel));
    extra.insert("composer".into(), json!(raw));
    if storage != "text" {
        extra.insert("composer_storage".into(), json!(storage));
    }
    if let Some(v) = cd.get("_v") {
        extra.insert("_v".into(), v.clone());
    }
    let mut a = agent(cid, V_IDE, Some(cid.to_string()), extra);
    a.created_at = ms_ts(cd.get("createdAt"));
    let mut l = Ledger::new(a);
    // A composer-level carrier (title, header) from an export.
    if opts.restore_carried {
        if let Some(a) = carried_agent(cd) {
            apply_agent(l.agent_mut(), &a);
        }
        if let Some(evs) = cd.get(carry::KEY).and_then(|_| {
            let mut r = cd.clone();
            carry::take(&mut r, &[])
        }) {
            for e in evs {
                l.append_unique(e);
            }
        }
    }
    let mut head = Vec::new();
    if let Some(m) = get_str(cd, &["modelConfig", "modelName"]) {
        head.push(Event::new("", Kind::Settings).with("model", m));
    }
    if let Some(p) = get_str(cd, &["workspaceIdentifier", "uri", "fsPath"]) {
        match head.first_mut() {
            Some(s) => s.body.insert("cwd".into(), json!(p)),
            None => {
                head.push(Event::new("", Kind::Settings).with("cwd", p));
                None
            }
        };
    }
    if let Some(n) = get_str(cd, &["name"]).filter(|n| !n.is_empty()) {
        head.push(Event::new("", Kind::Title).with("text", n));
    }
    if !head.is_empty() && cd.get(carry::KEY).is_none_or(|_| !opts.restore_carried) {
        push_record(&mut l, "composerData", native(V_IDE, "composerData"), head, ms_ts(cd.get("lastUpdatedAt")));
    }
    let mut turn: Option<String> = None;
    let mut model: Option<String> = None;
    let inline: Vec<Value> = cd.get("conversation").and_then(Value::as_array).cloned().unwrap_or_default();
    if !inline.is_empty() {
        // `_v` < 3: bubbles inside the composer row, rendered from it on export.
        for b in &inline {
            let bid = get_str(b, &["bubbleId"]).unwrap_or("").to_string();
            let nid = format!("inline:{bid}");
            let evs = bubble_events(b, &bid, &mut turn, &mut model);
            push_record(&mut l, &nid, native(V_IDE, &nid), evs, None);
        }
    }
    // The bubbles the composer lists, in its order: the row `bubbleId:<composer>:<id>`,
    // else (cursor-history's fallback) this composer's row whose own `bubbleId` is the id.
    let prefix = format!("bubbleId:{cid}:");
    let headers: Vec<Value> = cd.get("fullConversationHeadersOnly").and_then(Value::as_array).cloned().unwrap_or_default();
    for h in headers {
        let Some(bid) = get_str(&h, &["bubbleId"]) else { continue };
        let key = format!("{prefix}{bid}");
        let found = if composer_of(&key) == Some(cid) && rows.get(&key).is_some() && !rows.taken.contains(&key) {
            Some(key)
        } else {
            rows.rows
                .iter()
                .find(|(k, c)| {
                    k.starts_with("bubbleId:")
                        && composer_of(k) == Some(cid)
                        && !rows.taken.contains(k)
                        && c.json().is_some_and(|b| get_str(&b, &["bubbleId"]) == Some(bid))
                })
                .map(|(k, _)| k.clone())
        };
        let Some(key) = found else {
            losses.add("missing-bubble", Some(Kind::Native), "a bubble the composer lists has no row");
            continue;
        };
        rows.taken.insert(key.clone());
        let cell = rows.get(&key).unwrap().clone();
        push_row(&mut l, cid, &key, &cell, true, &mut turn, &mut model, opts);
    }
    // Every other row of this composer (`<kind>:<composer>:...`): kept, with no neutral meaning.
    let others: Vec<(String, Cell)> = rows.rows.iter().filter(|(k, _)| !rows.taken.contains(k) && composer_of(k) == Some(cid)).cloned().collect();
    for (key, cell) in others {
        rows.taken.insert(key.clone());
        push_row(&mut l, cid, &key, &cell, false, &mut turn, &mut model, opts);
    }
    Ok(l)
}

/// One row as a native record. A row the composer lists (`listed`) whose
/// value is a bubble also gives the bubble's events.
#[allow(clippy::too_many_arguments)]
fn push_row(l: &mut Ledger, cid: &str, key: &str, cell: &Cell, listed: bool, turn: &mut Option<String>, model: &mut Option<String>, opts: &ImportOptions) {
    let nid = key.strip_prefix(&format!("bubbleId:{cid}:")).filter(|s| !s.is_empty()).unwrap_or(key);
    let (record, raw) = row_record(key, cell);
    let mut n = native(V_IDE, nid).record_raw(&record).expect("a row record is JSON");
    n.raw = raw;
    let b = cell.json().filter(|b| listed && b.is_object());
    let Some(b) = b else {
        push_record(l, nid, n, vec![], None);
        return;
    };
    if opts.restore_carried && b.get(carry::KEY).is_some() {
        let mut r = b.clone();
        if let Some(evs) = carry::take(&mut r, &[]) {
            for e in evs {
                l.append_unique(e);
            }
            return;
        }
    }
    let evs = bubble_events(&b, nid, turn, model);
    let ts = get_str(&b, &["createdAt"]).map(String::from);
    push_record(l, nid, n, evs, ts);
}

fn json_arg(v: Option<&Value>) -> Option<Value> {
    match v? {
        Value::String(s) if !s.is_empty() => Some(serde_json::from_str(s).unwrap_or(json!({"input": s}))),
        Value::Object(o) => Some(Value::Object(o.clone())),
        _ => None,
    }
}

fn bubble_events(b: &Value, bid: &str, turn: &mut Option<String>, model: &mut Option<String>) -> Vec<Event> {
    let ts = get_str(b, &["createdAt"]).map(String::from).or_else(|| ms_ts(b.get("timingInfo").and_then(|t| t.get("clientStartTime"))));
    let ty = b.get("type").and_then(Value::as_i64).unwrap_or(0);
    if ty == 1 {
        *turn = Some(bid.to_string());
    }
    let group = (ty == 2).then(|| bid.to_string());
    let mk = |k: Kind| Event::new("", k).ts(ts.clone()).turn(turn.clone()).group(group.clone());
    let mut out = Vec::new();
    if let Some(m) = get_str(b, &["modelInfo", "modelName"]).filter(|m| !m.is_empty())
        && model.as_deref() != Some(m)
    {
        out.push(mk(Kind::Settings).with("model", m).group(None::<String>));
        *model = Some(m.to_string());
    }
    if let Some(t) = get_str(b, &["thinking", "text"]).filter(|t| !t.is_empty()) {
        let mut e = mk(Kind::Reasoning).with("text", t);
        if let Some(s) = get_str(b, &["thinking", "signature"]).filter(|s| !s.is_empty()) {
            e = e.with("sealed", json!({"vendor": "cursor", "kind": "signature", "data": s}));
        }
        out.push(e);
    }
    if let Some(t) = get_str(b, &["text"]).filter(|t| !t.is_empty()) {
        out.push(mk(Kind::Text).with("role", if ty == 1 { "user" } else { "assistant" }).with("text", t));
    }
    if let Some(tf) = b.get("toolFormerData").filter(|t| t.is_object()) {
        let call_id = get_str(tf, &["toolCallId"]).unwrap_or(bid).to_string();
        let input = json_arg(tf.get("rawArgs")).or_else(|| json_arg(tf.get("params"))).unwrap_or(json!({}));
        out.push(mk(Kind::ToolCall).with("call_id", call_id.as_str()).with("name", tf.get("name").cloned().unwrap_or(json!(""))).with("input", input));
        let status = get_str(tf, &["status"]).unwrap_or("");
        let result = tf.get("result").filter(|r| !r.is_null());
        if result.is_some() || matches!(status, "completed" | "error" | "cancelled") {
            let output = match result {
                Some(Value::String(s)) => json!(s),
                Some(o) => json!(o.to_string()),
                None => json!(get_str(tf, &["error"]).unwrap_or("")),
            };
            let mut e = mk(Kind::ToolResult).with("call_id", call_id.as_str()).with("output", output).group(None::<String>);
            if matches!(status, "error" | "cancelled") {
                e = e.with("is_error", true);
            }
            if status == "cancelled" {
                e = e.with("status", "cancelled");
            }
            out.push(e);
        }
    }
    if let Some(tc) = b.get("tokenCount") {
        let i = tc.get("inputTokens").and_then(Value::as_i64).unwrap_or(0);
        let o = tc.get("outputTokens").and_then(Value::as_i64).unwrap_or(0);
        if i > 0 || o > 0 {
            out.push(mk(Kind::Usage).with("input", tc["inputTokens"].clone()).with("output", tc["outputTokens"].clone()));
        }
    }
    if let Some(err) = b.get("errorDetails").filter(|e| !e.is_null()) {
        let msg = get_str(err, &["message"]).map(String::from).unwrap_or_else(|| err.to_string());
        out.push(mk(Kind::Error).with("kind", "error").with("message", msg).group(None::<String>));
    }
    out
}

// ---------------------------------------------------------------------------
// export

pub(super) fn export(set: &LedgerSet, opts: &ExportOptions) -> Result<Exported> {
    let mut losses = Losses::default();
    let mut rows: Vec<(String, Cell)> = Vec::new();
    let mut file = "state.vscdb".to_string();
    for l in set.iter() {
        let own_l = super::is_own_ledger(l, V_IDE);
        let extra = l.agent().source.as_ref().filter(|_| own_l).map(|s| s.extra.clone()).unwrap_or_default();
        if let Some(f) = extra.get("file").and_then(Value::as_str) {
            file = f.to_string();
        }
        let cid = l.agent().source.as_ref().filter(|_| own_l).and_then(|s| s.native_id.clone()).unwrap_or_else(|| super::stream::session_id(l));
        let mut own_rows: Vec<(String, Cell)> = Vec::new();
        // Bubbles rendered from foreign events: (bubble id, value), and their headers.
        let mut bubbles: Vec<(String, String)> = Vec::new();
        let mut headers: Vec<Value> = Vec::new();
        let mut foreign: Vec<&Event> = Vec::new();
        let mut leftover: Vec<Event> = Vec::new();
        let mut flush = |foreign: &mut Vec<&Event>, bubbles: &mut Vec<(String, String)>, headers: &mut Vec<Value>, losses: &mut Losses| {
            if foreign.is_empty() {
                return;
            }
            let (rendered, rest) = render(foreign, opts, losses);
            for (bid, ty, v) in rendered {
                headers.push(json!({"bubbleId": bid, "type": ty}));
                bubbles.push((bid, serde_json::to_string(&v).unwrap()));
            }
            leftover.extend(rest);
            foreign.clear();
        };
        let mut inline_own = false;
        for e in &l.events {
            match own(e, V_IDE).filter(|_| own_l) {
                Some(n) => {
                    flush(&mut foreign, &mut bubbles, &mut headers, &mut losses);
                    let id = n.id.clone().unwrap_or_default();
                    if id.starts_with("inline:") {
                        inline_own = true;
                    }
                    if n.part == 0 && id != "composerData" && !id.starts_with("inline:") {
                        // A row as read (key, storage class, bytes); a ledger from
                        // before rows were kept holds the bubble's text alone.
                        match row_of(n) {
                            Some(row) => own_rows.push(row),
                            None => {
                                if let Some(v) = crate::util::native_text(n) {
                                    own_rows.push((format!("bubbleId:{cid}:{id}"), Cell::Text(v.into_bytes())));
                                }
                            }
                        }
                    }
                }
                None => foreign.push(e),
            }
        }
        flush(&mut foreign, &mut bubbles, &mut headers, &mut losses);
        let storage = extra.get("composer_storage").and_then(Value::as_str);
        let composer = match extra.get("composer").and_then(Value::as_str) {
            Some(raw) if bubbles.is_empty() => Cell::bytes(storage, raw.as_bytes().to_vec()),
            Some(raw) => {
                // Foreign bubbles were appended: the composer lists them after its own.
                let mut cd: Value = serde_json::from_str(raw).unwrap_or(json!({}));
                if inline_own {
                    losses.add("inline-conversation-split", None, "appended bubbles are stored as rows next to an inline conversation");
                }
                let mut all = cd.get("fullConversationHeadersOnly").and_then(Value::as_array).cloned().unwrap_or_default();
                all.extend(headers.iter().cloned());
                cd["fullConversationHeadersOnly"] = Value::Array(all);
                Cell::bytes(storage, serde_json::to_string(&cd).unwrap().into_bytes())
            }
            None => Cell::Text(serde_json::to_string(&composer_for(l, &cid, &headers, &leftover, opts)).unwrap().into_bytes()),
        };
        rows.push((format!("composerData:{cid}"), composer));
        rows.extend(own_rows);
        for (bid, v) in bubbles {
            rows.push((format!("bubbleId:{cid}:{bid}"), Cell::Text(v.into_bytes())));
        }
    }
    // Every own ledger names its file's header values; one file is written.
    let headers: Vec<Option<&Value>> =
        set.iter().filter(|l| super::is_own_ledger(l, V_IDE)).filter_map(|l| l.agent().source.as_ref()).map(|s| s.extra.get(sqlite_header::KEY)).collect();
    let header = sqlite_header::pick(headers, &mut losses);
    let tmp = tempfile::tempdir()?;
    let p = tmp.path().join("state.vscdb");
    {
        let c = rusqlite::Connection::open(&p)?;
        sqlite_header::write(&c, header.as_ref())?;
        c.execute_batch(
            "CREATE TABLE ItemTable (key TEXT UNIQUE ON CONFLICT REPLACE, value BLOB);
             CREATE TABLE cursorDiskKV (key TEXT UNIQUE ON CONFLICT REPLACE, value BLOB);",
        )?;
        for (k, v) in &rows {
            c.execute("INSERT INTO cursorDiskKV (key, value) VALUES (?1, ?2)", rusqlite::params![k, v.sql()])?;
        }
    }
    let mut out = Output::default();
    out.put(file, std::fs::read(&p)?);
    Ok(Exported { output: out, losses: losses.sorted(), variant: V_IDE.into() })
}

fn composer_for(l: &Ledger, cid: &str, headers: &[Value], leftover: &[Event], opts: &ExportOptions) -> Value {
    let created =
        l.agent().created_at.as_deref().and_then(util::parse_rfc3339_ms).or_else(|| l.events.iter().find_map(|e| super::ts_ms(e.ts.as_deref()))).unwrap_or(0);
    let updated = l.events.iter().rev().find_map(|e| super::ts_ms(e.ts.as_deref())).unwrap_or(created);
    let title = l.events.iter().rev().find(|e| e.kind == Kind::Title).and_then(|e| e.str("text")).unwrap_or("");
    let mut cd = json!({
        "_v": 3,
        "composerId": cid,
        "name": title,
        "createdAt": created,
        "lastUpdatedAt": updated,
        "fullConversationHeadersOnly": headers,
        "conversationMap": {},
        "status": "completed",
        "isAgentic": true,
    });
    if let Some(m) = l.events.iter().find_map(|e| (e.kind == Kind::Settings).then(|| e.str("model")).flatten()) {
        cd["modelConfig"] = json!({"modelName": m, "maxMode": false});
    }
    if let Some(c) = l.events.iter().find_map(|e| (e.kind == Kind::Settings).then(|| e.str("cwd")).flatten()) {
        cd["workspaceIdentifier"] = json!({"uri": {"fsPath": c}});
    }
    if opts.carry {
        attach_with_agent(&mut cd, leftover, l.agent());
    }
    cd
}

/// Foreign events as bubbles `(bubble id, type, value)`.
/// Also returns the events no bubble could carry (when there are no bubbles).
fn render(events: &[&Event], opts: &ExportOptions, losses: &mut Losses) -> (Vec<(String, i64, Value)>, Vec<Event>) {
    let mut out: Vec<(String, i64, Value, Vec<Event>)> = Vec::new();
    let mut orphan: Vec<Event> = Vec::new();
    let mut calls: std::collections::HashMap<String, usize> = std::collections::HashMap::new();
    let new_bubble = |e: &Event, ty: i64| {
        let bid = util::uuid_for(&e.id);
        let mut b = json!({"_v": 3, "bubbleId": bid, "type": ty, "text": ""});
        if let Some(ts) = &e.ts {
            b["createdAt"] = json!(util::norm_ts(ts));
        }
        (bid, ty, b, Vec::<Event>::new())
    };
    for &e in events {
        let k = e.kind;
        match k {
            Kind::Text if matches!(e.str("role"), Some("user") | Some("assistant") | None) => {
                let ty = if e.str("role") == Some("assistant") { 2 } else { 1 };
                // Assistant text after thinking in the same message joins its bubble.
                let join = ty == 2
                    && matches!(out.last(), Some((_, 2, b, evs)) if b["text"] == "" && b.get("toolFormerData").is_none() && evs.last().is_some_and(|x| x.group.is_some() && x.group == e.group));
                if !join {
                    out.push(new_bubble(e, ty));
                }
                let last = out.last_mut().unwrap();
                last.2["text"] = json!(e.str("text").unwrap_or(""));
                last.3.append(&mut orphan);
                last.3.push(e.clone());
            }
            Kind::Reasoning => {
                if e.body.get("sealed").is_some_and(|s| s.get("vendor").and_then(Value::as_str) != Some("cursor")) {
                    losses.add("reasoning-sealed-dropped", Some(k), "only Cursor's own thinking signatures are kept");
                }
                let mut b = new_bubble(e, 2);
                b.2["thinking"] = json!({"text": e.str("text").unwrap_or("")});
                if let Some(s) = e.body.get("sealed").filter(|s| s.get("vendor").and_then(Value::as_str) == Some("cursor")) {
                    b.2["thinking"]["signature"] = s["data"].clone();
                }
                b.3.append(&mut orphan);
                b.3.push(e.clone());
                out.push(b);
            }
            Kind::ToolCall => {
                let mut b = new_bubble(e, 2);
                let id = e.str("call_id").unwrap_or("").to_string();
                let input = crate::formats::pi_session::tool_args_object(e, losses);
                b.2["capabilityType"] = json!(15);
                b.2["toolFormerData"] = json!({"toolCallId": id, "name": e.str("name").unwrap_or(""), "rawArgs": input.to_string(), "params": input.to_string(), "status": "cancelled"});
                b.3.append(&mut orphan);
                b.3.push(e.clone());
                calls.insert(id, out.len());
                out.push(b);
            }
            Kind::ToolResult => {
                let id = e.str("call_id").unwrap_or("").to_string();
                let (text, media) = output_text(e.body.get("output"));
                if media {
                    losses.add("tool-result-media-dropped", Some(k), "media in tool output is dropped");
                }
                let err = e.body.get("is_error").and_then(Value::as_bool) == Some(true);
                match calls.get(&id) {
                    Some(&i) => {
                        let tf = &mut out[i].2["toolFormerData"];
                        tf["result"] = json!(text);
                        tf["status"] = json!(if e.str("status") == Some("cancelled") {
                            "cancelled"
                        } else if err {
                            "error"
                        } else {
                            "completed"
                        });
                        // Carried by the next bubble, so a carry trip keeps its place.
                        orphan.push(e.clone());
                    }
                    None => {
                        losses.add("tool-result-unpaired", Some(k), "a result whose call is not in the ledger");
                        orphan.push(e.clone());
                    }
                }
            }
            Kind::Usage => match out.iter_mut().rev().find(|b| b.1 == 2) {
                Some(b) if b.2.get("tokenCount").is_none() => {
                    b.2["tokenCount"] = json!({"inputTokens": e.body.get("input").cloned().unwrap_or(json!(0)), "outputTokens": e.body.get("output").cloned().unwrap_or(json!(0))});
                    orphan.push(e.clone());
                }
                _ => {
                    losses.add("usage-dropped", Some(k), "usage with no assistant bubble to hold it");
                    orphan.push(e.clone());
                }
            },
            Kind::Error => {
                let mut b = new_bubble(e, 2);
                b.2["errorDetails"] = json!({"message": e.str("message").unwrap_or("")});
                b.3.append(&mut orphan);
                b.3.push(e.clone());
                out.push(b);
            }
            // Shown by the composer row; carried by the next bubble.
            Kind::Title | Kind::Settings => {
                if k == Kind::Settings && e.body.keys().any(|k| !matches!(k.as_str(), "model" | "cwd" | "complete")) {
                    losses.add("settings-dropped", Some(k), "a composer holds only its model and workspace");
                }
                orphan.push(e.clone());
            }
            other => {
                let code = match (other, e.str("role")) {
                    (Kind::Text, _) => "system-text-dropped".to_string(),
                    _ => format!("{}-dropped", other.as_str().replace('_', "-")),
                };
                losses.add(&code, Some(other), "a composer has no bubble for it");
                orphan.push(e.clone());
            }
        }
    }
    if !orphan.is_empty()
        && let Some(last) = out.last_mut()
    {
        last.3.append(&mut orphan);
    }
    let bubbles = out
        .into_iter()
        .map(|(bid, ty, mut v, evs)| {
            if opts.carry {
                carry::attach(&mut v, &[], &evs);
            }
            (bid, ty, v)
        })
        .collect();
    (bubbles, orphan)
}
