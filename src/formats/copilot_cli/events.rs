//! `events.jsonl` (session directory, >= 0.0.378) and the flat
//! `<id>.jsonl` (0.0.342 - 0.0.377): the Copilot CLI event log.

use super::{DEFAULT_VERSION, NAME, V_DIR, V_FLAT, native, output_text, own, push_record, session_id, uuid};
use crate::carry;
use crate::formats::atif::jsonfmt;
use crate::formats::gemini_cli::{apply_agent, attach_with_agent, carried_agent};
use crate::formats::{ExportOptions, Exported, ImportOptions, Imported, Losses, Output};
use crate::ledger::{Ledger, LedgerSet};
use crate::model::{Agent, Audience, Event, Kind, Obj, Parent, Relation, Source};
use crate::util::{self, get_str};
use anyhow::{Context, Result, bail};
use base64::Engine;
use serde_json::{Map, Value, json};
use std::collections::{BTreeMap, HashMap, HashSet};
use std::path::{Path, PathBuf};

const B64: base64::engine::GeneralPurpose = base64::engine::general_purpose::STANDARD;
/// Lifecycle events that describe a subagent from its parent's side.
const PARENT_SIDE: [&str; 3] = ["subagent.started", "subagent.completed", "subagent.failed"];

/// One session to import: (variant, path relative to the input, events file).
fn sessions(input: &Path) -> Result<Vec<(&'static str, String, PathBuf)>> {
    if input.is_file() {
        let name = input.file_name().unwrap().to_string_lossy().to_string();
        let (v, rel) = if name == "events.jsonl" { (V_DIR, String::new()) } else { (V_FLAT, name) };
        return Ok(vec![(v, rel, input.to_path_buf())]);
    }
    let mut out = Vec::new();
    for e in walkdir::WalkDir::new(input).max_depth(2).sort_by_file_name() {
        let e = e?;
        if !e.file_type().is_file() {
            continue;
        }
        let rel = e.path().strip_prefix(input)?.to_string_lossy().replace('\\', "/");
        let name = e.file_name().to_string_lossy().to_string();
        if name == "events.jsonl" {
            let dir = rel.rsplit_once('/').map(|x| x.0.to_string()).unwrap_or_default();
            out.push((V_DIR, dir, e.path().to_path_buf()));
        } else if name.ends_with(".jsonl") && !rel.contains('/') {
            out.push((V_FLAT, rel, e.path().to_path_buf()));
        }
    }
    Ok(out)
}

pub(super) fn import(input: &Path, opts: &ImportOptions) -> Result<Imported> {
    let mut set = LedgerSet::new();
    let mut losses = Losses::default();
    let mut variant = String::new();
    for (v, rel, path) in sessions(input)? {
        let text = std::fs::read_to_string(&path).with_context(|| path.display().to_string())?;
        let mut extra = Obj::new();
        if v == V_DIR {
            extra.insert("dir".into(), json!(rel));
            // Everything else in the session directory, kept verbatim.
            let dir = path.parent().unwrap();
            let mut files = Map::new();
            if !input.is_file() {
                for f in walkdir::WalkDir::new(dir).sort_by_file_name().into_iter().flatten() {
                    if !f.file_type().is_file() {
                        continue;
                    }
                    let r = f.path().strip_prefix(dir)?.to_string_lossy().replace('\\', "/");
                    if r == "events.jsonl" || r == "SOURCE.md" || r == "expect.json" {
                        continue;
                    }
                    files.insert(r, json!(B64.encode(std::fs::read(f.path())?)));
                }
            }
            if let Some(y) = files.get("workspace.yaml").and_then(Value::as_str).and_then(|b| B64.decode(b).ok()) {
                let y = String::from_utf8_lossy(&y).to_string();
                extra.insert("workspace".into(), Value::Object(parse_yaml(&y)));
            }
            extra.insert("files".into(), Value::Object(files));
        } else {
            extra.insert("file".into(), json!(rel.clone()));
        }
        let stem = if v == V_DIR { rel.rsplit('/').next().unwrap_or(&rel).to_string() } else { rel.trim_end_matches(".jsonl").to_string() };
        let ledgers = import_log(v, &stem, &text, extra, opts, &mut losses);
        for l in ledgers {
            set.insert(l);
        }
        if variant.is_empty() {
            variant = v.to_string();
        }
    }
    if set.is_empty() {
        bail!("{}: no Copilot CLI events found", input.display());
    }
    Ok(Imported { set, losses: losses.sorted(), variant })
}

/// Minimal YAML for `workspace.yaml`: top-level `key: scalar` lines.
pub(super) fn parse_yaml(text: &str) -> Obj {
    let mut out = Obj::new();
    for line in text.lines() {
        if line.starts_with(' ') || line.starts_with('#') {
            continue;
        }
        let Some((k, v)) = line.split_once(':') else { continue };
        let v = v.trim();
        let val = if v.starts_with('"') && v.ends_with('"') && v.len() >= 2 {
            serde_json::from_str::<String>(v).map(Value::String).unwrap_or_else(|_| json!(&v[1..v.len() - 1]))
        } else if v.starts_with('\'') && v.ends_with('\'') && v.len() >= 2 {
            json!(v[1..v.len() - 1].replace("''", "'"))
        } else if v == "true" || v == "false" {
            json!(v == "true")
        } else if v.parse::<i64>().is_ok() {
            json!(v.parse::<i64>().unwrap())
        } else if v.is_empty() || v == "null" || v == "~" {
            Value::Null
        } else {
            json!(v)
        };
        out.insert(k.trim().to_string(), val);
    }
    out
}

/// A YAML scalar, quoted when plain style would change its meaning.
pub(super) fn yaml_scalar(s: &str) -> String {
    let plain_ok = !s.is_empty()
        && !s.starts_with([' ', '-', '?', ':', ',', '[', ']', '{', '}', '#', '&', '*', '!', '|', '>', '\'', '"', '%', '@', '`'])
        && !s.ends_with(' ')
        && !s.contains(": ")
        && !s.contains(" #")
        && !s.contains('\n')
        && !matches!(s, "true" | "false" | "null" | "~" | "yes" | "no" | "on" | "off")
        && s.parse::<f64>().is_err();
    if plain_ok { s.to_string() } else { serde_json::to_string(s).unwrap() }
}

struct Ctx {
    variant: &'static str,
    session: String,
    version: Option<String>,
    turn: Option<String>,
    calls: HashSet<String>,
    reasoning: HashSet<String>,
}

