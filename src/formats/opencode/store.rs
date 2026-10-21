//! Containers: reading and writing the five opencode variants.
//!
//! A container yields [`Item`]s (native records with their logical value) and
//! takes back [`OutSession`]s. A record read from container C and written to
//! container C is emitted unchanged; written to another container of the same
//! generation it is converted through its logical value.

use super::render::{NativeRec, OutItem, OutSession};
use super::{Container, Item, MsgSrc, Role, SessionSrc, SourceSet, StoreSrc, s};
use crate::formats::{Losses, Output};
use crate::model::Obj;
use crate::util::OrderedMap;
use crate::util::{minify, pretty};
use anyhow::{Context, Result};
use serde_json::{Value, json};
use std::collections::{BTreeMap, BTreeSet, HashMap};
use std::path::Path;

// ---------------------------------------------------------------- JSON text

/// How a JSON file was laid out, so it can be written back byte for byte.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Style {
    /// `JSON.stringify(x, null, 2)` (opencode 1.x storage).
    Pretty,
    /// `JSON.stringify(x, null, 2) + EOL` (`opencode export`, `opencode session export`).
    PrettyEol,
    /// `JSON.stringify(x)` (files rewritten by 1.x storage migrations).
    Compact,
}

impl Style {
    fn name(self) -> &'static str {
        match self {
            Style::Pretty => "pretty",
            Style::PrettyEol => "pretty-eol",
            Style::Compact => "compact",
        }
    }
    fn from_name(n: &str) -> Option<Style> {
        Some(match n {
            "pretty" => Style::Pretty,
            "pretty-eol" => Style::PrettyEol,
            "compact" => Style::Compact,
            _ => return None,
        })
    }
}

/// Lay out compact JSON text in `style`; tokens are not re-spelled.
pub fn render_text(compact: &str, style: Style) -> String {
    match style {
        Style::Pretty => pretty(compact),
        Style::PrettyEol => pretty(compact) + "\n",
        Style::Compact => compact.to_string(),
    }
}

/// A value nobody spelled before (a converted or rendered record) in `style`.
pub fn render_json(v: &Value, style: Style) -> String {
    render_text(&serde_json::to_string(v).expect("json"), style)
}

/// A parsed JSON file: its value, its compact text (tokens as the file spells
/// them), and the layout that reproduces the file from that text, if one does.
struct Styled {
    value: Value,
    compact: String,
    style: Option<Style>,
}

fn parse_styled(text: &str) -> Option<Styled> {
    let value: Value = serde_json::from_str(text).ok()?;
    let compact = minify(text);
    let style = [Style::Pretty, Style::PrettyEol, Style::Compact].into_iter().find(|st| render_text(&compact, *st) == text);
    Some(Styled { value, compact, style })
}

/// The compact text of an export document's records, in document order:
/// `(info, [(message info, [parts])])`, sliced out of `compact` unchanged.
#[allow(clippy::type_complexity)]
fn record_texts(compact: &str, c: Container) -> Option<(Option<String>, Vec<(Option<String>, Vec<String>)>)> {
    use serde_json::value::RawValue;
    #[derive(serde::Deserialize)]
    struct Doc<'a> {
        #[serde(borrow, default)]
        info: Option<&'a RawValue>,
        #[serde(borrow, default)]
        messages: Vec<&'a RawValue>,
    }
    #[derive(serde::Deserialize)]
    struct Msg<'a> {
        #[serde(borrow, default)]
        info: Option<&'a RawValue>,
        #[serde(borrow, default)]
        parts: Vec<&'a RawValue>,
    }
    let text = |r: Option<&RawValue>| r.map(|r| r.get().to_string());
    let doc: Doc = serde_json::from_str(compact).ok()?;
    let mut messages = Vec::new();
    for m in doc.messages {
        messages.push(match c {
            Container::Export => {
                let m: Msg = serde_json::from_str(m.get()).ok()?;
                (text(m.info), m.parts.iter().map(|p| p.get().to_string()).collect())
            }
            _ => (Some(m.get().to_string()), vec![]),
        });
    }
    Some((text(doc.info), messages))
}

/// `it` held as its compact source text.
fn with_text(mut it: Item, text: Option<String>) -> Item {
    if let Some(t) = text {
        it.text = Some(t);
    }
    it
}

// ---------------------------------------------------------------- detect

fn is_sqlite(p: &Path) -> bool {
    std::fs::read(p).map(|b| b.starts_with(b"SQLite format 3\0")).unwrap_or(false)
}

/// The storage root under `dir` (`""`, `storage/` or `opencode/storage/`).
fn storage_prefix(dir: &Path) -> Option<&'static str> {
    ["", "storage/", "opencode/storage/"]
        .into_iter()
        .find(|p| dir.join(p).join("session").is_dir() && (dir.join(p).join("message").is_dir() || dir.join(p).join("part").is_dir()))
}

fn json_kind(v: &Value) -> Option<Container> {
    let info = v.get("info")?;
    let msgs = v.get("messages")?.as_array()?;
    if let Some(m) = msgs.first() {
        if m.get("info").is_some() && m.get("parts").is_some() {
            return Some(Container::Export);
        }
        if m.get("type").is_some() {
            return Some(Container::Transfer);
        }
        return None;
    }
    // No messages: tell by the session info.
    if info.get("location").is_some() || info.get("tokens").is_some() {
        Some(Container::Transfer)
    } else if info.get("directory").is_some() || info.get("slug").is_some() || info.get("projectID").is_some() {
        Some(Container::Export)
    } else {
        None
    }
}

fn json_files(dir: &Path) -> Vec<std::path::PathBuf> {
    let mut v: Vec<_> = std::fs::read_dir(dir)
        .into_iter()
        .flatten()
        .filter_map(|e| e.ok().map(|e| e.path()))
        .filter(|p| p.is_file() && p.extension().is_some_and(|x| x == "json"))
        .collect();
    v.sort();
    v
}

fn db_files(dir: &Path) -> Vec<std::path::PathBuf> {
    let mut v: Vec<_> = std::fs::read_dir(dir)
        .into_iter()
        .flatten()
        .filter_map(|e| e.ok().map(|e| e.path()))
        .filter(|p| p.is_file() && p.extension().is_some_and(|x| x == "db") && is_sqlite(p))
        .collect();
    v.sort();
    v
}

pub fn detect(input: &Path) -> Option<(Container, Option<String>)> {
    if input.is_file() {
        if is_sqlite(input) {
            return sqlite::detect(input);
        }
        let text = std::fs::read_to_string(input).ok()?;
        let v: Value = serde_json::from_str(&text).ok()?;
        let c = json_kind(&v)?;
        let version = v.get("info").and_then(|i| s(i, "version")).map(str::to_string);
        return Some((c, version));
    }
    if input.is_dir() {
        if let Some(prefix) = storage_prefix(input) {
            let _ = prefix;
            return Some((Container::Storage, None));
        }
        if let Some(db) = db_files(input).first() {
            return sqlite::detect(db);
        }
        let files = json_files(input);
        let first = files.first()?;
        let v: Value = serde_json::from_str(&std::fs::read_to_string(first).ok()?).ok()?;
        let c = json_kind(&v)?;
        return Some((c, v.get("info").and_then(|i| s(i, "version")).map(str::to_string)));
    }
    None
}

// ---------------------------------------------------------------- read

pub fn read(input: &Path, c: Container) -> Result<SourceSet> {
    match c {
        Container::Storage => read_storage(input),
        Container::Export | Container::Transfer => read_json_files(input, c),
        Container::SqliteV1 | Container::SqliteV2 => {
            let db = if input.is_file() {
                input.to_path_buf()
            } else {
                db_files(input).into_iter().next().ok_or_else(|| anyhow::anyhow!("{}: no opencode.db", input.display()))?
            };
            sqlite::read(&db, c)
        }
    }
}

