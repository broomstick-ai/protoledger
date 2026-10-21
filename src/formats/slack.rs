//! Slack conversations as conversation input.
//!
//! Slack is not an agent harness: a thread is people (and bots) talking. It is
//! imported so that a Slack discussion can be handed to an agent, and exported
//! so that a thread can be posted back or archived. Three shapes:
//!
//! - `api-replies`: one Web API response object with a `messages` array
//!   (`conversations.replies`, `conversations.history`), or any wrapper object
//!   with one (letta-ai/trajectory's `{channel, users, messages}` input).
//! - `messages-array`: a bare JSON array of message objects (one day file of a
//!   workspace export, or a dump).
//! - `export-dir`: a workspace export directory (`users.json`,
//!   `channels.json`, `<channel>/<YYYY-MM-DD>.json`).
//!
//! A file becomes one ledger; an export directory becomes one ledger per
//! channel plus a `slack:workspace` ledger holding the directory-level files.
//! Messages become `text` (role `assistant` for bot messages, `user` otherwise)
//! and `media` (shared files, by URL) events; who said it is in `meta.slack`.
//! See `docs/formats/slack.md`.

use crate::carry;
use crate::formats::atif::jsonfmt::{self, Style};
use crate::formats::{Detection, ExportOptions, Exported, Format, ImportOptions, Imported, Losses, Output, Variant};
use crate::ledger::{Ledger, LedgerSet, file_safe};
use crate::model::{Agent, Event, Kind, Link, Native, Obj, Parent, SPEC_VERSION, Source};
use crate::util;
use anyhow::{Context, Result, anyhow, bail};
use serde_json::{Map, Value, json};
use std::collections::{BTreeMap, HashMap};
use std::path::Path;

pub const FORMAT: &str = "slack";
const WORKSPACE: &str = "slack:workspace";
const WRAPPER_ID: &str = "response";
/// Message subtypes that carry conversation content. Everything else (joins,
/// topic changes, pins, edit/delete events) is kept as a `native` event.
const CONTENT_SUBTYPES: [&str; 5] = ["bot_message", "thread_broadcast", "file_share", "me_message", "reply_broadcast"];
const USER_ID: &str = "U0PROTOLEDGER";
const BOT_ID: &str = "B0PROTOLEDGER";

static VARIANTS: [Variant; 3] = [
    Variant {
        name: "api-replies",
        versions: "Slack Web API conversations.replies / conversations.history responses; any object with a messages array",
        import: true,
        export: true,
        notes: "One file, one ledger. The response wrapper (ok, has_more, response_metadata, ...) is kept as a native record. Default export.",
    },
    Variant {
        name: "messages-array",
        versions: "a bare JSON array of message objects (an export day file, a dump)",
        import: true,
        export: true,
        notes: "One file, one ledger.",
    },
    Variant {
        name: "export-dir",
        versions: "Slack workspace export directory (users.json, channels.json, <channel>/<YYYY-MM-DD>.json)",
        import: true,
        export: true,
        notes: "One ledger per channel, linked member-of a slack:workspace ledger that holds the directory-level files.",
    },
];

pub struct Slack;

impl Format for Slack {
    fn name(&self) -> &'static str {
        FORMAT
    }
    fn describe(&self) -> &'static str {
        "Slack threads and channel exports (Web API replies JSON, message arrays, workspace export directories)"
    }
    fn variants(&self) -> &'static [Variant] {
        &VARIANTS
    }
    fn detect(&self, input: &Path) -> Option<Detection> {
        if input.is_dir() {
            if is_export_dir(input) {
                return Some(Detection { variant: "export-dir", confidence: 90, version: None });
            }
            return None;
        }
        if input.extension().is_some_and(|x| x != "json") {
            return None;
        }
        let text = std::fs::read_to_string(input).ok()?;
        let v: Value = serde_json::from_str(&text).ok()?;
        match &v {
            Value::Object(o) if o.get("messages").and_then(Value::as_array).is_some_and(|a| looks_like_messages(a)) => {
                let c = if o.contains_key("ok") { 85 } else { 70 };
                Some(Detection { variant: "api-replies", confidence: c, version: None })
            }
            Value::Array(a) if !a.is_empty() && looks_like_messages(a) => Some(Detection { variant: "messages-array", confidence: 70, version: None }),
            _ => None,
        }
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
    let mut im = Importer { set: LedgerSet::new(), losses: Losses::default(), restore: opts.restore_carried };
    let variant = if input.is_dir() && is_export_dir(input) {
        im.import_dir(input)?;
        "export-dir"
    } else if input.is_dir() {
        // A directory of thread files (what exporting several ledgers writes).
        let mut files: Vec<_> =
            std::fs::read_dir(input)?.filter_map(|e| e.ok().map(|e| e.path())).filter(|p| p.is_file() && p.extension().is_some_and(|x| x == "json")).collect();
        files.sort();
        if files.is_empty() {
            bail!("{}: no Slack JSON files", input.display());
        }
        let mut v = "api-replies";
        for f in files {
            v = im.import_file(&f)?;
        }
        v
    } else {
        im.import_file(input)?
    };
    Ok(Imported { set: im.set, losses: im.losses, variant: variant.into() })
}

fn looks_like_messages(a: &[Value]) -> bool {
    a.iter().take(20).any(|m| m.get("ts").is_some_and(Value::is_string) && (m.get("type").is_some() || m.get("text").is_some()))
        && a.iter().all(Value::is_object)
}

fn is_day_file(name: &str) -> bool {
    let b = name.as_bytes();
    name.len() == 15 && name.ends_with(".json") && b[4] == b'-' && b[7] == b'-' && name[..4].bytes().all(|c| c.is_ascii_digit())
}

fn is_export_dir(dir: &Path) -> bool {
    let has_top = ["channels.json", "users.json"].iter().any(|f| dir.join(f).is_file());
    let has_day = std::fs::read_dir(dir).into_iter().flatten().filter_map(|e| e.ok()).any(|e| {
        e.path().is_dir() && std::fs::read_dir(e.path()).into_iter().flatten().filter_map(|f| f.ok()).any(|f| is_day_file(&f.file_name().to_string_lossy()))
    });
    has_top && has_day
}

// ---- timestamps ----

