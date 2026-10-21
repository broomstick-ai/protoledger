//! The "semantic JSONL interchange v1" that `copilot sessions import <file>`
//! (>= 1.0.85) reads: a `session` header line, then `message`, `tool-call`
//! and `tool-result` records. The import creates a new session; it keeps the
//! conversation but not timestamps, reasoning or usage.

use super::{NAME, V_INTERCHANGE, native, output_text, own, push_record, uuid};
use crate::formats::atif::jsonfmt;
use crate::formats::{ExportOptions, Exported, ImportOptions, Imported, Losses, Output};
use crate::ledger::{Ledger, LedgerSet};
use crate::model::{Agent, Event, Kind, Obj, Source};
use crate::util::{self, get_str};
use anyhow::{Context, Result, bail};
use serde_json::{Value, json};
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
            .filter(|e| e.file_type().is_file() && e.file_name().to_string_lossy().ends_with(".jsonl"))
            .map(|e| (e.path().strip_prefix(input).unwrap().to_string_lossy().replace('\\', "/"), e.path().to_path_buf()))
            .collect()
    };
    for (rel, path) in files {
        let text = std::fs::read_to_string(&path).with_context(|| path.display().to_string())?;
        let (lines, final_nl) = crate::formats::cursor::raw_lines(&text);
        let first: Option<Value> = lines.first().and_then(|(_, l)| serde_json::from_str(l).ok());
        let Some(head) = first.filter(|h| get_str(h, &["type"]) == Some("session")) else { continue };
        let id = get_str(&head, &["externalId"]).unwrap_or(&rel).to_string();
        let mut extra = Obj::new();
        extra.insert("file".into(), json!(rel));
        if !final_nl {
            extra.insert("no_final_newline".into(), json!(true));
        }
        let mut l = Ledger::new(Agent {
            id: id.clone(),
            created_at: get_str(&head, &["createdAt"]).map(String::from),
            source: Some(Source { format: NAME.into(), variant: Some(V_INTERCHANGE.into()), version: None, native_id: Some(id), extra }),
            parent: None,
            links: vec![],
            meta: Obj::new(),
        });
        for (off, line) in lines {
            let nid = format!("@{off}");
            let Ok(v) = serde_json::from_str::<Value>(line) else {
                push_record(&mut l, &nid, native(V_INTERCHANGE, &nid, None).raw(line), vec![], None);
                continue;
            };
            let ts = get_str(&v, &["timestamp"]).map(String::from);
            let turn = get_str(&v, &["turnId"]).map(String::from);
            let mk = |k: Kind| Event::new("", k).ts(ts.clone()).turn(turn.clone());
            let mut evs = Vec::new();
            match get_str(&v, &["type"]).unwrap_or("") {
                "session" => {
                    let mut s = Event::new("", Kind::Settings).ts(get_str(&v, &["createdAt"]));
                    if let Some(c) = get_str(&v, &["workspace", "workingDirectory"]) {
                        s = s.with("cwd", c);
                    }
                    if let Some(b) = get_str(&v, &["workspace", "branch"]) {
                        s = s.with("git_branch", b);
                    }
                    if !s.body.is_empty() {
                        evs.push(s);
                    }
                    if let Some(t) = get_str(&v, &["title"]) {
                        evs.push(Event::new("", Kind::Title).with("text", t));
                    }
                }
                "message" => {
                    let role = if get_str(&v, &["role"]) == Some("assistant") { "assistant" } else { "user" };
                    let group = get_str(&v, &["id"]).map(String::from);
                    if let Some(m) = get_str(&v, &["model"]) {
                        evs.push(mk(Kind::Settings).with("model", m));
                    }
                    for p in v.get("content").and_then(Value::as_array).into_iter().flatten() {
                        match get_str(p, &["type"]) {
                            Some("text") => {
                                evs.push(mk(Kind::Text).with("role", role).with("text", p.get("text").cloned().unwrap_or(json!(""))).group(group.clone()))
                            }
                            Some("attachment") => {
                                let mut e = mk(Kind::Media)
                                    .with("role", role)
                                    .with("media_type", p.get("mediaType").cloned().unwrap_or(json!("application/octet-stream")))
                                    .with("path", p.get("path").cloned().unwrap_or(json!("")));
                                if let Some(n) = p.get("name") {
                                    e = e.with("name", n.clone());
                                }
                                evs.push(e);
                            }
                            _ => {}
                        }
                    }
                }
                "tool-call" => evs.push(
                    mk(Kind::ToolCall)
                        .with("call_id", v.get("id").cloned().unwrap_or(json!("")))
                        .with("name", v.get("name").cloned().unwrap_or(json!("")))
                        .with("input", v.get("arguments").cloned().unwrap_or(json!({}))),
                ),
                "tool-result" => {
                    let ok = v.get("success").and_then(Value::as_bool).unwrap_or(true);
                    let out = if ok {
                        v.get("content").cloned().unwrap_or(json!(""))
                    } else {
                        v.get("error").and_then(|e| e.get("message")).cloned().unwrap_or(json!(""))
                    };
                    let out = if out.is_string() { out } else { Value::String(out.to_string()) };
                    let mut e = mk(Kind::ToolResult).with("call_id", v.get("toolCallId").cloned().unwrap_or(json!(""))).with("output", out);
                    if !ok {
                        e = e.with("is_error", true);
                    }
                    evs.push(e);
                }
                _ => {}
            }
            // The line is held as its own text (SPEC §4).
            let mut n = native(V_INTERCHANGE, &nid, None);
            jsonfmt::hold(&mut n, Some(line), &v);
            push_record(&mut l, &nid, n, evs, ts);
        }
        set.insert(l);
    }
    if set.is_empty() {
        bail!("{}: no Copilot CLI interchange file found", input.display());
    }
    Ok(Imported { set, losses: Losses::default(), variant: V_INTERCHANGE.into() })
}

