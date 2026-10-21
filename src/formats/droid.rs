//! Factory Droid sessions: `~/.factory/sessions/<cwd-slug>/<uuid>.jsonl`
//! plus a pretty-printed `<uuid>.settings.json` sidecar (and droid's own
//! `.settings.json.bak` copy of the previous settings). See
//! `docs/formats/droid.md`.

use crate::formats::openclaw::jsonl::{self, Line, Taken};
use crate::formats::{Detection, ExportOptions, Exported, Format, ImportOptions, Imported, Losses, Output, Variant};
use crate::ledger::{Ledger, LedgerSet};
use crate::model::{Agent, Audience, Event, Kind, Link, Native, Obj, Parent, Relation, Source};
use crate::util;
use anyhow::{Context, Result, bail};
use serde_json::{Map, Value, json};
use std::collections::{HashMap, HashSet};
use std::path::{Path, PathBuf};

const FORMAT: &str = "droid";
const SETTINGS: &str = ".settings.json";
const SETTINGS_BAK: &str = ".settings.json.bak";

pub struct Droid;

static VARIANTS: &[Variant] = &[
    Variant {
        name: "session-v2-linear",
        versions: "droid <= ~0.13x (Jan-May 2026), session_start.version 2",
        import: true,
        export: false,
        notes: "messages without parentId; sessionTitle on session_start",
    },
    Variant {
        name: "session-v2",
        versions: "droid ~0.14x-0.24x and Factory Desktop (Jun 2026 on), session_start.version 2",
        import: true,
        export: true,
        notes: "parentId tree, agent_turn_outcome, compaction_state, hook rows, forks",
    },
];

impl Format for Droid {
    fn name(&self) -> &'static str {
        FORMAT
    }
    fn describe(&self) -> &'static str {
        "Factory Droid sessions (~/.factory/sessions/<cwd>/<uuid>.jsonl + .settings.json)"
    }
    fn variants(&self) -> &'static [Variant] {
        VARIANTS
    }
    fn detect(&self, input: &Path) -> Option<Detection> {
        let files: Vec<PathBuf> =
            if input.is_file() { vec![input.to_path_buf()] } else { jsonl::walk(input, ".jsonl").into_iter().map(|(_, p)| p).take(4).collect() };
        for p in files {
            let text = std::fs::read_to_string(&p).ok()?;
            let first = jsonl::lines(&text).into_iter().find_map(|l| l.value)?;
            if str_at(&first, "type") == Some("session_start") && first.get("id").is_some() {
                return Some(Detection {
                    variant: variant_of(&text),
                    confidence: 90,
                    version: first.get("version").map(|v| format!("session-v{}", v.as_u64().unwrap_or(0))),
                });
            }
        }
        None
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
    import(input, opts)
}

fn str_at<'a>(v: &'a Value, k: &str) -> Option<&'a str> {
    v.get(k).and_then(Value::as_str)
}

/// Messages carry `parentId` from mid-2026 on; files without it are the
/// older linear shape.
fn variant_of(text: &str) -> &'static str {
    if text.contains("\"parentId\"") || !text.contains("\"type\":\"message\"") { "session-v2" } else { "session-v2-linear" }
}

// ---------------------------------------------------------------- import

fn import(input: &Path, opts: &ImportOptions) -> Result<Imported> {
    let mut set = LedgerSet::new();
    let mut losses = Losses::default();
    let (root, files): (PathBuf, Vec<(String, PathBuf)>) = if input.is_file() {
        let name = input.file_name().unwrap().to_string_lossy().to_string();
        (input.parent().unwrap_or(Path::new(".")).to_path_buf(), vec![(name, input.to_path_buf())])
    } else {
        (input.to_path_buf(), jsonl::walk(input, ".jsonl"))
    };
    if files.is_empty() {
        bail!("{}: no droid session .jsonl files", input.display());
    }
    let mut variant = None;
    for (rel, path) in &files {
        let text = std::fs::read_to_string(path).with_context(|| path.display().to_string())?;
        // A file with nothing in it yet names no session: its id would be the
        // file's, not the session's (SPEC §9.3, prefix-stable).
        if text.trim().is_empty() {
            continue;
        }
        variant.get_or_insert(variant_of(&text));
        let mut l = import_text(&text, rel, opts, &mut losses)?;
        // Sidecars, only when importing a directory (a lone .jsonl is a lone file).
        if input.is_dir() {
            let stem = rel.trim_end_matches(".jsonl");
            for suffix in [SETTINGS, SETTINGS_BAK] {
                let srel = format!("{stem}{suffix}");
                let sp = root.join(&srel);
                if sp.is_file() {
                    let st = std::fs::read_to_string(&sp)?;
                    import_sidecar(&mut l, &st, &srel, suffix == SETTINGS, opts);
                }
            }
        }
        set.insert(l);
    }
    if set.is_empty() {
        bail!("{}: no droid session record yet", input.display());
    }
    link_agents(&mut set);
    Ok(Imported { set, losses, variant: variant.unwrap_or("session-v2").to_string() })
}

/// `callingSessionId` makes a structural subagent; `parent` + `forkedAtMessageId`
/// a fork. Both become `agent.parent` only when the other session is present.
fn link_agents(set: &mut LedgerSet) {
    let ids: HashSet<String> = set.ledgers.keys().cloned().collect();
    let mut updates: Vec<(String, Parent)> = Vec::new();
    for l in set.iter() {
        let Some(extra) = l.agent().source.as_ref().map(|s| &s.extra) else { continue };
        // A Task subagent is harness-run: structural even when the caller's
        // file is not in this set.
        if let Some(caller) = extra.get("calling_session").and_then(Value::as_str) {
            let call = extra.get("calling_tool_use").and_then(Value::as_str);
            let ev = call.and_then(|c| set.get(caller)?.events.iter().find(|e| e.kind == Kind::ToolCall && e.str("call_id") == Some(c)).map(|e| e.id.clone()));
            updates.push((l.id().to_string(), Parent { agent: caller.to_string(), event: ev, relation: Relation::Subagent }));
        } else if let Some(p) = extra.get("fork_parent").and_then(Value::as_str).filter(|c| ids.contains(*c)) {
            let at = extra.get("forked_at").and_then(Value::as_str);
            let ev = at.and_then(|a| set.get(p)?.events.iter().find(|e| e.id == a).map(|e| e.id.clone()));
            updates.push((l.id().to_string(), Parent { agent: p.to_string(), event: ev, relation: Relation::Fork }));
        }
    }
    for (id, p) in updates {
        let l = set.ledgers.get_mut(&id).unwrap();
        if l.agent().parent.is_none() {
            l.agent_mut().parent = Some(p);
        }
    }
}