/// `1700000000.000001` → `2023-11-14T22:13:20.000001Z`.
pub fn ts_to_rfc3339(ts: &str) -> Option<String> {
    let (secs, micro) = ts.split_once('.').unwrap_or((ts, "0"));
    let secs: i64 = secs.parse().ok()?;
    if !micro.bytes().all(|c| c.is_ascii_digit()) || micro.len() > 9 {
        return None;
    }
    let mut m = micro.to_string();
    m.truncate(6);
    while m.len() < 6 {
        m.push('0');
    }
    // Checked: a damaged `ts` past i64 milliseconds is no time (it overflowed),
    // and one past year 9999 is not RFC 3339's four-digit year.
    let base = util::ms_to_rfc3339(secs.checked_mul(1000)?);
    (base.len() == 24).then(|| format!("{}.{m}Z", &base[..19]))
}

/// RFC 3339 → Slack ts with microseconds.
pub fn rfc3339_to_ts(ts: &str) -> Option<(i64, u32)> {
    let ms = util::parse_rfc3339_ms(ts)?;
    let secs = ms.div_euclid(1000);
    let frac = ts.get(19..).and_then(|r| r.strip_prefix('.')).unwrap_or("");
    let digits: String = frac.chars().take_while(|c| c.is_ascii_digit()).take(6).collect();
    let micro = if digits.is_empty() { 0 } else { format!("{digits:0<6}").parse().unwrap_or(0) };
    Some((secs, micro))
}

// ---- import ----

struct Importer {
    set: LedgerSet,
    losses: Losses,
    restore: bool,
}

/// Names for user ids, from a users directory and the messages themselves.
#[derive(Default)]
struct Directory {
    names: HashMap<String, String>,
}

impl Directory {
    fn add_users(&mut self, users: Option<&Value>) {
        for u in users.and_then(Value::as_array).into_iter().flatten() {
            let Some(id) = u.get("id").and_then(Value::as_str) else { continue };
            let p = u.get("profile");
            let name = [p.and_then(|p| p.get("display_name")), p.and_then(|p| p.get("real_name")), u.get("real_name"), u.get("name")]
                .into_iter()
                .flatten()
                .filter_map(Value::as_str)
                .find(|s| !s.trim().is_empty());
            if let Some(n) = name {
                self.names.insert(id.to_string(), n.to_string());
            }
        }
    }

    fn name_for(&self, m: &Value) -> Option<String> {
        let id = speaker(m)?;
        if let Some(n) = self.names.get(&id) {
            return Some(n.clone());
        }
        let p = m.get("user_profile");
        [p.and_then(|p| p.get("display_name")), p.and_then(|p| p.get("real_name")), m.get("bot_profile").and_then(|b| b.get("name")), m.get("username")]
            .into_iter()
            .flatten()
            .filter_map(Value::as_str)
            .find(|s| !s.trim().is_empty())
            .map(str::to_string)
    }
}

fn speaker(m: &Value) -> Option<String> {
    m.get("user").or_else(|| m.get("bot_id")).and_then(Value::as_str).filter(|s| !s.is_empty()).map(str::to_string)
}

fn is_bot(m: &Value) -> bool {
    m.get("bot_id").is_some_and(|v| v.as_str().is_some_and(|s| !s.is_empty()))
        || m.get("subtype").and_then(Value::as_str) == Some("bot_message")
        || m.get("bot_profile").is_some_and(Value::is_object)
}

/// Parsed JSON plus its layout (or `None` when no style reproduces it).
fn read_json(p: &Path) -> Result<(String, Value, Option<Style>)> {
    let text = std::fs::read_to_string(p).with_context(|| p.display().to_string())?;
    let v: Value = serde_json::from_str(&text).with_context(|| format!("{} is not JSON", p.display()))?;
    let st = Style::detect(&text, &v);
    Ok((text, v, st))
}

/// The compact texts of the `n` items of a list's compact text, or `None`s
/// when they cannot be cut from it.
fn item_texts(list: Option<&str>, n: usize) -> Vec<Option<String>> {
    match list.and_then(jsonfmt::items).filter(|t| t.len() == n) {
        Some(t) => t.into_iter().map(|x| Some(x.to_string())).collect(),
        None => vec![None; n],
    }
}

fn style_value(st: Option<Style>) -> Value {
    st.map(|s| s.to_value()).unwrap_or(json!("raw"))
}

