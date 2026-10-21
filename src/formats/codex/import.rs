//! Codex rollout → protoledger.

use super::{Carried, FORMAT, SESSION_INDEX, V_LEGACY, V_TS, V_V0, canonical_variant, read_text, rollout_files, sniff, take_carrier};
use crate::formats::{ImportOptions, Imported, Losses};
use crate::ledger::{Ledger, LedgerSet};
use crate::model::{Agent, Event, Kind, Link, Native, Obj, Parent, Relation, Source};
use crate::util::OrderedMap;
use crate::util::minify;
use anyhow::{Result, bail};
use serde_json::{Value, json};
use std::collections::{HashMap, HashSet};
use std::path::Path;

mod follow;
pub(super) use follow::follow;

pub(super) fn import(input: &Path, opts: &ImportOptions) -> Result<Imported> {
    let files = rollout_files(input)?;
    if files.is_empty() {
        bail!("{}: no Codex rollout files found", input.display());
    }
    let forced = match &opts.variant {
        Some(v) => Some(canonical_variant(v).ok_or_else(|| anyhow::anyhow!("codex: unknown variant {v:?}"))?),
        None => None,
    };
    let mut losses = Losses::default();
    let mut files_out: Vec<FileLedger> = Vec::new();
    let mut titles = if input.is_dir() { read_session_index(&input.join(SESSION_INDEX), opts)? } else { HashMap::new() };
    for (rel, path) in &files {
        let ft = read_text(path)?;
        let variant = match forced {
            Some(v) => v,
            None => match sniff(rel, &ft.text) {
                Some((v, _)) => v,
                None => {
                    if files.len() == 1 {
                        bail!("{}: not a Codex rollout", path.display());
                    }
                    losses.add("file-not-rollout", None, &format!("{rel} is not a Codex rollout and was skipped"));
                    continue;
                }
            },
        };
        let mut f = if variant == V_TS { import_ts_json(rel, &ft.text, opts, &mut losses)? } else { import_jsonl(rel, &ft.text, variant, opts, &mut losses) };
        if let Some(t) = f.thread_id.as_ref().and_then(|id| titles.remove(id)) {
            let mut events = std::mem::take(&mut f.ledger.events);
            merge_titles(&mut events, t);
            let mut l = Ledger::new(f.ledger.agent().clone());
            for e in events {
                l.append_unique(e);
            }
            f.ledger = l;
        }
        if f.restored_header {
            // A carried header: the container facts still belong to this file.
            if let Some(src) = f.ledger.agent_mut().source.as_mut() {
                if src.format == FORMAT {
                    src.extra.extend(ft.extra.clone());
                } else if !ft.extra.is_empty() {
                    src.extra.insert("codex".into(), Value::Object(ft.extra.clone()));
                }
            }
        } else if let Some(src) = f.ledger.agent_mut().source.as_mut() {
            src.extra.extend(ft.extra.clone());
        }
        files_out.push(f);
    }
    if !titles.is_empty() {
        losses.add("title-thread-missing", Some(Kind::Title), "session_index.jsonl names threads not in the input");
    }
    link_agents(&mut files_out);
    let mut set = LedgerSet::new();
    let mut variant = String::new();
    for f in files_out {
        if variant.is_empty() || f.variant != V_LEGACY {
            variant = f.variant.to_string();
        }
        let mut l = f.ledger;
        // Ids must be unique within the set; a reverted thread can share a thread id.
        let mut id = l.id().to_string();
        let mut n = 2;
        while set.get(&id).is_some() {
            id = format!("{}~{n}", l.id());
            n += 1;
        }
        l.agent_mut().id = id;
        set.insert(l);
    }
    Ok(Imported { set, losses, variant })
}

/// `session_index.jsonl` (`rollout/src/session_index.rs:20-50`): every line
/// names a thread (Codex appends one per rename; the last is the name), and
/// becomes a `title` event in that thread's ledger, placed by its
/// `updated_at`. Returns thread id → its title events.
fn read_session_index(path: &Path, opts: &ImportOptions) -> Result<HashMap<String, Vec<Event>>> {
    let mut out: HashMap<String, Vec<Event>> = HashMap::new();
    if !path.is_file() {
        return Ok(out);
    }
    let text = std::fs::read_to_string(path)?;
    let (lines, _) = split_lines(&text);
    for (off, line) in lines {
        let Some(mut v) = serde_json::from_str::<Value>(line).ok().filter(Value::is_object) else { continue };
        let mut raw = Some(line.to_string());
        let mut exact = Some(line);
        if v.get(crate::carry::KEY).is_some() {
            let original = v.clone();
            match take_carrier(&mut v) {
                // The title it names rode in the rollout's own carrier.
                Some(_) if opts.restore_carried => continue,
                _ => {
                    v = original;
                    // A `protoledger` member that is no carrier is the line's own.
                    if crate::carry::strip(&mut v) {
                        raw = None;
                        exact = None;
                    }
                }
            }
        }
        let (Some(id), Some(name)) = (s(&v, "id"), s(&v, "thread_name")) else { continue };
        let anchor = format!("@{off}");
        let mut n = Native::new(FORMAT, Some(anchor.clone())).file(Some(SESSION_INDEX));
        set_record_exact(&mut n, Some(v.clone()), exact);
        n.raw = raw.filter(|r| n.record_text() != Some(r.as_str()));
        let e = Event::new(format!("{SESSION_INDEX}{anchor}"), Kind::Title).ts(s(&v, "updated_at")).with("text", name).native(n);
        out.entry(id).or_default().push(e);
    }
    Ok(out)
}

/// Place each title where it was written: after the first run of events
/// stamped with its `updated_at` (an exporter stamps a name with the time of
/// what preceded it), else before the first event that happened after it.
fn merge_titles(events: &mut Vec<Event>, titles: Vec<Event>) {
    let ms = |e: &Event| e.ts.as_deref().and_then(crate::util::parse_rfc3339_ms);
    for t in titles {
        let pos = ms(&t).and_then(|at| match events.iter().position(|e| e.kind != Kind::Title && ms(e) == Some(at)) {
            Some(first) => {
                let run = events[first..].iter().take_while(|e| e.kind == Kind::Title || ms(e) == Some(at)).count();
                Some(first + run)
            }
            None => events.iter().position(|e| ms(e).is_some_and(|x| x > at)),
        });
        events.insert(pos.unwrap_or(events.len()), t);
    }
}

/// One imported file, with the cross-file facts the linker needs.
pub(super) struct FileLedger {
    pub ledger: Ledger,
    pub variant: &'static str,
    /// The header came from a carrier and must not be re-derived.
    pub restored_header: bool,
    pub thread_id: Option<String>,
    pub parent_thread_id: Option<String>,
    pub forked_from_id: Option<String>,
    /// child thread id → event id of the parent's spawn record.
    pub spawns: HashMap<String, String>,
}

fn link_agents(files: &mut [FileLedger]) {
    let by_thread: HashMap<String, (String, HashMap<String, String>)> =
        files.iter().filter_map(|f| f.thread_id.clone().map(|t| (t, (f.ledger.id().to_string(), f.spawns.clone())))).collect();
    for f in files.iter_mut() {
        if f.restored_header {
            continue;
        }
        let me = f.thread_id.clone();
        if let Some(p) = &f.parent_thread_id {
            let (agent, event) = match by_thread.get(p) {
                Some((agent, spawns)) => (agent.clone(), me.as_ref().and_then(|t| spawns.get(t).cloned())),
                None => (p.clone(), None),
            };
            f.ledger.agent_mut().parent = Some(Parent { agent, event, relation: Relation::Subagent });
        }
        if let Some(src) = &f.forked_from_id {
            let agent = by_thread.get(src).map(|(a, _)| a.clone()).unwrap_or_else(|| src.clone());
            f.ledger.agent_mut().links.push(Link { rel: "forked-from".into(), agent: Some(agent), event: None, extra: Obj::new() });
        }
    }
}

// ---------------------------------------------------------------------------
// Line splitting

/// Split into `(byte offset, line)` keeping every line (blank and torn ones
/// too); the flag says whether the text ended with a newline.
fn split_lines(text: &str) -> (Vec<(usize, &str)>, bool) {
    let mut out = Vec::new();
    let mut off = 0;
    for piece in text.split_inclusive('\n') {
        out.push((off, piece.strip_suffix('\n').unwrap_or(piece)));
        off += piece.len();
    }
    (out, text.is_empty() || text.ends_with('\n'))
}

// ---------------------------------------------------------------------------
// The per-file importer