/// Model/effort seen so far: a `settings` event is emitted only when an
/// assistant message changes them.
#[derive(Default, Clone)]
struct State {
    model: Option<String>,
    effort: Option<String>,
}

fn import_text(text: &str, rel: &str, opts: &ImportOptions, losses: &mut Losses) -> Result<Ledger> {
    let lines = jsonl::lines(text);
    let variant = variant_of(text);
    let start = lines.iter().find_map(|l| l.value.as_ref().filter(|v| str_at(v, "type") == Some("session_start")));
    let sid =
        start.and_then(|s| str_at(s, "id")).map(str::to_string).unwrap_or_else(|| rel.rsplit('/').next().unwrap_or(rel).trim_end_matches(".jsonl").to_string());
    let vtag = start.and_then(|s| s.get("version")).and_then(Value::as_u64).map(|v| format!("session-v{v}"));
    let mut extra = Obj::new();
    extra.insert("file".into(), json!(rel));
    if !text.is_empty() && !text.ends_with('\n') {
        extra.insert("final_newline".into(), json!(false));
    }
    let mut links = Vec::new();
    if let Some(s) = start {
        if let Some(c) = str_at(s, "callingSessionId") {
            extra.insert("calling_session".into(), json!(c));
            if let Some(t) = str_at(s, "callingToolUseId") {
                extra.insert("calling_tool_use".into(), json!(t));
            }
        }
        if let Some(p) = str_at(s, "parent") {
            extra.insert("fork_parent".into(), json!(p));
            if let Some(a) = str_at(s, "forkedAtMessageId") {
                extra.insert("forked_at".into(), json!(a));
            }
            links.push(Link { rel: "forked-from".into(), agent: Some(p.into()), event: None, extra: Obj::new() });
        }
    }
    let created = lines.iter().find_map(|l| l.value.as_ref().and_then(|v| str_at(v, "timestamp")).map(str::to_string));
    let agent = Agent {
        id: sid.clone(),
        created_at: created,
        source: Some(Source { format: FORMAT.into(), variant: Some(variant.into()), version: vtag.clone(), native_id: Some(sid.clone()), extra }),
        parent: None,
        links,
        meta: Obj::new(),
    };
    let mut l = Ledger::new(agent);
    let mut st = State::default();
    let mut prev: Option<String> = None;
    let mut carried_agent = None;
    for line in &lines {
        let anchor = format!("@{}", line.off);
        let Some(value) = &line.value else {
            // The variant is the header's: decided by later lines (whether any
            // message has a `parentId`), so no record carries it (SPEC §9.3).
            let mut n = jsonl::native_of(FORMAT, variant, vtag.as_deref(), &anchor, line, Some(rel));
            n.variant = None;
            l.append_unique(Event::new(anchor, Kind::Native).native(n));
            losses.add("invalid-json-line", Some(Kind::Native), "a line that is not JSON is kept only verbatim");
            continue;
        };
        let mut rec = value.clone();
        let mut nl = Line { off: line.off, text: line.text, value: Some(rec.clone()), exact: line.exact };
        match jsonl::take(&mut rec, opts.restore_carried) {
            Taken::Restored(evs, a) => {
                for e in evs {
                    l.append_unique(e);
                }
                if a.is_some() {
                    carried_agent = a;
                }
                if let Some(id) = str_at(&rec, "id").filter(|_| str_at(&rec, "type") == Some("message")) {
                    prev = Some(id.to_string());
                }
                // Keep the model state in step with what the record says.
                let _ = record_events(&rec, "", &mut st);
                continue;
            }
            Taken::Stale if opts.restore_carried => {
                nl.value = Some(rec.clone());
                nl.exact = false;
            }
            _ => {}
        }
        let id = record_id(&rec, &anchor);
        let mut evs = record_events(&rec, &id, &mut st);
        let mut native = jsonl::native_of(FORMAT, variant, vtag.as_deref(), &id, &nl, Some(rel));
        native.variant = None;
        let is_msg = str_at(&rec, "type") == Some("message");
        let after = if is_msg { str_at(&rec, "parentId").filter(|p| prev.as_deref() != Some(*p)).map(str::to_string) } else { None };
        if is_msg {
            prev = Some(id.clone());
        }
        for (i, e) in evs.iter_mut().enumerate() {
            e.native = Some(if i == 0 { native.clone() } else { native.part_of(i as u32) });
            if i == 0 {
                e.after = after.clone();
            }
        }
        for e in evs {
            l.append_unique(e);
        }
    }
    if let Some(a) = carried_agent {
        l.header.agent = a;
    }
    Ok(l)
}

fn record_id(rec: &Value, anchor: &str) -> String {
    str_at(rec, "id").map(str::to_string).unwrap_or_else(|| anchor.to_string())
}

/// Neutral events for one droid record. Pure apart from the model state.
/// SPEC §3.1.5: what a thinking block's `signature` seals. Behind a
/// chat-completions API it is only the name of the field the reasoning came in
/// (`reasoning_content`), not a seal; behind OpenAI's Responses API it is the
/// reasoning item as JSON, whose `encrypted_content` is the seal; otherwise it
/// is the provider's signature.
fn droid_sealed(sig: &str, provider: &str) -> Option<Value> {
    match provider {
        "generic-chat-completion-api" => None,
        "openai" => {
            let item: Value = serde_json::from_str(sig).ok()?;
            let enc = item.get("encrypted_content").and_then(Value::as_str)?;
            Some(json!({"vendor": "openai", "kind": "encrypted", "data": enc}))
        }
        p => Some(json!({"vendor": p, "kind": "signature", "data": sig})),
    }
}

