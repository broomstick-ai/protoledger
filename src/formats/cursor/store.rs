//! `cursor-agent` chat store: `~/.cursor/chats/<md5(cwd)>/<agentId>/store.db`.
//!
//! Two tables: `meta(key, value)` whose key `"0"` holds hex-encoded JSON
//! `{agentId, latestRootBlobId, name, mode, createdAt, lastUsedModel?}`, and
//! `blobs(id, data)`, a content-addressed DAG (`id` = sha256(data)). The root
//! blob is protobuf: field 1 repeats 32-byte refs to the conversation's
//! message blobs, which are AI SDK messages as compact JSON; field 2 refs a
//! small metadata subtree (title, preview). Newer CLIs add a field-8 turn index
//! of protobuf turns, read here only when field 1 is absent.

use super::{V_STORE, agent, ai_sdk_events, ms_ts, native, output_text, own, push_record};
use crate::formats::atif::jsonfmt;
use crate::formats::{ExportOptions, Exported, ImportOptions, Imported, Losses, Output};
use crate::ledger::{Ledger, LedgerSet};
use crate::model::{Event, Kind, Obj};
use crate::util::{self, get_str};
use anyhow::{Context, Result, anyhow, bail};
use base64::Engine;
use serde_json::{Value, json};
use sha2::{Digest, Sha256};
use std::collections::BTreeMap;
use std::path::{Path, PathBuf};

const B64: base64::engine::GeneralPurpose = base64::engine::general_purpose::STANDARD;
/// Files of a chat directory kept verbatim for an exact self trip.
const CHAT_FILES: [&str; 4] = ["store.db", "store.db-wal", "store.db-shm", "meta.json"];

// ---- protobuf, just enough ----

#[derive(Debug, Clone)]
pub(super) enum Pb<'a> {
    Varint(u64),
    Bytes(&'a [u8]),
    /// 64- and 32-bit fixed fields; skipped, never interpreted.
    #[allow(dead_code)]
    Fixed(u64),
}

fn varint(b: &[u8], i: &mut usize) -> Option<u64> {
    let mut v = 0u64;
    let mut shift = 0;
    loop {
        let x = *b.get(*i)?;
        *i += 1;
        v |= ((x & 0x7f) as u64) << shift;
        if x & 0x80 == 0 {
            return Some(v);
        }
        shift += 7;
        if shift > 63 {
            return None;
        }
    }
}

pub(super) fn pb_parse(b: &[u8]) -> Option<Vec<(u64, Pb<'_>)>> {
    let mut out = Vec::new();
    let mut i = 0;
    while i < b.len() {
        let tag = varint(b, &mut i)?;
        let (field, wire) = (tag >> 3, tag & 7);
        let v = match wire {
            0 => Pb::Varint(varint(b, &mut i)?),
            1 => {
                let x = b.get(i..i + 8)?;
                i += 8;
                Pb::Fixed(u64::from_le_bytes(x.try_into().ok()?))
            }
            2 => {
                let n = varint(b, &mut i)? as usize;
                let x = b.get(i..i + n)?;
                i += n;
                Pb::Bytes(x)
            }
            5 => {
                let x = b.get(i..i + 4)?;
                i += 4;
                Pb::Fixed(u32::from_le_bytes(x.try_into().ok()?) as u64)
            }
            _ => return None,
        };
        out.push((field, v));
    }
    Some(out)
}

fn pb_bytes(field: u64, payload: &[u8]) -> Vec<u8> {
    let mut out = Vec::new();
    let mut tag = (field << 3) | 2;
    loop {
        let byte = (tag & 0x7f) as u8;
        tag >>= 7;
        if tag == 0 {
            out.push(byte);
            break;
        }
        out.push(byte | 0x80);
    }
    let mut n = payload.len() as u64;
    loop {
        let byte = (n & 0x7f) as u8;
        n >>= 7;
        if n == 0 {
            out.push(byte);
            break;
        }
        out.push(byte | 0x80);
    }
    out.extend_from_slice(payload);
    out
}

fn blob_id(data: &[u8]) -> String {
    hex::encode(Sha256::digest(data))
}