fn item(role: Role, id: &str, value: Value, record: Value, raw: Option<String>, file: &str) -> Item {
    Item { role, id: id.to_string(), value, record, raw, text: None, file: file.to_string() }
}

fn read_json_files(input: &Path, c: Container) -> Result<SourceSet> {
    let files = if input.is_file() { vec![input.to_path_buf()] } else { json_files(input) };
    let mut sessions = Vec::new();
    let mut losses = Losses::default();
    for f in files {
        let text = std::fs::read_to_string(&f).with_context(|| f.display().to_string())?;
        let Some(Styled { value: v, compact, style }) = parse_styled(&text) else {
            losses.add("file-unparsed", None, "a JSON file that did not parse was skipped");
            continue;
        };
        if json_kind(&v) != Some(c) {
            losses.add("file-unrecognised", None, "a JSON file that is not an opencode export was skipped");
            continue;
        }
        let name = f.file_name().unwrap().to_string_lossy().to_string();
        let mut extra = Obj::new();
        extra.insert("file".into(), json!(name));
        match style {
            Some(st) => {
                extra.insert("style".into(), json!(st.name()));
            }
            None => {
                losses.add("export-file-reformatted", None, "the export file's whitespace is not JSON.stringify(x, null, 2); it is re-emitted in that layout")
            }
        }
        // Each record's own text, sliced out of the document (SPEC §4).
        let (info_text, msg_texts) = record_texts(&compact, c).unwrap_or_default();
        let info = v.get("info").cloned().unwrap_or(Value::Null);
        let sid = s(&info, "id").unwrap_or(name.trim_end_matches(".json")).to_string();
        let session = with_text(item(Role::Session, &sid, info.clone(), info, None, &name), info_text);
        let mut messages = Vec::new();
        for (mi_ix, m) in v.get("messages").and_then(Value::as_array).into_iter().flatten().enumerate() {
            let (m_text, p_texts) = msg_texts.get(mi_ix).cloned().unwrap_or_default();
            match c {
                Container::Export => {
                    let mi = m.get("info").cloned().unwrap_or(Value::Null);
                    let n = messages.len();
                    let mid = s(&mi, "id").map(str::to_string).unwrap_or_else(|| format!("@messages/{n}"));
                    let parts = m
                        .get("parts")
                        .and_then(Value::as_array)
                        .into_iter()
                        .flatten()
                        .enumerate()
                        .map(|(k, p)| {
                            // A part with no id gets a location anchor (SPEC §2.2).
                            let pid = s(p, "id").map(str::to_string).unwrap_or_else(|| format!("{mid}/parts/{k}"));
                            with_text(item(Role::Part, &pid, p.clone(), p.clone(), None, &name), p_texts.get(k).cloned())
                        })
                        .collect();
                    let info = with_text(item(Role::Message, &mid, mi.clone(), mi, None, &name), m_text);
                    messages.push(MsgSrc { info, parts });
                }
                _ => {
                    let n = messages.len();
                    let mid = s(m, "id").map(str::to_string).unwrap_or_else(|| format!("@messages/{n}"));
                    let info = with_text(item(Role::Message, &mid, m.clone(), m.clone(), None, &name), m_text);
                    messages.push(MsgSrc { info, parts: vec![] });
                }
            }
        }
        sessions.push(SessionSrc { session, messages, extras: vec![], extra });
    }
    Ok(SourceSet { container: c, sessions, store: None, losses })
}

fn read_storage(input: &Path) -> Result<SourceSet> {
    let prefix = storage_prefix(input).ok_or_else(|| anyhow::anyhow!("{}: no opencode storage tree", input.display()))?;
    let mut losses = Losses::default();
    // Every file under the input, keyed relative to it.
    let mut files: BTreeMap<String, String> = BTreeMap::new();
    for e in walkdir::WalkDir::new(input).sort_by_file_name() {
        let e = e?;
        if !e.file_type().is_file() {
            continue;
        }
        let rel = e.path().strip_prefix(input)?.to_string_lossy().replace('\\', "/");
        if rel.starts_with('.') || rel == "SOURCE.md" || rel == "expect.json" {
            continue;
        }
        match std::fs::read_to_string(e.path()) {
            Ok(t) => {
                files.insert(rel, t);
            }
            Err(_) => losses.add("file-unreadable", None, "a non-UTF-8 file in the storage tree was skipped"),
        }
    }
    let mut sessions: BTreeMap<String, SessionSrc> = BTreeMap::new();
    let mut messages: BTreeMap<String, (String, Item)> = BTreeMap::new(); // msg id → (session id, item)
    let mut parts: BTreeMap<String, Vec<Item>> = BTreeMap::new(); // msg id → parts
    let mut extras: Vec<(String, Item)> = Vec::new(); // (rel under prefix, item)
    let mut styles: BTreeMap<String, Style> = BTreeMap::new();
    let mut migration: Option<(String, String)> = None;
    for (rel, text) in &files {
        let (value, compact, style) = match parse_styled(text) {
            Some(st) => (Some(st.value), Some(st.compact), st.style),
            None => (None, None, None),
        };
        // The file's own text, when no layout reproduces it from the record's text.
        let raw = if style.is_none() { Some(text.clone()) } else { None };
        let item = |role: Role, id: &str, value: Value, record: Value, raw: Option<String>, file: &str| Item {
            text: compact.clone(),
            ..item(role, id, value, record, raw, file)
        };
        if let Some(st) = style
            && st != Style::Pretty
        {
            styles.insert(rel.clone(), st);
        }
        let inner = rel.strip_prefix(prefix).unwrap_or(rel);
        let segs: Vec<&str> = inner.split('/').collect();
        let stem = |x: &str| x.trim_end_matches(".json").to_string();
        let record = value.clone().unwrap_or_else(|| Value::String(text.clone()));
        match (segs.as_slice(), &value) {
            (["session", _pid, f], Some(v)) if f.ends_with(".json") && rel.starts_with(prefix) => {
                let id = s(v, "id").map(str::to_string).unwrap_or_else(|| stem(f));
                sessions.insert(
                    id.clone(),
                    SessionSrc { session: item(Role::Session, &id, v.clone(), record, raw, rel), messages: vec![], extras: vec![], extra: Obj::new() },
                );
            }
            (["message", sid, f], Some(v)) if f.ends_with(".json") && rel.starts_with(prefix) => {
                let id = s(v, "id").map(str::to_string).unwrap_or_else(|| stem(f));
                let sid = s(v, "sessionID").unwrap_or(sid).to_string();
                messages.insert(id.clone(), (sid, item(Role::Message, &id, v.clone(), record, raw, rel)));
            }
            (["part", mid, f], Some(v)) if f.ends_with(".json") && rel.starts_with(prefix) => {
                let id = s(v, "id").map(str::to_string).unwrap_or_else(|| stem(f));
                let mid = s(v, "messageID").unwrap_or(mid).to_string();
                parts.entry(mid).or_default().push(item(Role::Part, &id, v.clone(), record, raw, rel));
            }
            // The storage migration counter is bookkeeping, kept on the header.
            (["migration"], _) => {
                migration = Some((rel.clone(), text.clone()));
            }
            _ => extras.push((inner.to_string(), item(Role::Extra, &format!("@{rel}"), Value::Null, record, raw, rel))),
        }
    }
    // Attach messages and their parts (by id). opencode 1.1 lists messages by
    // id; 1.2+ by (time created, id), which agrees for ids opencode minted and
    // keeps hand-made ids in conversation order.
    let mut messages: Vec<(String, (String, Item))> = messages.into_iter().collect();
    messages.sort_by_key(|(id, (_, it))| (super::get_ms(&it.value, &["time", "created"]).unwrap_or(0), id.clone()));
    for (mid, (sid, info)) in messages {
        let mut ps = parts.remove(&mid).unwrap_or_default();
        ps.sort_by(|a, b| a.id.cmp(&b.id));
        match sessions.get_mut(&sid) {
            Some(sess) => sess.messages.push(MsgSrc { info, parts: ps }),
            None => losses.add_n("orphan-message", None, 1 + ps.len() as u64, "message files for a session with no session file were skipped"),
        }
    }
    for (_, ps) in parts {
        losses.add_n("orphan-part", None, ps.len() as u64, "part files for a message with no message file were skipped");
    }
    // Extras: per-session files go to their session, project files to the
    // sessions of that project, the rest to the first session.
    let first = sessions.keys().next().cloned();
    for (inner, x) in extras {
        let segs: Vec<&str> = inner.split('/').collect();
        let owner_sid = segs.iter().map(|seg| seg.trim_end_matches(".json")).find(|seg| sessions.contains_key(*seg)).map(str::to_string);
        if let Some(sid) = owner_sid {
            sessions.get_mut(&sid).unwrap().extras.push(x);
            continue;
        }
        if segs.first() == Some(&"project") && segs.len() == 2 {
            // A project file is bookkeeping for every session of the project: on the header.
            let pid = segs[1].trim_end_matches(".json");
            let owners: Vec<String> = sessions.iter().filter(|(_, ss)| s(&ss.session.value, "projectID") == Some(pid)).map(|(k, _)| k.clone()).collect();
            if !owners.is_empty() {
                let text = files.get(&x.file).cloned().unwrap_or_default();
                for o in owners {
                    sessions.get_mut(&o).unwrap().extra.insert("project".into(), json!({"path": x.file, "text": text}));
                }
                continue;
            }
        }
        match &first {
            Some(f) => sessions.get_mut(f).unwrap().extras.push(x),
            None => losses.add("file-unowned", None, "a storage file with no session to hold it was skipped"),
        }
    }
    let mut out = Vec::new();
    for (_, mut sess) in sessions {
        sess.extra.insert("prefix".into(), json!(prefix));
        if let Some((path, text)) = &migration {
            sess.extra.insert("migration".into(), json!({"path": path, "text": text}));
        }
        let mine: BTreeMap<String, Value> = styles
            .iter()
            .filter(|(k, _)| {
                *k == &sess.session.file
                    || sess.extras.iter().any(|x| &x.file == *k)
                    || sess.messages.iter().any(|m| &m.info.file == *k || m.parts.iter().any(|p| &p.file == *k))
            })
            .map(|(k, v)| (k.clone(), json!(v.name())))
            .collect();
        if !mine.is_empty() {
            sess.extra.insert("styles".into(), json!(mine));
        }
        out.push(sess);
    }
    Ok(SourceSet { container: Container::Storage, sessions: out, store: None, losses })
}

