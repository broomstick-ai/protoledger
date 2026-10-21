//! Claude Code → protoledger.

use super::*;
use crate::carry;
use crate::formats::Losses;
use crate::ledger::Ledger;
use crate::model::{Agent, Audience, Event, Kind, Link, Native, Obj, Parent, Relation, Source};
use anyhow::bail;
use base64::Engine;
use serde_json::{Map, json};
use std::collections::{BTreeMap, HashMap, HashSet};
use std::path::PathBuf;

mod follow;
pub(super) use follow::follow;

// ---------------------------------------------------------------------------
// Detection
// ---------------------------------------------------------------------------

const LINE_TYPES: &[&str] = &[
    "user",
    "assistant",
    "system",
    "attachment",
    "summary",
    "queue-operation",
    "file-history-snapshot",
    "last-prompt",
    "ai-title",
    "permission-mode",
    "mode",
    "progress",
];

/// Does this text look like a Claude Code transcript? Returns the first version seen.
fn sniff(text: &str) -> Option<Option<String>> {
    let mut hits = 0;
    let mut version = None;
    for line in text.lines().take(200) {
        let Ok(v) = serde_json::from_str::<Value>(line) else { continue };
        let Some(t) = s(&v, "type") else { continue };
        if !LINE_TYPES.contains(&t) {
            continue;
        }
        let claude_keys = v.get("sessionId").is_some()
            || v.get("parentUuid").is_some()
            || v.get("leafUuid").is_some()
            || (t == "file-history-snapshot" && v.get("snapshot").is_some());
        if claude_keys {
            hits += 1;
            if version.is_none() {
                version = s(&v, "version").map(str::to_string);
            }
        }
    }
    (hits > 0).then_some(version)
}

pub(super) fn detect(input: &Path) -> Option<Detection> {
    let files = collect(input).ok()?;
    let mut version = None;
    let mut found = false;
    for f in &files.entries {
        if !matches!(f.role, Role::Session | Role::SidecarAgent { .. } | Role::FlatAgent { .. }) {
            continue;
        }
        let head = &f.bytes[..f.bytes.len().min(256 * 1024)];
        let text = String::from_utf8_lossy(head);
        if let Some(v) = sniff(&text) {
            found = true;
            version = version.or(v);
        }
        if found && version.is_some() {
            break;
        }
    }
    found.then(|| Detection { variant: overall_variant(&files, version.as_deref()), confidence: 90, version })
}

// ---------------------------------------------------------------------------
// Input enumeration
// ---------------------------------------------------------------------------

#[derive(Debug, Clone, PartialEq)]
enum Role {
    Session,
    /// `<sid>/subagents/agent-<aid>.jsonl`
    SidecarAgent {
        sid: String,
        aid: String,
    },
    /// `agent-<aid>.jsonl` beside the session files.
    FlatAgent {
        aid: String,
    },
    /// `agent-<aid>.meta.json` beside a sidecar agent.
    Meta {
        aid: String,
    },
    /// Anything else (tool-results, unknown files). `owner` is the session
    /// whose sidecar directory holds it.
    Other {
        owner: Option<String>,
    },
}

struct Entry {
    rel: String,
    role: Role,
    bytes: Vec<u8>,
}

struct Files {
    entries: Vec<Entry>,
}

fn stem(name: &str, ext: &str) -> Option<String> {
    name.strip_suffix(ext).map(str::to_string)
}

fn collect(input: &Path) -> Result<Files> {
    let mut entries = Vec::new();
    if input.is_file() {
        let name = input.file_name().and_then(|n| n.to_str()).unwrap_or("session.jsonl").to_string();
        entries.push(Entry { rel: name.clone(), role: role_of(&name, &HashSet::new()), bytes: std::fs::read(input)? });
        // A session file named on its own still brings its sidecar directory.
        if let (Some(sid), Some(dir)) = (stem(&name, ".jsonl"), input.parent()) {
            let side = dir.join(&sid);
            if side.is_dir() {
                walk(dir, &side, &mut entries)?;
            }
        }
    } else if input.is_dir() {
        walk(input, input, &mut entries)?;
    } else {
        bail!("{}: no such file or directory", input.display());
    }
    // Roles depend on which `<sid>.jsonl` files exist (a directory beside one is its sidecar).
    let sessions: HashSet<String> = entries.iter().filter_map(|e| e.rel.strip_suffix(".jsonl").map(str::to_string)).collect();
    for e in &mut entries {
        e.role = role_of(&e.rel, &sessions);
    }
    entries.sort_by(|a, b| a.rel.cmp(&b.rel));
    Ok(Files { entries })
}

fn walk(root: &Path, dir: &Path, out: &mut Vec<Entry>) -> Result<()> {
    let mut items: Vec<PathBuf> = std::fs::read_dir(dir)?.filter_map(|e| e.ok().map(|e| e.path())).collect();
    items.sort();
    for p in items {
        let name = p.file_name().and_then(|n| n.to_str()).unwrap_or("");
        if name.starts_with('.') || name == "SOURCE.md" || name == "expect.json" {
            continue;
        }
        if p.is_dir() {
            walk(root, &p, out)?;
        } else {
            let rel = p.strip_prefix(root)?.to_string_lossy().replace('\\', "/");
            out.push(Entry { rel, role: Role::Other { owner: None }, bytes: std::fs::read(&p)? });
        }
    }
    Ok(())
}

fn role_of(rel: &str, sessions: &HashSet<String>) -> Role {
    let parts: Vec<&str> = rel.split('/').collect();
    let name = *parts.last().unwrap();
    let dir = &parts[..parts.len() - 1];
    // The session whose sidecar directory this file sits in, if any.
    let mut owner = None;
    for i in (1..=dir.len()).rev() {
        let candidate = dir[..i].join("/");
        if sessions.contains(&candidate) {
            owner = Some(dir[i - 1].to_string());
            break;
        }
    }
    if let Some(pos) = dir.iter().rposition(|c| *c == "subagents")
        && pos >= 1
    {
        let sid = dir[pos - 1].to_string();
        if let Some(aid) = name.strip_prefix("agent-").and_then(|n| n.strip_suffix(".jsonl")) {
            return Role::SidecarAgent { sid, aid: aid.to_string() };
        }
        if let Some(aid) = name.strip_prefix("agent-").and_then(|n| n.strip_suffix(".meta.json")) {
            return Role::Meta { aid: aid.to_string() };
        }
    }
    if owner.is_some() || !name.ends_with(".jsonl") {
        return Role::Other { owner };
    }
    match name.strip_prefix("agent-").and_then(|n| n.strip_suffix(".jsonl")) {
        Some(aid) => Role::FlatAgent { aid: aid.to_string() },
        None => Role::Session,
    }
}

fn overall_variant(files: &Files, version: Option<&str>) -> &'static str {
    overall_variant_with(files, version, None)
}

/// Does this text hold a line that looks like an inline sidechain line?
fn inline_text(bytes: &[u8]) -> bool {
    let t = String::from_utf8_lossy(bytes);
    t.lines().any(|l| l.contains("\"isSidechain\":true") && !l.contains("\"agentId\""))
}

/// [`overall_variant`], told by `known` (entry index, answer) what
/// [`inline_text`] says of a session whose bytes are not all held.
fn overall_variant_with(files: &Files, version: Option<&str>, known: Option<(usize, bool)>) -> &'static str {
    let mut has_flat = false;
    let mut has_sidecar = false;
    let mut has_inline = false;
    for (i, e) in files.entries.iter().enumerate() {
        match &e.role {
            Role::SidecarAgent { .. } | Role::Meta { .. } => has_sidecar = true,
            Role::Other { owner: Some(_) } => has_sidecar = true,
            Role::FlatAgent { .. } => has_flat = true,
            Role::Session => {
                let inline = match known {
                    Some((k, answer)) if k == i => answer,
                    _ => inline_text(&e.bytes),
                };
                if inline {
                    has_inline = true;
                }
            }
            _ => {}
        }
    }
    if has_sidecar {
        V_SIDECAR
    } else if has_flat {
        V_FLAT
    } else if has_inline {
        V_INLINE
    } else {
        version.map(variant_for_version).unwrap_or(V_SIDECAR)
    }
}

// ---------------------------------------------------------------------------
// Lines
// ---------------------------------------------------------------------------

pub(super) struct Line {
    pub offset: usize,
    pub text: String,
    /// The line's bytes when they are not UTF-8.
    pub bytes: Option<Vec<u8>>,
    pub value: Option<Value>,
}

