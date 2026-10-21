//! Copilot CLI <= 0.0.341: `~/.copilot/history-session-state/session_<id>_<ms>.json`,
//! `JSON.stringify({sessionId, startTime, chatMessages, timeline, selectedModel?}, null, 2)`.
//! `chatMessages` is the OpenAI-format model history; `timeline` the UI's view
//! of it (with timestamps).

use super::{NAME, V_LEGACY, native, output_text, own, push_record, session_id, uuid};
use crate::formats::atif::jsonfmt;
use crate::formats::gemini_cli::pretty;
use crate::formats::{ExportOptions, Exported, ImportOptions, Imported, Losses, Output};
use crate::ledger::{Ledger, LedgerSet};
use crate::model::{Agent, Audience, Event, Kind, Obj, Source};
use crate::util::{self, get_str};
use anyhow::{Context, Result, bail};
use serde_json::{Value, json};
use std::collections::HashMap;
use std::path::Path;

pub(super) fn import(input: &Path, _opts: &ImportOptions) -> Result<Imported> {
    let mut set = LedgerSet::new();
    let files: Vec<(String, std::path::PathBuf)> = if input.is_file() {
        vec![(input.file_name().unwrap().to_string_lossy().to_string(), input.to_path_buf())]
    } else {
        walkdir::WalkDir::new(input)
            .sort_by_file_name()
            .into_iter()
            .flatten()
            .filter(|e| e.file_type().is_file() && e.file_name().to_string_lossy().ends_with(".json"))
            .map(|e| (e.path().strip_prefix(input).unwrap().to_string_lossy().replace('\\', "/"), e.path().to_path_buf()))
            .collect()
    };
    for (rel, path) in files {
        let text = std::fs::read_to_string(&path)?;
        let doc: Value = serde_json::from_str(&text).with_context(|| path.display().to_string())?;
        if doc.get("chatMessages").is_none() && doc.get("timeline").is_none() {
            continue;
        }
        set.insert(import_doc(&rel, &text, &doc));
    }
    if set.is_empty() {
        bail!("{}: no legacy Copilot CLI session found", input.display());
    }
    Ok(Imported { set, losses: Losses::default(), variant: V_LEGACY.into() })
}

