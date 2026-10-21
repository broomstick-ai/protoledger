//! Neutral comparison of two ledger sets: what a round trip lost (SPEC §7.1).
//!
//! Events are aligned by id first, then usage by response group. Whatever is
//! left is aligned by a longest common subsequence over a signature that keys
//! text by role and content (falling back to role alone between those
//! anchors), so a cross-format trip that re-minted every id still lines up and
//! one dropped text does not shift every later pairing. Usage the target
//! re-grouped is then paired by the response it follows.
//!
//! A native record marked `altered` (SPEC §4) is never exact: it is counted in
//! [`Diff::altered`], not as a loss. Exactness of a native record is decided on
//! its exact text, never on parsed JSON (which cannot see byte drift).

use crate::ledger::{Ledger, LedgerSet};
use crate::model::{Event, Kind, Native};
use crate::util::norm_ts;
use serde::{Deserialize, Serialize};
use serde_json::Value;
use std::collections::{BTreeMap, HashMap};

#[derive(Debug, Clone, Copy, Default)]
pub struct CompareOptions {
    /// Do not count a missing `native` record as a loss.
    pub ignore_native: bool,
    /// Do not compare `meta`.
    pub ignore_meta: bool,
    /// Do not compare `ts`.
    pub ignore_ts: bool,
}

#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
pub struct Diff {
    pub agents_a: usize,
    pub agents_b: usize,
    #[serde(skip_serializing_if = "Vec::is_empty", default)]
    pub agents_missing: Vec<String>,
    #[serde(skip_serializing_if = "Vec::is_empty", default)]
    pub agents_extra: Vec<String>,
    /// `header.<field>` → number of agents where it differs.
    #[serde(skip_serializing_if = "BTreeMap::is_empty", default)]
    pub header: BTreeMap<String, u64>,
    pub events_a: u64,
    pub events_b: u64,
    pub matched: u64,
    #[serde(skip_serializing_if = "BTreeMap::is_empty", default)]
    pub missing: BTreeMap<String, u64>,
    /// Facts in the missing events: each counts 1 plus its non-empty body
    /// fields, so dropping an event never scores better than keeping part of it.
    #[serde(skip_serializing_if = "is_zero", default)]
    pub missing_facts: u64,
    #[serde(skip_serializing_if = "BTreeMap::is_empty", default)]
    pub extra: BTreeMap<String, u64>,
    /// `<kind>.<field>` → number of matched events where it differs.
    #[serde(skip_serializing_if = "BTreeMap::is_empty", default)]
    pub changed: BTreeMap<String, u64>,
    /// Harness-only records (kind `native`) that did not cross. Expected when
    /// the target is another harness; reported, not scored.
    #[serde(skip_serializing_if = "is_zero", default)]
    pub native_only_dropped: u64,
    /// Turn markers the target added. Structure, not a lost fact; not scored.
    #[serde(skip_serializing_if = "is_zero", default)]
    pub turns_added: u64,
    /// Missing `settings` events whose body held nothing: no fact was lost
    /// with them. Reported, not scored.
    #[serde(skip_serializing_if = "is_zero", default)]
    pub empty_dropped: u64,
    /// Matched events whose native record is marked `altered` (SPEC §4): no
    /// longer the source's bytes, so neither exact nor a loss. Reported, not
    /// scored, and a diff holding any is not [`Diff::is_exact`].
    #[serde(skip_serializing_if = "is_zero", default)]
    pub altered: u64,
}

fn is_zero(n: &u64) -> bool {
    *n == 0
}

impl Diff {
    /// One number to track: everything that did not survive.
    pub fn score(&self) -> u64 {
        self.agents_missing.len() as u64
            + self.agents_extra.len() as u64
            + self.header.values().sum::<u64>()
            + self.missing_facts
            + self.extra.values().sum::<u64>()
            + self.changed.values().sum::<u64>()
    }

    /// Nothing was lost (an altered record is not a loss).
    pub fn is_clean(&self) -> bool {
        self.score() == 0
    }

    /// Nothing was lost and no matched native record is `altered`: the only
    /// state in which a trip may be called exact (SPEC §4).
    pub fn is_exact(&self) -> bool {
        self.is_clean() && self.altered == 0
    }

    pub fn summary(&self) -> String {
        if self.is_exact() {
            return format!("clean: {} agents, {} events", self.agents_a, self.events_a);
        }
        if self.is_clean() {
            return format!("clean but not exact: {} agents, {} events; altered {}", self.agents_a, self.events_a, self.altered);
        }
        let mut parts = vec![format!("score {}", self.score())];
        let fmt = |m: &BTreeMap<String, u64>| m.iter().map(|(k, v)| format!("{k}×{v}")).collect::<Vec<_>>().join(", ");
        if !self.agents_missing.is_empty() {
            parts.push(format!("agents missing {}", self.agents_missing.len()));
        }
        if !self.agents_extra.is_empty() {
            parts.push(format!("agents extra {}", self.agents_extra.len()));
        }
        if !self.header.is_empty() {
            parts.push(format!("header [{}]", fmt(&self.header)));
        }
        if !self.missing.is_empty() {
            parts.push(format!("missing [{}]", fmt(&self.missing)));
        }
        if !self.extra.is_empty() {
            parts.push(format!("extra [{}]", fmt(&self.extra)));
        }
        if !self.changed.is_empty() {
            parts.push(format!("changed [{}]", fmt(&self.changed)));
        }
        if self.altered > 0 {
            parts.push(format!("altered {}", self.altered));
        }
        parts.join("; ")
    }
}

pub fn compare(a: &LedgerSet, b: &LedgerSet, opts: CompareOptions) -> Diff {
    let mut d = Diff { agents_a: a.len(), agents_b: b.len(), ..Default::default() };
    // Pair agents by id; pair the leftovers in order (roots first, then by creation).
    let mut pairs: Vec<(&Ledger, &Ledger)> = Vec::new();
    let mut left_a: Vec<&Ledger> = Vec::new();
    let mut used_b: Vec<&str> = Vec::new();
    for la in a.iter() {
        match b.get(la.id()) {
            Some(lb) => {
                pairs.push((la, lb));
                used_b.push(lb.id());
            }
            None => left_a.push(la),
        }
    }
    let mut left_b: Vec<&Ledger> = b.iter().filter(|l| !used_b.contains(&l.id())).collect();
    let order = |l: &&Ledger| (l.agent().parent.is_some(), l.agent().created_at.clone(), l.events.len());
    left_a.sort_by_key(order);
    left_b.sort_by_key(order);
    // Pair renamed agents by what they say (shared text), best overlap first;
    // whatever shares nothing pairs in order.
    let texts = |l: &Ledger| -> std::collections::HashSet<String> {
        l.events.iter().filter(|e| e.kind == Kind::Text).filter_map(|e| e.str("text").map(|t| t.trim().to_string())).collect()
    };
    let ta: Vec<_> = left_a.iter().map(|l| texts(l)).collect();
    let tb: Vec<_> = left_b.iter().map(|l| texts(l)).collect();
    let mut cand: Vec<(usize, usize, usize)> = Vec::new();
    for (i, x) in ta.iter().enumerate() {
        for (j, y) in tb.iter().enumerate() {
            let n = x.intersection(y).count();
            if n > 0 {
                cand.push((n, i, j));
            }
        }
    }
    cand.sort_by(|p, q| q.0.cmp(&p.0).then(p.1.cmp(&q.1)).then(p.2.cmp(&q.2)));
    let (mut done_a, mut done_b) = (vec![false; left_a.len()], vec![false; left_b.len()]);
    for (_, i, j) in cand {
        if !done_a[i] && !done_b[j] {
            done_a[i] = true;
            done_b[j] = true;
            pairs.push((left_a[i], left_b[j]));
        }
    }
    let rest_a: Vec<&Ledger> = left_a.iter().enumerate().filter(|(i, _)| !done_a[*i]).map(|(_, l)| *l).collect();
    let rest_b: Vec<&Ledger> = left_b.iter().enumerate().filter(|(j, _)| !done_b[*j]).map(|(_, l)| *l).collect();
    let (left_a, left_b) = (rest_a, rest_b);
    let n = left_a.len().min(left_b.len());
    for i in 0..n {
        pairs.push((left_a[i], left_b[i]));
    }
    for la in &left_a[n..] {
        // With natives ignored, a store container of native records alone
        // (which no format but its own renders) holds nothing to lose.
        if opts.ignore_native && la.events.iter().all(|e| e.kind == Kind::Native) && crate::formats::is_store_container(la, a) {
            d.events_a += la.events.len() as u64;
            d.native_only_dropped += la.events.len() as u64;
            continue;
        }
        d.agents_missing.push(la.id().to_string());
        d.events_a += la.events.len() as u64;
        for e in &la.events {
            note_missing(e, &mut d);
        }
    }
    for lb in &left_b[n..] {
        d.agents_extra.push(lb.id().to_string());
        d.events_b += lb.events.len() as u64;
        for e in &lb.events {
            *d.extra.entry(e.kind.to_string()).or_default() += 1;
        }
    }
    for (la, lb) in pairs {
        compare_header(la, lb, &mut d);
        compare_events(&la.events, &lb.events, opts, &mut d);
    }
    d
}