fn import_log(variant: &'static str, stem: &str, text: &str, mut extra: Obj, opts: &ImportOptions, _losses: &mut Losses) -> Vec<Ledger> {
    let (lines, final_nl) = crate::formats::cursor::raw_lines(text);
    if !final_nl {
        extra.insert("no_final_newline".into(), json!(true));
    }
    let parsed: Vec<Option<Value>> = lines.iter().map(|(_, l)| serde_json::from_str(l).ok()).collect();
    let start = parsed.iter().flatten().find(|v| get_str(v, &["type"]) == Some("session.start"));
    let session = start.and_then(|s| get_str(s, &["data", "sessionId"])).map(String::from).unwrap_or_else(|| stem.to_string());
    let version = start.and_then(|s| get_str(s, &["data", "copilotVersion"])).map(String::from);
    let created = start.and_then(|s| get_str(s, &["data", "startTime"]).or(get_str(s, &["timestamp"]))).map(String::from);
    let mut main = Ledger::new(Agent {
        id: session.clone(),
        created_at: created,
        source: Some(Source { format: NAME.into(), variant: Some(variant.into()), version: version.clone(), native_id: Some(session.clone()), extra }),
        parent: None,
        links: vec![],
        meta: Obj::new(),
    });
    let mut children: BTreeMap<String, Ledger> = BTreeMap::new();
    let mut started_at: HashMap<String, String> = HashMap::new(); // agentId -> parent event id
    let mut order: Vec<(String, u64)> = Vec::new();
    let mut ctx = Ctx { variant, session: session.clone(), version, turn: None, calls: HashSet::new(), reasoning: HashSet::new() };
    for ((off, line), v) in lines.iter().zip(parsed) {
        let anchor = format!("@{off}");
        let agent_key = v
            .as_ref()
            .filter(|v| !PARENT_SIDE.contains(&get_str(v, &["type"]).unwrap_or("")))
            .and_then(|v| get_str(v, &["agentId"]))
            .map(String::from)
            .unwrap_or_default();
        match order.last_mut() {
            Some((k, n)) if *k == agent_key => *n += 1,
            _ => order.push((agent_key.clone(), 1)),
        }
        let target: &mut Ledger = if agent_key.is_empty() {
            &mut main
        } else {
            children.entry(agent_key.clone()).or_insert_with(|| {
                let mut a = main.agent().clone();
                a.id = format!("{session}.{agent_key}");
                a.created_at = v.as_ref().and_then(|v| get_str(v, &["timestamp"])).map(String::from);
                let mut extra = Obj::new();
                extra.insert("agentId".into(), json!(agent_key));
                a.source = Some(Source {
                    format: NAME.into(),
                    variant: Some(variant.into()),
                    version: ctx.version.clone(),
                    native_id: Some(agent_key.clone()),
                    extra,
                });
                a.parent = Some(Parent { agent: session.clone(), event: started_at.get(&agent_key).cloned(), relation: Relation::Subagent });
                Ledger::new(a)
            })
        };
        let Some(v) = v else {
            push_record(target, &anchor, native(variant, &anchor, ctx.version.as_deref()).raw(*line), vec![], None);
            continue;
        };
        if opts.restore_carried && v.get(carry::KEY).is_some() {
            if let Some(a) = carried_agent(&v) {
                apply_agent(target.agent_mut(), &a);
            }
            let mut r = v.clone();
            if let Some(evs) = carry::take(&mut r, &[]) {
                for e in evs {
                    target.append_unique(e);
                }
                continue;
            }
        }
        let ty = get_str(&v, &["type"]).unwrap_or("").to_string();
        let data = v.get("data").cloned().unwrap_or(json!({}));
        if matches!(ty.as_str(), "session.start" | "session.resume")
            && let Some(cv) = get_str(&data, &["copilotVersion"])
        {
            ctx.version = Some(cv.to_string());
        }
        let nid = get_str(&v, &["id"]).map(String::from).unwrap_or(anchor);
        let ts = get_str(&v, &["timestamp"]).map(String::from);
        let evs = events_for(&ty, &data, &v, &ts, &mut ctx);
        if ty == "subagent.started" {
            let child = get_str(&v, &["agentId"]).or(get_str(&data, &["toolCallId"])).unwrap_or("").to_string();
            started_at.insert(child, nid.clone());
        }
        // The line is held as its own text (SPEC §4).
        let mut n = native(variant, &nid, ctx.version.as_deref());
        jsonfmt::hold(&mut n, Some(line), &v);
        push_record(target, &nid, n, evs, ts);
    }
    // Children whose subagent.started came after their first event.
    for (k, c) in children.iter_mut() {
        if let Some(p) = c.agent_mut().parent.as_mut()
            && p.event.is_none()
        {
            p.event = started_at.get(k).cloned();
        }
    }
    // workspace.yaml's name is the session title.
    let ws_name = main.agent().source.as_ref().and_then(|s| s.extra.get("workspace")).and_then(|w| w.get("name")).and_then(Value::as_str).map(String::from);
    let have = |n: &str| main.events.iter().any(|e| e.kind == Kind::Title && e.str("text") == Some(n));
    if let Some(n) = ws_name.filter(|n| !n.is_empty() && !have(n)) {
        let ts = main.agent().source.as_ref().and_then(|s| s.extra.get("workspace")).and_then(|w| get_str(w, &["updated_at"])).map(String::from);
        let e = Event::new("workspace.yaml#name", Kind::Title).with("text", n).ts(ts).native(native(variant, "workspace.yaml#name", ctx.version.as_deref()));
        main.append_unique(e);
    }
    if !children.is_empty() {
        let o: Vec<Value> = order.iter().map(|(k, n)| json!([k, n])).collect();
        if let Some(src) = main.agent_mut().source.as_mut().filter(|s| s.format == NAME) {
            src.extra.insert("order".into(), Value::Array(o));
        }
    }
    let mut out = vec![main];
    out.extend(children.into_values());
    out
}

