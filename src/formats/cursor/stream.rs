//! `cursor-agent --print --output-format stream-json`.

use super::{KNOWN_TOOLS, NAME, V_STREAM, agent, ms_ts, native, output_text, own, push_record, ts_ms};
use crate::carry;
use crate::formats::atif::jsonfmt;
use crate::formats::gemini_cli::{apply_agent, attach_with_agent, carried_agent};
use crate::formats::{ExportOptions, Exported, ImportOptions, Imported, Losses, Output};
use crate::ledger::{Ledger, LedgerSet};
use crate::model::{Event, Kind, Obj};
use crate::util::{self, get_str};
use anyhow::{Context, Result, bail};
use serde_json::{Value, json};
use std::collections::HashMap;
use std::path::Path;

pub(super) fn files(input: &Path, ext: &[&str]) -> Result<Vec<(String, String)>> {
    let mut out = Vec::new();
    if input.is_file() {
        let name = input.file_name().unwrap().to_string_lossy().to_string();
        out.push((name, std::fs::read_to_string(input).with_context(|| input.display().to_string())?));
        return Ok(out);
    }
    for e in walkdir::WalkDir::new(input).sort_by_file_name() {
        let e = e?;
        let rel = e.path().strip_prefix(input)?.to_string_lossy().replace('\\', "/");
        let base = rel.rsplit('/').next().unwrap_or("").to_string();
        if !e.file_type().is_file() || base.starts_with('.') || rel == "SOURCE.md" || rel == "expect.json" {
            continue;
        }
        if ext.iter().any(|x| base.ends_with(x)) {
            out.push((rel, std::fs::read_to_string(e.path())?));
        }
    }
    Ok(out)
}

/// Every line with its byte offset, including blank and non-JSON ones (a
/// stream capture may carry `#` comments); `true` if the text ends in `\n`.
pub(crate) fn raw_lines(text: &str) -> (Vec<(usize, &str)>, bool) {
    let mut out = Vec::new();
    let mut off = 0;
    for l in text.split_inclusive('\n') {
        out.push((off, l.strip_suffix('\n').unwrap_or(l)));
        off += l.len();
    }
    (out, text.ends_with('\n') || text.is_empty())
}

struct Line<'a> {
    off: usize,
    text: &'a str,
    val: Option<Value>,
}

impl Line<'_> {
    fn ty(&self) -> Option<&str> {
        self.val.as_ref().and_then(|v| get_str(v, &["type"]))
    }
    fn has_carrier(&self) -> bool {
        self.val.as_ref().is_some_and(|v| v.get(carry::KEY).is_some())
    }
}

pub(super) fn import(input: &Path, opts: &ImportOptions) -> Result<Imported> {
    let mut set = LedgerSet::new();
    let mut losses = Losses::default();
    let multi = input.is_dir();
    for (rel, text) in files(input, &[".jsonl", ".ndjson", ".txt", ".json"])? {
        if multi && super::detect_file(&input.join(&rel)).map(|d| d.variant) != Some(V_STREAM) {
            continue;
        }
        let l = import_file(&rel, &text, opts, &mut losses)?;
        set.insert(l);
    }
    if set.is_empty() {
        bail!("{}: no cursor-agent stream-json found", input.display());
    }
    Ok(Imported { set, losses: losses.sorted(), variant: V_STREAM.into() })
}