impl Importer {
    fn import_file(&mut self, p: &Path) -> Result<&'static str> {
        let (text, v, st) = read_json(p)?;
        let name = p.file_name().unwrap().to_string_lossy().to_string();
        let mut dir = Directory::default();
        // Records are held as the source spelled them (SPEC §4): the compact
        // text of the wrapper and of every message, cut from the file's text.
        let compact = jsonfmt::minify(&text);
        let (variant, wrapper, messages, list) = match &v {
            Value::Object(o) => {
                let msgs = o.get("messages").and_then(Value::as_array).cloned().ok_or_else(|| anyhow!("{name}: no messages array"))?;
                dir.add_users(o.get("users"));
                let mut w = v.clone();
                w["messages"] = json!([]);
                let wt = jsonfmt::with_member(&compact, "messages", "[]");
                ("api-replies", Some((w, wt)), msgs, jsonfmt::member(&compact, "messages"))
            }
            Value::Array(a) => ("messages-array", None, a.clone(), Some(compact.as_str())),
            _ => bail!("{name}: not a Slack message list"),
        };
        let texts = item_texts(list, messages.len());
        let channel = v
            .get("channel")
            .and_then(|c| c.as_str().map(str::to_string).or_else(|| c.get("id").and_then(Value::as_str).map(str::to_string)))
            .or_else(|| messages.iter().find_map(|m| m.get("channel").and_then(Value::as_str)).map(str::to_string));
        let threads: Vec<&str> = messages.iter().filter_map(|m| m.get("thread_ts").or_else(|| m.get("ts")).and_then(Value::as_str)).collect();
        let one_thread = !threads.is_empty() && threads.iter().all(|t| *t == threads[0]);
        let stem = name.strip_suffix(".json").unwrap_or(&name).to_string();
        let mut id = match (&channel, one_thread) {
            (Some(c), true) => format!("slack:{c}:{}", threads[0]),
            (Some(c), false) => format!("slack:{c}"),
            (None, true) => format!("slack:{}", threads[0]),
            (None, false) => format!("slack:{stem}"),
        };
        let mut extra = Obj::new();
        extra.insert("file".into(), json!(name));
        extra.insert("json".into(), style_value(st));
        let mut meta = Obj::new();
        let mut sm = Map::new();
        if let Some(c) = &channel {
            sm.insert("channel".into(), json!(c));
        }
        if let Some(n) = v.get("channel_name").and_then(Value::as_str) {
            sm.insert("channel_name".into(), json!(n));
        }
        if !sm.is_empty() {
            meta.insert("slack".into(), Value::Object(sm));
        }
        let mut links = Vec::new();
        let mut created_at = None;
        let mut parent = None;
        if self.restore
            && let Some(h) = v.get(carry::KEY).and_then(|c| c.get("agent"))
        {
            restore_header(h, &mut id, &mut created_at, &mut meta, &mut links, &mut parent);
        }
        let mut evs: Vec<(Event, Option<u64>)> = Vec::new();
        let ours = self.restore && v.get(carry::KEY).is_some();
        if let Some((w, wt)) = wrapper.filter(|_| !ours) {
            let mut e = Event::new(WRAPPER_ID, Kind::Native);
            let mut n = Native::new(FORMAT, Some(WRAPPER_ID.into())).variant(Some(variant));
            jsonfmt::hold(&mut n, wt.as_deref(), &w);
            // With no layout to lay the file out again, `raw` is its whole text.
            if st.is_none() {
                n.raw = Some(text.clone());
            }
            e.native = Some(n);
            evs.push((e, None));
        } else if st.is_none() {
            extra.insert("raw".into(), json!(text));
        }
        for (m, t) in messages.iter().zip(&texts) {
            self.message_events(m, t.as_deref(), variant, None, &dir, &mut evs);
        }
        let first_ts = messages.iter().filter_map(|m| m.get("ts").and_then(Value::as_str)).min_by(|a, b| cmp_ts(a, b)).and_then(ts_to_rfc3339);
        let native_id = if one_thread { Some(threads[0].to_string()) } else { channel.clone() };
        if self.set.get(&id).is_some() {
            id = format!("slack:{stem}");
            let base = id.clone();
            let mut n = 2;
            while self.set.get(&id).is_some() {
                id = format!("{base}~{n}");
                n += 1;
            }
        }
        let created_at = if ours { created_at } else { first_ts };
        let mut l = build_ledger(id, created_at, variant, native_id, extra, meta, links, evs);
        l.agent_mut().parent = parent;
        self.set.insert(l);
        Ok(variant)
    }

    fn import_dir(&mut self, root: &Path) -> Result<()> {
        if !is_export_dir(root) {
            bail!("{}: not a Slack export directory (channels.json/users.json plus <channel>/<date>.json)", root.display());
        }
        let mut dir = Directory::default();
        let mut top: Vec<(String, String, Value, Option<Style>)> = Vec::new();
        let mut channel_dirs = Vec::new();
        let mut stray = 0;
        let mut entries: Vec<_> = std::fs::read_dir(root)?.filter_map(|e| e.ok()).map(|e| e.path()).collect();
        entries.sort();
        for p in entries {
            let name = p.file_name().unwrap().to_string_lossy().to_string();
            if name.starts_with('.') || name == "SOURCE.md" || name == "expect.json" {
                continue;
            }
            if p.is_dir() {
                channel_dirs.push((name, p));
            } else if name.ends_with(".json") {
                let (text, v, st) = read_json(&p)?;
                if name == "users.json" {
                    dir.add_users(Some(&v));
                }
                top.push((name, text, v, st));
            } else {
                stray += 1;
            }
        }
        let channels: Vec<Value> = top
            .iter()
            .filter(|(n, ..)| ["channels.json", "groups.json", "mpims.json", "dms.json"].contains(&n.as_str()))
            .flat_map(|(_, _, v, _)| v.as_array().cloned().unwrap_or_default())
            .collect();

        // The workspace ledger: one native event per directory-level file.
        let mut wevs = Vec::new();
        let mut wextra = Obj::new();
        let mut styles = Map::new();
        for (name, text, v, st) in &top {
            let mut e = Event::new(format!("file:{name}"), Kind::Native);
            let mut n = Native::new(FORMAT, Some(format!("file:{name}"))).variant(Some("export-dir")).file(Some(name.clone()));
            let compact = jsonfmt::minify(text);
            jsonfmt::hold(&mut n, Some(&compact), v);
            // With no layout to lay the file out again, `raw` is its whole text.
            if st.is_none() {
                n.raw = Some(text.clone());
            }
            e.native = Some(n);
            wevs.push((e, None));
            styles.insert(name.clone(), style_value(*st));
        }
        wextra.insert("files".into(), Value::Object(styles));
        let ws = build_ledger(WORKSPACE.into(), None, "export-dir", None, wextra, Obj::new(), Vec::new(), wevs);
        self.set.insert(ws);

        for (cname, cpath) in channel_dirs {
            let mut days: Vec<_> = std::fs::read_dir(&cpath)?.filter_map(|e| e.ok()).map(|e| e.path()).collect();
            days.sort();
            let mut evs = Vec::new();
            let mut styles = Map::new();
            let mut raws = Map::new();
            let mut first_ts: Option<String> = None;
            for d in days {
                let fname = d.file_name().unwrap().to_string_lossy().to_string();
                if !is_day_file(&fname) {
                    stray += 1;
                    continue;
                }
                let rel = format!("{cname}/{fname}");
                let (text, v, st) = read_json(&d)?;
                let Some(arr) = v.as_array() else {
                    stray += 1;
                    continue;
                };
                styles.insert(rel.clone(), style_value(st));
                if st.is_none() {
                    raws.insert(rel.clone(), json!(text));
                }
                if arr.is_empty() {
                    // Keep the (empty) file: a marker event.
                    let mut e = Event::new(format!("{rel}@empty"), Kind::Native);
                    e.native = Some(Native::new(FORMAT, Some(format!("{rel}@empty"))).variant(Some("export-dir")).file(Some(rel.clone())));
                    evs.push((e, None));
                }
                let compact = jsonfmt::minify(&text);
                let texts = item_texts(Some(&compact), arr.len());
                for (m, t) in arr.iter().zip(&texts) {
                    if first_ts.is_none() {
                        first_ts = m.get("ts").and_then(Value::as_str).and_then(ts_to_rfc3339);
                    }
                    self.message_events(m, t.as_deref(), "export-dir", Some(&rel), &dir, &mut evs);
                }
            }
            let info = channels.iter().find(|c| c.get("name").and_then(Value::as_str) == Some(cname.as_str()));
            let mut sm = Map::new();
            if let Some(cid) = info.and_then(|c| c.get("id")).and_then(Value::as_str) {
                sm.insert("channel".into(), json!(cid));
            }
            sm.insert("channel_name".into(), json!(cname));
            let mut meta = Obj::new();
            meta.insert("slack".into(), Value::Object(sm));
            let mut extra = Obj::new();
            extra.insert("dir".into(), json!(cname));
            extra.insert("files".into(), Value::Object(styles));
            if !raws.is_empty() {
                extra.insert("raw".into(), Value::Object(raws));
            }
            let link = Link { rel: "member-of".into(), agent: Some(WORKSPACE.into()), event: None, extra: Obj::new() };
            let native_id = info.and_then(|c| c.get("id")).and_then(Value::as_str).map(str::to_string);
            let l = build_ledger(format!("slack:{cname}"), first_ts, "export-dir", native_id, extra, meta, vec![link], evs);
            self.set.insert(l);
        }
        self.losses.add_n("file-not-imported", None, stray, "a file in the export that is not JSON or not a day file");
        Ok(())
    }

    /// The events one message makes (or the events it carried).
    fn message_events(&mut self, m: &Value, text: Option<&str>, variant: &str, file: Option<&str>, dir: &Directory, out: &mut Vec<(Event, Option<u64>)>) {
        if self.restore {
            let mut rec = m.clone();
            if let Some(carried) = carry::take(&mut rec, &[]) {
                out.extend(carried.into_iter().map(|e| {
                    let s = e.seq;
                    (e, Some(s))
                }));
                return;
            }
        }
        let ts = m.get("ts").and_then(Value::as_str).unwrap_or("").to_string();
        let nid = if ts.is_empty() { format!("@{}", out.len()) } else { ts.clone() };
        let when = ts_to_rfc3339(&ts);
        let subtype = m.get("subtype").and_then(Value::as_str);
        let content = m.get("type").and_then(Value::as_str).is_none_or(|t| t == "message") && subtype.is_none_or(|s| CONTENT_SUBTYPES.contains(&s));
        let role = if is_bot(m) { "assistant" } else { "user" };
        let mut parts: Vec<(Kind, Obj)> = Vec::new();
        if content {
            if let Some(t) = m.get("text").and_then(Value::as_str).filter(|t| !t.is_empty()) {
                parts.push((Kind::Text, obj(json!({"role": role, "text": t}))));
            }
            for f in m.get("files").and_then(Value::as_array).into_iter().flatten() {
                let url = ["url_private", "permalink", "url_private_download"].iter().find_map(|k| f.get(*k).and_then(Value::as_str));
                let Some(url) = url else {
                    self.losses.add("file-without-url", Some(Kind::Media), "a shared file with no URL (kept only in native)");
                    continue;
                };
                let mut b = obj(json!({"role": role}));
                let mt = f.get("mimetype").and_then(Value::as_str).unwrap_or("application/octet-stream");
                b.insert("media_type".into(), json!(mt));
                b.insert("url".into(), json!(url));
                if let Some(n) = f.get("name").or_else(|| f.get("title")).and_then(Value::as_str) {
                    b.insert("name".into(), json!(n));
                }
                parts.push((Kind::Media, b));
            }
        }
        if parts.is_empty() {
            parts.push((Kind::Native, Obj::new()));
        }
        let mut sm = Map::new();
        if let Some(u) = m.get("user").and_then(Value::as_str) {
            sm.insert("user".into(), json!(u));
        }
        if let Some(b) = m.get("bot_id").and_then(Value::as_str) {
            sm.insert("bot_id".into(), json!(b));
        }
        if let Some(n) = dir.name_for(m) {
            sm.insert("name".into(), json!(n));
        }
        if let Some(s) = subtype {
            sm.insert("subtype".into(), json!(s));
        }
        let thread = m.get("thread_ts").and_then(Value::as_str).map(str::to_string);
        for (i, (kind, body)) in parts.into_iter().enumerate() {
            let id = if i == 0 { nid.clone() } else { format!("{nid}#{i}") };
            let mut e = Event::new(id, kind);
            e.body = body;
            e.ts = when.clone();
            e.turn = thread.clone();
            if kind != Kind::Native {
                e.group = Some(nid.clone());
            }
            if !sm.is_empty() {
                e.meta.insert("slack".into(), Value::Object(sm.clone()));
            }
            let mut n = Native::new(FORMAT, Some(nid.clone())).variant(Some(variant.to_string())).file(file.map(str::to_string));
            if i == 0 {
                jsonfmt::hold(&mut n, text, m);
            } else {
                n.part = i as u32;
            }
            e.native = Some(n);
            out.push((e, None));
        }
    }
}