fn events_for(ty: &str, d: &Value, env: &Value, ts: &Option<String>, ctx: &mut Ctx) -> Vec<Event> {
    let turn = get_str(d, &["turnId"]).map(String::from).or(ctx.turn.clone());
    let mk = |k: Kind| Event::new("", k).ts(ts.clone()).turn(turn.clone());
    let mut out = Vec::new();
    let str_of = |v: Option<&Value>| v.filter(|x| x.is_string()).cloned();
    match ty {
        "session.start" | "session.resume" | "session.context_changed" => {
            let ctxv = if ty == "session.context_changed" { Some(d) } else { d.get("context") };
            let mut e = mk(Kind::Settings);
            if let Some(m) = str_of(d.get("selectedModel")) {
                e = e.with("model", m);
            }
            if let Some(c) = ctxv {
                if let Some(x) = str_of(c.get("cwd")) {
                    e = e.with("cwd", x);
                }
                if let Some(x) = str_of(c.get("branch")) {
                    e = e.with("git_branch", x);
                }
            }
            if let Some(x) = str_of(d.get("reasoningEffort")) {
                e = e.with("effort", x);
            }
            if !e.body.is_empty() {
                out.push(e);
            }
        }
        "session.model_change" => {
            let mut e = mk(Kind::Settings).with("model", d.get("newModel").cloned().unwrap_or(Value::Null));
            if let Some(x) = str_of(d.get("reasoningEffort")) {
                e = e.with("effort", x);
            }
            out.push(e);
        }
        "session.mode_changed" => out.push(mk(Kind::Settings).with("permission_mode", d.get("newMode").cloned().unwrap_or(Value::Null))),
        "subagent.selected" => out.push(mk(Kind::Settings).with("agent", d.get("agentName").cloned().unwrap_or(Value::Null))),
        "session.info" => out.push(
            Event::new("", Kind::Text)
                .ts(ts.clone())
                .with("role", "system")
                .with("text", d.get("message").cloned().unwrap_or(json!("")))
                .audience(Audience::Record),
        ),
        "session.warning" => {
            out.push(Event::new("", Kind::Error).ts(ts.clone()).with("kind", "warning").with("message", d.get("message").cloned().unwrap_or(json!(""))))
        }
        "session.error" => {
            let mut e = Event::new("", Kind::Error)
                .ts(ts.clone())
                .with("kind", d.get("errorType").cloned().unwrap_or(json!("error")))
                .with("message", d.get("message").cloned().unwrap_or(json!("")));
            if let Some(s) = d.get("statusCode").filter(|s| s.is_number()) {
                e = e.with("status", s.clone());
            }
            out.push(e);
        }
        "session.truncation" => {
            let mut e = mk(Kind::Compaction).with("phase", "boundary").with("trigger", "auto");
            if let Some(n) = d.get("preTruncationTokensInMessages").filter(|n| n.is_number()) {
                e = e.with("tokens_before", n.clone());
            }
            if let Some(n) = d.get("postTruncationTokensInMessages").filter(|n| n.is_number()) {
                e = e.with("tokens_after", n.clone());
            }
            out.push(e);
        }
        "session.compaction_start" => {
            out.push(mk(Kind::Compaction).with("phase", "boundary").with("trigger", if get_str(d, &["trigger"]) == Some("manual") { "manual" } else { "auto" }))
        }
        "session.compaction_complete" => {
            if let Some(s) = d.get("summaryContent").filter(|s| s.is_string()) {
                out.push(mk(Kind::Compaction).with("phase", "summary").with("summary", s.clone()));
            }
        }
        "session.context_cleared" => out.push(mk(Kind::Compaction).with("phase", "boundary").with("trigger", "manual")),
        "session.title_changed" => {
            if let Some(t) = d.get("title").filter(|t| t.is_string()) {
                out.push(Event::new("", Kind::Title).ts(ts.clone()).with("text", t.clone()));
            }
        }
        "session.shutdown" => {
            if let Some(m) = d.get("modelMetrics").and_then(Value::as_object) {
                for (model, mm) in m {
                    let u = mm.get("usage").cloned().unwrap_or(json!({}));
                    let mut e = mk(Kind::Usage).with("model", model.as_str()).with("cumulative", true);
                    for (from, to) in [
                        ("inputTokens", "input"),
                        ("outputTokens", "output"),
                        ("cacheReadTokens", "cache_read"),
                        ("cacheWriteTokens", "cache_write"),
                        ("reasoningTokens", "reasoning"),
                    ] {
                        if let Some(n) = u.get(from).filter(|n| n.is_number()) {
                            e = e.with(to, n.clone());
                        }
                    }
                    // SPEC §3.1.1: Copilot's inputTokens counts cache reads and
                    // writes (54 + 198884 + 22998 = 221936 in a real session);
                    // neutral `input` does not.
                    let n = |k: &str| u.get(k).and_then(Value::as_u64);
                    if let Some(i) = n("inputTokens") {
                        let cached = n("cacheReadTokens").unwrap_or(0) + n("cacheWriteTokens").unwrap_or(0);
                        if cached <= i {
                            e = e.with("input", i - cached);
                        }
                    }
                    out.push(e);
                }
            }
        }
        "user.message" => {
            let text = d.get("transformedContent").filter(|t| t.as_str().is_some_and(|s| !s.is_empty())).or(d.get("content")).cloned().unwrap_or(json!(""));
            if text.as_str().is_some_and(|s| !s.is_empty()) {
                out.push(mk(Kind::Text).with("role", "user").with("text", text));
            }
            for a in d.get("attachments").and_then(Value::as_array).into_iter().flatten() {
                if get_str(a, &["type"]) == Some("blob") && a.get("data").is_some() {
                    let mut e = mk(Kind::Media)
                        .with("role", "user")
                        .with("media_type", a.get("mimeType").cloned().unwrap_or(json!("application/octet-stream")))
                        .with("data", a["data"].clone());
                    if let Some(n) = a.get("displayName") {
                        e = e.with("name", n.clone());
                    }
                    out.push(e);
                } else if let Some(p) = get_str(a, &["path"]).filter(|_| matches!(get_str(a, &["type"]), Some("file") | Some("directory"))) {
                    let mut e = mk(Kind::Media)
                        .with("role", "user")
                        .with(
                            "media_type",
                            a.get("mimeType").cloned().unwrap_or(json!(if get_str(a, &["type"]) == Some("directory") {
                                "inode/directory"
                            } else {
                                "application/octet-stream"
                            })),
                        )
                        .with("path", p);
                    if let Some(n) = a.get("displayName") {
                        e = e.with("name", n.clone());
                    }
                    out.push(e);
                }
            }
        }
        "system.message" => {
            let role = if get_str(d, &["role"]) == Some("developer") { "developer" } else { "system" };
            out.push(mk(Kind::Text).with("role", role).with("text", d.get("content").cloned().unwrap_or(json!(""))));
        }
        "system.notification" => out.push(mk(Kind::Text).with("role", "system").with("text", d.get("content").cloned().unwrap_or(json!("")))),
        "assistant.turn_start" => {
            ctx.turn = get_str(d, &["turnId"]).map(String::from);
            out.push(Event::new("", Kind::Turn).ts(ts.clone()).turn(ctx.turn.clone()).with("phase", "start"));
        }
        "assistant.turn_end" => out.push(mk(Kind::Turn).with("phase", "end").with("outcome", "ok")),
        "abort" => out.push(mk(Kind::Turn).with("phase", "end").with("outcome", "aborted").with("reason", d.get("reason").cloned().unwrap_or(Value::Null))),
        "assistant.message" | "assistant.reasoning" => {
            let group = get_str(d, &["messageId"]).or(get_str(d, &["reasoningId"])).map(String::from);
            let mk = |k: Kind| Event::new("", k).ts(ts.clone()).turn(turn.clone()).group(group.clone());
            let rtext = if ty == "assistant.reasoning" { str_of(d.get("content")) } else { str_of(d.get("reasoningText")) };
            let opaque = d
                .get("reasoningOpaque")
                .filter(|x| x.is_string())
                .map(|x| ("anthropic", x.clone()))
                .or_else(|| d.get("encryptedContent").filter(|x| x.is_string()).map(|x| ("openai", x.clone())));
            let rt = rtext.as_ref().and_then(Value::as_str).unwrap_or("").to_string();
            if (!rt.is_empty() || opaque.is_some())
                && ctx.reasoning.insert(format!("{}:{}:{}", turn.as_deref().unwrap_or(""), util::short_id(&rt, 16), opaque.is_some()))
            {
                let mut e = mk(Kind::Reasoning).with("text", rt.as_str());
                if let Some((vendor, data)) = opaque {
                    // SPEC §3.1.5: Anthropic's opaque data beside thinking text
                    // is the thinking block's signature; alone (redacted
                    // thinking) or OpenAI's, it is encrypted content.
                    let kind = if vendor == "anthropic" && !rt.is_empty() { "signature" } else { "encrypted" };
                    e = e.with("sealed", json!({"vendor": vendor, "kind": kind, "data": data}));
                    if rt.is_empty() {
                        e = e.with("redacted", true);
                    }
                }
                out.push(e);
            }
            if ty == "assistant.message" {
                if let Some(t) = d.get("content").and_then(Value::as_str).filter(|t| !t.is_empty()) {
                    out.push(mk(Kind::Text).with("role", "assistant").with("text", t));
                }
                for r in d.get("toolRequests").and_then(Value::as_array).into_iter().flatten() {
                    let id = get_str(r, &["toolCallId"]).unwrap_or("").to_string();
                    ctx.calls.insert(id.clone());
                    let mut e = mk(Kind::ToolCall)
                        .with("call_id", id.as_str())
                        .with("name", r.get("name").cloned().unwrap_or(json!("")))
                        .with("input", r.get("arguments").cloned().unwrap_or(json!({})));
                    if let Some(t) = r.get("type").filter(|t| t.is_string()) {
                        e = e.with("tool_type", t.clone());
                    }
                    if let Some(s) = r.get("mcpServerName").filter(|t| t.is_string()) {
                        e = e.with("server", s.clone()).with("tool_type", "mcp");
                    }
                    out.push(e);
                }
                if let Some(n) = d.get("outputTokens").filter(|n| n.is_number()) {
                    let mut e = mk(Kind::Usage).with("output", n.clone());
                    if let Some(m) = str_of(d.get("model")) {
                        e = e.with("model", m);
                    }
                    out.push(e);
                }
            }
        }
        "tool.execution_start" | "tool.user_requested" => {
            let id = get_str(d, &["toolCallId"]).unwrap_or("").to_string();
            if ctx.calls.insert(id.clone()) {
                let mut e = mk(Kind::ToolCall)
                    .with("call_id", id.as_str())
                    .with("name", d.get("toolName").cloned().unwrap_or(json!("")))
                    .with("input", d.get("arguments").cloned().unwrap_or(json!({})));
                if let Some(s) = d.get("mcpServerName").filter(|t| t.is_string()) {
                    e = e.with("server", s.clone()).with("tool_type", "mcp");
                }
                out.push(e);
            }
        }
        "tool.execution_complete" => {
            let id = get_str(d, &["toolCallId"]).unwrap_or("");
            let ok = d.get("success").and_then(Value::as_bool).unwrap_or(true);
            let output = match (d.get("result"), d.get("error")) {
                (Some(r), _) if r.is_string() => r.clone(),
                (Some(r), _) if r.get("content").is_some() => r["content"].clone(),
                (_, Some(e)) if e.is_string() => e.clone(),
                (_, Some(e)) => e.get("message").cloned().unwrap_or(json!(e.to_string())),
                (Some(r), _) => json!(r.to_string()),
                _ => json!(""),
            };
            let mut e = mk(Kind::ToolResult).with("call_id", id).with("output", output);
            if !ok {
                e = e.with("is_error", true);
            }
            out.push(e);
            let edits = d.get("fileEdits").and_then(Value::as_array).cloned().unwrap_or_default();
            for fe in &edits {
                let op = match get_str(fe, &["kind"]) {
                    Some("create") => "create",
                    Some("delete") => "delete",
                    _ => "edit",
                };
                let mut f = mk(Kind::FileChange).with("path", fe.get("path").cloned().unwrap_or(json!(""))).with("op", op);
                if edits.len() == 1
                    && let Some(dc) = d.get("result").and_then(|r| r.get("detailedContent")).filter(|x| x.is_string())
                {
                    f = f.with("diff", dc.clone());
                }
                out.push(f);
            }
        }
        "subagent.started" => {
            let call = get_str(d, &["toolCallId"]).unwrap_or("");
            let child = get_str(env, &["agentId"]).unwrap_or(call);
            let mut e = mk(Kind::Subagent).with("phase", "start").with("agent", format!("{}.{child}", ctx.session)).with("call_id", call);
            if let Some(n) = str_of(d.get("agentName")) {
                e = e.with("agent_type", n);
            }
            if let Some(n) = str_of(d.get("agentDescription")) {
                e = e.with("description", n);
            }
            out.push(e);
        }
        "subagent.completed" | "subagent.failed" => {
            let call = get_str(d, &["toolCallId"]).unwrap_or("");
            let child = get_str(env, &["agentId"]).unwrap_or(call);
            let mut e = mk(Kind::Subagent)
                .with("phase", "end")
                .with("agent", format!("{}.{child}", ctx.session))
                .with("call_id", call)
                .with("outcome", if ty == "subagent.failed" { "error" } else { "ok" });
            if let Some(err) = d.get("error").filter(|x| x.is_string()) {
                e = e.with("result", err.clone());
            }
            out.push(e);
        }
        _ => {}
    }
    let _ = ctx.variant;
    out
}