fn import_file(rel: &str, text: &str, opts: &ImportOptions, _losses: &mut Losses) -> Result<Ledger> {
    let (raw, final_nl) = raw_lines(text);
    let lines: Vec<Line> = raw.iter().map(|(off, t)| Line { off: *off, text: t, val: serde_json::from_str(t).ok() }).collect();
    let session = lines.iter().find_map(|l| l.val.as_ref().and_then(|v| get_str(v, &["session_id"])).map(String::from));
    let stem = rel.rsplit('/').next().unwrap_or(rel);
    let stem = stem.split('.').next().unwrap_or(stem);
    let mut extra = Obj::new();
    extra.insert("file".into(), json!(rel));
    if !final_nl {
        extra.insert("no_final_newline".into(), json!(true));
    }
    let id = session.clone().unwrap_or_else(|| stem.to_string());
    let mut l = Ledger::new(agent(&id, V_STREAM, session, extra));
    let mut turn: Option<String> = None;
    let mut i = 0;
    while i < lines.len() {
        // Group a thinking run (deltas .. completed) or an assistant run
        // (partial-output deltas and their flushes) into one native record.
        let ty = lines[i].ty().map(String::from);
        let mut j = i;
        if matches!(ty.as_deref(), Some("thinking") | Some("assistant")) {
            while j + 1 < lines.len() && lines[j + 1].ty() == ty.as_deref() && !lines[j].has_carrier() {
                if ty.as_deref() == Some("thinking") && get_str(lines[j].val.as_ref().unwrap(), &["subtype"]) == Some("completed") {
                    break;
                }
                j += 1;
            }
        }
        let group = &lines[i..=j];
        i = j + 1;
        let nid = format!("@{}", group[0].off);
        // Carried events ride on the last line of a rendered group.
        if opts.restore_carried
            && let Some(v) = group.last().unwrap().val.as_ref().filter(|v| v.get(carry::KEY).is_some())
        {
            if let Some(a) = carried_agent(v) {
                apply_agent(l.agent_mut(), &a);
            }
            let mut r = v.clone();
            if let Some(evs) = carry::take(&mut r, &[]) {
                for e in evs {
                    l.append_unique(e);
                }
                continue;
            }
        }
        let mut n = native(V_STREAM, &nid);
        if group.len() == 1 {
            match &group[0].val {
                // The line is held as its own text (SPEC §4).
                Some(v) => jsonfmt::hold(&mut n, Some(group[0].text), v),
                None => n = n.raw(group[0].text),
            }
        } else {
            n = n.record(Value::Array(group.iter().map(|g| g.val.clone().unwrap_or(Value::Null)).collect()));
            n = n.raw(group.iter().map(|g| g.text).collect::<Vec<_>>().join("\n"));
        }
        let (evs, ts) = events_for(group, &mut turn, &nid);
        push_record(&mut l, &nid, n, evs, ts);
    }
    Ok(l)
}