fn record_events(rec: &Value, id: &str, st: &mut State) -> Vec<Event> {
    let ts = str_at(rec, "timestamp").map(str::to_string);
    let mut out: Vec<Event> = Vec::new();
    let mk = |out: &Vec<Event>, kind: Kind| {
        let n = out.len();
        Event::new(if n == 0 { id.to_string() } else { format!("{id}#{n}") }, kind).ts(ts.clone())
    };
    match str_at(rec, "type").unwrap_or("") {
        "session_start" => {
            let e = mk(&out, Kind::Settings).with("cwd", rec.get("cwd").cloned().unwrap_or(Value::Null));
            out.push(e);
            if let Some(t) = str_at(rec, "sessionTitle").or(str_at(rec, "title")) {
                let e = mk(&out, Kind::Title).with("text", t);
                out.push(e);
            }
        }
        "message" => {
            let m = rec.get("message").cloned().unwrap_or(Value::Null);
            let role = str_at(&m, "role").unwrap_or("user");
            let hidden = str_at(&m, "visibility") == Some("user_only");
            if role == "assistant" {
                let model = str_at(&m, "modelId").map(str::to_string);
                let effort = str_at(&m, "reasoningEffort").map(str::to_string);
                if (model.is_some() && model != st.model) || (effort.is_some() && effort != st.effort) {
                    let mut e = mk(&out, Kind::Settings);
                    if model.is_some() && model != st.model {
                        e = e.with("model", model.clone());
                    }
                    if effort.is_some() && effort != st.effort {
                        e = e.with("effort", effort.clone());
                    }
                    out.push(e);
                }
                if model.is_some() {
                    st.model = model;
                }
                if effort.is_some() {
                    st.effort = effort;
                }
            }
            let provider = str_at(&m, "apiProvider").unwrap_or("droid").to_string();
            let blocks: Vec<Value> = match m.get("content") {
                Some(Value::Array(a)) => a.clone(),
                Some(Value::String(s)) => vec![json!({"type":"text","text": s})],
                _ => vec![],
            };
            for b in &blocks {
                let e = match str_at(b, "type") {
                    Some("text") => Some(mk(&out, Kind::Text).with("role", role).with("text", b.get("text").cloned().unwrap_or(Value::Null))),
                    Some("thinking") => {
                        let mut e = mk(&out, Kind::Reasoning).with("text", b.get("thinking").cloned().unwrap_or(Value::Null));
                        if let Some(sig) = str_at(b, "signature").filter(|s| !s.is_empty())
                            && let Some(sealed) = droid_sealed(sig, str_at(b, "signatureProvider").unwrap_or(&provider))
                        {
                            e = e.with("sealed", sealed);
                        }
                        Some(e)
                    }
                    Some("redacted_thinking") => Some(
                        mk(&out, Kind::Reasoning)
                            .with("redacted", true)
                            .with("sealed", json!({"vendor": "anthropic", "kind": "encrypted", "data": b.get("data")})),
                    ),
                    Some("tool_use") => {
                        let name = str_at(b, "name").unwrap_or("");
                        let mut e = mk(&out, Kind::ToolCall)
                            .with("call_id", b.get("id").cloned().unwrap_or(Value::Null))
                            .with("name", name)
                            .with("input", b.get("input").cloned().unwrap_or(json!({})));
                        if let Some(rest) = name.strip_prefix("mcp__") {
                            e = e.with("tool_type", "mcp").with("server", rest.split("__").next().unwrap_or(""));
                        } else {
                            e = e.with("tool_type", "function");
                        }
                        Some(e)
                    }
                    Some("tool_result") => Some(
                        mk(&out, Kind::ToolResult)
                            .with("call_id", b.get("tool_use_id").cloned().unwrap_or(Value::Null))
                            .with("output", tool_output(b.get("content")))
                            .with("is_error", b.get("is_error").cloned().unwrap_or(Value::Null)),
                    ),
                    Some("image") => Some(
                        mk(&out, Kind::Media)
                            .with("role", role)
                            .with("media_type", b.pointer("/source/media_type").cloned().unwrap_or(Value::Null))
                            .with("data", b.pointer("/source/data").cloned().unwrap_or(Value::Null)),
                    ),
                    _ => None,
                };
                if let Some(mut e) = e {
                    if role == "assistant" {
                        e.group = Some(id.to_string());
                    }
                    if hidden {
                        e.audience = Some(Audience::Record);
                    }
                    out.push(e);
                }
            }
            if out.is_empty() {
                // Hook audit rows (`content: []` + hook* fields) and other empties.
                out.push(mk(&out, Kind::Native));
            }
        }
        "agent_turn_outcome" => {
            let reason = str_at(rec, "reason").unwrap_or("");
            let outcome = match reason {
                "completed" => "ok",
                "cancelled" | "interrupted" | "aborted" => "interrupted",
                _ => "error",
            };
            let mut e = mk(&out, Kind::Turn).with("phase", "end").with("outcome", outcome).with("reason", reason);
            e.turn = str_at(rec, "turnId").map(str::to_string);
            out.push(e);
        }
        "compaction_state" => {
            let e = mk(&out, Kind::Compaction)
                .with("phase", "summary")
                .with("trigger", "auto")
                .with("summary", rec.get("summaryText").cloned().unwrap_or(Value::Null))
                .with("tokens_after", rec.get("summaryTokens").cloned().unwrap_or(Value::Null));
            out.push(e);
        }
        // todo_state and anything newer: droid-only state.
        _ => out.push(mk(&out, Kind::Native)),
    }
    out
}

fn tool_output(content: Option<&Value>) -> Value {
    match content {
        Some(Value::String(s)) => json!(s),
        Some(Value::Array(blocks)) => Value::Array(
            blocks
                .iter()
                .filter_map(|b| match str_at(b, "type") {
                    Some("text") => Some(json!({"type":"text","text": b.get("text")})),
                    Some("image") => Some(json!({"type":"media","media_type": b.pointer("/source/media_type"),"data": b.pointer("/source/data")})),
                    _ => None,
                })
                .collect(),
        ),
        Some(Value::Null) | None => json!(""),
        Some(other) => json!(other.to_string()),
    }
}

