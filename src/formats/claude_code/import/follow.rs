//! Following one growing transcript (SPEC §9.3): the lines appended to it are
//! split, parsed and mapped on their own, with the state the mapping of the
//! earlier lines left, and yield exactly what a whole re-import would add.
//!
//! The import is prefix-stable for the followed file's own lines (SPEC §9.3),
//! but a new line can still change what came before in a few places. Each is
//! checked before anything is yielded, and any of them ends the follow
//! (`None`: sync re-imports the location whole):
//!
//! - **the `parentUuid` tree**: a new line an earlier line names as its parent
//!   changes that line's conversation (and so its rewind `after`).
//! - **sidecar files**: a new line references a file another transcript
//!   consumed (the whole import maps this transcript first, so it would take
//!   the file).
//! - **headers and variant**: any agent's header, or the import's variant,
//!   differs from what was yielded (the first line's time, a carried header,
//!   an inline sidechain's line list, a structural parent; a new root shows
//!   as some header's parent going away). A tool call that reuses another
//!   transcript's call id changes only a subagent's parent event, a header.
//!
//! The location's other files (a session's sidecar directory) are not
//! followed here: sync re-imports whole when they change.

use super::*;
use crate::formats::Follow;

pub(in crate::formats::claude_code) fn follow(input: &Path, opts: &ImportOptions) -> Option<Result<(Imported, Box<dyn Follow>)>> {
    if !input.is_file() {
        return None;
    }
    let mut files = match collect(input) {
        Ok(f) => f,
        Err(e) => return Some(Err(e)),
    };
    let name = input.file_name()?.to_str()?.to_string();
    let main = files.entries.iter().position(|e| e.rel == name)?;
    let role = files.entries[main].role.clone();
    let transcripts = files.entries.iter().filter(|e| matches!(e.role, Role::Session | Role::SidecarAgent { .. } | Role::FlatAgent { .. })).count();
    match role {
        Role::Session => {}
        // A lone agent transcript; next to others, it would not be read first.
        Role::FlatAgent { .. } | Role::SidecarAgent { .. } if transcripts == 1 => {}
        _ => return None,
    }
    // A final line with no newline yet is held.
    let bytes = std::mem::take(&mut files.entries[main].bytes);
    let cut = bytes.iter().rposition(|b| *b == b'\n').map_or(0, |i| i + 1);
    let held = bytes[cut..].to_vec();
    files.entries[main].bytes = bytes[..cut].to_vec();
    let main_text_inline = inline_text(&files.entries[main].bytes);
    let mut r = match begin_files(files, opts) {
        Ok(r) => r,
        Err(e) => return Some(Err(e)),
    };
    let (lines_n, main_inline) = {
        let (lines, _) = &r.parsed[&main];
        (lines.len(), lines.iter().any(|l| l.value.as_ref().is_some_and(is_inline)))
    };
    let mut live = None;
    let mut others: Vec<Ledger> = Vec::new();
    let mut consumed_by_main = HashSet::new();
    for i in r.order() {
        let m = r.transcript(i);
        if i == main {
            consumed_by_main = r.g.consumed.clone();
            live = Some(m);
        } else {
            others.push(m.main.finish());
            for c in m.children {
                others.push(c.finish());
            }
        }
    }
    let Mapped { main: sink, children, st } = live?;
    let by_others: HashSet<usize> = r.g.consumed.difference(&consumed_by_main).copied().collect();
    // The import, exactly as `import` ends it.
    let finished = |s: &Sink| {
        let mut l = s.ledger.clone();
        *l.agent_mut() = s.header();
        l
    };
    let mut ledgers: Vec<Ledger> = vec![finished(&sink)];
    ledgers.extend(children.iter().map(finished));
    ledgers.extend(others.iter().cloned());
    attach_unconsumed(&r.files, &r.g, &mut ledgers, &mut r.losses, r.variant);
    // Headers as the structural pass first sees them: the fixed ones are
    // every other transcript's, and a `claude-code-files` ledger.
    let fixed: Vec<Agent> = ledgers[1 + children.len()..].iter().map(|l| l.agent().clone()).collect();
    link_parents(&mut ledgers, &r.g);
    let roots = ledgers.iter().filter(|l| l.agent().parent.is_none()).count();
    link_forks(&mut ledgers);
    let headers: BTreeMap<String, Agent> = ledgers.iter().map(|l| (l.id().to_string(), l.agent().clone())).collect();
    let mut set = LedgerSet::new();
    for l in ledgers {
        set.insert(l);
    }
    // Forks are linked across roots by their events; a location with more
    // than one root is not followed.
    if roots > 1 {
        return None;
    }
    let imported = Imported { set, losses: r.losses.clone(), variant: r.variant.to_string() };
    let Run { mut files, g, losses, variant, .. } = r;
    files.entries[main].bytes.truncate(SNIFF_BYTES);
    let mut dangling = HashSet::new();
    for n in st.tree.nodes.values() {
        if let Some(p) = &n.parent
            && !st.tree.nodes.contains_key(p)
        {
            dangling.insert(p.clone());
        }
    }
    let yielded_children = children.iter().map(|c| c.ledger.events.len()).collect();
    let f = Following {
        rel: name,
        main,
        is_session: role == Role::Session,
        forced: opts.variant.clone(),
        yielded_main: sink.ledger.events.len(),
        yielded_children,
        files,
        g,
        losses,
        st,
        sink,
        children,
        fixed,
        by_others,
        lines: lines_n,
        offset: cut,
        held,
        main_inline,
        main_text_inline,
        dangling,
        headers,
        variant: variant.to_string(),
        spent: false,
    };
    Some(Ok((imported, Box::new(f))))
}