fn import_doc(rel: &str, text: &str, doc: &Value) -> Ledger {
    let mut extra = Obj::new();
    extra.insert("file".into(), json!(rel));
    // The layout is matched on the file's own tokens, so key order and number
    // spelling never decide it; records are held as the source spelled them.
    let compact = crate::util::minify(text);
    let rendered = crate::util::pretty(&compact);
    if text == rendered {
        extra.insert("style".into(), json!("pretty2"));
    } else if text.strip_suffix('\n') == Some(rendered.as_str()) {
        extra.insert("style".into(), json!("pretty2+newline"));
    } else {
        extra.insert("raw_file".into(), json!(text));
    }
    let mut sk = doc.clone();
    let mut sk_text = Some(compact.clone());
    for k in ["chatMessages", "timeline"] {
        if sk.get(k).is_some() {
            sk[k] = json!([]);
            sk_text = sk_text.and_then(|t| jsonfmt::with_member(&t, k, "[]"));
        }
    }
    extra.insert("document".into(), sk);
    if let Some(t) = sk_text {
        extra.insert("document_text".into(), json!(t));
    }
    let texts_of = |k: &str| -> Vec<Option<String>> {
        let n = doc.get(k).and_then(Value::as_array).map_or(0, Vec::len);
        match jsonfmt::member(&compact, k).and_then(jsonfmt::items).filter(|t| t.len() == n) {
            Some(t) => t.into_iter().map(|x| Some(x.to_string())).collect(),
            None => vec![None; n],
        }
    };
    let (chat_texts, timeline_texts) = (texts_of("chatMessages"), texts_of("timeline"));
    let sid = get_str(doc, &["sessionId"]).map(String::from).unwrap_or_else(|| rel.trim_end_matches(".json").to_string());
    let mut l = Ledger::new(Agent {
        id: sid.clone(),
        created_at: get_str(doc, &["startTime"]).map(String::from),
        source: Some(Source { format: NAME.into(), variant: Some(V_LEGACY.into()), version: None, native_id: Some(sid), extra }),
        parent: None,
        links: vec![],
        meta: Obj::new(),
    });
    if let Some(m) = doc.get("selectedModel").filter(|m| m.is_string()) {
        push_record(&mut l, "@selectedModel", native(V_LEGACY, "@selectedModel", None), vec![Event::new("", Kind::Settings).with("model", m.clone())], None);
    }
    // Timestamps from the timeline, matched by order of kind / by call id.
    let timeline: Vec<Value> = doc.get("timeline").and_then(Value::as_array).cloned().unwrap_or_default();
    let mut user_ts = timeline.iter().filter(|t| get_str(t, &["type"]) == Some("user")).filter_map(|t| get_str(t, &["timestamp"]).map(String::from));
    let mut asst_ts = timeline.iter().filter(|t| get_str(t, &["type"]) == Some("copilot")).filter_map(|t| get_str(t, &["timestamp"]).map(String::from));
    let call_ts: HashMap<String, (Option<String>, Option<String>)> =
        timeline.iter().filter_map(|t| get_str(t, &["callId"]).map(|c| (c.to_string(), t))).fold(HashMap::new(), |mut m, (c, t)| {
            let e = m.entry(c).or_insert((None, None));
            match get_str(t, &["type"]) {
                Some("tool_call_requested") => e.0 = get_str(t, &["timestamp"]).map(String::from),
                Some("tool_call_completed") => e.1 = get_str(t, &["timestamp"]).map(String::from),
                _ => {}
            }
            m
        });
    let mut turn: Option<String> = None;
    for (i, m) in doc.get("chatMessages").and_then(Value::as_array).cloned().unwrap_or_default().iter().enumerate() {
        let nid = format!("chat@{i}");
        let role = get_str(m, &["role"]).unwrap_or("");
        let mut evs = Vec::new();
        match role {
            "system" | "developer" => evs.push(Event::new("", Kind::Text).with("role", role).with("text", text_of(m.get("content")))),
            "user" => {
                let ts = user_ts.next();
                turn = Some(nid.clone());
                let mk = |k: Kind| Event::new("", k).ts(ts.clone()).turn(turn.clone());
                match m.get("content") {
                    Some(Value::Array(parts)) => {
                        for p in parts {
                            match get_str(p, &["type"]) {
                                Some("text") => evs.push(mk(Kind::Text).with("role", "user").with("text", p.get("text").cloned().unwrap_or(json!("")))),
                                Some("image_url") => {
                                    let url = get_str(p, &["image_url", "url"]).unwrap_or("");
                                    let mut e = mk(Kind::Media).with("role", "user");
                                    match url.strip_prefix("data:").and_then(|u| u.split_once(";base64,")) {
                                        Some((mt, data)) => e = e.with("media_type", mt).with("data", data),
                                        None => e = e.with("media_type", "image/*").with("url", url),
                                    }
                                    evs.push(e);
                                }
                                _ => {}
                            }
                        }
                    }
                    c => evs.push(mk(Kind::Text).with("role", "user").with("text", text_of(c))),
                }
            }
            "assistant" => {
                let ts = asst_ts.next().or_else(|| {
                    m.get("tool_calls")
                        .and_then(Value::as_array)
                        .and_then(|a| a.first())
                        .and_then(|c| get_str(c, &["id"]))
                        .and_then(|c| call_ts.get(c))
                        .and_then(|x| x.0.clone())
                });
                let mk = |k: Kind| Event::new("", k).ts(ts.clone()).turn(turn.clone()).group(Some(nid.clone()));
                let t = text_of(m.get("content"));
                if !t.is_empty() {
                    evs.push(mk(Kind::Text).with("role", "assistant").with("text", t));
                }
                for c in m.get("tool_calls").and_then(Value::as_array).into_iter().flatten() {
                    let args = match get_str(c, &["function", "arguments"]) {
                        Some(s) => serde_json::from_str(s).unwrap_or(json!({"input": s})),
                        None => c.get("function").and_then(|f| f.get("arguments")).cloned().unwrap_or(json!({})),
                    };
                    let mut e = mk(Kind::ToolCall)
                        .with("call_id", c.get("id").cloned().unwrap_or(json!("")))
                        .with("name", get_str(c, &["function", "name"]).unwrap_or(""))
                        .with("input", args);
                    if let Some(ts) = get_str(c, &["id"]).and_then(|id| call_ts.get(id)).and_then(|x| x.0.clone()) {
                        e = e.ts(Some(ts));
                    }
                    evs.push(e);
                }
            }
            "tool" | "function" => {
                let id = get_str(m, &["tool_call_id"]).unwrap_or("").to_string();
                let ts = call_ts.get(&id).and_then(|x| x.1.clone());
                let failed = timeline.iter().any(|t| {
                    get_str(t, &["callId"]) == Some(id.as_str()) && matches!(get_str(t, &["result", "type"]), Some("failure" | "rejected" | "denied"))
                });
                let mut e = Event::new("", Kind::ToolResult).ts(ts).turn(turn.clone()).with("call_id", id.as_str()).with("output", text_of(m.get("content")));
                if failed {
                    e = e.with("is_error", true);
                }
                evs.push(e);
            }
            _ => {}
        }
        let mut n = native(V_LEGACY, &nid, None);
        jsonfmt::hold(&mut n, chat_texts.get(i).cloned().flatten().as_deref(), m);
        push_record(&mut l, &nid, n, evs, None);
    }
    for (i, t) in timeline.iter().enumerate() {
        let id = get_str(t, &["id"]).map(String::from).unwrap_or_else(|| format!("@timeline{}", util::short_id(&t.to_string(), 8)));
        let ts = get_str(t, &["timestamp"]).map(String::from);
        let evs = match get_str(t, &["type"]) {
            Some("info") => vec![
                Event::new("", Kind::Text)
                    .ts(ts.clone())
                    .with("role", "system")
                    .with("text", t.get("text").cloned().unwrap_or(json!("")))
                    .audience(Audience::Record),
            ],
            Some("error") => {
                vec![Event::new("", Kind::Error).ts(ts.clone()).with("kind", "error").with("message", t.get("text").cloned().unwrap_or(json!("")))]
            }
            _ => vec![],
        };
        let mut n = native(V_LEGACY, &id, None);
        jsonfmt::hold(&mut n, timeline_texts.get(i).cloned().flatten().as_deref(), t);
        push_record(&mut l, &id, n, evs, ts);
    }
    l
}

