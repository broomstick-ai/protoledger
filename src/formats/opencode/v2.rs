//! opencode 2.x messages (`packages/schema/src/session-message.ts`) → neutral events.

use super::{Container, Item, MapCtx, get_ms, js_num, s, stamp, ts};
use crate::carry;
use crate::model::{Event, Kind};
use serde_json::{Value, json};

/// The session info's neutral facts: where it ran, with what, and its title.
///
/// `agent`, `model` and `location` on a 2.x session are the *current* selection
/// (the projector rewrites them on every switch, and each switch message records
/// the `previous` value), so the opening snapshot takes them from the first
/// switch's `previous` when the session switched, and from the session only when
/// it never did.
pub fn session_events<'a>(info: &Value, messages: impl Iterator<Item = &'a Value>) -> Vec<Event> {
    let created = get_ms(info, &["time", "created"]);
    let mut set = Event::new("", Kind::Settings).ts(ts(created));
    let (mut first_agent, mut first_model, mut first_dir) = (None, None, None);
    for m in messages {
        match s(m, "type") {
            Some("agent-switched") if first_agent.is_none() => first_agent = Some(m.get("previous").and_then(Value::as_str)),
            Some("model-switched") if first_model.is_none() => first_model = Some(m.get("previous")),
            Some("location-switched") if first_dir.is_none() => {
                first_dir = Some(m.get("previous").and_then(|p| p.get("location")).and_then(|l| s(l, "directory")))
            }
            _ => {}
        }
    }
    let dir = first_dir.unwrap_or_else(|| info.get("location").and_then(|l| s(l, "directory")));
    if let Some(d) = dir.filter(|d| !is_unknown_dir(d)) {
        set = set.with("cwd", d);
    }
    if let Some(a) = first_agent.unwrap_or(s(info, "agent")) {
        set = set.with("agent", a);
    }
    if let Some(m) = first_model.unwrap_or(info.get("model")) {
        set = model_fields(set, m);
    }
    // The session's opening settings, even when it states none (the session
    // record rides on it).
    let mut out = vec![set];
    if let Some(t) = s(info, "title").filter(|t| !super::is_default_title(t)) {
        out.push(Event::new("", Kind::Title).ts(ts(created)).with("text", t));
    }
    // Session totals: a cumulative usage (the importer places it last).
    if let Some(u) = totals(info) {
        out.push(u);
    }
    out
}

/// `/` is what a session with no known directory is written with (opencode
/// requires an absolute `location.directory`, and its import endpoint replaces it
/// with the importing directory anyway): not a fact about where it ran.
pub fn is_unknown_dir(d: &str) -> bool {
    d == "/"
}

/// The session's running totals (`cost`, `tokens`) as a cumulative usage, when
/// there are any. Zero fields are left out.
fn totals(info: &Value) -> Option<Event> {
    let tok = info.get("tokens")?;
    // Neutral output includes reasoning (SPEC §3.1.1); opencode's excludes it.
    let output = js_num(tok.get("output").and_then(Value::as_f64).unwrap_or(0.0) + tok.get("reasoning").and_then(Value::as_f64).unwrap_or(0.0));
    let fields = [
        ("input", tok.get("input")),
        ("output", Some(&output)),
        ("reasoning", tok.get("reasoning")),
        ("cache_read", tok.get("cache").and_then(|c| c.get("read"))),
        ("cache_write", tok.get("cache").and_then(|c| c.get("write"))),
        ("cost_usd", info.get("cost")),
    ];
    let nonzero: Vec<(&str, Value)> =
        fields.into_iter().filter_map(|(k, v)| v.filter(|v| v.as_f64().is_some_and(|f| f != 0.0)).map(|v| (k, v.clone()))).collect();
    if nonzero.is_empty() {
        return None;
    }
    let mut e = Event::new("", Kind::Usage).ts(ts(get_ms(info, &["time", "updated"])));
    if let Some(m) = info.get("model").and_then(|m| s(m, "id")) {
        e = e.with("model", m);
    }
    for (k, v) in nonzero {
        e = e.with(k, v);
    }
    Some(e.with("cumulative", true))
}

