//! Following one growing JSONL rollout (SPEC §9.3): the lines appended to it
//! are parsed and mapped on their own, with the state the earlier lines left,
//! and yield exactly what a whole re-import would add.
//!
//! The import is prefix-stable (SPEC §9.3): no line changes an earlier
//! line's events. A response's output items are grouped as they are read, and
//! a UI record is settled by what was read before it or, if it waits for its
//! item, by an event of its own when the response ends. A change of the header
//! (a carried one, or the linker's parent and fork links) ends the follow
//! (`None`: sync re-imports the location whole). A rollback's `after` needs
//! only the events before it.
//!
//! Only `.jsonl` rollouts are followed: a `.jsonl.zst` or a TypeScript-CLI
//! `.json` document is rewritten whole, never appended to. The subagent
//! rollouts a thread spawns are separate files, each its own location (a
//! binding follows the file it names; finding the others would be a scan,
//! which sync never does).

use super::*;
use crate::formats::Follow;

pub(in crate::formats::codex) fn follow(input: &Path, opts: &ImportOptions) -> Option<Result<(Imported, Box<dyn Follow>)>> {
    if !input.is_file() {
        return None;
    }
    let rel = input.file_name()?.to_str()?.to_string();
    if !rel.ends_with(".jsonl") {
        return None;
    }
    let bytes = match std::fs::read(input) {
        Ok(b) => b,
        Err(e) => return Some(Err(e.into())),
    };
    // A final line with no newline yet is held.
    let cut = bytes.iter().rposition(|b| *b == b'\n').map_or(0, |i| i + 1);
    let text = std::str::from_utf8(&bytes[..cut]).ok()?;
    let variant = match &opts.variant {
        Some(v) => canonical_variant(v)?,
        None => sniff(&rel, text)?.0,
    };
    if variant == V_TS {
        return None;
    }
    let mut ctx = Ctx::new(variant, opts);
    let mut losses = Losses::default();
    for (off, line) in split_lines(text).0 {
        read_line(&mut ctx, off, line, &mut losses);
    }
    let out = Ledger::new(linked_header(&ctx, &rel, &HashMap::new()));
    let mut f = Following { rel, ctx, losses, offset: cut, held: bytes[cut..].to_vec(), out, turns: None, spawns: HashMap::new(), spent: false };
    let events = f.ctx.events.clone();
    f.append(events);
    *f.out.agent_mut() = f.header();
    let mut set = LedgerSet::new();
    set.insert(f.out.clone());
    let imported = Imported { set, losses: f.losses.clone(), variant: variant.to_string() };
    Some(Ok((imported, Box::new(f))))
}

struct Following {
    /// The file's name.
    rel: String,
    /// The mapping's state.
    ctx: Ctx,
    losses: Losses,
    /// The offset of the next byte to read, and a final line still without its newline.
    offset: usize,
    held: Vec<u8>,
    /// The ledger as yielded: rollbacks placed, header linked.
    out: Ledger,
    /// [`rollback_step`]'s turns, from the first rollback on (none before it).
    turns: Option<Vec<usize>>,
    /// child thread id → the event that spawned it.
    spawns: HashMap<String, String>,
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

/// The ledger's header as `import` ends it for a lone file: the file's, then linked.
fn linked_header(ctx: &Ctx, rel: &str, spawns: &HashMap<String, String>) -> Agent {
    let mut f = FileLedger {
        ledger: Ledger::new(header_of(ctx, rel, jsonl_extra(rel, true))),
        variant: ctx.variant,
        restored_header: ctx.restored_agent.is_some(),
        thread_id: ctx.header.thread_id.clone(),
        parent_thread_id: ctx.header.parent_thread_id.clone(),
        forked_from_id: ctx.header.forked_from_id.clone(),
        spawns: spawns.clone(),
    };
    link_agents(std::slice::from_mut(&mut f));
    f.ledger.header.agent
}

impl Following {
    fn header(&self) -> Agent {
        linked_header(&self.ctx, &self.rel, &self.spawns)
    }

    /// Append `events` to the yielded ledger, placing rollbacks; returns where they start.
    fn append(&mut self, events: Vec<Event>) -> usize {
        let first = self.out.events.len();
        for e in events {
            self.out.append_unique(e);
        }
        for i in first..self.out.events.len() {
            if self.turns.is_none() && is_rollback(&self.out.events[i]) {
                let mut turns = Vec::new();
                for j in 0..i {
                    rollback_step(&mut self.out, j, &mut turns);
                }
                self.turns = Some(turns);
            }
            if let Some(turns) = self.turns.as_mut() {
                rollback_step(&mut self.out, i, turns);
            }
            let e = &self.out.events[i];
            if e.kind == Kind::Subagent
                && e.str("phase") == Some("start")
                && let Some(child) = e.str("agent")
            {
                self.spawns.entry(child.to_string()).or_insert_with(|| e.id.clone());
            }
        }
        first
    }

    fn read(&mut self, chunk: &[u8]) -> Option<LedgerSet> {
        let text = std::str::from_utf8(chunk).ok()?;
        let old_len = self.ctx.events.len();
        for (off, line) in split_lines(text).0 {
            read_line(&mut self.ctx, self.offset + off, line, &mut self.losses);
        }
        self.offset += chunk.len();
        let events = self.ctx.events[old_len..].to_vec();
        let first = self.append(events);
        if self.header() != *self.out.agent() {
            return None;
        }
        let mut l = Ledger::new(self.out.agent().clone());
        for e in &self.out.events[first..] {
            l.append(e.clone()).expect("ids are unique within a ledger");
            l.events.last_mut().unwrap().seq = e.seq;
        }
        Some(LedgerSet::single(l))
    }
}