struct Following {
    /// The followed file's name, and its entry among the location's files
    /// (whose bytes are now only its first [`SNIFF_BYTES`], for the variant).
    rel: String,
    main: usize,
    is_session: bool,
    forced: Option<String>,
    files: Files,
    g: Global,
    losses: Losses,
    st: FileState,
    /// The followed file's ledger and its inline sidechains.
    sink: Sink,
    children: Vec<Sink>,
    /// How many events of each were yielded.
    yielded_main: usize,
    yielded_children: Vec<usize>,
    /// The other transcripts' headers before the structural pass.
    fixed: Vec<Agent>,
    /// Sidecar files another transcript consumed.
    by_others: HashSet<usize>,
    /// Lines read, the offset of the next byte to read, and a final line still without its newline.
    lines: usize,
    offset: usize,
    held: Vec<u8>,
    /// The file holds an inline sidechain line (parsed), or text that looks like one.
    main_inline: bool,
    main_text_inline: bool,
    /// `parentUuid`s no line of the file has yet.
    dangling: HashSet<String>,
    /// What was yielded: every agent's header, and the import's variant.
    headers: BTreeMap<String, Agent>,
    variant: String,
    spent: bool,
}

impl Follow for Following {
    fn feed(&mut self, appended: &[u8]) -> Result<Option<LedgerSet>> {
        if self.spent {
            return Ok(None);
        }
        self.held.extend_from_slice(appended);
        let Some(cut) = self.held.iter().rposition(|b| *b == b'\n') else {
            return Ok(Some(LedgerSet::new()));
        };
        let chunk: Vec<u8> = self.held.drain(..=cut).collect();
        let out = self.read(&chunk);
        if out.is_none() {
            self.spent = true;
        }
        Ok(out)
    }
    fn len(&self) -> u64 {
        (self.offset + self.held.len()) as u64
    }
}

/// `header` with `events`, which keep the `seq` they have.
fn delta(header: &Agent, events: &[Event]) -> Ledger {
    let mut l = Ledger::new(header.clone());
    for e in events {
        let seq = e.seq;
        l.append(e.clone()).expect("ids are unique within a ledger");
        l.events.last_mut().unwrap().seq = seq;
    }
    l
}