fn model_fields(mut e: Event, m: &Value) -> Event {
    if let Some(id) = s(m, "id") {
        e = e.with("model", id);
    }
    // `unknown` is the placeholder an export writes when no provider was recorded.
    if let Some(p) = s(m, "providerID").filter(|p| *p != "unknown") {
        e = e.with("provider", p);
    }
    if let Some(v) = s(m, "variant")
        && v != "default"
    {
        e = e.with("effort", v);
    }
    e
}

/// Provider proof on reasoning (`state`, the provider-scoped metadata the AI
/// layer replays): Anthropic `signature` / `redactedData`
/// (`packages/ai/src/protocols/anthropic-messages.ts`), OpenAI
/// `reasoningEncryptedContent`, Gemini `thoughtSignature`. The key names the
/// vendor, not the provider that relayed it.
pub fn sealed_of(state: Option<&Value>, _provider: &str) -> Option<Value> {
    let st = state?;
    for (key, kind, by_kind) in [
        ("signature", "signature", "anthropic"),
        ("thoughtSignature", "signature", "google"),
        ("redactedData", "encrypted", "anthropic"),
        ("reasoningEncryptedContent", "encrypted", "openai"),
    ] {
        if let Some(data) = s(st, key) {
            // SPEC §3.1.5 names the seal's vendor by its kind: the state key is the
            // protocol that issued it, whichever provider relayed it.
            return Some(json!({"vendor": by_kind, "kind": kind, "data": data}));
        }
    }
    None
}

/// The reverse of [`sealed_of`]: the `state` that carries a neutral seal. An
/// encrypted seal is OpenAI's `reasoningEncryptedContent` on summary reasoning
/// (`summary`), and Anthropic's `redactedData` (redacted thinking) otherwise; a
/// Google signature is a `thoughtSignature`.
pub fn state_of(sealed: &Value, summary: bool) -> Option<Value> {
    let data = s(sealed, "data")?;
    match s(sealed, "kind") {
        Some("signature") if s(sealed, "vendor") == Some("google") => Some(json!({"thoughtSignature": data})),
        Some("signature") => Some(json!({"signature": data})),
        Some("encrypted") if summary => Some(json!({"reasoningEncryptedContent": data})),
        Some("encrypted") => Some(json!({"redactedData": data})),
        _ => None,
    }
}

/// Tool output content (`Tool.Content[]`) → a neutral `output`.
pub fn tool_output(content: Option<&Value>) -> Value {
    let Some(items) = content.and_then(Value::as_array) else { return json!("") };
    if items.len() == 1 && s(&items[0], "type") == Some("text") {
        return items[0].get("text").cloned().unwrap_or(json!(""));
    }
    let parts: Vec<Value> = items
        .iter()
        .map(|c| match s(c, "type") {
            Some("text") => json!({"type": "text", "text": c.get("text").cloned().unwrap_or(json!(""))}),
            Some("file") => {
                let mut m = json!({"type": "media", "media_type": c.get("mime").cloned().unwrap_or(Value::Null)});
                let uri = s(c, "uri").unwrap_or("");
                if let Some((_, data)) = uri.split_once(";base64,").filter(|_| uri.starts_with("data:")) {
                    m["data"] = json!(data);
                } else {
                    m["url"] = json!(uri);
                }
                if let Some(n) = c.get("name") {
                    m["name"] = n.clone();
                }
                m
            }
            _ => c.clone(),
        })
        .collect();
    Value::Array(parts)
}