// ---------------------------------------------------------------- write

/// Files (or one database) for the target container.
pub fn write(sessions: &[OutSession], c: Container, losses: &mut Losses) -> Result<Output> {
    match c {
        Container::Storage => write_storage(sessions),
        Container::Export | Container::Transfer => write_json_files(sessions, c),
        Container::SqliteV1 | Container::SqliteV2 => sqlite::write(sessions, c, losses),
    }
}

/// The compact text of `it` in container `c`: a record that came from `c`
/// exactly as the ledger holds it (never re-serialised), anything else from
/// its logical value.
pub fn record_text_in(it: &OutItem, c: Container) -> String {
    match &it.native {
        Some(n) if n.container == c => match n.raw.as_ref().or(n.text.as_ref()) {
            Some(t) => t.clone(),
            None => serde_json::to_string(&n.record).expect("json"),
        },
        _ => serde_json::to_string(&it.value).expect("json"),
    }
}

fn write_json_files(sessions: &[OutSession], c: Container) -> Result<Output> {
    let mut out = Output::default();
    for sess in sessions.iter().filter(|s| !s.store) {
        let same = sess.src == Some(c);
        let name = if same { sess.extra.get("file").and_then(Value::as_str).map(str::to_string) } else { None }.unwrap_or_else(|| format!("{}.json", sess.id));
        let style = if same { sess.extra.get("style").and_then(Value::as_str).and_then(Style::from_name) } else { None }.unwrap_or(Style::PrettyEol);
        // The document is assembled from the records' texts, keys in the order
        // opencode writes them (`{info, messages}`, `{info, parts}`).
        let info = record_text_in(&sess.info, c);
        let messages: Vec<String> = match c {
            Container::Export => sess
                .messages
                .iter()
                .map(|m| {
                    let parts: Vec<String> = m.parts.iter().map(|p| record_text_in(p, c)).collect();
                    format!("{{\"info\":{},\"parts\":[{}]}}", record_text_in(&m.info, c), parts.join(","))
                })
                .collect(),
            _ => sess.messages.iter().map(|m| record_text_in(&m.info, c)).collect(),
        };
        let doc = format!("{{\"info\":{info},\"messages\":[{}]}}", messages.join(","));
        out.put(name, render_text(&doc, style));
    }
    Ok(out)
}

fn write_storage(sessions: &[OutSession]) -> Result<Output> {
    let mut out = Output::default();
    let c = Container::Storage;
    for sess in sessions.iter().filter(|s| !s.store) {
        let same = sess.src == Some(c);
        let prefix = if same { sess.extra.get("prefix").and_then(Value::as_str) } else { None }.unwrap_or("storage/").to_string();
        let styles: HashMap<String, Style> = sess
            .extra
            .get("styles")
            .and_then(Value::as_object)
            .map(|o| o.iter().filter_map(|(k, v)| Some((k.clone(), Style::from_name(v.as_str()?)?))).collect())
            .unwrap_or_default();
        let put = |out: &mut Output, it: &OutItem, default_path: String| {
            let (path, text) = match &it.native {
                Some(NativeRec { container, record, text, raw, file }) if *container == c => {
                    let style = styles.get(file).copied().unwrap_or(Style::Pretty);
                    let text = match (raw, text) {
                        (Some(r), _) => r.clone(),
                        (None, Some(t)) => render_text(t, style),
                        (None, None) => render_json(record, style),
                    };
                    (file.clone(), text)
                }
                _ => (default_path, render_json(&it.value, Style::Pretty)),
            };
            out.files.entry(path).or_insert_with(|| text.into_bytes());
        };
        let pid = s(&sess.info.value, "projectID").unwrap_or("global").to_string();
        put(&mut out, &sess.info, format!("{prefix}session/{pid}/{}.json", sess.id));
        if !same && s(&sess.info.value, "projectID").is_some() {
            // The project the session belongs to: 1.2's JSON migration skips sessions of unknown projects.
            let t = sess.info.value.get("time").and_then(|t| t.get("created")).cloned().unwrap_or(json!(0));
            let worktree = if pid == "global" { json!("/") } else { sess.info.value.get("directory").cloned().unwrap_or(json!("/")) };
            let project = json!({"id": pid, "worktree": worktree, "sandboxes": [], "time": {"created": t, "updated": t}});
            out.files.entry(format!("{prefix}project/{pid}.json")).or_insert_with(|| render_json(&project, Style::Pretty).into_bytes());
        }
        for m in &sess.messages {
            let mid = s(&m.info.value, "id").unwrap_or("").to_string();
            put(&mut out, &m.info, format!("{prefix}message/{}/{mid}.json", sess.id));
            for p in &m.parts {
                let id = s(&p.value, "id").unwrap_or("");
                put(&mut out, p, format!("{prefix}part/{mid}/{id}.json"));
            }
        }
        for x in &sess.extras {
            put(&mut out, x, String::new());
        }
        if let Some(p) = sess.extra.get("project").filter(|_| same)
            && let (Some(path), Some(text)) = (p.get("path").and_then(Value::as_str), p.get("text").and_then(Value::as_str))
        {
            out.files.entry(path.to_string()).or_insert_with(|| text.as_bytes().to_vec());
        }
        match sess.extra.get("migration").filter(|_| same) {
            Some(m) => {
                let path = m.get("path").and_then(Value::as_str).unwrap_or("migration").to_string();
                let text = m.get("text").and_then(Value::as_str).unwrap_or("2").to_string();
                out.files.entry(path).or_insert_with(|| text.into_bytes());
            }
            // 1.1.x runs its storage migrations from this counter; both are done.
            None => {
                out.files.entry(format!("{prefix}migration")).or_insert_with(|| b"2".to_vec());
            }
        }
    }
    out.files.remove("");
    Ok(out)
}