impl Following {
    fn read(&mut self, chunk: &[u8]) -> Option<LedgerSet> {
        let (mut lines, _) = split_lines(chunk);
        for l in &mut lines {
            l.offset += self.offset;
        }
        let base = self.lines;
        let head = &mut self.files.entries[self.main].bytes;
        if head.len() < SNIFF_BYTES {
            let room = SNIFF_BYTES - head.len();
            head.extend_from_slice(&chunk[..chunk.len().min(room)]);
        }
        self.main_text_inline |= inline_text(chunk);
        self.main_inline |= lines.iter().any(|l| l.value.as_ref().is_some_and(is_inline));

        // What the new lines would change about the earlier ones.
        for l in &lines {
            let Some(v) = &l.value else { continue };
            // A parent an earlier line named.
            if s(v, "uuid").is_some_and(|u| self.dangling.contains(u)) {
                return None;
            }
        }

        let mut fi = FileImport {
            rel: self.rel.clone(),
            g: &mut self.g,
            files: &self.files,
            losses: &mut self.losses,
            st: std::mem::take(&mut self.st),
            pending_file: vec![],
            touched: vec![],
        };
        fi.prepare(&lines, base);
        fi.map_lines(&lines, base, &mut self.sink, &mut self.children, self.is_session);
        let touched = std::mem::take(&mut fi.touched);
        self.st = std::mem::take(&mut fi.st);
        self.lines += lines.len();
        self.offset += chunk.len();

        if touched.iter().any(|i| self.by_others.contains(i)) {
            return None;
        }
        for l in &lines {
            let Some(v) = &l.value else { continue };
            if let Some(p) = s(v, "parentUuid")
                && !self.st.tree.nodes.contains_key(p)
            {
                self.dangling.insert(p.to_string());
            }
        }

        let variant = import_variant(&self.files, self.forced.as_deref(), Some((self.main, self.main_text_inline)));
        if variant != self.variant {
            return None;
        }
        let headers = self.headers(variant);
        for (id, h) in &self.headers {
            if headers.get(id) != Some(h) {
                return None;
            }
        }

        // (Ids are the finished headers': a carried header names its own agent.)
        let mut set = LedgerSet::new();
        set.insert(delta(&headers[&self.sink.header().id], &self.sink.ledger.events[self.yielded_main..]));
        self.yielded_main = self.sink.ledger.events.len();
        for (k, c) in self.children.iter().enumerate() {
            let from = self.yielded_children.get(k).copied().unwrap_or(0);
            set.insert(delta(&headers[&c.header().id], &c.ledger.events[from..]));
        }
        self.yielded_children = self.children.iter().map(|c| c.ledger.events.len()).collect();
        self.headers = headers;
        Some(set)
    }

    /// Every agent's header as a whole import would end it.
    fn headers(&mut self, variant: &'static str) -> BTreeMap<String, Agent> {
        if self.is_session {
            let fv = if self.main_inline { V_INLINE } else { variant };
            if let Some(src) = self.sink.ledger.agent_mut().source.as_mut() {
                src.variant = Some(fv.into());
            }
        }
        set_session(&mut self.sink, self.st.first_sid.as_deref());
        for c in self.children.iter_mut() {
            let lines = c.inline_lines.clone();
            set_inline(c, &self.rel, lines);
        }
        let mut ledgers: Vec<Ledger> = vec![Ledger::new(self.sink.header())];
        ledgers.extend(self.children.iter().map(|c| Ledger::new(c.header())));
        ledgers.extend(self.fixed.iter().cloned().map(Ledger::new));
        link_parents(&mut ledgers, &self.g);
        ledgers.into_iter().map(|l| (l.id().to_string(), l.header.agent)).collect()
    }
}
