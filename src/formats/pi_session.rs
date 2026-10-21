//! The pi-agent session JSONL format, shared by pi (`badlogic/pi-mono`, now
//! `earendil-works/pi`) and its fork oh-my-pi (`omp`).
//!
//! A session file is one `type: "session"` header followed by entries that
//! form a **tree** through `id` / `parentId`. pi's active position (the leaf)
//! is never persisted: on load it is the last entry in the file (omp: the last
//! entry, unless it is a late `model_usage` that names its `activeLeafId`).
//!
//! **Branches.** The active path (root → leaf) becomes the agent's ledger.
//! Every other branch becomes its own ledger with
//! `agent.parent = {agent, event: <fork-point event>, relation: fork}`
//! (SPEC §1.1: a forked branch is its own file). Inside a branch the deepest
//! leaf (latest in file order) is that branch's own active path, and branches
//! off it nest the same way, so every ledger is linear and `after` is never
//! needed. The physical line of every record is kept in
//! `agent.source.extra.lines` so the exporter can interleave the branch
//! ledgers back into one file byte for byte.

use crate::carry;
use crate::formats::atif::jsonfmt;
use crate::formats::{Detection, ExportOptions, Exported, ImportOptions, Imported, Losses, Output};
use crate::ledger::{Ledger, LedgerSet};
use crate::model::{Agent, Audience, Event, Kind, Link, Native, Obj, Parent, Relation, Source};
use crate::util;
use anyhow::{Context, Result, bail};
use serde_json::{Map, Value, json};
use std::collections::{BTreeMap, HashMap, HashSet};
use std::path::Path;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum Dialect {
    Pi,
    Omp,
    /// Letta Code's local backend (`lc-local-backend/conversations/*/messages.jsonl`):
    /// pi session v3 entries whose messages carry an `id` and are re-appended
    /// as snapshots when they change (the latest snapshot wins).
    LettaLocal,
}

impl Dialect {
    pub fn format(self) -> &'static str {
        match self {
            Dialect::Pi => "pi",
            Dialect::Omp => "omp",
            Dialect::LettaLocal => "letta-code",
        }
    }
}

pub const BRANCH_SUMMARY_PREFIX: &str = "The following is a summary of a branch that this conversation came back from:\n\n<summary>\n";
pub const BRANCH_SUMMARY_SUFFIX: &str = "</summary>";
/// omp's fixed-width first line (`session-title-slot.ts`): exactly 256 UTF-8 bytes with its newline.
pub const TITLE_SLOT_BYTES: usize = 256;

// ---------------------------------------------------------------------------
// Physical lines
// ---------------------------------------------------------------------------

#[derive(Debug)]
enum Line {
    Blank { no: usize, text: String },
    Malformed { no: usize, off: usize, text: String },
    Record(Rec),
}

#[derive(Debug, Clone)]
struct Rec {
    no: usize,
    off: usize,
    value: Value,
    /// The line exactly as written.
    text: String,
    /// `value` re-serialises to `text`.
    exact: bool,
}

impl Rec {
    fn typ(&self) -> &str {
        self.value.get("type").and_then(Value::as_str).unwrap_or("")
    }
    fn id(&self) -> Option<&str> {
        self.value.get("id").and_then(Value::as_str).filter(|s| !s.is_empty())
    }
    fn parent_id(&self) -> Option<&str> {
        self.value.get("parentId").and_then(Value::as_str)
    }
}

/// Split into lines, remembering whether the text ended with a newline.
fn physical_lines(text: &str) -> (Vec<Line>, bool) {
    let trailing = text.is_empty() || text.ends_with('\n');
    let mut pieces: Vec<&str> = text.split('\n').collect();
    if trailing {
        pieces.pop();
    }
    let mut off = 0usize;
    let mut out = Vec::new();
    for (no, p) in pieces.into_iter().enumerate() {
        let start = off;
        off += p.len() + 1;
        if p.trim().is_empty() {
            out.push(Line::Blank { no, text: p.to_string() });
            continue;
        }
        match serde_json::from_str::<Value>(p) {
            Ok(v) if v.is_object() => {
                let exact = serde_json::to_string(&v).map(|s| s == p).unwrap_or(false);
                out.push(Line::Record(Rec { no, off: start, value: v, text: p.to_string(), exact }));
            }
            _ => out.push(Line::Malformed { no, off: start, text: p.to_string() }),
        }
    }
    (out, trailing)
}

fn is_title_slot(v: &Value) -> bool {
    v.get("type").and_then(Value::as_str) == Some("title") && v.get("v").and_then(Value::as_u64) == Some(1) && v.get("pad").is_some()
}

// ---------------------------------------------------------------------------
// Detection
// ---------------------------------------------------------------------------

/// Score how much `input` looks like a session of dialect `d`.
pub(crate) fn detect(d: Dialect, input: &Path) -> Option<Detection> {
    let file = if input.is_dir() { session_files(input).into_iter().next()? } else { input.to_path_buf() };
    let head;
    {
        use std::io::Read;
        let mut f = std::fs::File::open(&file).ok()?;
        let mut buf = vec![0u8; 256 * 1024];
        let n = f.read(&mut buf).ok()?;
        buf.truncate(n);
        head = String::from_utf8_lossy(&buf).to_string();
    }
    let mut saw_header = None;
    let mut slot = false;
    let (mut omp_hits, mut pi_hits) = (0, 0);
    let mut version = None;
    for (i, line) in head.split('\n').take(400).enumerate() {
        let Ok(v) = serde_json::from_str::<Value>(line) else { continue };
        let typ = v.get("type").and_then(Value::as_str).unwrap_or("");
        if i == 0 && is_title_slot(&v) {
            slot = true;
            continue;
        }
        if saw_header.is_none() {
            if typ == "session" && v.get("id").is_some_and(Value::is_string) {
                saw_header = Some(v.clone());
                for k in ["title", "titleSource", "additionalDirectories", "previousSessionFiles", "providerPromptCacheKey"] {
                    if v.get(k).is_some() {
                        omp_hits += 1;
                    }
                }
                continue;
            }
            return None;
        }
        if let Some(rv) = v.get("runtime").and_then(|r| r.get("version")).and_then(Value::as_str) {
            omp_hits += 2;
            version.get_or_insert_with(|| rv.to_string());
        }
        match typ {
            "title_change"
            | "session_init"
            | "mode_change"
            | "ttsr_injection"
            | "service_tier_change"
            | "model_usage"
            | "credential_pin"
            | "reset_boundary" => omp_hits += 2,
            "context_edit" | "usage" => pi_hits += 2,
            "model_change" => {
                if v.get("modelId").is_some() {
                    pi_hits += 1
                } else if v.get("model").is_some() {
                    omp_hits += 2
                }
            }
            "message" => match util::get_str(&v, &["message", "role"]) {
                Some("system") => pi_hits += 1,
                Some("developer" | "fileMention" | "pythonExecution") => omp_hits += 2,
                _ => {}
            },
            _ => {}
        }
    }
    saw_header.as_ref()?;
    let path = file.to_string_lossy();
    let in_omp = path.contains("/.omp/");
    let in_pi = path.contains("/.pi/");
    let omp_score = if slot || omp_hits >= 2 || in_omp {
        90
    } else if pi_hits > 0 || in_pi {
        40
    } else {
        55
    };
    let pi_score = if slot || omp_hits >= 2 || in_omp {
        40
    } else if pi_hits > 0 || in_pi {
        90
    } else {
        60
    };
    let sv = saw_header.as_ref().and_then(|h| h.get("version")).and_then(Value::as_u64).unwrap_or(1);
    let variant = match d {
        Dialect::Pi => pi_variant(sv),
        Dialect::Omp => {
            if slot {
                "title-slot"
            } else {
                "header-first"
            }
        }
        Dialect::LettaLocal => return None,
    };
    Some(Detection {
        variant,
        confidence: match d {
            Dialect::Pi => pi_score,
            _ => omp_score,
        },
        version: if d == Dialect::Omp { version } else { None },
    })
}

fn pi_variant(session_version: u64) -> &'static str {
    match session_version {
        0 | 1 => "v1",
        2 => "v2",
        _ => "v3",
    }
}

/// `*.jsonl` files under `dir`, sorted, skipping omp backups and advisor transcripts.
fn session_files(dir: &Path) -> Vec<std::path::PathBuf> {
    let mut v: Vec<_> = walkdir::WalkDir::new(dir)
        .sort_by_file_name()
        .into_iter()
        .filter_map(|e| e.ok())
        .filter(|e| e.file_type().is_file())
        .map(|e| e.path().to_path_buf())
        .filter(|p| {
            let n = p.file_name().map(|n| n.to_string_lossy().to_string()).unwrap_or_default();
            n.ends_with(".jsonl") && !n.starts_with('.') && !n.starts_with("__advisor")
        })
        .collect();
    // Shallow paths first, so a session is imported before its subagent files.
    v.sort_by_key(|p| (p.components().count(), p.clone()));
    v
}

// ---------------------------------------------------------------------------
// Import
// ---------------------------------------------------------------------------

pub(crate) fn import(d: Dialect, input: &Path, opts: &ImportOptions) -> Result<Imported> {
    let mut set = LedgerSet::new();
    let mut losses = Losses::default();
    let mut variant: Option<String> = None;
    let (root, files): (std::path::PathBuf, Vec<std::path::PathBuf>) = if input.is_dir() {
        (input.to_path_buf(), session_files(input))
    } else {
        (input.parent().unwrap_or(Path::new(".")).to_path_buf(), vec![input.to_path_buf()])
    };
    if files.is_empty() {
        bail!("{}: no session .jsonl files", input.display());
    }
    // rel path -> (agent id of its main ledger, subagent stem -> spawning tool_call event id)
    type Spawns = HashMap<String, (String, String)>;
    let mut by_rel: HashMap<String, (String, Spawns)> = HashMap::new();
    for f in &files {
        let rel = f.strip_prefix(&root).unwrap_or(f).to_string_lossy().replace('\\', "/");
        let text = std::fs::read_to_string(f).with_context(|| format!("reading {}", f.display()))?;
        // omp keeps a session's subagents at `<stem>/<AgentId>.jsonl`.
        let mut parent = None;
        if d == Dialect::Omp
            && let Some((dir, name)) = rel.rsplit_once('/')
        {
            let owner = format!("{dir}.jsonl");
            if let Some((pid, spawns)) = by_rel.get(&owner) {
                let stem = name.trim_end_matches(".jsonl");
                parent = Some(match spawns.get(stem) {
                    Some((lid, ev)) => Parent { agent: lid.clone(), event: Some(ev.clone()), relation: Relation::Subagent },
                    None => Parent { agent: pid.clone(), event: None, relation: Relation::Subagent },
                });
            }
        }
        // Children of this file, for linking task results to their transcripts.
        let mut child_ids = HashMap::new();
        if d == Dialect::Omp {
            let dir = f.with_extension("");
            if dir.is_dir() {
                for c in std::fs::read_dir(&dir)?.filter_map(|e| e.ok()) {
                    let p = c.path();
                    if p.extension().is_some_and(|x| x == "jsonl")
                        && let Some(id) = first_header_id(&p)
                    {
                        child_ids.insert(p.file_stem().unwrap().to_string_lossy().to_string(), id);
                    }
                }
            }
        }
        let fi = import_text(d, &text, &rel, parent, &child_ids, opts, &mut losses).with_context(|| format!("{}: {rel}", d.format()))?;
        variant.get_or_insert(fi.variant.clone());
        by_rel.insert(rel.clone(), (fi.main_id.clone(), fi.spawns.clone()));
        for l in fi.ledgers {
            set.insert(l);
        }
    }
    Ok(Imported { set, losses, variant: variant.unwrap_or_else(|| "v3".into()) })
}

fn first_header_id(p: &Path) -> Option<String> {
    let text = std::fs::read_to_string(p).ok()?;
    for line in text.lines().take(3) {
        let v: Value = serde_json::from_str(line).ok()?;
        if v.get("type").and_then(Value::as_str) == Some("session") {
            return v.get("id").and_then(Value::as_str).map(String::from);
        }
    }
    None
}

struct FileImport {
    ledgers: Vec<Ledger>,
    main_id: String,
    variant: String,
    /// omp: subagent file stem → (ledger, `task` tool_call event) that spawned it.
    spawns: HashMap<String, (String, String)>,
}