fn compare_header(a: &Ledger, b: &Ledger, d: &mut Diff) {
    let (x, y) = (a.agent(), b.agent());
    let mut bump = |k: &str| *d.header.entry(format!("header.{k}")).or_default() += 1;
    // Gaining information is not a loss; losing or changing it is.
    if x.created_at.is_some() && x.created_at.as_deref().map(norm_ts) != y.created_at.as_deref().map(norm_ts) {
        bump("created_at");
    }
    if x.parent.as_ref().map(|p| p.relation) != y.parent.as_ref().map(|p| p.relation) {
        bump("parent");
    }
    if x.links.len() != y.links.len() {
        bump("links");
    }
    if x.meta != y.meta {
        bump("meta");
    }
}

/// A coarse signature for alignment when ids differ: the kind plus its role,
/// call id or phase. Every text of one role looks alike under it.
fn coarse_signature(e: &Event) -> String {
    let k = e.kind.as_str();
    match e.kind {
        Kind::Text | Kind::Media => format!("{k}:{}", e.str("role").unwrap_or("")),
        Kind::ToolCall | Kind::ToolResult => format!("{k}:{}", e.str("call_id").unwrap_or("")),
        Kind::Compaction | Kind::Turn | Kind::Subagent => format!("{k}:{}", e.str("phase").unwrap_or("")),
        _ => k.to_string(),
    }
}

/// How many characters of normalised text key a text's alignment.
const CONTENT_KEY_CHARS: usize = 64;

/// A text's content key: its first [`CONTENT_KEY_CHARS`] characters, with
/// whitespace runs collapsed to one space and the ends trimmed, so a target
/// that re-wraps a message or changes its tail still lines up.
fn content_key(text: &str) -> String {
    let mut out = String::new();
    let mut n = 0;
    for w in text.split_whitespace() {
        if n > 0 {
            out.push(' ');
            n += 1;
        }
        for c in w.chars() {
            if n >= CONTENT_KEY_CHARS {
                return out;
            }
            out.push(c);
            n += 1;
        }
        if n >= CONTENT_KEY_CHARS {
            break;
        }
    }
    out
}

/// The alignment signature: the coarse one, plus a content key for text and
/// peer messages (whose "role" is their direction).
fn signature(e: &Event) -> String {
    match e.kind {
        Kind::Text => format!("{}\u{1f}{}", coarse_signature(e), content_key(e.str("text").unwrap_or(""))),
        Kind::PeerMessage => format!("{}:{}\u{1f}{}", coarse_signature(e), e.str("direction").unwrap_or(""), content_key(e.str("text").unwrap_or(""))),
        _ => coarse_signature(e),
    }
}

fn compare_events(a: &[Event], b: &[Event], opts: CompareOptions, d: &mut Diff) {
    d.events_a += a.len() as u64;
    d.events_b += b.len() as u64;
    // 1. by id
    let b_index: HashMap<&str, usize> = b.iter().enumerate().map(|(i, e)| (e.id.as_str(), i)).collect();
    let mut a_match: Vec<Option<usize>> = vec![None; a.len()];
    let mut b_used = vec![false; b.len()];
    for (i, e) in a.iter().enumerate() {
        if let Some(&j) = b_index.get(e.id.as_str())
            && a[i].kind == b[j].kind
        {
            a_match[i] = Some(j);
            b_used[j] = true;
        }
    }
    // 1b. usage by response group: where it sits around the tool results is a
    // convention detail, not a fact (SPEC §3.1.1).
    let mut by_group: HashMap<&str, Vec<usize>> = HashMap::new();
    for (j, e) in b.iter().enumerate() {
        if !b_used[j]
            && e.kind == Kind::Usage
            && let Some(g) = &e.group
        {
            by_group.entry(g.as_str()).or_default().push(j);
        }
    }
    for (i, e) in a.iter().enumerate() {
        if a_match[i].is_none()
            && e.kind == Kind::Usage
            && let Some(js) = e.group.as_deref().and_then(|g| by_group.get_mut(g))
            && let Some(pos) = js.iter().position(|j| !b_used[*j])
        {
            let j = js.remove(pos);
            a_match[i] = Some(j);
            b_used[j] = true;
        }
    }
    // 1c. a tool result, and a `subagent` record, by the call it answers:
    // a result's place among the other results of a parallel call, and
    // a derived subagent record's place beside the result (a source can only
    // write it once a later line names the agent, SPEC §9.3), is the source's
    // order, not a fact. Only a call id each side holds once per kind (and
    // phase).
    let call_key = |e: &Event| -> Option<String> {
        let c = e.str("call_id").filter(|c| !c.is_empty())?;
        match e.kind {
            Kind::ToolResult => Some(format!("result\u{1f}{c}")),
            Kind::Subagent => Some(format!("subagent:{}\u{1f}{c}", e.str("phase").unwrap_or(""))),
            _ => None,
        }
    };
    let results = |evs: &[Event], used: &dyn Fn(usize) -> bool| -> HashMap<String, Vec<usize>> {
        let mut m: HashMap<String, Vec<usize>> = HashMap::new();
        for (i, e) in evs.iter().enumerate() {
            if !used(i)
                && let Some(k) = call_key(e)
            {
                m.entry(k).or_default().push(i);
            }
        }
        m
    };
    let ra_by_call = results(a, &|i| a_match[i].is_some());
    let rb_by_call = results(b, &|j| b_used[j]);
    let (mut by_call_a, mut by_call_b) = (vec![false; a.len()], vec![false; b.len()]);
    for (c, is) in &ra_by_call {
        if let (&[i], Some(&[j])) = (is.as_slice(), rb_by_call.get(c).map(Vec::as_slice)) {
            a_match[i] = Some(j);
            b_used[j] = true;
            (by_call_a[i], by_call_b[j]) = (true, true);
        }
    }
    // 2. LCS over what is left, in order. Harness-only records never align
    // across harnesses; keep them out so they cannot displace real matches.
    // Usage is kept out too: it is paired by the response it follows (3).
    // The results paired by their call stay in, each pair its own token, so
    // they anchor the alignment where they did before (1c); the subagent
    // records paired by their call are records, and stay out.
    let token = |e: &Event, by_call: bool| by_call && e.kind == Kind::ToolResult;
    let ra: Vec<usize> =
        (0..a.len()).filter(|&i| (a_match[i].is_none() || token(&a[i], by_call_a[i])) && !matches!(a[i].kind, Kind::Native | Kind::Usage)).collect();
    let rb: Vec<usize> = (0..b.len()).filter(|&j| (!b_used[j] || token(&b[j], by_call_b[j])) && !matches!(b[j].kind, Kind::Native | Kind::Usage)).collect();
    align(a, b, &ra, &rb, &by_call_a, &by_call_b, &mut a_match, &mut b_used);
    // 3. usage the target re-grouped (a cross-format trip re-mints group ids),
    // by the matched response it follows.
    pair_usage_by_response(a, b, &mut a_match, &mut b_used);
    // 4. whatever usage is left, in order between the events already paired.
    pair_usage_in_gaps(a, b, &mut a_match, &mut b_used);
    group_partition(a, b, &a_match, d);
    branches(a, b, &a_match, d);
    for (i, m) in a_match.iter().enumerate() {
        match m {
            Some(j) if a[i].kind == Kind::Native && b[*j].native.as_ref().map(|n| &n.format) != a[i].native.as_ref().map(|n| &n.format) => {
                d.native_only_dropped += 1;
            }
            Some(j) => {
                d.matched += 1;
                compare_event(&a[i], &b[*j], &message_times(a, i), opts, d);
            }
            None if a[i].kind == Kind::Native => d.native_only_dropped += 1,
            None => note_missing(&a[i], d),
        }
    }
    for (j, used) in b_used.iter().enumerate() {
        if !used {
            match b[j].kind {
                // A target's own records (it writes some on every export) are not a loss of ours.
                Kind::Native => {}
                Kind::Turn => d.turns_added += 1,
                k => *d.extra.entry(k.to_string()).or_default() += 1,
            }
        }
    }
}

