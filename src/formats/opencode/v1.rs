//! opencode 1.x messages and parts (`packages/schema/src/v1/session.ts`,
//! `MessageV2` in 1.x) → neutral events.

use super::{Container, MapCtx, MsgSrc, get_ms, s, stamp, ts};
use crate::carry;
use crate::formats::Losses;
use crate::model::{Audience, Event, Kind};
use crate::util::OrderedMap;
use serde_json::{Value, json};

/// The session info's neutral facts.
pub fn session_events(info: &Value) -> Vec<Event> {
    let created = get_ms(info, &["time", "created"]);
    let mut set = Event::new("", Kind::Settings).ts(ts(created));
    if let Some(d) = s(info, "directory") {
        set = set.with("cwd", d);
    }
    if let Some(a) = s(info, "agent") {
        set = set.with("agent", a);
    }
    if let Some(m) = info.get("model") {
        if let Some(id) = s(m, "id").or_else(|| s(m, "modelID")) {
            set = set.with("model", id);
        }
        if let Some(p) = s(m, "providerID") {
            set = set.with("provider", p);
        }
    }
    let mut out = vec![set];
    if let Some(t) = s(info, "title").filter(|t| !super::is_default_title(t)) {
        out.push(Event::new("", Kind::Title).ts(ts(created)).with("text", t));
    }
    out
}

/// A reasoning part's provider signature or encrypted content, if any.
fn sealed(meta: Option<&Value>) -> Option<Value> {
    // AI SDK provider metadata: `{<provider>: {signature | reasoningEncryptedContent}}`.
    let meta = meta?.as_object()?;
    meta.iter().find_map(|(vendor, st)| super::v2::sealed_of(Some(st), vendor))
}

fn file_media(p: &Value, role: &str, t: Option<String>) -> Event {
    let mut e = Event::new("", Kind::Media).ts(t).with("role", role).with("media_type", p.get("mime").cloned().unwrap_or(Value::Null));
    let url = s(p, "url").unwrap_or("");
    if let Some(rest) = url.strip_prefix("data:") {
        if let Some((_, data)) = rest.split_once(";base64,") {
            e = e.with("data", data);
        } else {
            e = e.with("url", url);
        }
    } else if let Some(path) = url.strip_prefix("file://") {
        e = e.with("path", path).with("url", url);
    } else {
        e = e.with("url", url);
    }
    if let Some(n) = p.get("filename") {
        e = e.with("name", n.clone());
    }
    e
}