/// Split a transcript into lines. Returns the lines and whether the file
/// ended with a newline.
pub(super) fn split_lines(bytes: &[u8]) -> (Vec<Line>, bool) {
    let trailing = bytes.last().is_none_or(|b| *b == b'\n');
    let mut pieces: Vec<&[u8]> = bytes.split(|b| *b == b'\n').collect();
    if trailing {
        pieces.pop();
    }
    let mut off = 0;
    let mut out = Vec::with_capacity(pieces.len());
    for p in pieces {
        let offset = off;
        off += p.len() + 1;
        match std::str::from_utf8(p) {
            Ok(t) => {
                let value = serde_json::from_str::<Value>(t).ok();
                out.push(Line { offset, text: t.to_string(), bytes: None, value });
            }
            Err(_) => out.push(Line { offset, text: String::new(), bytes: Some(p.to_vec()), value: None }),
        }
    }
    (out, trailing)
}

// ---------------------------------------------------------------------------
// Import
// ---------------------------------------------------------------------------

/// Facts gathered across every file before any line is mapped.
#[derive(Default)]
struct Global {
    /// tool_use id → child ledger id
    child_by_tool: HashMap<String, String>,
    /// agentId → child ledger id
    child_by_aid: HashMap<String, String>,
    /// child ledger id → (agent_type, description)
    child_info: HashMap<String, (Option<String>, Option<String>)>,
    /// tool_use id → (subagent_type, description) of its input, as calls are mapped
    call_input: HashMap<String, (Option<String>, Option<String>)>,
    /// tool_use id → the time of the line that made the call
    call_ts: HashMap<String, Option<String>>,
    /// tool_use ids whose `subagent` start event was written
    started: HashSet<String>,
    /// tool_use id → (ledger id, event id), filled while importing
    tool_event: HashMap<String, (String, String)>,
    /// `<sid>/tool-results/<name>` → entry index
    tool_results: HashMap<String, usize>,
    /// entry indexes already turned into events
    consumed: HashSet<usize>,
    restore: bool,
}

/// An import in progress: every file of the input, and the facts gathered
/// across all of them before any line is mapped.
struct Run {
    files: Files,
    g: Global,
    losses: Losses,
    variant: &'static str,
    metas: HashMap<String, (Value, Option<String>)>,
    /// Every transcript split into lines, until it is mapped.
    parsed: BTreeMap<usize, (Vec<Line>, bool)>,
}

/// One transcript mapped: its ledger and inline sidechains, not yet finished.
struct Mapped {
    main: Sink,
    children: Vec<Sink>,
    st: FileState,
}

pub(super) fn import(input: &Path, opts: &ImportOptions) -> Result<Imported> {
    let mut r = begin(input, opts)?;
    let mut ledgers: Vec<Ledger> = Vec::new();
    for i in r.order() {
        let m = r.transcript(i);
        ledgers.push(m.main.finish());
        for c in m.children {
            ledgers.push(c.finish());
        }
    }
    Ok(r.end(ledgers))
}

/// How much of a transcript's start is sniffed for its version.
const SNIFF_BYTES: usize = 256 * 1024;

/// The import's variant: the forced one, else what the files show (see
/// [`overall_variant_with`] for `known`).
fn import_variant(files: &Files, forced: Option<&str>, known: Option<(usize, bool)>) -> &'static str {
    let first_version = files
        .entries
        .iter()
        .filter(|e| matches!(e.role, Role::Session | Role::SidecarAgent { .. } | Role::FlatAgent { .. }))
        .find_map(|e| sniff(&String::from_utf8_lossy(&e.bytes[..e.bytes.len().min(SNIFF_BYTES)])).flatten());
    match forced {
        Some(v) => VARIANTS.iter().find(|x| x.name == v).map(|x| x.name).unwrap_or(overall_variant_with(files, None, known)),
        None => overall_variant_with(files, first_version.as_deref(), known),
    }
}

fn begin(input: &Path, opts: &ImportOptions) -> Result<Run> {
    begin_files(collect(input)?, opts)
}

fn begin_files(files: Files, opts: &ImportOptions) -> Result<Run> {
    let losses = Losses::default();
    let mut g = Global { restore: opts.restore_carried, ..Default::default() };
    let variant = import_variant(&files, opts.variant.as_deref(), None);

    // Split every transcript once.
    let mut parsed: BTreeMap<usize, (Vec<Line>, bool)> = BTreeMap::new();
    for (i, e) in files.entries.iter().enumerate() {
        if matches!(e.role, Role::Session | Role::SidecarAgent { .. } | Role::FlatAgent { .. }) {
            parsed.insert(i, split_lines(&e.bytes));
        }
    }

    // Agents: ids, metadata, and which tool_use spawned them.
    let mut metas: HashMap<String, (Value, Option<String>)> = HashMap::new();
    for e in &files.entries {
        if let Role::Meta { aid } = &e.role {
            let text = String::from_utf8_lossy(&e.bytes).to_string();
            if let Ok(v) = serde_json::from_str::<Value>(&text) {
                let raw = (serde_json::to_string(&v).ok().as_deref() != Some(text.as_str())).then_some(text);
                metas.insert(aid.clone(), (v, raw));
            }
        }
    }
    for (i, e) in files.entries.iter().enumerate() {
        match &e.role {
            Role::SidecarAgent { aid, .. } | Role::FlatAgent { aid } => {
                let id = child_id(aid);
                g.child_by_aid.insert(aid.clone(), id.clone());
                if let Some((m, _)) = metas.get(aid) {
                    if let Some(t) = s(m, "toolUseId") {
                        g.child_by_tool.insert(t.to_string(), id.clone());
                    }
                    g.child_info.insert(id, (s(m, "agentType").map(str::to_string), s(m, "description").map(str::to_string)));
                }
            }
            Role::Other { owner: Some(owner) } => {
                if let Some(pos) = e.rel.rfind("/tool-results/") {
                    let name = &e.rel[pos + "/tool-results/".len()..];
                    g.tool_results.insert(format!("{owner}/tool-results/{name}"), i);
                }
            }
            _ => {}
        }
    }
    // Older layouts have no meta files: there the spawning tool_result names
    // the agent, and it is learned when that line is mapped (SPEC §9.3: no
    // line is mapped with what a later line says).

    Ok(Run { files, g, losses, variant, metas, parsed })
}

impl Run {
    /// The transcripts in the order they are mapped: sessions first, so parents precede children.
    fn order(&self) -> Vec<usize> {
        let mut order: Vec<usize> = self.parsed.keys().copied().collect();
        order.sort_by_key(|i| (!matches!(self.files.entries[*i].role, Role::Session), *i));
        order
    }

    /// Map transcript `i`.
    fn transcript(&mut self, i: usize) -> Mapped {
        let variant = self.variant;
        let e = &self.files.entries[i];
        let (lines, trailing) = self.parsed.remove(&i).unwrap();
        let (id, native_id, file_variant, meta) = match &e.role {
            Role::Session => {
                let sid = stem(e.rel.rsplit('/').next().unwrap(), ".jsonl").unwrap_or_default();
                let inline = lines.iter().any(|l| l.value.as_ref().is_some_and(is_inline));
                let fv = if inline { V_INLINE } else { variant };
                (sid.clone(), sid, fv, None)
            }
            Role::SidecarAgent { aid, .. } => (child_id(aid), aid.clone(), V_SIDECAR, self.metas.get(aid).cloned()),
            Role::FlatAgent { aid } => (child_id(aid), aid.clone(), V_FLAT, None),
            _ => unreachable!(),
        };
        let mut fi = FileImport::new(&e.rel, &mut self.g, &self.files, &mut self.losses);
        let mut main = Sink::new(Agent {
            id: id.clone(),
            created_at: None,
            source: Some(Source { format: FORMAT.into(), variant: Some(file_variant.into()), version: None, native_id: Some(native_id), extra: Obj::new() }),
            parent: None,
            links: vec![],
            meta: Obj::new(),
        });
        let mut children: Vec<Sink> = Vec::new();
        fi.run(&lines, &mut main, &mut children, matches!(e.role, Role::Session));
        let st = std::mem::take(&mut fi.st);
        file_extras(&mut main, &e.rel, trailing, meta);
        Mapped { main, children, st }
    }

    /// The import, from every transcript's finished ledgers.
    fn end(mut self, mut ledgers: Vec<Ledger>) -> Imported {
        attach_unconsumed(&self.files, &self.g, &mut ledgers, &mut self.losses, self.variant);
        link_parents(&mut ledgers, &self.g);
        link_forks(&mut ledgers);
        let mut set = LedgerSet::new();
        for l in ledgers {
            set.insert(l);
        }
        Imported { set, losses: self.losses, variant: self.variant.to_string() }
    }
}