/// The index of the earlier event `evs[i].after` continues from, when that is
/// not simply the previous event (SPEC §2.3; as [`Ledger::on_path`] reads it).
fn branch_target(evs: &[Event], i: usize) -> Option<usize> {
    let after = evs[i].after.as_deref()?;
    let t = evs[..i].iter().rposition(|e| e.id == after)?;
    (t + 1 < i).then_some(t)
}

/// Which events are on the active path (SPEC §2.3), by index.
fn active_path(evs: &[Event]) -> Vec<bool> {
    let mut on = vec![false; evs.len()];
    let mut cur = evs.len().checked_sub(1);
    while let Some(i) = cur {
        on[i] = true;
        cur = branch_target(evs, i).or_else(|| i.checked_sub(1));
    }
    on
}

/// The nearest event `evs[i]` continues from (its `after`, else the previous
/// event, and so on back) that `paired` accepts.
fn paired_predecessor(evs: &[Event], i: usize, paired: impl Fn(usize) -> bool) -> Option<usize> {
    let mut cur = branch_target(evs, i).or_else(|| i.checked_sub(1));
    while let Some(k) = cur {
        if paired(k) {
            return Some(k);
        }
        cur = branch_target(evs, k).or_else(|| k.checked_sub(1));
    }
    None
}

/// Charge a change in branch structure (SPEC §2.3): `compare_event` sees
/// no `after`, because `after` names ids a target re-mints. Two charges, on
/// matched events only (a dropped event is already missing):
///
/// - `<kind>.branch`: the event is on one side's active path and off the
///   other's. It costs what dropping it would (1 plus its non-empty body
///   fields), so writing an abandoned branch as the conversation never scores
///   better than leaving it out, as SPEC §2.3 tells a target that cannot
///   express a branch to do.
/// - `<kind>.after`: where either side starts a branch at the event, the
///   events it continues from (the nearest matched one back along the tree)
///   are not partners.
fn branches(a: &[Event], b: &[Event], a_match: &[Option<usize>], d: &mut Diff) {
    let branched = |evs: &[Event]| (0..evs.len()).any(|i| branch_target(evs, i).is_some());
    if !branched(a) && !branched(b) {
        return;
    }
    let (on_a, on_b) = (active_path(a), active_path(b));
    let mut b_from: Vec<Option<usize>> = vec![None; b.len()];
    for (i, m) in a_match.iter().enumerate() {
        if let Some(j) = m {
            b_from[*j] = Some(i);
        }
    }
    let real = |e: &Event| e.kind != Kind::Native;
    for (i, m) in a_match.iter().enumerate() {
        let Some(j) = *m else { continue };
        if !real(&a[i]) || a[i].kind != b[j].kind {
            continue;
        }
        let k = a[i].kind.as_str();
        if on_a[i] != on_b[j] {
            let facts = a[i].body.values().filter(|v| !is_empty_value(v)).count() as u64;
            *d.changed.entry(format!("{k}.branch")).or_default() += 1 + facts;
        }
        if branch_target(a, i).is_some() || branch_target(b, j).is_some() {
            let pa = paired_predecessor(a, i, |x| a_match[x].is_some() && real(&a[x]));
            let pb = paired_predecessor(b, j, |y| b_from[y].is_some() && real(&b[y]));
            if pa.and_then(|x| a_match[x]) != pb {
                *d.changed.entry(format!("{k}.after")).or_default() += 1;
            }
        }
    }
}

/// Charge a change in how matched events are grouped into responses (SPEC
/// §3.1.4), whatever the group ids are called: a source response whose events
/// come back in k target responses is split k−1 times (`<kind>.group-split`,
/// on the first event of each extra piece), and a target response holding the
/// events of m source responses merged them m−1 times (`<kind>.group-merged`,
/// likewise). Only events grouped on both sides count: a group lost outright
/// is charged per event as `<kind>.group`, and one gained is not a loss.
fn group_partition(a: &[Event], b: &[Event], a_match: &[Option<usize>], d: &mut Diff) {
    // Matched, grouped on both sides, in `a`'s order.
    let pairs: Vec<(usize, &str, &str)> = a_match
        .iter()
        .enumerate()
        .filter_map(|(i, m)| {
            let j = (*m)?;
            (a[i].kind != Kind::Native && a[i].kind == b[j].kind).then_some(())?;
            Some((i, a[i].group.as_deref()?, b[j].group.as_deref()?))
        })
        .collect();
    #[allow(clippy::type_complexity)] // a one-off tuple of rows, named by the function that builds it
    let mut charge = |label: &str, by: &dyn Fn(&(usize, &str, &str)) -> (String, String)| {
        // key → the pieces it falls into, in order of first appearance.
        let mut seen: HashMap<String, Vec<String>> = HashMap::new();
        for p in &pairs {
            let (key, piece) = by(p);
            let pieces = seen.entry(key).or_default();
            if !pieces.contains(&piece) {
                if !pieces.is_empty() {
                    *d.changed.entry(format!("{}.{label}", a[p.0].kind.as_str())).or_default() += 1;
                }
                pieces.push(piece);
            }
        }
    };
    charge("group-split", &|&(_, ga, gb)| (ga.to_string(), gb.to_string()));
    charge("group-merged", &|&(_, ga, gb)| (gb.to_string(), ga.to_string()));
}