// ---------------------------------------------------------------------------
// export

pub(super) fn export(set: &LedgerSet, opts: &ExportOptions, variant: &str) -> Result<Exported> {
    let mut out = Output::default();
    let mut losses = Losses::default();
    let v: &'static str = if variant == V_FLAT { V_FLAT } else { V_DIR };
    // Children are written inside their root's log.
    let roots: Vec<&Ledger> = set.iter().filter(|l| l.agent().parent.as_ref().is_none_or(|p| set.get(&p.agent).is_none())).collect();
    // SPEC §2.3: Copilot CLI has no branch; each ledger's active path is
    // written, and an abandoned event rides on the next line written.
    let on_of: HashMap<String, Vec<bool>> = set
        .iter()
        .map(|l| (l.id().to_string(), crate::formats::active_path_only(l, &mut losses, "with carry they ride on the next line", |e| own(e, v).is_some())))
        .collect();
    let on = |l: &Ledger, i: usize| on_of.get(l.id()).is_none_or(|o| o[i]);
    for root in roots {
        let mut lines: Vec<String> = Vec::new();
        let own_root = root.agent().source.as_ref().is_some_and(|s| s.format == NAME && s.variant.as_deref() == Some(v));
        let extra = root.agent().source.as_ref().filter(|_| own_root).map(|s| s.extra.clone()).unwrap_or_default();
        let sid = session_id(root);
        let children: Vec<&Ledger> = set.iter().filter(|l| l.agent().parent.as_ref().is_some_and(|p| p.agent == root.id())).collect();
        let mut w = Writer::new(sid.clone(), opts, &mut losses);
        if !own_root {
            ensure_start(&mut w, root);
        }
        // Own records, interleaved as they were.
        let order: Vec<(String, u64)> = extra
            .get("order")
            .and_then(Value::as_array)
            .map(|a| a.iter().filter_map(|x| Some((x.get(0)?.as_str()?.to_string(), x.get(1)?.as_u64()?))).collect())
            .unwrap_or_default();
        let child_by_key: HashMap<String, &Ledger> = children
            .iter()
            .filter_map(|c| {
                c.agent().source.as_ref().filter(|s| s.format == NAME).and_then(|s| s.extra.get("agentId")).and_then(Value::as_str).map(|k| (k.to_string(), *c))
            })
            .collect();
        let mut cursors: HashMap<String, usize> = HashMap::new();
        let mut done_children: HashSet<String> = HashSet::new();
        let mut take_records = |key: &str, n: u64, w: &mut Writer, lines: &mut Vec<String>, set_ledger: &Ledger| {
            // Write n own records (and the foreign events between them) of `set_ledger`.
            let cur = cursors.entry(key.to_string()).or_default();
            let mut written = 0;
            while *cur < set_ledger.events.len() {
                let e = &set_ledger.events[*cur];
                match own(e, v) {
                    Some(nat) if nat.part == 0 => {
                        if written == n {
                            break;
                        }
                        w.flush(lines, set_ledger);
                        match (crate::util::current_raw(nat), nat.record_text()) {
                            (Some(r), _) => lines.push(r.to_string()),
                            (None, Some(r)) => lines.push(r.to_string()),
                            _ => {}
                        }
                        w.note_own(nat.record_json().as_ref());
                        written += 1;
                    }
                    Some(_) => {}
                    None if !on(set_ledger, *cur) => w.orphan.push(e.clone()),
                    None => w.push(e, if key.is_empty() { None } else { Some(key.to_string()) }),
                }
                *cur += 1;
            }
        };
        if own_root && !order.is_empty() {
            for (k, n) in &order {
                if k.is_empty() {
                    take_records("", *n, &mut w, &mut lines, root);
                } else if let Some(c) = child_by_key.get(k) {
                    take_records(k, *n, &mut w, &mut lines, c);
                    done_children.insert(c.id().to_string());
                }
            }
            // Events appended to a replayed child after its last record.
            for (k, c) in &child_by_key {
                if done_children.contains(c.id()) {
                    take_records(k, u64::MAX, &mut w, &mut lines, c);
                }
            }
        }
        // The rest: the root's remaining events, each foreign child placed
        // after the parent event that started it.
        let start_main = *cursors.get("").unwrap_or(&0);
        for i in start_main..root.events.len() {
            let e = &root.events[i];
            match own(e, v).filter(|_| own_root) {
                Some(nat) if nat.part == 0 => {
                    w.flush(&mut lines, root);
                    match (crate::util::current_raw(nat), nat.record_text()) {
                        (Some(r), _) => lines.push(r.to_string()),
                        (None, Some(r)) => lines.push(r.to_string()),
                        _ => {}
                    }
                    w.note_own(nat.record_json().as_ref());
                }
                Some(_) => {}
                None if !on(root, i) => w.orphan.push(e.clone()),
                None => w.push(e, None),
            }
            for c in &children {
                if done_children.contains(c.id()) {
                    continue;
                }
                let starts_here = c.agent().parent.as_ref().and_then(|p| p.event.as_deref()) == Some(e.id.as_str())
                    || (e.kind == Kind::Subagent && e.str("phase") == Some("start") && e.str("agent") == Some(c.id()));
                if starts_here {
                    w.flush(&mut lines, root);
                    let key = e.str("call_id").map(String::from).unwrap_or_else(|| c.id().to_string());
                    write_child(c, &key, &mut w, &mut lines, v, &on_of);
                    done_children.insert(c.id().to_string());
                }
            }
        }
        w.flush(&mut lines, root);
        for c in &children {
            if !done_children.contains(c.id()) {
                let key = c
                    .agent()
                    .source
                    .as_ref()
                    .and_then(|s| s.extra.get("agentId"))
                    .and_then(Value::as_str)
                    .map(String::from)
                    .unwrap_or_else(|| c.id().to_string());
                write_child(c, &key, &mut w, &mut lines, v, &on_of);
            }
        }
        w.close(&mut lines, root);
        let mut text = lines.join("\n");
        if extra.get("no_final_newline").is_none() && !lines.is_empty() {
            text.push('\n');
        }
        if v == V_FLAT {
            let path = extra.get("file").and_then(Value::as_str).map(String::from).unwrap_or_else(|| format!("{sid}.jsonl"));
            out.put(path, text);
            continue;
        }
        let dir = extra.get("dir").and_then(Value::as_str).map(String::from).unwrap_or_else(|| sid.clone());
        let at = |f: &str| if dir.is_empty() { f.to_string() } else { format!("{dir}/{f}") };
        out.put(at("events.jsonl"), text);
        let files = extra.get("files").and_then(Value::as_object).cloned().unwrap_or_default();
        for (name, b) in &files {
            out.put(at(name), B64.decode(b.as_str().unwrap_or("")).context("kept file")?);
        }
        if !own_root && !files.contains_key("workspace.yaml") {
            out.put(at("workspace.yaml"), workspace_yaml(root, &sid));
        }
    }
    Ok(Exported { output: out, losses: losses.sorted(), variant: v.into() })
}