/// Mutable state while mapping one file's records to events.
struct Ctx<'a> {
    d: Dialect,
    variant: String,
    losses: &'a mut Losses,
    /// tool call id → (name, arguments, event id)
    calls: HashMap<String, (String, Value, String)>,
    /// entry id → the event ids made from it.
    entry_events: HashMap<String, Vec<String>>,
    parent_of: HashMap<String, Option<String>>,
    /// v1: entry index (header = 0) → the event ids made from it.
    v1_index_events: Vec<Vec<String>>,
    child_ids: &'a HashMap<String, String>,
    spawns: HashMap<String, String>,
    /// Letta local backend: physical lines whose message a later snapshot replaces.
    superseded: HashSet<usize>,
}

fn import_text(
    d: Dialect,
    text: &str,
    rel: &str,
    parent: Option<Parent>,
    child_ids: &HashMap<String, String>,
    opts: &ImportOptions,
    losses: &mut Losses,
) -> Result<FileImport> {
    let (lines, trailing) = physical_lines(text);
    let recs: Vec<&Rec> = lines.iter().filter_map(|l| if let Line::Record(r) = l { Some(r) } else { None }).collect();
    let header_pos = recs.iter().position(|r| r.typ() == "session" && r.id().is_some());
    let Some(hp) = header_pos else { bail!("no session header") };
    let header = recs[hp];
    let slot = hp > 0 && is_title_slot(&recs[0].value);
    let session_version = header.value.get("version").and_then(Value::as_u64).unwrap_or(1);
    let variant = match d {
        Dialect::Pi => pi_variant(session_version).to_string(),
        Dialect::Omp => if slot { "title-slot" } else { "header-first" }.to_string(),
        Dialect::LettaLocal => "local-backend".to_string(),
    };
    let session_id = header.id().unwrap().to_string();
    let main_id = session_id.clone();

    // --- the tree -----------------------------------------------------------
    // Entries are the records after the header (and the title slot) with an id.
    let entries: Vec<&Rec> = recs.iter().skip(hp + 1).copied().collect();
    let mut by_id: HashMap<String, usize> = HashMap::new();
    let mut tree_member = vec![false; entries.len()];
    for (i, e) in entries.iter().enumerate() {
        if let Some(id) = e.id()
            && !by_id.contains_key(id)
        {
            by_id.insert(id.to_string(), i);
            tree_member[i] = true;
        }
    }
    let parent_idx = |i: usize| -> Option<usize> { entries[i].parent_id().and_then(|p| by_id.get(p).copied()) };
    let leaf = resolve_leaf(d, &entries, &tree_member, &by_id);
    let mut on_path = vec![false; entries.len()];
    let mut path_order = Vec::new();
    if let Some(mut cur) = leaf {
        let mut seen = HashSet::new();
        loop {
            if !seen.insert(cur) {
                break;
            }
            on_path[cur] = true;
            path_order.push(cur);
            match parent_idx(cur) {
                Some(p) => cur = p,
                None => break,
            }
        }
        path_order.reverse();
    }
    // Ledger assignment: 0 is the main ledger.
    let mut assign: Vec<usize> = vec![0; entries.len()];
    let mut children: HashMap<Option<usize>, Vec<usize>> = HashMap::new();
    for i in 0..entries.len() {
        if tree_member[i] && !on_path[i] {
            children.entry(parent_idx(i)).or_default().push(i);
        }
    }
    struct Branch {
        parent_ledger: usize,
        fork: Option<usize>,
        first: usize,
    }
    let mut branches: Vec<Branch> = Vec::new();
    // Breadth over (parent ledger, fork point) pairs.
    let mut todo: Vec<(usize, usize, Option<usize>)> = Vec::new(); // (root idx, parent ledger, fork idx)
    for &p in &path_order {
        for &c in children.get(&Some(p)).map(|v| v.as_slice()).unwrap_or(&[]) {
            todo.push((c, 0, Some(p)));
        }
    }
    for &c in children.get(&None).map(|v| v.as_slice()).unwrap_or(&[]) {
        todo.push((c, 0, None));
    }
    let mut assigned = on_path.clone();
    while !todo.is_empty() {
        todo.sort_by_key(|t| std::cmp::Reverse(t.0));
        let (r, pl, fork) = todo.pop().unwrap();
        if assigned[r] {
            continue;
        }
        // Deepest leaf of r's subtree, by file order.
        let mut best = r;
        let mut stack = vec![r];
        let mut seen = HashSet::new();
        while let Some(x) = stack.pop() {
            if !seen.insert(x) {
                continue;
            }
            best = best.max(x);
            for &c in children.get(&Some(x)).map(|v| v.as_slice()).unwrap_or(&[]) {
                if !assigned[c] {
                    stack.push(c);
                }
            }
        }
        let mut path = vec![best];
        let mut cur = best;
        while cur != r {
            match parent_idx(cur) {
                Some(p) if !path.contains(&p) => {
                    path.push(p);
                    cur = p;
                }
                _ => break,
            }
        }
        path.reverse();
        let li = branches.len() + 1;
        branches.push(Branch { parent_ledger: pl, fork, first: r });
        for &p in &path {
            assign[p] = li;
            assigned[p] = true;
        }
        for &p in &path {
            for &c in children.get(&Some(p)).map(|v| v.as_slice()).unwrap_or(&[]) {
                if !assigned[c] {
                    todo.push((c, li, Some(p)));
                }
            }
        }
    }
    // Anything left (cycles, unreachable) stays in the main ledger by file order.

    // --- events -------------------------------------------------------------
    let mut ctx = Ctx {
        d,
        variant: variant.clone(),
        losses,
        calls: HashMap::new(),
        entry_events: HashMap::new(),
        parent_of: HashMap::new(),
        v1_index_events: Vec::new(),
        child_ids,
        spawns: HashMap::new(),
        superseded: HashSet::new(),
    };
    if d == Dialect::LettaLocal {
        let mut last: HashMap<&str, usize> = HashMap::new();
        for e in &entries {
            if let Some(mid) = util::get_str(&e.value, &["message", "id"])
                && let Some(prev) = last.insert(mid, e.no)
            {
                ctx.superseded.insert(prev);
            }
        }
    }
    for (i, e) in entries.iter().enumerate() {
        if let Some(id) = e.id() {
            ctx.parent_of.insert(id.to_string(), e.parent_id().map(String::from));
        }
        let _ = i;
    }
    let ledger_ids: Vec<String> =
        std::iter::once(main_id.clone()).chain(branches.iter().map(|b| format!("{main_id}@{}", entries[b.first].id().unwrap_or("x")))).collect();
    let mut ledgers: Vec<Ledger> = Vec::new();
    let mut agent_override: Vec<Option<Agent>> = vec![None; ledger_ids.len()];
    let mut ledger_lines: Vec<Vec<usize>> = vec![Vec::new(); ledger_ids.len()];
    let runtime_version = entries.iter().find_map(|e| util::get_str(&e.value, &["runtime", "version"])).map(String::from);
    let created = header.value.get("timestamp").and_then(Value::as_str).map(String::from);
    for (li, lid) in ledger_ids.iter().enumerate() {
        let mut extra = Obj::new();
        extra.insert("file".into(), json!(rel));
        let agent = if li == 0 {
            let mut links = Vec::new();
            for k in ["parentSession", "branchedFrom"] {
                if let Some(ps) = header.value.get(k).and_then(Value::as_str) {
                    let mut lx = Obj::new();
                    lx.insert(if ps.ends_with(".jsonl") { "path" } else { "session" }.into(), json!(ps));
                    links.push(Link { rel: "forked-from".into(), agent: session_id_from(ps), event: None, extra: lx });
                }
            }
            Agent {
                id: lid.clone(),
                created_at: created.clone(),
                source: Some(Source {
                    format: d.format().into(),
                    variant: Some(variant.clone()),
                    version: runtime_version.clone(),
                    native_id: Some(session_id.clone()),
                    extra,
                }),
                parent: parent.clone(),
                links,
                meta: Obj::new(),
            }
        } else {
            let b = &branches[li - 1];
            let first = entries[b.first];
            Agent {
                id: lid.clone(),
                created_at: first.value.get("timestamp").and_then(Value::as_str).map(String::from),
                source: Some(Source {
                    format: d.format().into(),
                    variant: Some(variant.clone()),
                    version: runtime_version.clone(),
                    native_id: first.id().map(String::from),
                    extra,
                }),
                parent: Some(Parent {
                    agent: ledger_ids[b.parent_ledger].clone(),
                    event: b.fork.and_then(|f| entries[f].id().map(String::from)),
                    relation: Relation::Fork,
                }),
                links: vec![],
                meta: Obj::new(),
            }
        };
        ledgers.push(Ledger::new(agent));
    }
    // v1 compaction indexes count the header as entry 0.
    ctx.v1_index_events.push(vec![]);
    let mut entry_pos: HashMap<usize, usize> = HashMap::new(); // physical line -> entry index
    for (i, e) in entries.iter().enumerate() {
        entry_pos.insert(e.no, i);
    }
    let mut first_of_ledger = vec![true; ledger_ids.len()];
    // A file protoledger rendered from a foreign ledger: its title slot is derived.
    let header_carried = opts.restore_carried && header.value.get(carry::KEY).is_some();
    for line in &lines {
        match line {
            Line::Blank { .. } => {}
            Line::Malformed { no, off, text } => {
                ctx.losses.add("malformed-line", None, "a line that is not a JSON object; pi skips it on load");
                let id = format!("@{off}");
                let ev = Event::new(id.clone(), Kind::Native).native(Native::new(d.format(), Some(id)).variant(Some(ctx.variant.clone())).raw(text.clone()));
                ledgers[0].append_unique(ev);
                ledger_lines[0].push(*no);
            }
            Line::Record(r) => {
                if header_carried && slot && r.no == recs[0].no {
                    continue;
                }
                let li = entry_pos.get(&r.no).map(|&i| assign[i]).unwrap_or(0);
                let mut rec = r.clone();
                let mut restored: Option<Vec<Event>> = None;
                if opts.restore_carried
                    && rec.value.get(carry::KEY).is_some()
                    && let Some(c) = take_carrier(&mut rec.value)
                {
                    if let Some(a) = c.agent
                        && (first_of_ledger[li] || rec.typ() == "session")
                    {
                        agent_override[li] = Some(a);
                    }
                    restored = Some(c.events);
                }
                first_of_ledger[li] = false;
                let evs = match restored {
                    Some(evs) => evs,
                    None => {
                        let is_header = std::ptr::eq(r, header) || (r.no == header.no);
                        map_record(&mut ctx, &rec, is_header, slot && r.no == recs[0].no)
                    }
                };
                let mut ids = Vec::new();
                for e in evs {
                    ids.push(ledgers[li].append_unique(e).id.clone());
                }
                if r.typ() != "session" && !(slot && r.no == recs[0].no) {
                    ctx.v1_index_events.push(ids.clone());
                }
                if let Some(id) = rec.id() {
                    ctx.entry_events.entry(id.to_string()).or_insert(ids);
                }
                ledger_lines[li].push(r.no);
            }
        }
    }
    // Keep physical placement when it is not simply "one ledger in file order".
    let blanks: Vec<Value> = lines.iter().filter_map(|l| if let Line::Blank { no, text } = l { Some(json!([no, text])) } else { None }).collect();
    let need_lines = ledgers.len() > 1 || !blanks.is_empty();
    for (li, l) in ledgers.iter_mut().enumerate() {
        if let Some(a) = agent_override[li].take() {
            l.header.agent = a;
            continue;
        }
        let ex = &mut l.agent_mut().source.as_mut().unwrap().extra;
        if need_lines {
            ex.insert("lines".into(), json!(ledger_lines[li]));
        }
        if li == 0 {
            if !blanks.is_empty() {
                ex.insert("blank".into(), Value::Array(blanks.clone()));
            }
            if !trailing {
                ex.insert("eof_newline".into(), json!(false));
            }
        }
    }
    // A spawning tool call may sit on a branch: point at the ledger that holds it.
    let spawns = std::mem::take(&mut ctx.spawns)
        .into_iter()
        .map(|(stem, ev)| {
            let owner = ledgers.iter().find(|l| l.contains(&ev)).map(|l| l.id().to_string()).unwrap_or_else(|| main_id.clone());
            (stem, (owner, ev))
        })
        .collect();
    Ok(FileImport { ledgers, main_id, variant, spawns })
}