/// An inline sidechain line: `isSidechain` with no `agentId` (Claude Code before 2.0.30).
fn is_inline(v: &Value) -> bool {
    v["isSidechain"] == json!(true) && v.get("agentId").is_none()
}

/// The inline sidechain's lines, on its header.
fn set_inline(c: &mut Sink, rel: &str, lines: Vec<usize>) {
    c.ledger.agent_mut().source.as_mut().unwrap().extra.insert("inline".into(), json!({"file": rel, "lines": lines}));
}

/// A flat or sidecar agent names its session.
fn set_session(main: &mut Sink, sid: Option<&str>) {
    if main.ledger.id().starts_with("agent-")
        && let Some(sid) = sid
        && let Some(src) = main.ledger.agent_mut().source.as_mut()
    {
        src.extra.insert("session".into(), json!(sid));
    }
}

/// What a transcript's header says about its file.
fn file_extras(main: &mut Sink, rel: &str, trailing: bool, meta: Option<(Value, Option<String>)>) {
    let extra = &mut main.ledger.agent_mut().source.as_mut().unwrap().extra;
    extra.insert("file".into(), json!(rel));
    if !trailing {
        extra.insert("no_final_newline".into(), json!(true));
    }
    if let Some((m, raw)) = meta {
        extra.insert("meta".into(), m);
        if let Some(r) = raw {
            extra.insert("meta_raw".into(), json!(r));
        }
    }
}

/// Sidecar files nobody referenced, and unknown files: native events on their owner.
/// Returns (file entry index, agent id) for each file attached.
fn attach_unconsumed(files: &Files, g: &Global, ledgers: &mut Vec<Ledger>, losses: &mut Losses, variant: &str) -> Vec<(usize, String)> {
    let mut attached = Vec::new();
    let referable: HashSet<usize> = g.tool_results.values().copied().collect();
    for (i, e) in files.entries.iter().enumerate() {
        if g.consumed.contains(&i) {
            continue;
        }
        if !matches!(e.role, Role::Other { .. } | Role::Meta { .. }) {
            continue;
        }
        if matches!(e.role, Role::Meta { aid: ref a } if g.child_by_aid.contains_key(a)) {
            continue;
        }
        // A persisted tool output no line refers to yet: Claude Code writes
        // the file before the line that refers to it, and that line takes it
        // in (SPEC §9.3), so it is not read before then.
        if referable.contains(&i) {
            losses.add("tool-result-file-unreferenced", None, "a persisted tool output no line refers to; not read until a line does");
            continue;
        }
        // Any other file no line can refer to goes in a ledger of its own: at
        // the end of a transcript's ledger, the events of lines appended later
        // would go before it.
        let target = match ledgers.iter().position(|l| l.id() == FILES_LEDGER) {
            Some(t) => t,
            None => {
                ledgers.push(Ledger::new(Agent {
                    id: FILES_LEDGER.into(),
                    created_at: None,
                    source: Some(Source { format: FORMAT.into(), variant: Some(variant.into()), version: None, native_id: None, extra: Obj::new() }),
                    parent: None,
                    links: vec![],
                    meta: Obj::new(),
                }));
                ledgers.len() - 1
            }
        };
        let ev = file_event(&e.rel, &e.bytes, None, None);
        ledgers[target].append_unique(ev);
        attached.push((i, ledgers[target].id().to_string()));
        losses.add("sidecar-file-native-only", Some(Kind::Native), "a file in the session directory with no known meaning, kept byte for byte in native");
    }
    attached
}

/// The ledger that holds the location's files no line refers to.
const FILES_LEDGER: &str = "claude-code-files";

/// Structural parents of subagents, now that every spawning tool_use has an event.
fn link_parents(ledgers: &mut [Ledger], g: &Global) {
    let ids: HashSet<String> = ledgers.iter().map(|l| l.id().to_string()).collect();
    let tool_of_child: HashMap<String, String> = g.child_by_tool.iter().map(|(t, c)| (c.clone(), t.clone())).collect();
    for l in ledgers.iter_mut() {
        let id = l.id().to_string();
        // A header restored from a carrier is already the original one.
        let Some(src) = l.agent().source.clone().filter(|s| s.format == FORMAT) else { continue };
        let is_child = id.starts_with("agent-") || id.starts_with("sidechain-");
        if !is_child || l.agent().parent.is_some() {
            continue;
        }
        let spawned = tool_of_child.get(&id).and_then(|t| g.tool_event.get(t)).cloned();
        let parent = match spawned {
            Some((agent, event)) if agent != id => Some(Parent { agent, event: Some(event), relation: Relation::Subagent }),
            _ => {
                // Fall back to the parentAgentId in the meta file, then to the session.
                let meta_parent = src.extra.get("meta").and_then(|m| s(m, "parentAgentId")).map(child_id).filter(|p| ids.contains(p));
                let session = src.extra.get("session").and_then(Value::as_str).map(str::to_string).filter(|p| ids.contains(p));
                meta_parent.or(session).map(|agent| Parent { agent, event: None, relation: Relation::Subagent })
            }
        };
        l.agent_mut().parent = parent;
    }
}

pub(super) fn child_id(aid: &str) -> String {
    format!("agent-{aid}")
}

/// `--fork-session` copies the history into a new session file under the
/// same line uuids: a root whose first conversation line also opens an older
/// root is `forked-from` it.
fn link_forks(ledgers: &mut [Ledger]) {
    let conv = |l: &Ledger| -> Vec<String> {
        l.events
            .iter()
            .filter(|e| e.native.as_ref().is_some_and(|n| n.part == 0 && n.record_json().as_ref().is_some_and(|r| r.get("uuid").is_some())))
            .map(|e| e.id.clone())
            .collect()
    };
    let roots: Vec<usize> = (0..ledgers.len()).filter(|i| ledgers[*i].agent().parent.is_none()).collect();
    let lists: Vec<Vec<String>> = roots.iter().map(|i| conv(&ledgers[*i])).collect();
    let mut links = Vec::new();
    for (bi, b) in roots.iter().enumerate() {
        let Some(first) = lists[bi].first() else { continue };
        for (ai, a) in roots.iter().enumerate() {
            if ai == bi || !lists[ai].contains(first) {
                continue;
            }
            let (ca, cb) = (&ledgers[*a].agent().created_at, &ledgers[*b].agent().created_at);
            let a_is_original = (ca, ledgers[*a].id()) < (cb, ledgers[*b].id());
            if !a_is_original {
                continue;
            }
            let shared: HashSet<&String> = lists[ai].iter().collect();
            let last = lists[bi].iter().rev().find(|u| shared.contains(u)).cloned();
            links.push((*b, Link { rel: "forked-from".into(), agent: Some(ledgers[*a].id().to_string()), event: last, extra: Obj::new() }));
            break;
        }
    }
    for (b, link) in links {
        ledgers[b].agent_mut().links.push(link);
    }
}

fn tool_result_ids(v: &Value) -> Vec<String> {
    v.get("message")
        .and_then(|m| m.get("content"))
        .and_then(Value::as_array)
        .map(|a| a.iter().filter(|b| s(b, "type") == Some("tool_result")).filter_map(|b| s(b, "tool_use_id").map(str::to_string)).collect())
        .unwrap_or_default()
}

/// A sidecar file as one event whose native record is the file itself.
fn file_event(rel: &str, bytes: &[u8], call_id: Option<&str>, abs_path: Option<&str>) -> Event {
    let mut n = Native::new(FORMAT, Some(rel.to_string())).file(Some(rel));
    let (kind, mut ev) = match std::str::from_utf8(bytes) {
        Ok(t) => {
            n = n.raw(t);
            match call_id {
                Some(_) => (Kind::FileChange, Event::new(rel, Kind::FileChange).with("op", "create").with("content", t)),
                None => (Kind::Native, Event::new(rel, Kind::Native)),
            }
        }
        Err(_) => {
            n = n.record(json!({"base64": base64::engine::general_purpose::STANDARD.encode(bytes)}));
            (Kind::Native, Event::new(rel, Kind::Native))
        }
    };
    if kind == Kind::FileChange {
        ev = ev.with("path", abs_path.unwrap_or(rel));
        if let Some(c) = call_id {
            ev = ev.meta_ns(FORMAT, json!({"persisted_output_of": c}));
        }
    }
    ev.native(n)
}