/// Neutral events for one part (unstamped).
fn part_events(p: &Value, role: &str, summary: bool, msg_t: Option<String>) -> Vec<Event> {
    let pt = ts(get_ms(p, &["time", "start"])).or(msg_t.clone());
    match s(p, "type").unwrap_or("") {
        "text" => {
            let text = p.get("text").cloned().unwrap_or(json!(""));
            if summary && role == "assistant" {
                return vec![Event::new("", Kind::Compaction).ts(pt).with("phase", "summary").with("summary", text)];
            }
            if role == "user"
                && let Some(peer) = text.as_str().and_then(super::peer_unwrap)
            {
                return vec![peer.ts(pt)];
            }
            let mut e = Event::new("", Kind::Text).ts(pt).with("role", role).with("text", text);
            if p.get("synthetic").and_then(Value::as_bool) == Some(true) {
                e = e.with("synthetic", true);
            }
            if p.get("ignored").and_then(Value::as_bool) == Some(true) {
                e = e.audience(Audience::Record);
            }
            vec![e]
        }
        "reasoning" => {
            let mut e = Event::new("", Kind::Reasoning).ts(pt).with("text", p.get("text").cloned().unwrap_or(json!("")));
            if let Some(sl) = sealed(p.get("metadata")) {
                e = e.with("sealed", sl);
            }
            vec![e]
        }
        "file" => vec![file_media(p, role, pt)],
        "tool" => {
            let call = s(p, "callID").unwrap_or("").to_string();
            let name = s(p, "tool").unwrap_or("").to_string();
            let st = p.get("state").cloned().unwrap_or(Value::Null);
            let status = s(&st, "status").unwrap_or("");
            let input = st.get("input").cloned().unwrap_or(json!({}));
            let start = ts(get_ms(&st, &["time", "start"])).or(msg_t.clone());
            let mut out = vec![
                Event::new("", Kind::ToolCall)
                    .ts(start.clone())
                    .with("call_id", call.as_str())
                    .with("name", name.as_str())
                    .with("input", input.clone())
                    .with("tool_type", "function"),
            ];
            let md = st.get("metadata");
            let child = md.and_then(|m| s(m, "sessionId").or_else(|| s(m, "sessionID"))).map(str::to_string);
            if let Some(child) = child {
                out.push(
                    Event::new("", Kind::Subagent)
                        .ts(start.clone())
                        .with("phase", "start")
                        .with("agent", child.as_str())
                        .with("call_id", call.as_str())
                        .with("agent_type", input.get("subagent_type").or_else(|| input.get("agent")).cloned().unwrap_or(Value::Null))
                        .with("description", input.get("description").cloned().unwrap_or(Value::Null)),
                );
            }
            let end = ts(get_ms(&st, &["time", "end"])).or(start);
            match status {
                "completed" => {
                    let mut out_v = st.get("output").cloned().unwrap_or(json!(""));
                    let atts: Vec<Value> = st
                        .get("attachments")
                        .and_then(Value::as_array)
                        .into_iter()
                        .flatten()
                        .map(|a| {
                            let e = file_media(a, "tool", None);
                            let mut m = json!({"type": "media"});
                            for (k, v) in e.body {
                                if k != "role" {
                                    m[k] = v;
                                }
                            }
                            m
                        })
                        .collect();
                    if !atts.is_empty() {
                        let mut parts = vec![json!({"type": "text", "text": out_v})];
                        parts.extend(atts);
                        out_v = Value::Array(parts);
                    }
                    out.push(Event::new("", Kind::ToolResult).ts(end).with("call_id", call.as_str()).with("output", out_v));
                }
                "error" => out.push(
                    Event::new("", Kind::ToolResult)
                        .ts(end)
                        .with("call_id", call.as_str())
                        .with("output", st.get("error").cloned().unwrap_or(json!("")))
                        .with("is_error", true),
                ),
                _ => {}
            }
            out
        }
        "compaction" => vec![
            Event::new("", Kind::Compaction)
                .ts(pt)
                .with("phase", "boundary")
                .with("trigger", if p.get("auto").and_then(Value::as_bool) == Some(true) { "auto" } else { "manual" }),
        ],
        "retry" => {
            let err = p.get("error").cloned().unwrap_or(Value::Null);
            let data = err.get("data").cloned().unwrap_or(Value::Null);
            vec![
                Event::new("", Kind::Error)
                    .ts(ts(get_ms(p, &["time", "created"])).or(msg_t))
                    .with("kind", err.get("name").cloned().unwrap_or(json!("APIError")))
                    .with("message", data.get("message").cloned().unwrap_or(Value::Null))
                    .with("retryable", true)
                    .with("status", data.get("statusCode").cloned().unwrap_or(Value::Null)),
            ]
        }
        "snapshot" => vec![Event::new("", Kind::FileChange).ts(pt).with("op", "snapshot").with("snapshot", p.get("snapshot").cloned().unwrap_or(Value::Null))],
        "patch" => {
            let files: Vec<Value> = p.get("files").and_then(Value::as_array).cloned().unwrap_or_default();
            if files.is_empty() {
                return vec![Event::new("", Kind::FileChange).ts(pt).with("op", "snapshot").with("snapshot", p.get("hash").cloned().unwrap_or(Value::Null))];
            }
            files
                .into_iter()
                .map(|f| {
                    Event::new("", Kind::FileChange)
                        .ts(pt.clone())
                        .with("path", f)
                        .with("op", "edit")
                        .with("snapshot", p.get("hash").cloned().unwrap_or(Value::Null))
                })
                .collect()
        }
        // step-start, step-finish, agent, subtask: no neutral meaning of their own.
        _ => vec![Event::new("", Kind::Native).ts(pt)],
    }
}