fn error_event(err: &Value, ts_: Option<String>) -> Event {
    let mut e = Event::new("", Kind::Error).ts(ts_);
    if let Some(t) = s(err, "type") {
        e = e.with("kind", t);
    }
    if let Some(m) = s(err, "message") {
        e = e.with("message", m);
    }
    if let Some(st) = err.get("status") {
        e = e.with("status", st.clone());
    }
    e
}

/// Neutral events for one V2 message, stamped with ids and natives.
pub fn message_events(item: &Item, ctx: &mut MapCtx, container: Container, version: Option<&str>) -> Vec<Event> {
    let m = &item.value;
    let id = item.id.as_str();
    let t = ts(get_ms(m, &["time", "created"]));
    let ty = s(m, "type").unwrap_or("");
    // Turns end at idle markers; where a store has none, a user prompt after a
    // finished assistant step starts the next turn.
    if ty == "user" && ctx.last_finish.as_deref().is_some_and(|f| f != "tool-calls") {
        ctx.turn_open = false;
    }
    if ty == "assistant" {
        ctx.last_finish = s(m, "finish").map(str::to_string);
    } else if ty == "user" {
        ctx.last_finish = None;
    }
    if !ctx.turn_open && ty != "idle" {
        ctx.turn = Some(id.to_string());
        ctx.turn_open = true;
    }
    let turn = ctx.turn.clone();
    let mut out: Vec<Event> = Vec::new();
    match ty {
        "user" => {
            let text = s(m, "text").unwrap_or("");
            let files = m.get("files").and_then(Value::as_array).cloned().unwrap_or_default();
            for sk in m.get("skills").and_then(Value::as_array).into_iter().flatten() {
                if let Some(txt) = s(sk, "text") {
                    out.push(
                        Event::new("", Kind::Text)
                            .ts(t.clone())
                            .with("role", "user")
                            .with("text", txt)
                            .with("skill", sk.get("name").cloned().unwrap_or(Value::Null)),
                    );
                }
            }
            if let Some(p) = super::peer_unwrap(text) {
                out.push(p.ts(t.clone()));
            } else if !text.is_empty() || (files.is_empty() && out.is_empty()) {
                out.push(Event::new("", Kind::Text).ts(t.clone()).with("role", "user").with("text", text));
            }
            for f in files {
                let mut e = Event::new("", Kind::Media)
                    .ts(t.clone())
                    .with("role", "user")
                    .with("media_type", f.get("mime").cloned().unwrap_or(Value::Null))
                    .with("data", f.get("data").cloned().unwrap_or(Value::Null));
                if let Some(n) = f.get("name") {
                    e = e.with("name", n.clone());
                }
                if let Some(u) = f.get("source").filter(|x| s(x, "type") == Some("uri")).and_then(|x| x.get("uri")) {
                    e = e.with("url", u.clone());
                }
                out.push(e);
            }
        }
        "synthetic" if super::peer_unwrap(s(m, "text").unwrap_or("")).is_some() => {
            out.push(super::peer_unwrap(s(m, "text").unwrap_or("")).unwrap().ts(t.clone()));
        }
        "synthetic" => {
            let text = s(m, "text").unwrap_or("");
            // A peer message written by an earlier export (`description` names the peer).
            let peer = s(m, "description").and_then(|d| d.strip_prefix("peer message (")).and_then(|r| r.split_once(") "));
            match peer.filter(|(dir, _)| *dir == "in" || *dir == "out") {
                Some((dir, rest)) => {
                    let (who, network) = match rest.strip_suffix(']').and_then(|r| r.rsplit_once(" [")) {
                        Some((w, n)) => (w, Some(n)),
                        None => (rest, None),
                    };
                    let mut e = Event::new("", Kind::PeerMessage).ts(t.clone()).with("direction", dir).with("peer", who);
                    if let Some(n) = network {
                        e = e.with("network", n);
                    }
                    out.push(e.with("text", text));
                }
                None => out.push(Event::new("", Kind::Text).ts(t.clone()).with("role", "user").with("text", text).with("synthetic", true)),
            }
        }
        "system" => {
            out.push(Event::new("", Kind::Text).ts(t.clone()).with("role", "system").with("text", s(m, "text").unwrap_or("")));
        }
        "skill" => {
            out.push(
                Event::new("", Kind::Text)
                    .ts(t.clone())
                    .with("role", "user")
                    .with("text", s(m, "text").unwrap_or(""))
                    .with("skill", m.get("name").cloned().unwrap_or(Value::Null)),
            );
        }
        "shell" => {
            let call = s(m, "shellID").unwrap_or(id).to_string();
            out.push(
                Event::new("", Kind::ToolCall)
                    .ts(t.clone())
                    .with("call_id", call.as_str())
                    .with("name", "shell")
                    .with("input", json!({"command": m.get("command").cloned().unwrap_or(json!(""))}))
                    .with("tool_type", "shell")
                    .with("user", true),
            );
            if s(m, "status") != Some("running") {
                let output = m.get("output").and_then(|o| o.get("output")).cloned().unwrap_or(json!(""));
                let exit = m.get("exit").and_then(Value::as_i64);
                out.push(
                    Event::new("", Kind::ToolResult)
                        .ts(ts(get_ms(m, &["time", "completed"])).or(t.clone()))
                        .with("call_id", call.as_str())
                        .with("output", output)
                        .with("is_error", if exit.is_some_and(|x| x != 0) { json!(true) } else { Value::Null })
                        .with("status", m.get("status").cloned().unwrap_or(Value::Null)),
                );
            }
        }
        "agent-switched" => {
            let a = s(m, "agent").unwrap_or("");
            ctx.agent = Some(a.to_string());
            out.push(Event::new("", Kind::Settings).ts(t.clone()).with("agent", a));
        }
        "model-switched" => {
            let e = Event::new("", Kind::Settings).ts(t.clone());
            out.push(model_fields(e, m.get("model").unwrap_or(&Value::Null)));
        }
        "location-switched" => {
            let d = m.get("location").and_then(|l| s(l, "directory")).unwrap_or("");
            out.push(Event::new("", Kind::Settings).ts(t.clone()).with("cwd", d));
        }
        "assistant" => out.extend(assistant_events(m)),
        "compaction" => {
            let status = s(m, "status").unwrap_or("");
            let trigger = s(m, "reason").unwrap_or("auto");
            if status == "completed" {
                out.push(
                    Event::new("", Kind::Compaction)
                        .ts(t.clone())
                        .with("phase", "summary")
                        .with("trigger", trigger)
                        .with("summary", m.get("summary").cloned().unwrap_or(json!(""))),
                );
            } else {
                out.push(Event::new("", Kind::Compaction).ts(t.clone()).with("phase", "boundary").with("trigger", trigger));
                if let Some(err) = m.get("error") {
                    out.push(error_event(err, t.clone()));
                }
            }
        }
        "idle" => {
            let outcome = match s(m, "outcome") {
                Some("succeeded") => "ok",
                Some("failed") => "error",
                Some("interrupted") => "interrupted",
                _ => "ok",
            };
            let mut e = Event::new("", Kind::Turn).ts(t.clone()).with("phase", "end").with("outcome", outcome);
            if let Some(msg) = m.get("error").and_then(|e| s(e, "message")) {
                e = e.with("reason", msg);
            }
            out.push(e);
            ctx.turn_open = false;
        }
        _ => out.push(Event::new("", Kind::Native).ts(t.clone())),
    }
    if out.is_empty() {
        out.push(Event::new("", Kind::Native).ts(t.clone()));
    }
    // A group re-assembles a message of several events (SPEC §2.2): an assistant
    // step, or a prompt with attachments.
    let grouped = ty == "assistant" || out.len() > 1;
    for e in &mut out {
        if grouped {
            e.group = Some(id.to_string());
        }
    }
    let mut out = stamp(out, item, container, version);
    for e in &mut out {
        e.turn = turn.clone();
    }
    out
}