// ---------------------------------------------------------------- logical ↔ rows

/// Parse a JSON text column; `null` and non-JSON stay as they are.
fn json_col(v: Option<&Value>) -> Option<Value> {
    match v? {
        Value::String(t) => serde_json::from_str(t).ok(),
        Value::Null => None,
        other => Some(other.clone()),
    }
}

fn text_col(v: Option<&Value>) -> Value {
    match v {
        None | Some(Value::Null) => Value::Null,
        Some(x) => Value::String(serde_json::to_string(x).expect("json")),
    }
}

fn put_opt(o: &mut serde_json::Map<String, Value>, k: &str, v: Option<Value>) {
    if let Some(v) = v.filter(|v| !v.is_null()) {
        o.insert(k.into(), v);
    }
}

pub fn v1_session_from_row(r: &Value) -> Value {
    let mut o = serde_json::Map::new();
    o.insert("id".into(), r.get("id").cloned().unwrap_or(Value::Null));
    o.insert("slug".into(), r.get("slug").cloned().unwrap_or(Value::Null));
    o.insert("projectID".into(), r.get("project_id").cloned().unwrap_or(Value::Null));
    put_opt(&mut o, "workspaceID", r.get("workspace_id").cloned());
    o.insert("directory".into(), r.get("directory").cloned().unwrap_or(Value::Null));
    put_opt(&mut o, "path", r.get("path").cloned());
    put_opt(&mut o, "parentID", r.get("parent_id").cloned());
    if ["summary_additions", "summary_deletions", "summary_files"].iter().any(|k| r.get(*k).is_some_and(|v| !v.is_null())) {
        let mut sm = json!({
            "additions": r.get("summary_additions").cloned().filter(|v| !v.is_null()).unwrap_or(json!(0)),
            "deletions": r.get("summary_deletions").cloned().filter(|v| !v.is_null()).unwrap_or(json!(0)),
            "files": r.get("summary_files").cloned().filter(|v| !v.is_null()).unwrap_or(json!(0)),
        });
        if let Some(d) = json_col(r.get("summary_diffs")) {
            sm["diffs"] = d;
        }
        o.insert("summary".into(), sm);
    }
    if let Some(u) = r.get("share_url").and_then(Value::as_str) {
        o.insert("share".into(), json!({"url": u}));
    }
    put_opt(&mut o, "title", r.get("title").cloned());
    put_opt(&mut o, "agent", r.get("agent").cloned());
    put_opt(&mut o, "model", json_col(r.get("model")));
    put_opt(&mut o, "metadata", json_col(r.get("metadata")));
    o.insert("version".into(), r.get("version").cloned().unwrap_or(Value::Null));
    let mut time = json!({"created": r.get("time_created").cloned().unwrap_or(Value::Null), "updated": r.get("time_updated").cloned().unwrap_or(Value::Null)});
    if let Some(v) = r.get("time_compacting").filter(|v| !v.is_null()) {
        time["compacting"] = v.clone();
    }
    if let Some(v) = r.get("time_archived").filter(|v| !v.is_null()) {
        time["archived"] = v.clone();
    }
    o.insert("time".into(), time);
    put_opt(&mut o, "permission", json_col(r.get("permission")));
    put_opt(&mut o, "revert", json_col(r.get("revert")));
    Value::Object(o)
}

pub fn v1_session_to_row(i: &Value, slug: &str, version: &str) -> Value {
    let tok = i.get("tokens");
    let n = |v: Option<&Value>| v.cloned().unwrap_or(json!(0));
    let created = i.get("time").and_then(|t| t.get("created")).cloned().unwrap_or(json!(0));
    let mut row = json!({
        "id": i.get("id"),
        "project_id": i.get("projectID").cloned().unwrap_or(json!("global")),
        "workspace_id": i.get("workspaceID"),
        "parent_id": i.get("parentID"),
        "slug": i.get("slug"),
        "directory": i.get("directory"),
        "path": i.get("path"),
        "title": i.get("title"),
        "version": i.get("version"),
        "share_url": i.get("share").and_then(|x| x.get("url")),
        "summary_additions": i.get("summary").and_then(|x| x.get("additions")),
        "summary_deletions": i.get("summary").and_then(|x| x.get("deletions")),
        "summary_files": i.get("summary").and_then(|x| x.get("files")),
        "summary_diffs": text_col(i.get("summary").and_then(|x| x.get("diffs"))),
        "metadata": text_col(i.get("metadata")),
        "cost": n(i.get("cost")),
        "tokens_input": n(tok.and_then(|t| t.get("input"))),
        "tokens_output": n(tok.and_then(|t| t.get("output"))),
        "tokens_reasoning": n(tok.and_then(|t| t.get("reasoning"))),
        "tokens_cache_read": n(tok.and_then(|t| t.get("cache")).and_then(|c| c.get("read"))),
        "tokens_cache_write": n(tok.and_then(|t| t.get("cache")).and_then(|c| c.get("write"))),
        "revert": text_col(i.get("revert")),
        "permission": text_col(i.get("permission")),
        "agent": i.get("agent"),
        "model": text_col(i.get("model")),
        "time_created": i.get("time").and_then(|t| t.get("created")),
        "time_updated": i.get("time").and_then(|t| t.get("updated")),
        "time_compacting": i.get("time").and_then(|t| t.get("compacting")),
        "time_archived": i.get("time").and_then(|t| t.get("archived")),
    });
    // NOT NULL columns a hand-made or older session info may lack.
    let defaults = [
        ("slug", json!(slug)),
        ("directory", json!("/")),
        ("title", json!(format!("New session - {}", crate::util::ms_to_rfc3339(created.as_i64().unwrap_or(0))))),
        ("version", json!(version)),
        ("time_created", created.clone()),
        ("time_updated", created),
    ];
    for (k, v) in defaults {
        if row[k].is_null() {
            row[k] = v;
        }
    }
    row
}

fn without(v: &Value, keys: &[&str]) -> Value {
    let mut v = v.clone();
    if let Value::Object(o) = &mut v {
        for k in keys {
            o.remove_ordered(k);
        }
    }
    v
}

pub fn v1_message_from_row(r: &Value) -> Value {
    let mut v = match r.get("data") {
        Some(Value::Object(o)) => Value::Object(o.clone()),
        Some(Value::String(t)) => serde_json::from_str(t).unwrap_or(json!({})),
        _ => json!({}),
    };
    v["id"] = r.get("id").cloned().unwrap_or(Value::Null);
    v["sessionID"] = r.get("session_id").cloned().unwrap_or(Value::Null);
    v
}

pub fn v1_message_to_row(m: &Value, session: &str) -> Value {
    let created = m.get("time").and_then(|t| t.get("created")).cloned().unwrap_or(json!(0));
    let updated = m.get("time").and_then(|t| t.get("completed")).cloned().unwrap_or(created.clone());
    json!({"id": m.get("id"), "session_id": m.get("sessionID").cloned().unwrap_or(json!(session)), "time_created": created, "time_updated": updated, "data": without(m, &["id", "sessionID"])})
}

pub fn v1_part_from_row(r: &Value) -> Value {
    let mut v = match r.get("data") {
        Some(Value::Object(o)) => Value::Object(o.clone()),
        Some(Value::String(t)) => serde_json::from_str(t).unwrap_or(json!({})),
        _ => json!({}),
    };
    v["id"] = r.get("id").cloned().unwrap_or(Value::Null);
    v["sessionID"] = r.get("session_id").cloned().unwrap_or(Value::Null);
    v["messageID"] = r.get("message_id").cloned().unwrap_or(Value::Null);
    v
}