/// Does this 1.x version store `tokens.output` without reasoning? Verified for
/// 1.19.3 (and not for 1.1.8, 1.2.0, 1.2.21); versions in between are not.
pub fn output_excludes_reasoning(version: Option<&str>) -> bool {
    let Some(v) = version else { return false };
    let mut it = v.split('.').map(|p| p.parse::<u64>().unwrap_or(0));
    let (major, minor) = (it.next().unwrap_or(0), it.next().unwrap_or(0));
    major == 1 && minor >= 19
}

/// Neutral events for a V1 message and its parts, stamped.
pub fn message_events(m: &MsgSrc, ctx: &mut MapCtx, container: Container, version: Option<&str>, losses: &mut Losses) -> Vec<Event> {
    let _ = losses;
    let info = &m.info.value;
    let id = m.info.id.as_str();
    let role = s(info, "role").unwrap_or("user");
    let t = ts(get_ms(info, &["time", "created"]));
    let summary = info.get("summary").and_then(Value::as_bool) == Some(true);
    let turn = if role == "user" { Some(id.to_string()) } else { s(info, "parentID").map(str::to_string).or(ctx.turn.clone()) };
    if role == "user" {
        ctx.turn = Some(id.to_string());
    }
    let mut out: Vec<Event> = Vec::new();

    let mut info_events: Vec<Event> = Vec::new();
    if role == "user" {
        let model = info.get("model").map(|mo| (s(mo, "providerID").unwrap_or("").to_string(), s(mo, "modelID").unwrap_or("").to_string()));
        let agent = s(info, "agent").map(str::to_string);
        let mut set = Event::new("", Kind::Settings).ts(t.clone());
        let mut changed = false;
        if model.is_some() && model != ctx.model {
            let (p, mo) = model.clone().unwrap();
            set = set.with("model", mo).with("provider", p);
            if let Some(v) = info.get("model").and_then(|x| s(x, "variant")) {
                set = set.with("effort", v);
            }
            ctx.model = model;
            changed = true;
        }
        if agent.is_some() && agent != ctx.agent {
            set = set.with("agent", agent.clone().unwrap());
            ctx.agent = agent;
            changed = true;
        }
        if let Some(sys) = s(info, "system") {
            set = set.with("system_prompt", sys);
            changed = true;
        }
        if let Some(tools) = info.get("tools").and_then(Value::as_object) {
            let on: Vec<Value> = tools.iter().filter(|(_, v)| v.as_bool() == Some(true)).map(|(k, _)| json!(k)).collect();
            set = set.with("tools", Value::Array(on));
            changed = true;
        }
        info_events.push(if changed { set } else { Event::new("", Kind::Native).ts(t.clone()) });
    } else {
        let done = ts(get_ms(info, &["time", "completed"])).or(t.clone());
        if let Some(tok) = info.get("tokens") {
            // 1.19 stores output without reasoning (`getUsage`: `outputTokens -
            // reasoningTokens`); 1.1 and 1.2 stored the AI SDK's output, reasoning
            // included, which is the neutral spelling (SPEC §3.1.1).
            let output = match (tok.get("output").and_then(Value::as_f64), tok.get("reasoning").and_then(Value::as_f64)) {
                (Some(o), Some(r)) if output_excludes_reasoning(version) => super::js_num(o + r),
                _ => tok.get("output").cloned().unwrap_or(Value::Null),
            };
            let mut u = Event::new("", Kind::Usage)
                .ts(done.clone())
                .with("model", info.get("modelID").cloned().unwrap_or(Value::Null))
                .with("input", tok.get("input").cloned().unwrap_or(Value::Null))
                .with("output", output)
                .with("reasoning", tok.get("reasoning").cloned().unwrap_or(Value::Null))
                .with("cache_read", tok.get("cache").and_then(|c| c.get("read")).cloned().unwrap_or(Value::Null))
                .with("cache_write", tok.get("cache").and_then(|c| c.get("write")).cloned().unwrap_or(Value::Null));
            if let Some(c) = info.get("cost") {
                u = u.with("cost_usd", c.clone());
            }
            info_events.push(u);
        }
        if let Some(err) = info.get("error") {
            let data = err.get("data").cloned().unwrap_or(Value::Null);
            info_events.push(
                Event::new("", Kind::Error)
                    .ts(done)
                    .with("kind", err.get("name").cloned().unwrap_or(Value::Null))
                    .with("message", data.get("message").cloned().unwrap_or(Value::Null))
                    .with("status", data.get("statusCode").cloned().unwrap_or(Value::Null)),
            );
        }
        if info_events.is_empty() {
            info_events.push(Event::new("", Kind::Native).ts(t.clone()));
        }
    }
    let info_events = stamp(info_events, &m.info, container, version);
    if role == "user" {
        out.extend(info_events.clone());
    }
    for p in &m.parts {
        let evs = part_events(&p.value, role, summary, t.clone());
        out.extend(stamp(evs, p, container, version));
    }
    if role != "user" {
        out.extend(info_events);
    }
    // A group re-assembles a message of several content events (SPEC §2.2).
    let content = out.iter().filter(|e| !matches!(e.kind, Kind::Native | Kind::Settings | Kind::Usage)).count();
    let grouped = role == "assistant" || content > 1;
    for e in &mut out {
        if grouped {
            e.group = Some(id.to_string());
        }
        e.turn = turn.clone();
    }
    out
}