/// One ledger being built, with the state the mapping carries line to line.
struct Sink {
    ledger: Ledger,
    prev_uuid: Option<String>,
    turn: Option<String>,
    cwd: Option<String>,
    branch: Option<String>,
    perm: Option<String>,
    model: Option<String>,
    system_prompt: Option<String>,
    first_ts: Option<String>,
    first_version: Option<String>,
    /// Line indexes (in the main file) of an inline sidechain's lines.
    inline_lines: Vec<usize>,
    /// An agent header restored from a carrier (SPEC §6).
    carried_agent: Option<Agent>,
    /// uuid-bearing lines of this ledger, in file order.
    uuids: Vec<String>,
    /// uuid → the last event its line made, which a rewind continues from.
    last_event: HashMap<String, String>,
}

impl Sink {
    fn new(agent: Agent) -> Self {
        Sink {
            ledger: Ledger::new(agent),
            prev_uuid: None,
            turn: None,
            cwd: None,
            branch: None,
            perm: None,
            model: None,
            system_prompt: None,
            first_ts: None,
            first_version: None,
            inline_lines: vec![],
            carried_agent: None,
            uuids: vec![],
            last_event: HashMap::new(),
        }
    }

    /// Note that the line `uuid` made events up to `last`.
    fn saw(&mut self, uuid: Option<&str>, last: Option<String>) {
        if let (Some(u), Some(e)) = (uuid, last) {
            if !self.last_event.contains_key(u) {
                self.uuids.push(u.to_string());
            }
            self.last_event.insert(u.to_string(), e);
        }
    }
    /// The header the ledger ends with: a carried one, else the agent with
    /// the time and version of its first line.
    fn header(&self) -> Agent {
        if let Some(agent) = &self.carried_agent {
            return agent.clone();
        }
        let mut a = self.ledger.agent().clone();
        a.created_at = self.first_ts.clone();
        if let Some(src) = a.source.as_mut() {
            src.version = self.first_version.clone();
        }
        a
    }
    fn finish(mut self) -> Ledger {
        *self.ledger.agent_mut() = self.header();
        self.ledger
    }
}

struct FileImport<'a> {
    rel: String,
    g: &'a mut Global,
    files: &'a Files,
    losses: &'a mut Losses,
    /// What the mapping carries from line to line (kept while a file is followed).
    st: FileState,
    /// Sidecar files a line referenced: (entry index, call id, path as written).
    pending_file: Vec<(usize, Option<String>, String)>,
    /// Every sidecar file a line referenced, consumed by it or not.
    touched: Vec<usize>,
}

/// What mapping one transcript carries from line to line, besides its sinks.
#[derive(Default)]
struct FileState {
    /// The file's `parentUuid` tree.
    tree: Tree,
    /// message.ids whose usage event was written (on the line that settled the message).
    usage_of: HashSet<String>,
    /// uuid → index of the inline sidechain it belongs to
    side_of: HashMap<String, usize>,
    /// The first `sessionId` the file names.
    first_sid: Option<String>,
}

/// One uuid-bearing line of a transcript, as the `parentUuid` tree sees it.
#[derive(Debug, Default)]
struct Node {
    idx: usize,
    parent: Option<String>,
    /// A `user` or `assistant` line: part of the conversation, not progress or bookkeeping.
    message: bool,
    /// An assistant line's `message.id`.
    mid: Option<String>,
    /// An assistant line's `tool_use` ids.
    uses: Vec<String>,
    /// A user line's `tool_result` ids.
    results: Vec<String>,
}

/// A transcript's `parentUuid` tree (SPEC §2.3). Claude Code writes the
/// conversation as a chain from the leaf back through `parentUuid`; parallel
/// tool calls are the one tree shape that is not a branch: the content blocks
/// of one response are chained lines, each tool result points at the line of
/// its own `tool_use`, and Claude Code puts the siblings (same `message.id`)
/// and their results back into the conversation. A `/rewind` is a line whose
/// `parentUuid` goes back past conversation lines that are none of these.
#[derive(Debug, Default)]
struct Tree {
    nodes: HashMap<String, Node>,
}

impl Tree {
    /// Add `lines`, the file's lines from index `base` on.
    fn extend(&mut self, lines: &[Line], base: usize) {
        let nodes = &mut self.nodes;
        for (i, l) in lines.iter().enumerate() {
            let idx = base + i;
            let Some(v) = &l.value else { continue };
            let Some(u) = s(v, "uuid") else { continue };
            let t = s(v, "type").unwrap_or("");
            let content = v.get("message").and_then(|m| m.get("content")).and_then(Value::as_array);
            let ids = |ty: &str, key: &str| -> Vec<String> {
                content.into_iter().flatten().filter(|b| s(b, "type") == Some(ty)).filter_map(|b| s(b, key).map(str::to_string)).collect()
            };
            let node = Node {
                idx,
                parent: s(v, "parentUuid").map(str::to_string),
                message: matches!(t, "user" | "assistant"),
                mid: (t == "assistant").then(|| v.get("message").and_then(|m| s(m, "id")).map(str::to_string)).flatten(),
                uses: if t == "assistant" { ids("tool_use", "id") } else { vec![] },
                results: if t == "user" { ids("tool_result", "tool_use_id") } else { vec![] },
            };
            nodes.entry(u.to_string()).or_insert(node);
        }
    }

    /// The conversation that line `leaf` ends, as Claude Code loads it: its
    /// `parentUuid` chain, plus the sibling blocks of every response on it and
    /// the results of their tool calls (lines before the leaf only).
    fn conversation(&self, leaf: &str) -> HashSet<String> {
        let mut on: HashSet<String> = HashSet::new();
        let Some(end) = self.nodes.get(leaf).map(|n| n.idx) else { return on };
        let mut cur = Some(leaf.to_string());
        while let Some(c) = cur {
            if !on.insert(c.clone()) {
                break;
            }
            cur = self.nodes.get(&c).and_then(|n| n.parent.clone()).filter(|p| self.nodes.contains_key(p));
        }
        let mids: HashSet<&str> = on.iter().filter_map(|u| self.nodes[u].mid.as_deref()).collect();
        let siblings: Vec<&String> =
            self.nodes.iter().filter(|(_, n)| n.idx < end && n.mid.as_deref().is_some_and(|m| mids.contains(m))).map(|(u, _)| u).collect();
        let mut uses: HashSet<&str> = HashSet::new();
        for u in on.iter().chain(siblings.iter().copied()) {
            uses.extend(self.nodes[u].uses.iter().map(String::as_str));
        }
        let results: Vec<&String> =
            self.nodes.iter().filter(|(_, n)| n.idx < end && n.results.iter().any(|r| uses.contains(r.as_str()))).map(|(u, _)| u).collect();
        on.extend(siblings.into_iter().cloned());
        on.extend(results.into_iter().cloned());
        on
    }

    /// The `after` of line `uuid`, whose `parentUuid` is a line already in
    /// `sink` but not the previous one: the last event of the latest line of
    /// its conversation when conversation lines were passed over to reach it
    /// (a rewind: they are an abandoned branch), and none otherwise (parallel
    /// tool results and progress lines are no branch).
    fn rewind_after(&self, uuid: &str, sink: &Sink) -> Option<String> {
        let conv = self.conversation(uuid);
        let mut passed_message = false;
        for u in sink.uuids.iter().rev() {
            if conv.contains(u) {
                return passed_message.then(|| sink.last_event[u].clone());
            }
            passed_message |= self.nodes.get(u).is_some_and(|n| n.message);
        }
        None
    }
}

impl<'a> FileImport<'a> {
    fn new(rel: &str, g: &'a mut Global, files: &'a Files, losses: &'a mut Losses) -> Self {
        FileImport { rel: rel.to_string(), g, files, losses, st: FileState::default(), pending_file: vec![], touched: vec![] }
    }

    fn run(&mut self, lines: &[Line], main: &mut Sink, children: &mut Vec<Sink>, is_session: bool) {
        self.prepare(lines, 0);
        self.map_lines(lines, 0, main, children, is_session);
        for c in children.iter_mut() {
            let lines = c.inline_lines.clone();
            set_inline(c, &self.rel, lines);
        }
        // Flat and sidecar agents name their session.
        set_session(main, self.st.first_sid.as_deref());
    }

    /// Take in the whole-file facts of `lines` (the file's lines from index
    /// `base` on) before any of them is mapped.
    fn prepare(&mut self, lines: &[Line], base: usize) {
        self.st.tree.extend(lines, base);
        for l in lines {
            if self.st.first_sid.is_none() {
                self.st.first_sid = l.value.as_ref().and_then(|v| s(v, "sessionId")).map(str::to_string);
            }
        }
    }