/// Pair `a[ra]` with `b[rb]`, in order: a longest common subsequence over
/// [`signature`] (text keyed by content) fixes the anchors, and between each
/// two consecutive anchors whatever is left pairs by [`coarse_signature`]
/// (text by role alone).
#[allow(clippy::too_many_arguments)]
fn align(a: &[Event], b: &[Event], ra: &[usize], rb: &[usize], paired_a: &[bool], paired_b: &[bool], a_match: &mut [Option<usize>], b_used: &mut [bool]) {
    if ra.is_empty() || rb.is_empty() {
        return;
    }
    // An event already paired (`paired_*`) is its pair's own token: it can
    // only align with its partner, and is never paired again in a gap.
    let sa: Vec<String> = ra.iter().map(|&i| if paired_a[i] { format!("\u{1e}{}", a_match[i].unwrap()) } else { signature(&a[i]) }).collect();
    let sb: Vec<String> = rb.iter().map(|&j| if paired_b[j] { format!("\u{1e}{j}") } else { signature(&b[j]) }).collect();
    // The longest alignment wins; of equally long ones, the one that pairs the
    // most texts keyed by their content (stronger evidence than a structural
    // match). A match weighs more than the most texts any alignment can hold,
    // so the text count only ever breaks ties. (The table is bounded at 4e7
    // cells, so the weight times the length stays far inside a u32.)
    let unit = ra.len().min(rb.len()) as u32 + 1;
    let w: Vec<u32> = ra.iter().map(|&i| if matches!(a[i].kind, Kind::Text | Kind::PeerMessage) { unit + 1 } else { unit }).collect();
    let anchors = lcs_weighted(&sa, &sb, &w);
    let (mut px, mut py) = (0, 0);
    for (x, y) in anchors.into_iter().chain(std::iter::once((ra.len(), rb.len()))) {
        let ga: Vec<usize> = ra[px..x].iter().copied().filter(|&i| !paired_a[i]).collect();
        let gb: Vec<usize> = rb[py..y].iter().copied().filter(|&j| !paired_b[j]).collect();
        if !ga.is_empty() && !gb.is_empty() {
            let ca: Vec<String> = ga.iter().map(|&i| coarse_signature(&a[i])).collect();
            let cb: Vec<String> = gb.iter().map(|&j| coarse_signature(&b[j])).collect();
            for (p, q) in lcs(&ca, &cb) {
                a_match[ga[p]] = Some(gb[q]);
                b_used[gb[q]] = true;
            }
        }
        if x < ra.len() && !paired_a[ra[x]] {
            a_match[ra[x]] = Some(rb[y]);
            b_used[rb[y]] = true;
        }
        (px, py) = (x + 1, y + 1);
    }
}

/// Whether `e` is part of a model response that a usage event reports on.
fn response_part(e: &Event) -> bool {
    matches!(e.kind, Kind::ToolCall | Kind::Reasoning) || (e.kind == Kind::Text && e.str("role") == Some("assistant"))
}

/// The response event usage `evs[i]` reports on: the nearest earlier matched
/// response part of its own group, or, when it has no group, the nearest
/// earlier matched response part with no earlier ungrouped usage in between
/// (that usage owns what lies before it).
fn usage_anchor(evs: &[Event], i: usize, matched: impl Fn(usize) -> bool) -> Option<usize> {
    let u = &evs[i];
    for k in (0..i).rev() {
        let e = &evs[k];
        if e.kind == Kind::Usage {
            if e.group == u.group {
                return None;
            }
            continue;
        }
        if u.group.is_some() && e.group != u.group {
            continue;
        }
        if response_part(e) && matched(k) {
            return Some(k);
        }
    }
    None
}

/// The matched response parts of the message usage `evs[i]` reports on.
/// A grouped usage reports on its whole message: every matched response part
/// of its group, before or after it, with no other usage of that group in
/// between (a message id a source repeats is two messages). An ungrouped usage
/// reports on its [`usage_anchor`] alone. Where in the message the usage sits
/// is a convention of the source (Claude Code writes it on the line that
/// settles the message, SPEC §9.3), not a fact.
fn usage_message(evs: &[Event], i: usize, matched: impl Fn(usize) -> bool) -> Vec<usize> {
    let u = &evs[i];
    let Some(g) = u.group.as_deref() else {
        return usage_anchor(evs, i, matched).into_iter().collect();
    };
    let same = |e: &Event| e.group.as_deref() == Some(g);
    let mut out = Vec::new();
    for k in (0..i).rev() {
        let e = &evs[k];
        if e.kind == Kind::Usage && same(e) {
            break;
        }
        if same(e) && response_part(e) && matched(k) {
            out.push(k);
        }
    }
    out.reverse();
    for (k, e) in evs.iter().enumerate().skip(i + 1) {
        if e.kind == Kind::Usage && same(e) {
            break;
        }
        if same(e) && response_part(e) && matched(k) {
            out.push(k);
        }
    }
    out
}

/// Pair usage whose groups did not line up (a target re-mints them) by the
/// message it reports on: a usage in `a` (of its group's message, or,
/// ungrouped, of the response it follows) pairs with an unpaired usage in `b`
/// whose message holds a partner of one of that message's parts, cumulative
/// with cumulative; of several, the one that differs in the fewest fields.
/// Where the usage sits within its message, or around the message's tool
/// results, on either side does not matter.
fn pair_usage_by_response(a: &[Event], b: &[Event], a_match: &mut [Option<usize>], b_used: &mut [bool]) {
    let cumulative = |e: &Event| e.body.get("cumulative").and_then(Value::as_bool).unwrap_or(false);
    // Each unpaired usage of `b`, with the message it reports on.
    let mut cands: Vec<(usize, Vec<usize>)> = Vec::new();
    for j in 0..b.len() {
        if !b_used[j] && b[j].kind == Kind::Usage {
            let m = usage_message(b, j, |k| b_used[k]);
            if !m.is_empty() {
                cands.push((j, m));
            }
        }
    }
    for i in 0..a.len() {
        if a_match[i].is_some() || a[i].kind != Kind::Usage {
            continue;
        }
        let partners: Vec<usize> = usage_message(a, i, |k| a_match[k].is_some()).into_iter().filter_map(|k| a_match[k]).collect();
        if partners.is_empty() {
            continue;
        }
        let best = cands
            .iter()
            .enumerate()
            .filter(|(_, (j, m))| !b_used[*j] && cumulative(&b[*j]) == cumulative(&a[i]) && m.iter().any(|k| partners.contains(k)))
            .min_by_key(|(_, (j, _))| body_differences(&a[i], &b[*j]))
            .map(|(pos, _)| pos);
        if let Some(pos) = best {
            let (j, _) = cands.remove(pos);
            a_match[i] = Some(j);
            b_used[j] = true;
        }
    }
}