fn write_child(c: &Ledger, key: &str, w: &mut Writer, lines: &mut Vec<String>, v: &str, on_of: &HashMap<String, Vec<bool>>) {
    // The parent's turn stays open around the subagent's own turns.
    let saved = (w.open_turn.take(), w.msg.take());
    let mut first = true;
    for (i, e) in c.events.iter().enumerate() {
        if own(e, v).is_none() && on_of.get(c.id()).is_some_and(|o| !o[i]) {
            w.orphan.push(e.clone());
            continue;
        }
        match own(e, v) {
            Some(nat) if nat.part == 0 => {
                w.flush(lines, c);
                match (crate::util::current_raw(nat), nat.record_text()) {
                    (Some(r), _) => lines.push(r.to_string()),
                    (None, Some(r)) => lines.push(r.to_string()),
                    _ => {}
                }
                w.note_own(nat.record_json().as_ref());
            }
            Some(_) => {}
            None => {
                if first {
                    w.header_for = Some(c.agent().clone());
                }
                w.push(e, Some(key.to_string()));
            }
        }
        first = false;
    }
    w.close_turn();
    w.flush(lines, c);
    (w.open_turn, w.msg) = saved;
}

fn workspace_yaml(l: &Ledger, sid: &str) -> String {
    let title = l.events.iter().rev().find(|e| e.kind == Kind::Title).and_then(|e| e.str("text").map(String::from));
    let cwd = l.events.iter().find_map(|e| (e.kind == Kind::Settings).then(|| e.str("cwd").map(String::from)).flatten()).unwrap_or_else(|| "/".into());
    let branch = l.events.iter().find_map(|e| (e.kind == Kind::Settings).then(|| e.str("git_branch").map(String::from)).flatten());
    let created = util::norm_ts(
        &l.agent().created_at.clone().or_else(|| l.events.iter().find_map(|e| e.ts.clone())).unwrap_or_else(|| "1970-01-01T00:00:00.000Z".into()),
    );
    let updated = l.events.iter().rev().find_map(|e| e.ts.as_deref().map(util::norm_ts)).unwrap_or_else(|| created.clone());
    let mut y = format!("id: {sid}\ncwd: {}\n", yaml_scalar(&cwd));
    if let Some(b) = branch {
        y.push_str(&format!("branch: {}\n", yaml_scalar(&b)));
    }
    if let Some(t) = title {
        y.push_str(&format!("name: {}\nuser_named: false\n", yaml_scalar(&t)));
    }
    y.push_str(&format!("summary_count: 0\ncreated_at: {created}\nupdated_at: {updated}\n"));
    y
}