/// `<uuid>.settings.json`: a settings snapshot plus cumulative usage, placed
/// right after `session_start`. The `.bak` copy is kept verbatim only.
fn import_sidecar(l: &mut Ledger, text: &str, rel: &str, current: bool, opts: &ImportOptions) {
    let parsed: Option<Value> = serde_json::from_str(text).ok();
    let id = format!("{}{}", l.id(), if current { SETTINGS } else { SETTINGS_BAK });
    let mut n = Native::new(FORMAT, Some(id.clone())).file(Some(rel)).raw(text);
    let mut evs: Vec<Event> = Vec::new();
    match (&parsed, current) {
        (Some(v), true) => {
            let mut v = v.clone();
            match jsonl::take(&mut v, opts.restore_carried) {
                // Rendered from facts carried elsewhere: nothing of its own.
                Taken::Restored(..) => return,
                // The file's own text, token for token (SPEC §4); it is written back from `raw`.
                Taken::None => {
                    jsonl::hold_file(&mut n, text);
                    n.raw = Some(text.to_string());
                }
                Taken::Stale => n.set_record(Some(v.clone())),
            }
            evs = settings_events(&v, &id);
            for (i, e) in evs.iter_mut().enumerate() {
                e.native = Some(if i == 0 { n.clone() } else { n.part_of(i as u32) });
            }
        }
        _ => {
            if !jsonl::hold_file(&mut n, text) {
                n.set_record(parsed);
            }
            n.raw = Some(text.to_string());
            evs.push(Event::new(id, Kind::Native).native(n));
        }
    }
    // Insert after the session_start events (native id = the session id).
    let sid = l.id().to_string();
    let pos = l.events.iter().take_while(|e| e.native.as_ref().is_some_and(|n| n.id.as_deref() == Some(sid.as_str()))).count();
    let header = l.header.clone();
    let mut events = std::mem::take(&mut l.events);
    let tail = events.split_off(pos);
    let mut fresh = Ledger::new(header.agent.clone());
    fresh.header = header;
    for e in events.into_iter().chain(evs).chain(tail) {
        fresh.append_unique(e);
    }
    *l = fresh;
}

fn settings_events(v: &Value, id: &str) -> Vec<Event> {
    let mut out = Vec::new();
    let mut s = Event::new(id, Kind::Settings)
        .with("model", v.get("model").cloned().unwrap_or(Value::Null))
        .with("effort", v.get("reasoningEffort").cloned().unwrap_or(Value::Null))
        .with("permission_mode", v.get("autonomyMode").or(v.get("autonomyLevel")).cloned().unwrap_or(Value::Null));
    if let Some(p) = str_at(v, "providerLock") {
        s = s.with("provider", p);
    }
    out.push(s);
    if let Some(u) = v.get("tokenUsage").filter(|u| u.is_object()) {
        let g = |k: &str| u.get(k).cloned().unwrap_or(Value::Null);
        out.push(
            Event::new(format!("{id}#1"), Kind::Usage)
                .with("model", v.get("model").cloned().unwrap_or(Value::Null))
                .with("input", g("inputTokens"))
                .with("output", g("outputTokens"))
                .with("cache_read", g("cacheReadTokens"))
                .with("cache_write", g("cacheCreationTokens"))
                .with("reasoning", g("thinkingTokens"))
                .with("cumulative", true),
        );
    }
    out
}

// ---------------------------------------------------------------- export

fn export(set: &LedgerSet, opts: &ExportOptions) -> Result<Exported> {
    let mut output = Output::default();
    let mut losses = Losses::default();
    // Session ids, so a subagent can name its caller.
    let sids: HashMap<String, String> = set.iter().map(|l| (l.id().to_string(), session_id(l))).collect();
    for l in set.iter() {
        export_ledger(l, set, &sids, opts, &mut output, &mut losses);
    }
    Ok(Exported { output, losses: losses.sorted(), variant: "session-v2".into() })
}

fn session_id(l: &Ledger) -> String {
    l.agent()
        .source
        .as_ref()
        .filter(|s| s.format == FORMAT)
        .and_then(|s| s.native_id.clone())
        .unwrap_or_else(|| if jsonl::is_safe_id(l.id()) { l.id().to_string() } else { util::uuid_for(l.id()) })
}

/// droid's directory key for a cwd: `/` and `\` become `-`, a drive colon goes.
pub fn cwd_slug(cwd: &str) -> String {
    let s: String = cwd.replace(':', "").chars().map(|c| if c == '/' || c == '\\' { '-' } else { c }).collect();
    if s.starts_with('-') { s } else { format!("-{s}") }
}

enum Item {
    Raw(String),
    Rec(Value, Vec<Event>),
}

struct Builder<'a> {
    l: &'a Ledger,
    items: Vec<Item>,
    last_msg: Option<String>,
    /// The message id each rendered event went into, so an event that
    /// continues from an earlier one (SPEC §2.3 `after`) names it as `parentId`.
    msg_of: HashMap<String, String>,
    open_asst: Option<(usize, Option<String>)>,
    open_user: Option<(usize, Option<String>)>,
    /// user message holding tool results, open while results keep coming.
    open_results: Option<usize>,
    last_user_id: Option<String>,
    pending: Vec<Event>,
    model: Option<String>,
    /// SPEC §3.1.3: the provider a settings event names (`providerLock`).
    provider: Option<String>,
    effort: Option<String>,
    usage: [u64; 5],
    have_usage: bool,
    losses: &'a mut Losses,
}