/// The compact text of an unchanged document: the skeleton's own text with
/// each list filled from its records' texts. `None` when a piece is missing,
/// the layout was not reproducible, or the text would not say `doc`.
fn own_text(extra: &Obj, doc: &Value, lists: [(&str, Vec<Option<String>>); 2]) -> Option<String> {
    extra.get("style")?;
    let mut t = extra.get("document_text").and_then(Value::as_str)?.to_string();
    for (k, texts) in lists {
        let texts: Vec<String> = texts.into_iter().collect::<Option<_>>()?;
        if jsonfmt::member(&t, k).is_some() {
            t = jsonfmt::with_member(&t, k, &jsonfmt::array_text(texts.iter().map(String::as_str)))?;
        } else if !texts.is_empty() {
            return None;
        }
    }
    (serde_json::from_str::<Value>(&t).ok().as_ref() == Some(doc)).then_some(t)
}

fn text_of(c: Option<&Value>) -> String {
    match c {
        Some(Value::String(s)) => s.clone(),
        Some(Value::Array(a)) => a.iter().filter_map(|p| get_str(p, &["text"])).collect::<Vec<_>>().join(""),
        _ => String::new(),
    }
}

pub(super) fn export(set: &LedgerSet, _opts: &ExportOptions) -> Result<Exported> {
    let mut out = Output::default();
    let mut losses = Losses::default();
    for l in set.iter() {
        let own_l = l.agent().source.as_ref().is_some_and(|s| s.format == NAME && s.variant.as_deref() == Some(V_LEGACY));
        let extra = l.agent().source.as_ref().filter(|_| own_l).map(|s| s.extra.clone()).unwrap_or_default();
        let sid = session_id(l);
        let start = util::norm_ts(
            &l.agent().created_at.clone().or_else(|| l.events.iter().find_map(|e| e.ts.clone())).unwrap_or_else(|| "1970-01-01T00:00:00.000Z".into()),
        );
        let mut doc = extra.get("document").cloned().unwrap_or_else(|| json!({"sessionId": sid, "startTime": start, "chatMessages": [], "timeline": []}));
        let mut chat: Vec<Value> = Vec::new();
        let mut timeline: Vec<Value> = Vec::new();
        let all_own = own_l && l.events.iter().all(|e| own(e, V_LEGACY).is_some());
        let (mut chat_texts, mut timeline_texts): (Vec<Option<String>>, Vec<Option<String>>) = (Vec::new(), Vec::new());
        let mut asst: Option<usize> = None;
        let mut names: HashMap<String, (String, Value)> = HashMap::new();
        let mut model: Option<String> = None;
        for e in &l.events {
            if let Some(n) = own(e, V_LEGACY).filter(|_| own_l) {
                if n.part == 0
                    && let Some(r) = &n.record_json()
                {
                    let text = crate::util::native_text(n);
                    if n.id.as_deref().is_some_and(|i| i.starts_with("chat@")) {
                        chat.push(r.clone());
                        chat_texts.push(text);
                    } else {
                        timeline.push(r.clone());
                        timeline_texts.push(text);
                    }
                }
                asst = None;
                continue;
            }
            let ts = util::norm_ts(e.ts.as_deref().unwrap_or(&start));
            let tl = |ty: &str, extra: Value| {
                let mut t = json!({"id": uuid(&format!("{}#timeline", e.id)), "timestamp": ts, "type": ty});
                if let Value::Object(o) = extra {
                    for (k, v) in o {
                        t[k] = v;
                    }
                }
                t
            };
            let k = e.kind;
            match k {
                Kind::Text => match e.str("role").unwrap_or("user") {
                    "assistant" => {
                        let text = e.str("text").unwrap_or("");
                        match asst.filter(|&i| chat[i].get("tool_calls").is_none()) {
                            Some(i) => {
                                let cur = chat[i]["content"].as_str().unwrap_or("").to_string();
                                chat[i]["content"] = json!(cur + text);
                            }
                            None => {
                                chat.push(json!({"role": "assistant", "content": text}));
                                asst = Some(chat.len() - 1);
                            }
                        }
                        timeline.push(tl("copilot", json!({"text": text})));
                    }
                    role @ ("system" | "developer") => {
                        chat.push(json!({"role": role, "content": e.str("text").unwrap_or("")}));
                        asst = None;
                    }
                    _ => {
                        chat.push(json!({"role": "user", "content": e.str("text").unwrap_or("")}));
                        timeline.push(tl("user", json!({"text": e.str("text").unwrap_or("")})));
                        asst = None;
                    }
                },
                Kind::ToolCall => {
                    let id = e.str("call_id").unwrap_or("").to_string();
                    let name = e.str("name").unwrap_or("").to_string();
                    let args = crate::formats::pi_session::tool_args_object(e, &mut losses);
                    names.insert(id.clone(), (name.clone(), args.clone()));
                    let i = match asst {
                        Some(i) => i,
                        None => {
                            chat.push(json!({"role": "assistant", "content": ""}));
                            chat.len() - 1
                        }
                    };
                    if chat[i].get("tool_calls").is_none() {
                        chat[i]["tool_calls"] = json!([]);
                    }
                    chat[i]["tool_calls"]
                        .as_array_mut()
                        .unwrap()
                        .push(json!({"id": id, "type": "function", "function": {"name": name, "arguments": args.to_string()}}));
                    asst = Some(i);
                    timeline.push(tl("tool_call_requested", json!({"callId": id, "name": name, "arguments": args})));
                }
                Kind::ToolResult => {
                    let id = e.str("call_id").unwrap_or("").to_string();
                    let (text, media) = output_text(e.body.get("output"));
                    if media {
                        losses.add("tool-result-media-dropped", Some(k), "media in tool output is dropped");
                    }
                    chat.push(json!({"role": "tool", "tool_call_id": id, "content": text}));
                    let err = e.body.get("is_error").and_then(Value::as_bool) == Some(true);
                    let (name, args) = names.get(&id).cloned().unwrap_or_default();
                    timeline.push(tl(
                        "tool_call_completed",
                        json!({"callId": id, "name": name, "result": {"type": if err { "failure" } else { "success" }, "log": text}, "arguments": args}),
                    ));
                    asst = None;
                }
                Kind::Error => timeline.push(tl("error", json!({"text": e.str("message").unwrap_or("")}))),
                Kind::Settings => {
                    if let Some(m) = e.str("model") {
                        model = Some(m.to_string());
                    }
                }
                Kind::Media => {
                    match (e.str("data"), e.str("url")) {
                        (Some(d), _) => chat.push(json!({"role": "user", "content": [{"type": "image_url", "image_url": {"url": format!("data:{};base64,{d}", e.str("media_type").unwrap_or("image/png"))}}]})),
                        (None, Some(u)) => chat.push(json!({"role": "user", "content": [{"type": "image_url", "image_url": {"url": u}}]})),
                        _ => losses.add("media-dropped", Some(k), "media given only by path"),
                    }
                    asst = None;
                }
                Kind::Reasoning => losses.add("reasoning-dropped", Some(k), "the legacy format records no reasoning"),
                other => losses.add(&format!("{}-dropped", other.as_str().replace('_', "-")), Some(other), "the legacy format has no record for it"),
            }
        }
        doc["chatMessages"] = Value::Array(chat);
        doc["timeline"] = Value::Array(timeline);
        if let Some(m) = model.filter(|_| doc.get("selectedModel").is_none()) {
            doc["selectedModel"] = json!(m);
        }
        // A document kept whole (no layout reproduces it) is written back as
        // read only while its records still say what it says.
        if all_own
            && let (Some(raw), Some(f)) = (extra.get("raw_file").and_then(Value::as_str), extra.get("file").and_then(Value::as_str))
            && crate::util::same_json(raw, &serde_json::to_string(&doc).expect("json"))
        {
            out.put(f, raw);
            continue;
        }
        // Unchanged: laid out again from the records' own texts.
        let own_text = all_own.then(|| own_text(&extra, &doc, [("chatMessages", chat_texts), ("timeline", timeline_texts)])).flatten();
        let mut s = match own_text {
            Some(t) => crate::util::pretty(&t),
            None => {
                if all_own {
                    // Unchanged, yet not rebuilt from its records' texts.
                    losses.add(jsonfmt::EXACT_TEXT_FALLBACK, None, jsonfmt::EXACT_TEXT_FALLBACK_DETAIL);
                }
                pretty(&doc)
            }
        };
        if extra.get("style").and_then(Value::as_str) == Some("pretty2+newline") {
            s.push('\n');
        }
        let ms = util::parse_rfc3339_ms(&start).unwrap_or(0);
        let path = extra.get("file").and_then(Value::as_str).map(String::from).unwrap_or_else(|| format!("session_{sid}_{ms}.json"));
        out.put(path, s);
    }
    Ok(Exported { output: out, losses: losses.sorted(), variant: V_LEGACY.into() })
}