/// Renders foreign events as Copilot CLI events, keeping the `parentId`
/// chain and the turn structure (`assistant.turn_start` → `assistant.message`
/// → `tool.execution_*` → `assistant.turn_end`).
struct Writer<'a> {
    sid: String,
    opts: &'a ExportOptions,
    losses: &'a mut Losses,
    last_id: Option<String>,
    last_ts: Option<String>,
    started: bool,
    /// The agent header to carry on the next rendered line (a foreign child).
    header_for: Option<Agent>,
    pending: Vec<(Value, Vec<Event>, Option<String>)>, // (line, events, agentId) not yet written
    open_turn: Option<(String, Option<String>)>,       // (turnId, agentId)
    msg: Option<usize>,                                // index in pending of the open assistant.message
    calls: HashMap<String, String>,                    // call id -> tool name
    turns: u64,
    orphan: Vec<Event>,
    model: Option<String>,
    agent_names: HashMap<String, String>,
}

impl<'a> Writer<'a> {
    fn new(sid: String, opts: &'a ExportOptions, losses: &'a mut Losses) -> Self {
        Writer {
            sid,
            opts,
            losses,
            last_id: None,
            last_ts: None,
            started: false,
            header_for: None,
            pending: vec![],
            open_turn: None,
            msg: None,
            calls: HashMap::new(),
            turns: 0,
            orphan: vec![],
            model: None,
            agent_names: HashMap::new(),
        }
    }

    fn note_own(&mut self, r: Option<&Value>) {
        if let Some(r) = r {
            self.started = true;
            self.last_id = get_str(r, &["id"]).map(String::from).or(self.last_id.take());
            self.last_ts = get_str(r, &["timestamp"]).map(String::from).or(self.last_ts.take());
        }
    }

    fn ts(&mut self, e: Option<&Event>) -> String {
        if let Some(t) = e.and_then(|e| e.ts.clone()) {
            self.last_ts = Some(util::norm_ts(&t));
        }
        self.last_ts.clone().unwrap_or_else(|| "1970-01-01T00:00:00.000Z".into())
    }

    fn line(&mut self, ty: &str, data: Value, seed: &str, e: Option<&Event>, agent: Option<String>) -> Value {
        let ts = self.ts(e);
        let id = uuid(seed);
        let mut v = json!({"type": ty, "data": data, "id": id, "timestamp": ts, "parentId": self.last_id.clone()});
        if let Some(a) = agent {
            v["agentId"] = json!(a);
        }
        self.last_id = Some(id);
        v
    }

    fn emit(&mut self, v: Value, evs: Vec<Event>, agent: Option<String>) {
        self.pending.push((v, evs, agent));
    }

    fn start(&mut self, root: &Ledger, first: Option<&Event>) {
        if self.started {
            return;
        }
        self.started = true;
        let settings: Vec<&Event> = root.events.iter().filter(|e| e.kind == Kind::Settings).collect();
        let model = settings.iter().find_map(|e| e.str("model"));
        let cwd = settings.iter().find_map(|e| e.str("cwd")).unwrap_or("/");
        let branch = settings.iter().find_map(|e| e.str("git_branch"));
        let start =
            util::norm_ts(&root.agent().created_at.clone().or_else(|| first.and_then(|f| f.ts.clone())).unwrap_or_else(|| "1970-01-01T00:00:00.000Z".into()));
        self.last_ts = Some(start.clone());
        let mut data = json!({"sessionId": self.sid, "version": 1, "producer": "copilot-agent", "copilotVersion": self.opts.target_version.clone().unwrap_or_else(|| DEFAULT_VERSION.into()), "startTime": start});
        if let Some(m) = model {
            data["selectedModel"] = json!(m);
            self.model = Some(m.to_string());
        }
        let mut c = json!({"cwd": cwd});
        if let Some(b) = branch {
            c["branch"] = json!(b);
        }
        data["context"] = c;
        let v = self.line("session.start", data, &format!("{}#session.start", root.id()), None, None);
        self.header_for = Some(root.agent().clone());
        self.emit(v, vec![], None);
    }

    fn close_turn(&mut self) {
        if let Some((t, agent)) = self.open_turn.take() {
            let v = self.line("assistant.turn_end", json!({"turnId": t}), &format!("{}#turn_end#{t}", self.sid), None, agent.clone());
            self.emit(v, vec![], agent);
        }
        self.msg = None;
    }