/// The error message an export writes for a tool call the ledger has no result
/// for (SPEC: a call without a result). Read back as a call without a result.
pub const UNSETTLED_MESSAGE: &str = "No result was recorded for this tool call";

/// A neutral `usage` from a 2.x `tokens` record (SPEC §3.1.1). opencode stores
/// `output` without reasoning (`visibleOutputTokens`, `packages/core/src/session/usage.ts`)
/// and `input` without cache reads and writes, so neutral `output` is
/// `output + reasoning`.
pub fn usage_event(tok: &Value, cost: Option<&Value>, ts_: Option<String>) -> Event {
    let n = |p: &[&str]| crate::util::get(tok, p).and_then(Value::as_f64);
    let mut u = Event::new("", Kind::Usage).ts(ts_);
    let reasoning = n(&["reasoning"]).unwrap_or(0.0);
    if let Some(i) = n(&["input"]) {
        u = u.with("input", js_num(i));
    }
    if let Some(o) = n(&["output"]) {
        u = u.with("output", js_num(o + reasoning));
    }
    for (k, p) in [("reasoning", &["reasoning"][..]), ("cache_read", &["cache", "read"]), ("cache_write", &["cache", "write"])] {
        if let Some(v) = n(p) {
            u = u.with(k, js_num(v));
        }
    }
    if let Some(c) = cost.filter(|c| !c.is_null()) {
        u = u.with("cost_usd", c.clone());
    }
    u
}