fn events_for(group: &[Line], turn: &mut Option<String>, nid: &str) -> (Vec<Event>, Option<String>) {
    let Some(v) = group[0].val.as_ref() else { return (vec![], None) };
    let ts = ms_ts(v.get("timestamp_ms"));
    let mk = |k: Kind, turn: &Option<String>| Event::new("", k).ts(ts.clone()).turn(turn.clone());
    let mut out = Vec::new();
    match get_str(v, &["type"]).unwrap_or("") {
        "system" if get_str(v, &["subtype"]) == Some("init") => {
            let mut e = mk(Kind::Settings, turn);
            for (from, to) in [("model", "model"), ("cwd", "cwd"), ("permissionMode", "permission_mode")] {
                if let Some(x) = v.get(from) {
                    e = e.with(to, x.clone());
                }
            }
            out.push(e);
        }
        "user" => {
            *turn = Some(nid.to_string());
            for p in content_parts(v) {
                if let Some(t) = p.get("text").and_then(Value::as_str) {
                    out.push(mk(Kind::Text, turn).with("role", "user").with("text", t));
                }
            }
        }
        "assistant" => {
            let mut acc = String::new();
            for g in group {
                let t: String = g
                    .val
                    .as_ref()
                    .map(|v| content_parts(v).iter().filter_map(|p| p.get("text").and_then(Value::as_str).map(String::from)).collect())
                    .unwrap_or_default();
                if !acc.is_empty() && t == acc {
                    continue; // a flush repeating what was streamed
                }
                acc.push_str(&t);
            }
            let mc = group.iter().find_map(|g| g.val.as_ref().and_then(|v| get_str(v, &["model_call_id"])).map(String::from));
            if !acc.is_empty() {
                out.push(mk(Kind::Text, turn).with("role", "assistant").with("text", acc).group(mc));
            }
        }
        "thinking" => {
            let text: String = group.iter().filter_map(|g| g.val.as_ref().and_then(|v| get_str(v, &["text"])).map(String::from)).collect();
            if !text.is_empty() {
                out.push(mk(Kind::Reasoning, turn).with("text", text));
            }
        }
        "tool_call" => {
            let call_id = get_str(v, &["call_id"]).unwrap_or("").to_string();
            let tc = v.get("tool_call").cloned().unwrap_or(json!({}));
            let Some((key, body)) = tool_entry(&tc) else { return (out, ts) };
            let ts = ts.clone().or_else(|| ms_ts(tc.get("startedAtMs")));
            let mc = get_str(v, &["model_call_id"]).map(String::from);
            let mk = |k: Kind| Event::new("", k).ts(ts.clone()).turn(turn.clone()).group(mc.clone());
            let (name, input, tool_type, server) = describe_tool(&key, &body);
            match get_str(v, &["subtype"]).unwrap_or("") {
                "started" => {
                    let mut e = mk(Kind::ToolCall).with("call_id", call_id.as_str()).with("name", name.as_str()).with("input", input.clone());
                    if let Some(t) = tool_type {
                        e = e.with("tool_type", t);
                    }
                    if let Some(s) = server {
                        e = e.with("server", s);
                    }
                    out.push(e);
                    if key == "taskToolCall" {
                        let mut s = mk(Kind::Subagent).with("phase", "start").with("call_id", call_id.as_str());
                        if let Some(a) = input.get("agentId").filter(|a| a.is_string()) {
                            s = s.with("agent", a.clone());
                        }
                        if let Some(d) = input.get("description").filter(|a| a.is_string()) {
                            s = s.with("description", d.clone());
                        }
                        if let Some(t) = input.get("subagentType").and_then(Value::as_object).and_then(|o| o.keys().next().cloned()) {
                            s = s.with("agent_type", t);
                        }
                        out.push(s);
                    }
                }
                "completed" | "failed" => {
                    let ts2 = ms_ts(tc.get("completedAtMs")).or(ts.clone());
                    let mk2 = |k: Kind| Event::new("", k).ts(ts2.clone()).turn(turn.clone());
                    let result = body.get("result").cloned().unwrap_or(Value::Null);
                    let (output, is_error) = result_output(&result);
                    let mut e = mk2(Kind::ToolResult).with("call_id", call_id.as_str()).with("output", output);
                    if is_error || get_str(v, &["subtype"]) == Some("failed") {
                        e = e.with("is_error", true);
                    }
                    out.push(e);
                    let ok = result.get("success");
                    match (key.as_str(), ok) {
                        ("editToolCall", Some(s)) if s.get("diffString").is_some() => {
                            let mut f = mk2(Kind::FileChange)
                                .with("path", s.get("path").or(input.get("path")).cloned().unwrap_or(json!("")))
                                .with("op", "edit")
                                .with("diff", s["diffString"].clone());
                            if let Some(c) = s.get("afterFullFileContent").filter(|c| c.is_string()) {
                                f = f.with("content", c.clone());
                            }
                            out.push(f);
                        }
                        ("writeToolCall", Some(s)) => {
                            let mut f =
                                mk2(Kind::FileChange).with("path", s.get("path").or(input.get("path")).cloned().unwrap_or(json!(""))).with("op", "create");
                            if let Some(c) = input.get("fileText").filter(|c| c.is_string()) {
                                f = f.with("content", c.clone());
                            }
                            out.push(f);
                        }
                        ("taskToolCall", _) => {
                            let mut s = mk2(Kind::Subagent)
                                .with("phase", "end")
                                .with("call_id", call_id.as_str())
                                .with("outcome", if ok.is_some() { "ok" } else { "error" });
                            if let Some(a) = ok.and_then(|s| s.get("agentId")).or(input.get("agentId")).filter(|a| a.is_string()) {
                                s = s.with("agent", a.clone());
                            }
                            if let Some(steps) = ok.and_then(|s| s.get("conversationSteps")).and_then(Value::as_array) {
                                let t: Vec<&str> = steps.iter().filter_map(|st| get_str(st, &["assistantMessage", "text"])).collect();
                                if !t.is_empty() {
                                    s = s.with("result", t.join("\n\n"));
                                }
                            }
                            out.push(s);
                        }
                        _ => {}
                    }
                }
                _ => {}
            }
        }
        "result" => {
            let is_error = v.get("is_error").and_then(Value::as_bool) == Some(true) || get_str(v, &["subtype"]) == Some("error");
            if let Some(u) = v.get("usage").filter(|u| u.is_object()) {
                let mut e = mk(Kind::Usage, turn);
                for (from, to) in [("inputTokens", "input"), ("outputTokens", "output"), ("cacheReadTokens", "cache_read"), ("cacheWriteTokens", "cache_write")]
                {
                    if let Some(n) = u.get(from).filter(|n| n.is_number()) {
                        e = e.with(to, n.clone());
                    }
                }
                if let Some(c) = u.get("totalCost").or(v.get("total_cost_usd")).filter(|n| n.is_number()) {
                    e = e.with("cost_usd", c.clone());
                }
                out.push(e);
            } else if let Some(c) = v.get("total_cost_usd").filter(|n| n.is_number()) {
                out.push(mk(Kind::Usage, turn).with("cost_usd", c.clone()));
            }
            if is_error {
                let msg = v.get("error").or(v.get("result")).cloned().unwrap_or(json!(""));
                let msg = if msg.is_string() { msg } else { Value::String(msg.to_string()) };
                out.push(mk(Kind::Error, turn).with("kind", "result").with("message", msg));
            }
            let mut e = mk(Kind::Turn, turn).with("phase", "end").with("outcome", if is_error { "error" } else { "ok" });
            if let Some(d) = v.get("duration_ms").filter(|n| n.is_number()) {
                e = e.with("duration_ms", d.clone());
            }
            out.push(e);
        }
        "error" => {
            let msg = v.get("error").or(v.get("message")).cloned().unwrap_or(json!(""));
            let msg = if msg.is_string() { msg } else { Value::String(msg.to_string()) };
            out.push(mk(Kind::Error, turn).with("kind", "error").with("message", msg));
        }
        _ => {}
    }
    (out, ts)
}