    /// Map `lines`, the file's lines from index `base` on.
    fn map_lines(&mut self, lines: &[Line], base: usize, main: &mut Sink, children: &mut Vec<Sink>, is_session: bool) {
        let main_id = main.ledger.id().to_string();
        for (i, l) in lines.iter().enumerate() {
            let i = base + i;
            let inline = is_session && l.value.as_ref().is_some_and(|v| v["isSidechain"] == json!(true) && v.get("agentId").is_none());
            if !inline {
                self.line(l, main);
                continue;
            }
            let v = l.value.as_ref().unwrap();
            let uuid = s(v, "uuid").unwrap_or("").to_string();
            let parent = s(v, "parentUuid");
            let k = match parent.and_then(|p| self.st.side_of.get(p)) {
                Some(k) => *k,
                None => {
                    let root = if uuid.is_empty() { format!("{}", l.offset) } else { uuid.clone() };
                    let mut extra = Obj::new();
                    extra.insert("session".into(), json!(main_id));
                    let sink = Sink::new(Agent {
                        id: format!("sidechain-{root}"),
                        created_at: None,
                        source: Some(Source { format: FORMAT.into(), variant: Some(V_INLINE.into()), version: None, native_id: Some(root), extra }),
                        parent: Some(Parent { agent: main_id.clone(), event: None, relation: Relation::Subagent }),
                        links: vec![],
                        meta: Obj::new(),
                    });
                    children.push(sink);
                    children.len() - 1
                }
            };
            if !uuid.is_empty() {
                self.st.side_of.insert(uuid, k);
            }
            children[k].inline_lines.push(i);
            self.line(l, &mut children[k]);
        }
    }

    fn line(&mut self, l: &Line, sink: &mut Sink) {
        let anchor = format!("@{}", l.offset);
        let Some(v) = &l.value else {
            let mut n = Native::new(FORMAT, Some(anchor.clone()));
            n = match &l.bytes {
                Some(b) => n.record(json!({"base64": base64::engine::general_purpose::STANDARD.encode(b)})),
                None => n.raw(l.text.clone()),
            };
            if !l.text.trim().is_empty() || l.bytes.is_some() {
                self.losses.add("unparsable-line", Some(Kind::Native), "a line that is not JSON, kept byte for byte in native.raw");
            }
            sink.ledger.append_unique(Event::new(anchor, Kind::Native).native(n));
            return;
        };
        if sink.first_ts.is_none() {
            sink.first_ts = timestamp(v);
        }
        if sink.first_version.is_none() {
            sink.first_version = s(v, "version").map(str::to_string);
        }
        // Carried protoledger events (SPEC §6).
        if self.g.restore && v.get(carry::KEY).is_some() {
            let mut rec = v.clone();
            if let Some(c) = carry::take_with(&mut rec, &[]) {
                if c.agent.is_some() {
                    sink.carried_agent = c.agent;
                }
                let mut last = None;
                for e in c.events {
                    if e.kind == Kind::Subagent
                        && e.str("phase") == Some("start")
                        && let Some(c) = e.str("call_id")
                    {
                        self.g.started.insert(c.to_string());
                    }
                    let call = (e.kind == Kind::ToolCall).then(|| e.str("call_id").map(str::to_string)).flatten();
                    let id = sink.ledger.append_unique(e).id.clone();
                    if let Some(c) = call {
                        self.g.tool_event.insert(c, (sink.ledger.id().to_string(), id.clone()));
                    }
                    last = Some(id);
                }
                sink.saw(s(&rec, "uuid"), last);
                self.observe(&rec, sink);
                return;
            }
            // Edited in the harness: import it fresh, without the stale carrier.
            let fresh = Line { offset: l.offset, text: serde_json::to_string(&rec).unwrap(), bytes: None, value: Some(rec) };
            return self.line(&fresh, sink);
        }
        let base = s(v, "uuid").map(str::to_string).unwrap_or(anchor);
        let mut parts = self.map(v, sink);
        let extras: Vec<Event> = std::mem::take(&mut self.pending_file)
            .into_iter()
            .map(|(i, call, path)| {
                let f = &self.files.entries[i];
                file_event(&f.rel, &f.bytes, Some(call.as_deref().unwrap_or("")), Some(&path))
            })
            .collect();
        if parts.is_empty() {
            parts.push(Event::new("", Kind::Native));
        }
        let ts = timestamp(v);
        let group = if s(v, "type") == Some("assistant") { v.get("message").and_then(|m| s(m, "id")).map(str::to_string) } else { None };
        // SPEC §2.3: `after` marks a rewind. A parent outside this ledger (a
        // resumed or continued session) is kept as it is, and starts no branch.
        let after = match (s(v, "uuid"), s(v, "parentUuid")) {
            (Some(u), Some(p)) if sink.prev_uuid.as_deref() != Some(p) => {
                if sink.last_event.contains_key(p) {
                    self.st.tree.rewind_after(u, sink)
                } else {
                    Some(p.to_string())
                }
            }
            _ => None,
        };
        let native = Native::new(FORMAT, Some(base.clone())).version(s(v, "version"));
        // The record is the line's own text, byte for byte (SPEC §4). `raw`
        // keeps a second copy only when the record could not be held as that
        // text (carriers keep exact text, so no other case needs one).
        let mut native0 = native.clone().record_raw(&l.text).unwrap_or_else(|_| native.clone().record(v.clone()));
        if native0.record_text() != Some(l.text.as_str()) {
            native0 = native0.raw(l.text.clone());
        }
        let turn = sink.turn.clone();
        let n = parts.len();
        let mut last = None;
        for (i, mut e) in parts.into_iter().enumerate() {
            // A message's usage keeps the id `map` gave it (keyed by the message id).
            if e.id.is_empty() {
                e.id = if i == 0 { base.clone() } else { format!("{base}#{i}") };
            }
            if e.ts.is_none() {
                e.ts = ts.clone();
            }
            if e.turn.is_none() {
                e.turn = turn.clone();
            }
            if group.is_some() && matches!(e.kind, Kind::Text | Kind::Reasoning | Kind::ToolCall | Kind::ToolResult | Kind::Media | Kind::Usage) {
                e.group = group.clone();
            }
            if i == 0 {
                e.after = after.clone();
                e.native = Some(native0.clone());
            } else {
                e.native = Some(native.part_of(i as u32));
            }
            let id = e.id.clone();
            let kind = e.kind;
            let call = e.str("call_id").map(str::to_string);
            let appended = sink.ledger.append_unique(e).id.clone();
            last = Some(appended.clone());
            if kind == Kind::ToolCall
                && let Some(c) = call
            {
                self.g.tool_event.insert(c, (sink.ledger.id().to_string(), appended));
            }
            let _ = (id, n);
        }
        for e in extras {
            let mut e = e;
            if e.ts.is_none() {
                e.ts = ts.clone();
            }
            if e.turn.is_none() {
                e.turn = turn.clone();
            }
            last = Some(sink.ledger.append_unique(e).id.clone());
        }
        sink.saw(s(v, "uuid"), last);
        if s(v, "uuid").is_some() {
            sink.prev_uuid = s(v, "uuid").map(str::to_string);
        }
    }
}