pub(super) fn export(set: &LedgerSet, _opts: &ExportOptions) -> Result<Exported> {
    let mut out = Output::default();
    let mut losses = Losses::default();
    for l in set.iter() {
        let own_l = l.agent().source.as_ref().is_some_and(|s| s.format == NAME && s.variant.as_deref() == Some(V_INTERCHANGE));
        let extra = l.agent().source.as_ref().filter(|_| own_l).map(|s| s.extra.clone()).unwrap_or_default();
        let mut lines: Vec<String> = Vec::new();
        let has_head = l.events.iter().any(|e| own(e, V_INTERCHANGE).and_then(|n| n.record_json()).is_some_and(|r| get_str(&r, &["type"]) == Some("session")));
        if !has_head {
            let created = util::norm_ts(
                &l.agent().created_at.clone().or_else(|| l.events.iter().find_map(|e| e.ts.clone())).unwrap_or_else(|| "1970-01-01T00:00:00.000Z".into()),
            );
            let updated = l.events.iter().rev().find_map(|e| e.ts.as_deref().map(util::norm_ts)).unwrap_or_else(|| created.clone());
            let mut h = json!({"type": "session", "version": 1, "externalId": l.id(), "createdAt": created, "updatedAt": updated});
            if let Some(t) = l.events.iter().rev().find(|e| e.kind == Kind::Title).and_then(|e| e.str("text")) {
                h["title"] = json!(t);
            }
            h["source"] = json!({"application": "protoledger", "version": env!("CARGO_PKG_VERSION")});
            let mut ws = json!({});
            if let Some(c) = l.events.iter().find_map(|e| (e.kind == Kind::Settings).then(|| e.str("cwd")).flatten()) {
                ws["workingDirectory"] = json!(c);
            }
            if let Some(b) = l.events.iter().find_map(|e| (e.kind == Kind::Settings).then(|| e.str("git_branch")).flatten()) {
                ws["branch"] = json!(b);
            }
            if ws.as_object().is_some_and(|o| !o.is_empty()) {
                h["workspace"] = ws;
            }
            lines.push(serde_json::to_string(&h).unwrap());
        }
        let mut last_role: Option<String> = None;
        let mut turn = 0u32;
        let mut model: Option<String> = None;
        let mut msg: Option<(usize, Option<String>)> = None; // (line index, group) of an open message
        let mut msgs: Vec<Value> = Vec::new();
        let mut order: Vec<Option<usize>> = Vec::new(); // own lines are pushed directly
        let _ = &mut order;
        let flush = |msgs: &mut Vec<Value>, lines: &mut Vec<String>| {
            for m in msgs.drain(..) {
                lines.push(serde_json::to_string(&m).unwrap());
            }
        };
        for e in &l.events {
            if let Some(n) = own(e, V_INTERCHANGE).filter(|_| own_l) {
                if n.part == 0 {
                    flush(&mut msgs, &mut lines);
                    msg = None;
                    match (crate::util::current_raw(n), n.record_text()) {
                        (Some(r), _) => lines.push(r.to_string()),
                        (None, Some(r)) => lines.push(r.to_string()),
                        _ => {}
                    }
                    last_role = n.record_json().as_ref().and_then(|r| get_str(r, &["role"]).map(String::from)).or(last_role);
                }
                continue;
            }
            let ts = util::norm_ts(e.ts.as_deref().unwrap_or("1970-01-01T00:00:00.000Z"));
            let k = e.kind;
            match k {
                Kind::Text if matches!(e.str("role"), Some("user") | Some("assistant") | None) => {
                    let role = e.str("role").unwrap_or("user");
                    if role == "user" && last_role.as_deref() != Some("user") {
                        turn += 1;
                    }
                    let reuse = msg.as_ref().filter(|(i, g)| msgs[*i]["role"] == role && (g.is_none() || *g == e.group)).map(|(i, _)| *i);
                    let i = match reuse {
                        Some(i) => i,
                        None => {
                            let mut m =
                                json!({"type": "message", "id": uuid(&e.id), "timestamp": ts, "role": role, "turnId": format!("t{turn}"), "content": []});
                            if role == "assistant"
                                && let Some(md) = &model
                            {
                                m["model"] = json!(md);
                            }
                            msgs.push(m);
                            msgs.len() - 1
                        }
                    };
                    msgs[i]["content"].as_array_mut().unwrap().push(json!({"type": "text", "text": e.str("text").unwrap_or("")}));
                    msg = Some((i, e.group.clone()));
                    last_role = Some(role.to_string());
                }
                Kind::ToolCall => {
                    // A tool call must follow an assistant message of its turn.
                    if last_role.as_deref() != Some("assistant") {
                        msgs.push(json!({"type": "message", "id": uuid(&format!("{}#message", e.id)), "timestamp": ts, "role": "assistant", "turnId": format!("t{turn}"), "content": [{"type": "text", "text": ""}]}));
                        losses.add("empty-assistant-message-added", Some(k), "a tool call with no assistant text before it gets an empty assistant message");
                        last_role = Some("assistant".into());
                    }
                    msgs.push(json!({"type": "tool-call", "id": e.str("call_id").unwrap_or(""), "timestamp": ts, "turnId": format!("t{turn}"), "name": e.str("name").unwrap_or(""), "arguments": crate::formats::pi_session::tool_args_object(e, &mut losses)}));
                    msg = None;
                }
                Kind::ToolResult => {
                    let (text, media) = output_text(e.body.get("output"));
                    if media {
                        losses.add("tool-result-media-dropped", Some(k), "media in tool output is dropped");
                    }
                    let ok = e.body.get("is_error").and_then(Value::as_bool) != Some(true);
                    let mut r = json!({"type": "tool-result", "id": uuid(&e.id), "timestamp": ts, "turnId": format!("t{turn}"), "toolCallId": e.str("call_id").unwrap_or(""), "success": ok});
                    if ok {
                        r["content"] = json!(text);
                    } else {
                        r["error"] = json!({"message": text});
                    }
                    msgs.push(r);
                    msg = None;
                }
                Kind::Settings => {
                    if let Some(m) = e.str("model") {
                        model = Some(m.to_string());
                    }
                }
                Kind::Title => {}
                Kind::Media => losses.add("media-dropped", Some(k), "attachments must be files next to the interchange file"),
                Kind::Text => losses.add("system-text-dropped", Some(k), "the interchange has only user and assistant messages"),
                other => {
                    losses.add(&format!("{}-dropped", other.as_str().replace('_', "-")), Some(other), "the interchange keeps messages and tool calls only")
                }
            }
        }
        flush(&mut msgs, &mut lines);
        let mut s = lines.join("\n");
        if extra.get("no_final_newline").is_none() && !lines.is_empty() {
            s.push('\n');
        }
        let path =
            extra.get("file").and_then(Value::as_str).map(String::from).unwrap_or_else(|| format!("{}.interchange.jsonl", crate::ledger::file_safe(l.id())));
        out.put(path, s);
    }
    Ok(Exported { output: out, losses: losses.sorted(), variant: V_INTERCHANGE.into() })
}