/// Where a 1.x message's carrier sits: `metadata.protoledger` on its first
/// text, reasoning or tool part (fields opencode declares, so 1.19's schema
/// decode keeps them), else a top-level `protoledger` on the message info
/// (kept by 1.1/1.2, which store records as given). The hash covers the info
/// and all its parts, carriers stripped.
pub fn carrier_part(parts: &[Value]) -> Option<usize> {
    parts.iter().position(|p| matches!(s(p, "type"), Some("text") | Some("reasoning") | Some("tool")))
}

pub fn carrier_hash(info: &Value, parts: &[Value]) -> String {
    let mut i = info.clone();
    carry::strip(&mut i);
    let ps: Vec<Value> = parts
        .iter()
        .map(|p| {
            let mut p = p.clone();
            carry::strip(&mut p);
            // An empty `metadata` made only to hold the carrier goes too.
            if p.get("metadata").and_then(Value::as_object).is_some_and(|m| m.is_empty()) {
                p.as_object_mut().unwrap().remove_ordered("metadata");
            }
            p
        })
        .collect();
    carry::record_hash(&json!({"info": i, "parts": ps}))
}

/// Attach a carrier to a rendered 1.x message.
pub fn attach_carrier(info: &mut Value, parts: &mut [Value], carrier: Value) {
    match carrier_part(parts) {
        Some(i) => {
            let p = parts[i].as_object_mut().unwrap();
            let md = p.entry("metadata").or_insert(json!({}));
            if !md.is_object() {
                *md = json!({});
            }
            md.as_object_mut().unwrap().insert(carry::KEY.into(), carrier);
        }
        None => info[carry::KEY] = carrier,
    }
}

/// The carrier of a 1.x message, wherever it sits.
pub fn find_carrier(m: &MsgSrc) -> Option<&Value> {
    m.parts.iter().find_map(|p| p.value.get("metadata").and_then(|md| md.get(carry::KEY))).or_else(|| m.info.value.get(carry::KEY))
}

pub fn take_carrier(m: &MsgSrc) -> Option<Vec<Event>> {
    let c = find_carrier(m)?;
    let h = s(c, "h")?;
    let parts: Vec<Value> = m.parts.iter().map(|p| p.value.clone()).collect();
    if carrier_hash(&m.info.value, &parts) != h {
        return None;
    }
    super::carried_events(c)
}