/// A tool's `state.input` → neutral `input` / `input_text`. A lone `input` key is
/// how a tool whose input is not an object is written (opencode requires an
/// object): a string is a free-text (`custom`) tool's input, anything else the
/// input itself. No opencode tool takes a single parameter named `input`.
pub fn tool_input(st: &Value) -> (Option<Value>, Option<String>) {
    let input = match st.get("input") {
        Some(Value::String(raw)) => serde_json::from_str(raw).unwrap_or(json!({})),
        Some(v) => v.clone(),
        None => json!({}),
    };
    if let Some(o) = input.as_object().filter(|o| o.len() == 1) {
        match o.get("input") {
            Some(Value::String(t)) => return (None, Some(t.clone())),
            Some(v) => return (Some(v.clone()), None),
            None => {}
        }
    }
    (Some(input), None)
}

/// Is this reasoning item OpenAI Responses reasoning? Its `text` is then a
/// reasoning *summary* (`packages/ai/src/protocols/open-responses.ts` streams
/// one item per summary part, with `{itemId, reasoningEncryptedContent}` state).
fn is_summary_reasoning(state: Option<&Value>) -> bool {
    state.and_then(Value::as_object).is_some_and(|o| o.contains_key("reasoningEncryptedContent") || o.contains_key("itemId"))
}

