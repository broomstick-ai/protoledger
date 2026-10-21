//! `~/.cursor/projects/<slug>/agent-transcripts/` JSONL.

use super::stream::{files, raw_lines};
use super::{V_TRANSCRIPT, agent, native, output_text, own, push_record};
use crate::carry;
use crate::formats::atif::jsonfmt;
use crate::formats::gemini_cli::{apply_agent, attach_with_agent, carried_agent};
use crate::formats::{ExportOptions, Exported, ImportOptions, Imported, Losses, Output};
use crate::ledger::{Ledger, LedgerSet};
use crate::model::{Event, Kind, Obj, Parent, Relation};
use crate::util::get_str;
use anyhow::{Result, bail};
use serde_json::{Value, json};
use std::collections::HashMap;
use std::path::Path;

pub(super) fn import(input: &Path, opts: &ImportOptions) -> Result<Imported> {
    let mut set = LedgerSet::new();
    let mut losses = Losses::default();
    let multi = input.is_dir();
    let mut parents: Vec<(String, String)> = Vec::new();
    for (rel, text) in files(input, &[".jsonl"])? {
        if multi && super::detect_file(&input.join(&rel)).map(|d| d.variant) != Some(V_TRANSCRIPT) {
            continue;
        }
        let l = import_file(&rel, &text, opts, &mut losses);
        // `<parent>/subagents/<child>.jsonl`
        let segs: Vec<&str> = rel.split('/').collect();
        if segs.len() >= 3 && segs[segs.len() - 2] == "subagents" && l.agent().parent.is_none() {
            parents.push((l.id().to_string(), segs[segs.len() - 3].to_string()));
        }
        set.insert(l);
    }
    if set.is_empty() {
        bail!("{}: no Cursor agent transcripts found", input.display());
    }
    for (child, parent) in parents {
        if set.get(&parent).is_some() {
            set.ledgers.get_mut(&child).unwrap().agent_mut().parent = Some(Parent { agent: parent, event: None, relation: Relation::Subagent });
        }
    }
    Ok(Imported { set, losses: losses.sorted(), variant: V_TRANSCRIPT.into() })
}

/// `<timestamp>Tuesday, Sep 29, 2026, 10:22 AM (UTC+9)</timestamp>` → RFC 3339.
pub(super) fn parse_cursor_stamp(text: &str) -> Option<String> {
    let s = text.split("<timestamp>").nth(1)?.split("</timestamp>").next()?;
    let parts: Vec<&str> = s.split(", ").collect();
    // ["Tuesday", "Sep 29", "2026", "10:22 AM (UTC+9)"]
    if parts.len() < 4 {
        return None;
    }
    let (mon, day) = parts[1].split_once(' ')?;
    let month = ["Jan", "Feb", "Mar", "Apr", "May", "Jun", "Jul", "Aug", "Sep", "Oct", "Nov", "Dec"].iter().position(|m| mon.starts_with(m))? + 1;
    let year: i64 = parts[2].trim().parse().ok()?;
    let rest = parts[3];
    let (clock, tz) = rest.split_once(" (")?;
    let (hm, ampm) = clock.split_once(' ')?;
    let (h, m) = hm.split_once(':')?;
    let mut h: i64 = h.parse().ok()?;
    let m: i64 = m.parse().ok()?;
    if ampm == "PM" && h != 12 {
        h += 12;
    }
    if ampm == "AM" && h == 12 {
        h = 0;
    }
    let tz = tz.trim_end_matches(')').trim_start_matches("UTC");
    let (sign, off) = match tz.chars().next() {
        Some('+') => (1, &tz[1..]),
        Some('-') => (-1, &tz[1..]),
        _ => (1, "0"),
    };
    let (oh, om) = off.split_once(':').unwrap_or((off, "0"));
    let offset = sign * (oh.parse::<i64>().ok()? * 60 + om.parse::<i64>().unwrap_or(0));
    let local = format!("{year:04}-{month:02}-{:02}T{h:02}:{m:02}:00Z", day.parse::<u32>().ok()?);
    let ms = crate::util::parse_rfc3339_ms(&local)? - offset * 60_000;
    Some(crate::util::ms_to_rfc3339(ms))
}