/// [`carry::take`] for a top-level carrier: the carried events, with each
/// carried native record's exact text put back.
fn take_carrier(record: &mut Value) -> Option<carry::Carried> {
    carry::take_with(record, &[])
}

/// pi: the last entry. omp: the last entry, unless it is a late `model_usage`
/// that captured the leaf that was active when it was appended.
fn resolve_leaf(d: Dialect, entries: &[&Rec], member: &[bool], by_id: &HashMap<String, usize>) -> Option<usize> {
    let last = (0..entries.len()).rev().find(|&i| member[i])?;
    if d == Dialect::Omp && entries[last].typ() == "model_usage" {
        match entries[last].value.get("activeLeafId") {
            Some(Value::String(s)) => {
                if let Some(&i) = by_id.get(s) {
                    return Some(i);
                }
                return (0..entries.len()).rev().find(|&i| member[i] && entries[i].typ() != "model_usage");
            }
            Some(Value::Null) => return None,
            _ => {}
        }
    }
    Some(last)
}

/// `.../2026-01-01T00-00-00-000Z_<id>.jsonl` → `<id>`; a bare id stays itself.
fn session_id_from(s: &str) -> Option<String> {
    let name = s.rsplit(['/', '\\']).next().unwrap_or(s);
    if let Some(stem) = name.strip_suffix(".jsonl") {
        return stem.split_once('_').map(|(_, id)| id.to_string()).or_else(|| Some(stem.to_string()));
    }
    Some(s.to_string())
}

// ---- record → events --------------------------------------------------------

enum Part {
    Text(String),
    Image { mime: String, data: String },
}

fn parts_of(content: &Value) -> Vec<Part> {
    match content {
        Value::String(s) => vec![Part::Text(s.clone())],
        Value::Array(a) => a
            .iter()
            .filter_map(|b| match b.get("type").and_then(Value::as_str) {
                Some("text") => Some(Part::Text(b.get("text").and_then(Value::as_str).unwrap_or("").to_string())),
                Some("image") => Some(Part::Image {
                    mime: b.get("mimeType").and_then(Value::as_str).unwrap_or("application/octet-stream").to_string(),
                    data: b.get("data").and_then(Value::as_str).unwrap_or("").to_string(),
                }),
                _ => None,
            })
            .collect(),
        _ => vec![],
    }
}

fn media_event(role: &str, mime: &str, data: &str) -> Event {
    let e = Event::new("", Kind::Media).with("role", role).with("media_type", mime);
    if data.starts_with("blob:sha256:") { e.with("url", data) } else { e.with("data", data) }
}

/// SPEC §3.1.6: a peer message in a harness without a peer channel is user
/// text wrapped as `<peer-message direction="in" peer="ID" network="NET">\nTEXT\n</peer-message>`.
pub(crate) fn peer_wrap(e: &Event) -> String {
    let mut open = String::from("<peer-message");
    for k in ["direction", "peer", "network"] {
        if let Some(v) = e.str(k) {
            open.push_str(&format!(" {k}=\"{v}\""));
        }
    }
    format!("{open}>\n{}\n</peer-message>", e.str("text").unwrap_or(""))
}

/// The `peer_message` a wrapped text stands for, if it is one.
pub(crate) fn peer_unwrap(text: &str) -> Option<Event> {
    let rest = text.strip_prefix("<peer-message")?;
    let (attrs, body) = rest.split_once(">\n")?;
    let body = body.strip_suffix("\n</peer-message>")?;
    let mut e = Event::new("", Kind::PeerMessage);
    let mut a = attrs.trim();
    while !a.is_empty() {
        let (k, r) = a.split_once("=\"")?;
        let (v, r) = r.split_once('"')?;
        if !matches!(k, "direction" | "peer" | "network") {
            return None;
        }
        e = e.with(k, v);
        a = r.trim_start();
    }
    e.str("direction")?;
    Some(e.with("text", body))
}

/// SPEC §3.1.6 on import: a user or assistant text event that is a peer
/// message's wrapper becomes that peer message, keeping its id, native and
/// everything else (so one record still makes as many events).
pub(crate) fn unwrap_peer_in_place(e: &mut Event) {
    if e.kind != Kind::Text || !matches!(e.str("role"), Some("user" | "assistant")) {
        return;
    }
    if let Some(p) = e.str("text").and_then(peer_unwrap) {
        e.kind = Kind::PeerMessage;
        e.body = p.body;
    }
}

/// SPEC §3.1.6 on export, in a harness with no peer channel: the role and
/// wrapped text a peer message is rendered as (user when incoming, assistant
/// when outgoing; the importer's [`unwrap_peers`] turns it back).
pub(crate) fn peer_as_text(e: &Event) -> (&'static str, String) {
    (if e.str("direction") == Some("out") { "assistant" } else { "user" }, peer_wrap(e))
}

/// SPEC §3.1.2 on export to a harness whose tool arguments are an object: the
/// call's `input` when it is one; a custom tool's `input_text` (or an input
/// that is not an object) wrapped as `{"input": …}`, and named; `{}` when the
/// call has neither.
pub(crate) fn tool_args_object(e: &Event, losses: &mut crate::formats::Losses) -> Value {
    match (e.body.get("input"), e.str("input_text")) {
        (Some(v @ Value::Object(_)), _) => v.clone(),
        (_, Some(t)) => {
            losses.add("tool-input-text-wrapped", Some(Kind::ToolCall), "raw tool input (a custom tool's input_text) becomes arguments {\"input\": text}");
            json!({"input": t})
        }
        (None | Some(Value::Null), None) => json!({}),
        (Some(v), None) => {
            losses.add("tool-input-wrapped", Some(Kind::ToolCall), "non-object tool input becomes arguments {\"input\": value}");
            json!({"input": v})
        }
    }
}

/// Loss code for a peer message whose body holds more than the wrapper says.
pub(crate) const PEER_FIELDS_DROPPED: &str = "peer-message-fields-dropped";
pub(crate) const PEER_FIELDS_DROPPED_DETAIL: &str = "only direction, peer, network and text fit the peer-message wrapper";

/// Whether a peer message holds fields the wrapper cannot say.
pub(crate) fn peer_has_more(e: &Event) -> bool {
    e.body.keys().any(|k| !matches!(k.as_str(), "direction" | "peer" | "network" | "text"))
}

/// [`unwrap_peer_in_place`] over every event a `format` importer made from
/// its own records (carried events it restored are left as they came).
pub(crate) fn unwrap_peers(set: &mut crate::ledger::LedgerSet, format: &str) {
    for l in set.ledgers.values_mut() {
        for e in l.events.iter_mut().filter(|e| e.native.as_ref().is_some_and(|n| n.format == format)) {
            unwrap_peer_in_place(e);
        }
    }
}

/// A text event, or the peer message it wraps.
pub(crate) fn text_or_peer(role: &str, text: String) -> Event {
    if matches!(role, "user" | "assistant")
        && let Some(p) = peer_unwrap(&text)
    {
        return p;
    }
    Event::new("", Kind::Text).with("role", role).with("text", text)
}

fn content_events(role: &str, content: &Value, losses: &mut Losses) -> Vec<Event> {
    parts_of(content)
        .into_iter()
        .map(|p| match p {
            Part::Text(t) => text_or_peer(role, t),
            Part::Image { mime, data } => {
                if data.starts_with("blob:sha256:") {
                    losses.add("media-blob-ref", Some(Kind::Media), "omp image stored in its blob store; kept as a blob: url");
                }
                media_event(role, &mime, &data)
            }
        })
        .collect()
}

fn text_of(content: &Value) -> String {
    parts_of(content).into_iter().filter_map(|p| if let Part::Text(t) = p { Some(t) } else { None }).collect::<Vec<_>>().join("")
}

/// SPEC §3.1.1 usage: the response's counts. The provider is a setting (it
/// arrives on the header / `model_change` settings), not a usage fact.
fn usage_event(_provider: Option<&str>, model: Option<&str>, u: &Value) -> Event {
    let n = |k: &str| u.get(k).cloned().unwrap_or(Value::Null);
    Event::new("", Kind::Usage)
        .with("model", model.map(|m| json!(m)).unwrap_or(Value::Null))
        .with("input", n("input"))
        .with("output", n("output"))
        .with("cache_read", n("cacheRead"))
        .with("cache_write", n("cacheWrite"))
        .with("reasoning", n("reasoning"))
        .with("cost_usd", u.get("cost").and_then(|c| c.get("total")).cloned().unwrap_or(Value::Null))
        .with("cumulative", false)
}

/// pi's `bashExecutionToText` (coding-agent `core/messages.ts`): what the model is shown.
fn bash_execution_text(m: &Value) -> String {
    let s = |k: &str| m.get(k).and_then(Value::as_str).unwrap_or("");
    let mut t = format!("Ran `{}`\n", s("command"));
    let out = s("output");
    if !out.is_empty() {
        t.push_str(&format!("```\n{out}\n```"));
    } else {
        t.push_str("(no output)");
    }
    if m.get("cancelled").and_then(Value::as_bool) == Some(true) {
        t.push_str("\n\n(command cancelled)");
    } else if let Some(c) = m.get("exitCode").and_then(Value::as_i64)
        && c != 0
    {
        t.push_str(&format!("\n\nCommand exited with code {c}"));
    }
    if m.get("truncated").and_then(Value::as_bool) == Some(true)
        && let Some(p) = m.get("fullOutputPath").and_then(Value::as_str)
    {
        t.push_str(&format!("\n\n[Output truncated. Full output: {p}]"));
    }
    t
}

fn split_model(m: &str) -> (Option<String>, String) {
    match m.split_once('/') {
        Some((p, rest)) => (Some(p.to_string()), rest.to_string()),
        None => (None, m.to_string()),
    }
}