fn content_parts(v: &Value) -> Vec<Value> {
    match v.get("message").and_then(|m| m.get("content")) {
        Some(Value::Array(a)) => a.clone(),
        Some(Value::String(s)) => vec![json!({"type": "text", "text": s})],
        _ => vec![],
    }
}

/// The `<name>ToolCall` (or `function`) entry of a `tool_call` object.
fn tool_entry(tc: &Value) -> Option<(String, Value)> {
    let o = tc.as_object()?;
    o.iter().find(|(k, v)| (k.ends_with("ToolCall") || k.as_str() == "function") && v.is_object()).map(|(k, v)| (k.clone(), v.clone()))
}

/// (name, input, tool_type, server) of a tool entry.
fn describe_tool(key: &str, body: &Value) -> (String, Value, Option<&'static str>, Option<Value>) {
    if key == "function" {
        let name = get_str(body, &["name"]).unwrap_or("").to_string();
        let input = match body.get("arguments") {
            Some(Value::String(s)) => serde_json::from_str(s).unwrap_or(json!({"arguments": s})),
            Some(v) => v.clone(),
            None => json!({}),
        };
        return (name, input, None, None);
    }
    let args = body.get("args").cloned().unwrap_or(json!({}));
    if key == "mcpToolCall" {
        let name = get_str(&args, &["toolName"]).or(get_str(&args, &["name"])).unwrap_or("").to_string();
        let server = args.get("providerIdentifier").or(args.get("serverIdentifier")).cloned();
        let input = args.get("args").cloned().unwrap_or(json!({}));
        return (name, input, Some("mcp"), server);
    }
    let name = key.strip_suffix("ToolCall").unwrap_or(key).to_string();
    let tool_type = (name == "shell").then_some("shell");
    (name, args, tool_type, None)
}