/// An assistant step's events, in the order they happened: content items in
/// order (a text dated by the end of the reasoning before it), each tool result
/// after its call (past parallel calls that started before it came back), the
/// usage when the response finished streaming, then snapshot file changes and
/// the step's error.
fn assistant_events(m: &Value) -> Vec<Event> {
    let created = get_ms(m, &["time", "created"]);
    let t = ts(created);
    let vendor = m.get("model").and_then(|mo| s(mo, "providerID")).unwrap_or("unknown");
    // (event, when) for content; (event, earliest content index it may follow, when) for the rest.
    let mut content: Vec<(Event, Option<i64>)> = Vec::new();
    let mut floats: Vec<(Event, usize, Option<i64>)> = Vec::new();
    // When the previous item ended, for dating a text item (which has no time of its own).
    let mut prev_end = created;
    // The OpenAI reasoning item being continued (consecutive summary parts of one item).
    let mut open_item: Option<(usize, String)> = None;
    for c in m.get("content").and_then(Value::as_array).into_iter().flatten() {
        let ty = s(c, "type");
        if ty != Some("reasoning") {
            open_item = None;
        }
        match ty {
            Some("text") => {
                let e = Event::new("", Kind::Text).ts(ts(prev_end)).with("role", "assistant").with("text", c.get("text").cloned().unwrap_or(json!("")));
                content.push((e, prev_end));
            }
            Some("reasoning") => {
                let rc = get_ms(c, &["time", "created"]).or(prev_end);
                prev_end = get_ms(c, &["time", "completed"]).or(rc);
                let text = c.get("text").and_then(Value::as_str).unwrap_or("").to_string();
                let state = c.get("state");
                if is_summary_reasoning(state) {
                    let item = state.and_then(|st| s(st, "itemId")).map(str::to_string);
                    if let (Some((i, open)), Some(id)) = (&open_item, &item)
                        && open == id
                    {
                        let (e, _) = &mut content[*i];
                        if !text.is_empty()
                            && let Some(Value::Array(a)) = e.body.get_mut("summary")
                        {
                            a.push(json!(text));
                        }
                        if e.body.get("sealed").is_none()
                            && let Some(sl) = sealed_of(state, vendor)
                        {
                            e.body.insert("sealed".into(), sl);
                        }
                        continue;
                    }
                    let summary: Vec<Value> = if text.is_empty() { vec![] } else { vec![json!(text)] };
                    let mut e = Event::new("", Kind::Reasoning).ts(ts(rc)).with("summary", Value::Array(summary));
                    if let Some(sl) = sealed_of(state, vendor) {
                        e = e.with("sealed", sl);
                    }
                    open_item = item.map(|id| (content.len(), id));
                    content.push((e, rc));
                } else {
                    let mut e = Event::new("", Kind::Reasoning).ts(ts(rc)).with("text", text);
                    if let Some(sl) = sealed_of(state, vendor) {
                        e = e.with("sealed", sl);
                    }
                    content.push((e, rc));
                }
            }
            Some("tool") => {
                let call = s(c, "id").unwrap_or("").to_string();
                let name = s(c, "name").unwrap_or("").to_string();
                let st = c.get("state").cloned().unwrap_or(Value::Null);
                let status = s(&st, "status").unwrap_or("");
                let (input, input_text) = tool_input(&st);
                let cms = get_ms(c, &["time", "created"]).or(prev_end);
                // A text after a tool is dated by the step, as before (only reasoning says when it ended).
                prev_end = created;
                let mut e = Event::new("", Kind::ToolCall).ts(ts(cms)).with("call_id", call.as_str()).with("name", name.as_str());
                match &input_text {
                    Some(text) => e = e.with("input_text", text.as_str()).with("tool_type", "custom"),
                    None => e = e.with("input", input.clone().unwrap_or(json!({}))).with("tool_type", "function"),
                }
                let call_at = content.len();
                content.push((e, cms));
                let child = st.get("metadata").and_then(|md| s(md, "sessionID")).map(str::to_string);
                // The subagent tool (and any tool that records a child session) spawned a child.
                if let Some(child) = child {
                    let inp = input.clone().unwrap_or(Value::Null);
                    content.push((
                        Event::new("", Kind::Subagent)
                            .ts(ts(cms))
                            .with("phase", "start")
                            .with("agent", child.as_str())
                            .with("call_id", call.as_str())
                            // 2.x `subagent` takes `agent`; the 1.x `task` tool took `subagent_type`.
                            .with("agent_type", inp.get("agent").or_else(|| inp.get("subagent_type")).cloned().unwrap_or(Value::Null))
                            .with("description", inp.get("description").cloned().unwrap_or(Value::Null)),
                        cms,
                    ));
                }
                let unsettled = status == "error"
                    && st.get("content").is_none()
                    && st.get("error").and_then(|e| s(e, "type")) == Some("tool.interrupted")
                    && st.get("error").and_then(|e| s(e, "message")) == Some(UNSETTLED_MESSAGE);
                if (status == "completed" || status == "error") && !unsettled {
                    let rms = get_ms(c, &["time", "completed"]).or(created);
                    let mut r = Event::new("", Kind::ToolResult).ts(ts(rms)).with("call_id", call.as_str());
                    if status == "error" {
                        let msg = st.get("error").and_then(|e| s(e, "message")).unwrap_or("").to_string();
                        let out_v = if st.get("content").is_some() { tool_output(st.get("content")) } else { json!(msg) };
                        // The error's kind and message, unless they only restate a failed
                        // tool's output (`tool.execution` is opencode's kind for any failure).
                        let restated = |k: &str| k == "tool.execution" && json!(msg) == out_v;
                        if let Some(k) = st.get("error").and_then(|e| s(e, "type")).filter(|k| !restated(k)) {
                            r = r.with("error", json!({"kind": k, "message": msg}));
                        }
                        r = r.with("output", out_v).with("is_error", true);
                    } else {
                        // Some 2.0 betas kept a tool's output in `structured` with empty `content`.
                        let empty = st.get("content").and_then(Value::as_array).is_none_or(|a| a.is_empty());
                        let out_v = match st.get("structured").and_then(|x| x.get("content")).filter(|_| empty) {
                            Some(Value::String(t)) => json!(t),
                            _ => tool_output(st.get("content")),
                        };
                        r = r.with("output", out_v);
                    }
                    floats.push((r, call_at, rms));
                }
            }
            _ => {}
        }
    }
    let end = content.len();
    let done_ms = get_ms(m, &["time", "completed"]).or(created);
    if let Some(files) = m.get("snapshot").and_then(|sn| sn.get("files")).and_then(Value::as_array) {
        let snap = m.get("snapshot").and_then(|sn| s(sn, "end")).map(str::to_string);
        for f in files {
            let mut e = Event::new("", Kind::FileChange).ts(t.clone()).with("path", f.clone()).with("op", "edit");
            if let Some(sn) = &snap {
                e = e.with("snapshot", sn.as_str());
            }
            // The snapshot is taken when the step ends.
            floats.push((e, end, done_ms));
        }
    }
    if let Some(tok) = m.get("tokens") {
        // When the response finished streaming (`time.streamed`): after the
        // response's own events and before the tool results that came back
        // later (SPEC §3.1.1). Without it, when the step completed.
        let ums = get_ms(m, &["time", "streamed"]).or(done_ms);
        let mut u = usage_event(tok, m.get("cost"), ts(ums));
        if let Some(id) = m.get("model").and_then(|mo| s(mo, "id")) {
            u = u.with("model", id);
        }
        floats.push((u, end, ums));
    }
    if let Some(err) = m.get("error") {
        floats.push((error_event(err, ts(done_ms)), end, done_ms));
    }
    place(content, floats)
}