fn restore_header(h: &Value, id: &mut String, created_at: &mut Option<String>, meta: &mut Obj, links: &mut Vec<Link>, parent: &mut Option<Parent>) {
    if let Some(i) = h.get("id").and_then(Value::as_str) {
        *id = i.to_string();
    }
    *created_at = h.get("created_at").and_then(Value::as_str).map(str::to_string);
    if let Some(m) = h.get("meta").and_then(Value::as_object) {
        *meta = m.clone();
    }
    if let Some(ls) = h.get("links").and_then(|l| serde_json::from_value::<Vec<Link>>(l.clone()).ok()) {
        links.extend(ls);
    }
    *parent = h.get("parent").and_then(|p| serde_json::from_value::<Parent>(p.clone()).ok());
}

#[allow(clippy::too_many_arguments)]
fn build_ledger(
    id: String,
    created_at: Option<String>,
    variant: &str,
    native_id: Option<String>,
    extra: Obj,
    meta: Obj,
    links: Vec<Link>,
    mut evs: Vec<(Event, Option<u64>)>,
) -> Ledger {
    // Restored events come back in their original order within each run.
    let mut i = 0;
    while i < evs.len() {
        if evs[i].1.is_none() {
            i += 1;
            continue;
        }
        let mut j = i;
        while j < evs.len() && evs[j].1.is_some() {
            j += 1;
        }
        evs[i..j].sort_by_key(|(_, s)| s.unwrap());
        i = j;
    }
    let agent = Agent {
        id,
        created_at,
        source: Some(Source { format: FORMAT.into(), variant: Some(variant.into()), version: None, native_id, extra }),
        parent: None,
        links,
        meta,
    };
    let mut l = Ledger::new(agent);
    for (e, _) in evs {
        l.append_unique(e);
    }
    l
}

fn cmp_ts(a: &str, b: &str) -> std::cmp::Ordering {
    let k = |s: &str| {
        let (x, y) = s.split_once('.').unwrap_or((s, "0"));
        (x.parse::<i64>().unwrap_or(0), format!("{y:0<6}"))
    };
    k(a).cmp(&k(b))
}

fn obj(v: Value) -> Obj {
    match v {
        Value::Object(m) => m,
        _ => Obj::new(),
    }
}

// ---- export ----

fn is_native(l: &Ledger) -> bool {
    l.agent().source.as_ref().is_some_and(|s| s.format == FORMAT)
}

fn extra<'a>(l: &'a Ledger, k: &str) -> Option<&'a Value> {
    l.agent().source.as_ref()?.extra.get(k)
}