fn export_ledger(l: &Ledger, set: &LedgerSet, sids: &HashMap<String, String>, opts: &ExportOptions, out: &mut Output, losses: &mut Losses) {
    let src = l.agent().source.as_ref().filter(|s| s.format == FORMAT);
    let sid = sids[l.id()].clone();
    let cwd = l.events.iter().filter(|e| e.kind == Kind::Settings).find_map(|e| e.str("cwd").map(str::to_string));
    let path = src.and_then(|s| s.extra.get("file")).and_then(Value::as_str).map(str::to_string).unwrap_or_else(|| match &cwd {
        Some(c) if !c.is_empty() => format!("{}/{sid}.jsonl", cwd_slug(c)),
        _ => format!("{sid}.jsonl"),
    });
    let final_newline = src.and_then(|s| s.extra.get("final_newline")).and_then(Value::as_bool).unwrap_or(true);
    let counts = jsonl::part_counts(l, FORMAT);
    let mut b = Builder {
        l,
        items: vec![],
        last_msg: None,
        msg_of: HashMap::new(),
        open_asst: None,
        open_user: None,
        open_results: None,
        last_user_id: None,
        pending: vec![],
        model: None,
        provider: None,
        effort: None,
        usage: [0; 5],
        have_usage: false,
        losses,
    };
    let native_start = l.events.iter().any(|e| e.native.as_ref().and_then(|n| n.record_json()).is_some_and(|r| str_at(&r, "type") == Some("session_start")));
    if !native_start {
        let title = l.events.iter().find(|e| e.kind == Kind::Title).and_then(|e| e.str("text")).unwrap_or("").to_string();
        let mut s = json!({"type":"session_start","id": sid,"title": title,"sessionTitle": title,"owner":"protoledger","version":2,
            "cwd": cwd.clone().unwrap_or_default()});
        if let Some(p) = &l.agent().parent
            && let Some(psid) = sids.get(&p.agent).cloned().or_else(|| Some(p.agent.clone()))
        {
            match p.relation {
                Relation::Subagent => {
                    s["callingSessionId"] = json!(psid);
                    let call = p.event.as_ref().and_then(|ev| set.get(&p.agent)?.events.iter().find(|e| &e.id == ev)?.str("call_id").map(str::to_string));
                    if let Some(c) = call {
                        s["callingToolUseId"] = json!(c);
                    }
                }
                Relation::Fork => {
                    s["parent"] = json!(psid);
                }
            }
        }
        if opts.carry {
            jsonl::attach(&mut s, &[], Some(l.agent()));
        }
        b.items.push(Item::Raw(serde_json::to_string(&s).unwrap()));
    }
    let mut done: HashSet<String> = HashSet::new();
    let mut st = State::default();
    let mut sidecars: Vec<(String, String)> = vec![];
    for (i, e) in l.events.iter().enumerate() {
        if let Some(n) = e.native.as_ref().filter(|n| n.format == FORMAT) {
            let nid = n.id.clone().unwrap_or_default();
            if n.part == 0 {
                done.remove(&nid);
                // A sidecar settings file.
                if let Some(file) = n.file.as_ref().filter(|f| f.ends_with(SETTINGS) || f.ends_with(SETTINGS_BAK)) {
                    let want = match &n.record_json() {
                        Some(r) if file.ends_with(SETTINGS) => settings_events(r, &nid).len(),
                        _ => 1,
                    };
                    // The file as read; else its record's text laid out as Droid writes the file.
                    if counts[i] == want
                        && let Some(raw) = crate::util::current_raw(n).map(str::to_string).or_else(|| n.record_text().map(crate::util::pretty))
                    {
                        sidecars.push((file.clone(), raw));
                        done.insert(nid.clone());
                        continue;
                    }
                } else {
                    let want = n.record_json().as_ref().map(|r| record_events(r, &nid, &mut st.clone()).len()).unwrap_or(1);
                    if counts[i] == want
                        && let Some(line) = jsonl::line_of(n)
                    {
                        if let Some(r) = &n.record_json() {
                            let _ = record_events(r, &nid, &mut st);
                            if str_at(r, "type") == Some("message") {
                                b.last_msg = str_at(r, "id").map(str::to_string);
                                if let Some(m) = &b.last_msg {
                                    b.msg_of.insert(e.id.clone(), m.clone());
                                }
                                if r.pointer("/message/role").and_then(Value::as_str) == Some("user") {
                                    b.last_user_id = b.last_msg.clone();
                                }
                            }
                        }
                        b.flush();
                        b.items.push(Item::Raw(line));
                        done.insert(nid.clone());
                        continue;
                    }
                }
            } else if done.contains(&nid) {
                continue;
            }
        }
        b.push(e);
    }
    b.finish(opts.carry);
    let mut text = String::new();
    for it in &b.items {
        match it {
            Item::Raw(s) => text.push_str(s),
            Item::Rec(v, _) => text.push_str(&serde_json::to_string(v).unwrap()),
        }
        text.push('\n');
    }
    if !final_newline {
        text.pop();
    }
    let stem = path.trim_end_matches(".jsonl").to_string();
    let wrote_settings = sidecars.iter().any(|(f, _)| f.ends_with(SETTINGS));
    for (f, raw) in sidecars {
        out.put(f, raw.into_bytes());
    }
    // A foreign ledger with model or usage facts gets a settings file.
    if !wrote_settings && src.is_none() && (b.model.is_some() || b.provider.is_some() || b.have_usage) {
        let mut s = Map::new();
        if let Some(m) = &b.model {
            s.insert("model".into(), json!(m));
        }
        if let Some(p) = &b.provider {
            s.insert("providerLock".into(), json!(p));
        }
        if let Some(e) = &b.effort {
            s.insert("reasoningEffort".into(), json!(e));
        }
        if b.have_usage {
            let u = b.usage;
            s.insert(
                "tokenUsage".into(),
                json!({"inputTokens":u[0],"outputTokens":u[1],"cacheCreationTokens":u[3],"cacheReadTokens":u[2],"thinkingTokens":u[4]}),
            );
        }
        let mut v = Value::Object(s);
        if opts.carry {
            jsonl::mark(&mut v);
        }
        out.put(format!("{stem}{SETTINGS}"), jsonl::pretty(&v).into_bytes());
    }
    out.put(path, text.into_bytes());
}