pub fn v1_part_to_row(p: &Value, msg_time: &Value, session: &str, message: &str) -> Value {
    let t = p
        .get("time")
        .and_then(|t| t.get("start"))
        .or_else(|| p.get("state").and_then(|st| st.get("time")).and_then(|t| t.get("start")))
        .cloned()
        .unwrap_or(msg_time.clone());
    json!({"id": p.get("id"), "message_id": p.get("messageID").cloned().unwrap_or(json!(message)), "session_id": p.get("sessionID").cloned().unwrap_or(json!(session)), "time_created": t, "time_updated": t, "data": without(p, &["id", "sessionID", "messageID"])})
}

pub fn v2_session_from_row(r: &Value) -> Value {
    let mut o = serde_json::Map::new();
    o.insert("id".into(), r.get("id").cloned().unwrap_or(Value::Null));
    put_opt(&mut o, "parentID", r.get("parent_id").cloned());
    if let (Some(fs), Some(fb)) = (r.get("fork_session_id").filter(|v| !v.is_null()), json_col(r.get("fork_boundary"))) {
        o.insert("fork".into(), json!({"sessionID": fs, "boundary": fb}));
    }
    o.insert("projectID".into(), r.get("project_id").cloned().unwrap_or(Value::Null));
    put_opt(&mut o, "agent", r.get("agent").cloned());
    if let Some(mut m) = json_col(r.get("model")) {
        if m.get("variant").is_none() {
            m["variant"] = json!("default");
        }
        o.insert("model".into(), m);
    }
    o.insert("cost".into(), r.get("cost").cloned().map(|c| super::js_num(c.as_f64().unwrap_or(0.0))).unwrap_or(json!(0)));
    o.insert(
        "tokens".into(),
        json!({
            "input": r.get("tokens_input").cloned().unwrap_or(json!(0)),
            "output": r.get("tokens_output").cloned().unwrap_or(json!(0)),
            "reasoning": r.get("tokens_reasoning").cloned().unwrap_or(json!(0)),
            "cache": {"read": r.get("tokens_cache_read").cloned().unwrap_or(json!(0)), "write": r.get("tokens_cache_write").cloned().unwrap_or(json!(0))},
        }),
    );
    put_opt(&mut o, "outcome", r.get("idle_outcome").cloned());
    let mut time = json!({"created": r.get("time_created").cloned().unwrap_or(Value::Null), "updated": r.get("time_updated").cloned().unwrap_or(Value::Null)});
    for (k, col) in [("idle", "time_idle"), ("viewed", "time_viewed"), ("archived", "time_archived")] {
        if let Some(v) = r.get(col).filter(|v| !v.is_null()) {
            time[k] = v.clone();
        }
    }
    o.insert("time".into(), time);
    put_opt(&mut o, "title", r.get("title").cloned());
    let mut loc = json!({"directory": r.get("directory").cloned().unwrap_or(Value::Null)});
    if let Some(w) = r.get("workspace_id").filter(|v| !v.is_null()) {
        loc["workspaceID"] = w.clone();
    }
    o.insert("location".into(), loc);
    put_opt(&mut o, "subpath", r.get("path").cloned().filter(|p| p.as_str() != Some("")));
    put_opt(&mut o, "metadata", json_col(r.get("metadata")));
    put_opt(&mut o, "permissions", json_col(r.get("permission")));
    put_opt(&mut o, "revert", json_col(r.get("revert")));
    // Not part of Session.Info, kept so a row converts back to itself.
    put_opt(&mut o, "slug", r.get("slug").cloned());
    put_opt(&mut o, "version", r.get("version").cloned());
    Value::Object(o)
}

pub fn v2_session_to_row(i: &Value, slug: &str, version: &str) -> Value {
    let tok = i.get("tokens");
    let n = |v: Option<&Value>| v.cloned().unwrap_or(json!(0));
    let t = |k: &str| i.get("time").and_then(|t| t.get(k)).cloned().unwrap_or(Value::Null);
    json!({
        "id": i.get("id"),
        "project_id": i.get("projectID").cloned().unwrap_or(json!("global")),
        "workspace_id": i.get("location").and_then(|l| l.get("workspaceID")),
        "parent_id": i.get("parentID"),
        "fork_session_id": i.get("fork").and_then(|f| f.get("sessionID")),
        "fork_boundary": text_col(i.get("fork").and_then(|f| f.get("boundary"))),
        "slug": i.get("slug").cloned().unwrap_or(json!(slug)),
        "directory": i.get("location").and_then(|l| l.get("directory")).cloned().unwrap_or(json!("/")),
        "path": i.get("subpath"),
        "title": i.get("title"),
        "version": i.get("version").cloned().unwrap_or(json!(version)),
        "share_url": null,
        "summary_additions": null,
        "summary_deletions": null,
        "summary_files": null,
        "summary_diffs": null,
        "metadata": text_col(i.get("metadata")),
        "cost": n(i.get("cost")),
        "tokens_input": n(tok.and_then(|t| t.get("input"))),
        "tokens_output": n(tok.and_then(|t| t.get("output"))),
        "tokens_reasoning": n(tok.and_then(|t| t.get("reasoning"))),
        "tokens_cache_read": n(tok.and_then(|t| t.get("cache")).and_then(|c| c.get("read"))),
        "tokens_cache_write": n(tok.and_then(|t| t.get("cache")).and_then(|c| c.get("write"))),
        "revert": text_col(i.get("revert")),
        "permission": text_col(i.get("permissions")),
        "agent": i.get("agent"),
        "model": text_col(i.get("model")),
        "time_created": t("created"),
        "time_updated": t("updated"),
        "time_idle": t("idle"),
        "time_viewed": t("viewed"),
        "idle_outcome": i.get("outcome"),
        "time_compacting": null,
        "time_archived": t("archived"),
        "time_suspended": null,
        "resume_attempts": 0,
    })
}

pub fn v2_message_from_row(r: &Value) -> Value {
    let data = match r.get("data") {
        Some(Value::Object(o)) => o.clone(),
        Some(Value::String(t)) => serde_json::from_str(t).unwrap_or_default(),
        _ => serde_json::Map::new(),
    };
    let mut o = serde_json::Map::new();
    o.insert("id".into(), r.get("id").cloned().unwrap_or(Value::Null));
    o.extend(data);
    o.insert("type".into(), r.get("type").cloned().unwrap_or(Value::Null));
    Value::Object(o)
}

pub fn v2_message_to_row(m: &Value, session: &str, seq: i64) -> Value {
    let created = m.get("time").and_then(|t| t.get("created")).cloned().unwrap_or(json!(0));
    let updated = m.get("time").and_then(|t| t.get("completed")).cloned().unwrap_or(created.clone());
    json!({"id": m.get("id"), "session_id": session, "type": m.get("type"), "seq": seq, "time_created": created, "time_updated": updated, "data": without(m, &["id", "type"])})
}

// ---------------------------------------------------------------- sqlite

#[cfg(not(feature = "sqlite"))]
mod sqlite {
    use super::*;
    use anyhow::bail;
    pub fn detect(_p: &Path) -> Option<(Container, Option<String>)> {
        None
    }
    pub fn read(p: &Path, _c: Container) -> Result<SourceSet> {
        bail!("{}: opencode.db support needs the `sqlite` feature", p.display())
    }
    pub fn write(_s: &[OutSession], _c: Container, _l: &mut Losses) -> Result<Output> {
        bail!("opencode: sqlite export needs the `sqlite` feature")
    }
}

#[cfg(feature = "sqlite")]
mod sqlite {
    use super::*;
    use base64::Engine;
    use rusqlite::types::{Value as Sv, ValueRef};
    use rusqlite::{Connection, OpenFlags};