/// The model-facing output of a `tool_call.<key>.result`, and whether it failed.
fn result_output(r: &Value) -> (Value, bool) {
    let Some(o) = r.as_object() else { return (json!(""), false) };
    if let Some(s) = o.get("success") {
        if let Some(t) = s.get("content").and_then(Value::as_str) {
            return (json!(t), false);
        }
        if let Some(parts) = s.get("content").and_then(Value::as_array) {
            // MCP: [{text:{text}} | {image:{data, mimeType}}]
            let mut out = Vec::new();
            for p in parts {
                if let Some(t) = get_str(p, &["text", "text"]) {
                    out.push(json!({"type": "text", "text": t}));
                } else if let Some(img) = p.get("image") {
                    out.push(json!({"type": "media", "media_type": img.get("mimeType").cloned().unwrap_or(json!("image/png")), "data": img.get("data").cloned().unwrap_or(json!(""))}));
                }
            }
            let err = s.get("isError").and_then(Value::as_bool) == Some(true);
            if out.len() == 1 && out[0]["type"] == "text" {
                return (out[0]["text"].clone(), err);
            }
            return (Value::Array(out), err);
        }
        if s.get("stdout").is_some() || s.get("stderr").is_some() {
            let mut t = get_str(s, &["stdout"]).unwrap_or("").to_string();
            let e = get_str(s, &["stderr"]).unwrap_or("");
            if !e.is_empty() {
                if !t.is_empty() {
                    t.push('\n');
                }
                t.push_str(e);
            }
            let code = s.get("exitCode").and_then(Value::as_i64).unwrap_or(0);
            return (json!(t), code != 0);
        }
        for k in ["resultForModel", "markdown", "message", "text"] {
            if let Some(t) = s.get(k).and_then(Value::as_str) {
                return (json!(t), false);
            }
        }
        return (json!(s.to_string()), false);
    }
    for (k, field) in [("error", "errorMessage"), ("failure", "error"), ("permissionDenied", "error"), ("rejected", "reason")] {
        if let Some(e) = o.get(k) {
            let t = e.get(field).and_then(Value::as_str).map(String::from).unwrap_or_else(|| e.to_string());
            return (json!(t), true);
        }
    }
    (json!(r.to_string()), false)
}

// ---------------------------------------------------------------------------
// export

pub(super) fn session_id(l: &Ledger) -> String {
    let id = l.id();
    if uuid::Uuid::parse_str(id).is_ok() {
        return id.to_string();
    }
    l.agent().source.as_ref().filter(|s| s.format == NAME).and_then(|s| s.native_id.clone()).unwrap_or_else(|| util::uuid_for(id))
}

pub(super) fn export(set: &LedgerSet, opts: &ExportOptions) -> Result<Exported> {
    let mut out = Output::default();
    let mut losses = Losses::default();
    for l in set.iter() {
        let own_l = super::is_own_ledger(l, V_STREAM);
        let extra = l.agent().source.as_ref().filter(|_| own_l).map(|s| s.extra.clone()).unwrap_or_default();
        let path = extra.get("file").and_then(Value::as_str).map(String::from).unwrap_or_else(|| format!("{}.jsonl", session_id(l)));
        let mut lines: Vec<String> = Vec::new();
        let mut foreign: Vec<&Event> = Vec::new();
        let mut have_init = l.events.iter().any(|e| own(e, V_STREAM).and_then(|n| n.record_json()).is_some_and(|r| get_str(&r, &["type"]) == Some("system")));
        // SPEC §2.3: a stream has no branch; the active path is written, and an
        // abandoned event rides on the next line.
        let on = crate::formats::active_path_only(l, &mut losses, "with carry they ride on the next line", |e| own(e, V_STREAM).is_some() && own_l);
        let off: std::collections::HashSet<&str> = l.events.iter().zip(&on).filter(|(_, o)| !**o).map(|(e, _)| e.id.as_str()).collect();
        let mut flush = |foreign: &mut Vec<&Event>, lines: &mut Vec<String>, have_init: &mut bool| {
            if foreign.is_empty() {
                return;
            }
            for v in render(l, foreign, &off, opts, have_init, &mut losses) {
                lines.push(serde_json::to_string(&v).unwrap());
            }
            foreign.clear();
        };
        for e in &l.events {
            match own(e, V_STREAM).filter(|_| own_l) {
                Some(n) => {
                    flush(&mut foreign, &mut lines, &mut have_init);
                    if n.part == 0 {
                        match (group_raw(n), n.record_text()) {
                            (Some(r), _) => lines.push(r.to_string()),
                            (None, Some(r)) => lines.push(r.to_string()),
                            _ => {}
                        }
                    }
                }
                None => foreign.push(e),
            }
        }
        flush(&mut foreign, &mut lines, &mut have_init);
        if lines.is_empty() && !own_l {
            // A thread with no events is still a session: its `init` line (which
            // carries the header) keeps the file recognisably Cursor's.
            lines.extend(render(l, &[], &off, opts, &mut have_init, &mut losses).iter().map(|v| serde_json::to_string(v).unwrap()));
        }
        let mut s = lines.join("\n");
        if extra.get("no_final_newline").is_none() && !lines.is_empty() {
            s.push('\n');
        }
        out.put(path, s);
    }
    Ok(Exported { output: out, losses: losses.sorted(), variant: V_STREAM.into() })
}