// ---- import ----

/// Chat directories (relative to `input`, "" for `input` itself) holding a store.db.
fn chat_dirs(input: &Path) -> Result<Vec<(String, PathBuf)>> {
    if input.is_file() {
        return Ok(vec![(String::new(), input.parent().unwrap_or(Path::new(".")).to_path_buf())]);
    }
    let mut out = Vec::new();
    for e in walkdir::WalkDir::new(input).sort_by_file_name() {
        let e = e?;
        if e.file_type().is_file() && e.file_name() == "store.db" {
            let dir = e.path().parent().unwrap().to_path_buf();
            let rel = dir.strip_prefix(input)?.to_string_lossy().replace('\\', "/");
            out.push((rel, dir));
        }
    }
    Ok(out)
}

pub(super) fn import(input: &Path, _opts: &ImportOptions) -> Result<Imported> {
    let mut set = LedgerSet::new();
    let mut losses = Losses::default();
    let single_file = input.is_file();
    for (rel, dir) in chat_dirs(input)? {
        let l = import_chat(&rel, &dir, single_file, &mut losses).with_context(|| dir.display().to_string())?;
        set.insert(l);
    }
    if set.is_empty() {
        bail!("{}: no cursor-agent store.db found", input.display());
    }
    Ok(Imported { set, losses: losses.sorted(), variant: V_STORE.into() })
}

fn import_chat(rel: &str, dir: &Path, single_file: bool, losses: &mut Losses) -> Result<Ledger> {
    // Open a copy, so a WAL is replayed without touching the source.
    let tmp = tempfile::tempdir()?;
    let mut kept = Obj::new();
    for f in CHAT_FILES {
        let p = dir.join(f);
        if single_file && f != "store.db" {
            continue;
        }
        if p.is_file() {
            let b = std::fs::read(&p)?;
            if f != "meta.json" {
                std::fs::write(tmp.path().join(f), &b)?;
            }
            kept.insert(f.into(), json!(B64.encode(&b)));
        }
    }
    let c = rusqlite::Connection::open(tmp.path().join("store.db"))?;
    let meta_hex: String = c.query_row("SELECT value FROM meta WHERE key = '0'", [], |r| r.get(0)).context("meta row 0")?;
    let meta_bytes = hex::decode(meta_hex.trim()).map_err(|e| anyhow!("meta is not hex: {e}"))?;
    let meta: Value = serde_json::from_slice(&meta_bytes).context("meta is not JSON")?;
    let mut blobs: BTreeMap<String, Vec<u8>> = BTreeMap::new();
    {
        let mut st = c.prepare("SELECT id, data FROM blobs ORDER BY rowid")?;
        let rows = st.query_map([], |r| Ok((r.get::<_, String>(0)?, r.get::<_, Vec<u8>>(1)?)))?;
        for r in rows {
            let (id, data) = r?;
            blobs.insert(id, data);
        }
    }
    let chat_json: Option<Value> = std::fs::read(dir.join("meta.json")).ok().and_then(|b| serde_json::from_slice(&b).ok());
    let agent_id = get_str(&meta, &["agentId"]).map(String::from).unwrap_or_else(|| dir.file_name().unwrap().to_string_lossy().to_string());
    let mut extra = Obj::new();
    extra.insert("dir".into(), json!(rel));
    extra.insert("meta".into(), meta.clone());
    extra.insert("files".into(), Value::Object(kept));
    let mut a = agent(&agent_id, V_STORE, Some(agent_id.clone()), extra);
    a.created_at = ms_ts(meta.get("createdAt")).or_else(|| chat_json.as_ref().and_then(|j| ms_ts(j.get("createdAtMs"))));
    let mut l = Ledger::new(a);

    // Settings and title from the store metadata.
    let mut head = Vec::new();
    let mut s = Event::new("", Kind::Settings);
    if let Some(m) = meta.get("lastUsedModel").filter(|m| m.is_string()) {
        s = s.with("model", m.clone());
    }
    if let Some(cwd) = chat_json.as_ref().and_then(|j| j.get("cwd")).filter(|c| c.is_string()) {
        s = s.with("cwd", cwd.clone());
    }
    if !s.body.is_empty() {
        head.push(s);
    }
    if let Some(n) = get_str(&meta, &["name"]).filter(|n| !n.is_empty()) {
        head.push(Event::new("", Kind::Title).with("text", n));
    }
    if !head.is_empty() {
        push_record(&mut l, "@meta", native(V_STORE, "@meta"), head, None);
    }

    let root_id = get_str(&meta, &["latestRootBlobId"]).unwrap_or("");
    let root = blobs.get(root_id).ok_or_else(|| anyhow!("root blob {root_id} is missing"))?;
    let fields = pb_parse(root).ok_or_else(|| anyhow!("root blob is not protobuf"))?;
    let refs: Vec<String> = fields
        .iter()
        .filter_map(|(f, v)| match (f, v) {
            (1, Pb::Bytes(b)) if b.len() == 32 => Some(hex::encode(b)),
            _ => None,
        })
        .collect();
    let mut turn: Option<String> = None;
    if !refs.is_empty() {
        for id in refs {
            let Some(data) = blobs.get(&id) else {
                losses.add("missing-blob", Some(Kind::Native), "a message the root names is not in the store");
                continue;
            };
            let text = String::from_utf8_lossy(data).to_string();
            let Ok(m) = serde_json::from_str::<Value>(&text) else {
                push_record(&mut l, &id, native(V_STORE, &id).record(json!({"hex": hex::encode(data)})), vec![], None);
                continue;
            };
            if get_str(&m, &["role"]) == Some("user") {
                turn = Some(id.clone());
            }
            let group = (get_str(&m, &["role"]) == Some("assistant")).then(|| id.clone());
            let evs = ai_sdk_events(&m, None, group, turn.clone());
            // The blob is held as its own text (SPEC §4).
            let mut n = native(V_STORE, &id);
            jsonfmt::hold(&mut n, Some(&text), &m);
            push_record(&mut l, &id, n, evs, None);
        }
    } else if let Some(Pb::Bytes(ti)) = fields.iter().find(|(f, _)| *f == 8).map(|(_, v)| v.clone()) {
        losses.add("turn-index-provisional", None, "a field-8 turn index was read with provisional field numbers (text only)");
        import_turn_index(&mut l, ti, &blobs);
    }
    Ok(l)
}