fn map_record(ctx: &mut Ctx, r: &Rec, is_header: bool, is_slot: bool) -> Vec<Event> {
    let d = ctx.d;
    let v = &r.value;
    let ts = v.get("timestamp").and_then(Value::as_str).map(String::from);
    let base = if is_header || (r.id().is_some() && r.typ() != "title") { r.id().unwrap().to_string() } else { format!("@{}", r.off) };
    let s = |k: &str| v.get(k).and_then(Value::as_str);
    let mut evs: Vec<Event> = Vec::new();
    let mut group = None;
    if ctx.superseded.contains(&r.no) {
        ctx.losses.add("snapshot-superseded", Some(Kind::Native), "an earlier snapshot of a message a later line replaces; kept native only");
        evs.push(Event::new("", Kind::Native));
    } else if is_slot {
        let t = s("title").unwrap_or("");
        evs.push(if t.is_empty() { Event::new("", Kind::Native) } else { Event::new("", Kind::Title).with("text", t) });
    } else if is_header {
        let mut e = Event::new("", Kind::Settings).with("cwd", s("cwd").map(|x| json!(x)).unwrap_or(Value::Null));
        if let Some(m) = s("modelId") {
            e = e.with("model", m).with("provider", s("provider").map(|x| json!(x)).unwrap_or(Value::Null));
        }
        if let Some(t) = s("thinkingLevel") {
            e = e.with("effort", t);
        }
        if let Some(dirs) = v.get("additionalDirectories") {
            e = e.with("additional_directories", dirs.clone());
        }
        // SPEC §3.1.3: a header with an empty `cwd` and nothing else (what
        // this exporter writes for a ledger that names no settings) states
        // no setting.
        let states = e.body.iter().any(|(k, x)| !x.is_null() && !(k == "cwd" && x == &json!("")));
        evs.push(if states { e } else { Event::new("", Kind::Native) });
        if let Some(t) = s("title").filter(|t| !t.is_empty()) {
            evs.push(Event::new("", Kind::Title).with("text", t));
        }
    } else {
        match r.typ() {
            "message" => {
                let m = v.get("message").cloned().unwrap_or(Value::Null);
                let ms = |k: &str| m.get(k).and_then(Value::as_str);
                let content = m.get("content").cloned().unwrap_or(Value::Null);
                let role = ms("role").unwrap_or("");
                if matches!(role, "user" | "developer" | "custom" | "hookMessage") && parts_of(&content).len() > 1 {
                    group = Some(base.clone());
                }
                match role {
                    "user" => evs.extend(content_events("user", &content, ctx.losses)),
                    "developer" => evs.extend(content_events("developer", &content, ctx.losses)),
                    "custom" | "hookMessage" => evs.extend(content_events("user", &content, ctx.losses)),
                    "system" => {
                        let mut prompt = Vec::new();
                        if let Some(Value::Object(sec)) = m.get("sections") {
                            for x in sec.values().filter_map(Value::as_str) {
                                prompt.push(x.to_string());
                            }
                        }
                        let c = text_of(&content);
                        if !c.is_empty() {
                            prompt.push(c);
                        }
                        let mut e = Event::new("", Kind::Settings);
                        if !prompt.is_empty() {
                            e = e.with("system_prompt", prompt.join("\n\n"));
                        }
                        if let Some(t) = m.get("toolsAdded") {
                            e = e.with("tools", t.clone());
                        }
                        evs.push(e);
                    }
                    "bashExecution" => {
                        let mut e = Event::new("", Kind::Text).with("role", "user").with("text", bash_execution_text(&m));
                        if m.get("excludeFromContext").and_then(Value::as_bool) == Some(true) {
                            e = e.audience(Audience::Record);
                        }
                        evs.push(e);
                    }
                    "branchSummary" => evs.push(
                        Event::new("", Kind::Text)
                            .with("role", "user")
                            .with("text", format!("{BRANCH_SUMMARY_PREFIX}{}{BRANCH_SUMMARY_SUFFIX}", ms("summary").unwrap_or(""))),
                    ),
                    "assistant" => {
                        // SPEC §3.1.4: the provider's response id when known.
                        group = Some(ms("responseId").filter(|r| !r.is_empty()).unwrap_or(&base).to_string());
                        let provider = ms("provider").unwrap_or("").to_string();
                        let api = ms("api").unwrap_or("");
                        for b in content.as_array().map(|a| a.as_slice()).unwrap_or(&[]) {
                            let bs = |k: &str| b.get(k).and_then(Value::as_str);
                            match bs("type") {
                                Some("text") => evs.push(Event::new("", Kind::Text).with("role", "assistant").with("text", bs("text").unwrap_or(""))),
                                Some("thinking") => {
                                    let mut e = Event::new("", Kind::Reasoning).with("text", bs("thinking").unwrap_or(""));
                                    if b.get("redacted").and_then(Value::as_bool) == Some(true) {
                                        e = e.with("redacted", true);
                                    }
                                    if let Some(sig) = bs("thinkingSignature").filter(|x| !x.is_empty()) {
                                        // SPEC §3.1.5: anthropic for a signature, openai for encrypted content.
                                        let (vendor, kind) = if api.starts_with("openai") || provider.starts_with("openai") {
                                            ("openai", "encrypted")
                                        } else if api.starts_with("anthropic") || provider == "anthropic" {
                                            ("anthropic", "signature")
                                        } else {
                                            (provider.as_str(), "signature")
                                        };
                                        e = e.with("sealed", json!({"vendor": vendor, "kind": kind, "data": sig}));
                                    }
                                    evs.push(e);
                                }
                                Some("toolCall") => {
                                    let id = bs("id").unwrap_or("").to_string();
                                    let name = bs("name").unwrap_or("").to_string();
                                    let args = b.get("arguments").cloned().unwrap_or(json!({}));
                                    let ev_id = if evs.is_empty() { base.clone() } else { format!("{base}#{}", evs.len()) };
                                    ctx.calls.insert(id.clone(), (name.clone(), args.clone(), ev_id));
                                    let tool_type = if matches!(name.as_str(), "bash" | "shell") { "shell" } else { "function" };
                                    evs.push(
                                        Event::new("", Kind::ToolCall).with("call_id", id).with("name", name).with("input", args).with("tool_type", tool_type),
                                    );
                                }
                                _ => {}
                            }
                        }
                        // An all-zero block states nothing (pi writes one when a
                        // provider reports no usage, as this exporter does): no usage.
                        if let Some(u) = m.get("usage").filter(|u| u.is_object() && !says_nothing(u)) {
                            evs.push(usage_event(ms("provider"), ms("model"), u));
                        }
                        match ms("stopReason") {
                            Some("error") => {
                                evs.push(Event::new("", Kind::Error).with("kind", "model").with("message", ms("errorMessage").unwrap_or("").to_string()))
                            }
                            Some("aborted") => evs.push(
                                Event::new("", Kind::Turn)
                                    .with("phase", "end")
                                    .with("outcome", "aborted")
                                    .with("reason", ms("errorMessage").map(|x| json!(x)).unwrap_or(Value::Null)),
                            ),
                            _ => {}
                        }
                    }
                    "toolResult" => {
                        let call_id = ms("toolCallId").unwrap_or("").to_string();
                        let is_error = m.get("isError").and_then(Value::as_bool).unwrap_or(false);
                        let ps = parts_of(&content);
                        let output = if ps.iter().all(|p| matches!(p, Part::Text(_))) && ps.len() <= 1 {
                            json!(text_of(&content))
                        } else {
                            Value::Array(
                                ps.into_iter()
                                    .map(|p| match p {
                                        Part::Text(t) => json!({"type":"text","text":t}),
                                        Part::Image { mime, data } => json!({"type":"media","media_type":mime,"data":data}),
                                    })
                                    .collect(),
                            )
                        };
                        evs.push(Event::new("", Kind::ToolResult).with("call_id", call_id.clone()).with("output", output).with("is_error", is_error));
                        let call = ctx.calls.get(&call_id).cloned();
                        let tool = ms("toolName").unwrap_or("");
                        if let (false, Some((_, args, _))) = (is_error, &call) {
                            let path = args.get("path").or_else(|| args.get("file_path")).and_then(Value::as_str);
                            match (tool, path) {
                                ("edit", Some(p)) => {
                                    let mut e = Event::new("", Kind::FileChange).with("path", p).with("op", "edit");
                                    if let Some(diff) = util::get_str(&m, &["details", "diff"]) {
                                        e = e.with("diff", diff);
                                    }
                                    evs.push(e);
                                }
                                ("write", Some(p)) => {
                                    let mut e = Event::new("", Kind::FileChange).with("path", p).with("op", "create");
                                    if let Some(c) = args.get("content").and_then(Value::as_str) {
                                        e = e.with("content", c);
                                    }
                                    evs.push(e);
                                }
                                _ => {}
                            }
                        }
                        // omp's task tool: one result per spawned subagent.
                        if d == Dialect::Omp && tool == "task" {
                            for res in util::get(&m, &["details", "results"]).and_then(Value::as_array).map(|a| a.as_slice()).unwrap_or(&[]) {
                                let rs = |k: &str| res.get(k).and_then(Value::as_str);
                                let Some(aid) = rs("id") else { continue };
                                let child = ctx.child_ids.get(aid).cloned();
                                if let Some((_, _, ev)) = &call {
                                    ctx.spawns.insert(aid.to_string(), ev.clone());
                                }
                                let outcome = if res.get("aborted").and_then(Value::as_bool) == Some(true) {
                                    "aborted"
                                } else if res.get("exitCode").and_then(Value::as_i64).unwrap_or(0) != 0 || rs("error").is_some() {
                                    "error"
                                } else {
                                    "ok"
                                };
                                evs.push(
                                    Event::new("", Kind::Subagent)
                                        .with("phase", "end")
                                        .with("agent", child.map(Value::String).unwrap_or(Value::Null))
                                        .with("call_id", call_id.clone())
                                        .with("agent_type", rs("agent").map(|x| json!(x)).unwrap_or(Value::Null))
                                        .with("description", rs("description").map(|x| json!(x)).unwrap_or(Value::Null))
                                        .with("outcome", outcome)
                                        .with("result", rs("output").map(|x| json!(x)).unwrap_or(Value::Null)),
                                );
                            }
                        }
                    }
                    _ => {}
                }
            }
            "model_change" => {
                let (provider, model) =
                    if let Some(mid) = s("modelId") { (s("provider").map(String::from), mid.to_string()) } else { split_model(s("model").unwrap_or("")) };
                let role = s("role").unwrap_or("default");
                if role == "default" && !model.is_empty() {
                    evs.push(Event::new("", Kind::Settings).with("model", model).with("provider", provider.map(Value::String).unwrap_or(Value::Null)));
                }
            }
            "thinking_level_change" => {
                evs.push(Event::new("", Kind::Settings).with("effort", s("thinkingLevel").unwrap_or("off")));
            }
            "compaction" => {
                let mut e = Event::new("", Kind::Compaction)
                    .with("phase", "summary")
                    .with("summary", s("summary").unwrap_or(""))
                    .with("tokens_before", v.get("tokensBefore").cloned().unwrap_or(Value::Null))
                    .with("tokens_after", v.get("tokensAfter").cloned().unwrap_or(Value::Null));
                if let Some(p) = preserved(ctx, r) {
                    e = e.with("preserved", p);
                }
                evs.push(e);
                if let Some(u) = v.get("usage").filter(|u| u.is_object()) {
                    evs.push(usage_event(None, None, u));
                }
            }
            "branch_summary" => {
                evs.push(
                    Event::new("", Kind::Text)
                        .with("role", "user")
                        .with("text", format!("{BRANCH_SUMMARY_PREFIX}{}{BRANCH_SUMMARY_SUFFIX}", s("summary").unwrap_or(""))),
                );
                if let Some(u) = v.get("usage").filter(|u| u.is_object()) {
                    evs.push(usage_event(None, None, u));
                }
            }
            "custom_message" => {
                let c = v.get("content").cloned().unwrap_or(Value::Null);
                if parts_of(&c).len() > 1 {
                    group = Some(base.clone());
                }
                evs.extend(content_events("user", &c, ctx.losses));
            }
            "session_info" => evs.push(Event::new("", Kind::Title).with("text", s("name").unwrap_or("").trim())),
            "title_change" => evs.push(Event::new("", Kind::Title).with("text", s("title").unwrap_or(""))),
            "usage" | "model_usage" => {
                if let Some(u) = v.get("usage").filter(|u| u.is_object()) {
                    let model = s("model").map(String::from);
                    let mut e = usage_event(s("provider"), model.as_deref(), u);
                    if let Some(k) = s("kind").or(s("purpose")) {
                        e = e.with("purpose", k);
                    }
                    evs.push(e);
                }
            }
            "reset_boundary" => evs.push(Event::new("", Kind::Compaction).with("phase", "boundary").with("trigger", "manual").with("reason", "reset")),
            "session_init" => {
                let mut e = Event::new("", Kind::Settings);
                match v.get("systemPrompt") {
                    Some(Value::String(p)) => e = e.with("system_prompt", p.clone()),
                    Some(Value::Array(a)) => e = e.with("system_prompt", a.iter().filter_map(Value::as_str).collect::<Vec<_>>().join("\n\n")),
                    _ => {}
                }
                if let Some(t) = v.get("tools") {
                    e = e.with("tools", t.clone());
                }
                if let Some(a) = s("agent") {
                    e = e.with("agent", a);
                }
                evs.push(e);
            }
            _ => {}
        }
    }
    if evs.is_empty() {
        let t = if is_slot {
            "title"
        } else if is_header {
            "session"
        } else {
            r.typ()
        };
        ctx.losses.add("record-native-only", Some(Kind::Native), &format!("{} records with no neutral meaning (e.g. {t})", d.format()));
        evs.push(Event::new("", Kind::Native));
    }
    let version = util::get_str(v, &["runtime", "version"]).map(String::from);
    let native_id = if is_header || (r.id().is_some() && !is_slot) { base.clone() } else { format!("@{}", r.off) };
    let mut native = Native::new(d.format(), Some(native_id)).variant(Some(ctx.variant.clone())).version(version);
    // The line as written (SPEC §4); a line a stale carrier was taken out of
    // is held as what is left.
    if serde_json::from_str::<Value>(&r.text).ok().as_ref() == Some(v) {
        jsonfmt::hold(&mut native, Some(&r.text), v);
    } else {
        native.set_record(Some(v.clone()));
        native.raw = (!r.exact).then(|| r.text.clone());
    }
    let n = evs.len();
    for (i, e) in evs.iter_mut().enumerate() {
        e.id = if i == 0 { base.clone() } else { format!("{base}#{i}") };
        e.ts = ts.clone();
        e.group = group.clone().filter(|_| n > 0);
        e.native = Some(if i == 0 { native.clone() } else { native.part_of(i as u32) });
    }
    evs
}