// The mapping proper. `extras` collects events with their own native record
// (sidecar files) that follow the line's own events.
impl FileImport<'_> {
    /// Keep the per-ledger state in step with a line whose events came from a carrier.
    fn observe(&mut self, v: &Value, sink: &mut Sink) {
        let _ = self.settings_change(v, sink);
        if let Some(p) = s(v, "promptId") {
            sink.turn = Some(p.to_string());
        }
        if s(v, "uuid").is_some() {
            sink.prev_uuid = s(v, "uuid").map(str::to_string);
        }
    }

    /// A `settings` event for whatever governs the model that changed on this line.
    fn settings_change(&mut self, v: &Value, sink: &mut Sink) -> Option<Event> {
        let t = s(v, "type")?;
        let mut e = Event::new("", Kind::Settings);
        let mut changed = false;
        let mut set = |cur: &mut Option<String>, new: Option<&str>, key: &str, e: &mut Event| {
            if let Some(n) = new
                && cur.as_deref() != Some(n)
            {
                *cur = Some(n.to_string());
                e.body.insert(key.into(), json!(n));
                changed = true;
            }
        };
        if v.get("uuid").is_some() && matches!(t, "user" | "assistant" | "system" | "attachment" | "progress") {
            set(&mut sink.cwd, s(v, "cwd"), "cwd", &mut e);
            // An empty branch (no repository) is no fact until it replaces a known one.
            if sink.branch.is_none() && s(v, "gitBranch") == Some("") {
                sink.branch = Some(String::new());
            }
            set(&mut sink.branch, s(v, "gitBranch"), "git_branch", &mut e);
            set(&mut sink.perm, s(v, "permissionMode"), "permission_mode", &mut e);
            if t == "assistant" {
                let m = v.get("message").and_then(|m| s(m, "model")).filter(|m| *m != "<synthetic>");
                set(&mut sink.model, m, "model", &mut e);
            }
        }
        if t == "permission-mode" {
            set(&mut sink.perm, s(v, "permissionMode"), "permission_mode", &mut e);
        }
        if t == "attachment"
            && let Some(a) = v.get("attachment").filter(|a| s(a, "type") == Some("prompt_snapshot"))
        {
            let sp = a.get("systemPrompt").map(|p| match p {
                Value::Array(xs) => xs.iter().filter_map(Value::as_str).collect::<Vec<_>>().join("\n\n"),
                Value::String(x) => x.clone(),
                _ => String::new(),
            });
            if let Some(sp) = sp
                && sink.system_prompt.as_deref() != Some(sp.as_str())
            {
                sink.system_prompt = Some(sp.clone());
                e.body.insert("system_prompt".into(), json!(sp));
                changed = true;
            }
            if let Some(tools) = a.get("tools").and_then(Value::as_array) {
                let names: Vec<Value> = tools.iter().filter_map(|t| t.get("name").cloned()).collect();
                if changed {
                    e.body.insert("tools".into(), Value::Array(names));
                }
            }
        }
        changed.then_some(e)
    }

    fn map(&mut self, v: &Value, sink: &mut Sink) -> Vec<Event> {
        let mut out = Vec::new();
        if let Some(e) = self.settings_change(v, sink) {
            out.push(e);
        }
        if let Some(p) = s(v, "promptId") {
            sink.turn = Some(p.to_string());
        }
        let t = s(v, "type").unwrap_or("");
        match t {
            "user" => self.user(v, &mut out),
            "assistant" => self.assistant(v, &mut out),
            "system" => out.push(self.system(v)),
            "attachment" => {
                if let Some(e) = self.attachment(v) {
                    out.push(e);
                } else if out.is_empty() {
                    out.push(Event::new("", Kind::Native));
                }
            }
            "summary" => out.push(title(s(v, "summary"))),
            "ai-title" => out.push(title(s(v, "aiTitle"))),
            "custom-title" => out.push(title(s(v, "customTitle"))),
            "file-history-snapshot" => {
                let mut e = Event::new("", Kind::FileChange).with("op", "snapshot");
                if let Some(snap) = v.get("snapshot") {
                    e = e.with("snapshot", snap.clone());
                }
                out.push(e);
            }
            "file-history-delta" => {
                out.push(Event::new("", Kind::FileChange).with("op", "edit").with("path", s(v, "trackingPath").map(Value::from).unwrap_or(Value::Null)))
            }
            "cost-state" => out.push(cost_state(v)),
            "continued-in" => {
                if let Some(next) = s(v, "continuedInSessionId") {
                    let a = sink.ledger.agent_mut();
                    if !a.links.iter().any(|l| l.rel == "continued-in" && l.agent.as_deref() == Some(next)) {
                        a.links.push(Link { rel: "continued-in".into(), agent: Some(next.to_string()), event: None, extra: Obj::new() });
                    }
                }
                out.push(Event::new("", Kind::Native));
            }
            _ => {
                if out.is_empty() || t != "permission-mode" {
                    out.push(Event::new("", Kind::Native));
                }
            }
        }
        // A `permission-mode` line whose mode did not change is plain native.
        if out.is_empty() {
            out.push(Event::new("", Kind::Native));
        }
        out
    }

    fn user(&mut self, v: &Value, out: &mut Vec<Event>) {
        let meta = v["isMeta"] == json!(true);
        let transcript_only = v["isVisibleInTranscriptOnly"] == json!(true);
        let role = if meta { "system" } else { "user" };
        let content = v.get("message").and_then(|m| m.get("content"));
        if v["isCompactSummary"] == json!(true) {
            let text = content.map(text_of).unwrap_or_default();
            out.push(Event::new("", Kind::Compaction).with("phase", "summary").with("summary", text));
            return;
        }
        let text_event = |text: &str, out: &mut Vec<Event>| {
            let mut e = Event::new("", Kind::Text).with("role", role).with("text", text);
            if transcript_only {
                e = e.audience(Audience::Record);
            }
            out.push(e);
        };
        match content {
            Some(Value::String(t))
                if !meta
                    && !transcript_only
                    && let Some((peer, network, text)) = peer_unwrap(t) =>
            {
                let mut e = Event::new("", Kind::PeerMessage).with("direction", "in");
                if let Some(p) = peer {
                    e = e.with("peer", p);
                }
                if let Some(n) = network {
                    e = e.with("network", n);
                }
                out.push(e.with("text", text));
            }
            Some(Value::String(t))
                if !meta
                    && !transcript_only
                    && let Some((call, output)) = orphan_result_unwrap(t) =>
            {
                let mut e = Event::new("", Kind::ToolResult);
                if !call.is_empty() {
                    e = e.with("call_id", call);
                }
                out.push(e.with("output", output));
            }
            Some(Value::String(t)) => text_event(t, out),
            Some(Value::Array(blocks)) => {
                for b in blocks {
                    match s(b, "type") {
                        Some("text") => text_event(s(b, "text").unwrap_or(""), out),
                        Some("image") | Some("document") => out.push(media(b, "user")),
                        Some("tool_result") => {
                            let mut e = Event::new("", Kind::ToolResult)
                                .with("call_id", s(b, "tool_use_id").map(Value::from).unwrap_or(Value::Null))
                                .with("output", result_output(b.get("content")));
                            if let Some(err) = b.get("is_error").and_then(Value::as_bool) {
                                e = e.with("is_error", err);
                            }
                            out.push(e);
                        }
                        _ => {
                            self.losses.add("block-native-only", Some(Kind::Native), "a content block type with no neutral meaning");
                            out.push(Event::new("", Kind::Native));
                        }
                    }
                }
            }
            _ => out.push(Event::new("", Kind::Native)),
        }
        // Older layouts have no meta files: the spawning tool_result names the
        // agent (and maybe its type), known from this line on.
        let tur = v.get("toolUseResult");
        if let Some(r) = tur
            && let Some(child) = s(r, "agentId").and_then(|aid| self.g.child_by_aid.get(aid)).cloned()
        {
            for call in tool_result_ids(v) {
                self.g.child_by_tool.entry(call).or_insert_with(|| child.clone());
            }
            let info = self.g.child_info.entry(child).or_insert((None, None));
            if info.0.is_none() {
                info.0 = s(r, "agentType").map(str::to_string);
            }
            if info.1.is_none() {
                info.1 = s(r, "description").map(str::to_string);
            }
        }
        // Subagent ends: a sync agent's tool_result, or an async agent's task
        // notification. A start not written at the call (the agent was not
        // known yet) is written here, before the end.
        for call in tool_result_ids(v) {
            if let Some(child) = self.g.child_by_tool.get(&call).cloned() {
                if !self.g.started.contains(&call) {
                    out.push(self.subagent_start(&call, &child));
                }
                let r = tur.cloned().unwrap_or(Value::Null);
                if s(&r, "status") == Some("async_launched") {
                    continue;
                }
                let mut e = Event::new("", Kind::Subagent).with("phase", "end").with("agent", child).with("call_id", call.clone());
                if let Some(st) = s(&r, "status") {
                    e = e.with("outcome", st);
                }
                if let Some(c) = r.get("content") {
                    let t = text_of(c);
                    if !t.is_empty() {
                        e = e.with("result", t);
                    }
                }
                out.push(e);
            }
        }
        if let Some(Value::String(t)) = content
            && let Some(n) = parse_task_notification(t)
            && let Some(child) = self.g.child_by_aid.get(&n.task_id).cloned()
        {
            let mut e = Event::new("", Kind::Subagent).with("phase", "end").with("agent", child);
            if let Some(c) = n.tool_use_id {
                e = e.with("call_id", c);
            }
            if let Some(st) = n.status {
                e = e.with("outcome", st);
            }
            if let Some(r) = n.result {
                e = e.with("result", r);
            }
            out.push(e);
        }
        // Large outputs persisted to <session>/tool-results/.
        let mut paths: Vec<String> = Vec::new();
        if let Some(p) = tur.and_then(|r| s(r, "persistedOutputPath")) {
            paths.push(p.to_string());
        }
        if let Some(Value::Array(blocks)) = content {
            for b in blocks.iter().filter(|b| s(b, "type") == Some("tool_result")) {
                let t = text_of(b.get("content").unwrap_or(&Value::Null));
                if let Some(rest) = t.split("Full output saved to: ").nth(1) {
                    let p = rest.lines().next().unwrap_or("").trim().to_string();
                    if !p.is_empty() && !paths.contains(&p) {
                        paths.push(p);
                    }
                }
            }
        }
        let calls = tool_result_ids(v);
        for p in paths {
            let key = p.rsplitn(3, '/').collect::<Vec<_>>();
            if key.len() < 3 {
                continue;
            }
            // `.../<sid>/tool-results/<name>` → `<sid>/tool-results/<name>`
            let (name, dir) = (key[0], key[1]);
            if dir != "tool-results" {
                continue;
            }
            let sid = p.trim_end_matches(&format!("/tool-results/{name}")).rsplit('/').next().unwrap_or("");
            let k = format!("{sid}/tool-results/{name}");
            if let Some(&i) = self.g.tool_results.get(&k) {
                self.touched.push(i);
                if self.g.consumed.insert(i) {
                    self.pending_file.push((i, calls.first().cloned(), p.clone()));
                }
            }
        }
    }

    fn assistant(&mut self, v: &Value, out: &mut Vec<Event>) {
        let msg = v.get("message").cloned().unwrap_or(Value::Null);
        let synthetic = s(&msg, "model") == Some("<synthetic>");
        let blocks = msg.get("content").and_then(Value::as_array).cloned().unwrap_or_default();
        if v["isApiErrorMessage"] == json!(true) {
            let mut e = Event::new("", Kind::Error).with("message", text_of(&Value::Array(blocks)));
            if let Some(k) = s(v, "error") {
                e = e.with("kind", k);
            }
            if let Some(st) = v.get("apiErrorStatus").filter(|x| !x.is_null()) {
                e = e.with("status", st.clone());
            }
            out.push(e);
            return;
        }
        for b in &blocks {
            let mut e = match s(b, "type") {
                Some("text") => Event::new("", Kind::Text).with("role", "assistant").with("text", s(b, "text").unwrap_or("")),
                Some("thinking") => {
                    let mut e = Event::new("", Kind::Reasoning).with("text", s(b, "thinking").unwrap_or(""));
                    if let Some(sig) = s(b, "signature").filter(|x| !x.is_empty()) {
                        e = e.with("sealed", json!({"vendor": "anthropic", "kind": "signature", "data": sig}));
                    }
                    e
                }
                Some("redacted_thinking") => Event::new("", Kind::Reasoning)
                    .with("redacted", true)
                    .with("sealed", json!({"vendor": "anthropic", "kind": "encrypted", "data": s(b, "data").unwrap_or("")})),
                Some("tool_use") | Some("server_tool_use") | Some("mcp_tool_use") => {
                    let name = s(b, "name").unwrap_or("");
                    let mut e = Event::new("", Kind::ToolCall).with("call_id", s(b, "id").map(Value::from).unwrap_or(Value::Null)).with("name", name);
                    let input = b.get("input").cloned().unwrap_or(json!({}));
                    let freeform = freeform_text(&input).filter(|_| s(b, "type") == Some("tool_use") && !name.starts_with("mcp__"));
                    let is_freeform = freeform.is_some();
                    match freeform {
                        // How a free-text (custom) tool call is written: the API takes object inputs only.
                        Some(t) => e = e.with("input_text", t).with("tool_type", "custom"),
                        None => e = e.with("input", input),
                    }
                    if is_freeform {
                    } else if let Some(rest) = name.strip_prefix("mcp__") {
                        e = e.with("tool_type", "mcp").with("server", rest.split("__").next().unwrap_or(""));
                    } else if s(b, "type") == Some("server_tool_use") {
                        e = e.with("tool_type", if name == "web_search" { "web_search" } else { "server" });
                    } else if s(b, "type") == Some("mcp_tool_use") {
                        e = e.with("tool_type", "mcp").with("server", s(b, "server_name").unwrap_or(""));
                    } else {
                        e = e.with("tool_type", "function");
                    }
                    e
                }
                Some(t) if t.ends_with("_tool_result") => Event::new("", Kind::ToolResult)
                    .with("call_id", s(b, "tool_use_id").map(Value::from).unwrap_or(Value::Null))
                    .with("output", result_output(b.get("content"))),
                _ => {
                    self.losses.add("block-native-only", Some(Kind::Native), "a content block type with no neutral meaning");
                    Event::new("", Kind::Native)
                }
            };
            if synthetic && e.kind == Kind::Text {
                // Filler the harness wrote itself ("No response requested."); never sent.
                e = e.audience(Audience::Record);
            }
            let call = e.str("call_id").map(str::to_string);
            let is_call = e.kind == Kind::ToolCall;
            out.push(e);
            if is_call && let Some(c) = &call {
                let input = b.get("input").cloned().unwrap_or(Value::Null);
                let typed = (s(&input, "subagent_type").map(str::to_string), s(&input, "description").map(str::to_string));
                self.g.call_input.insert(c.clone(), typed);
                self.g.call_ts.insert(c.clone(), timestamp(v));
                if let Some(child) = self.g.child_by_tool.get(c).cloned() {
                    out.push(self.subagent_start(c, &child));
                }
            }
        }
        if blocks.is_empty() {
            out.push(Event::new("", Kind::Native));
        }
        // Usage once per message, on the line that settles it (SPEC §9.3: an
        // importer is prefix-stable, so nothing here may wait on a later line).
        // Claude writes one line per content block, each repeating the
        // message's usage as it then stood; a line cannot know it is the last,
        // but some can know they are not: a line holding only reasoning whose
        // `stop_reason` is unset, `end_turn` or `tool_use` (Claude Code writes
        // these on thinking lines a reply follows: from 2.1.2xx every line of
        // a message carries its final stop reason), and a line whose
        // `stop_reason` is `tool_use` but holds no tool call. The first other
        // line that carries usage settles the message; that includes a line of
        // reasoning alone with any other stop reason (`max_tokens`), where the
        // message ended on its reasoning. Its id is keyed by the message
        // id, so it is the same however many of the message's lines have been
        // read.
        let mid = msg.get("id").and_then(Value::as_str);
        let reasoning_only = !blocks.is_empty() && blocks.iter().all(|b| matches!(s(b, "type"), Some("thinking" | "redacted_thinking")));
        let calls = blocks.iter().any(|b| matches!(s(b, "type"), Some("tool_use" | "server_tool_use" | "mcp_tool_use")));
        let stop = s(&msg, "stop_reason");
        let not_last = (reasoning_only && matches!(stop, None | Some("end_turn" | "tool_use"))) || (stop == Some("tool_use") && !calls);
        let settles = !not_last && mid.is_none_or(|m| !self.st.usage_of.contains(m));
        if settles
            && !synthetic
            && let Some(u) = msg.get("usage").filter(|u| u.is_object())
        {
            let n = |k: &str| u.get(k).cloned().unwrap_or(Value::Null);
            let total: u64 = ["input_tokens", "output_tokens", "cache_read_input_tokens", "cache_creation_input_tokens"]
                .iter()
                .filter_map(|k| u.get(*k).and_then(Value::as_u64))
                .sum();
            if total > 0 {
                let mut e = Event::new("", Kind::Usage)
                    .with("model", msg.get("model").cloned().unwrap_or(Value::Null))
                    .with("input", n("input_tokens"))
                    .with("output", n("output_tokens"))
                    .with("cache_read", n("cache_read_input_tokens"))
                    .with("cache_write", n("cache_creation_input_tokens"));
                if let Some(r) = u.get("output_tokens_details").and_then(|d| d.get("thinking_tokens")) {
                    e = e.with("reasoning", r.clone());
                }
                if let Some(id) = mid {
                    self.st.usage_of.insert(id.to_string());
                    e.id = format!("{id}#usage");
                }
                out.push(e);
            }
        }
    }

    /// The `subagent` start for the call `call` that spawned `child`, from
    /// what is known of the agent so far, else the call's own input.
    fn subagent_start(&mut self, call: &str, child: &str) -> Event {
        self.g.started.insert(call.to_string());
        let (mut at, mut desc) = self.g.child_info.get(child).cloned().unwrap_or((None, None));
        let (it, id) = self.g.call_input.get(call).cloned().unwrap_or((None, None));
        at = at.or(it);
        desc = desc.or(id);
        let mut e = Event::new("", Kind::Subagent).with("phase", "start").with("agent", child).with("call_id", call);
        // The agent started when it was called, whichever later line first
        // names it: the call was read before, so this stays prefix-stable.
        e.ts = self.g.call_ts.get(call).cloned().flatten();
        if let Some(a) = at {
            e = e.with("agent_type", a);
        }
        if let Some(d) = desc {
            e = e.with("description", d);
        }
        e
    }

    fn system(&mut self, v: &Value) -> Event {
        match s(v, "subtype").unwrap_or("") {
            "compact_boundary" => {
                let m = v.get("compactMetadata").cloned().unwrap_or(Value::Null);
                let mut e = Event::new("", Kind::Compaction).with("phase", "boundary");
                if let Some(t) = s(&m, "trigger") {
                    e = e.with("trigger", t);
                }
                if let Some(x) = m.get("preTokens") {
                    e = e.with("tokens_before", x.clone());
                }
                if let Some(x) = m.get("postTokens") {
                    e = e.with("tokens_after", x.clone());
                }
                if let Some(u) = m.get("preservedMessages").and_then(|p| p.get("uuids")).filter(|u| u.is_array()) {
                    e = e.with("preserved", u.clone());
                }
                e
            }
            "turn_duration" => {
                let mut e = Event::new("", Kind::Turn).with("phase", "end");
                if let Some(d) = v.get("durationMs") {
                    e = e.with("duration_ms", d.clone());
                }
                e
            }
            "api_error" => {
                let err = v.get("error").cloned().unwrap_or(Value::Null);
                let msg = s(&err, "message")
                    .map(str::to_string)
                    .or_else(|| err.get("error").and_then(|e| s(e, "message")).map(str::to_string))
                    .or_else(|| s(v, "content").map(str::to_string))
                    .unwrap_or_default();
                let mut e = Event::new("", Kind::Error).with("kind", "api_error").with("message", msg);
                if let Some(st) = err.get("status").filter(|x| !x.is_null()) {
                    e = e.with("status", st.clone());
                }
                if v.get("retryAttempt").is_some() || v.get("retryInMs").is_some() {
                    e = e.with("retryable", true);
                }
                e
            }
            "informational" if s(v, "level") == Some("error") => Event::new("", Kind::Error).with("message", s(v, "content").unwrap_or("")),
            _ => Event::new("", Kind::Native),
        }
    }

    fn attachment(&mut self, v: &Value) -> Option<Event> {
        let a = v.get("attachment")?;
        match s(a, "type") {
            Some("prompt_snapshot") => None,
            Some("queued_command") if a.get("prompt").is_some_and(Value::is_string) => {
                let prompt = s(a, "prompt").unwrap();
                if let Some(o) = a.get("origin").filter(|o| s(o, "kind") == Some("peer")) {
                    let peer = s(o, "name").or(s(o, "from")).unwrap_or("");
                    return Some(Event::new("", Kind::PeerMessage).with("direction", "in").with("peer", peer).with("text", prompt));
                }
                let role = if a["isMeta"] == json!(true) { "system" } else { "user" };
                Some(Event::new("", Kind::Text).with("role", role).with("text", prompt))
            }
            _ => {
                let rendered = v.get("rendered").and_then(Value::as_array)?;
                let parts: Vec<String> = rendered.iter().map(|r| text_of(r.get("content").unwrap_or(r))).filter(|t| !t.is_empty()).collect();
                if parts.is_empty() {
                    return None;
                }
                Some(Event::new("", Kind::Text).with("role", "system").with("text", parts.join("\n")))
            }
        }
    }
}