fn style_of(v: Option<&Value>) -> Option<Style> {
    v.and_then(Style::from_value)
}

fn export(set: &LedgerSet, opts: &ExportOptions) -> Result<Exported> {
    let variant = match opts.variant.as_deref() {
        None => None,
        Some(v) if VARIANTS.iter().any(|x| x.name == v) => Some(v.to_string()),
        Some(v) => bail!("unknown slack variant {v:?} (api-replies, messages-array, export-dir)"),
    };
    let mut out = Output::default();
    let mut losses = Losses::default();
    let natives: Vec<&Ledger> = set.iter().filter(|l| is_native(l)).collect();
    let src_variant = natives.first().and_then(|l| l.agent().source.as_ref()?.variant.clone());
    let want = variant.clone().or(src_variant).unwrap_or_else(|| "api-replies".into());
    if want == "export-dir" {
        export_dir(set, opts, &mut out, &mut losses)?;
    } else {
        for l in set.iter() {
            if l.id() == WORKSPACE {
                continue;
            }
            let name = if is_native(l) && extra(l, "dir").is_none() { extra(l, "file").and_then(Value::as_str).map(str::to_string) } else { None }
                .unwrap_or_else(|| format!("{}.json", file_safe(l.id())));
            let mut r = Render::new(l, opts.carry);
            let (msgs, wrapper) = r.messages(&mut losses);
            let exact = r.exact;
            let wrap = want != "messages-array";
            let doc = if wrap {
                let mut w = wrapper.unwrap_or_else(|| json!({"ok": true, "messages": [], "has_more": false}));
                w["messages"] = Value::Array(msgs);
                if opts.carry && !exact {
                    attach_header(&mut w, l);
                }
                w
            } else {
                if wrapper.is_some() {
                    losses.add("wrapper-dropped", None, "the API response fields around messages");
                }
                Value::Array(msgs)
            };
            let same_shape = is_native(l) && (wrap == r.had_wrapper);
            // The records' own texts, laid out again when nothing changed.
            let list = r.texts.iter().cloned().collect::<Option<Vec<String>>>().map(|t| jsonfmt::array_text(t.iter().map(String::as_str)));
            let own = match (wrap, list) {
                (true, Some(list)) => r.wrapper_text.as_deref().and_then(|w| jsonfmt::with_member(w, "messages", &list)),
                (false, list) => list,
                _ => None,
            };
            let text =
                render_file(&doc, exact && same_shape, r.raw.as_deref(), own, style_of(extra(l, "json")).filter(|_| same_shape), Style::COMPACT, &mut losses);
            out.put(name, text.into_bytes());
        }
    }
    Ok(Exported { output: out, losses, variant: want })
}

/// A file's text: an unchanged document laid out from its records' own
/// compact text (`own`) in its own style, else the whole file's `raw` text
/// when it still says the same, else `doc` rendered.
fn render_file(doc: &Value, exact: bool, raw: Option<&str>, own: Option<String>, style: Option<Style>, fallback: Style, losses: &mut Losses) -> String {
    if exact
        && let (Some(st), Some(own)) = (style, own)
        && serde_json::from_str::<Value>(&own).ok().as_ref() == Some(doc)
    {
        return st.relay(&own);
    }
    // A whole file's `raw` only while it still says what the records say.
    if exact
        && let Some(raw) = raw
        && crate::util::same_json(raw, &serde_json::to_string(doc).expect("json"))
    {
        return raw.to_string();
    }
    if exact {
        // Unchanged, yet neither its records' texts nor its whole text could
        // be written: the parsed value is.
        losses.add(jsonfmt::EXACT_TEXT_FALLBACK, None, jsonfmt::EXACT_TEXT_FALLBACK_DETAIL);
    }
    style.unwrap_or(fallback).render(doc)
}

fn attach_header(w: &mut Value, l: &Ledger) {
    let a = l.agent();
    let mut h = Map::new();
    h.insert("id".into(), json!(a.id));
    if let Some(c) = &a.created_at {
        h.insert("created_at".into(), json!(c));
    }
    if !a.meta.is_empty() {
        h.insert("meta".into(), Value::Object(a.meta.clone()));
    }
    if !a.links.is_empty() {
        h.insert("links".into(), serde_json::to_value(&a.links).unwrap_or_default());
    }
    if let Some(p) = &a.parent {
        h.insert("parent".into(), serde_json::to_value(p).unwrap_or_default());
    }
    w[carry::KEY] = json!({"v": SPEC_VERSION, "agent": h});
}

/// Slack's export layout: 4-space indent, escaped slashes, ASCII escapes.
const EXPORT_STYLE: Style = Style { indent: Some(4), spaced: false, ascii: true, slash: true, newline: false, numbers_inline: false };

fn export_dir(set: &LedgerSet, opts: &ExportOptions, out: &mut Output, losses: &mut Losses) -> Result<()> {
    let ws = set.get(WORKSPACE).filter(|l| is_native(l));
    if let Some(ws) = ws {
        let styles = extra(ws, "files");
        for e in &ws.events {
            let Some(n) = e.native.as_ref().filter(|n| n.format == FORMAT) else { continue };
            let (Some(f), Some(rec)) = (&n.file, &n.record_json()) else { continue };
            let st = style_of(styles.and_then(|s| s.get(f)));
            // With a layout, `raw` is the record's own text; without, the file's.
            let (raw, own) = match st {
                Some(_) => (None, crate::util::native_text(n)),
                None => (n.raw.as_deref(), None),
            };
            out.put(f.clone(), render_file(rec, true, raw, own, st, EXPORT_STYLE, losses).into_bytes());
        }
    }
    let mut channels = Vec::new();
    for l in set.iter() {
        if l.id() == WORKSPACE {
            continue;
        }
        let native_dir = if is_native(l) { extra(l, "dir").and_then(Value::as_str).map(str::to_string) } else { None };
        let cname = native_dir.clone().unwrap_or_else(|| file_safe(l.id().strip_prefix("slack:").unwrap_or(l.id())));
        let mut r = Render::new(l, opts.carry);
        r.day_files = true;
        let (msgs, _) = r.messages(losses);
        if native_dir.is_none() {
            let created = msgs.first().and_then(|m| m.get("ts")).and_then(Value::as_str).and_then(|t| t.split('.').next()?.parse::<i64>().ok());
            channels.push(json!({"id": format!("C{}", util::short_id(l.id(), 10).to_uppercase()), "name": cname, "created": created.unwrap_or(0)}));
        }
        // Group into day files: native ones keep their file, rendered ones go by UTC date.
        let mut files: BTreeMap<String, Vec<Value>> = BTreeMap::new();
        let mut texts: BTreeMap<String, Vec<Option<String>>> = BTreeMap::new();
        for ((m, f), t) in msgs.into_iter().zip(r.files).zip(r.texts) {
            let f = f.unwrap_or_else(|| {
                let date = m.get("ts").and_then(Value::as_str).and_then(ts_to_rfc3339).map(|t| t[..10].to_string()).unwrap_or_else(|| "1970-01-01".into());
                format!("{cname}/{date}.json")
            });
            texts.entry(f.clone()).or_default().push(t);
            files.entry(f).or_default().push(m);
        }
        for f in r.empty_files {
            files.entry(f).or_default();
        }
        let styles = extra(l, "files");
        let raws = extra(l, "raw");
        for (f, msgs) in files {
            let st = style_of(styles.and_then(|s| s.get(&f)));
            let raw = raws.and_then(|r| r.get(&f)).and_then(Value::as_str);
            let own = texts.remove(&f).unwrap_or_default().into_iter().collect::<Option<Vec<String>>>();
            let own = own.map(|t| jsonfmt::array_text(t.iter().map(String::as_str)));
            out.put(f, render_file(&Value::Array(msgs), r.exact, raw, own, st, EXPORT_STYLE, losses).into_bytes());
        }
    }
    if ws.is_none() {
        let users = json!([
            {"id": USER_ID, "name": "user", "real_name": "User", "is_bot": false},
            {"id": BOT_ID, "name": "assistant", "real_name": "Assistant", "is_bot": true}
        ]);
        out.put("users.json", EXPORT_STYLE.render(&users).into_bytes());
        out.put("channels.json", EXPORT_STYLE.render(&Value::Array(channels)).into_bytes());
    } else if !channels.is_empty() {
        losses.add("channel-not-listed", None, "a rendered channel is not added to the native channels.json");
    }
    Ok(())
}