/// How many of `a`'s non-empty body fields `b` does not hold the same.
fn body_differences(a: &Event, b: &Event) -> usize {
    a.body
        .keys()
        .filter(|k| {
            let (x, y) = (body_value(a, k), body_value(b, k));
            x.as_ref().is_some_and(|v| !is_empty_value(v)) && !json_eq(x.as_ref(), y.as_ref())
        })
        .count()
}

/// Pair the usage still unpaired, in order, between the events already paired:
/// one LCS over the paired events (each its own token) and the unpaired usage.
fn pair_usage_in_gaps(a: &[Event], b: &[Event], a_match: &mut [Option<usize>], b_used: &mut [bool]) {
    let mut ia: Vec<usize> = Vec::new();
    let mut sa: Vec<String> = Vec::new();
    for i in 0..a.len() {
        match a_match[i] {
            Some(j) => {
                ia.push(i);
                sa.push(format!("#{j}"));
            }
            None if a[i].kind == Kind::Usage => {
                ia.push(i);
                sa.push(coarse_signature(&a[i]));
            }
            None => {}
        }
    }
    let mut ib: Vec<usize> = Vec::new();
    let mut sb: Vec<String> = Vec::new();
    for j in 0..b.len() {
        if b_used[j] {
            ib.push(j);
            sb.push(format!("#{j}"));
        } else if b[j].kind == Kind::Usage {
            ib.push(j);
            sb.push(coarse_signature(&b[j]));
        }
    }
    if !sa.iter().any(|s| !s.starts_with('#')) || !sb.iter().any(|s| !s.starts_with('#')) {
        return;
    }
    for (x, y) in lcs(&sa, &sb) {
        let (i, j) = (ia[x], ib[y]);
        if a_match[i].is_none() && !b_used[j] {
            a_match[i] = Some(j);
            b_used[j] = true;
        }
    }
}

fn lcs(a: &[String], b: &[String]) -> Vec<(usize, usize)> {
    lcs_weighted(a, b, &vec![1; a.len()])
}

/// A common subsequence of `a` and `b` of greatest total weight, a match of
/// `a[i]` weighing `w[i]` (all 1: the longest common subsequence).
fn lcs_weighted(a: &[String], b: &[String], w: &[u32]) -> Vec<(usize, usize)> {
    let (n, m) = (a.len(), b.len());
    // Guard the quadratic table on very large inputs: fall back to greedy.
    if n.saturating_mul(m) > 40_000_000 {
        let mut out = Vec::new();
        let mut j = 0;
        for (i, x) in a.iter().enumerate() {
            if let Some(k) = b[j..].iter().position(|y| y == x) {
                out.push((i, j + k));
                j += k + 1;
            }
        }
        return out;
    }
    let mut t = vec![vec![0u32; m + 1]; n + 1];
    for i in (0..n).rev() {
        for j in (0..m).rev() {
            let skip = t[i + 1][j].max(t[i][j + 1]);
            t[i][j] = if a[i] == b[j] { skip.max(t[i + 1][j + 1] + w[i]) } else { skip };
        }
    }
    let (mut i, mut j, mut out) = (0, 0, Vec::new());
    while i < n && j < m {
        if a[i] == b[j] && t[i][j] == t[i + 1][j + 1] + w[i] {
            out.push((i, j));
            i += 1;
            j += 1;
        } else if t[i + 1][j] >= t[i][j + 1] {
            i += 1;
        } else {
            j += 1;
        }
    }
    out
}

/// The times of the message a per-response usage `evs[i]` reports on:
/// every time an event of its group carries, with no other usage of that group
/// in between. Which of its message's lines a source stamped the usage with is
/// the source's convention (Claude Code: the line that settled it), not a fact;
/// a target that stamps it with another time of the same message keeps it.
/// Empty for anything else.
fn message_times(evs: &[Event], i: usize) -> Vec<String> {
    let u = &evs[i];
    let Some(g) = u.group.as_deref().filter(|_| u.kind == Kind::Usage) else { return Vec::new() };
    let same = |e: &Event| e.group.as_deref() == Some(g);
    let before = evs[..i].iter().rev().take_while(|e| !(e.kind == Kind::Usage && same(e)));
    let after = evs[i + 1..].iter().take_while(|e| !(e.kind == Kind::Usage && same(e)));
    before.chain(after).filter(|e| same(e)).filter_map(|e| e.ts.as_deref().map(norm_ts)).collect()
}

fn compare_event(a: &Event, b: &Event, message_times: &[String], opts: CompareOptions, d: &mut Diff) {
    let k = a.kind.as_str();
    let mut bump = |f: &str| *d.changed.entry(format!("{k}.{f}")).or_default() += 1;
    if a.kind != b.kind {
        bump("kind");
        return;
    }
    let bt = b.ts.as_deref().map(norm_ts);
    if !opts.ignore_ts && a.ts.is_some() && a.ts.as_deref().map(norm_ts) != bt && !bt.is_some_and(|t| message_times.contains(&t)) {
        bump("ts");
    }
    if a.effective_audience() != b.effective_audience() {
        bump("audience");
    }
    if a.turn.is_some() && b.turn.is_none() {
        bump("turn");
    }
    if a.group.is_some() && b.group.is_none() {
        bump("group");
    }
    // Only what `a` holds can be lost; a field `b` gained is not scored.
    for key in a.body.keys() {
        let (x, y) = (body_value(a, key), body_value(b, key));
        if x.as_ref().is_some_and(|v| !is_empty_value(v)) && !json_eq(x.as_ref(), y.as_ref()) {
            bump(&format!("body.{key}"));
        }
    }
    if !opts.ignore_meta {
        for key in a.meta.keys().chain(b.meta.keys().filter(|k| !a.meta.contains_key(*k))) {
            if a.meta.get(key) != b.meta.get(key) {
                bump(&format!("meta.{key}"));
            }
        }
    }
    if !opts.ignore_native {
        match (&a.native, &b.native) {
            // An altered record is no longer the source's (SPEC §4): never
            // exact, and not a loss either.
            (Some(x), Some(y)) if x.format == y.format && (x.altered || y.altered) => {
                if x.part == 0 {
                    d.altered += 1;
                }
            }
            // Exactness is the record's exact text: parsed JSON (and so
            // `Native ==`) cannot see key order or number spelling.
            (Some(x), Some(y)) if x.format == y.format && x.part == 0 && !same_record_text(x, y) => bump("native"),
            (Some(x), Some(y)) if x.format != y.format => bump("native"),
            (Some(_), None) => bump("native"),
            _ => {}
        }
    }
}

/// Whether two native records are the same bytes: their record text and their
/// raw source text, compared as text.
fn same_record_text(x: &Native, y: &Native) -> bool {
    x.record_text() == y.record_text() && x.raw == y.raw
}

fn note_missing(e: &Event, d: &mut Diff) {
    if e.kind == Kind::Native {
        d.native_only_dropped += 1;
        return;
    }
    let facts = e.body.values().filter(|v| !is_empty_value(v)).count() as u64;
    // A settings snapshot that sets nothing holds no fact to lose.
    if e.kind == Kind::Settings && facts == 0 {
        d.empty_dropped += 1;
        return;
    }
    *d.missing.entry(e.kind.to_string()).or_default() += 1;
    d.missing_facts += 1 + facts;
}

/// A body field with SPEC defaults applied (§3.1: an omitted `tool_type` is `function`).
fn body_value(e: &Event, key: &str) -> Option<Value> {
    match (e.kind, key, e.body.get(key)) {
        (Kind::ToolCall, "tool_type", None) => Some(Value::String("function".into())),
        (_, _, v) => v.cloned(),
    }
}