/// Render foreign events as stream-json lines, each carrying (with carry on)
/// the events it represents.
fn render(l: &Ledger, events: &[&Event], off: &std::collections::HashSet<&str>, opts: &ExportOptions, have_init: &mut bool, losses: &mut Losses) -> Vec<Value> {
    let sid = session_id(l);
    let mut out: Vec<(Value, Vec<Event>)> = Vec::new();
    let mut orphan: Vec<Event> = Vec::new();
    let mut calls: HashMap<String, (String, Value)> = HashMap::new(); // call id -> (key, body without result)
    let mut pending_usage: Option<Event> = None;
    let mut assistant_text = String::new();
    let tms = |e: &Event| ts_ms(e.ts.as_deref());
    let settings: Vec<&&Event> = events.iter().filter(|e| e.kind == Kind::Settings).collect();
    let first_model = settings.iter().find_map(|e| e.str("model").map(String::from));
    let first_cwd = settings.iter().find_map(|e| e.str("cwd").map(String::from));
    let init = |model: Option<String>, cwd: Option<String>, perm: Option<String>| json!({"type": "system", "subtype": "init", "apiKeySource": "login", "cwd": cwd.unwrap_or_else(|| "/".into()), "session_id": sid, "model": model.unwrap_or_else(|| "Auto".into()), "permissionMode": perm.unwrap_or_else(|| "default".into())});
    let mut head_line: Option<usize> = None;
    if !*have_init {
        out.push((init(first_model.clone(), first_cwd.clone(), None), vec![]));
        head_line = Some(0);
        *have_init = true;
    }
    let result_line = |is_error: bool, text: &str, usage: Option<&Event>, turn: Option<&Event>| {
        let mut r = json!({"type": "result", "subtype": if is_error { "error" } else { "success" }});
        if let Some(d) = turn.and_then(|t| t.body.get("duration_ms")) {
            r["duration_ms"] = d.clone();
            r["duration_api_ms"] = d.clone();
        }
        r["is_error"] = json!(is_error);
        r["result"] = json!(text);
        r["session_id"] = json!(sid);
        if let Some(u) = usage {
            let mut m = serde_json::Map::new();
            for (from, to) in [("input", "inputTokens"), ("output", "outputTokens"), ("cache_read", "cacheReadTokens"), ("cache_write", "cacheWriteTokens")] {
                if let Some(n) = u.body.get(from) {
                    m.insert(to.into(), n.clone());
                }
            }
            if let Some(c) = u.body.get("cost_usd") {
                m.insert("totalCost".into(), c.clone());
            }
            r["usage"] = Value::Object(m);
        }
        r
    };
    for &e in events {
        if off.contains(e.id.as_str()) {
            orphan.push(e.clone());
            continue;
        }
        let k = e.kind;
        let line = |v: Value, out: &mut Vec<(Value, Vec<Event>)>, orphan: &mut Vec<Event>| {
            let mut evs = std::mem::take(orphan);
            evs.push(e.clone());
            out.push((v, evs));
        };
        match k {
            Kind::Settings => {
                // Only the init line opening this run may hold it, or the
                // carried order would change.
                let out_len_is_one = out.len() == 1 && orphan.is_empty();
                let changed =
                    e.str("model").is_some_and(|m| Some(m) != first_model.as_deref()) || e.str("cwd").is_some_and(|c| Some(c) != first_cwd.as_deref());
                if e.body.keys().any(|k| !matches!(k.as_str(), "model" | "cwd" | "permission_mode" | "complete")) {
                    losses.add("settings-dropped", Some(k), "an init event holds only model, cwd and permissionMode");
                }
                if changed {
                    line(
                        init(e.str("model").map(String::from), e.str("cwd").map(String::from), e.str("permission_mode").map(String::from)),
                        &mut out,
                        &mut orphan,
                    );
                } else if let Some((_, evs)) = out.first_mut().filter(|(v, _)| v["subtype"] == "init").filter(|_| out_len_is_one) {
                    evs.push(e.clone());
                } else {
                    orphan.push(e.clone());
                }
            }
            Kind::Text | Kind::Compaction | Kind::PeerMessage => {
                let (role, text) = match k {
                    Kind::Compaction => {
                        let Some(s) = e.str("summary") else {
                            losses.add("compaction-boundary-dropped", Some(k), "no stream event for a compaction boundary");
                            orphan.push(e.clone());
                            continue;
                        };
                        losses.add("compaction-as-user-text", Some(k), "a compaction summary becomes a user message");
                        ("user", s.to_string())
                    }
                    Kind::PeerMessage => {
                        if crate::formats::pi_session::peer_has_more(e) {
                            losses.add(crate::formats::pi_session::PEER_FIELDS_DROPPED, Some(k), crate::formats::pi_session::PEER_FIELDS_DROPPED_DETAIL);
                        }
                        crate::formats::pi_session::peer_as_text(e)
                    }
                    _ => (e.str("role").unwrap_or("user"), e.str("text").unwrap_or("").to_string()),
                };
                match role {
                    "assistant" => {
                        assistant_text.push_str(&text);
                        let mut v =
                            json!({"type": "assistant", "message": {"role": "assistant", "content": [{"type": "text", "text": text}]}, "session_id": sid});
                        if let Some(g) = &e.group {
                            v["model_call_id"] = json!(g);
                        }
                        if let Some(t) = tms(e) {
                            v["timestamp_ms"] = json!(t);
                        }
                        line(v, &mut out, &mut orphan);
                    }
                    "user" => {
                        assistant_text.clear();
                        line(
                            json!({"type": "user", "message": {"role": "user", "content": [{"type": "text", "text": text}]}, "session_id": sid}),
                            &mut out,
                            &mut orphan,
                        );
                    }
                    _ => {
                        losses.add("system-text-dropped", Some(k), "stream-json has no system / developer message");
                        orphan.push(e.clone());
                    }
                }
            }
            Kind::Reasoning => {
                let text = e.str("text").unwrap_or("");
                if e.body.get("sealed").is_some() {
                    losses.add("reasoning-sealed-dropped", Some(k), "thinking events carry text only");
                }
                if text.is_empty() {
                    orphan.push(e.clone());
                    continue;
                }
                let mut d = json!({"type": "thinking", "subtype": "delta", "text": text, "session_id": sid});
                let mut c = json!({"type": "thinking", "subtype": "completed", "session_id": sid});
                if let Some(t) = tms(e) {
                    d["timestamp_ms"] = json!(t);
                    c["timestamp_ms"] = json!(t);
                }
                out.push((d, vec![]));
                line(c, &mut out, &mut orphan);
            }
            Kind::ToolCall => {
                let call_id = e.str("call_id").unwrap_or("").to_string();
                let name = e.str("name").unwrap_or("").to_string();
                let input = crate::formats::pi_session::tool_args_object(e, losses);
                let (key, body) = if e.str("tool_type") == Some("mcp") {
                    let server = e.str("server").unwrap_or("mcp");
                    (
                        "mcpToolCall".to_string(),
                        json!({"args": {"name": format!("{server}-{name}"), "args": input, "toolCallId": call_id, "providerIdentifier": server, "toolName": name}}),
                    )
                } else if KNOWN_TOOLS.contains(&name.as_str()) {
                    (format!("{name}ToolCall"), json!({"args": input}))
                } else {
                    ("function".to_string(), json!({"name": name, "arguments": input.to_string()}))
                };
                calls.insert(call_id.clone(), (key.clone(), body.clone()));
                let mut v = json!({"type": "tool_call", "subtype": "started", "call_id": call_id, "tool_call": {key: body}});
                if let Some(g) = &e.group {
                    v["model_call_id"] = json!(g);
                }
                v["session_id"] = json!(sid);
                if let Some(t) = tms(e) {
                    v["timestamp_ms"] = json!(t);
                }
                line(v, &mut out, &mut orphan);
            }
            Kind::ToolResult => {
                let call_id = e.str("call_id").unwrap_or("").to_string();
                let (text, media) = output_text(e.body.get("output"));
                if media {
                    losses.add("tool-result-media-dropped", Some(k), "media in tool output is dropped");
                }
                let is_err = e.body.get("is_error").and_then(Value::as_bool) == Some(true);
                let (key, mut body) = calls.get(&call_id).cloned().unwrap_or_else(|| {
                    losses.add("tool-result-unpaired", Some(k), "a result whose call is not in the ledger");
                    ("function".into(), json!({"name": "unknown", "arguments": "{}"}))
                });
                body["result"] = if is_err {
                    json!({"error": {"errorMessage": text}})
                } else if key == "shellToolCall" {
                    json!({"success": {"stdout": text, "stderr": "", "exitCode": 0}})
                } else {
                    json!({"success": {"content": text}})
                };
                let mut v = json!({"type": "tool_call", "subtype": "completed", "call_id": call_id, "tool_call": {key: body}, "session_id": sid});
                if let Some(t) = tms(e) {
                    v["timestamp_ms"] = json!(t);
                }
                line(v, &mut out, &mut orphan);
            }
            Kind::Usage => {
                if pending_usage.is_some() {
                    losses.add("usage-merged", Some(k), "one result line holds one usage");
                    orphan.push(e.clone());
                } else {
                    // Its numbers go into the next result line; the event is
                    // carried in place, so a carry trip keeps the order.
                    pending_usage = Some(e.clone());
                    orphan.push(e.clone());
                }
            }
            Kind::Turn if e.str("phase") == Some("end") => {
                let err = e.str("outcome").is_some_and(|o| o != "ok");
                let v = result_line(err, &assistant_text, pending_usage.as_ref(), Some(e));
                let mut evs = std::mem::take(&mut orphan);
                pending_usage = None;
                evs.push(e.clone());
                out.push((v, evs));
                assistant_text.clear();
            }
            Kind::Error => {
                let mut v = result_line(true, "", pending_usage.as_ref(), None);
                v["error"] = json!(e.str("message").unwrap_or(""));
                let mut evs = std::mem::take(&mut orphan);
                pending_usage = None;
                evs.push(e.clone());
                out.push((v, evs));
                if e.str("kind").is_some_and(|k| k != "result") || e.body.get("retryable").is_some() {
                    losses.add("error-fields-dropped", Some(k), "a failed result line holds only the message");
                }
            }
            Kind::Media => {
                losses.add("media-dropped", Some(k), "stream-json carries no images in messages");
                orphan.push(e.clone());
            }
            Kind::Turn => {
                losses.add("turn-dropped", Some(k), "only a turn's end has a stream event (result)");
                orphan.push(e.clone());
            }
            other => {
                losses.add(&format!("{}-dropped", other.as_str().replace('_', "-")), Some(other), "no stream-json event for it");
                orphan.push(e.clone());
            }
        }
    }
    if let Some(u) = pending_usage.take() {
        let v = result_line(false, &assistant_text, Some(&u), None);
        let evs = std::mem::take(&mut orphan);
        out.push((v, evs));
    }
    if !orphan.is_empty() {
        match out.last_mut() {
            Some((_, evs)) => evs.append(&mut orphan),
            None => out.push((init(first_model, first_cwd, None), std::mem::take(&mut orphan))),
        }
    }
    // A carrier rides on the last line of the run it renders, so a line with
    // no events of its own (a thinking delta) is never given one.
    out.into_iter()
        .enumerate()
        .map(|(i, (mut v, evs))| {
            if opts.carry && head_line == Some(i) {
                attach_with_agent(&mut v, &evs, l.agent());
            } else if opts.carry {
                carry::attach(&mut v, &[], &evs);
            }
            v
        })
        .collect()
}

/// A record's `raw` while it still matches the record ([`crate::util::current_raw`]).
/// Lines that share one id are one record, the array of the lines' values,
/// with the lines joined by newlines as `raw`: that `raw` matches while each
/// line matches its item (a line that is not JSON stands for `null`).
fn group_raw(n: &crate::model::Native) -> Option<&str> {
    let raw = n.raw.as_deref()?;
    if !raw.contains('\n') {
        return crate::util::current_raw(n);
    }
    let compact = n.record_text().map(jsonfmt::minify)?;
    let items = jsonfmt::items(&compact)?;
    let lines: Vec<&str> = raw.split('\n').collect();
    let same = lines.len() == items.len()
        && lines
            .iter()
            .zip(&items)
            .all(|(l, it)| crate::util::same_json(l, it) || (*it == "null" && serde_json::from_str::<serde::de::IgnoredAny>(l).is_err()));
    same.then_some(raw)
}