/// Turns one ledger into Slack messages.
struct Render<'l> {
    l: &'l Ledger,
    carry: bool,
    exact: bool,
    had_wrapper: bool,
    raw: Option<String>,
    /// The wrapper's own compact text, and each returned message's (native
    /// ones; `None` for rendered ones), as the source spelled them.
    wrapper_text: Option<String>,
    texts: Vec<Option<String>>,
    day_files: bool,
    /// The day file of each returned message (native ones), in order.
    files: Vec<Option<String>>,
    empty_files: Vec<String>,
}

impl<'l> Render<'l> {
    fn new(l: &'l Ledger, carry: bool) -> Self {
        Render {
            l,
            carry,
            exact: is_native(l),
            had_wrapper: false,
            raw: None,
            wrapper_text: None,
            texts: Vec::new(),
            day_files: false,
            files: Vec::new(),
            empty_files: Vec::new(),
        }
    }

    /// The messages, and the API wrapper object if the ledger has one.
    fn messages(&mut self, losses: &mut Losses) -> (Vec<Value>, Option<Value>) {
        let mut msgs: Vec<Value> = Vec::new();
        let mut carried: Vec<Vec<Event>> = Vec::new();
        let mut rendered: Vec<bool> = Vec::new();
        let mut wrapper = None;
        let mut pending: Vec<Event> = Vec::new();
        let mut last_ts: (i64, u32) = (0, 0);
        let mut thread: Option<String> = None;
        let mut groups: HashMap<String, usize> = HashMap::new();
        if !is_native(self.l) {
            self.exact = false;
        } else if let Some(r) = extra(self.l, "raw").and_then(Value::as_str) {
            self.raw = Some(r.to_string());
        }
        // SPEC §2.3: a thread is one list of messages; the active path is
        // written, and an abandoned event rides on the next message.
        let own = |e: &Event| e.native.as_ref().is_some_and(|n| n.format == FORMAT);
        let on = crate::formats::active_path_only(self.l, losses, "with carry they ride on the next message", own);
        for (e, on) in self.l.events.iter().zip(on) {
            if !on && !own(e) {
                pending.push(e.clone());
                continue;
            }
            if let Some(n) = e.native.as_ref().filter(|n| n.format == FORMAT) {
                if n.id.as_deref() == Some(WRAPPER_ID) {
                    wrapper = n.record_json();
                    self.had_wrapper = true;
                    // With no layout, `raw` is the whole file's text; with
                    // one, the wrapper's own.
                    if extra(self.l, "json").and_then(Value::as_str) == Some("raw") {
                        self.raw = n.raw.clone();
                        self.wrapper_text = n.record_text().map(str::to_string);
                    } else {
                        self.wrapper_text = crate::util::native_text(n);
                    }
                    continue;
                }
                if n.id.as_deref().is_some_and(|i| i.ends_with("@empty")) {
                    if let Some(f) = &n.file {
                        self.empty_files.push(f.clone());
                    }
                    continue;
                }
                if n.part == 0
                    && let Some(rec) = &n.record_json()
                {
                    if let Some(t) = rec.get("ts").and_then(Value::as_str).and_then(|t| {
                        let (a, b) = t.split_once('.')?;
                        Some((a.parse().ok()?, format!("{b:0<6}")[..6].parse().ok()?))
                    }) {
                        last_ts = last_ts.max(t);
                    }
                    if thread.is_none() {
                        thread = rec.get("thread_ts").and_then(Value::as_str).map(str::to_string);
                    }
                    msgs.push(rec.clone());
                    carried.push(Vec::new());
                    rendered.push(false);
                    self.files.push(n.file.clone());
                    self.texts.push(crate::util::native_text(n));
                }
                continue;
            }
            self.exact = false;
            // Rendered from neutral fields.
            let role = e.str("role");
            let as_message = matches!(e.kind, Kind::Text) && matches!(role, Some("user") | Some("assistant")) || e.kind == Kind::PeerMessage;
            let media = e.kind == Kind::Media && e.body.get("url").and_then(Value::as_str).is_some();
            if as_message || media {
                let joined = e.group.as_ref().and_then(|g| groups.get(g).copied()).filter(|i| rendered[*i]);
                let i = match joined {
                    Some(i) => i,
                    None => {
                        let ts = next_ts(e.ts.as_deref(), &mut last_ts);
                        let bot = if e.kind == Kind::PeerMessage { e.str("direction") == Some("out") } else { role == Some("assistant") };
                        let sm = e.meta.get("slack");
                        let mut m = Map::new();
                        m.insert("type".into(), json!("message"));
                        if bot {
                            m.insert("subtype".into(), json!("bot_message"));
                            let b = sm.and_then(|s| s.get("bot_id")).and_then(Value::as_str).unwrap_or(BOT_ID);
                            m.insert("bot_id".into(), json!(b));
                            m.insert("username".into(), json!(sm.and_then(|s| s.get("name")).and_then(Value::as_str).unwrap_or("assistant")));
                        } else {
                            let u = sm.and_then(|s| s.get("user")).and_then(Value::as_str).unwrap_or(USER_ID);
                            m.insert("user".into(), json!(u));
                        }
                        m.insert("text".into(), json!(""));
                        m.insert("ts".into(), json!(ts));
                        let th = thread.get_or_insert_with(|| ts.clone()).clone();
                        if !self.day_files {
                            m.insert("thread_ts".into(), json!(th));
                        }
                        msgs.push(Value::Object(m));
                        carried.push(std::mem::take(&mut pending));
                        rendered.push(true);
                        self.files.push(None);
                        self.texts.push(None);
                        let i = msgs.len() - 1;
                        if let Some(g) = &e.group {
                            groups.insert(g.clone(), i);
                        }
                        i
                    }
                };
                let m = &mut msgs[i];
                if media {
                    let mut f = Map::new();
                    if let Some(n) = e.str("name") {
                        f.insert("name".into(), json!(n));
                    }
                    if let Some(t) = e.str("media_type") {
                        f.insert("mimetype".into(), json!(t));
                    }
                    f.insert("url_private".into(), json!(e.str("url")));
                    let files = m.as_object_mut().unwrap().entry("files").or_insert_with(|| json!([]));
                    files.as_array_mut().unwrap().push(Value::Object(f));
                    if m.get("text").and_then(Value::as_str) == Some("") && m.get("subtype").is_none() {
                        m["subtype"] = json!("file_share");
                    }
                } else {
                    let t = if e.kind == Kind::PeerMessage { crate::formats::pi_session::peer_wrap(e) } else { e.str("text").unwrap_or("").to_string() };
                    let cur = m["text"].as_str().unwrap_or("").to_string();
                    m["text"] = json!(if cur.is_empty() { t } else { format!("{cur}\n{t}") });
                    if e.kind == Kind::PeerMessage && crate::formats::pi_session::peer_has_more(e) {
                        losses.add(
                            crate::formats::pi_session::PEER_FIELDS_DROPPED,
                            Some(Kind::PeerMessage),
                            crate::formats::pi_session::PEER_FIELDS_DROPPED_DETAIL,
                        );
                    }
                }
                carried[i].push(e.clone());
                continue;
            }
            let code = match e.kind {
                Kind::Text => "system-message-dropped",
                Kind::Media => "media-dropped",
                Kind::Reasoning => "reasoning-dropped",
                Kind::ToolCall => "tool-call-dropped",
                Kind::ToolResult => "tool-result-dropped",
                Kind::Subagent => "subagent-dropped",
                Kind::Compaction => "compaction-dropped",
                Kind::Usage => "usage-dropped",
                Kind::Settings => "settings-dropped",
                Kind::Title => "title-dropped",
                Kind::Turn => "turn-dropped",
                Kind::Error => "error-dropped",
                Kind::FileChange => "file-change-dropped",
                Kind::App => "app-dropped",
                _ => "native-dropped",
            };
            losses.add(code, Some(e.kind), "a Slack thread holds only messages and files; carried when carry is on");
            match rendered.iter().rposition(|r| *r) {
                Some(i) => carried[i].push(e.clone()),
                None => pending.push(e.clone()),
            }
        }
        if !pending.is_empty() {
            if let Some(i) = rendered.iter().position(|r| *r) {
                let mut p = std::mem::take(&mut pending);
                p.append(&mut carried[i]);
                carried[i] = p;
            } else {
                // Nothing renderable at all: an empty message holds the carrier.
                let ts = next_ts(self.l.agent().created_at.as_deref(), &mut last_ts);
                msgs.push(json!({"type": "message", "user": USER_ID, "text": "", "ts": ts}));
                carried.push(std::mem::take(&mut pending));
                rendered.push(true);
                self.files.push(None);
                self.texts.push(None);
                losses.add("empty-thread-padded", None, "an empty message holds events with no Slack form");
            }
        }
        if self.carry {
            for (m, c) in msgs.iter_mut().zip(carried) {
                carry::attach(m, &[], &c);
            }
        }
        (msgs, wrapper)
    }
}