/// Event ids of the entries a compaction keeps verbatim (`firstKeptEntryId`
/// up to the compaction, along its own path; v1: `firstKeptEntryIndex`).
fn preserved(ctx: &Ctx, r: &Rec) -> Option<Value> {
    let v = &r.value;
    if let Some(idx) = v.get("firstKeptEntryIndex").and_then(Value::as_u64) {
        let idx = idx as usize;
        if idx >= ctx.v1_index_events.len() {
            return None;
        }
        let ids: Vec<Value> = ctx.v1_index_events[idx..].iter().flatten().map(|s| json!(s)).collect();
        return Some(Value::Array(ids));
    }
    let first = v.get("firstKeptEntryId").and_then(Value::as_str)?;
    let own = r.id()?;
    if first == own {
        return Some(json!([]));
    }
    let mut chain = Vec::new();
    let mut cur = ctx.parent_of.get(own).cloned().flatten();
    let mut guard = 0;
    while let Some(id) = cur {
        guard += 1;
        if guard > 1_000_000 {
            return None;
        }
        chain.push(id.clone());
        if id == first {
            break;
        }
        cur = ctx.parent_of.get(&id).cloned().flatten();
    }
    if chain.last().map(String::as_str) != Some(first) {
        return None;
    }
    chain.reverse();
    let ids: Vec<Value> = chain.iter().flat_map(|id| ctx.entry_events.get(id).cloned().unwrap_or_default()).map(Value::String).collect();
    Some(Value::Array(ids))
}

// ---------------------------------------------------------------------------
// Export
// ---------------------------------------------------------------------------

pub(crate) fn export(d: Dialect, set: &LedgerSet, opts: &ExportOptions) -> Result<Exported> {
    let mut out = Output::default();
    let mut losses = Losses::default();
    let fmt = d.format();
    // Which file owns each ledger: forks live inside their parent's file.
    let mut files: BTreeMap<String, Vec<&Ledger>> = BTreeMap::new();
    for l in set.iter() {
        files.entry(owner_of(set, l)).or_default().push(l);
    }
    // File paths, parents before subagents.
    let mut paths: HashMap<String, String> = HashMap::new();
    let mut owners: Vec<&Ledger> = files.keys().filter_map(|k| set.get(k)).collect();
    owners.sort_by_key(|l| depth(set, l));
    let use_slot = d == Dialect::Omp && omp_slot_wanted(opts);
    for o in &owners {
        let native_file = o.agent().source.as_ref().filter(|s| s.format == fmt).and_then(|s| s.extra.get("file")).and_then(Value::as_str).map(String::from);
        let path = match native_file {
            Some(f) => f,
            None => {
                let sid = native_session_id(o);
                let ts = o.agent().created_at.clone().or_else(|| o.events.iter().find_map(|e| e.ts.clone()));
                let stamp = ts.map(|t| util::norm_ts(&t)).unwrap_or_else(|| "1970-01-01T00:00:00.000Z".into()).replace([':', '.'], "-");
                let parent_path =
                    o.agent().parent.as_ref().and_then(|p| set.get(&p.agent)).map(|pl| owner_of(set, pl)).and_then(|oid| paths.get(&oid)).cloned();
                match (d, parent_path) {
                    (Dialect::Omp, Some(pp)) => {
                        let name = o.events.iter().find_map(|e| e.str("agent_type").map(String::from)).unwrap_or_else(|| "agent".into());
                        format!("{}/{}-{}.jsonl", pp.trim_end_matches(".jsonl"), sanitize(&name), util::short_id(o.id(), 6))
                    }
                    (_, pp) => {
                        if pp.is_some() {
                            losses.add("subagent-separate-session", Some(Kind::Subagent), "pi has no subagents: the child is written as its own session file");
                        }
                        format!("{stamp}_{sid}.jsonl")
                    }
                }
            }
        };
        paths.insert(o.id().to_string(), path);
    }
    for o in &owners {
        let ledgers = &files[o.id()];
        let parent_path = o.agent().parent.as_ref().and_then(|p| set.get(&p.agent)).map(|pl| owner_of(set, pl)).and_then(|oid| paths.get(&oid)).cloned();
        let text = render_file(d, set, o, ledgers, parent_path.as_deref(), opts, use_slot, &mut losses)?;
        out.put(paths[o.id()].clone(), text.into_bytes());
    }
    let variant = match d {
        Dialect::Pi => "v3",
        Dialect::Omp => {
            if use_slot {
                "title-slot"
            } else {
                "header-first"
            }
        }
        Dialect::LettaLocal => "local-backend",
    };
    Ok(Exported { output: out, losses: losses.sorted(), variant: variant.into() })
}

fn owner_of(set: &LedgerSet, l: &Ledger) -> String {
    let mut cur = l;
    let mut guard = 0;
    while let Some(p) = &cur.agent().parent {
        guard += 1;
        if p.relation != Relation::Fork || guard > 10_000 {
            break;
        }
        match set.get(&p.agent) {
            Some(pl) => cur = pl,
            None => break,
        }
    }
    cur.id().to_string()
}

fn depth(set: &LedgerSet, l: &Ledger) -> usize {
    let mut n = 0;
    let mut cur = l;
    while let Some(p) = &cur.agent().parent {
        match set.get(&p.agent) {
            Some(x) if n < 10_000 => {
                n += 1;
                cur = x;
            }
            _ => break,
        }
    }
    n
}

fn sanitize(s: &str) -> String {
    s.chars().map(|c| if c.is_ascii_alphanumeric() || c == '-' || c == '_' { c } else { '-' }).collect()
}

fn omp_slot_wanted(opts: &ExportOptions) -> bool {
    if let Some(v) = &opts.variant {
        return v != "header-first";
    }
    match &opts.target_version {
        Some(v) => version_ge(v, &[16, 2, 3]),
        None => true,
    }
}

fn version_ge(v: &str, min: &[u64]) -> bool {
    let parts: Vec<u64> = v.trim_start_matches('v').split(['.', '-']).take(3).map(|p| p.parse().unwrap_or(0)).collect();
    for (i, m) in min.iter().enumerate() {
        let x = parts.get(i).copied().unwrap_or(0);
        if x != *m {
            return x > *m;
        }
    }
    true
}

/// The pi session id a ledger exports under.
fn native_session_id(l: &Ledger) -> String {
    l.agent().source.as_ref().filter(|s| s.format == "pi" || s.format == "omp").and_then(|s| s.native_id.clone()).unwrap_or_else(|| util::uuid_for(l.id()))
}

/// omp's `serializeTitleSlot`: a `title` record padded to exactly 256 bytes.
pub(crate) fn title_slot_line(title: &str, updated_at: &str) -> String {
    let line = |t: &str, pad: &str| {
        let mut s = serde_json::to_string(&json!({"type":"title","v":1,"title":t,"updatedAt":updated_at,"pad":pad})).unwrap();
        s.push('\n');
        s
    };
    let cps: Vec<char> = title.chars().collect();
    let mut n = cps.len();
    let mut t: String = cps.iter().collect();
    while line(&t, "").len() > TITLE_SLOT_BYTES && n > 0 {
        n -= 1;
        t = cps[..n].iter().collect();
    }
    let pad = TITLE_SLOT_BYTES.saturating_sub(line(&t, "").len());
    line(&t, &" ".repeat(pad))
}

struct Renderer<'a> {
    d: Dialect,
    carry: bool,
    losses: &'a mut Losses,
    ledger_id: String,
    recs: Vec<(Option<usize>, Value, Option<String>)>, // (line, record, raw)
    /// For each record, the protoledger events it represents (carry).
    rec_events: Vec<Vec<Event>>,
    /// Records emitted unchanged from this format's own native data.
    rec_native: Vec<bool>,
    last_entry: Option<String>,
    last_ts: Option<String>,
    provider: String,
    model: String,
    call_names: HashMap<String, String>,
    pending: Vec<Event>,
    /// event id → entry id it landed in.
    entry_of: HashMap<String, String>,
    asst: Option<Acc>,
    user: Option<Acc>,
    native_count: usize,
    fresh_count: usize,
}

struct Acc {
    group: Option<String>,
    ts: Option<String>,
    seed: String,
    content: Vec<Value>,
    usage: Option<Value>,
    model: Option<String>,
    provider: Option<String>,
    events: Vec<Event>,
    has_tool: bool,
}

fn ms_of(ts: &Option<String>) -> i64 {
    ts.as_deref().and_then(util::parse_rfc3339_ms).unwrap_or(0)
}

fn api_for(provider: &str) -> &'static str {
    match provider {
        "anthropic" => "anthropic-messages",
        "openai" | "openai-codex" => "openai-responses",
        "google" | "gemini" => "google-generative-ai",
        _ => "openai-completions",
    }
}

fn pi_usage(b: &Obj) -> Value {
    let n = |k: &str| b.get(k).and_then(Value::as_u64).unwrap_or(0);
    let (i, o, cr, cw) = (n("input"), n("output"), n("cache_read"), n("cache_write"));
    let total = b.get("cost_usd").cloned().unwrap_or(json!(0));
    let mut u = json!({"input": i, "output": o, "cacheRead": cr, "cacheWrite": cw, "totalTokens": i + o + cr + cw,
        "cost": {"input": 0, "output": 0, "cacheRead": 0, "cacheWrite": 0, "total": total}});
    if let Some(r) = b.get("reasoning").filter(|r| !r.is_null()) {
        u["reasoning"] = r.clone();
    }
    u
}

/// A usage block whose counters and cost are all zero says nothing.
fn says_nothing(u: &Value) -> bool {
    let z = |v: Option<&Value>| v.is_none_or(|v| v.as_f64() == Some(0.0));
    ["input", "output", "cacheRead", "cacheWrite", "totalTokens", "reasoning"].iter().all(|k| z(u.get(*k))) && z(u.get("cost").and_then(|c| c.get("total")))
}

fn zero_usage() -> Value {
    json!({"input":0,"output":0,"cacheRead":0,"cacheWrite":0,"totalTokens":0,"cost":{"input":0,"output":0,"cacheRead":0,"cacheWrite":0,"total":0}})
}

impl<'a> Renderer<'a> {
    fn push(&mut self, line: Option<usize>, rec: Value, raw: Option<String>, events: Vec<Event>) -> usize {
        if let Some(id) = rec.get("id").and_then(Value::as_str)
            && rec.get("type").and_then(Value::as_str) != Some("session")
            && rec.get("type").and_then(Value::as_str) != Some("title")
        {
            self.last_entry = Some(id.to_string());
            for e in &events {
                self.entry_of.insert(e.id.clone(), id.to_string());
            }
        }
        if let Some(t) = rec.get("timestamp").and_then(Value::as_str) {
            self.last_ts = Some(t.to_string());
        }
        let mut evs = std::mem::take(&mut self.pending);
        evs.extend(events);
        self.recs.push((line, rec, raw));
        self.rec_events.push(evs);
        self.rec_native.push(false);
        self.recs.len() - 1
    }

    fn ts_or_last(&self, ts: &Option<String>) -> String {
        ts.clone().or_else(|| self.last_ts.clone()).unwrap_or_else(|| "1970-01-01T00:00:00.000Z".into())
    }

    /// A new entry with pi's base fields first.
    fn entry(&mut self, typ: &str, seed: &str, ts: &Option<String>, fields: Value) -> Value {
        let mut m = Map::new();
        m.insert("type".into(), json!(typ));
        m.insert("id".into(), json!(util::short_id(&format!("{}/{seed}", self.ledger_id), 8)));
        m.insert("parentId".into(), self.last_entry.clone().map(Value::String).unwrap_or(Value::Null));
        m.insert("timestamp".into(), json!(self.ts_or_last(ts)));
        if let Value::Object(f) = fields {
            for (k, v) in f {
                m.insert(k, v);
            }
        }
        self.fresh_count += 1;
        Value::Object(m)
    }