    const SCHEMA_V2: &str = include_str!("schema_v2.sql");
    const MIGRATIONS_V2: &str = include_str!("migrations_v2.txt");
    const SCHEMA_V1: &str = include_str!("schema_v1.sql");
    /// The drizzle journal 1.2.0 writes: (folder, sha256 of migration.sql).
    const MIGRATIONS_V1: &[(&str, &str)] = &[
        ("20260127222353_familiar_lady_ursula", "30e179e4c29b97cc6ee462ba7b7156113f2ee2d9b5ba302dac93c7d8baa60578"),
        ("20260211171708_add_project_commands", "d8dd043540753fd59122af8ee190f04677315c5d6d06356ae407472d5f9b6a12"),
        ("20260213144116_wakeful_the_professor", "738a920a0fd2afd341bca9fe98cadb216084bd7c8523180e13286ef1e322eb50"),
    ];
    const DRIZZLE_DDL: &str =
        "CREATE TABLE IF NOT EXISTS \"__drizzle_migrations\" (\n\t\t\tid SERIAL PRIMARY KEY,\n\t\t\thash text NOT NULL,\n\t\t\tcreated_at numeric\n\t\t)";
    const MIGRATION_DDL: &str = "CREATE TABLE \"migration\" (id TEXT PRIMARY KEY, time_completed INTEGER NOT NULL)";

    /// Tables kept as bookkeeping (needed for opencode to open the db as is).
    const BOOKKEEPING: &[&str] = &["migration", "__drizzle_migrations"];

    fn open(p: &Path) -> Result<Connection> {
        Ok(Connection::open_with_flags(p, OpenFlags::SQLITE_OPEN_READ_ONLY | OpenFlags::SQLITE_OPEN_NO_MUTEX)?)
    }

    fn tables(db: &Connection) -> Result<Vec<String>> {
        let mut st = db.prepare("SELECT name FROM sqlite_master WHERE type = 'table' ORDER BY rowid")?;
        let v = st.query_map([], |r| r.get::<_, String>(0))?.collect::<std::result::Result<Vec<_>, _>>()?;
        Ok(v)
    }

    pub fn detect(p: &Path) -> Option<(Container, Option<String>)> {
        let db = open(p).ok()?;
        let t = tables(&db).ok()?;
        let has = |n: &str| t.iter().any(|x| x == n);
        let (c, table) = if has("session_v2") && has("session_message") {
            (Container::SqliteV2, "session_v2")
        } else if has("session") && has("message") && has("part") {
            (Container::SqliteV1, "session")
        } else {
            return None;
        };
        let version: Option<String> = db.query_row(&format!("SELECT version FROM \"{table}\" ORDER BY time_updated DESC LIMIT 1"), [], |r| r.get(0)).ok();
        Some((c, version))
    }

    fn to_json(v: ValueRef<'_>) -> Value {
        match v {
            ValueRef::Null => Value::Null,
            ValueRef::Integer(i) => json!(i),
            ValueRef::Real(f) => serde_json::Number::from_f64(f).map(Value::Number).unwrap_or(Value::Null),
            ValueRef::Text(t) => Value::String(String::from_utf8_lossy(t).into_owned()),
            ValueRef::Blob(b) => json!({"$blob": base64::engine::general_purpose::STANDARD.encode(b)}),
        }
    }

    fn to_sql(v: &Value) -> Sv {
        match v {
            Value::Null => Sv::Null,
            Value::Bool(b) => Sv::Integer(*b as i64),
            Value::Number(n) => match n.as_i64() {
                Some(i) if !n.to_string().contains(['.', 'e', 'E']) => Sv::Integer(i),
                _ => Sv::Real(n.as_f64().unwrap_or(0.0)),
            },
            Value::String(s) => Sv::Text(s.clone()),
            Value::Object(o) if o.len() == 1 && o.contains_key("$blob") => {
                Sv::Blob(base64::engine::general_purpose::STANDARD.decode(o["$blob"].as_str().unwrap_or("")).unwrap_or_default())
            }
            other => Sv::Text(serde_json::to_string(other).expect("json")),
        }
    }

    /// Rows of `table` (optionally filtered), as JSON objects in column order.
    fn rows(db: &Connection, sql: &str, params: &[&dyn rusqlite::ToSql]) -> Result<Vec<Value>> {
        crate::formats::sqlite_schema_is_utf8(db)?;
        let mut st = db.prepare(sql)?;
        let cols: Vec<String> = st.column_names().iter().map(|c| c.to_string()).collect();
        let mut q = st.query(params)?;
        let mut out = Vec::new();
        while let Some(r) = q.next()? {
            let mut o = serde_json::Map::new();
            for (i, c) in cols.iter().enumerate() {
                o.insert(c.clone(), to_json(r.get_ref(i)?));
            }
            out.push(Value::Object(o));
        }
        Ok(out)
    }

    /// Parse the `data` column; keep its exact text when JSON would not reproduce it.
    fn with_data(mut row: Value) -> (Value, Option<String>) {
        let mut raw = None;
        if let Some(Value::String(t)) = row.get("data").cloned()
            && let Ok(v) = serde_json::from_str::<Value>(&t)
        {
            if serde_json::to_string(&v).expect("json") != t {
                raw = Some(t);
            }
            row["data"] = v;
        }
        (row, raw)
    }