/// A unique, increasing Slack ts for an event time.
fn next_ts(ts: Option<&str>, last: &mut (i64, u32)) -> String {
    let mut t = ts.and_then(rfc3339_to_ts).unwrap_or(*last);
    if t <= *last {
        t = if last.1 >= 999_999 { (last.0 + 1, 0) } else { (last.0, last.1 + 1) };
    }
    *last = t;
    format!("{}.{:06}", t.0, t.1)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn timestamps() {
        assert_eq!(ts_to_rfc3339("1700000000.000001").as_deref(), Some("2023-11-14T22:13:20.000001Z"));
        // Found by the mutation harness (seed 0x2026101905, slack/letta-thread): a
        // `ts` past i64 milliseconds overflowed; past year 9999 it is no RFC
        // 3339 time either. Opposed by the last second of year 9999.
        assert_eq!(ts_to_rfc3339("17000000001700000000.000001"), None);
        assert_eq!(ts_to_rfc3339("9223372036854775807"), None);
        assert_eq!(ts_to_rfc3339("253402300800"), None);
        assert_eq!(ts_to_rfc3339("253402300799").as_deref(), Some("9999-12-31T23:59:59.000000Z"));
        assert_eq!(rfc3339_to_ts("2023-11-14T22:13:20.000001Z"), Some((1700000000, 1)));
        assert_eq!(rfc3339_to_ts("2023-11-14T23:13:20.5+01:00"), Some((1700000000, 500000)));
        let mut last = (0, 0);
        assert_eq!(next_ts(Some("2023-11-14T22:13:20Z"), &mut last), "1700000000.000000");
        assert_eq!(next_ts(Some("2023-11-14T22:13:20Z"), &mut last), "1700000000.000001");
        assert_eq!(next_ts(None, &mut last), "1700000000.000002");
    }

    fn import_str(text: &str) -> Imported {
        let dir = tempfile::tempdir().unwrap();
        let p = dir.path().join("t.json");
        std::fs::write(&p, text).unwrap();
        Slack.import(&p, &ImportOptions::standard()).unwrap()
    }

    /// A response spelled the way a parsed `Value` does not re-spell it under
    /// any feature set (keys out of order, the escape `\u00e9` beside a raw
    /// `ü`, `1e3`, `0.000005`, Python's spaced separators) is held as its
    /// records' own texts and laid out again from them byte for byte.
    #[test]
    fn records_are_held_and_reemitted_as_spelled() {
        let text = "{\"ok\": true, \"messages\": [{\"type\": \"message\", \"user\": \"U1\", \"text\": \"caf\\u00e9 \u{fc}\", \"ts\": \"1700000000.000001\", \"n\": 1e3, \"x\": 0.000005}], \"has_more\": false}";
        let im = import_str(text);
        let l = im.set.iter().next().unwrap();
        let held: Vec<&str> = l.events.iter().filter_map(|e| e.native.as_ref()).filter(|n| n.part == 0).filter_map(|n| n.record_text()).collect();
        assert_eq!(
            held,
            [
                r#"{"ok":true,"messages":[],"has_more":false}"#,
                "{\"type\":\"message\",\"user\":\"U1\",\"text\":\"caf\\u00e9 \u{fc}\",\"ts\":\"1700000000.000001\",\"n\":1e3,\"x\":0.000005}",
            ]
        );
        let ex = export(&im.set, &ExportOptions::standard()).unwrap();
        assert_eq!(std::str::from_utf8(&ex.output.files["t.json"]).unwrap(), text);
        // From the records' own texts alone (no `raw`), too.
        let ex = export(&jsonfmt::without_raw(&im.set), &ExportOptions::standard()).unwrap();
        assert_eq!(std::str::from_utf8(&ex.output.files["t.json"]).unwrap(), text);
    }

    /// An unchanged record whose `raw` is a spelling within
    /// `util::NUMBER_ULPS` of its record (so `current_raw` writes it) cannot be
    /// laid back into the document it came from: the rebuilt text no longer
    /// parses to the document's value, the exporter falls back to the value,
    /// and that is named.
    #[test]
    fn an_unchanged_document_not_rebuilt_from_its_texts_names_the_fallback() {
        let text = "{\"ok\": true, \"messages\": [{\"type\": \"message\", \"user\": \"U1\", \"text\": \"hi\", \"ts\": \"1700000000.000001\", \"x\": 0.000005}], \"has_more\": false}";
        let set = import_str(text).set;
        let has = |ex: &Exported| ex.losses.0.iter().any(|l| l.code == jsonfmt::EXACT_TEXT_FALLBACK);
        // Opposed: as imported, it is rebuilt from its texts and names nothing.
        let ex = export(&set, &ExportOptions::standard()).unwrap();
        assert_eq!(std::str::from_utf8(&ex.output.files["t.json"]).unwrap(), text);
        assert!(!has(&ex), "{:?}", ex.losses);
        let ex = export(&nudged(&set, "0.000005", "5.000000000000001e-06"), &ExportOptions::standard()).unwrap();
        assert!(has(&ex), "the fallback is named: {:?}", ex.losses);
    }

    fn nudged(set: &LedgerSet, from: &str, to: &str) -> LedgerSet {
        let mut set = set.clone();
        let id = set.iter().next().unwrap().id().to_string();
        let l = set.ledgers.get_mut(&id).unwrap();
        let n = l
            .events
            .iter_mut()
            .filter_map(|e| e.native.as_mut())
            .find(|n| n.part == 0 && n.record_text().is_some_and(|t| t.contains(from)))
            .expect("the record to nudge");
        n.raw = Some(n.record_text().unwrap().replace(from, to));
        assert_eq!(crate::util::current_raw(n), n.raw.as_deref(), "sentinel: the nudged raw still stands for the record");
        set
    }

    #[test]
    fn messages_map_by_speaker_kind() {
        let im = import_str(
            r#"{"ok":true,"messages":[
            {"type":"message","user":"U1","text":"hi <@U2>","ts":"1700000000.000001","thread_ts":"1700000000.000001","reply_count":2},
            {"type":"message","subtype":"bot_message","bot_id":"B1","username":"helper","text":"on it","ts":"1700000001.000001","thread_ts":"1700000000.000001",
             "files":[{"name":"a.png","mimetype":"image/png","url_private":"https://files.slack.com/a.png"}]},
            {"type":"message","subtype":"channel_join","user":"U3","text":"joined","ts":"1700000002.000001"}],"has_more":false}"#,
        );
        assert_eq!(im.variant, "api-replies");
        let l = im.set.iter().next().unwrap();
        let kinds: Vec<&str> = l.events.iter().map(|e| e.kind.as_str()).collect();
        assert_eq!(kinds, ["native", "text", "text", "media", "native"]);
        assert_eq!(l.events[1].str("role"), Some("user"));
        assert_eq!(l.events[2].str("role"), Some("assistant"));
        assert_eq!(l.events[2].meta["slack"]["name"], json!("helper"));
        assert_eq!(l.events[3].id, "1700000001.000001#1");
        assert_eq!(l.events[1].turn.as_deref(), Some("1700000000.000001"));
        assert_eq!(l.events[1].ts.as_deref(), Some("2023-11-14T22:13:20.000001Z"));
    }

    #[test]
    fn foreign_ledger_becomes_a_thread_and_carries_the_rest() {
        let mut l = Ledger::new(Agent { id: "a1".into(), created_at: None, source: None, parent: None, links: vec![], meta: Obj::new() });
        let mut add = |id: &str, kind: Kind, body: Value| {
            let mut e = Event::new(id, kind).ts(Some("2026-01-01T00:00:00Z"));
            e.body = obj(body);
            l.append(e).unwrap();
        };
        add("u", Kind::Text, json!({"role": "user", "text": "fix it"}));
        add("c", Kind::ToolCall, json!({"call_id": "x", "name": "Bash", "input": {}}));
        add("t", Kind::Text, json!({"role": "assistant", "text": "fixed"}));
        let set = LedgerSet::single(l);
        let ex = export(&set, &ExportOptions::no_carry()).unwrap();
        let doc: Value = serde_json::from_slice(&ex.output.files["a1.json"]).unwrap();
        let m = doc["messages"].as_array().unwrap();
        assert_eq!(m.len(), 2);
        assert_eq!(m[1]["ts"], json!("1767225600.000001"));
        assert_eq!(m[1]["thread_ts"], m[0]["ts"]);
        assert_eq!(ex.losses.0[0].code, "tool-call-dropped");
        let ex = export(&set, &ExportOptions::standard()).unwrap();
        let dir = tempfile::tempdir().unwrap();
        let p = crate::roundtrip::materialise(&ex.output, dir.path()).unwrap();
        let back = Slack.import(&p, &ImportOptions::standard()).unwrap().set;
        let d = crate::loss::compare(&set, &back, crate::loss::CompareOptions { ignore_native: true, ..Default::default() });
        assert!(d.is_clean(), "{}", d.summary());
    }
}