    fn flush(&mut self) {
        if let Some(a) = self.user.take() {
            let content = if a.content.len() == 1 && a.content[0].get("type").and_then(Value::as_str) == Some("text") {
                a.content[0]["text"].clone()
            } else {
                Value::Array(a.content)
            };
            let rec = self.entry("message", &a.seed, &a.ts, json!({"message": {"role":"user","content":content,"timestamp":ms_of(&a.ts)}}));
            self.push(None, rec, None, a.events);
        }
        if let Some(a) = self.asst.take() {
            let model = a.model.clone().unwrap_or_else(|| self.model.clone());
            let provider = a.provider.clone().unwrap_or_else(|| self.provider.clone());
            let msg = json!({"role":"assistant","content":a.content,"api":api_for(&provider),"provider":provider,
                "model":model,"usage":a.usage.unwrap_or_else(zero_usage),
                "stopReason": if a.has_tool {"toolUse"} else {"stop"},"timestamp":ms_of(&a.ts)});
            let rec = self.entry("message", &a.seed, &a.ts, json!({"message": msg}));
            self.push(None, rec, None, a.events);
        }
    }

    fn user_part(&mut self, e: &Event, block: Value) {
        let joins = self.user.as_ref().is_some_and(|a| a.group.is_some() && a.group == e.group);
        if !joins {
            self.flush();
            self.user = Some(Acc {
                group: e.group.clone(),
                ts: e.ts.clone(),
                seed: e.id.clone(),
                content: vec![],
                usage: None,
                model: None,
                provider: None,
                events: vec![],
                has_tool: false,
            });
        }
        let a = self.user.as_mut().unwrap();
        a.content.push(block);
        a.events.push(e.clone());
    }

    fn asst_part(&mut self, e: &Event, block: Option<Value>) {
        let joins = self.asst.as_ref().is_some_and(|a| a.group == e.group && (block.is_some() || a.usage.is_none()));
        if !joins {
            self.flush();
            self.asst = Some(Acc {
                group: e.group.clone(),
                ts: e.ts.clone(),
                seed: e.id.clone(),
                content: vec![],
                usage: None,
                model: None,
                provider: None,
                events: vec![],
                has_tool: false,
            });
        } else if self.user.is_some() {
            let u = self.user.take();
            let a = self.asst.take();
            self.user = u;
            self.flush();
            self.asst = a;
        }
        let a = self.asst.as_mut().unwrap();
        if let Some(b) = block {
            if b.get("type").and_then(Value::as_str) == Some("toolCall") {
                a.has_tool = true;
            }
            a.content.push(b);
        }
        a.events.push(e.clone());
    }

    fn render_foreign(&mut self, e: &Event) {
        let k = e.kind;
        match k {
            Kind::Text => {
                let role = e.str("role").unwrap_or("user");
                let text = e.str("text").unwrap_or("").to_string();
                match role {
                    "assistant" => self.asst_part(e, Some(json!({"type":"text","text":text}))),
                    "system" | "developer" => {
                        self.flush();
                        let role = if self.d == Dialect::Omp { "developer" } else { "system" };
                        let content = if role == "system" { json!(text) } else { json!([{"type":"text","text":text}]) };
                        let rec = self.entry("message", &e.id, &e.ts, json!({"message":{"role":role,"content":content,"timestamp":ms_of(&e.ts)}}));
                        self.push(None, rec, None, vec![e.clone()]);
                    }
                    _ => {
                        if self.asst.is_some() {
                            self.flush();
                        }
                        self.user_part(e, json!({"type":"text","text":text}))
                    }
                }
            }
            Kind::Media => {
                let data = e.str("data");
                match (e.str("role").unwrap_or("user"), data) {
                    ("user", Some(data)) => {
                        if self.asst.is_some() {
                            self.flush();
                        }
                        let mime = e.str("media_type").unwrap_or("image/png").to_string();
                        self.user_part(e, json!({"type":"image","data":data,"mimeType":mime}))
                    }
                    _ => {
                        self.losses.add("media-dropped", Some(Kind::Media), "pi images need inline base64 data on a user message");
                        self.defer(e);
                    }
                }
            }
            Kind::Reasoning => {
                let mut b = json!({"type":"thinking","thinking": e.str("text").unwrap_or("")});
                if e.body.get("redacted").and_then(Value::as_bool) == Some(true) {
                    b["redacted"] = json!(true);
                }
                if let Some(s) = e.body.get("sealed") {
                    if s.get("kind").and_then(Value::as_str) == Some("signature") {
                        if let Some(dt) = s.get("data").and_then(Value::as_str) {
                            b["thinkingSignature"] = json!(dt);
                            // SPEC §3.1.5: a signature's vendor is the provider
                            // that can replay it, when nothing else names one.
                            if self.provider == "unknown"
                                && self.asst.as_ref().is_none_or(|a| a.provider.is_none())
                                && let Some(v) = s.get("vendor").and_then(Value::as_str)
                            {
                                self.provider = v.to_string();
                            }
                        }
                    } else {
                        self.losses.add(
                            "reasoning-sealed-dropped",
                            Some(Kind::Reasoning),
                            "encrypted reasoning from another vendor's API is not replayable in pi",
                        );
                    }
                }
                self.asst_part(e, Some(b));
            }
            Kind::ToolCall => {
                let id = e.str("call_id").unwrap_or("").to_string();
                let name = e.str("name").unwrap_or("tool").to_string();
                self.call_names.insert(id.clone(), name.clone());
                let args = match e.body.get("input") {
                    Some(Value::Object(o)) => Value::Object(o.clone()),
                    Some(v) if !v.is_null() => {
                        self.losses.add(
                            "tool-input-wrapped",
                            Some(Kind::ToolCall),
                            "pi tool arguments are an object; a non-object input is wrapped as {input}",
                        );
                        json!({"input": v})
                    }
                    _ => match e.str("input_text") {
                        Some(t) => {
                            self.losses.add(
                                "tool-input-wrapped",
                                Some(Kind::ToolCall),
                                "pi tool arguments are an object; raw input text is wrapped as {input}",
                            );
                            json!({"input": t})
                        }
                        None => json!({}),
                    },
                };
                self.asst_part(e, Some(json!({"type":"toolCall","id":id,"name":name,"arguments":args})));
            }
            Kind::Usage if e.body.get("cumulative").and_then(Value::as_bool) == Some(true) => {
                // SPEC §3.1.1: a running total is never rendered as one response's usage.
                self.losses.add("usage-cumulative-dropped", Some(Kind::Usage), "pi records usage per response only");
                self.defer(e);
            }
            Kind::Usage => {
                if self.asst.as_ref().is_some_and(|a| a.usage.is_none() && (a.group == e.group || e.group.is_none())) {
                    let u = pi_usage(&e.body);
                    let a = self.asst.as_mut().unwrap();
                    a.usage = Some(u);
                    if let Some(m) = e.str("model") {
                        a.model = Some(m.to_string());
                    }
                    if let Some(p) = e.str("provider") {
                        a.provider = Some(p.to_string());
                    }
                    a.events.push(e.clone());
                } else {
                    self.flush();
                    let model = e.str("model").unwrap_or(&self.model).to_string();
                    if let Some(p) = e.str("provider") {
                        self.provider = p.to_string();
                    }
                    let rec = match self.d {
                        Dialect::Pi | Dialect::LettaLocal => self.entry(
                            "usage",
                            &e.id,
                            &e.ts,
                            json!({"kind": e.str("purpose").unwrap_or("protoledger"),
                            "provider": self.provider, "model": model, "usage": pi_usage(&e.body)}),
                        ),
                        Dialect::Omp => self.entry(
                            "model_usage",
                            &e.id,
                            &e.ts,
                            json!({"purpose": e.str("purpose").unwrap_or("protoledger"),
                            "api": api_for(&self.provider), "provider": self.provider, "model": model, "usage": pi_usage(&e.body), "stopReason":"stop"}),
                        ),
                    };
                    self.push(None, rec, None, vec![e.clone()]);
                }
            }
            Kind::ToolResult => {
                self.flush();
                let id = e.str("call_id").unwrap_or("").to_string();
                let name = self.call_names.get(&id).cloned().unwrap_or_else(|| "tool".into());
                let content = match e.body.get("output") {
                    Some(Value::String(s)) => json!([{"type":"text","text":s}]),
                    Some(Value::Array(a)) => Value::Array(
                        a.iter()
                            .filter_map(|p| match p.get("type").and_then(Value::as_str) {
                                Some("text") => Some(json!({"type":"text","text":p.get("text").cloned().unwrap_or(json!(""))})),
                                Some("media") if p.get("data").is_some() => Some(json!({"type":"image","data":p["data"],
                                    "mimeType": p.get("media_type").cloned().unwrap_or(json!("image/png"))})),
                                _ => {
                                    self.losses.add("media-dropped", Some(Kind::ToolResult), "a tool output part pi cannot hold");
                                    None
                                }
                            })
                            .collect(),
                    ),
                    Some(v) if !v.is_null() => json!([{"type":"text","text": v.to_string()}]),
                    _ => json!([]),
                };
                let is_error = e.body.get("is_error").and_then(Value::as_bool).unwrap_or(false);
                let rec = self.entry(
                    "message",
                    &e.id,
                    &e.ts,
                    json!({"message":{"role":"toolResult","toolCallId":id,"toolName":name,
                    "content":content,"isError":is_error,"timestamp":ms_of(&e.ts)}}),
                );
                self.push(None, rec, None, vec![e.clone()]);
            }
            Kind::Settings => {
                self.flush();
                // One record per setting; the first carries the event, the rest
                // carry an empty list (they are covered).
                let mut first = true;
                let mut emit = |r: &mut Self, rec: Value| {
                    let evs = if first { vec![e.clone()] } else { vec![] };
                    let idx = r.push(None, rec, None, evs);
                    if !first {
                        r.rec_events[idx].clear();
                    }
                    first = false;
                };
                if let Some(p) = e.str("provider") {
                    self.provider = p.to_string();
                }
                if let Some(m) = e.str("model") {
                    self.model = m.to_string();
                    let f = match self.d {
                        Dialect::Omp => json!({"model": format!("{}/{m}", self.provider)}),
                        _ => json!({"provider": self.provider, "modelId": m}),
                    };
                    let rec = self.entry("model_change", &format!("{}#m", e.id), &e.ts, f);
                    emit(self, rec);
                }
                if let Some(t) = e.str("effort") {
                    let rec = self.entry("thinking_level_change", &format!("{}#t", e.id), &e.ts, json!({"thinkingLevel": t}));
                    emit(self, rec);
                }
                if let Some(sp) = e.str("system_prompt") {
                    match self.d {
                        Dialect::Pi | Dialect::LettaLocal => {
                            let mut msg = json!({"role":"system","content":sp,"timestamp":ms_of(&e.ts)});
                            if let Some(t) = e.body.get("tools").filter(|t| t.as_array().is_some_and(|a| a.iter().all(Value::is_object))) {
                                msg["toolsAdded"] = t.clone();
                            }
                            let rec = self.entry("message", &format!("{}#s", e.id), &e.ts, json!({"message": msg}));
                            emit(self, rec);
                        }
                        Dialect::Omp => {
                            self.losses.add("system-prompt-dropped", Some(Kind::Settings), "omp rebuilds its system prompt; a foreign one is not stored")
                        }
                    }
                }
                let known = ["model", "provider", "effort", "system_prompt", "tools", "cwd", "complete", "agent", "additional_directories"];
                if e.body.keys().any(|k| !known.contains(&k.as_str())) {
                    self.losses.add("settings-partial", Some(Kind::Settings), "settings fields pi cannot record (sandbox, approval, ...)");
                }
                if first {
                    self.defer(e);
                }
            }
            Kind::Title => {
                self.flush();
                let t = e.str("text").unwrap_or("");
                let rec = match self.d {
                    Dialect::Omp => self.entry("title_change", &e.id, &e.ts, json!({"title": t, "source": "user"})),
                    _ => self.entry("session_info", &e.id, &e.ts, json!({"name": t})),
                };
                self.push(None, rec, None, vec![e.clone()]);
            }
            Kind::Compaction => {
                self.flush();
                match e.str("phase") {
                    Some("summary") | None if e.str("summary").is_some() => {
                        let first = e
                            .body
                            .get("preserved")
                            .and_then(Value::as_array)
                            .and_then(|a| a.first())
                            .and_then(Value::as_str)
                            .and_then(|id| self.entry_of.get(id).cloned());
                        let tb = e.body.get("tokens_before").cloned().filter(|v| !v.is_null()).unwrap_or(json!(0));
                        let mut rec = self.entry("compaction", &e.id, &e.ts, json!({"summary": e.str("summary").unwrap_or(""), "tokensBefore": tb}));
                        let own = rec["id"].clone();
                        rec["firstKeptEntryId"] = first.map(Value::String).unwrap_or(own);
                        self.push(None, rec, None, vec![e.clone()]);
                    }
                    _ => {
                        self.losses.add("compaction-boundary-dropped", Some(Kind::Compaction), "a compaction marker without a summary has no pi record");
                        self.defer(e);
                    }
                }
            }
            Kind::PeerMessage => {
                // SPEC §3.1.6: wrapped text the importer turns back into a peer message.
                let text = peer_wrap(e);
                if e.str("direction") == Some("out") {
                    self.asst_part(e, Some(json!({"type":"text","text":text})));
                } else {
                    if self.asst.is_some() {
                        self.flush();
                    }
                    self.user_part(e, json!({"type":"text","text":text}));
                }
            }
            Kind::Error | Kind::App => {
                self.flush();
                let ct = if k == Kind::Error { "protoledger.error" } else { "protoledger.app" };
                let rec = self.entry("custom", &e.id, &e.ts, json!({"customType": ct, "data": Value::Object(e.body.clone())}));
                self.push(None, rec, None, vec![e.clone()]);
            }
            Kind::Turn | Kind::Subagent | Kind::FileChange | Kind::Native => {
                let code = match k {
                    Kind::Turn => "turn-dropped",
                    Kind::Subagent => "subagent-event-dropped",
                    Kind::FileChange => "file-change-dropped",
                    _ => "native-dropped",
                };
                self.losses.add(code, Some(k), "no pi record for this kind");
                self.defer(e);
            }
        }
    }