struct Ctx {
    variant: &'static str,
    version: Option<String>,
    file: Option<String>,
    opts: ImportOptions,
    turn: Option<String>,
    /// This thread's agent path (`/root`, `/root/worker`), for peer direction.
    self_path: Option<String>,
    model: Option<String>,
    last_settings: Option<Obj>,
    saw_meta: bool,
    /// Paginated subagents: ordinals below this were copied from the parent.
    inherited_until: Option<u64>,
    /// The last `token_count` totals seen.
    last_total: Option<Value>,
    /// The usage of a `token_usage_record` whose `token_count` copy has not been seen.
    pending_record: Option<Value>,
    /// The anchor of the line being read (`@<offset>`, `@items/<i>`).
    anchor: String,
    /// The group of the response being read (SPEC §3.1.4): its first output
    /// item's id (or event id), given to its output items as they are read and
    /// to its `token_usage_record`; `None` between responses.
    run: Option<String>,
    /// A response whose output items a tool output followed before any
    /// `token_usage_record` did: the next record still reports on it.
    closed_run: Option<String>,
    events: Vec<Event>,
    /// (class, text, event index) of model-visible content that UI records copy.
    primaries: Vec<(&'static str, String, usize)>,
    /// Indices into `primaries` of those a UI record copies.
    claimed: HashSet<usize>,
    /// (class, text, event index) of the UI records of the line being read.
    dups: Vec<(&'static str, String, usize)>,
    /// UI records read before any item they copy (Codex shows an answer or
    /// reasoning, then records it): (class, text, the neutral event their
    /// content makes if no item claims them before the response ends).
    waiting: Vec<(&'static str, String, Event)>,
    /// event index → native record of every response item, for compaction matching.
    response_records: Vec<(usize, Value)>,
    header: HeaderFacts,
    restored_agent: Option<Agent>,
}

#[derive(Default)]
struct HeaderFacts {
    thread_id: Option<String>,
    created_at: Option<String>,
    parent_thread_id: Option<String>,
    forked_from_id: Option<String>,
}

impl Ctx {
    fn new(variant: &'static str, opts: &ImportOptions) -> Self {
        Ctx {
            variant,
            version: None,
            file: None,
            opts: opts.clone(),
            turn: None,
            self_path: None,
            model: None,
            last_settings: None,
            saw_meta: false,
            inherited_until: None,
            last_total: None,
            pending_record: None,
            anchor: String::new(),
            run: None,
            closed_run: None,
            events: Vec::new(),
            primaries: Vec::new(),
            claimed: HashSet::new(),
            dups: Vec::new(),
            waiting: Vec::new(),
            response_records: Vec::new(),
            header: HeaderFacts::default(),
            restored_agent: None,
        }
    }

    fn native(&self, anchor: &str) -> Native {
        Native::new(FORMAT, Some(anchor.to_string())).variant(Some(self.variant)).version(self.version.clone()).file(self.file.clone())
    }

    /// Push the events one native record made, giving the first the record:
    /// held as `text`, the source's own spelling of it, when there is one.
    fn push(&mut self, anchor: &str, ts: Option<&str>, mut evs: Vec<Event>, record: Option<Value>, text: Option<&str>, raw: Option<String>) -> usize {
        if evs.is_empty() {
            evs.push(Event::new("", Kind::Native));
        }
        let several = evs.len() > 1;
        let first = self.events.len();
        let mut base = self.native(anchor);
        set_record_exact(&mut base, record, text);
        // A second copy only when the record could not be held as that text.
        base.raw = raw.filter(|r| base.record_text() != Some(r.as_str()));
        for (i, mut e) in evs.into_iter().enumerate() {
            e.id = if several { format!("{anchor}#{i}") } else { anchor.to_string() };
            if e.ts.is_none() {
                e.ts = ts.map(str::to_string);
            }
            if e.turn.is_none() {
                e.turn = self.turn.clone();
            }
            // The parts of one message belong together even without an item id.
            if several && e.group.is_none() && matches!(e.kind, Kind::Text | Kind::Media) {
                e.group = Some(anchor.to_string());
            }
            e.native = Some(if i == 0 { base.clone() } else { base.part_of(i as u32) });
            self.events.push(e);
        }
        first
    }

    fn restore(&mut self, carried: Carried) {
        if let Some(a) = carried.agent {
            self.restored_agent = Some(a);
        }
        self.events.extend(carried.events);
    }
}

fn import_jsonl(rel: &str, text: &str, variant: &'static str, opts: &ImportOptions, losses: &mut Losses) -> FileLedger {
    let mut ctx = Ctx::new(variant, opts);
    let (lines, final_newline) = split_lines(text);
    for (off, line) in lines {
        read_line(&mut ctx, off, line, losses);
    }
    finish(ctx, rel, jsonl_extra(rel, final_newline), losses)
}

fn jsonl_extra(rel: &str, final_newline: bool) -> Obj {
    let mut extra = Obj::new();
    extra.insert("file".into(), json!(rel));
    if !final_newline {
        extra.insert("final_newline".into(), json!(false));
    }
    extra
}

/// Map one line of a JSONL rollout, at byte offset `off`.
fn read_line(ctx: &mut Ctx, off: usize, line: &str, losses: &mut Losses) {
    {
        let anchor = format!("@{off}");
        let parsed: Option<Value> = serde_json::from_str::<Value>(line).ok().filter(Value::is_object);
        let Some(mut v) = parsed else {
            // Blank, torn or malformed: Codex skips it (`recorder.rs:1119-1125`); keep the bytes.
            if !line.trim().is_empty() {
                losses.add("line-malformed", None, "a line that is not a JSON object was kept as raw native text");
            }
            ctx.push(&anchor, None, vec![], None, None, Some(line.to_string()));
            return;
        };
        // `raw` is kept only if the record cannot be held as the line's own
        // text ([`Ctx::push`] drops it otherwise: carriers keep exact text).
        let mut raw = Some(line.to_string());
        let mut exact = Some(line);
        let settles = settles(ctx, &v);
        if v.get(crate::carry::KEY).is_some() {
            let original = v.clone();
            match take_carrier(&mut v) {
                Some(c) if ctx.opts.restore_carried => {
                    settle_waiting(ctx, settles);
                    ctx.restore(c);
                    return;
                }
                // Edited since export (or restoring is off): import the line
                // fresh. A `protoledger` member that is no carrier is the
                // line's own, and the line stays exact.
                _ => {
                    v = original;
                    if crate::carry::strip(&mut v) {
                        raw = None;
                        exact = None;
                    }
                }
            }
        }
        let ts = v.get("timestamp").and_then(Value::as_str).filter(|t| *t != super::UNKNOWN_TS).map(str::to_string);
        let inherited = ctx.inherited_until.zip(v.get("ordinal").and_then(Value::as_u64)).is_some_and(|(end, o)| o < end);
        ctx.anchor = anchor.clone();
        let primaries_before = ctx.primaries.len();
        let evs = map_line(ctx, &v, ts.as_deref());
        // SPEC §9.3: only what was read decides. An item this line records
        // claims the UI records that showed it first; a line after which their
        // item can no longer come settles the rest, whose content is emitted
        // here, before the line's own events.
        claim_waiting(ctx, primaries_before);
        settle_waiting(ctx, settles);
        fix_dup_indices(ctx, evs.len());
        let first = ctx.push(&anchor, ts.as_deref(), evs, Some(v), exact, raw);
        if inherited {
            // Parent records copied into a subagent's file as its starting context
            // (`SessionMeta::subagent_history_start_ordinal`, `protocol.rs:3316-3320`).
            for e in &mut ctx.events[first..] {
                let m = e.meta.entry("codex").or_insert_with(|| json!({}));
                m["inherited"] = json!(true);
            }
        }
        pair_ui_records(ctx);
    }
}

/// The classes of waiting UI records (see [`pair_ui_records`]) whose item can
/// no longer come once this record is read. Codex shows a model output, then
/// records it, within one response, and a response's items come in the
/// Responses API's order: reasoning, then messages, then calls (in 0.47 every
/// UI record of a response, then a `token_count`, then its items). So:
///
/// - a record that ends the response settles them all: a user or developer
///   message, a tool's output, the `token_usage_record`, a turn's start or
///   end, a compaction;
/// - an assistant message settles reasoning; a call settles everything;
/// - any record but a UI record settles a user prompt's copy (Codex records
///   a prompt, then shows it, so one still waiting has no item).
fn settles(ctx: &Ctx, v: &Value) -> fn(&str) -> bool {
    fn all(_: &str) -> bool {
        true
    }
    fn none(_: &str) -> bool {
        false
    }
    fn user(c: &str) -> bool {
        c == "user"
    }
    fn up_to_messages(c: &str) -> bool {
        c == "user" || c.starts_with("reasoning")
    }
    if ctx.variant == V_V0 {
        return none;
    }
    let p = v.get("payload").unwrap_or(&Value::Null);
    let pt = p.get("type").and_then(Value::as_str).unwrap_or("");
    match v.get("type").and_then(Value::as_str).unwrap_or("") {
        "response_item" => match pt {
            "message" if p.get("role").and_then(Value::as_str) == Some("assistant") => up_to_messages,
            "message" => all,
            t if t.ends_with("_output") => all,
            "reasoning" => user,
            "function_call" | "custom_tool_call" | "local_shell_call" | "web_search_call" | "tool_search_call" | "image_generation_call" => all,
            _ => user,
        },
        "token_usage_record" | "compacted" => all,
        "event_msg" => match pt {
            "task_started" | "turn_started" | "task_complete" | "turn_complete" | "turn_aborted" | "context_compacted" => all,
            "user_message" | "agent_message" | "agent_reasoning" | "agent_reasoning_raw_content" => none,
            "item_completed" if matches!(p.pointer("/item/type").and_then(Value::as_str), Some("UserMessage" | "AgentMessage" | "Reasoning")) => none,
            _ => user,
        },
        _ => user,
    }
}

/// `map_line` records dup/primary indices relative to the events it returns;
/// rebase them on where those events will land.
fn fix_dup_indices(ctx: &mut Ctx, _n: usize) {
    let base = ctx.events.len();
    for list in [&mut ctx.primaries, &mut ctx.dups] {
        for e in list.iter_mut().rev() {
            if e.2 >= PENDING {
                e.2 = base + (e.2 - PENDING);
            } else {
                break;
            }
        }
    }
    for e in ctx.response_records.iter_mut().rev() {
        if e.0 >= PENDING {
            e.0 = base + (e.0 - PENDING);
        } else {
            break;
        }
    }
}

const PENDING: usize = usize::MAX / 2;

/// The compact texts of a TypeScript-CLI document's session and items, sliced
/// out of `compact` unchanged (SPEC §4); `None` when the shape is unexpected.
fn ts_record_texts(compact: &str) -> Option<(Option<String>, Vec<String>)> {
    use serde_json::value::RawValue;
    #[derive(serde::Deserialize)]
    struct Doc<'a> {
        #[serde(borrow, default)]
        session: Option<&'a RawValue>,
        #[serde(borrow, default)]
        items: Vec<&'a RawValue>,
    }
    let doc: Doc = serde_json::from_str(compact).ok()?;
    Some((doc.session.map(|r| r.get().to_string()), doc.items.iter().map(|r| r.get().to_string()).collect()))
}

fn import_ts_json(rel: &str, text: &str, opts: &ImportOptions, losses: &mut Losses) -> Result<FileLedger> {
    let doc: Value = serde_json::from_str(text)?;
    let compact = minify(text);
    let (session_text, item_texts) = ts_record_texts(&compact).unwrap_or_default();
    let mut ctx = Ctx::new(V_TS, opts);
    let mut session = doc.get("session").cloned().unwrap_or(json!({}));
    let mut session_text = session_text.filter(|_| doc.get("session").is_some());
    let mut restored_session = false;
    if session.get(crate::carry::KEY).is_some() {
        session_text = None;
        match take_carrier(&mut session) {
            Some(c) if opts.restore_carried => {
                ctx.restore(c);
                restored_session = true;
            }
            _ => {
                crate::carry::strip(&mut session);
            }
        }
    }
    if !restored_session {
        let ts = session.get("timestamp").and_then(Value::as_str).map(str::to_string);
        let evs = map_v0_meta(&mut ctx, &session);
        let raw = session_text.clone();
        ctx.push("@session", ts.as_deref(), evs, Some(session.clone()), session_text.as_deref(), raw);
    }
    let items = doc.get("items").and_then(Value::as_array).cloned().unwrap_or_default();
    for (i, mut item) in items.into_iter().enumerate() {
        let anchor = format!("@items/{i}");
        let mut item_text = item_texts.get(i).cloned();
        if item.get(crate::carry::KEY).is_some() {
            item_text = None;
            let original = item.clone();
            match take_carrier(&mut item) {
                Some(c) if opts.restore_carried => {
                    ctx.restore(c);
                    continue;
                }
                _ => {
                    item = original;
                    crate::carry::strip(&mut item);
                }
            }
        }
        ctx.anchor = anchor.clone();
        let evs = map_response_item(&mut ctx, &item, None);
        fix_dup_indices(&mut ctx, evs.len());
        let raw = item_text.clone();
        ctx.push(&anchor, None, evs, Some(item), item_text.as_deref(), raw);
    }
    // The file is JSON.stringify(doc, null, 2): its records' texts laid out
    // again reproduce it byte for byte; when they do not, keep the text.
    let mut extra = Obj::new();
    extra.insert("file".into(), json!(rel));
    let pretty = crate::util::pretty(&compact);
    if pretty != text {
        if format!("{pretty}\n") == text {
            extra.insert("final_newline".into(), json!(true));
        } else {
            extra.insert("raw_text".into(), json!(text));
            losses.add("ts-json-raw-kept", None, "the JSON document did not re-serialise exactly; its text was kept");
        }
    }
    finish(ctx, rel, extra, losses).pipe(Ok)
}

trait Pipe: Sized {
    fn pipe<T>(self, f: impl FnOnce(Self) -> T) -> T {
        f(self)
    }
}
impl<T> Pipe for T {}

fn finish(ctx: Ctx, rel: &str, extra: Obj, losses: &mut Losses) -> FileLedger {
    if !ctx.waiting.is_empty() {
        losses.add_n(
            "ui-record-unsettled",
            None,
            ctx.waiting.len() as u64,
            "a UI record at the end of the file whose item may still be written: its content is in its native record until the response ends",
        );
    }
    let agent = header_of(&ctx, rel, extra);
    let restored_header = ctx.restored_agent.is_some();
    let mut ledger = Ledger::new(agent);
    for e in ctx.events {
        ledger.append_unique(e);
    }
    mark_rollbacks(&mut ledger);
    FileLedger {
        spawns: spawns_of(&ledger),
        ledger,
        variant: ctx.variant,
        restored_header,
        thread_id: ctx.header.thread_id,
        parent_thread_id: ctx.header.parent_thread_id,
        forked_from_id: ctx.header.forked_from_id,
    }
}

/// child thread id → the parent event that spawned it.
fn spawns_of(ledger: &Ledger) -> HashMap<String, String> {
    let mut spawns = HashMap::new();
    for e in &ledger.events {
        if e.kind == Kind::Subagent
            && e.str("phase") == Some("start")
            && let Some(child) = e.str("agent")
        {
            spawns.entry(child.to_string()).or_insert_with(|| e.id.clone());
        }
    }
    spawns
}

/// The file's agent, before the linker: a carried header, else what its records say.
fn header_of(ctx: &Ctx, rel: &str, extra: Obj) -> Agent {
    match ctx.restored_agent.clone() {
        Some(mut a) => {
            if let Some(src) = &mut a.source
                && src.format == FORMAT
            {
                for (k, v) in extra {
                    src.extra.insert(k, v);
                }
            }
            a
        }
        None => {
            let thread = ctx.header.thread_id.clone();
            let stem = rel.rsplit('/').next().unwrap_or(rel);
            let id = match (&thread, rollout_id_from_name(stem)) {
                (Some(t), Some(r)) if &r != t => format!("{t}_{r}"),
                (Some(t), _) => t.clone(),
                (None, _) => stem.trim_end_matches(".zst").trim_end_matches(".jsonl").trim_end_matches(".json").to_string(),
            };
            Agent {
                id,
                created_at: ctx.header.created_at.clone(),
                source: Some(Source { format: FORMAT.into(), variant: Some(ctx.variant.into()), version: ctx.version.clone(), native_id: thread, extra }),
                parent: None,
                links: vec![],
                meta: Obj::new(),
            }
        }
    }
}

/// A rollout record's item: a `response_item`'s payload, or a bare item (v0, ts-json).
fn item_of(rec: &Value) -> Option<&Value> {
    match rec.get("type").and_then(Value::as_str) {
        Some("response_item") => rec.get("payload"),
        Some("message") => Some(rec),
        _ => None,
    }
}

/// The `num_turns` of a `thread_rolled_back` record
/// (`EventMsg::ThreadRolledBack`, `protocol/src/protocol.rs:1515`, `:3814-3817`).
fn rolled_back_turns(rec: &Value) -> Option<usize> {
    let p = rec.get("payload").filter(|_| rec.get("type").and_then(Value::as_str) == Some("event_msg"))?;
    (p.get("type").and_then(Value::as_str) == Some("thread_rolled_back"))
        .then(|| p.get("num_turns").and_then(Value::as_u64).map(|n| usize::try_from(n).unwrap_or(usize::MAX)))
        .flatten()
}

/// Codex's compaction summary prefix, the text of
/// `prompts/templates/compact/summary_prefix.md` (openai/codex 0e67502), which
/// has no final newline: a local compaction persists its summary as a user
/// message `"{SUMMARY_PREFIX}\n{summary}"` (`core/src/compact.rs:404`) that
/// "must not consume a rollback turn" (`core/src/context_manager/history.rs:1382-1385`).
const SUMMARY_PREFIX: &str = "Another language model started to solve this problem and produced a summary of its thinking process. You also have access to the state of the tools that were used by that language model. Use this to build on the work that has already been done and avoid duplicating work. Here is the summary produced by the other language model, use the information in this summary to assist with your own analysis:";

/// `is_summary_message` (`core/src/compact.rs:622-624`): the prefix and a newline.
fn is_summary_message(t: &str) -> bool {
    t.strip_prefix(SUMMARY_PREFIX).is_some_and(|rest| rest.starts_with('\n'))
}

/// An absolute agent path as `AgentPath` deserialises one
/// (`protocol/src/agent_path.rs:149-171`, `:125-147`): `/morpheus`, or `/root`
/// followed by `/`-separated names of lowercase letters, digits and `_`.
fn is_agent_path(p: &str) -> bool {
    if p == "/morpheus" {
        return true;
    }
    let Some(rest) = p.strip_prefix("/root") else { return false };
    if rest.is_empty() {
        return true;
    }
    let Some(names) = rest.strip_prefix('/') else { return false };
    names.split('/').all(|n| !n.is_empty() && n != "root" && n.chars().all(|c| c.is_ascii_lowercase() || c.is_ascii_digit() || c == '_'))
}

/// `InterAgentCommunication::is_message_content` (`protocol/src/protocol.rs:1027-1038`,
/// the struct at `:912-929`): exactly one `input_text` or `output_text` part
/// whose text deserialises as an `InterAgentCommunication`, which needs
/// `author` and `recipient` agent paths, a string `content` and a boolean
/// `trigger_turn` (no serde default); `other_recipients` defaults to empty,
/// `id`, `encrypted_content` and the metadata passthrough are optional.
fn is_inter_agent_message_content(parts: &[Value]) -> bool {
    let [part] = parts else { return false };
    if !matches!(part.get("type").and_then(Value::as_str), Some("input_text" | "output_text")) {
        return false;
    }
    let Some(v) = part.get("text").and_then(Value::as_str).and_then(|t| serde_json::from_str::<Value>(t).ok()) else {
        return false;
    };
    let path = |k: &str| v.get(k).and_then(Value::as_str).is_some_and(is_agent_path);
    path("author")
        && path("recipient")
        && v.get("content").is_some_and(Value::is_string)
        && v.get("trigger_turn").is_some_and(Value::is_boolean)
        && v.get("other_recipients").is_none_or(|r| r.as_array().is_some_and(|a| a.iter().all(|x| x.as_str().is_some_and(is_agent_path))))
        && v.get("id").is_none_or(|x| x.is_null() || x.is_string())
        && v.get("encrypted_content").is_none_or(|x| x.is_null() || x.is_string())
}

/// Is this response item a user turn boundary, as Codex counts turns for a
/// rollback (`is_user_turn_boundary`, `core/src/context_manager/history.rs:1372-1387`,
/// at openai/codex 0e67502): an `agent_message`; a user `message` that is not
/// injected context (`is_contextual_user_message_content`) nor a compaction
/// summary ([`is_summary_message`]); an assistant `message` whose content is
/// an inter-agent instruction ([`is_inter_agent_message_content`]).
fn is_turn_boundary_item(item: &Value) -> bool {
    let ty = item.get("type").and_then(Value::as_str);
    if ty == Some("agent_message") {
        return true;
    }
    if ty != Some("message") {
        return false;
    }
    let parts = match item.get("content") {
        Some(Value::Array(a)) => a.clone(),
        Some(Value::String(t)) => vec![json!({"type": "input_text", "text": t})],
        _ => vec![],
    };
    match item.get("role").and_then(Value::as_str) {
        Some("user") => {
            !parts.iter().filter_map(|p| p.get("text").and_then(Value::as_str)).any(is_injected_context)
                && !parts
                    .iter()
                    .filter(|p| p.get("type").and_then(Value::as_str) == Some("input_text"))
                    .filter_map(|p| p.get("text").and_then(Value::as_str))
                    .any(is_summary_message)
        }
        Some("assistant") => is_inter_agent_message_content(&parts),
        _ => false,
    }
}

/// How one event moves Codex's list of user turns (`drop_last_n_user_turns`
/// counts the turns of the history the rollout rebuilds,
/// `core/src/session/rollout_reconstruction.rs:430-440`, `:505-549`).
enum TurnMark {
    None,
    /// The first event of a user turn.
    Turn,
    /// A compaction whose `replacement_history` replaces the history: the
    /// turns from here on are the boundaries in it (`:290-315`, `:520-545`).
    Replace(Vec<usize>),
}

/// The turn mark of `l.events[i]`. A record from Codex is read as Codex reads
/// it: a `response_item` boundary, an `inter_agent_communication` line
/// (always a turn, `:436-440`), or a compaction with a replacement history,
/// whose boundaries are the events it preserves verbatim (in file order) and,
/// for each boundary it does not, the compaction itself. An event carried from
/// another harness is a turn when it is user text or media the model saw, one
/// per group.
fn turn_mark(l: &Ledger, i: usize) -> TurnMark {
    let e = &l.events[i];
    if let Some(n) = e.native.as_ref().filter(|n| n.format == FORMAT) {
        if n.part != 0 {
            return TurnMark::None;
        }
        let Some(rec) = n.record_json() else { return TurnMark::None };
        let ty = rec.get("type").and_then(Value::as_str);
        if ty == Some("inter_agent_communication") {
            return TurnMark::Turn;
        }
        if ty == Some("compacted") {
            let Some(rh) = rec.pointer("/payload/replacement_history").and_then(Value::as_array) else {
                // A legacy compaction rebuilds history from the user messages
                // before it: the turns stand.
                return TurnMark::None;
            };
            let boundaries = rh.iter().filter(|x| is_turn_boundary_item(x)).count();
            let preserved: Vec<usize> = e
                .body
                .get("preserved")
                .and_then(Value::as_array)
                .into_iter()
                .flatten()
                .filter_map(Value::as_str)
                .filter_map(|id| l.events[..i].iter().position(|x| x.id == id))
                .filter(|&k| matches!(turn_mark(l, k), TurnMark::Turn))
                .collect();
            let mut turns = preserved;
            turns.sort_unstable();
            let missing = boundaries.saturating_sub(turns.len());
            turns.extend(std::iter::repeat_n(i, missing));
            return TurnMark::Replace(turns);
        }
        return match item_of(&rec) {
            Some(item) if is_turn_boundary_item(item) => TurnMark::Turn,
            _ => TurnMark::None,
        };
    }
    let user = |e: &Event| {
        matches!(e.kind, Kind::Text | Kind::Media | Kind::PeerMessage)
            && (e.kind == Kind::PeerMessage || e.str("role") == Some("user"))
            && e.effective_audience() == crate::model::Audience::Model
            && e.meta.get("codex").and_then(|m| m.get("injected")).is_none()
    };
    let prev = i.checked_sub(1).map(|p| &l.events[p]);
    if user(e) && !prev.is_some_and(|p| user(p) && p.group.is_some() && p.group == e.group) { TurnMark::Turn } else { TurnMark::None }
}

/// SPEC §2.3: a `thread_rolled_back` record drops the last `num_turns` user
/// turns (`core/src/session/rollout_reconstruction.rs:547-549`,
/// `context_manager/history.rs:777-796`: all of them when there are fewer).
/// It stays a native event, and its `after` names the event just before the
/// first dropped turn, so the dropped events stay in the ledger as an
/// abandoned branch.
fn mark_rollbacks(l: &mut Ledger) {
    let any = l.events.iter().any(is_rollback);
    if !any {
        return;
    }
    let mut turns: Vec<usize> = Vec::new();
    for i in 0..l.events.len() {
        rollback_step(l, i, &mut turns);
    }
}

/// Is this event a native `thread_rolled_back` record (that [`mark_rollbacks`] has to place)?
fn is_rollback(e: &Event) -> bool {
    e.native.as_ref().is_some_and(|n| n.format == FORMAT && n.record_text().is_some_and(|t| t.contains("thread_rolled_back")))
}

/// [`mark_rollbacks`] for `l.events[i]`, given `turns`: the index of the
/// first event of each user turn still in the conversation before it.
fn rollback_step(l: &mut Ledger, i: usize, turns: &mut Vec<usize>) {
    let e = &l.events[i];
    let rolled = e.native.as_ref().filter(|n| n.format == FORMAT && n.part == 0).and_then(|n| n.record_json()).and_then(|r| rolled_back_turns(&r));
    if let Some(n) = rolled {
        if n == 0 || turns.is_empty() {
            return;
        }
        let keep = turns.len().saturating_sub(n);
        let cut = turns[keep];
        turns.truncate(keep);
        if cut > 0 && l.events[i].after.is_none() {
            l.events[i].after = Some(l.events[cut - 1].id.clone());
        }
        return;
    }
    match turn_mark(l, i) {
        TurnMark::Turn => turns.push(i),
        TurnMark::Replace(t) => *turns = t,
        TurnMark::None => {}
    }
}

/// `rollout-<ts>-<thread>[_<rollout>].jsonl` → the rollout id (`rollout_file_name.rs:44-70`).
fn rollout_id_from_name(name: &str) -> Option<String> {
    let core = name.strip_prefix("rollout-")?;
    let core = core.strip_suffix(".zst").unwrap_or(core);
    let core = core.strip_suffix(".jsonl")?;
    let ids = core.get(20..)?;
    let (_, r) = ids.split_once('_').unwrap_or((ids, ids));
    Some(r.to_string())
}

// ---------------------------------------------------------------------------
// UI copies (legacy `event_msg`, paginated `item_completed`)

/// Pair the UI records of the line just read. A UI record whose item was
/// read before it (the order Codex records a prompt, then shows it) copies the
/// nearest unclaimed one of its class whose text it matches, and becomes a
/// `native` event naming it. Any other is `native` too, naming none, and
/// waits: an item read later in the response claims it (Codex shows an answer
/// or reasoning, then records it), and if none does by the time the response
/// ends, its content is emitted then, as its own event ([`settle_waiting`]).
/// Only what was read decides (SPEC §9.3: an importer is prefix-stable).
fn pair_ui_records(ctx: &mut Ctx) {
    for (class, text, di) in std::mem::take(&mut ctx.dups) {
        let best = ctx
            .primaries
            .iter()
            .enumerate()
            .filter(|(pi, (pc, ptext, pidx))| *pidx < di && *pc == class && !ctx.claimed.contains(pi) && texts_match(ptext, &text))
            .max_by_key(|(_, (_, _, pidx))| *pidx)
            .map(|(pi, _)| pi);
        match best {
            Some(pi) => {
                ctx.claimed.insert(pi);
                let primary = json!(ctx.events[ctx.primaries[pi].2].id);
                mark_copy(&mut ctx.events[di], primary);
            }
            None => {
                let e = &ctx.events[di];
                let mut content = e.clone();
                content.id = format!("{}#1", e.id);
                content.native = e.native.as_ref().map(|n| n.part_of(1));
                ctx.waiting.push((class, text, content));
                mark_copy(&mut ctx.events[di], Value::Null);
            }
        }
    }
}

/// The items the line just read records (`primaries[from..]`) claim the
/// waiting UI records that showed them: of each class, the nearest whose text
/// the item's matches.
fn claim_waiting(ctx: &mut Ctx, from: usize) {
    for pi in from..ctx.primaries.len() {
        let (pc, ptext, _) = &ctx.primaries[pi];
        if let Some(w) = ctx.waiting.iter().rposition(|(wc, wtext, _)| wc == pc && texts_match(ptext, wtext)) {
            ctx.waiting.remove(w);
            ctx.claimed.insert(pi);
        }
    }
}

/// UI records of the classes `settled` whose item can no longer come: Codex
/// showed content it did not record (or a message the model never saw), so
/// their content is emitted now, each as part 1 of its UI record's native
/// record.
fn settle_waiting(ctx: &mut Ctx, settled: fn(&str) -> bool) {
    let mut keep = Vec::new();
    for (class, text, e) in std::mem::take(&mut ctx.waiting) {
        if settled(class) {
            ctx.events.push(e);
        } else {
            keep.push((class, text, e));
        }
    }
    ctx.waiting = keep;
}

/// A UI record that copies the event `primary_id` (null: one read after it).
fn mark_copy(e: &mut Event, primary_id: Value) {
    e.kind = Kind::Native;
    e.body = Obj::new();
    e.audience = None;
    e.meta.insert("codex".into(), json!({"ui_copy_of": primary_id}));
}

fn texts_match(primary: &str, copy: &str) -> bool {
    let (p, c) = (primary.trim(), copy.trim());
    p == c || (!c.is_empty() && p.contains(c))
}

// ---------------------------------------------------------------------------
// Mapping

fn s(v: &Value, k: &str) -> Option<String> {
    v.get(k).and_then(Value::as_str).map(str::to_string)
}

fn map_line(ctx: &mut Ctx, v: &Value, ts: Option<&str>) -> Vec<Event> {
    // rollout-v0: the first line is bare session meta; later lines are bare items.
    if ctx.variant == V_V0 {
        if !ctx.saw_meta && v.get("type").is_none() && v.get("record_type").is_none() {
            return map_v0_meta(ctx, v);
        }
        if v.get("record_type").is_some() {
            return vec![];
        }
        return map_response_item(ctx, v, ts);
    }
    let ty = v.get("type").and_then(Value::as_str).unwrap_or("");
    let p = v.get("payload").cloned().unwrap_or(Value::Null);
    match ty {
        "session_meta" => map_session_meta(ctx, &p),
        "response_item" => map_response_item(ctx, &p, ts),
        "turn_context" => map_turn_context(ctx, &p),
        "event_msg" => map_event_msg(ctx, &p, ts),
        "compacted" => map_compacted(ctx, &p),
        "inter_agent_communication" => map_inter_agent(ctx, &p),
        "token_usage_record" => map_usage_record(ctx, &p),
        // world_state, retained_context, security_risk_score, realtime_item,
        // inter_agent_communication_metadata and unknown future types.
        _ => vec![],
    }
}

fn map_v0_meta(ctx: &mut Ctx, v: &Value) -> Vec<Event> {
    ctx.saw_meta = true;
    ctx.header.thread_id = s(v, "id");
    ctx.header.created_at = s(v, "timestamp");
    let mut e = Event::new("", Kind::Settings);
    if let Some(t) = v.get("instructions").and_then(Value::as_str).filter(|t| !t.is_empty()) {
        e = e.with("system_prompt", t);
    }
    if let Some(b) = v.pointer("/git/branch").and_then(Value::as_str) {
        e = e.with("git_branch", b);
    }
    if e.body.is_empty() { vec![] } else { vec![e] }
}

fn map_session_meta(ctx: &mut Ctx, p: &Value) -> Vec<Event> {
    if ctx.saw_meta {
        // Later session_meta lines are copied from a fork's parent (`recorder.rs:1131-1135`).
        return vec![];
    }
    ctx.saw_meta = true;
    ctx.version = s(p, "cli_version").or(ctx.version.take());
    ctx.header.thread_id = s(p, "id").or_else(|| s(p, "session_id"));
    ctx.header.created_at = s(p, "timestamp").filter(|t| t != super::UNKNOWN_TS);
    ctx.inherited_until = p.get("subagent_history_start_ordinal").and_then(Value::as_u64);
    ctx.header.forked_from_id = s(p, "forked_from_id");
    let spawn = p.pointer("/source/subagent/thread_spawn");
    ctx.header.parent_thread_id = spawn
        .and_then(|t| s(t, "parent_thread_id"))
        .or_else(|| p.pointer("/source/subagent").and(s(p, "parent_thread_id")))
        .or_else(|| s(p, "parent_thread_id").filter(|_| p.get("thread_source").is_some_and(|t| t != "user")));
    ctx.self_path = s(p, "agent_path").or_else(|| spawn.and_then(|t| s(t, "agent_path")));
    let mut e = Event::new("", Kind::Settings)
        .with("cwd", p.get("cwd").filter(|c| c.as_str() != Some(super::UNKNOWN_CWD)).cloned().unwrap_or(Value::Null))
        .with("provider", p.get("model_provider").cloned().unwrap_or(Value::Null))
        .with("model", p.get("model").cloned().unwrap_or(Value::Null))
        .with("system_prompt", p.pointer("/base_instructions/text").cloned().unwrap_or(Value::Null))
        .with("git_branch", p.pointer("/git/branch").cloned().unwrap_or(Value::Null));
    if p.get("instructions").and_then(Value::as_str).is_some_and(|t| !t.is_empty()) && !e.body.contains_key("system_prompt") {
        e = e.with("system_prompt", p["instructions"].clone());
    }
    let role = s(p, "agent_role").or_else(|| spawn.and_then(|t| s(t, "agent_role")));
    let nick = s(p, "agent_nickname").or_else(|| spawn.and_then(|t| s(t, "agent_nickname")));
    if role.is_some() || nick.is_some() {
        e = e.with("agent", json!({"role": role, "nickname": nick, "path": ctx.self_path}));
    }
    if let Some(m) = s(p, "model") {
        ctx.model = Some(m);
    }
    if e.body.is_empty() { vec![] } else { vec![e] }
}

fn map_turn_context(ctx: &mut Ctx, p: &Value) -> Vec<Event> {
    if let Some(t) = s(p, "turn_id") {
        ctx.turn = Some(t);
    }
    let mut e = Event::new("", Kind::Settings);
    for (from, to) in [
        ("model", "model"),
        ("cwd", "cwd"),
        ("effort", "effort"),
        ("approval_policy", "approval"),
        ("sandbox_policy", "sandbox"),
        ("summary", "reasoning_summary"),
        ("collaboration_mode", "collaboration_mode"),
    ] {
        if let Some(v) = p.get(from).filter(|v| !v.is_null()) {
            e.body.insert(to.into(), v.clone());
        }
    }
    if let Some(m) = s(p, "model") {
        ctx.model = Some(m);
    }
    // Codex writes one per turn; only a change is a neutral settings event.
    if ctx.last_settings.as_ref() == Some(&e.body) || e.body.is_empty() {
        return vec![];
    }
    ctx.last_settings = Some(e.body.clone());
    vec![e]
}

fn content_text(content: &Value, types: &[&str]) -> String {
    content
        .as_array()
        .map(|a| {
            a.iter()
                .filter(|c| c.get("type").and_then(Value::as_str).is_some_and(|t| types.contains(&t)))
                .filter_map(|c| c.get("text").and_then(Value::as_str))
                .collect::<Vec<_>>()
                .join("\n")
        })
        .unwrap_or_default()
}

/// `data:<media type>;base64,<data>` → (media type, data).
fn split_data_url(u: &str) -> Option<(String, String)> {
    let rest = u.strip_prefix("data:")?;
    let (head, data) = rest.split_once(',')?;
    let mt = head.strip_suffix(";base64")?;
    Some((mt.to_string(), data.to_string()))
}

fn image_body(e: Event, part: &Value) -> Event {
    let url = part.get("image_url").and_then(Value::as_str);
    match url.and_then(split_data_url) {
        Some((mt, data)) => e.with("media_type", mt).with("data", data),
        None => {
            let e = match url {
                Some(u) => e.with("url", u).with("media_type", guess_media_type(u)),
                None => e.with("media_type", "image/*"),
            };
            match part.get("file_id") {
                Some(f) => e.with("file_id", f.clone()),
                None => e,
            }
        }
    }
    .with("detail", part.get("detail").cloned().unwrap_or(Value::Null))
}

fn guess_media_type(u: &str) -> &'static str {
    let l = u.to_ascii_lowercase();
    if l.ends_with(".png") {
        "image/png"
    } else if l.ends_with(".jpg") || l.ends_with(".jpeg") {
        "image/jpeg"
    } else if l.ends_with(".gif") {
        "image/gif"
    } else if l.ends_with(".webp") {
        "image/webp"
    } else {
        "image/*"
    }
}

fn map_message(ctx: &mut Ctx, p: &Value) -> Vec<Event> {
    let role = p.get("role").and_then(Value::as_str).unwrap_or("user").to_string();
    let content = p.get("content").cloned().unwrap_or(Value::Null);
    let mut evs = Vec::new();
    let parts: Vec<Value> = match &content {
        Value::Array(a) => a.clone(),
        Value::String(t) => vec![json!({"type": "input_text", "text": t})],
        _ => vec![],
    };
    for part in &parts {
        let t = part.get("type").and_then(Value::as_str).unwrap_or("");
        match t {
            "input_text" | "output_text" | "text" => {
                let mut e = Event::new("", Kind::Text).with("role", role.as_str()).with("text", part.get("text").cloned().unwrap_or(json!("")));
                if let Some(ph) = p.get("phase").filter(|v| !v.is_null()) {
                    e = e.with("phase", ph.clone());
                }
                if role == "user" && is_injected_context(part.get("text").and_then(Value::as_str).unwrap_or("")) {
                    e = e.meta_ns("codex", json!({"injected": true}));
                }
                // SPEC §3.1.6: a peer message from outside the thread's agent tree.
                if role == "user"
                    && parts.len() == 1
                    && p.get("phase").is_none_or(Value::is_null)
                    && let Some((peer, network, text)) = part.get("text").and_then(Value::as_str).and_then(super::peer_unwrap)
                {
                    e = Event::new("", Kind::PeerMessage).with("direction", "in").with("peer", peer).with("network", network).with("text", text);
                }
                evs.push(e);
            }
            "refusal" => evs.push(
                Event::new("", Kind::Text).with("role", role.as_str()).with("text", part.get("refusal").cloned().unwrap_or(json!(""))).with("refusal", true),
            ),
            "input_image" => evs.push(image_body(Event::new("", Kind::Media).with("role", role.as_str()), part)),
            "input_audio" => {
                let e = Event::new("", Kind::Media).with("role", role.as_str());
                let u = part.get("audio_url").and_then(Value::as_str).unwrap_or("");
                evs.push(match split_data_url(u) {
                    Some((mt, data)) => e.with("media_type", mt).with("data", data),
                    None => e.with("media_type", "audio/*").with("url", u),
                });
            }
            _ => {}
        }
    }
    let group = p.get("id").and_then(Value::as_str).map(str::to_string);
    if evs.len() > 1 {
        for e in &mut evs {
            e.group = group.clone();
        }
    }
    if let Some(sealed) = p.get("encrypted_content").and_then(Value::as_str)
        && let Some(e) = evs.first_mut()
    {
        e.body.insert("sealed".into(), json!({"vendor": "openai", "kind": "encrypted", "data": sealed}));
    }
    let class = match role.as_str() {
        "user" => Some("user"),
        "assistant" => Some("assistant"),
        _ => None,
    };
    if let Some(class) = class {
        let text = content_text(&content, &["input_text", "output_text", "text"]);
        if !evs.is_empty() {
            ctx.primaries.push((class, text, PENDING));
        }
    }
    evs
}

/// User-role messages Codex writes itself (environment context, AGENTS.md,
/// aborted-turn notes); they are model-visible but no person typed them.
fn is_injected_context(t: &str) -> bool {
    let t = t.trim_start();
    ["<environment_context>", "<user_instructions>", "# AGENTS.md instructions", "<turn_aborted>", "<user_shell_command>", "<subagent_notification>", "<skill>"]
        .iter()
        .any(|p| t.starts_with(p))
}

fn output_body(e: Event, out: &Value) -> Event {
    match out {
        Value::String(t) => e.with("output", t.as_str()),
        Value::Array(items) => {
            let parts: Vec<Value> = items
                .iter()
                .map(|it| match it.get("type").and_then(Value::as_str) {
                    Some("input_text") | Some("output_text") | Some("text") => {
                        json!({"type": "text", "text": it.get("text").cloned().unwrap_or(json!(""))})
                    }
                    Some("input_image") => {
                        let m = image_body(Event::new("", Kind::Media), it);
                        let mut o = serde_json::Map::new();
                        o.insert("type".into(), json!("media"));
                        o.extend(m.body);
                        Value::Object(o)
                    }
                    _ => json!({"type": "native", "item": it}),
                })
                .collect();
            e.with("output", parts)
        }
        // Older rollouts: {"content": "...", "success": bool}.
        Value::Object(o) => {
            let e = e.with("output", o.get("content").cloned().unwrap_or_else(|| out.clone()));
            match o.get("success").and_then(Value::as_bool) {
                Some(ok) => e.with("is_error", !ok),
                None => e,
            }
        }
        _ => e.with("output", ""),
    }
}

fn map_response_item(ctx: &mut Ctx, p: &Value, _ts: Option<&str>) -> Vec<Event> {
    let ty = p.get("type").and_then(Value::as_str).unwrap_or(if p.get("role").is_some() { "message" } else { "" });
    let evs = match ty {
        "message" => map_message(ctx, p),
        "reasoning" => {
            let summary: Vec<Value> =
                p.get("summary").and_then(Value::as_array).map(|a| a.iter().filter_map(|x| x.get("text").cloned()).collect()).unwrap_or_default();
            let text = p.get("content").map(|c| content_text(c, &["reasoning_text", "text"])).filter(|t| !t.is_empty());
            let mut e = Event::new("", Kind::Reasoning).with("summary", summary.clone());
            if let Some(t) = &text {
                e = e.with("text", t.as_str());
            }
            if let Some(enc) = p.get("encrypted_content").and_then(Value::as_str) {
                e = e.with("sealed", json!({"vendor": "openai", "kind": "encrypted", "data": enc}));
            }
            for st in summary.iter().filter_map(Value::as_str) {
                ctx.primaries.push(("reasoning", st.to_string(), PENDING));
            }
            ctx.primaries.push(("reasoning_item", summary.iter().filter_map(Value::as_str).collect::<Vec<_>>().join("\n"), PENDING));
            if let Some(t) = text {
                ctx.primaries.push(("reasoning_raw", t, PENDING));
            }
            vec![e]
        }
        "function_call" => {
            let args = p.get("arguments").and_then(Value::as_str).unwrap_or("");
            let mut e = Event::new("", Kind::ToolCall)
                .with("call_id", p.get("call_id").cloned().unwrap_or(json!("")))
                .with("name", p.get("name").cloned().unwrap_or(json!("")));
            e = match serde_json::from_str::<Value>(args) {
                // Structured input when the arguments are spelled as serde writes
                // them (a decision that must not depend on serde_json's features).
                Ok(v) if compact_spelling(args) => e.with("input", v),
                _ => e.with("input_text", args),
            };
            let ns = p.get("namespace").and_then(Value::as_str);
            let name = p.get("name").and_then(Value::as_str).unwrap_or("");
            match (ns.and_then(mcp_server_of_namespace), ns) {
                // MCP tools are namespaced `mcp__<server>__` (`codex-mcp/src/mcp/mod.rs:80-86`,
                // `codex-mcp/src/tools.rs:225-235`); older Codex prefixed the name instead.
                (Some(server), _) => e = e.with("tool_type", "mcp").with("server", server),
                (None, Some(ns)) => e = e.with("tool_type", "function").with("namespace", ns),
                (None, None) => match mcp_server_of_name(name) {
                    Some(server) => e = e.with("tool_type", "mcp").with("server", server),
                    None => e = e.with("tool_type", "function"),
                },
            }
            if let Some(ns) = p.get("namespace").filter(|v| !v.is_null() && !v.is_string()) {
                e = e.with("namespace", ns.clone());
            }
            vec![e]
        }
        "custom_tool_call" => vec![
            Event::new("", Kind::ToolCall)
                .with("call_id", p.get("call_id").cloned().unwrap_or(json!("")))
                .with("name", p.get("name").cloned().unwrap_or(json!("")))
                .with("input_text", p.get("input").cloned().unwrap_or(json!("")))
                .with("tool_type", "custom"),
        ],
        "local_shell_call" => vec![
            Event::new("", Kind::ToolCall)
                .with("call_id", p.get("call_id").filter(|v| !v.is_null()).or(p.get("id")).cloned().unwrap_or(json!("")))
                .with("name", "local_shell")
                .with("input", p.get("action").cloned().unwrap_or(json!({})))
                .with("tool_type", "shell")
                .with("status", p.get("status").cloned().unwrap_or(Value::Null)),
        ],
        "web_search_call" => vec![
            Event::new("", Kind::ToolCall)
                .with("call_id", p.get("id").cloned().unwrap_or(json!("")))
                .with("name", "web_search")
                .with("input", p.get("action").cloned().unwrap_or(json!({})))
                .with("tool_type", "web_search")
                .with("status", p.get("status").cloned().unwrap_or(Value::Null)),
        ],
        "tool_search_call" => vec![
            Event::new("", Kind::ToolCall)
                .with("call_id", p.get("call_id").filter(|v| !v.is_null()).or(p.get("id")).cloned().unwrap_or(json!("")))
                .with("name", "tool_search")
                .with("input", p.get("arguments").cloned().unwrap_or(json!({})))
                .with("tool_type", "tool_search"),
        ],
        "function_call_output" | "custom_tool_call_output" => {
            let e = Event::new("", Kind::ToolResult).with("call_id", p.get("call_id").cloned().unwrap_or(json!("")));
            vec![output_body(e, p.get("output").unwrap_or(&Value::Null))]
        }
        "tool_search_output" => vec![
            Event::new("", Kind::ToolResult)
                .with("call_id", p.get("call_id").cloned().unwrap_or(json!("")))
                .with("output", serde_json::to_string(p.get("tools").unwrap_or(&json!([]))).unwrap_or_default())
                .with("status", p.get("status").cloned().unwrap_or(Value::Null)),
        ],
        "image_generation_call" => match p.get("result").and_then(Value::as_str).filter(|r| !r.is_empty()) {
            Some(r) => vec![
                Event::new("", Kind::Media)
                    .with("role", "assistant")
                    .with("media_type", "image/png")
                    .with("data", r)
                    .with("revised_prompt", p.get("revised_prompt").cloned().unwrap_or(Value::Null)),
            ],
            None => vec![],
        },
        "compaction" | "compaction_summary" | "context_compaction" => {
            let mut e = Event::new("", Kind::Compaction).with("phase", "summary");
            if let Some(enc) = p.get("encrypted_content").and_then(Value::as_str) {
                e = e.with("sealed", json!({"vendor": "openai", "kind": "encrypted", "data": enc}));
            }
            vec![e]
        }
        "agent_message" => {
            let author = s(p, "author").unwrap_or_default();
            let recipient = s(p, "recipient").unwrap_or_default();
            let text = content_text(p.get("content").unwrap_or(&Value::Null), &["input_text"]);
            let out = ctx.self_path.as_deref().is_some_and(|me| me == author);
            let mut e = Event::new("", Kind::PeerMessage)
                .with("direction", if out { "out" } else { "in" })
                .with("peer", if out { recipient } else { author })
                .with("text", text);
            let sealed: Vec<Value> =
                p.get("content").and_then(Value::as_array).map(|a| a.iter().filter_map(|c| c.get("encrypted_content").cloned()).collect()).unwrap_or_default();
            if let Some(d) = sealed.first() {
                e = e.with("sealed", json!({"vendor": "openai", "kind": "encrypted", "data": d}));
            }
            vec![e]
        }
        "additional_tools" => {
            vec![Event::new("", Kind::Settings).with("tools", p.get("tools").cloned().unwrap_or(json!([])))]
        }
        _ => vec![],
    };
    let output = match ty {
        "message" => p.get("role").and_then(Value::as_str) == Some("assistant"),
        "reasoning" | "function_call" | "custom_tool_call" | "local_shell_call" | "web_search_call" | "tool_search_call" | "image_generation_call" => true,
        _ => false,
    };
    let mut evs = evs;
    // On a model output item, an id Codex would not send to the Responses API
    // (one without the `<prefix>_` form, `protocol/src/response_item_id.rs:36-39`,
    // stripped by `core/src/client.rs:1098-1103`) is not an API item id but the
    // id of the message the item was recorded from: its group (SPEC §3.1.4).
    if let Some(id) = p.get("id").and_then(Value::as_str).filter(|i| output && !i.is_empty() && !is_api_item_id(i)) {
        for e in &mut evs {
            if e.group.is_none() {
                e.group = Some(id.to_string());
            }
        }
    }
    // SPEC §3.1.4 and §9.3: a response's output items share its group, which
    // must be known as each is read. Codex names the response only in the
    // `token_usage_record` written after its items (from 0.153), so the
    // response is named by its first output item instead: that item's id, or
    // its event id. Parts of one message already share theirs.
    let parts = evs.len() > 1 && evs.iter().all(|e| matches!(e.kind, Kind::Text | Kind::Media));
    if output && !parts && records_responses(ctx) && evs.iter().any(|e| e.group.is_none()) {
        let first = evs.iter().position(|e| e.group.is_none()).unwrap_or(0);
        let own = p.get("id").and_then(Value::as_str).filter(|i| !i.is_empty()).map(str::to_string);
        let id = if evs.len() > 1 { format!("{}#{first}", ctx.anchor) } else { ctx.anchor.clone() };
        let key = ctx.run.get_or_insert_with(|| own.unwrap_or(id)).clone();
        for e in &mut evs {
            if e.group.is_none() {
                e.group = Some(key.clone());
            }
        }
    } else if ty == "message" && !output {
        // A user or developer message starts a new request.
        ctx.run = None;
        ctx.closed_run = None;
    } else if ty.ends_with("_output") {
        // The response's items are all read; its record may still follow.
        if let Some(r) = ctx.run.take() {
            ctx.closed_run = Some(r);
        }
    }
    if !ty.is_empty() && !evs.is_empty() {
        ctx.response_records.push((PENDING, strip_volatile(p)));
    }
    evs
}

/// Does this rollout write a `token_usage_record` per response (Codex 0.153
/// and later, as `session_meta` says before any item)? Only then are output
/// items grouped by response.
fn records_responses(ctx: &Ctx) -> bool {
    ctx.variant != V_V0 && ctx.variant != V_TS && ctx.version.as_deref().is_some_and(|v| super::export::version_at_least(v, &[0, 153]))
}

/// `mcp__<server>__` → `<server>`.
pub(super) fn mcp_server_of_namespace(ns: &str) -> Option<String> {
    ns.strip_prefix("mcp__")?.strip_suffix("__").filter(|s| !s.is_empty()).map(str::to_string)
}

/// A pre-namespace MCP tool name, `mcp__<server>__<tool>` → `<server>`.
fn mcp_server_of_name(name: &str) -> Option<String> {
    let (server, tool) = name.strip_prefix("mcp__")?.split_once("__")?;
    (!server.is_empty() && !tool.is_empty()).then(|| server.to_string())
}

/// Codex's `ResponseItemId::is_prefixed`: `<prefix>_<suffix>`, both non-empty.
pub(super) fn is_api_item_id(id: &str) -> bool {
    id.split_once('_').is_some_and(|(p, s)| !p.is_empty() && !s.is_empty())
}

/// Hold a native record as `text`, the source's exact spelling of it (SPEC §4),
/// when there is one a ledger line can hold; otherwise as `record`'s serialisation.
fn set_record_exact(n: &mut Native, record: Option<Value>, text: Option<&str>) {
    match text.map(|t| n.clone().record_raw(t)) {
        Some(Ok(held)) => *n = held,
        _ => n.set_record(record),
    }
}

/// Whether `text`, which parses as JSON, is spelled the way serde_json writes
/// the value it holds with source key order and number spelling kept: no
/// whitespace between tokens, every string escaped as serde escapes it, no
/// repeated key. This is what `serde_json::to_string(&v) == text` would answer
/// with serde_json's `preserve_order` and `arbitrary_precision` on; it gives
/// that answer whatever features serde_json is built with.
pub(crate) fn compact_spelling(text: &str) -> bool {
    let b = text.as_bytes();
    let mut i = 0;
    // One set of keys per open object; `None` for an array.
    let mut open: Vec<Option<std::collections::HashSet<String>>> = Vec::new();
    let mut expect_key = false;
    while i < b.len() {
        match b[i] {
            b' ' | b'\t' | b'\n' | b'\r' => return false,
            b'"' => {
                let start = i;
                i += 1;
                while i < b.len() && b[i] != b'"' {
                    if b[i] == b'\\' {
                        i += 1;
                    }
                    i += 1;
                }
                if i >= b.len() {
                    return false;
                }
                let tok = &text[start..=i];
                let Ok(decoded) = serde_json::from_str::<String>(tok) else { return false };
                if serde_json::to_string(&decoded).ok().as_deref() != Some(tok) {
                    return false;
                }
                if expect_key {
                    if let Some(Some(keys)) = open.last_mut()
                        && !keys.insert(decoded)
                    {
                        return false;
                    }
                    expect_key = false;
                }
                i += 1;
                continue;
            }
            b'{' => {
                open.push(Some(Default::default()));
                expect_key = true;
            }
            b'[' => open.push(None),
            b'}' | b']' => {
                open.pop();
            }
            b',' => expect_key = matches!(open.last(), Some(Some(_))),
            _ => {}
        }
        i += 1;
    }
    true
}

/// Two `TokenUsage` records with the same counts.
fn same_usage(a: &Value, b: &Value) -> bool {
    ["input_tokens", "cached_input_tokens", "cache_write_input_tokens", "output_tokens", "reasoning_output_tokens", "total_tokens"]
        .iter()
        .all(|k| a.get(*k).and_then(Value::as_i64).unwrap_or(0) == b.get(*k).and_then(Value::as_i64).unwrap_or(0))
}

/// A response item as compared against `compacted.replacement_history`.
fn strip_volatile(p: &Value) -> Value {
    let mut p = p.clone();
    if let Value::Object(m) = &mut p {
        m.remove_ordered("id");
        m.remove_ordered("status");
        m.remove_ordered("internal_chat_message_metadata_passthrough");
    }
    p
}

/// A Codex `TokenUsage` as neutral usage (SPEC §3.1.1). Codex's `input_tokens`
/// counts cache reads and writes too (`codex-api/src/sse/responses.rs:128-141`:
/// `input_tokens` is the Responses API total, `cached_tokens` and
/// `cache_write_tokens` its details); neutral `input` excludes both.
/// Codex writes every count, so a zero cannot say "not reported" (OpenAI does
/// not report cache writes at all, and reasoning only for reasoning models): a
/// zero cache write or reasoning count is read as not stated. A zero cache
/// read is kept, as it always has been.
fn usage_body(e: Event, u: &Value) -> Event {
    let n = |k: &str| u.get(k).and_then(Value::as_i64);
    let cached = n("cached_input_tokens").unwrap_or(0).max(0);
    let write = n("cache_write_input_tokens").unwrap_or(0).max(0);
    let input = n("input_tokens").map(|i| json!((i - cached - write).max(0))).unwrap_or(Value::Null);
    e.with("input", input)
        .with("output", u.get("output_tokens").cloned().unwrap_or(Value::Null))
        .with("cache_read", u.get("cached_input_tokens").cloned().unwrap_or(Value::Null))
        .with("cache_write", u.get("cache_write_input_tokens").filter(|v| v.as_i64() != Some(0)).cloned().unwrap_or(Value::Null))
        .with("reasoning", u.get("reasoning_output_tokens").filter(|v| v.as_i64() != Some(0)).cloned().unwrap_or(Value::Null))
}

/// Does a `TokenUsage` report no response of its own: no input, output or
/// reasoning tokens, at most a context estimate in `total_tokens` (Codex's
/// `recompute_token_usage`, `core/src/session/mod.rs:5150-5170`, and its
/// external-session importer's seed, `external-agent-migration/src/sessions/export.rs:195-207`)?
fn is_estimate_only(u: &Value) -> bool {
    ["input_tokens", "cached_input_tokens", "cache_write_input_tokens", "output_tokens", "reasoning_output_tokens"]
        .iter()
        .all(|k| u.get(*k).and_then(Value::as_i64).unwrap_or(0) == 0)
}

/// `token_usage_record` (`protocol.rs:2389-2400`), written once per completed
/// response (`core/src/session/mod.rs:5049-5072`) after that response's output
/// items. Its usage takes the group those items were given as they were read
/// (SPEC §9.3: they are not re-grouped); with no items, `response_id`.
fn map_usage_record(ctx: &mut Ctx, p: &Value) -> Vec<Event> {
    let response = ctx.run.take().or(ctx.closed_run.take()).or_else(|| s(p, "response_id").filter(|r| !r.is_empty()));
    let Some(u) = p.get("usage") else {
        return vec![];
    };
    ctx.pending_record = Some(u.clone());
    let e = usage_body(Event::new("", Kind::Usage), u).with("cumulative", false).with("model", ctx.model.clone()).group(response).turn(s(p, "turn_id"));
    vec![e]
}

fn map_event_msg(ctx: &mut Ctx, p: &Value, ts: Option<&str>) -> Vec<Event> {
    let ty = p.get("type").and_then(Value::as_str).unwrap_or("");
    match ty {
        "task_started" | "turn_started" => {
            ctx.run = None;
            ctx.closed_run = None;
            ctx.turn = s(p, "turn_id").or(ctx.turn.take());
            vec![Event::new("", Kind::Turn).with("phase", "start")]
        }
        "task_complete" | "turn_complete" => {
            let turn = s(p, "turn_id").or_else(|| ctx.turn.clone());
            let err = p.get("error").filter(|v| !v.is_null());
            let mut e = Event::new("", Kind::Turn)
                .with("phase", "end")
                .with("outcome", if err.is_some() { "error" } else { "ok" })
                .with("duration_ms", p.get("duration_ms").cloned().unwrap_or(Value::Null))
                .turn(turn);
            if let Some(m) = err.and_then(|e| e.get("message")) {
                e = e.with("reason", m.clone());
            }
            ctx.turn = None;
            vec![e]
        }
        "turn_aborted" => {
            let turn = s(p, "turn_id").or_else(|| ctx.turn.clone());
            let reason = s(p, "reason").unwrap_or_default();
            let e = Event::new("", Kind::Turn)
                .with("phase", "end")
                .with("outcome", if reason == "interrupted" { "interrupted" } else { "aborted" })
                .with("reason", reason)
                .with("duration_ms", p.get("duration_ms").cloned().unwrap_or(Value::Null))
                .turn(turn);
            ctx.turn = None;
            vec![e]
        }
        "token_count" => {
            // Early 2025: the TokenUsage itself was the payload.
            if p.get("input_tokens").is_some() {
                return vec![usage_body(Event::new("", Kind::Usage), p).with("cumulative", false)];
            }
            let Some(info) = p.get("info").filter(|v| !v.is_null()) else {
                return vec![];
            };
            let Some(total) = info.get("total_token_usage") else {
                return vec![];
            };
            let last = info.get("last_token_usage").unwrap_or(total);
            let previous = ctx.last_total.replace(total.clone());
            // The UI's copy of the `token_usage_record` just written for this response.
            if ctx.pending_record.take().is_some_and(|r| same_usage(&r, last)) {
                return vec![];
            }
            let cw = info.get("model_context_window").cloned().unwrap_or(Value::Null);
            // Totals with no response at all (every last count zero): a running
            // total restated on its own (SPEC §3.1.1: cumulative).
            if is_estimate_only(last) && last.get("total_tokens").and_then(Value::as_i64).unwrap_or(0) == 0 && !is_estimate_only(total) {
                let e = usage_body(Event::new("", Kind::Usage), total).with("cumulative", true).with("model", ctx.model.clone()).with("context_window", cw);
                return vec![e];
            }
            // Codex re-sends the same totals with every rate-limit refresh and
            // after re-estimating its context; only a change is a new response's
            // usage (`protocol.rs:2403-2430`).
            if previous.as_ref() == Some(total) {
                return vec![];
            }
            // A context estimate with no response (an importer's seed).
            if is_estimate_only(last) {
                return vec![];
            }
            let e = usage_body(Event::new("", Kind::Usage), last).with("cumulative", false).with("model", ctx.model.clone()).with("context_window", cw);
            vec![e]
        }
        "user_message" => {
            let text = s(p, "message").unwrap_or_default();
            ctx.dups.push(("user", text.clone(), PENDING));
            vec![Event::new("", Kind::Text).with("role", "user").with("text", text)]
        }
        "agent_message" => {
            let text = s(p, "message").unwrap_or_default();
            ctx.dups.push(("assistant", text.clone(), PENDING));
            vec![Event::new("", Kind::Text).with("role", "assistant").with("text", text)]
        }
        "agent_reasoning" => {
            let text = s(p, "text").unwrap_or_default();
            ctx.dups.push(("reasoning", text.clone(), PENDING));
            vec![Event::new("", Kind::Reasoning).with("summary", json!([text]))]
        }
        "agent_reasoning_raw_content" => {
            let text = s(p, "text").unwrap_or_default();
            ctx.dups.push(("reasoning_raw", text.clone(), PENDING));
            vec![Event::new("", Kind::Reasoning).with("text", text)]
        }
        "item_completed" => map_item_completed(ctx, p),
        "context_compacted" => vec![Event::new("", Kind::Compaction).with("phase", "boundary")],
        "error" | "stream_error" => vec![
            Event::new("", Kind::Error)
                .with("kind", ty)
                .with("message", p.get("message").cloned().unwrap_or(json!("")))
                .with("retryable", ty == "stream_error"),
        ],
        "collab_agent_spawn_end" => {
            let child = s(p, "new_thread_id");
            vec![
                Event::new("", Kind::Subagent)
                    .with("phase", "start")
                    .with("agent", child)
                    .with("call_id", p.get("call_id").cloned().unwrap_or(Value::Null))
                    .with("agent_type", p.get("new_agent_role").cloned().unwrap_or(Value::Null))
                    .with("description", p.get("prompt").cloned().unwrap_or(Value::Null)),
            ]
        }
        "sub_agent_activity" => {
            let child = s(p, "agent_thread_id");
            let kind = s(p, "kind").unwrap_or_default();
            vec![
                Event::new("", Kind::Subagent)
                    .with("phase", if kind == "completed" { "end" } else { "start" })
                    .with("agent", child)
                    .with("agent_type", p.get("agent_path").cloned().unwrap_or(Value::Null)),
            ]
        }
        "patch_apply_end" => file_changes(p.get("changes")),
        _ => {
            let _ = ts;
            vec![]
        }
    }
}

fn map_item_completed(ctx: &mut Ctx, p: &Value) -> Vec<Event> {
    let item = p.get("item").cloned().unwrap_or(Value::Null);
    let turn = s(p, "turn_id");
    let ty = item.get("type").and_then(Value::as_str).unwrap_or("");
    let joined = |k: &str| -> String {
        item.get(k)
            .and_then(Value::as_array)
            .map(|a| a.iter().filter_map(|c| c.get("text").and_then(Value::as_str).or(c.as_str())).collect::<Vec<_>>().join("\n"))
            .unwrap_or_default()
    };
    let evs = match ty {
        "UserMessage" => {
            let text = joined("content");
            ctx.dups.push(("user", text.clone(), PENDING));
            vec![Event::new("", Kind::Text).with("role", "user").with("text", text)]
        }
        "AgentMessage" => {
            let text = joined("content");
            ctx.dups.push(("assistant", text.clone(), PENDING));
            vec![Event::new("", Kind::Text).with("role", "assistant").with("text", text)]
        }
        "Reasoning" => {
            let summary = joined("summary_text");
            ctx.dups.push(("reasoning_item", summary.clone(), PENDING));
            let list = item.get("summary_text").cloned().unwrap_or(json!([]));
            vec![Event::new("", Kind::Reasoning).with("summary", list)]
        }
        "FileChange" => file_changes(item.get("changes")),
        "ContextCompaction" => vec![Event::new("", Kind::Compaction).with("phase", "boundary")],
        "SubAgentActivity" => {
            let child = s(&item, "agent_thread_id");
            let kind = s(&item, "kind").unwrap_or_default();
            vec![
                Event::new("", Kind::Subagent)
                    .with("phase", if kind == "completed" { "end" } else { "start" })
                    .with("agent", child)
                    .with("agent_type", item.get("agent_path").cloned().unwrap_or(Value::Null)),
            ]
        }
        _ => vec![],
    };
    evs.into_iter().map(|e| e.turn(turn.clone())).collect()
}

/// `patch_apply_end.changes` / `FileChange.changes`: path → add|delete|update,
/// internally tagged today (`protocol.rs:4340-4354`), externally tagged before.
fn file_changes(changes: Option<&Value>) -> Vec<Event> {
    let Some(map) = changes.and_then(Value::as_object) else {
        return vec![];
    };
    let mut out = Vec::new();
    for (path, ch) in map {
        let (kind, body) = match ch.get("type").and_then(Value::as_str) {
            Some(t) => (t.to_string(), ch.clone()),
            None => match ch.as_object().and_then(|o| o.iter().next()) {
                Some((k, v)) => (k.clone(), v.clone()),
                None => continue,
            },
        };
        let e = Event::new("", Kind::FileChange).with("path", path.as_str());
        out.push(match kind.as_str() {
            "add" => e.with("op", "create").with("content", body.get("content").cloned().unwrap_or(Value::Null)),
            "delete" => e.with("op", "delete").with("content", body.get("content").cloned().unwrap_or(Value::Null)),
            _ => e
                .with("op", "edit")
                .with("diff", body.get("unified_diff").cloned().unwrap_or(Value::Null))
                .with("move_path", body.get("move_path").cloned().unwrap_or(Value::Null)),
        });
    }
    out
}

fn map_compacted(ctx: &mut Ctx, p: &Value) -> Vec<Event> {
    ctx.run = None;
    ctx.closed_run = None;
    let mut e = Event::new("", Kind::Compaction)
        .with("phase", "summary")
        .with("summary", p.get("message").cloned().unwrap_or(json!("")))
        .group(s(p, "compaction_response_id"));
    if let Some(rh) = p.get("replacement_history").and_then(Value::as_array) {
        // Replacement items that repeat an earlier item verbatim are "preserved".
        let mut preserved = Vec::new();
        let mut seen = vec![false; ctx.response_records.len()];
        for item in rh {
            let want = strip_volatile(item);
            if let Some(i) = ctx.response_records.iter().enumerate().position(|(i, (_, r))| !seen[i] && *r == want) {
                seen[i] = true;
                let idx = ctx.response_records[i].0;
                if idx < ctx.events.len() {
                    preserved.push(json!(ctx.events[idx].id));
                }
            }
        }
        e = e.with("preserved", preserved).with("replacement_items", rh.len());
    }
    if let Some(u) = p.pointer("/latest_token_usage_record/thread_token_usage/total_tokens") {
        e = e.with("tokens_before", u.clone());
    }
    vec![e]
}

fn map_inter_agent(ctx: &mut Ctx, p: &Value) -> Vec<Event> {
    let author = s(p, "author").unwrap_or_default();
    let recipient = s(p, "recipient").unwrap_or_default();
    let out = ctx.self_path.as_deref().is_some_and(|me| me == author);
    let mut e = Event::new("", Kind::PeerMessage)
        .with("direction", if out { "out" } else { "in" })
        .with("peer", if out { recipient } else { author })
        .with("text", p.get("content").cloned().unwrap_or(json!("")));
    if let Some(enc) = p.get("encrypted_content").and_then(Value::as_str) {
        e = e.with("sealed", json!({"vendor": "openai", "kind": "encrypted", "data": enc}));
    }
    if p.get("trigger_turn").and_then(Value::as_bool) == Some(true) {
        e = e.with("trigger_turn", true);
    }
    vec![e]
}