#[cfg(test)]
pub fn assistant_events_for_test(m: &Value) -> Vec<Event> {
    assistant_events(m)
}

/// Merge `floats` into `content`: each goes after content index `min` and after
/// every later content item that did not happen after it; floats that land in
/// one place keep time order (and, at equal times, their own order).
fn place(content: Vec<(Event, Option<i64>)>, floats: Vec<(Event, usize, Option<i64>)>) -> Vec<Event> {
    // A result moves past later tool calls that started before it came back
    // (parallel calls), never past text or reasoning.
    let passable: Vec<bool> = content.iter().map(|(e, _)| matches!(e.kind, Kind::ToolCall | Kind::Subagent)).collect();
    // Content times as a running maximum, so an undated item never moves anything.
    let mut when: Vec<i64> = Vec::with_capacity(content.len());
    let mut run = i64::MIN;
    for (_, ms) in &content {
        run = run.max(ms.unwrap_or(run));
        when.push(run);
    }
    let mut slotted: Vec<(usize, i64, usize, Event)> = floats
        .into_iter()
        .enumerate()
        .map(|(n, (e, min, ms))| {
            let mut slot = (min + 1).min(content.len());
            let at = ms.unwrap_or(if slot > 0 { when[slot - 1] } else { i64::MIN });
            while slot < content.len() && passable[slot] && when[slot] <= at {
                slot += 1;
            }
            (slot, at, n, e)
        })
        .collect();
    slotted.sort_by_key(|(slot, at, n, _)| (*slot, *at, *n));
    let mut out = Vec::new();
    let mut fl = slotted.into_iter().peekable();
    for (i, (e, _)) in content.into_iter().enumerate() {
        while fl.peek().is_some_and(|f| f.0 <= i) {
            out.push(fl.next().unwrap().3);
        }
        out.push(e);
    }
    out.extend(fl.map(|f| f.3));
    if out.is_empty() {
        out.push(Event::new("", Kind::Native));
    }
    out
}