    pub fn read(p: &Path, c: Container) -> Result<SourceSet> {
        let db = open(p)?;
        let name = p.file_name().unwrap().to_string_lossy().to_string();
        let file = |t: &str| format!("{name}:{t}");
        let all = tables(&db)?;
        let has = |n: &str| all.iter().any(|x| x == n);
        let losses = Losses::default();
        // Schema, in creation order, so a re-export recreates the same tables.
        let schema: Vec<Value> = {
            let mut st = db.prepare("SELECT sql FROM sqlite_master WHERE sql IS NOT NULL AND name NOT LIKE 'sqlite_%' ORDER BY rowid")?;
            st.query_map([], |r| r.get::<_, String>(0))?.filter_map(|x| x.ok()).map(Value::String).collect()
        };
        let mut book = Obj::new();
        for t in BOOKKEEPING {
            if has(t) {
                book.insert(t.to_string(), Value::Array(rows(&db, &format!("SELECT * FROM \"{t}\""), &[])?));
            }
        }
        let (stable, threads): (&str, &[&str]) = match c {
            Container::SqliteV1 => ("session", &["session", "message", "part"]),
            _ => ("session_v2", &["session_v2", "session_message"]),
        };
        let projects: HashMap<String, Value> = if has("project") {
            rows(&db, "SELECT * FROM project ORDER BY id", &[])?.into_iter().map(|r| (s(&r, "id").unwrap_or("").to_string(), r)).collect()
        } else {
            HashMap::new()
        };
        let srows = rows(&db, &format!("SELECT * FROM \"{stable}\" ORDER BY id"), &[])?;
        let sids: BTreeSet<String> = srows.iter().filter_map(|r| s(r, "id").map(str::to_string)).collect();
        // Every other row of the store is a native record too (SPEC §4): a
        // row that names a session (`session_id`, an event log's
        // `aggregate_id`) goes with that session; the rest (every project row,
        // worktrees, instruction blobs, rows of a session the store no longer
        // holds, ...) go to the store's own ledger, which does not move as
        // sessions come and go. What the session model already holds is
        // skipped: the session, its messages and parts, its event-sequence
        // watermark (and its project row, also kept on its header).
        let mut carried: BTreeMap<String, Vec<Item>> = BTreeMap::new();
        let mut store_rows: Vec<Item> = Vec::new();
        for t in &all {
            if BOOKKEEPING.contains(&t.as_str()) || t.starts_with("sqlite_") || t == stable {
                continue;
            }
            let pk = primary_key(&db, t)?;
            let order = if pk.is_empty() { "rowid".to_string() } else { pk.iter().map(|k| format!("\"{k}\"")).collect::<Vec<_>>().join(", ") };
            for (n, r) in rows(&db, &format!("SELECT * FROM \"{t}\" ORDER BY {order}"), &[])?.into_iter().enumerate() {
                let named = ["session_id", "aggregate_id"].iter().find_map(|k| s(&r, k)).filter(|x| sids.contains(*x)).map(str::to_string);
                let held = match t.as_str() {
                    "project" => false,
                    "event_sequence" => named.is_some(),
                    _ => named.is_some() && threads.contains(&t.as_str()),
                };
                if held {
                    continue;
                }
                let key = if pk.is_empty() { n.to_string() } else { row_key(&r, &pk) };
                let x = item(Role::Extra, &format!("@{t}/{key}"), Value::Null, r, None, &file(t));
                match named {
                    Some(owner) => carried.entry(owner).or_default().push(x),
                    None => store_rows.push(x),
                }
            }
        }
        let mut container_extra = Obj::new();
        container_extra.insert("db".into(), json!(name));
        container_extra.insert("schema".into(), Value::Array(schema.clone()));
        if !book.is_empty() {
            container_extra.insert("bookkeeping".into(), Value::Object(book.clone()));
        }
        let store = if store_rows.is_empty() {
            None
        } else {
            let first: Option<(String, i64)> = if has("project") {
                db.query_row("SELECT id, time_created FROM project ORDER BY time_created, id LIMIT 1", [], |r| {
                    Ok((r.get::<_, String>(0)?, r.get::<_, i64>(1)?))
                })
                .ok()
            } else {
                None
            };
            Some(StoreSrc {
                id: super::super::store_ledger_id(first.as_ref().map(|(p, t)| (p.as_str(), *t))),
                rows: store_rows,
                extra: container_extra.clone(),
            })
        };
        let mut sessions = Vec::new();
        for srow in srows {
            let sid = s(&srow, "id").unwrap_or("").to_string();
            let value = match c {
                Container::SqliteV1 => v1_session_from_row(&srow),
                _ => v2_session_from_row(&srow),
            };
            let session = item(Role::Session, &sid, value, srow.clone(), None, &file(stable));
            let mut extras = carried.remove(&sid).unwrap_or_default();
            // Bookkeeping rows ride on the header: the project row the session
            // belongs to and its event sequence watermark.
            let mut book_rows = Obj::new();
            if let Some(pr) = s(&srow, "project_id").and_then(|p| projects.get(p)) {
                book_rows.insert("project".into(), pr.clone());
            }
            if has("event_sequence")
                && let Some(r) = rows(&db, "SELECT * FROM event_sequence WHERE aggregate_id = ?1", &[&sid])?.into_iter().next()
            {
                book_rows.insert("event_sequence".into(), r);
            }
            let mut messages = Vec::new();
            match c {
                Container::SqliteV1 => {
                    let mut parts: BTreeMap<String, Vec<Item>> = BTreeMap::new();
                    for r in rows(&db, "SELECT * FROM part WHERE session_id = ?1 ORDER BY id", &[&sid])? {
                        let (r, raw) = with_data(r);
                        let id = s(&r, "id").unwrap_or("").to_string();
                        let mid = s(&r, "message_id").unwrap_or("").to_string();
                        parts.entry(mid).or_default().push(item(Role::Part, &id, v1_part_from_row(&r), r, raw, &file("part")));
                    }
                    for r in rows(&db, "SELECT * FROM message WHERE session_id = ?1 ORDER BY time_created, id", &[&sid])? {
                        let (r, raw) = with_data(r);
                        let id = s(&r, "id").unwrap_or("").to_string();
                        let ps = parts.remove(&id).unwrap_or_default();
                        messages.push(MsgSrc { info: item(Role::Message, &id, v1_message_from_row(&r), r, raw, &file("message")), parts: ps });
                    }
                    // Parts of a message the session does not hold are kept as rows.
                    for p in parts.into_values().flatten() {
                        extras.push(item(Role::Extra, &format!("@part/{}", p.id), Value::Null, p.record, p.raw, &p.file));
                    }
                }
                _ => {
                    for r in rows(&db, "SELECT * FROM session_message WHERE session_id = ?1 ORDER BY seq", &[&sid])? {
                        let (r, raw) = with_data(r);
                        let id = s(&r, "id").unwrap_or("").to_string();
                        messages.push(MsgSrc { info: item(Role::Message, &id, v2_message_from_row(&r), r, raw, &file("session_message")), parts: vec![] });
                    }
                }
            }
            let mut extra = container_extra.clone();
            if !book_rows.is_empty() {
                extra.insert("rows".into(), Value::Object(book_rows));
            }
            sessions.push(SessionSrc { session, messages, extras, extra });
        }
        Ok(SourceSet { container: c, sessions, store, losses })
    }

    /// The primary key columns of `table`, in key order (empty for a rowid table).
    fn primary_key(db: &Connection, table: &str) -> Result<Vec<String>> {
        let mut st = db.prepare(&format!("SELECT name FROM pragma_table_info('{table}') WHERE pk > 0 ORDER BY pk"))?;
        let v = st.query_map([], |r| r.get::<_, String>(0))?.collect::<std::result::Result<Vec<_>, _>>()?;
        Ok(v)
    }

    /// A row's primary key values joined by `/` (its anchor id is `@<table>/<key>`).
    fn row_key(r: &Value, pk: &[String]) -> String {
        pk.iter().map(|k| key_part(r.get(k.as_str()))).collect::<Vec<_>>().join("/")
    }

    /// One primary key value in a row's anchor id (`@<table>/<key>/...`).
    fn key_part(v: Option<&Value>) -> String {
        match v {
            Some(Value::String(x)) => x.clone(),
            Some(x) => x.to_string(),
            None => String::new(),
        }
    }

    fn columns(db: &Connection, table: &str) -> Result<Vec<String>> {
        let mut st = db.prepare(&format!("SELECT name FROM pragma_table_info('{table}')"))?;
        let v = st.query_map([], |r| r.get::<_, String>(0))?.collect::<std::result::Result<Vec<_>, _>>()?;
        Ok(v)
    }