fn import_file(rel: &str, text: &str, opts: &ImportOptions, _losses: &mut Losses) -> Ledger {
    let (lines, final_nl) = raw_lines(text);
    let stem = rel.rsplit('/').next().unwrap_or(rel).trim_end_matches(".jsonl");
    let mut extra = Obj::new();
    extra.insert("file".into(), json!(rel));
    if !final_nl {
        extra.insert("no_final_newline".into(), json!(true));
    }
    let mut l = Ledger::new(agent(stem, V_TRANSCRIPT, Some(stem.to_string()), extra));
    let mut turn: Option<String> = None;
    let mut ts: Option<String> = None;
    let mut pending: Vec<(String, String)> = Vec::new(); // (tool name, call id) without a native id
    for (off, line) in lines {
        let nid = format!("@{off}");
        let v: Option<Value> = serde_json::from_str(line).ok();
        let Some(v) = v else {
            push_record(&mut l, &nid, native(V_TRANSCRIPT, &nid).raw(line), vec![], None);
            continue;
        };
        if opts.restore_carried && v.get(carry::KEY).is_some() {
            if let Some(a) = carried_agent(&v) {
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
        let nid = get_str(&v, &["id"]).map(String::from).unwrap_or(nid);
        // The line is held as its own text (SPEC §4).
        let mut n = native(V_TRANSCRIPT, &nid);
        jsonfmt::hold(&mut n, Some(line), &v);
        let mut evs = Vec::new();
        let role = get_str(&v, &["role"]);
        let content: Vec<Value> = match v.get("message").and_then(|m| m.get("content")) {
            Some(Value::String(s)) => vec![json!({"type": "text", "text": s})],
            Some(Value::Array(a)) => a.clone(),
            _ => vec![],
        };
        let model = get_str(&v, &["message", "model"]).map(String::from);
        if role == Some("user") && content.iter().any(|p| p.get("type").and_then(Value::as_str) == Some("text")) {
            turn = Some(nid.clone());
            let all: String = content.iter().filter_map(|p| get_str(p, &["text"])).collect();
            if let Some(t) = parse_cursor_stamp(&all) {
                ts = Some(t);
            }
        }
        let mk = |k: Kind| Event::new("", k).ts(ts.clone()).turn(turn.clone());
        if let Some(m) = &model {
            evs.push(mk(Kind::Settings).with("model", m.as_str()));
        }
        for (k, p) in content.iter().enumerate() {
            let group = (role == Some("assistant")).then(|| nid.clone());
            match get_str(p, &["type"]).unwrap_or("") {
                "text" => {
                    let r = match role {
                        Some("assistant") => "assistant",
                        Some("system") => "system",
                        _ => "user",
                    };
                    evs.push(mk(Kind::Text).with("role", r).with("text", p.get("text").cloned().unwrap_or(json!(""))).group(group));
                }
                "thinking" => evs.push(mk(Kind::Reasoning).with("text", p.get("thinking").or(p.get("text")).cloned().unwrap_or(json!(""))).group(group)),
                "tool_use" => {
                    let name = get_str(p, &["name"]).unwrap_or("").to_string();
                    let id = match get_str(p, &["id"]) {
                        Some(id) => id.to_string(),
                        None => {
                            let id = format!("{nid}#{k}");
                            pending.push((name.clone(), id.clone()));
                            id
                        }
                    };
                    evs.push(
                        mk(Kind::ToolCall)
                            .with("call_id", id.as_str())
                            .with("name", name.as_str())
                            .with("input", p.get("input").cloned().unwrap_or(json!({})))
                            .group(group),
                    );
                }
                "tool_result" => {
                    let id = match get_str(p, &["tool_use_id"]) {
                        Some(id) => id.to_string(),
                        None if !pending.is_empty() => pending.remove(0).1,
                        None => format!("{nid}#{k}"),
                    };
                    let output = match p.get("content") {
                        Some(Value::Array(a)) => {
                            let t: Vec<&str> = a.iter().filter_map(|x| get_str(x, &["text"])).collect();
                            json!(t.join("\n"))
                        }
                        Some(Value::String(s)) => json!(s),
                        Some(o) => json!(o.to_string()),
                        None => json!(""),
                    };
                    let mut e = mk(Kind::ToolResult).with("call_id", id.as_str()).with("output", output);
                    if p.get("is_error").and_then(Value::as_bool) == Some(true) {
                        e = e.with("is_error", true);
                    }
                    evs.push(e);
                }
                "image" => {
                    let mut e = mk(Kind::Media).with("role", if role == Some("assistant") { "assistant" } else { "user" });
                    let src = p.get("source").cloned().unwrap_or(json!({}));
                    e = e.with("media_type", src.get("media_type").cloned().unwrap_or(json!("image/png")));
                    if let Some(d) = src.get("data") {
                        e = e.with("data", d.clone());
                    } else if let Some(u) = src.get("url") {
                        e = e.with("url", u.clone());
                    }
                    evs.push(e);
                }
                _ => {}
            }
        }
        match get_str(&v, &["type"]) {
            Some("turn_ended") => {
                let ok = get_str(&v, &["status"]).is_none_or(|s| s == "success");
                evs.push(mk(Kind::Turn).with("phase", "end").with("outcome", if ok { "ok" } else { "error" }));
            }
            Some("error") => {
                let msg = v.get("error").cloned().unwrap_or(json!(""));
                let msg = if msg.is_string() { msg } else { Value::String(msg.to_string()) };
                evs.push(mk(Kind::Error).with("kind", "error").with("message", msg));
            }
            _ => {}
        }
        push_record(&mut l, &nid, n, evs, ts.clone());
    }
    l
}

// ---------------------------------------------------------------------------
// export

pub(super) fn export(set: &LedgerSet, opts: &ExportOptions) -> Result<Exported> {
    let mut out = Output::default();
    let mut losses = Losses::default();
    let nested = set.iter().any(|l| l.agent().parent.as_ref().is_some_and(|p| set.get(&p.agent).is_some()));
    for l in set.iter() {
        let own_l = super::is_own_ledger(l, V_TRANSCRIPT);
        let extra = l.agent().source.as_ref().filter(|_| own_l).map(|s| s.extra.clone()).unwrap_or_default();
        let id = crate::ledger::file_safe(l.id());
        let path = match extra.get("file").and_then(Value::as_str) {
            Some(f) => f.to_string(),
            None => match l.agent().parent.as_ref().filter(|p| set.get(&p.agent).is_some()) {
                Some(p) => format!("{}/subagents/{id}.jsonl", crate::ledger::file_safe(&p.agent)),
                None if nested => format!("{id}/{id}.jsonl"),
                None => format!("{id}.jsonl"),
            },
        };
        let mut lines: Vec<String> = Vec::new();
        let mut foreign: Vec<&Event> = Vec::new();
        let mut flush = |foreign: &mut Vec<&Event>, lines: &mut Vec<String>| {
            if foreign.is_empty() {
                return;
            }
            let head = if lines.is_empty() && !own_l { Some(l.agent()) } else { None };
            for v in render(foreign, opts, head, &mut losses) {
                lines.push(serde_json::to_string(&v).unwrap());
            }
            foreign.clear();
        };
        for e in &l.events {
            match own(e, V_TRANSCRIPT).filter(|_| own_l) {
                Some(n) => {
                    flush(&mut foreign, &mut lines);
                    if n.part == 0 {
                        match (crate::util::current_raw(n), n.record_text()) {
                            (Some(r), _) => lines.push(r.to_string()),
                            (None, Some(r)) => lines.push(r.to_string()),
                            _ => {}
                        }
                    }
                }
                None => foreign.push(e),
            }
        }
        flush(&mut foreign, &mut lines);
        let mut s = lines.join("\n");
        if extra.get("no_final_newline").is_none() && !lines.is_empty() {
            s.push('\n');
        }
        out.put(path, s);
    }
    Ok(Exported { output: out, losses: losses.sorted(), variant: V_TRANSCRIPT.into() })
}

fn render(events: &[&Event], opts: &ExportOptions, head: Option<&crate::model::Agent>, losses: &mut Losses) -> Vec<Value> {
    // (line, events it shows)
    let mut out: Vec<(Value, Vec<Event>)> = Vec::new();
    let mut orphan: Vec<Event> = Vec::new();
    let mut open_assistant: Option<Option<String>> = None; // group of the assistant line being built
    let mut names: HashMap<String, String> = HashMap::new();
    for &e in events {
        let k = e.kind;
        let add_block = |role: &str, block: Value, merge: bool, out: &mut Vec<(Value, Vec<Event>)>, orphan: &mut Vec<Event>| {
            let mut evs = std::mem::take(orphan);
            evs.push(e.clone());
            if merge
                && let Some((line, have)) = out.last_mut()
                && line["role"] == role
            {
                line["message"]["content"].as_array_mut().unwrap().push(block);
                have.extend(evs);
                return;
            }
            out.push((json!({"role": role, "message": {"content": [block]}}), evs));
        };
        match k {
            Kind::Text | Kind::Reasoning | Kind::ToolCall => {
                let role = if k == Kind::Text { e.str("role").unwrap_or("user") } else { "assistant" };
                if !matches!(role, "user" | "assistant") {
                    losses.add("system-text-dropped", Some(k), "a transcript holds only user and assistant lines");
                    orphan.push(e.clone());
                    continue;
                }
                let block = match k {
                    Kind::Text => json!({"type": "text", "text": e.str("text").unwrap_or("")}),
                    Kind::Reasoning => {
                        if e.body.get("sealed").is_some() {
                            losses.add("reasoning-sealed-dropped", Some(k), "thinking blocks carry text only");
                        }
                        json!({"type": "thinking", "thinking": e.str("text").unwrap_or("")})
                    }
                    _ => {
                        let id = e.str("call_id").unwrap_or("").to_string();
                        names.insert(id.clone(), e.str("name").unwrap_or("").to_string());
                        json!({"type": "tool_use", "id": id, "name": e.str("name").unwrap_or(""), "input": crate::formats::pi_session::tool_args_object(e, losses)})
                    }
                };
                let merge = role == "assistant" && open_assistant.as_ref().is_some_and(|g| g.is_none() || *g == e.group);
                add_block(role, block, merge, &mut out, &mut orphan);
                open_assistant = (role == "assistant").then(|| e.group.clone());
            }
            Kind::ToolResult => {
                let (text, media) = output_text(e.body.get("output"));
                if media {
                    losses.add("tool-result-media-dropped", Some(k), "media in tool output is dropped");
                }
                let mut b = json!({"type": "tool_result", "tool_use_id": e.str("call_id").unwrap_or(""), "content": [{"type": "text", "text": text}]});
                if e.body.get("is_error").and_then(Value::as_bool) == Some(true) {
                    b["is_error"] = json!(true);
                }
                let merge = matches!(out.last(), Some((l, _)) if l["role"] == "user" && l["message"]["content"][0]["type"] == "tool_result");
                add_block("user", b, merge, &mut out, &mut orphan);
                open_assistant = None;
            }
            Kind::Media => {
                let role = if e.str("role") == Some("assistant") { "assistant" } else { "user" };
                let src = match (e.str("data"), e.str("url")) {
                    (Some(d), _) => json!({"type": "base64", "media_type": e.str("media_type").unwrap_or("image/png"), "data": d}),
                    (None, Some(u)) => json!({"type": "url", "url": u}),
                    _ => {
                        losses.add("media-dropped", Some(k), "media given only by path");
                        orphan.push(e.clone());
                        continue;
                    }
                };
                add_block(role, json!({"type": "image", "source": src}), true, &mut out, &mut orphan);
            }
            Kind::Turn if e.str("phase") == Some("end") => {
                let mut evs = std::mem::take(&mut orphan);
                evs.push(e.clone());
                let ok = e.str("outcome").is_none_or(|o| o == "ok");
                out.push((json!({"type": "turn_ended", "status": if ok { "success" } else { "error" }}), evs));
                open_assistant = None;
            }
            Kind::Error => {
                let mut evs = std::mem::take(&mut orphan);
                evs.push(e.clone());
                out.push((json!({"type": "error", "error": e.str("message").unwrap_or("")}), evs));
                open_assistant = None;
            }
            Kind::Compaction if e.str("summary").is_some() => {
                losses.add("compaction-as-user-text", Some(k), "a compaction summary becomes user text");
                add_block("user", json!({"type": "text", "text": e.str("summary").unwrap_or("")}), false, &mut out, &mut orphan);
                open_assistant = None;
            }
            other => {
                let code = match other {
                    Kind::Turn => "turn-dropped".to_string(),
                    _ => format!("{}-dropped", other.as_str().replace('_', "-")),
                };
                losses.add(&code, Some(other), "a transcript has no line for it");
                orphan.push(e.clone());
            }
        }
    }
    if !orphan.is_empty() {
        match out.last_mut() {
            Some((_, evs)) => evs.append(&mut orphan),
            None => out.push((json!({"type": "turn_ended", "status": "success"}), std::mem::take(&mut orphan))),
        }
    }
    out.into_iter()
        .enumerate()
        .map(|(i, (mut v, evs))| {
            match head.filter(|_| opts.carry && i == 0) {
                Some(a) => attach_with_agent(&mut v, &evs, a),
                None if opts.carry => carry::attach(&mut v, &[], &evs),
                None => {}
            }
            v
        })
        .collect()
}

#[cfg(test)]
mod tests {
    #[test]
    fn cursor_stamp() {
        let t = "<timestamp>Tuesday, Sep 29, 2026, 10:22 AM (UTC+9)</timestamp>\n<user_query>x</user_query>";
        assert_eq!(super::parse_cursor_stamp(t).unwrap(), "2026-09-29T01:22:00.000Z");
        let t = "<timestamp>Tuesday, Jun 16, 2026, 8:41 PM (UTC-4)</timestamp>";
        assert_eq!(super::parse_cursor_stamp(t).unwrap(), "2026-06-17T00:41:00.000Z");
    }
}