/// The text of a tool input written as `{"input_text": <text>}`, the shape a
/// free-text (custom) tool call takes in Claude Code, whose API only takes
/// object inputs.
pub(super) fn freeform_text(input: &Value) -> Option<String> {
    let o = input.as_object().filter(|o| o.len() == 1)?;
    o.get("input_text").and_then(Value::as_str).map(str::to_string)
}

/// A line's time. The epoch is what the exporter writes when no time was
/// known (Claude Code needs a timestamp to load a line), so it reads as unknown.
fn timestamp(v: &Value) -> Option<String> {
    s(v, "timestamp").filter(|t| *t != UNKNOWN_TS).map(str::to_string)
}

fn title(t: Option<&str>) -> Event {
    match t {
        Some(t) => Event::new("", Kind::Title).with("text", t),
        None => Event::new("", Kind::Native),
    }
}

fn cost_state(v: &Value) -> Event {
    let mut e = Event::new("", Kind::Usage).with("cumulative", true);
    if let Some(c) = v.get("totalCostUSD") {
        e = e.with("cost_usd", c.clone());
    }
    if let Some(m) = v.get("modelUsage").and_then(Value::as_object) {
        // A count no model states is unknown, not zero.
        let sum = |k: &str| {
            let xs: Vec<u64> = m.values().filter_map(|u| u.get(k).and_then(Value::as_u64)).collect();
            (m.is_empty() || !xs.is_empty()).then(|| xs.iter().sum::<u64>())
        };
        for (k, native) in [
            ("input", "inputTokens"),
            ("output", "outputTokens"),
            ("cache_read", "cacheReadInputTokens"),
            ("cache_write", "cacheCreationInputTokens"),
            ("reasoning", "thinkingTokens"),
        ] {
            if let Some(n) = sum(native).filter(|_| k != "reasoning" || !m.is_empty()) {
                e = e.with(k, n);
            }
        }
        if m.len() == 1 {
            e = e.with("model", m.keys().next().unwrap().as_str());
        }
    }
    e
}