/// Resolve a field that is either a 32-byte blob ref or an inline message.
fn resolve<'a>(b: &'a [u8], blobs: &'a BTreeMap<String, Vec<u8>>) -> (&'a [u8], Option<String>) {
    if b.len() == 32 {
        let id = hex::encode(b);
        if let Some(d) = blobs.get(&id) {
            return (d, Some(id));
        }
    }
    (b, None)
}

fn pb_str<'a>(fields: &'a [(u64, Pb<'a>)], n: u64) -> Option<&'a str> {
    fields.iter().find_map(|(f, v)| match (f, v) {
        (x, Pb::Bytes(b)) if *x == n => std::str::from_utf8(b).ok(),
        _ => None,
    })
}

fn pb_var(fields: &[(u64, Pb<'_>)], n: u64) -> Option<u64> {
    fields.iter().find_map(|(f, v)| match (f, v) {
        (x, Pb::Varint(v)) if *x == n => Some(*v),
        _ => None,
    })
}

fn pb_msg<'a>(fields: &'a [(u64, Pb<'a>)], n: u64) -> Option<Vec<(u64, Pb<'a>)>> {
    fields.iter().find_map(|(f, v)| match (f, v) {
        (x, Pb::Bytes(b)) if *x == n => pb_parse(b),
        _ => None,
    })
}

/// Field-8 turns (field numbers from agentsview's capture, provisional):
/// turn f1 = user message (f1 text, f25/f26 ms), f2 = reasoning (f3{f1 text,
/// f4/f3 ms}) or assistant (f1{f1 text, f2 ms}).
fn import_turn_index(l: &mut Ledger, ti: &[u8], blobs: &BTreeMap<String, Vec<u8>>) {
    let (ti, _) = resolve(ti, blobs);
    let Some(index) = pb_parse(ti) else { return };
    for (f, v) in index {
        let (1, Pb::Bytes(tb)) = (f, v) else { continue };
        let (tb, tid) = resolve(tb, blobs);
        let Some(turn) = pb_parse(tb) else { continue };
        let tid = tid.unwrap_or_else(|| util::short_id(&hex::encode(tb), 16));
        let turn_id = Some(tid.clone());
        for (k, (tf, tv)) in turn.iter().enumerate() {
            let Pb::Bytes(mb) = tv else { continue };
            let (mb, mid) = resolve(mb, blobs);
            let Some(m) = pb_parse(mb) else { continue };
            let nid = mid.clone().unwrap_or_else(|| format!("{tid}#{k}"));
            let rec = json!({"hex": hex::encode(mb)});
            let mut evs = Vec::new();
            match tf {
                1 => {
                    if let Some(t) = pb_str(&m, 1) {
                        let ts = pb_var(&m, 25).or(pb_var(&m, 26)).and_then(|ms| ms_ts(Some(&json!(ms))));
                        evs.push(Event::new("", Kind::Text).with("role", "user").with("text", t).ts(ts).turn(turn_id.clone()));
                    }
                }
                2 => {
                    if let Some(r) = pb_msg(&m, 3) {
                        let ts = pb_var(&r, 4).or(pb_var(&r, 3)).and_then(|ms| ms_ts(Some(&json!(ms))));
                        if let Some(t) = pb_str(&r, 1) {
                            evs.push(Event::new("", Kind::Reasoning).with("text", t).ts(ts).turn(turn_id.clone()));
                        }
                    } else if let Some(a) = pb_msg(&m, 1) {
                        let ts = pb_var(&a, 2).and_then(|ms| ms_ts(Some(&json!(ms))));
                        if let Some(t) = pb_str(&a, 1) {
                            evs.push(
                                Event::new("", Kind::Text).with("role", "assistant").with("text", t).ts(ts).turn(turn_id.clone()).group(Some(nid.clone())),
                            );
                        }
                    }
                }
                _ => {}
            }
            if !evs.is_empty() {
                push_record(l, &nid, native(V_STORE, &nid).record(rec), evs, None);
            }
        }
    }
}

// ---- export ----

pub(super) fn export(set: &LedgerSet, _opts: &ExportOptions) -> Result<Exported> {
    let mut out = Output::default();
    let mut losses = Losses::default();
    for l in set.iter() {
        let own_l = super::is_own_ledger(l, V_STORE);
        let extra = l.agent().source.as_ref().filter(|_| own_l).map(|s| s.extra.clone()).unwrap_or_default();
        let unchanged = own_l && l.events.iter().all(|e| own(e, V_STORE).is_some());
        let dir = extra.get("dir").and_then(Value::as_str).map(String::from).unwrap_or_else(|| crate::ledger::file_safe(&agent_id(l)));
        let at = |f: &str| if dir.is_empty() { f.to_string() } else { format!("{dir}/{f}") };
        if unchanged && let Some(files) = extra.get("files").and_then(Value::as_object).filter(|f| f.contains_key("store.db")) {
            for (name, b) in files {
                out.put(at(name), B64.decode(b.as_str().unwrap_or("")).context("kept file")?);
            }
            continue;
        }
        if own_l {
            losses.add("store-rebuilt", None, "the store was rebuilt: orphan blobs, the metadata subtree and newer root fields are not kept");
        }
        let (db, meta_json) = build(l, &extra, &mut losses)?;
        out.put(at("store.db"), db);
        out.put(at("meta.json"), meta_json);
    }
    Ok(Exported { output: out, losses: losses.sorted(), variant: V_STORE.into() })
}

fn agent_id(l: &Ledger) -> String {
    super::stream::session_id(l)
}

/// AI SDK messages for a ledger: own message blobs as they were, foreign
/// events rendered.
fn messages(l: &Ledger, losses: &mut Losses) -> Vec<Vec<u8>> {
    let mut out: Vec<Vec<u8>> = Vec::new();
    let mut cur: Option<(String, Value)> = None; // (role, message) being built
    let flush = |cur: &mut Option<(String, Value)>, out: &mut Vec<Vec<u8>>| {
        if let Some((_, m)) = cur.take() {
            out.push(serde_json::to_vec(&m).unwrap());
        }
    };
    let mut names: BTreeMap<String, String> = BTreeMap::new();
    let mut asst_n = 0u32;
    for e in &l.events {
        if let Some(n) = own(e, V_STORE) {
            if n.part == 0 && n.id.as_deref() != Some("@meta") {
                flush(&mut cur, &mut out);
                match (crate::util::current_raw(n), &n.record_json()) {
                    (Some(r), _) => out.push(r.as_bytes().to_vec()),
                    (None, Some(r)) if r.get("hex").is_some() && r.get("role").is_none() => {
                        losses.add("binary-message-dropped", Some(Kind::Native), "protobuf turn blobs are not rewritten");
                    }
                    // The record's own text, never the record re-serialised.
                    (None, Some(_)) => out.push(n.record_text().unwrap_or_default().as_bytes().to_vec()),
                    _ => {}
                }
            }
            continue;
        }
        let k = e.kind;
        let part = match k {
            Kind::Text => match e.str("role") {
                Some("assistant") => Some(("assistant", json!({"type": "text", "text": e.str("text").unwrap_or("")}))),
                Some("user") | None => Some(("user", json!({"type": "text", "text": e.str("text").unwrap_or("")}))),
                _ => {
                    losses.add("system-text-dropped", Some(k), "a resumable store holds no system message");
                    None
                }
            },
            Kind::Media => match (e.str("data"), e.str("role")) {
                (Some(d), Some("user") | None) => Some(("user", json!({"type": "image", "image": d, "mimeType": e.str("media_type").unwrap_or("image/png")}))),
                _ => {
                    losses.add("media-dropped", Some(k), "only inline user images are kept");
                    None
                }
            },
            Kind::Reasoning => {
                let mut p = json!({"type": "reasoning", "text": e.str("text").unwrap_or("")});
                match e.body.get("sealed") {
                    Some(s) if s.get("vendor").and_then(Value::as_str) == Some("cursor") && s.get("kind").and_then(Value::as_str) == Some("signature") => {
                        p["signature"] = s["data"].clone();
                    }
                    Some(_) => losses.add("reasoning-sealed-dropped", Some(k), "only Cursor's own reasoning signatures are kept"),
                    None => {}
                }
                Some(("assistant", p))
            }
            Kind::ToolCall => {
                let id = e.str("call_id").unwrap_or("").to_string();
                let name = e.str("name").unwrap_or("").to_string();
                names.insert(id.clone(), name.clone());
                let args = crate::formats::pi_session::tool_args_object(e, losses);
                Some((
                    "assistant",
                    json!({"type": "tool-call", "toolCallId": id, "toolName": name, "args": args, "providerOptions": {"cursor": {"rawToolCallArgs": args.to_string()}}}),
                ))
            }
            Kind::ToolResult => {
                let id = e.str("call_id").unwrap_or("").to_string();
                let (text, media) = output_text(e.body.get("output"));
                if media {
                    losses.add("tool-result-media-dropped", Some(k), "media in tool output is dropped");
                }
                let mut p = json!({"type": "tool-result", "toolName": names.get(&id).cloned().unwrap_or_default(), "toolCallId": id, "result": text});
                if e.body.get("is_error").and_then(Value::as_bool) == Some(true) {
                    p["isError"] = json!(true);
                }
                flush(&mut cur, &mut out);
                out.push(serde_json::to_vec(&json!({"role": "tool", "id": id, "content": [p]})).unwrap());
                None
            }
            Kind::Compaction if e.str("summary").is_some() => {
                losses.add("compaction-as-user-text", Some(k), "a compaction summary becomes a user message");
                Some(("user", json!({"type": "text", "text": e.str("summary").unwrap_or("")})))
            }
            Kind::Title | Kind::Settings => None,
            other => {
                losses.add(
                    &format!("{}-dropped", other.as_str().replace('_', "-")),
                    Some(other),
                    "a store message holds only text, reasoning, images and tool calls/results",
                );
                None
            }
        };
        let Some((role, p)) = part else { continue };
        let same = matches!(&cur, Some((r, m)) if r == role && (role != "assistant" || m.get("group") .is_none()));
        let group_break = role == "assistant"
            && matches!(&cur, Some((_, m)) if m["content"].as_array().is_some_and(|a| a.iter().any(|x| x["type"] == "tool-call")) && p["type"] != "tool-call");
        if !same || group_break {
            flush(&mut cur, &mut out);
            let m = if role == "assistant" {
                asst_n += 1;
                json!({"id": "1", "role": "assistant", "content": []})
            } else {
                json!({"role": role, "content": []})
            };
            cur = Some((role.to_string(), m));
        }
        cur.as_mut().unwrap().1["content"].as_array_mut().unwrap().push(p);
    }
    flush(&mut cur, &mut out);
    let _ = asst_n;
    out
}

fn build(l: &Ledger, extra: &Obj, losses: &mut Losses) -> Result<(Vec<u8>, Vec<u8>)> {
    let msgs = messages(l, losses);
    let mut blobs: BTreeMap<String, Vec<u8>> = BTreeMap::new();
    let mut put = |b: Vec<u8>| {
        let id = blob_id(&b);
        blobs.insert(id.clone(), b);
        id
    };
    let ids: Vec<String> = msgs.into_iter().map(&mut put).collect();
    let title = l.events.iter().rev().find(|e| e.kind == Kind::Title).and_then(|e| e.str("text").map(String::from)).unwrap_or_default();
    let last_text = l
        .events
        .iter()
        .rev()
        .find(|e| e.kind == Kind::Text && e.str("role") == Some("assistant"))
        .and_then(|e| e.str("text").map(String::from))
        .unwrap_or_default();
    let aid = extra.get("meta").and_then(|m| get_str(m, &["agentId"])).map(String::from).unwrap_or_else(|| agent_id(l));
    // The metadata subtree specstory's writer (validated with `cursor-agent --resume`) uses.
    let title_id = put([pb_bytes(1, title.as_bytes()), pb_bytes(2, util::uuid_for(&format!("{aid}#title")).as_bytes())].concat());
    let empty_id = put(pb_bytes(3, b""));
    let preview_id = put(pb_bytes(1, &pb_bytes(1, last_text.as_bytes())));
    let r = |id: &str| hex::decode(id).unwrap();
    let meta_node = put([pb_bytes(1, &r(&title_id)), pb_bytes(2, &r(&empty_id)), pb_bytes(2, &r(&preview_id))].concat());
    let mut root = Vec::new();
    for id in &ids {
        root.extend(pb_bytes(1, &r(id)));
    }
    root.extend(pb_bytes(2, &r(&meta_node)));
    let root_id = put(root);
    let created =
        l.agent().created_at.as_deref().and_then(util::parse_rfc3339_ms).or_else(|| l.events.iter().find_map(|e| super::ts_ms(e.ts.as_deref()))).unwrap_or(0);
    let updated = l.events.iter().rev().find_map(|e| super::ts_ms(e.ts.as_deref())).unwrap_or(created);
    let mode = extra.get("meta").and_then(|m| m.get("mode")).cloned().unwrap_or(json!("default"));
    let mut meta = json!({"agentId": aid, "latestRootBlobId": root_id, "name": title, "mode": mode, "createdAt": created});
    if let Some(m) = l.events.iter().rev().find_map(|e| (e.kind == Kind::Settings).then(|| e.str("model")).flatten()) {
        meta["lastUsedModel"] = json!(m);
    }
    let tmp = tempfile::tempdir()?;
    let p = tmp.path().join("store.db");
    {
        let c = rusqlite::Connection::open(&p)?;
        c.execute_batch("CREATE TABLE blobs (id TEXT PRIMARY KEY, data BLOB); CREATE TABLE meta (key TEXT PRIMARY KEY, value TEXT);")?;
        for (id, data) in &blobs {
            c.execute("INSERT INTO blobs (id, data) VALUES (?1, ?2)", rusqlite::params![id, data])?;
        }
        c.execute("INSERT INTO meta (key, value) VALUES ('0', ?1)", [hex::encode(serde_json::to_vec(&meta)?)])?;
    }
    let db = std::fs::read(&p)?;
    let cwd = l.events.iter().rev().find_map(|e| (e.kind == Kind::Settings).then(|| e.str("cwd")).flatten()).map(String::from);
    let mut mj = json!({"schemaVersion": 1, "createdAtMs": created, "hasConversation": !ids.is_empty(), "updatedAtMs": updated});
    if let Some(c) = cwd {
        mj["cwd"] = json!(c);
    }
    Ok((db, serde_json::to_vec(&mj)?))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn protobuf_round_trip() {
        let a = pb_bytes(1, &[7u8; 32]);
        let b = pb_bytes(2, b"hi");
        let all = [a, b].concat();
        let f = pb_parse(&all).unwrap();
        assert_eq!(f.len(), 2);
        assert!(matches!(f[1], (2, Pb::Bytes(b"hi"))));
        let long = pb_bytes(1, &vec![1u8; 300]);
        assert_eq!(&long[..3], &[0x0a, 0xac, 0x02]);
    }

    /// A message blob spelled the way a parsed `Value` does not re-spell it
    /// under any feature set (whitespace, the escape `\u00e9`, `1e3`,
    /// `0.000005`) is held as its own text, and a rebuilt store writes it back
    /// as the same bytes (so under the same blob id).
    #[test]
    fn message_blobs_are_held_and_rewritten_as_spelled() {
        let odd = "{\"role\": \"user\",\"content\":[{\"type\":\"text\",\"text\":\"caf\\u00e9\"}],\"n\":1e3,\"x\":0.000005}";
        let t = tempfile::tempdir().unwrap();
        let root = pb_bytes(1, &hex::decode(blob_id(odd.as_bytes())).unwrap());
        let meta = json!({"agentId": "a1", "latestRootBlobId": blob_id(&root), "name": "", "createdAt": 0});
        {
            let c = rusqlite::Connection::open(t.path().join("store.db")).unwrap();
            c.execute_batch("CREATE TABLE blobs (id TEXT PRIMARY KEY, data BLOB); CREATE TABLE meta (key TEXT PRIMARY KEY, value TEXT);").unwrap();
            for b in [odd.as_bytes().to_vec(), root.clone()] {
                c.execute("INSERT INTO blobs (id, data) VALUES (?1, ?2)", rusqlite::params![blob_id(&b), b]).unwrap();
            }
            c.execute("INSERT INTO meta (key, value) VALUES ('0', ?1)", [hex::encode(serde_json::to_vec(&meta).unwrap())]).unwrap();
        }
        let mut set = import(t.path(), &ImportOptions::standard()).unwrap().set;
        let l = set.iter().next().unwrap();
        let held: Vec<&str> = l.events.iter().filter_map(|e| own(e, V_STORE)).filter(|n| n.part == 0).filter_map(|n| n.record_text()).collect();
        assert_eq!(held, [odd]);
        // A foreign event makes the exporter rebuild the store from its records:
        // once as imported, and once with no `raw` beside the record (a ledger
        // written by a build whose parsed values reproduce this text keeps
        // none), where only the record's own text can give the bytes back.
        let mut l = l.clone();
        l.append(Event::new("x", Kind::Text).with("role", "assistant").with("text", "ok")).unwrap();
        set.insert(l);
        for set in [set.clone(), jsonfmt::without_raw(&set)] {
            let ex = export(&set, &ExportOptions::standard()).unwrap();
            let out = tempfile::tempdir().unwrap();
            std::fs::write(out.path().join("store.db"), ex.output.files.iter().find(|(k, _)| k.ends_with("store.db")).unwrap().1).unwrap();
            let c = rusqlite::Connection::open(out.path().join("store.db")).unwrap();
            let data: Vec<u8> = c.query_row("SELECT data FROM blobs WHERE id = ?1", [blob_id(odd.as_bytes())], |r| r.get(0)).unwrap();
            assert_eq!(String::from_utf8(data).unwrap(), odd);
        }
    }
}