    /// An event with no record of its own rides with whatever is being built.
    fn defer(&mut self, e: &Event) {
        if let Some(a) = self.asst.as_mut() {
            a.events.push(e.clone());
        } else if let Some(a) = self.user.as_mut() {
            a.events.push(e.clone());
        } else {
            self.pending.push(e.clone());
        }
    }

    /// Events that rendered to nothing go into a `custom` entry pi ignores,
    /// so a carried trip still brings them back.
    fn flush_pending(&mut self) {
        if self.pending.is_empty() {
            return;
        }
        if !self.carry {
            self.pending.clear();
            return;
        }
        let seed = format!("{}#carry", self.pending[0].id);
        let ts = self.pending[0].ts.clone();
        let rec = self.entry("custom", &seed, &ts, json!({"customType":"protoledger.carry"}));
        self.push(None, rec, None, vec![]);
    }

    fn finish(&mut self) {
        self.flush();
        if self.pending.is_empty() {
            return;
        }
        match self.rec_native.last() {
            Some(false) if self.carry => {
                let p = std::mem::take(&mut self.pending);
                self.rec_events.last_mut().unwrap().extend(p);
            }
            _ => self.flush_pending(),
        }
    }
}

#[allow(clippy::too_many_arguments)]
fn render_file(
    d: Dialect,
    set: &LedgerSet,
    owner: &Ledger,
    ledgers: &[&Ledger],
    parent_path: Option<&str>,
    opts: &ExportOptions,
    use_slot: bool,
    losses: &mut Losses,
) -> Result<String> {
    let fmt = d.format();
    // Parents before forks.
    let mut order: Vec<&Ledger> = ledgers.to_vec();
    order.sort_by_key(|l| (depth(set, l), l.agent().created_at.clone(), l.id().to_string()));
    // Rendered ledgers: id → (records, events per record, entry_of, native/fresh counts).
    struct Done {
        recs: Vec<(Option<usize>, Value, Option<String>)>,
        rec_events: Vec<Vec<Event>>,
        rec_native: Vec<bool>,
        entry_of: HashMap<String, String>,
        exact_lines: bool,
    }
    let mut done: BTreeMap<String, Done> = BTreeMap::new();
    let mut header_written = false;
    for l in &order {
        let is_owner = l.id() == owner.id();
        let lines: Option<Vec<usize>> =
            l.agent().source.as_ref().filter(|s| s.format == fmt).and_then(|s| s.extra.get("lines")).and_then(|v| serde_json::from_value(v.clone()).ok());
        let fork_entry = l
            .agent()
            .parent
            .as_ref()
            .filter(|p| p.relation == Relation::Fork)
            .and_then(|p| p.event.as_ref().and_then(|ev| done.get(&p.agent).and_then(|dn| dn.entry_of.get(ev).cloned())));
        // SPEC §2.3: the importer makes every branch off the active path its
        // own fork ledger, so a rewound ledger is written as its active path,
        // and an abandoned event rides on the next record.
        let on = crate::formats::active_path_only(l, losses, "with carry they ride on the next record", |e| e.native.as_ref().is_some_and(|n| n.format == fmt));
        let mut r = Renderer {
            d,
            carry: opts.carry,
            losses,
            ledger_id: l.id().to_string(),
            recs: vec![],
            rec_events: vec![],
            rec_native: vec![],
            last_entry: if is_owner { None } else { fork_entry },
            last_ts: l.agent().created_at.clone(),
            provider: "unknown".into(),
            model: "unknown".into(),
            call_names: HashMap::new(),
            pending: vec![],
            entry_of: HashMap::new(),
            asst: None,
            user: None,
            native_count: 0,
            fresh_count: 0,
        };
        // Native records of this format, emitted unchanged.
        let has_native_header = l.events.iter().any(|e| {
            e.native.as_ref().is_some_and(|n| {
                n.format == fmt && n.part == 0 && n.record_json().as_ref().and_then(|r| r.get("type")).and_then(Value::as_str) == Some("session")
            })
        });
        if is_owner && !has_native_header {
            let ts = l.agent().created_at.clone().or_else(|| l.events.iter().find_map(|e| e.ts.clone()));
            let ts = ts.map(|t| util::norm_ts(&t)).unwrap_or_else(|| "1970-01-01T00:00:00.000Z".into());
            let cwd = l.events.iter().find_map(|e| if e.kind == Kind::Settings { e.str("cwd").map(String::from) } else { None });
            if use_slot {
                let title = ledgers
                    .iter()
                    .flat_map(|x| x.events.iter())
                    .filter(|e| e.kind == Kind::Title)
                    .max_by_key(|e| e.ts.as_deref().map(util::norm_ts))
                    .and_then(|e| e.str("text").map(String::from));
                let slot = title_slot_line(title.as_deref().unwrap_or(""), &ts);
                let v: Value = serde_json::from_str(slot.trim_end_matches('\n')).unwrap();
                r.push(None, v, None, vec![]);
            }
            let mut h = json!({"type":"session","version":3,"id":native_session_id(l),"timestamp":ts,"cwd":cwd.unwrap_or_default()});
            if let Some(pp) = parent_path {
                h["parentSession"] = json!(pp);
            }
            let idx = r.push(None, h, None, vec![]);
            let _ = idx;
            header_written = true;
        }
        let mut last_native: Option<(String, u32)> = None;
        let mut li = 0usize;
        for (e, on) in l.events.iter().zip(on) {
            if !on && e.native.as_ref().is_none_or(|n| n.format != fmt) {
                // What is open is written first, so the abandoned events ride
                // on the record after them, in their order.
                r.flush();
                r.pending.push(e.clone());
                continue;
            }
            if let Some(n) = e.native.as_ref().filter(|n| n.format == fmt) {
                if n.part == 0 && (n.record.is_some() || n.raw.is_some()) {
                    r.flush();
                    r.flush_pending();
                    let line = lines.as_ref().and_then(|ls| ls.get(li).copied());
                    li += 1;
                    let rec = n.record_json().unwrap_or(Value::Null);
                    if rec.get("type").and_then(Value::as_str) == Some("session") {
                        header_written = true;
                    }
                    if let Some(p) = rec.get("provider").and_then(Value::as_str) {
                        r.provider = p.to_string();
                    }
                    r.native_count += 1;
                    // Emitted as held, never re-serialised (SPEC §4).
                    let raw = crate::util::native_text(n).or_else(|| if rec.is_null() { Some(String::new()) } else { None });
                    let idx = r.push(line, rec, raw, vec![]);
                    r.rec_native[idx] = true;
                    if let Some(id) = r.recs[idx].1.get("id").and_then(Value::as_str) {
                        r.entry_of.insert(e.id.clone(), id.to_string());
                    }
                    last_native = n.id.clone().map(|i| (i, 0));
                    continue;
                }
                if n.part > 0 && last_native.as_ref().is_some_and(|(i, _)| Some(i) == n.id.as_ref()) {
                    if let Some(id) = r.last_entry.clone() {
                        r.entry_of.insert(e.id.clone(), id);
                    }
                    continue;
                }
            }
            last_native = None;
            // Keep the live provider/model for fresh assistant records.
            r.render_foreign(e);
        }
        r.finish();
        let exact_lines = lines.as_ref().is_some_and(|ls| ls.len() == r.native_count) && r.fresh_count == 0;
        done.insert(
            l.id().to_string(),
            Done {
                recs: std::mem::take(&mut r.recs),
                rec_events: std::mem::take(&mut r.rec_events),
                rec_native: std::mem::take(&mut r.rec_native),
                entry_of: std::mem::take(&mut r.entry_of),
                exact_lines,
            },
        );
    }
    let _ = header_written;
    // Carry: stow each fresh record's events, and the agent header of a
    // ledger that has no native header of its own.
    let mut placed: Vec<(Option<usize>, String, usize, usize)> = Vec::new(); // (line, ledger, idx, seq)
    for (lid, dn) in &mut done {
        let l = set.get(lid).unwrap();
        let native_agent = l.agent().source.as_ref().is_some_and(|s| s.format == fmt);
        for i in 0..dn.recs.len() {
            if opts.carry {
                let evs = std::mem::take(&mut dn.rec_events[i]);
                let fresh = !dn.rec_native[i] && dn.recs[i].1.get(carry::KEY).is_none();
                let rec = &mut dn.recs[i].1;
                let first = i == 0 || (lid == owner.id() && rec.get("type").and_then(Value::as_str) == Some("session"));
                let needs_agent = !native_agent && first && rec.get("type").and_then(Value::as_str) != Some("title");
                let is_slot = rec.get("type").and_then(Value::as_str) == Some("title");
                if fresh && !is_slot {
                    if evs.is_empty() && !needs_agent {
                        let h = carry::record_hash(rec);
                        rec[carry::KEY] = json!({"v": crate::model::SPEC_VERSION, "h": h, "events": []});
                    } else {
                        carry::attach_with(rec, &[], &evs, needs_agent.then(|| l.agent()));
                    }
                }
            }
        }
    }
    let all_exact = done.values().all(|d| d.exact_lines);
    let mut text = String::new();
    if all_exact {
        for (lid, dn) in &done {
            for (i, (line, _, _)) in dn.recs.iter().enumerate() {
                placed.push((*line, lid.clone(), i, 0));
            }
        }
        placed.sort_by_key(|p| p.0);
        let blank: Vec<(usize, String)> =
            owner.agent().source.as_ref().and_then(|s| s.extra.get("blank")).and_then(|v| serde_json::from_value(v.clone()).ok()).unwrap_or_default();
        let mut bi = 0;
        let mut lineno = 0usize;
        for (_, lid, i, _) in &placed {
            let line = done[lid].recs[*i].0.unwrap();
            while bi < blank.len() && blank[bi].0 <= line.max(lineno) && blank[bi].0 < line {
                text.push_str(&blank[bi].1);
                text.push('\n');
                bi += 1;
            }
            text.push_str(&line_text(&done[lid].recs[*i]));
            text.push('\n');
            lineno = line + 1;
        }
        for b in &blank[bi..] {
            text.push_str(&b.1);
            text.push('\n');
        }
        let eof_nl = owner.agent().source.as_ref().and_then(|s| s.extra.get("eof_newline")).and_then(Value::as_bool).unwrap_or(true);
        if !eof_nl && text.ends_with('\n') {
            text.pop();
        }
    } else {
        // Fallback: the owner's records in order, each fork right after its
        // fork point, keeping the owner's last record last so it stays the leaf.
        let mut seq: Vec<(String, usize)> = done[owner.id()].recs.iter().enumerate().map(|(i, _)| (owner.id().to_string(), i)).collect();
        for l in &order {
            if l.id() == owner.id() {
                continue;
            }
            let dn = &done[l.id()];
            let fork_entry = l
                .agent()
                .parent
                .as_ref()
                .and_then(|p| p.event.as_ref().and_then(|ev| done.get(&p.agent).and_then(|x| x.entry_of.get(ev).cloned()).map(|e| (p.agent.clone(), e))));
            let mut pos = seq.len().saturating_sub(1).max(if seq.is_empty() { 0 } else { 1 });
            if let Some((pa, entry)) = fork_entry
                && let Some(p) = seq.iter().position(|(lid, i)| *lid == pa && done[lid].recs[*i].1.get("id").and_then(Value::as_str) == Some(entry.as_str()))
            {
                pos = (p + 1).min(seq.len().saturating_sub(1).max(p + 1));
            }
            let owner_last = seq.iter().rposition(|(lid, _)| lid == owner.id());
            if let Some(ol) = owner_last
                && pos > ol
            {
                pos = ol;
            }
            for (k, i) in (0..dn.recs.len()).enumerate() {
                seq.insert(pos + k, (l.id().to_string(), i));
            }
        }
        for (lid, i) in seq {
            text.push_str(&line_text(&done[&lid].recs[i]));
            text.push('\n');
        }
    }
    Ok(text)
}