    fn ensure_message(&mut self, e: &Event, agent: &Option<String>, after_tools: bool) -> usize {
        let reuse = self.msg.is_some_and(|i| {
            let (v, evs, a) = &self.pending[i];
            a == agent
                && !after_tools
                && (e.group.is_none() || evs.first().is_none_or(|f| f.group.is_none() || f.group == e.group))
                && v["data"]["toolRequests"].as_array().is_none_or(|t| t.is_empty())
        });
        if reuse {
            return self.msg.unwrap();
        }
        if self.open_turn.is_some() {
            self.close_turn();
        }
        let t = self.turns.to_string();
        self.turns += 1;
        let ts_line = self.line("assistant.turn_start", json!({"turnId": t}), &format!("{}#turn_start", e.id), Some(e), agent.clone());
        self.emit(ts_line, vec![], agent.clone());
        self.open_turn = Some((t.clone(), agent.clone()));
        let mut data = json!({"messageId": uuid(&format!("{}#message", e.id)), "content": ""});
        if let Some(m) = &self.model {
            data["model"] = json!(m);
        }
        data["turnId"] = json!(t);
        let v = self.line("assistant.message", data, &e.id, Some(e), agent.clone());
        self.emit(v, vec![], agent.clone());
        self.msg = Some(self.pending.len() - 1);
        self.pending.len() - 1
    }

    fn push(&mut self, e: &Event, agent: Option<String>) {
        let k = e.kind;
        let attach = |w: &mut Self, i: usize, e: &Event| {
            let mut o = std::mem::take(&mut w.orphan);
            w.pending[i].1.append(&mut o);
            w.pending[i].1.push(e.clone());
        };
        match k {
            Kind::Text | Kind::Compaction | Kind::PeerMessage => {
                let (role, text) = match k {
                    Kind::Compaction => match e.str("summary") {
                        Some(s) => {
                            self.losses.add("compaction-as-user-text", Some(k), "a compaction summary becomes a user message");
                            ("user", s.to_string())
                        }
                        None => {
                            self.losses.add("compaction-boundary-dropped", Some(k), "a compaction boundary has no event");
                            self.orphan.push(e.clone());
                            return;
                        }
                    },
                    Kind::PeerMessage => {
                        if crate::formats::pi_session::peer_has_more(e) {
                            self.losses.add(crate::formats::pi_session::PEER_FIELDS_DROPPED, Some(k), crate::formats::pi_session::PEER_FIELDS_DROPPED_DETAIL);
                        }
                        crate::formats::pi_session::peer_as_text(e)
                    }
                    _ => (e.str("role").unwrap_or("user"), e.str("text").unwrap_or("").to_string()),
                };
                match role {
                    "assistant" => {
                        let i = self.ensure_message(e, &agent, false);
                        let cur = self.pending[i].0["data"]["content"].as_str().unwrap_or("").to_string();
                        self.pending[i].0["data"]["content"] = json!(cur + &text);
                        attach(self, i, e);
                    }
                    _ if e.effective_audience() == Audience::Record => {
                        // A notice for people, not the model.
                        let v = self.line("session.info", json!({"infoType": "notice", "message": text}), &e.id, Some(e), agent.clone());
                        self.emit(v, vec![], agent.clone());
                        let i = self.pending.len() - 1;
                        attach(self, i, e);
                    }
                    "system" | "developer" => {
                        self.close_turn();
                        let v = self.line("system.message", json!({"content": text, "role": role}), &e.id, Some(e), agent.clone());
                        self.emit(v, vec![], agent.clone());
                        let i = self.pending.len() - 1;
                        attach(self, i, e);
                    }
                    _ => {
                        self.close_turn();
                        let v = self.line("user.message", json!({"content": text}), &e.id, Some(e), agent.clone());
                        self.emit(v, vec![], agent.clone());
                        let i = self.pending.len() - 1;
                        attach(self, i, e);
                    }
                }
            }
            Kind::Media => {
                let (Some(d), true) = (e.str("data"), e.str("role") != Some("assistant")) else {
                    self.losses.add("media-dropped", Some(k), "only inline user media becomes a blob attachment");
                    self.orphan.push(e.clone());
                    return;
                };
                let mut blob = json!({"type": "blob", "mimeType": e.str("media_type").unwrap_or("application/octet-stream"), "data": d});
                if let Some(n) = e.str("name") {
                    blob["displayName"] = json!(n);
                }
                let last_user = self.pending.len().checked_sub(1).filter(|&i| self.pending[i].0["type"] == "user.message" && self.pending[i].2 == agent);
                let i = match last_user {
                    Some(i) => i,
                    None => {
                        self.close_turn();
                        let v = self.line("user.message", json!({"content": ""}), &e.id, Some(e), agent.clone());
                        self.emit(v, vec![], agent.clone());
                        self.pending.len() - 1
                    }
                };
                let data = &mut self.pending[i].0["data"];
                if data.get("attachments").is_none() {
                    data["attachments"] = json!([]);
                }
                data["attachments"].as_array_mut().unwrap().push(blob);
                attach(self, i, e);
            }
            Kind::Reasoning => {
                let text = e.str("text").unwrap_or("");
                if e.body.get("sealed").is_some() {
                    self.losses.add("reasoning-sealed-dropped", Some(k), "opaque reasoning is session-bound and stripped on resume");
                }
                if text.is_empty() {
                    self.orphan.push(e.clone());
                    return;
                }
                let i = self.ensure_message(e, &agent, false);
                let cur = self.pending[i].0["data"]["reasoningText"].as_str().map(String::from);
                self.pending[i].0["data"]["reasoningText"] = json!(match cur {
                    Some(c) => format!("{c}\n\n{text}"),
                    None => text.to_string(),
                });
                attach(self, i, e);
            }
            Kind::ToolCall => {
                let i = self.ensure_message(e, &agent, false);
                let id = e.str("call_id").unwrap_or("").to_string();
                let name = e.str("name").unwrap_or("").to_string();
                self.calls.insert(id.clone(), name.clone());
                let args = crate::formats::pi_session::tool_args_object(e, self.losses);
                let mut r = json!({"toolCallId": id, "name": name, "arguments": args.clone()});
                if let Some(s) = e.str("server") {
                    r["mcpServerName"] = json!(s);
                    r["mcpToolName"] = json!(name);
                }
                let data = &mut self.pending[i].0["data"];
                if data.get("toolRequests").is_none() {
                    data["toolRequests"] = json!([]);
                }
                data["toolRequests"].as_array_mut().unwrap().push(r);
                attach(self, i, e);
                // Tool execution follows the message.
                let mut d = json!({"toolCallId": id, "toolName": name, "arguments": args});
                if let Some(t) = self.open_turn.as_ref().map(|t| t.0.clone()) {
                    d["turnId"] = json!(t);
                }
                let v = self.line("tool.execution_start", d, &format!("{}#start", e.id), Some(e), agent.clone());
                self.emit(v, vec![], agent.clone());
                self.msg = Some(i);
            }
            Kind::ToolResult => {
                let id = e.str("call_id").unwrap_or("").to_string();
                let (text, media) = output_text(e.body.get("output"));
                if media {
                    self.losses.add("tool-result-media-dropped", Some(k), "media in tool output is dropped");
                }
                if !self.calls.contains_key(&id) {
                    self.losses.add("tool-result-unpaired", Some(k), "a result whose call is not in the ledger");
                }
                let ok = e.body.get("is_error").and_then(Value::as_bool) != Some(true);
                let mut d = json!({"toolCallId": id, "success": ok});
                if let Some(t) = self.open_turn.as_ref().map(|t| t.0.clone()) {
                    d["turnId"] = json!(t);
                }
                if ok {
                    d["result"] = json!({"content": text});
                } else {
                    d["error"] = json!({"message": text});
                }
                let v = self.line("tool.execution_complete", d, &e.id, Some(e), agent.clone());
                self.emit(v, vec![], agent.clone());
                let i = self.pending.len() - 1;
                attach(self, i, e);
                // Text after tool results is a new model call.
                if let Some(m) = self.msg
                    && self.pending[m].0["data"]["toolRequests"].as_array().is_some_and(|t| !t.is_empty())
                {
                    self.msg = None;
                }
            }
            Kind::Usage => {
                if e.body.get("cumulative").and_then(Value::as_bool) == Some(true) {
                    self.losses.add("usage-cumulative-dropped", Some(k), "session totals live in session.shutdown, which is not synthesized");
                    self.orphan.push(e.clone());
                    return;
                }
                match self.msg {
                    Some(i) if self.pending[i].0["data"].get("outputTokens").is_none() && e.body.get("output").is_some() => {
                        self.pending[i].0["data"]["outputTokens"] = e.body["output"].clone();
                        if e.body.keys().any(|k| !matches!(k.as_str(), "output" | "model")) {
                            self.losses.add("usage-fields-dropped", Some(k), "a message records only its output tokens");
                        }
                        attach(self, i, e);
                    }
                    _ => {
                        self.losses.add("usage-dropped", Some(k), "usage with no message to hold it");
                        self.orphan.push(e.clone());
                    }
                }
            }
            Kind::Settings => {
                let model = e.str("model").map(String::from);
                if let Some(m) = model.filter(|m| Some(m.as_str()) != self.model.as_deref() && self.started) {
                    self.close_turn();
                    let mut d = json!({"newModel": m});
                    if let Some(p) = &self.model {
                        d["previousModel"] = json!(p);
                    }
                    let v = self.line("session.model_change", d, &e.id, Some(e), agent.clone());
                    self.emit(v, vec![], agent.clone());
                    let i = self.pending.len() - 1;
                    attach(self, i, e);
                    self.model = Some(m);
                } else {
                    if e.body.keys().any(|k| !matches!(k.as_str(), "model" | "cwd" | "git_branch" | "complete")) {
                        self.losses.add("settings-dropped", Some(k), "only model, cwd and branch have a place");
                    }
                    self.orphan.push(e.clone());
                }
            }
            Kind::Error => {
                if e.body.get("retryable").is_some() {
                    self.losses.add("error-fields-dropped", Some(k), "session.error has no retryable flag");
                }
                self.close_turn();
                let v = self.line(
                    "session.error",
                    json!({"errorType": e.str("kind").unwrap_or("error"), "message": e.str("message").unwrap_or("")}),
                    &e.id,
                    Some(e),
                    agent.clone(),
                );
                self.emit(v, vec![], agent.clone());
                let i = self.pending.len() - 1;
                attach(self, i, e);
            }
            Kind::Turn => {
                if e.str("phase") == Some("end") && self.open_turn.is_some() {
                    self.orphan.push(e.clone());
                    self.close_turn();
                    let i = self.pending.len() - 1;
                    let mut o = std::mem::take(&mut self.orphan);
                    self.pending[i].1.append(&mut o);
                } else {
                    self.orphan.push(e.clone());
                }
            }
            Kind::Subagent => {
                let call = e.str("call_id").unwrap_or("").to_string();
                let key = if call.is_empty() { e.str("agent").unwrap_or("").to_string() } else { call.clone() };
                let (ty, mut d) = if e.str("phase") == Some("start") {
                    let ty_name = e.str("agent_type").unwrap_or("task");
                    self.agent_names.insert(key.clone(), ty_name.to_string());
                    (
                        "subagent.started",
                        json!({"toolCallId": call, "agentName": ty_name, "agentDisplayName": ty_name, "agentDescription": e.str("description").unwrap_or("")}),
                    )
                } else {
                    let ty_name = e.str("agent_type").map(String::from).or_else(|| self.agent_names.get(&key).cloned()).unwrap_or_else(|| "task".into());
                    let ty_name = ty_name.as_str();
                    let failed = e.str("outcome").is_some_and(|o| matches!(o, "error" | "failed" | "aborted"));
                    let mut d = json!({"toolCallId": call, "agentName": ty_name, "agentDisplayName": ty_name});
                    if failed {
                        d["error"] = json!(e.str("result").unwrap_or("failed"));
                    }
                    (if failed { "subagent.failed" } else { "subagent.completed" }, d)
                };
                if d["toolCallId"] == "" {
                    d["toolCallId"] = json!(key);
                }
                let v = self.line(ty, d, &e.id, Some(e), Some(key));
                self.emit(v, vec![], agent.clone());
                let i = self.pending.len() - 1;
                attach(self, i, e);
            }
            Kind::Title => self.orphan.push(e.clone()),
            other => {
                self.losses.add(&format!("{}-dropped", other.as_str().replace('_', "-")), Some(other), "no Copilot CLI event for it");
                self.orphan.push(e.clone());
            }
        }
    }