/// JSON equality that treats numerically equal numbers as equal.
fn json_eq(a: Option<&Value>, b: Option<&Value>) -> bool {
    match (a, b) {
        (None, None) => true,
        (Some(x), Some(y)) => value_eq(x, y),
        // An absent field and an explicit null/empty are the same fact.
        (None, Some(v)) | (Some(v), None) => is_empty_value(v),
    }
}

fn is_empty_value(v: &Value) -> bool {
    match v {
        Value::Null | Value::Bool(false) => true,
        Value::Array(a) => a.is_empty(),
        Value::Object(o) => o.is_empty(),
        _ => false,
    }
}

fn value_eq(x: &Value, y: &Value) -> bool {
    match (x, y) {
        (Value::Number(p), Value::Number(q)) => match (p.as_f64(), q.as_f64()) {
            (Some(a), Some(b)) => a == b,
            _ => p == q,
        },
        (Value::Array(p), Value::Array(q)) => p.len() == q.len() && p.iter().zip(q).all(|(a, b)| value_eq(a, b)),
        (Value::Object(p), Value::Object(q)) => p.keys().chain(q.keys()).all(|k| json_eq(p.get(k), q.get(k))),
        _ => x == y,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::model::{Agent, Obj};

    fn set(events: Vec<Event>) -> LedgerSet {
        let mut l = Ledger::new(Agent { id: "a".into(), created_at: None, source: None, parent: None, links: vec![], meta: Obj::new() });
        for e in events {
            l.append(e).unwrap();
        }
        LedgerSet::single(l)
    }

    fn text(id: &str, role: &str, t: &str) -> Event {
        Event::new(id, Kind::Text).with("role", role).with("text", t)
    }

    fn usage(id: &str, group: &str, input: u64) -> Event {
        Event::new(id, Kind::Usage).group(Some(group)).with("model", "m").with("input", input).with("output", input + 1)
    }

    fn diff(a: Vec<Event>, b: Vec<Event>) -> Diff {
        compare(&set(a), &set(b), CompareOptions::default())
    }

    fn map(pairs: &[(&str, u64)]) -> BTreeMap<String, u64> {
        pairs.iter().map(|(k, v)| (k.to_string(), *v)).collect()
    }

    // Guard 1: text is aligned by content, not by role alone.

    #[test]
    fn one_removed_text_does_not_shift_the_later_pairings() {
        let words = ["one", "two", "three", "four", "five"];
        let a: Vec<Event> = words.iter().map(|w| text(&format!("a-{w}"), "user", w)).collect();
        // The target re-minted every id and dropped "two".
        let b: Vec<Event> = words.iter().filter(|w| **w != "two").map(|w| text(&format!("b-{w}"), "user", w)).collect();
        let d = diff(a, b);
        assert_eq!(d.missing, map(&[("text", 1)]), "{}", d.summary());
        assert_eq!(d.changed, BTreeMap::new(), "a later text paired with the wrong one: {}", d.summary());
        assert_eq!((d.matched, d.missing_facts, d.score()), (4, 3, 3), "{}", d.summary());
    }

    #[test]
    fn texts_whose_keys_collide_still_pair_in_order() {
        // Same role and the same first CONTENT_KEY_CHARS characters: the key
        // cannot tell them apart, so they pair in order, as an LCS must, and
        // each pairing is a changed text (not a free swap).
        let head = "x".repeat(CONTENT_KEY_CHARS);
        let (x, y) = (format!("{head} alpha"), format!("{head} beta"));
        let d = diff(vec![text("a1", "user", &x), text("a2", "user", &y)], vec![text("b1", "user", &y), text("b2", "user", &x)]);
        assert_eq!((d.matched, d.missing.len(), d.extra.len()), (2, 0, 0), "{}", d.summary());
        assert_eq!(d.changed, map(&[("text.body.text", 2)]), "{}", d.summary());
        assert_eq!(content_key(&x), content_key(&y), "the case is not the one it claims to be");
    }

    #[test]
    fn swapped_distinct_texts_do_not_cross() {
        // Distinct keys, swapped: an order-preserving alignment pairs one of
        // them and names the other missing; it never pairs both crosswise.
        let d = diff(vec![text("a1", "user", "alpha"), text("a2", "user", "beta")], vec![text("b1", "user", "beta"), text("b2", "user", "alpha")]);
        assert_eq!(d.matched, 1, "{}", d.summary());
        assert_eq!((d.missing.clone(), d.extra.clone()), (map(&[("text", 1)]), map(&[("text", 1)])), "{}", d.summary());
        assert_eq!(d.changed, BTreeMap::new(), "{}", d.summary());
    }

    #[test]
    fn content_key_collapses_whitespace_and_keeps_a_prefix() {
        assert_eq!(content_key("  a \n\t b  "), "a b");
        assert_eq!(content_key(&"é".repeat(100)).chars().count(), CONTENT_KEY_CHARS);
        assert_ne!(content_key("alpha"), content_key("beta"));
    }

    // Guard 2: usage the target re-grouped pairs with its own response.

    fn call(id: &str, group: &str, call_id: &str) -> Event {
        Event::new(id, Kind::ToolCall).group(Some(group)).with("call_id", call_id).with("name", "ls")
    }

    fn result(id: &str, call_id: &str) -> Event {
        Event::new(id, Kind::ToolResult).with("call_id", call_id).with("output", "ok")
    }

    #[test]
    fn usage_with_reminted_groups_pairs_with_its_response() {
        // The target re-mints every group id and writes a response's usage
        // after its tool result instead of before it.
        let a = vec![
            text("a1", "assistant", "first").group(Some("g1")),
            call("a2", "g1", "c1"),
            usage("a3", "g1", 10),
            result("a4", "c1"),
            text("a5", "assistant", "second").group(Some("g2")),
            usage("a6", "g2", 20),
        ];
        let b = vec![
            text("b1", "assistant", "first").group(Some("h1")),
            call("b2", "h1", "c1"),
            result("b4", "c1"),
            usage("b3", "h1", 10),
            text("b5", "assistant", "second").group(Some("h2")),
            usage("b6", "h2", 20),
        ];
        let d = diff(a, b);
        assert_eq!((d.missing.clone(), d.extra.clone()), (BTreeMap::new(), BTreeMap::new()), "{}", d.summary());
        assert_eq!(d.changed, BTreeMap::new(), "{}", d.summary());
        assert_eq!((d.matched, d.score()), (6, 0), "{}", d.summary());
    }

    /// Claude Code writes a message's usage on the line that settles it,
    /// which can be before the message's last tool call; a target writes it
    /// after the whole message. Same message, so the same usage: no loss.
    #[test]
    fn usage_is_paired_by_message_not_by_its_place_in_the_message() {
        let a = vec![
            text("a1", "assistant", "reading").group(Some("g1")),
            call("a2", "g1", "c1"),
            usage("a3", "g1", 10),
            result("a4", "c1"),
            call("a5", "g1", "c2"),
            result("a6", "c2"),
            text("a7", "assistant", "done").group(Some("g2")),
            usage("a8", "g2", 20),
        ];
        let b = vec![
            text("b1", "assistant", "reading").group(Some("h1")),
            call("b2", "h1", "c1"),
            result("b4", "c1"),
            call("b5", "h1", "c2"),
            result("b6", "c2"),
            usage("b3", "h1", 10),
            text("b7", "assistant", "done").group(Some("h2")),
            usage("b8", "h2", 20),
        ];
        let d = diff(a.clone(), b.clone());
        assert_eq!((d.missing.clone(), d.extra.clone()), (BTreeMap::new(), BTreeMap::new()), "{}", d.summary());
        assert_eq!((d.changed.clone(), d.score()), (BTreeMap::new(), 0), "{}", d.summary());
        // Ungrouped on the target side: the usage reports on what precedes it.
        let ungrouped: Vec<Event> = b.iter().cloned().map(|e| if e.kind == Kind::Usage { e.group(None::<&str>) } else { e }).collect();
        let d = diff(a.clone(), ungrouped);
        assert_eq!((d.missing.clone(), d.extra.clone()), (BTreeMap::new(), BTreeMap::new()), "{}", d.summary());
        // Opposed: the usage of the first message is gone from the target, and the
        // second message's usage carries the first one's numbers. It is still the
        // second message's: the first is charged missing, not paired across messages.
        let mut moved = b.clone();
        moved.remove(5);
        moved[6] = usage("b8", "h2", 10);
        let d = diff(a, moved);
        assert_eq!(d.missing, map(&[("usage", 1)]), "{}", d.summary());
        assert_eq!(d.changed, map(&[("usage.body.input", 1), ("usage.body.output", 1)]), "{}", d.summary());
    }

    /// Parallel results come back in call order, and a derived subagent
    /// start sits before the result instead of after it: each result still
    /// pairs with the result of its own call.
    #[test]
    fn a_tool_result_pairs_by_its_call_not_its_place() {
        let start = |id: &str| Event::new(id, Kind::Subagent).with("phase", "start").with("agent", "kid").with("call_id", "c2");
        let a = vec![
            call("a1", "g1", "c1"),
            call("a2", "g1", "c2"),
            result("a3", "c2").with("output", "two"),
            start("a4"),
            result("a5", "c1").with("output", "one"),
        ];
        let b = vec![
            call("b1", "h1", "c1"),
            call("b2", "h1", "c2"),
            start("b4"),
            result("b5", "c1").with("output", "one"),
            result("b3", "c2").with("output", "two"),
        ];
        let d = diff(a.clone(), b.clone());
        assert_eq!((d.matched, d.score()), (5, 0), "{}", d.summary());
        // Opposed: a result whose call id the target changed is not paired by it;
        // the order-preserving alignment then pairs it with the wrong call's result.
        let mut moved = b;
        moved[4] = result("b3", "c9").with("output", "two");
        let d = diff(a, moved);
        assert!(d.score() > 0, "{}", d.summary());
        assert!(d.changed.contains_key("tool_result.body.call_id") || d.missing.contains_key("tool_result"), "{}", d.summary());
    }

    /// SPEC §2.3: the comparator reads branch structure. `rewind(p)`
    /// builds branch-rewind's shape with ids prefixed `p`: u1 a1 [u2 c r a2]
    /// then u3 continuing from a1.
    fn rewind(p: &str, branch: bool, abandoned: bool) -> Vec<Event> {
        let id = |x: &str| format!("{p}{x}");
        let mut v = vec![text(&id("u1"), "user", "haiku"), text(&id("a1"), "assistant", "rain")];
        if abandoned {
            v.extend([
                text(&id("u2"), "user", "count it"),
                call(&id("c"), &id("g"), "k1").group(None::<&str>),
                result(&id("r"), "k1"),
                text(&id("a2"), "assistant", "five"),
            ]);
        }
        let mut u3 = text(&id("u3"), "user", "translate");
        if branch && abandoned {
            u3 = u3.after(Some(id("a1")));
        }
        v.extend([u3, text(&id("a3"), "assistant", "pluie")]);
        v
    }

    #[test]
    fn a_branch_structure_change_moves_the_score_and_an_identical_tree_does_not() {
        // The same tree, every id re-minted: nothing lost.
        let d = diff(rewind("a-", true, true), rewind("b-", true, true));
        assert_eq!((d.changed.clone(), d.score()), (BTreeMap::new(), 0), "{}", d.summary());
        // The abandoned branch written as the conversation (no `after`): every
        // abandoned event is charged what dropping it costs, and the rewound
        // prompt continues from the wrong event.
        let d = diff(rewind("a-", true, true), rewind("b-", false, true));
        assert_eq!(d.changed, map(&[("text.branch", 6), ("tool_call.branch", 3), ("tool_result.branch", 3), ("text.after", 1)]), "{}", d.summary());
        // The active path alone (SPEC §2.3 for a target with no branches): the
        // abandoned events are missing, at the same cost, and nothing else.
        let d = diff(rewind("a-", true, true), rewind("b-", false, false));
        assert_eq!(d.changed, BTreeMap::new(), "{}", d.summary());
        assert_eq!(d.missing_facts, 12, "{}", d.summary());
        // A branch point moved: u3 continues from u1 instead of a1.
        let mut moved = rewind("b-", true, true);
        let u3 = moved.iter().position(|e| e.id == "b-u3").unwrap();
        moved[u3].after = Some("b-u1".into());
        let d = diff(rewind("a-", true, true), moved);
        assert_eq!(d.changed, map(&[("text.after", 1), ("text.branch", 3)]), "{}", d.summary());
    }

    /// The group partition: relabelling every group costs
    /// nothing; two source responses merged into one, or one split into two,
    /// is charged once per extra piece.
    #[test]
    fn a_group_partition_change_is_charged_and_a_relabel_is_not() {
        let said = |id: &str, t: &str, g: &str| text(id, "assistant", t).group(Some(g));
        let a = vec![said("a1", "one", "g1"), said("a2", "two", "g1"), said("a3", "three", "g2"), said("a4", "four", "g2")];
        let relabelled = vec![said("b1", "one", "x"), said("b2", "two", "x"), said("b3", "three", "y"), said("b4", "four", "y")];
        let d = diff(a.clone(), relabelled);
        assert_eq!((d.changed.clone(), d.score()), (BTreeMap::new(), 0), "{}", d.summary());
        let merged = vec![said("b1", "one", "x"), said("b2", "two", "x"), said("b3", "three", "x"), said("b4", "four", "x")];
        let d = diff(a.clone(), merged);
        assert_eq!(d.changed, map(&[("text.group-merged", 1)]), "{}", d.summary());
        let split = vec![said("b1", "one", "x"), said("b2", "two", "z"), said("b3", "three", "y"), said("b4", "four", "y")];
        let d = diff(a.clone(), split);
        assert_eq!(d.changed, map(&[("text.group-split", 1)]), "{}", d.summary());
        // A group lost outright stays the per-event charge it was, with no partition charge on top.
        let lost = vec![said("b1", "one", "x"), said("b2", "two", "x"), text("b3", "assistant", "three"), text("b4", "assistant", "four")];
        let d = diff(a, lost);
        assert_eq!(d.changed, map(&[("text.group", 2)]), "{}", d.summary());
    }

    /// An ungrouped source usage (a Codex rollout before 0.153) written after
    /// its response's tool results reports on that response; a target that
    /// writes it before the results (SPEC §3.1.1) keeps it. Opposed: when the
    /// target drops that response, the usage is charged, not paired elsewhere.
    #[test]
    fn an_ungrouped_usage_pairs_with_the_response_it_follows() {
        let ungrouped = |id: &str, n: u64| usage(id, "x", n).group(None::<&str>);
        // Two results: the usage crosses both, so no in-order pairing can reach it.
        let a = vec![
            text("a1", "assistant", "probing").group(None::<&str>),
            call("a2", "x", "c1").group(None::<&str>),
            call("a2b", "x", "c2").group(None::<&str>),
            result("a3", "c1"),
            result("a3b", "c2"),
            ungrouped("a4", 10),
            text("a5", "assistant", "done").group(None::<&str>),
            ungrouped("a6", 20),
        ];
        let b = vec![
            text("b1", "assistant", "probing").group(Some("s1")),
            call("b2", "s1", "c1"),
            call("b2b", "s1", "c2"),
            usage("b4", "s1", 10),
            result("b3", "c1"),
            result("b3b", "c2"),
            text("b5", "assistant", "done").group(Some("s2")),
            usage("b6", "s2", 20),
        ];
        let d = diff(a.clone(), b.clone());
        assert_eq!((d.missing.clone(), d.extra.clone(), d.score()), (BTreeMap::new(), BTreeMap::new(), 0), "{}", d.summary());
        let dropped: Vec<Event> = b.into_iter().filter(|e| !["b1", "b2", "b2b", "b4"].contains(&e.id.as_str())).collect();
        let d = diff(a, dropped);
        assert_eq!(d.missing.get("usage"), Some(&1), "{}", d.summary());
    }

    /// A usage stamped with the time of its settling line comes back
    /// stamped with its message's first time: the same message, no loss. A
    /// time no event of the message holds is still charged.
    #[test]
    fn usage_time_is_any_time_of_its_message() {
        let at = |e: Event, t: &str| e.ts(Some(t));
        let a = vec![
            at(text("a1", "assistant", "reading").group(Some("g1")), "2026-01-01T00:00:01Z"),
            at(call("a2", "g1", "c1"), "2026-01-01T00:00:02Z"),
            at(usage("a3", "g1", 10), "2026-01-01T00:00:02Z"),
            at(result("a4", "c1"), "2026-01-01T00:00:03Z"),
        ];
        let b = |t: &str| {
            vec![
                at(text("b1", "assistant", "reading").group(Some("h1")), "2026-01-01T00:00:01Z"),
                at(call("b2", "h1", "c1"), "2026-01-01T00:00:01Z"),
                at(usage("b3", "h1", 10), t),
                at(result("b4", "c1"), "2026-01-01T00:00:03Z"),
            ]
        };
        let d = diff(a.clone(), b("2026-01-01T00:00:01.000Z"));
        assert_eq!(d.changed, map(&[("tool_call.ts", 1)]), "{}", d.summary());
        // Opposed: the tool result's time is not a time of the message.
        let d = diff(a, b("2026-01-01T00:00:03Z"));
        assert_eq!(d.changed, map(&[("tool_call.ts", 1), ("usage.ts", 1)]), "{}", d.summary());
    }

    #[test]
    fn usage_of_a_response_absent_from_the_target_is_still_charged() {
        // The first response never reached the target; an unrelated usage
        // with the first response's numbers sits at the end of the target.
        let a = vec![
            text("a1", "assistant", "first").group(Some("g1")),
            usage("a2", "g1", 10),
            text("a3", "assistant", "second").group(Some("g2")),
            usage("a4", "g2", 20),
        ];
        let b = vec![text("b3", "assistant", "second").group(Some("h2")), usage("b4", "h2", 20), usage("b9", "h9", 10)];
        let d = diff(a, b);
        assert_eq!(d.missing, map(&[("text", 1), ("usage", 1)]), "{}", d.summary());
        assert_eq!(d.extra, map(&[("usage", 1)]), "{}", d.summary());
        // The usage that did cross paired with its own response's usage.
        assert_eq!(d.changed, BTreeMap::new(), "{}", d.summary());
    }

    // Guard 3: an empty settings snapshot holds no fact to lose.

    #[test]
    fn empty_settings_dropped_costs_nothing() {
        let a = vec![Event::new("a0", Kind::Settings).with("complete", false), text("a1", "user", "hi")];
        let d = diff(a, vec![text("b1", "user", "hi")]);
        assert_eq!((d.score(), d.empty_dropped), (0, 1), "{}", d.summary());
        assert!(d.missing.is_empty(), "{}", d.summary());
    }

    #[test]
    fn non_empty_settings_dropped_still_costs() {
        let a = vec![Event::new("a0", Kind::Settings).with("model", "m"), text("a1", "user", "hi")];
        let d = diff(a, vec![text("b1", "user", "hi")]);
        assert_eq!(d.missing, map(&[("settings", 1)]), "{}", d.summary());
        assert_eq!((d.missing_facts, d.score(), d.empty_dropped), (2, 2, 0), "{}", d.summary());
    }

    // Guard 4: exactness is the record's text, and `altered` is never exact.

    fn with_record(id: &str, record: &str, altered: bool) -> Event {
        let mut n = crate::model::Native::new("fmt", Some(id.into())).record_raw(record).unwrap();
        n.altered = altered;
        text(id, "user", "hi").native(n)
    }

    #[test]
    fn altered_record_is_neither_exact_nor_a_loss() {
        // A redacted (altered) record against the source's own bytes: they differ.
        let a = vec![with_record("e1", r#"{"token":"[redacted]"}"#, true)];
        let b = vec![with_record("e1", r#"{"token":"s3cret"}"#, false)];
        let d = diff(a, b);
        assert_eq!(d.altered, 1, "{}", d.summary());
        assert!(d.changed.is_empty() && d.is_clean(), "an altered record is not a loss: {}", d.summary());
        assert!(!d.is_exact(), "an altered record is never exact: {}", d.summary());
        assert!(d.summary().contains("altered 1"), "{}", d.summary());
    }

    #[test]
    fn respelled_record_is_not_exact() {
        // Not altered, same JSON, different bytes: key order, inner whitespace,
        // number spelling. Parsed JSON (and `Native ==`) calls the first two the
        // same under every feature set; the record text does not.
        for (x, y) in [(r#"{"b":1,"a":2}"#, r#"{"a":2,"b":1}"#), (r#"{"a":[1,2]}"#, r#"{"a":[1, 2]}"#), (r#"{"a":1e3}"#, r#"{"a":1000.0}"#)] {
            let d = diff(vec![with_record("e1", x, false)], vec![with_record("e1", y, false)]);
            assert_eq!(d.changed, map(&[("text.native", 1)]), "{x} vs {y}: {}", d.summary());
            assert_eq!(d.altered, 0);
            assert!(!d.is_exact());
        }
        // ... and the same bytes are exact.
        let d = diff(vec![with_record("e1", r#"{"b":1,"a":2}"#, false)], vec![with_record("e1", r#"{"b":1,"a":2}"#, false)]);
        assert!(d.is_exact(), "{}", d.summary());
    }

    #[test]
    fn same_record_text_compares_text_not_json() {
        let n = |r: &str| crate::model::Native::new("fmt", None).record_raw(r).unwrap();
        assert!(same_record_text(&n(r#"{"a":1}"#), &n(r#"{"a":1}"#)));
        assert!(!same_record_text(&n(r#"{"b":1,"a":2}"#), &n(r#"{"a":2,"b":1}"#)));
        assert!(!same_record_text(&n(r#"{"a":1}"#), &n(r#"{"a":1}"#).raw("{\"a\":1}")));
    }
}
