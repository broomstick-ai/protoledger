//! The rewind a real Claude Code wrote and the revert a real opencode staged
//! (fixtures/claude-code/_live-rewind-2.1.280 and
//! fixtures/opencode/_live-revert-2.0.32, captured once by the live tests this
//! repository no longer has: see their SOURCE.md), read by the importers as
//! branches (SPEC §2.3). They read the committed fixtures and nothing else;
//! moved here unchanged from `tests/live/rewind.rs` when that target was
//! removed.

use crate::common;
use protoledger::Event;
use protoledger::formats::{self, ExportOptions, ImportOptions};

const SESSION: &str = "e5d5cbf6-a4de-4e2c-98d6-f2126a7f511b";
/// Turn A's answer ("ok"), turn B's prompt ("two"), and turn C's prompt
/// ("three"), which Claude Code wrote with `parentUuid` = turn A's answer.
const A_ANSWER: &str = "0547eff8-f9c5-4d79-b460-0448d202890c";
const B_PROMPT: &str = "c0babb8c-aee7-4440-b937-8208e9edf18e";
const C_PROMPT: &str = "725c3825-a187-4210-9f0f-b9ea184bddb2";

fn text(e: &Event) -> &str {
    e.str("text").unwrap_or("")
}

/// The active path (SPEC §2.3): from the last event, follow `after`, or else
/// the previous event, back to the start.
fn active(evs: &[Event]) -> Vec<&Event> {
    let mut out = Vec::new();
    let mut i = evs.len() as isize - 1;
    while i >= 0 {
        let e = &evs[i as usize];
        out.push(e);
        i = match &e.after {
            Some(a) => evs.iter().position(|x| &x.id == a).map(|p| p as isize).unwrap_or(i - 1),
            None => i - 1,
        };
    }
    out.reverse();
    out
}

#[test]
fn claude_code_rewind_fixture_imports_as_an_after_branch() {
    let file = common::fixture("claude-code/_live-rewind-2.1.280/native").join(format!("{SESSION}.jsonl"));
    let f = formats::by_name("claude-code").unwrap();
    let imp = f.import(&file, &ImportOptions::standard()).unwrap();
    let l = imp.set.get(SESSION).expect("the session's agent");
    let evs = &l.events;
    // The events a line made: every event whose native record is that line
    // (its parts, and a usage it settled, `<message.id>#usage`).
    let of_line = |u: &str| -> Vec<&Event> { evs.iter().filter(|e| e.native.as_ref().and_then(|n| n.id.as_deref()) == Some(u)).collect() };

    // The rewound prompt continues from turn A's answer: its `after` names the
    // last event of that line.
    let a_last = of_line(A_ANSWER).last().map(|e| e.id.clone()).expect("turn A's answer was imported");
    let c = of_line(C_PROMPT);
    assert_eq!(c.first().map(|e| text(e)), Some("reply with the word three"), "turn C's prompt");
    assert_eq!(c[0].after.as_deref(), Some(a_last.as_str()), "the rewound prompt names turn A's answer as `after`");
    // Opposed: turn B's prompt, which only continued the conversation, is no branch.
    let b = of_line(B_PROMPT);
    assert_eq!(b.first().map(|e| text(e)), Some("reply with the word two"), "turn B's prompt");
    assert_eq!(b[0].after, None, "turn B's prompt continues the conversation: no `after`");
    let afters: Vec<&str> = evs.iter().filter(|e| e.after.is_some()).map(|e| e.id.as_str()).collect();
    assert_eq!(afters, vec![C_PROMPT], "the one rewind in the session is the one branch point");

    // The active path holds turns A and C and not the abandoned turn B.
    let path = active(evs);
    let said: Vec<&str> = path
        .iter()
        .map(|e| text(e))
        .filter(|t| ["reply with the word ok", "ok", "reply with the word two", "two", "reply with the word three", "three"].contains(t))
        .collect();
    assert_eq!(said, vec!["reply with the word ok", "ok", "reply with the word three", "three"], "the active path");
    assert!(evs.iter().any(|e| text(e) == "two"), "turn B's answer stays in the file (an abandoned branch)");

    // And the self trip keeps the branch byte for byte.
    let ex = f.export(&imp.set, &ExportOptions::standard()).unwrap();
    let back = ex.output.files.get(&format!("{SESSION}.jsonl")).expect("the session file is exported");
    assert!(back == &std::fs::read(&file).unwrap(), "claude-code export of the rewound session is byte-exact");
}

/// The staged revert a real opencode wrote (fixtures/opencode/_live-revert-2.0.32,
/// captured by a live run: see its SOURCE.md): two turns, then `session.revert`
/// staged at turn 2's prompt, the reverted messages still in the database.
const OC_SESSION: &str = "ses_eac2310b3ffeaj5g7XWrvTVgyB";
const OC_TURN1_IDLE: &str = "msg_153dcf642001zyRY0RbYEZSBbL";
const OC_TURN2_PROMPT: &str = "msg_153dcf904001iPgLeMHByB3FhG";

fn rid(e: &Event) -> &str {
    e.native.as_ref().and_then(|n| n.id.as_deref()).unwrap_or("")
}

#[test]
fn opencode_staged_revert_fixture_imports_as_an_after_branch() {
    let db = common::fixture("opencode/_live-revert-2.0.32/native/opencode.db");
    let f = formats::by_name("opencode").unwrap();
    let imp = f.import(&db, &ImportOptions::standard()).unwrap();
    assert_eq!(imp.variant, "v2-sqlite");
    let l = imp.set.get(OC_SESSION).expect("the session's agent");
    let evs = &l.events;
    // One event continues from the event before the reverted prompt: the last
    // event of turn 1 (its idle record).
    let before = evs.iter().rfind(|e| rid(e) == OC_TURN1_IDLE).map(|e| e.id.clone()).expect("turn 1's idle record was imported");
    let first_reverted = evs.iter().position(|e| rid(e) == OC_TURN2_PROMPT).expect("the reverted prompt is still imported");
    assert_eq!(evs[first_reverted - 1].id, before, "turn 1's idle record is right before the reverted prompt");
    let afters: Vec<(&str, &str)> = evs.iter().filter_map(|e| e.after.as_deref().map(|a| (e.id.as_str(), a))).collect();
    assert_eq!(afters.len(), 1, "one branch point: {afters:?}");
    assert_eq!(afters[0].1, before, "the branch continues from the event before the reverted prompt");
    let at = evs.iter().position(|e| e.id == afters[0].0).unwrap();
    assert!(at > first_reverted, "the branch point comes after the reverted messages, which stay in the ledger");
    // The active path holds turn 1 and none of turn 2.
    let path = active(evs);
    let said: Vec<&str> =
        path.iter().map(|e| text(e)).filter(|t| ["\"reply with the word ok\"", "ok", "\"reply with the word two\"", "two"].contains(t)).collect();
    assert_eq!(said, vec!["\"reply with the word ok\"", "ok"], "the active path");
    assert!(evs.iter().any(|e| text(e) == "two"), "turn 2's answer stays in the ledger (an abandoned branch)");
}