fn line_text(r: &(Option<usize>, Value, Option<String>)) -> String {
    match &r.2 {
        Some(raw) => raw.clone(),
        None => serde_json::to_string(&r.1).unwrap(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const TREE: &str = concat!(
        r#"{"type":"session","version":3,"id":"s1","timestamp":"2026-01-01T00:00:00.000Z","cwd":"/home/user/x"}"#,
        "\n",
        r#"{"type":"message","id":"a","parentId":null,"timestamp":"2026-01-01T00:00:01.000Z","message":{"role":"user","content":"hi","timestamp":1}}"#,
        "\n",
        r#"{"type":"message","id":"b","parentId":"a","timestamp":"2026-01-01T00:00:02.000Z","message":{"role":"assistant","content":[{"type":"text","text":"first"}],"api":"x","provider":"p","model":"m","usage":{"input":1,"output":2,"cacheRead":0,"cacheWrite":0,"totalTokens":3,"cost":{"input":0,"output":0,"cacheRead":0,"cacheWrite":0,"total":0}},"stopReason":"stop","timestamp":2}}"#,
        "\n",
        "\n",
        r#"{"type":"message","id":"c","parentId":"a","timestamp":"2026-01-01T00:00:03.000Z","message":{"role":"assistant","content":[{"type":"text","text":"second"}],"api":"x","provider":"p","model":"m","usage":{"input":1,"output":2,"cacheRead":0,"cacheWrite":0,"totalTokens":3,"cost":{"input":0,"output":0,"cacheRead":0,"cacheWrite":0,"total":0}},"stopReason":"stop","timestamp":3}}"#,
        "\n",
        "{\"type\":\"message\"\n",
    );

    fn imp(d: Dialect, text: &str) -> FileImport {
        let mut losses = Losses::default();
        import_text(d, text, "s.jsonl", None, &HashMap::new(), &ImportOptions::standard(), &mut losses).unwrap()
    }

    #[test]
    fn branches_become_fork_ledgers() {
        let fi = imp(Dialect::Pi, TREE);
        assert_eq!(fi.ledgers.len(), 2);
        let main = &fi.ledgers[0];
        let texts: Vec<_> = main.events.iter().filter_map(|e| e.str("text")).collect();
        assert_eq!(texts, ["hi", "second"]);
        let fork = &fi.ledgers[1];
        assert_eq!(fork.id(), "s1@b");
        let p = fork.agent().parent.as_ref().unwrap();
        assert_eq!((p.agent.as_str(), p.event.as_deref(), p.relation), ("s1", Some("a"), Relation::Fork));
        assert_eq!(fork.events[0].str("text"), Some("first"));
        assert_eq!(fork.events[1].kind, Kind::Usage);
    }

    #[test]
    fn self_trip_is_byte_exact_with_branches_blanks_and_garbage() {
        let fi = imp(Dialect::Pi, TREE);
        let mut set = LedgerSet::new();
        for l in fi.ledgers {
            set.insert(l);
        }
        let out = export(Dialect::Pi, &set, &ExportOptions::standard()).unwrap();
        assert_eq!(String::from_utf8(out.output.files["s.jsonl"].clone()).unwrap(), TREE);
    }

    #[test]
    fn foreign_ledger_renders_a_loadable_session_and_carries_back() {
        let mut l = Ledger::new(Agent {
            id: "foreign-1".into(),
            created_at: Some("2026-02-03T04:05:06.007Z".into()),
            source: None,
            parent: None,
            links: vec![],
            meta: Obj::new(),
        });
        l.append(Event::new("u1", Kind::Text).with("role", "user").with("text", "hello")).unwrap();
        l.append(Event::new("r1", Kind::Reasoning).with("text", "think").group(Some("g1"))).unwrap();
        l.append(Event::new("c1", Kind::ToolCall).with("call_id", "t1").with("name", "bash").with("input", json!({"command":"ls"})).group(Some("g1"))).unwrap();
        l.append(Event::new("x1", Kind::ToolResult).with("call_id", "t1").with("output", "a\n")).unwrap();
        l.append(Event::new("tu", Kind::Turn).with("phase", "end")).unwrap();
        let set = LedgerSet::single(l.clone());
        let out = export(Dialect::Pi, &set, &ExportOptions::no_carry()).unwrap();
        let (name, bytes) = out.output.files.iter().next().unwrap();
        assert!(name.starts_with("2026-02-03T04-05-06-007Z_"));
        let text = String::from_utf8(bytes.clone()).unwrap();
        let lines: Vec<Value> = text.lines().map(|x| serde_json::from_str(x).unwrap()).collect();
        assert_eq!(lines[0]["type"], "session");
        assert_eq!(lines[2]["message"]["content"][1]["type"], "toolCall");
        assert_eq!(lines[3]["message"]["toolName"], "bash");
        assert_eq!(lines[3]["parentId"], lines[2]["id"]);
        // With carry, re-import restores the original events and agent.
        let out = export(Dialect::Pi, &set, &ExportOptions::standard()).unwrap();
        let text = String::from_utf8(out.output.files.values().next().unwrap().clone()).unwrap();
        let mut losses = Losses::default();
        let fi = import_text(Dialect::Pi, &text, "x.jsonl", None, &HashMap::new(), &ImportOptions::standard(), &mut losses).unwrap();
        assert_eq!(fi.ledgers.len(), 1);
        assert_eq!(fi.ledgers[0].id(), "foreign-1");
        let ids: Vec<_> = fi.ledgers[0].events.iter().map(|e| e.id.as_str()).collect();
        assert_eq!(ids, ["u1", "r1", "c1", "x1", "tu"]);
    }

    #[test]
    fn title_slot_is_256_bytes() {
        assert_eq!(title_slot_line("", "2026-01-01T00:00:00.000Z").len(), 256);
        let long = "é".repeat(300);
        assert_eq!(title_slot_line(&long, "2026-01-01T00:00:00.000Z").len(), 256);
    }

    #[test]
    fn omp_leaf_follows_late_usage() {
        let text = concat!(
            r#"{"type":"session","version":3,"id":"s","timestamp":"t","cwd":"/"}"#,
            "\n",
            r#"{"type":"message","id":"a","parentId":null,"timestamp":"t","message":{"role":"user","content":"x","timestamp":1}}"#,
            "\n",
            r#"{"type":"message","id":"b","parentId":"a","timestamp":"t","message":{"role":"user","content":"y","timestamp":1}}"#,
            "\n",
            r#"{"type":"model_usage","id":"u","parentId":"a","timestamp":"t","purpose":"title","api":"x","provider":"p","model":"m","usage":{"input":1,"output":1},"stopReason":"stop","activeLeafId":"b"}"#,
            "\n",
        );
        let fi = imp(Dialect::Omp, text);
        // The usage hangs off `a` but the active path is a → b.
        assert_eq!(fi.ledgers.len(), 2);
        assert_eq!(fi.ledgers[0].events.iter().filter_map(|e| e.str("text")).collect::<Vec<_>>(), ["x", "y"]);
    }

    #[test]
    fn records_are_held_and_reemitted_as_written() {
        use crate::formats::atif::jsonfmt::without_raw;
        use crate::formats::openclaw::jsonl::spelling::{ODD, assert_value_loses, held};
        let header = r#"{"type":"session","version":3,"id":"s1","timestamp":"2026-01-01T00:00:00.000Z","cwd":"/x"}"#;
        let line = format!(
            r#"{{"type":"message","id":"a","parentId":null,"timestamp":"2026-01-01T00:00:01.000Z","message":{{"role":"user","content":"hi","timestamp":1}},{ODD}}}"#
        );
        assert_value_loses(&line);
        let src = format!("{header}\n{line}\n");
        for d in [Dialect::Pi, Dialect::Omp] {
            let mut set = LedgerSet::new();
            for l in imp(d, &src).ledgers {
                set.insert(l);
            }
            assert_eq!(held(&set, d.format()), [header, line.as_str()], "{d:?}: the ledger holds each line as written");
            for s in [set.clone(), without_raw(&set)] {
                let out = export(d, &s, &ExportOptions::standard()).unwrap();
                assert_eq!(String::from_utf8(out.output.files["s.jsonl"].clone()).unwrap(), src, "{d:?}");
            }
        }
    }

    #[test]
    fn carried_numbers_keep_their_text() {
        let mut e = Event::new("x", Kind::Native);
        // `0.000009` alone would survive a `Value` under serde_json's
        // `arbitrary_precision` (never on in protoledger's build); the escape
        // and the inner whitespace survive one under no build.
        let text = format!(r#"{{"n":0.000009,{}}}"#, crate::formats::openclaw::jsonl::spelling::ODD);
        crate::formats::openclaw::jsonl::spelling::assert_value_loses(&text);
        e.native = Some(Native::new("pi", None).record_raw(&text).unwrap());
        let mut rec = json!({"type":"custom","id":"c"});
        carry::attach(&mut rec, &[], &[e]);
        let mut rec: Value = serde_json::from_str(&rec.to_string()).unwrap();
        let back = take_carrier(&mut rec).unwrap().events;
        assert_eq!(back[0].native.as_ref().unwrap().record_text(), Some(text.as_str()));
    }

    #[test]
    fn letta_local_keeps_only_the_latest_snapshot_neutral() {
        let text = concat!(
            r#"{"type":"session","version":3,"id":"local-conv-1","timestamp":"t","cwd":"/"}"#,
            "\n",
            r#"{"type":"message","id":"a","parentId":null,"timestamp":"t","message":{"id":"ui-msg-1","role":"assistant","content":[{"type":"text","text":"Hel"}],"api":"local","provider":"local","model":"local","stopReason":"stop","timestamp":1}}"#,
            "\n",
            r#"{"type":"message","id":"b","parentId":"a","timestamp":"t","message":{"id":"ui-msg-1","role":"assistant","content":[{"type":"text","text":"Hello"}],"api":"local","provider":"local","model":"local","stopReason":"stop","timestamp":1}}"#,
            "\n",
        );
        let fi = imp(Dialect::LettaLocal, text);
        let l = &fi.ledgers[0];
        assert_eq!(l.events.iter().filter_map(|e| e.str("text")).collect::<Vec<_>>(), ["Hello"]);
        assert_eq!(l.events[1].kind, Kind::Native);
    }

    #[test]
    fn peer_messages_round_trip_through_wrapped_text() {
        let e = Event::new("p", Kind::PeerMessage).with("direction", "in").with("peer", "agent-7").with("network", "mesh").with("text", "hello\nthere");
        let t = peer_wrap(&e);
        assert_eq!(t, "<peer-message direction=\"in\" peer=\"agent-7\" network=\"mesh\">\nhello\nthere\n</peer-message>");
        let back = peer_unwrap(&t).unwrap();
        assert_eq!(back.body, e.body);
        assert!(peer_unwrap("<peer-message>\nx\n</peer-message>").is_none());
        assert_eq!(text_or_peer("user", "plain".into()).kind, Kind::Text);
    }

    #[test]
    fn version_compare() {
        assert!(version_ge("16.2.3", &[16, 2, 3]));
        assert!(version_ge("18.0.0", &[16, 2, 3]));
        assert!(!version_ge("16.2.2", &[16, 2, 3]));
    }
}