    /// Write out what is pending (with carriers).
    fn flush(&mut self, lines: &mut Vec<String>, owner: &Ledger) {
        if self.pending.is_empty() && self.orphan.is_empty() {
            return;
        }
        if !self.orphan.is_empty() {
            match self.pending.last_mut() {
                Some(p) => p.1.append(&mut self.orphan),
                None => return, // keep them for the next line
            }
        }
        let header = self.header_for.take();
        for (i, (mut v, evs, _)) in std::mem::take(&mut self.pending).into_iter().enumerate() {
            if self.opts.carry {
                match (&header, i) {
                    (Some(a), 0) => attach_with_agent(&mut v, &evs, a),
                    // A line the exporter synthesized (turn bounds, tool
                    // execution start) carries an empty list, so a carry
                    // trip does not re-import it as new events.
                    _ if evs.is_empty() => {
                        let h = carry::record_hash(&v);
                        v[carry::KEY] = json!({"v": crate::model::SPEC_VERSION, "h": h, "events": []});
                    }
                    _ => carry::attach(&mut v, &[], &evs),
                }
            }
            lines.push(serde_json::to_string(&v).unwrap());
        }
        let _ = owner;
        self.msg = None;
    }

    fn close(&mut self, lines: &mut Vec<String>, owner: &Ledger) {
        self.close_turn();
        self.flush(lines, owner);
    }
}

/// Make sure a foreign root gets a `session.start` before anything else.
fn ensure_start(w: &mut Writer, root: &Ledger) {
    w.start(root, root.events.first());
}