impl Builder<'_> {
    fn flush(&mut self) {
        self.open_asst = None;
        self.open_user = None;
        self.open_results = None;
    }

    fn ts(&self, e: &Event) -> String {
        e.ts.clone()
            .or_else(|| {
                let pos = self.l.events.iter().position(|x| x.id == e.id).unwrap_or(0);
                self.l.events[..pos].iter().rev().find_map(|x| x.ts.clone())
            })
            .or_else(|| self.l.agent().created_at.clone())
            .unwrap_or_else(|| util::ms_to_rfc3339(0))
    }

    fn message(&mut self, e: &Event, role: &str, block: Value) -> usize {
        self.flush();
        let id = util::uuid_for(&format!("{}\u{0}{}", self.l.id(), e.id));
        let mut m = json!({"role": role, "content": [block]});
        if role == "assistant" {
            if let Some(model) = &self.model {
                m["modelId"] = json!(model);
            }
            if let Some(eff) = &self.effort {
                m["reasoningEffort"] = json!(eff);
            }
        }
        let mut r = json!({"type":"message","id": id,"timestamp": self.ts(e),"message": m});
        // SPEC §2.3: an event that continues from an earlier one than the last
        // message hangs off that event's message; the messages between stay
        // in the file as a branch of the `parentId` tree.
        let branch = e.after.as_deref().and_then(|a| self.msg_of.get(a)).filter(|p| self.last_msg.as_ref() != Some(*p)).cloned();
        if let Some(p) = branch.as_ref().or(self.last_msg.as_ref()) {
            r["parentId"] = json!(p);
        }
        let mut evs = std::mem::take(&mut self.pending);
        evs.push(e.clone());
        for x in &evs {
            self.msg_of.insert(x.id.clone(), id.clone());
        }
        self.items.push(Item::Rec(r, evs));
        self.last_msg = Some(id.clone());
        if role == "user" {
            self.last_user_id = Some(id);
        }
        self.items.len() - 1
    }

    fn append(&mut self, idx: usize, e: &Event, block: Value) {
        let pending = std::mem::take(&mut self.pending);
        if let Item::Rec(v, evs) = &mut self.items[idx] {
            if let Some(id) = v.get("id").and_then(Value::as_str) {
                for x in pending.iter().chain([e]) {
                    self.msg_of.insert(x.id.clone(), id.to_string());
                }
            }
            v["message"]["content"].as_array_mut().unwrap().push(block);
            evs.extend(pending);
            evs.push(e.clone());
        }
    }

    fn other(&mut self, e: &Event, rec: Value) {
        self.flush();
        let mut evs = std::mem::take(&mut self.pending);
        evs.push(e.clone());
        self.items.push(Item::Rec(rec, evs));
    }

    fn assistant(&mut self, e: &Event, block: Value) {
        if let Some((idx, g)) = self.open_asst.clone()
            && g == e.group
        {
            return self.append(idx, e, block);
        }
        let idx = self.message(e, "assistant", block);
        self.open_asst = Some((idx, e.group.clone()));
    }

    fn user(&mut self, e: &Event, block: Value, extra: Option<(&str, Value)>) {
        if extra.is_none()
            && let Some((idx, g)) = self.open_user.clone()
            && g.is_some()
            && g == e.group
        {
            return self.append(idx, e, block);
        }
        let idx = self.message(e, "user", block);
        if let Some((k, v)) = extra {
            if let Item::Rec(r, _) = &mut self.items[idx] {
                r["message"][k] = v;
            }
        } else {
            self.open_user = Some((idx, e.group.clone()));
        }
    }

    fn defer(&mut self, e: &Event, code: &str, detail: &str) {
        self.losses.add(code, Some(e.kind), detail);
        self.pending.push(e.clone());
    }

    fn push(&mut self, e: &Event) {
        let b = &e.body;
        let s = |k: &str| b.get(k).and_then(Value::as_str).map(str::to_string);
        match e.kind {
            Kind::Text => {
                let block = json!({"type":"text","text": s("text").unwrap_or_default()});
                match s("role").as_deref() {
                    Some("assistant") => self.assistant(e, block),
                    Some("system") | Some("developer") => {
                        self.losses.add("system-text-as-context", Some(Kind::Text), "system/developer text becomes an llm_only user message");
                        self.user(e, block, Some(("visibility", json!("llm_only"))));
                    }
                    _ if e.effective_audience() == Audience::Record => self.user(e, block, Some(("visibility", json!("user_only")))),
                    _ => self.user(e, block, None),
                }
            }
            Kind::Media => match (s("data"), s("media_type")) {
                (Some(d), Some(mt)) if s("role").as_deref() != Some("assistant") => {
                    self.user(e, json!({"type":"image","source":{"type":"base64","data": d,"media_type": mt}}), None)
                }
                _ => self.defer(e, "media-dropped", "droid holds only inline base64 images in user messages"),
            },
            Kind::Reasoning => {
                let mut block = json!({"type":"thinking","thinking": s("text").unwrap_or_else(|| {
                    b.get("summary").and_then(Value::as_array).map(|a| a.iter().filter_map(Value::as_str).collect::<Vec<_>>().join("\n")).unwrap_or_default()
                })});
                if let Some(sealed) = b.get("sealed") {
                    if sealed.get("kind").and_then(Value::as_str) == Some("signature") {
                        block["signature"] = sealed.get("data").cloned().unwrap_or(Value::Null);
                        block["signatureProvider"] = sealed.get("vendor").cloned().unwrap_or(Value::Null);
                    } else if sealed.get("vendor").and_then(Value::as_str) == Some("openai")
                        && let Some(d) = sealed.get("data").and_then(Value::as_str)
                    {
                        // OpenAI's Responses API: the reasoning item as JSON (its `id` is not kept).
                        block["signature"] = json!(serde_json::to_string(&json!({"type": "reasoning", "content": [], "encrypted_content": d})).unwrap());
                        block["signatureProvider"] = json!("openai");
                    } else {
                        self.losses.add("reasoning-sealed-dropped", Some(Kind::Reasoning), "encrypted reasoning has no droid block");
                    }
                }
                self.assistant(e, block);
            }
            Kind::ToolCall => {
                let input = match (b.get("input"), s("input_text")) {
                    (Some(v @ Value::Object(_)), _) => v.clone(),
                    (_, Some(t)) => {
                        self.losses.add("tool-input-text-wrapped", Some(Kind::ToolCall), "raw tool input becomes {\"input\": text}");
                        json!({"input": t})
                    }
                    (None | Some(Value::Null), None) => json!({}),
                    (Some(v), None) => {
                        self.losses.add("tool-input-text-wrapped", Some(Kind::ToolCall), "non-object tool input becomes {\"input\": value}");
                        json!({"input": v})
                    }
                };
                self.assistant(
                    e,
                    json!({"type":"tool_use","id": s("call_id").unwrap_or_else(|| e.id.clone()),"name": s("name").unwrap_or_default(),"input": input}),
                );
            }
            Kind::ToolResult => {
                let content = match b.get("output") {
                    Some(Value::Array(parts)) => Value::Array(
                        parts
                            .iter()
                            .filter_map(|p| match p.get("type").and_then(Value::as_str) {
                                Some("text") => Some(json!({"type":"text","text": p.get("text")})),
                                Some("media") if p.get("data").is_some() => {
                                    Some(json!({"type":"image","source":{"type":"base64","data": p.get("data"),"media_type": p.get("media_type")}}))
                                }
                                _ => None,
                            })
                            .collect(),
                    ),
                    other => json!(jsonl::output_text(other)),
                };
                let block = json!({"type":"tool_result","tool_use_id": s("call_id").unwrap_or_default(),
                    "is_error": b.get("is_error").and_then(Value::as_bool).unwrap_or(false),"content": content});
                if let Some(idx) = self.open_results {
                    return self.append(idx, e, block);
                }
                let idx = self.message(e, "user", block);
                self.open_results = Some(idx);
            }
            Kind::Settings => {
                if let Some(m) = s("model") {
                    self.model = Some(m);
                }
                if let Some(x) = s("effort") {
                    self.effort = Some(x);
                }
                if let Some(p) = s("provider") {
                    self.provider = Some(p);
                }
                // Rendered as modelId/reasoningEffort on later assistant
                // messages and in the settings file; the event itself is carried.
                self.pending.push(e.clone());
            }
            Kind::Usage => {
                let n = |k: &str| b.get(k).and_then(Value::as_u64).unwrap_or(0);
                let v = [n("input"), n("output"), n("cache_read"), n("cache_write"), n("reasoning")];
                if b.get("cumulative").and_then(Value::as_bool) == Some(true) {
                    self.usage = v;
                } else {
                    for (u, x) in self.usage.iter_mut().zip(v) {
                        *u += x;
                    }
                }
                self.have_usage = true;
                self.losses.add("usage-aggregated", Some(Kind::Usage), "per-message usage is summed into settings.json tokenUsage");
                self.pending.push(e.clone());
            }
            Kind::Compaction if s("phase").as_deref() == Some("summary") || b.get("summary").is_some() => {
                let id = util::uuid_for(&format!("{}\u{0}{}", self.l.id(), e.id));
                let rec = json!({"type":"compaction_state","id": id,"timestamp": self.ts(e),"summaryText": s("summary").unwrap_or_default(),
                    "summaryTokens": b.get("tokens_after").and_then(Value::as_u64).unwrap_or(0),"summaryKind":"llm_summary","removedCount":0});
                self.other(e, rec);
            }
            Kind::Compaction => self.defer(e, "compaction-boundary-dropped", "a compaction boundary without a summary has no droid record"),
            Kind::Turn if s("phase").as_deref() == Some("end") => {
                let reason = match s("outcome").as_deref() {
                    Some("ok") | None => "completed".to_string(),
                    _ => s("reason").unwrap_or_else(|| "error".into()),
                };
                let turn = e.turn.clone().or(self.last_user_id.clone()).unwrap_or_default();
                self.other(e, json!({"type":"agent_turn_outcome","turnId": turn,"reason": reason,"resultKind":"text"}));
            }
            Kind::Turn => self.pending.push(e.clone()),
            Kind::Title => self.pending.push(e.clone()),
            Kind::PeerMessage => {
                // SPEC §3.1.6: wrapped text, which the importer unwraps.
                if crate::formats::pi_session::peer_has_more(e) {
                    self.losses.add(
                        crate::formats::pi_session::PEER_FIELDS_DROPPED,
                        Some(Kind::PeerMessage),
                        crate::formats::pi_session::PEER_FIELDS_DROPPED_DETAIL,
                    );
                }
                let (role, text) = crate::formats::pi_session::peer_as_text(e);
                let block = json!({"type":"text","text": text});
                if role == "assistant" { self.assistant(e, block) } else { self.user(e, block, None) }
            }
            Kind::Subagent => self.defer(e, "subagent-marker-dropped", "droid links subagents by session_start fields only"),
            Kind::Error => self.defer(e, "error-dropped", "droid has no error record"),
            Kind::FileChange => self.defer(e, "file-change-dropped", "droid has no file-change records"),
            Kind::App => self.defer(e, "app-dropped", "application records have no droid record"),
            Kind::Native => self.defer(e, "foreign-native-dropped", "another harness's native record"),
        }
    }

    fn finish(&mut self, carry: bool) {
        if !self.pending.is_empty() {
            let p = std::mem::take(&mut self.pending);
            match self.items.iter().rposition(|i| matches!(i, Item::Rec(..))) {
                Some(i) => {
                    if let Item::Rec(_, evs) = &mut self.items[i] {
                        evs.extend(p);
                    }
                }
                None if carry => {
                    // Nothing was rendered: carry on an empty todo_state, which droid ignores.
                    let id = util::uuid_for(&format!("{}\u{0}protoledger-carrier", self.l.id()));
                    let ts = p.iter().find_map(|x| x.ts.clone()).unwrap_or_else(|| self.ts(&p[0]));
                    self.items.push(Item::Rec(json!({"type":"todo_state","id": id,"timestamp": ts,"todos":{"todos":""},"messageIndex":0}), p));
                }
                None => {}
            }
        }
        if carry {
            for it in &mut self.items {
                if let Item::Rec(v, evs) = it {
                    jsonl::attach(v, evs, None);
                }
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// SPEC §2.3, decision "the target expresses the branch": a rewound
    /// ledger is written as a `parentId` tree whose last message continues
    /// from the message `after` names, and the importer's `after` brings the
    /// abandoned branch back.
    #[test]
    fn a_rewound_ledger_is_written_as_a_branch_of_the_parent_tree() {
        let fx = |c: &str| {
            let p = concat!(env!("CARGO_MANIFEST_DIR"), "/fixtures/protoledger/").to_string() + c + "/native";
            crate::formats::by_name("protoledger").unwrap().import(Path::new(&p), &ImportOptions::standard()).unwrap().set
        };
        let texts = |evs: Vec<&Event>| -> Vec<String> { evs.iter().filter(|e| e.kind == Kind::Text).filter_map(|e| e.str("text").map(String::from)).collect() };
        let trip = |set: &LedgerSet| -> Ledger {
            let out = Droid.export(set, &ExportOptions::no_carry()).unwrap();
            let t = tempfile::tempdir().unwrap();
            let path = crate::roundtrip::materialise(&out.output, t.path()).unwrap();
            let back = Droid.import(&path, &ImportOptions::standard()).unwrap().set;
            back.iter().next().unwrap().clone()
        };
        let src = fx("branch-rewind");
        let l0 = src.iter().next().unwrap();
        assert!(l0.is_branched(), "sentinel: the source is rewound");
        let l1 = trip(&src);
        let said = |l: &Ledger| l.events.iter().filter(|e| !matches!(e.kind, Kind::Settings | Kind::Title)).count();
        assert_eq!(said(&l1), said(l0), "every event is written, the abandoned branch too");
        assert!(l1.is_branched(), "the branch comes back");
        assert_eq!(texts(l1.active_path()), texts(l0.active_path()), "the active path is the source's");
        // Opposed: an unbranched ledger stays one chain.
        assert!(!trip(&fx("minimal")).is_branched(), "no branch where the source has none");
    }

    const SAMPLE: &str = concat!(
        r#"{"type":"session_start","id":"s1","title":"t","owner":"o","version":2,"cwd":"/home/user/p"}"#,
        "\n",
        r#"{"type":"message","id":"m1","timestamp":"2026-07-01T00:00:00.000Z","message":{"role":"user","content":[{"type":"text","text":"hi"}]}}"#,
        "\n",
        r#"{"type":"message","id":"m2","timestamp":"2026-07-01T00:00:01.000Z","message":{"role":"assistant","content":[{"type":"thinking","signature":"s","signatureProvider":"openai","thinking":"x"},{"type":"tool_use","id":"c1","name":"Execute","input":{"command":"ls"}}],"modelId":"m","reasoningEffort":"low"},"parentId":"m1"}"#,
        "\n",
        r#"{"type":"message","id":"m3","timestamp":"2026-07-01T00:00:02.000Z","message":{"role":"user","content":[{"type":"tool_result","tool_use_id":"c1","is_error":false,"content":"a"}]},"parentId":"m2"}"#,
        "\n",
        r#"{"type":"agent_turn_outcome","turnId":"m1","reason":"completed","resultKind":"text"}"#,
        "\n",
    );

    #[test]
    fn import_and_self_export() {
        let mut losses = Losses::default();
        let l = import_text(SAMPLE, "s1.jsonl", &ImportOptions::standard(), &mut losses).unwrap();
        let kinds: Vec<Kind> = l.events.iter().map(|e| e.kind).collect();
        assert_eq!(kinds, vec![Kind::Settings, Kind::Title, Kind::Text, Kind::Settings, Kind::Reasoning, Kind::ToolCall, Kind::ToolResult, Kind::Turn]);
        assert_eq!(l.events[3].str("model"), Some("m"));
        let out = export(&LedgerSet::single(l), &ExportOptions::standard()).unwrap();
        assert_eq!(String::from_utf8(out.output.files["s1.jsonl"].clone()).unwrap(), SAMPLE);
    }

    #[test]
    fn records_are_held_and_reemitted_as_written() {
        use crate::formats::atif::jsonfmt::without_raw;
        use crate::formats::openclaw::jsonl::spelling::{ODD, assert_value_loses, held};
        let start = r#"{"type":"session_start","id":"s1","title":"t","owner":"o","version":2,"cwd":"/home/user/p"}"#;
        let line = format!(
            r#"{{"type":"message","id":"m1","timestamp":"2026-07-01T00:00:00.000Z","message":{{"role":"user","content":[{{"type":"text","text":"hi"}}]}},{ODD}}}"#
        );
        assert_value_loses(&line);
        let src = format!("{start}\n{line}\n");
        let settings = "{\n  \"model\": \"m\",\n  \"b\": 1,\n  \"a\": 1.0,\n  \"c\": 0.000005,\n  \"d\": 1e3,\n  \"g\": \"\\u00e9\"\n}";
        let settings_held = r#"{"model":"m","b":1,"a":1.0,"c":0.000005,"d":1e3,"g":"\u00e9"}"#;
        assert_value_loses(settings_held);
        let mut losses = Losses::default();
        let mut l = import_text(&src, "s1.jsonl", &ImportOptions::standard(), &mut losses).unwrap();
        import_sidecar(&mut l, settings, "s1.settings.json", true, &ImportOptions::standard());
        let set = LedgerSet::single(l);
        let want_held = [start.to_string(), settings_held.to_string(), line.clone()];
        assert_eq!(held(&set, FORMAT), want_held, "the ledger holds each record as written");
        let out = export(&set, &ExportOptions::standard()).unwrap();
        assert_eq!(String::from_utf8(out.output.files["s1.jsonl"].clone()).unwrap(), src);
        assert_eq!(String::from_utf8(out.output.files["s1.settings.json"].clone()).unwrap(), settings);
        // And from the records' texts alone.
        let out = export(&without_raw(&set), &ExportOptions::standard()).unwrap();
        assert_eq!(String::from_utf8(out.output.files["s1.jsonl"].clone()).unwrap(), src);
        assert_eq!(String::from_utf8(out.output.files["s1.settings.json"].clone()).unwrap(), settings);
    }

    #[test]
    fn slug_rule() {
        assert_eq!(cwd_slug("/home/user/p"), "-home-user-p");
        assert_eq!(cwd_slug("C:\\Users\\a_b\\x"), "-C-Users-a_b-x");
    }

    #[test]
    fn foreign_round_trip_with_carry_is_clean() {
        let mut l = Ledger::new(Agent { id: "f1".into(), created_at: None, source: None, parent: None, links: vec![], meta: Obj::new() });
        l.append(Event::new("s", Kind::Settings).with("cwd", "/w").with("model", "m1")).unwrap();
        l.append(Event::new("u", Kind::Text).with("role", "user").with("text", "go").ts(Some("2026-01-01T00:00:00Z"))).unwrap();
        l.append(Event::new("c", Kind::ToolCall).with("call_id", "k").with("name", "Read").with("input", json!({"p":1})).group(Some("g"))).unwrap();
        l.append(Event::new("r", Kind::ToolResult).with("call_id", "k").with("output", "x")).unwrap();
        l.append(Event::new("us", Kind::Usage).with("input", 5).with("output", 2)).unwrap();
        let set = LedgerSet::single(l);
        let out = export(&set, &ExportOptions::standard()).unwrap();
        assert!(out.output.files.contains_key("-w/f1.settings.json"));
        let dir = tempfile::tempdir().unwrap();
        out.output.write_to(dir.path(), true).unwrap();
        let back = import(dir.path(), &ImportOptions::standard()).unwrap().set;
        let d = crate::loss::compare(&set, &back, crate::loss::CompareOptions { ignore_native: true, ..Default::default() });
        assert!(d.is_clean(), "{}", d.summary());
    }
}