/// Flatten message content (string or blocks) to its text.
pub(super) fn text_of(c: &Value) -> String {
    match c {
        Value::String(t) => t.clone(),
        Value::Array(a) => a
            .iter()
            .filter_map(|b| match b {
                Value::String(t) => Some(t.clone()),
                _ if s(b, "type").is_none_or(|t| t == "text") => s(b, "text").map(str::to_string),
                _ => None,
            })
            .collect::<Vec<_>>()
            .join("\n"),
        _ => String::new(),
    }
}

fn media(b: &Value, role: &str) -> Event {
    let src = b.get("source").cloned().unwrap_or(Value::Null);
    let mut e = Event::new("", Kind::Media).with("role", role);
    let mt = s(&src, "media_type").map(str::to_string).or_else(|| (s(b, "type") == Some("document")).then(|| "application/pdf".to_string()));
    if let Some(mt) = mt {
        e = e.with("media_type", mt);
    }
    match s(&src, "type") {
        Some("base64") => e = e.with("data", s(&src, "data").unwrap_or("")),
        Some("url") => e = e.with("url", s(&src, "url").unwrap_or("")),
        Some("text") => e = e.with("data", base64::engine::general_purpose::STANDARD.encode(s(&src, "data").unwrap_or(""))),
        _ => {}
    }
    if let Some(t) = s(b, "title") {
        e = e.with("name", t);
    }
    e
}

/// tool_result content → SPEC output: a string, or `{type:text}` / `{type:media}` parts.
fn result_output(c: Option<&Value>) -> Value {
    match c {
        None | Some(Value::Null) => json!(""),
        Some(Value::String(t)) => json!(t),
        Some(Value::Array(a)) => Value::Array(
            a.iter()
                .map(|b| match s(b, "type") {
                    Some("text") => json!({"type": "text", "text": s(b, "text").unwrap_or("")}),
                    Some("image") | Some("document") => {
                        let m = media(b, "tool");
                        let mut o = Map::new();
                        o.insert("type".into(), json!("media"));
                        for k in ["media_type", "data", "url", "name"] {
                            if let Some(x) = m.body.get(k) {
                                o.insert(k.into(), x.clone());
                            }
                        }
                        Value::Object(o)
                    }
                    _ => b.clone(),
                })
                .collect(),
        ),
        Some(other) => other.clone(),
    }
}

pub(super) struct TaskNotification {
    pub task_id: String,
    pub tool_use_id: Option<String>,
    pub status: Option<String>,
    pub result: Option<String>,
}

fn tag<'a>(t: &'a str, name: &str) -> Option<&'a str> {
    let open = format!("<{name}>");
    let close = format!("</{name}>");
    let start = t.find(&open)? + open.len();
    let end = t[start..].find(&close)? + start;
    Some(&t[start..end])
}

pub(super) fn parse_task_notification(t: &str) -> Option<TaskNotification> {
    let body = tag(t.trim_start(), "task-notification")?;
    Some(TaskNotification {
        task_id: tag(body, "task-id")?.to_string(),
        tool_use_id: tag(body, "tool-use-id").map(str::to_string),
        status: tag(body, "status").map(str::to_string),
        result: tag(body, "result").map(str::to_string),
    })
}