/// The carried events of a V2 message, if it still matches what was exported.
pub fn take_carrier(msg: &Value) -> Option<Vec<Event>> {
    let mut m = msg.clone();
    m.get("metadata")?.get(carry::KEY)?;
    carry::take(&mut m, &["metadata"])
}

/// Bring an assistant message's tools into the settled shape 2.x imports:
/// running or streaming tools become interrupted errors, and a completed tool
/// with no content (some 2.0 betas kept the output in `structured`) gets a text
/// item. Returns whether anything changed.
pub fn settle_tools(m: &mut Value) -> bool {
    let mut changed = false;
    let Some(content) = m.get_mut("content").and_then(Value::as_array_mut) else { return false };
    for c in content.iter_mut().filter(|c| s(c, "type") == Some("tool")) {
        let st = &mut c["state"];
        match s(st, "status") {
            Some("completed") => {
                if st.get("content").and_then(Value::as_array).is_none_or(|a| a.is_empty()) {
                    let text = st.get("structured").and_then(|x| s(x, "content")).unwrap_or("").to_string();
                    st["content"] = json!([{"type": "text", "text": text}]);
                    changed = true;
                }
            }
            Some("error") => {}
            _ => {
                let input = match st.get("input") {
                    Some(Value::Object(o)) => Value::Object(o.clone()),
                    Some(Value::String(t)) => serde_json::from_str(t).ok().filter(Value::is_object).unwrap_or(json!({})),
                    _ => json!({}),
                };
                *st = json!({"status": "error", "input": input, "error": {"type": "tool.interrupted", "message": "Tool execution was interrupted"}});
                changed = true;
            }
        }
    }
    changed
}

/// Do the session's totals equal the sum over its assistant (and compaction) messages?
pub fn totals_are_sum<'a>(info: &Value, messages: impl Iterator<Item = &'a Value>) -> bool {
    let mut sum = [0.0f64; 6];
    for m in messages {
        if !matches!(s(m, "type"), Some("assistant") | Some("compaction")) {
            continue;
        }
        let g = |p: &[&str]| crate::util::get(m, p).and_then(Value::as_f64).unwrap_or(0.0);
        for (i, p) in
            [&["tokens", "input"][..], &["tokens", "output"], &["tokens", "reasoning"], &["tokens", "cache", "read"], &["tokens", "cache", "write"], &["cost"]]
                .iter()
                .enumerate()
        {
            sum[i] += g(p);
        }
    }
    let g = |p: &[&str]| crate::util::get(info, p).and_then(Value::as_f64).unwrap_or(0.0);
    let have = [
        g(&["tokens", "input"]),
        g(&["tokens", "output"]),
        g(&["tokens", "reasoning"]),
        g(&["tokens", "cache", "read"]),
        g(&["tokens", "cache", "write"]),
        g(&["cost"]),
    ];
    sum.iter().zip(have.iter()).all(|(a, b)| (a - b).abs() < 1e-9)
}