    struct Writer<'a> {
        db: &'a Connection,
        cols: HashMap<String, Vec<String>>,
        /// Primary key columns per table (empty for a rowid table).
        pks: HashMap<String, Vec<String>>,
        seen: BTreeSet<(String, String)>,
        /// Non-null values for columns the target schema does not have.
        dropped: u64,
    }

    impl Writer<'_> {
        /// A row of `table` read from a database (its key is its primary
        /// key's values, `None` for a rowid table): one the store holds twice
        /// (on a session's header and in the store ledger) is written once.
        fn insert_row(&mut self, table: &str, row: &Value, raw: Option<&str>) -> Result<()> {
            if !self.pks.contains_key(table) {
                let pk = primary_key(self.db, table)?;
                self.pks.insert(table.to_string(), pk);
            }
            let pk = &self.pks[table];
            let key = if pk.is_empty() { None } else { Some(row_key(row, pk)) };
            self.insert(table, row, raw, key)
        }

        fn insert(&mut self, table: &str, row: &Value, raw: Option<&str>, key: Option<String>) -> Result<()> {
            if let Some(k) = key
                && !self.seen.insert((table.to_string(), k))
            {
                return Ok(());
            }
            if !self.cols.contains_key(table) {
                let c = columns(self.db, table)?;
                if c.is_empty() {
                    // A schema without this table (an older or partial db): nothing to write.
                    self.cols.insert(table.to_string(), c);
                    return Ok(());
                }
                self.cols.insert(table.to_string(), c);
            }
            let have = &self.cols[table];
            if have.is_empty() {
                return Ok(());
            }
            let Some(o) = row.as_object() else { return Ok(()) };
            let keys: Vec<&String> = o.keys().filter(|k| have.contains(k)).collect();
            self.dropped += o.iter().filter(|(k, v)| !have.contains(k) && !v.is_null()).count() as u64;
            let sql = format!(
                "INSERT INTO \"{table}\" ({}) VALUES ({})",
                keys.iter().map(|k| format!("\"{k}\"")).collect::<Vec<_>>().join(", "),
                (1..=keys.len()).map(|i| format!("?{i}")).collect::<Vec<_>>().join(", ")
            );
            let vals: Vec<Sv> = keys
                .iter()
                .map(|k| {
                    if k.as_str() == "data" {
                        if let Some(r) = raw {
                            return Sv::Text(r.to_string());
                        }
                        if o[*k].is_object() || o[*k].is_array() {
                            return Sv::Text(serde_json::to_string(&o[*k]).expect("json"));
                        }
                    }
                    to_sql(&o[*k])
                })
                .collect();
            self.db.execute(&sql, rusqlite::params_from_iter(vals.iter()))?;
            Ok(())
        }
    }

    fn table_of(file: &str) -> &str {
        file.rsplit_once(':').map(|(_, t)| t).unwrap_or(file)
    }

    pub fn write(sessions: &[OutSession], c: Container, losses: &mut Losses) -> Result<Output> {
        let tmp = tempfile::tempdir()?;
        // The db's name and schema come from the first session read from this container.
        let native = sessions.iter().find(|s| s.src == Some(c));
        let name = native.and_then(|s| s.extra.get("db")).and_then(Value::as_str).unwrap_or("opencode.db").to_string();
        let path = tmp.path().join(&name);
        {
            let db = Connection::open(&path)?;
            db.execute_batch("PRAGMA foreign_keys = OFF; PRAGMA journal_mode = DELETE;")?;
            let tx = db.unchecked_transaction()?;
            let stored: Option<Vec<String>> =
                native.and_then(|s| s.extra.get("schema")).and_then(Value::as_array).map(|a| a.iter().filter_map(|x| x.as_str().map(str::to_string)).collect());
            let book: Option<Obj> = native.and_then(|s| s.extra.get("bookkeeping")).and_then(Value::as_object).cloned();
            match &stored {
                Some(stmts) => {
                    for st in stmts {
                        tx.execute_batch(st)?;
                    }
                }
                None => match c {
                    Container::SqliteV1 => {
                        tx.execute_batch(SCHEMA_V1)?;
                        tx.execute_batch(DRIZZLE_DDL)?;
                    }
                    _ => {
                        tx.execute_batch(SCHEMA_V2)?;
                        tx.execute_batch(MIGRATION_DDL)?;
                    }
                },
            }
            let mut w = Writer { db: &tx, cols: HashMap::new(), pks: HashMap::new(), seen: BTreeSet::new(), dropped: 0 };
            match (&stored, book) {
                (Some(_), Some(book)) => {
                    for (t, rows) in book {
                        for r in rows.as_array().into_iter().flatten() {
                            w.insert(&t, r, None, None)?;
                        }
                    }
                }
                (Some(_), None) => {}
                (None, _) => match c {
                    Container::SqliteV1 => {
                        for (i, (m, hash)) in MIGRATIONS_V1.iter().enumerate() {
                            let p = &m[..14];
                            let iso = format!("{}-{}-{}T{}:{}:{}Z", &p[0..4], &p[4..6], &p[6..8], &p[8..10], &p[10..12], &p[12..14]);
                            let ms = crate::util::parse_rfc3339_ms(&iso).unwrap_or(0);
                            w.insert("__drizzle_migrations", &json!({"id": i + 1, "hash": hash, "created_at": ms}), None, None)?;
                        }
                    }
                    _ => {
                        let done =
                            sessions.iter().filter_map(|s| s.info.value.get("time").and_then(|t| t.get("created")).and_then(Value::as_i64)).min().unwrap_or(0);
                        for m in MIGRATIONS_V2.lines().filter(|l| !l.trim().is_empty()) {
                            w.insert("migration", &json!({"id": m.trim(), "time_completed": done}), None, None)?;
                        }
                    }
                },
            }
            let stable = if c == Container::SqliteV1 { "session" } else { "session_v2" };
            let mtable = if c == Container::SqliteV1 { "message" } else { "session_message" };
            for sess in sessions {
                if sess.store {
                    for x in &sess.extras {
                        if let Some(n) = x.native.as_ref().filter(|n| n.container == c) {
                            w.insert_row(table_of(&n.file), &n.record, n.raw.as_deref())?;
                        }
                    }
                    continue;
                }
                let same = |n: &Option<NativeRec>| n.as_ref().is_some_and(|n| n.container == c);
                // Bookkeeping rows first: the project row the session references.
                let mut has_project = false;
                let mut has_seq = false;
                let rows = if sess.src == Some(c) { sess.extra.get("rows").and_then(Value::as_object).cloned() } else { None };
                for (t, r) in rows.into_iter().flatten() {
                    has_project |= t == "project";
                    has_seq |= t == "event_sequence";
                    w.insert_row(&t, &r, None)?;
                }
                for x in &sess.extras {
                    if let Some(n) = x.native.as_ref().filter(|n| n.container == c) {
                        w.insert_row(table_of(&n.file), &n.record, n.raw.as_deref())?;
                    }
                }
                let srow = match &sess.info.native {
                    Some(n) if same(&sess.info.native) => n.record.clone(),
                    _ => match c {
                        Container::SqliteV1 => v1_session_to_row(&sess.info.value, &sess.slug, &sess.version),
                        _ => v2_session_to_row(&sess.info.value, &sess.slug, &sess.version),
                    },
                };
                if !has_project {
                    let pid = s(&srow, "project_id").unwrap_or("global").to_string();
                    let t = srow.get("time_created").cloned().unwrap_or(json!(0));
                    let worktree = if pid == "global" { json!("/") } else { srow.get("directory").cloned().unwrap_or(json!("/")) };
                    let row = json!({"id": pid, "worktree": worktree, "time_created": t, "time_updated": t, "time_active": t, "sandboxes": "[]"});
                    w.insert("project", &row, None, Some(pid))?;
                }
                w.insert(stable, &srow, None, None)?;
                let mut last_seq = -1i64;
                for (i, m) in sess.messages.iter().enumerate() {
                    let (row, raw) = match &m.info.native {
                        Some(n) if same(&m.info.native) => (n.record.clone(), n.raw.clone()),
                        _ => match c {
                            Container::SqliteV1 => (v1_message_to_row(&m.info.value, &sess.id), None),
                            _ => (v2_message_to_row(&m.info.value, &sess.id, m.seq.unwrap_or(i as i64 + 1)), None),
                        },
                    };
                    if let Some(sq) = row.get("seq").and_then(Value::as_i64) {
                        last_seq = last_seq.max(sq);
                    }
                    w.insert(mtable, &row, raw.as_deref(), None)?;
                    let mt = m.info.value.get("time").and_then(|t| t.get("created")).cloned().unwrap_or(json!(0));
                    let mid = s(&row, "id").unwrap_or("").to_string();
                    for p in &m.parts {
                        let (row, raw) = match &p.native {
                            Some(n) if same(&p.native) => (n.record.clone(), n.raw.clone()),
                            _ => (v1_part_to_row(&p.value, &mt, &sess.id, &mid), None),
                        };
                        w.insert("part", &row, raw.as_deref(), None)?;
                    }
                }
                if !has_seq && c == Container::SqliteV2 {
                    w.insert("event_sequence", &json!({"aggregate_id": sess.id, "seq": last_seq, "owner_id": null}), None, Some(sess.id.clone()))?;
                }
            }
            losses.add_n(
                "column-dropped",
                None,
                w.dropped,
                "fields the target database's schema has no column for (a 1.x session's agent, model or totals in the 1.2.0 schema)",
            );
            tx.commit()?;
        }
        let bytes = std::fs::read(&path)?;
        let mut out = Output::default();
        out.put(name, bytes);
        Ok(out)
    }
}
