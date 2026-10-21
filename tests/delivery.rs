//! Message delivery by appended facts (SPEC §11.3): a message
//! queued, cancelled and later re-delivered derives as sent exactly once; a
//! cancelled message is still owed; `withdrawn` is the only retraction;
//! every refusal appends nothing; a refused-shaped fact written straight
//! through an `Appender` is ignored; redaction leaves delivery alone; and no
//! message is ever lost over any sequence of the four facts.

use protoledger::delivery::{self, Act, CANCELLED, DELIVERED, Delivery, DeliveryError, MESSAGE, State, WITHDRAWN};
use protoledger::ledger::fact_type;
use protoledger::model::Obj;
use protoledger::{Agent, Appender, Event, Kind, Ledger};
use serde_json::{Value, json};
use std::collections::HashMap;
use std::path::{Path, PathBuf};

fn agent(id: &str) -> Agent {
    Agent { id: id.into(), created_at: None, source: None, parent: None, links: vec![], meta: Default::default() }
}

fn ids(v: &[&str]) -> Vec<String> {
    v.iter().map(|s| s.to_string()).collect()
}

fn new_ledger(dir: &Path) -> (PathBuf, Appender) {
    let path = dir.join("a.jsonl");
    let ap = Appender::open(&path, Some(agent("a"))).unwrap();
    (path, ap)
}

fn on_disk(path: &Path) -> Ledger {
    Ledger::read(path).unwrap()
}

fn queue(ap: &mut Appender, id: &str) {
    delivery::message(ap, id, &format!("text of {id}"), &Obj::new()).unwrap();
}

fn states(d: &Delivery) -> Vec<(String, State)> {
    d.messages.iter().map(|m| (m.message_id.clone(), m.state)).collect()
}

fn history(d: &Delivery, id: &str) -> Vec<Act> {
    d.get(id).unwrap().history.iter().map(|s| s.act).collect()
}

fn names(ms: &[&delivery::Message]) -> Vec<String> {
    ms.iter().map(|m| m.message_id.clone()).collect()
}

/// The val's case: queue 3, deliver 1, an interrupt cancels 2, later deliver
/// 1 of them → delivered, delivered, cancelled-and-still-owed. Every fact is
/// one appended line and every earlier byte of the file stays as it was.
#[test]
fn queue_three_deliver_one_cancel_two_redeliver_one_is_delivered_delivered_cancelled_still_owed() {
    let dir = tempfile::tempdir().unwrap();
    let (path, mut ap) = new_ledger(dir.path());
    let mut prev = std::fs::read(&path).unwrap();
    let mut step = |ap: &mut Appender, f: &dyn Fn(&mut Appender)| {
        f(ap);
        let now = std::fs::read(&path).unwrap();
        assert!(now.starts_with(&prev), "an append rewrote an earlier byte of the ledger");
        assert_eq!(now.iter().filter(|b| **b == b'\n').count(), prev.iter().filter(|b| **b == b'\n').count() + 1, "one fact, one line");
        prev = now;
    };
    for m in ["m1", "m2", "m3"] {
        step(&mut ap, &|ap| queue(ap, m));
    }
    step(&mut ap, &|ap| {
        delivery::delivered(ap, &ids(&["m1"]), Some("turn-1"), Some("prompt-1")).unwrap();
    });
    step(&mut ap, &|ap| {
        delivery::cancelled(ap, &ids(&["m2", "m3"]), "interrupt").unwrap();
    });
    step(&mut ap, &|ap| {
        delivery::delivered(ap, &ids(&["m2"]), Some("turn-2"), None).unwrap();
    });

    let d = delivery::state(&on_disk(&path));
    assert_eq!(states(&d), vec![("m1".into(), State::Delivered), ("m2".into(), State::Delivered), ("m3".into(), State::Cancelled)]);
    assert!(State::Cancelled.owed(), "a cancelled message is still owed");
    assert_eq!(names(&d.should_send()), ids(&["m3"]), "should send = queued − delivered − withdrawn");
    assert_eq!(names(&d.sent()), ids(&["m1", "m2"]));
    assert_eq!(history(&d, "m1"), vec![Act::Delivered]);
    assert_eq!(history(&d, "m2"), vec![Act::Cancelled, Act::Delivered], "the re-delivery is its own fact after the cancellation");
    assert_eq!(history(&d, "m3"), vec![Act::Cancelled]);
    for m in &d.messages {
        assert_eq!(
            m.history.iter().filter(|s| s.act == Act::Delivered).count(),
            usize::from(m.state == State::Delivered),
            "{}: sent exactly once",
            m.message_id
        );
    }
    assert!(d.ignored.is_empty(), "{:?}", d.ignored);
    assert_eq!(d.get("m1").unwrap().text, "text of m1");
    // The delivered fact keeps what it was given.
    let l = on_disk(&path);
    let fact = l.events.iter().find(|e| e.id == d.get("m1").unwrap().history[0].event).unwrap();
    assert_eq!(fact.body["data"], json!({"message_ids": ["m1"], "turn": "turn-1", "event": "prompt-1"}));
    // The writer's view and the file's agree.
    assert_eq!(delivery::state(ap.ledger()), d);
}

/// Cancel → cancel is allowed and the message stays owed; withdrawn is
/// the only retraction, and a withdrawn message refuses deliver, cancel and
/// withdraw.
#[test]
fn cancel_twice_is_still_owed_and_only_withdrawn_retracts_and_is_final() {
    let dir = tempfile::tempdir().unwrap();
    let (path, mut ap) = new_ledger(dir.path());
    queue(&mut ap, "m");
    delivery::cancelled(&mut ap, &ids(&["m"]), "interrupt").unwrap();
    delivery::cancelled(&mut ap, &ids(&["m"]), "turn failed").unwrap();
    let d = delivery::state(&on_disk(&path));
    assert_eq!(d.state_of("m"), Some(State::Cancelled));
    assert_eq!(names(&d.should_send()), ids(&["m"]), "cancelled twice, still owed");

    delivery::withdrawn(&mut ap, &ids(&["m"]), "sender retracted it").unwrap();
    let d = delivery::state(&on_disk(&path));
    assert_eq!(d.state_of("m"), Some(State::Withdrawn));
    assert!(d.should_send().is_empty() && d.sent().is_empty(), "withdrawn: neither owed nor sent");
    assert_eq!(history(&d, "m"), vec![Act::Cancelled, Act::Cancelled, Act::Withdrawn]);
    assert_eq!(d.messages.len(), 1, "a withdrawn message is still there");
}

/// Run `f`, which must be refused with `want`, and show it appended nothing:
/// the file's bytes and the writer's ledger are as they were.
fn refused(path: &Path, ap: &mut Appender, want: DeliveryError, f: impl FnOnce(&mut Appender) -> anyhow::Result<protoledger::Appended>) {
    let bytes = std::fs::read(path).unwrap();
    let held = ap.ledger().events.len();
    let got = f(ap).expect_err("refused").downcast::<DeliveryError>().expect("a DeliveryError");
    assert_eq!(got, want);
    assert_eq!(ap.ledger().events.len(), held, "{want}: the refused fact is in the writer's ledger");
    let now = std::fs::read(path).unwrap();
    assert_eq!(now.len(), bytes.len(), "{want}: the refused fact was appended to the file");
    assert_eq!(now, bytes, "{want}: the file changed");
}

/// Every SPEC §11.3 refusal, each appending nothing, each paired with the
/// accepted case it is not.
#[test]
fn every_refusal_appends_nothing_and_the_accepted_case_appends_one_fact() {
    let dir = tempfile::tempdir().unwrap();
    let (path, mut ap) = new_ledger(dir.path());
    let p = path.as_path();
    let lines = |p: &Path| std::fs::read(p).unwrap().iter().filter(|b| **b == b'\n').count();

    // A duplicate message id.
    let n = lines(p);
    queue(&mut ap, "a");
    assert_eq!(lines(p), n + 1, "the first protoledger/message for an id is accepted");
    refused(p, &mut ap, DeliveryError::Duplicate("a".into()), |ap| delivery::message(ap, "a", "again", &Obj::new()));
    queue(&mut ap, "b");
    queue(&mut ap, "c");
    queue(&mut ap, "w");

    for (act, f) in [
        (
            Act::Delivered,
            (|ap: &mut Appender, i: &[String]| delivery::delivered(ap, i, None, None)) as fn(&mut Appender, &[String]) -> anyhow::Result<protoledger::Appended>,
        ),
        (Act::Cancelled, |ap, i| delivery::cancelled(ap, i, "x")),
        (Act::Withdrawn, |ap, i| delivery::withdrawn(ap, i, "x")),
    ] {
        let ty = act.fact();
        // An empty list.
        refused(p, &mut ap, DeliveryError::Empty(ty), |ap| f(ap, &[]));
        // An id listed twice (and the rest of the fact is not appended either).
        refused(p, &mut ap, DeliveryError::Repeated(ty, "b".into()), |ap| f(ap, &ids(&["b", "c", "b"])));
        // An unknown message, alone and beside a valid one: nothing is appended.
        refused(p, &mut ap, DeliveryError::Unknown(ty, "ghost".into()), |ap| f(ap, &ids(&["ghost"])));
        refused(p, &mut ap, DeliveryError::Unknown(ty, "ghost".into()), |ap| f(ap, &ids(&["b", "ghost"])));
    }
    // The accepted counterparts of each (b, c, w are still pending: nothing above was applied).
    let d = delivery::state(&on_disk(p));
    assert_eq!(states(&d).into_iter().map(|(_, s)| s).collect::<Vec<_>>(), vec![State::Pending; 4], "a refusal changed a state");
    let n = lines(p);
    delivery::cancelled(&mut ap, &ids(&["b", "c"]), "interrupt").unwrap();
    delivery::delivered(&mut ap, &ids(&["a", "b"]), None, None).unwrap();
    delivery::withdrawn(&mut ap, &ids(&["w"]), "retracted").unwrap();
    assert_eq!(lines(p), n + 3, "three accepted facts, three lines");

    // Deliver twice; cancel, or withdraw, a delivered message.
    refused(p, &mut ap, DeliveryError::Final(DELIVERED, "a".into(), State::Delivered), |ap| delivery::delivered(ap, &ids(&["a"]), None, None));
    refused(p, &mut ap, DeliveryError::Final(DELIVERED, "b".into(), State::Delivered), |ap| delivery::delivered(ap, &ids(&["c", "b"]), None, None));
    refused(p, &mut ap, DeliveryError::Final(CANCELLED, "a".into(), State::Delivered), |ap| delivery::cancelled(ap, &ids(&["a"]), "x"));
    refused(p, &mut ap, DeliveryError::Final(WITHDRAWN, "b".into(), State::Delivered), |ap| delivery::withdrawn(ap, &ids(&["b"]), "x"));
    // Withdrawn refuses deliver, cancel and withdraw.
    refused(p, &mut ap, DeliveryError::Final(DELIVERED, "w".into(), State::Withdrawn), |ap| delivery::delivered(ap, &ids(&["w"]), None, None));
    refused(p, &mut ap, DeliveryError::Final(CANCELLED, "w".into(), State::Withdrawn), |ap| delivery::cancelled(ap, &ids(&["w"]), "x"));
    refused(p, &mut ap, DeliveryError::Final(WITHDRAWN, "w".into(), State::Withdrawn), |ap| delivery::withdrawn(ap, &ids(&["w"]), "x"));
    // c is cancelled: each act is still allowed on it (opposes a check that refuses everything).
    let n = lines(p);
    delivery::cancelled(&mut ap, &ids(&["c"]), "again").unwrap();
    delivery::delivered(&mut ap, &ids(&["c"]), None, None).unwrap();
    assert_eq!(lines(p), n + 2);
    let d = delivery::state(&on_disk(p));
    assert_eq!(
        states(&d),
        vec![("a".into(), State::Delivered), ("b".into(), State::Delivered), ("c".into(), State::Delivered), ("w".into(), State::Withdrawn)]
    );
    assert!(d.ignored.is_empty(), "the writer appended a fact a reader ignores: {:?}", d.ignored);
}

fn raw(ap: &mut Appender, id: &str, ty: &str, data: Value) -> String {
    let e = Event::new(id, Kind::App).with("type", ty).with("data", data);
    ap.append(e).unwrap();
    id.to_string()
}

/// A fact the writer would have refused where it stands, written straight
/// through an `Appender` (another program's ledger), is ignored and named;
/// a valid raw fact is honoured.
#[test]
fn raw_refused_shaped_facts_are_ignored_and_a_withdrawn_message_stays_withdrawn() {
    let dir = tempfile::tempdir().unwrap();
    let (path, mut ap) = new_ledger(dir.path());
    // A delivery naming a message before its protoledger/message.
    let early = raw(&mut ap, "early", DELIVERED, json!({"message_ids": ["c"]}));
    raw(&mut ap, "msg-a", MESSAGE, json!({"message_id": "a", "text": "A", "meta": {}}));
    raw(&mut ap, "msg-b", MESSAGE, json!({"message_id": "b", "text": "B", "meta": {"example.app": {"queue": 1}}}));
    raw(&mut ap, "msg-c", MESSAGE, json!({"message_id": "c", "text": "C"}));
    let dup = raw(&mut ap, "msg-b-again", MESSAGE, json!({"message_id": "b", "text": "B, replaced", "meta": {}}));
    raw(&mut ap, "wd-a", WITHDRAWN, json!({"message_ids": ["a"], "reason": "retracted"}));
    let after_wd = raw(&mut ap, "dl-a", DELIVERED, json!({"message_ids": ["a"]}));
    let cancel_wd = raw(&mut ap, "cn-a", CANCELLED, json!({"message_ids": ["a"], "reason": "x"}));
    raw(&mut ap, "dl-b", DELIVERED, json!({"message_ids": ["b"], "turn": "t1"}));
    let twice = raw(&mut ap, "dl-b-again", DELIVERED, json!({"message_ids": ["b"]}));
    let cancel_dl = raw(&mut ap, "cn-b", CANCELLED, json!({"message_ids": ["b"], "reason": "x"}));
    let empty = raw(&mut ap, "dl-none", DELIVERED, json!({"message_ids": []}));
    let rep = raw(&mut ap, "cn-cc", CANCELLED, json!({"message_ids": ["c", "c"], "reason": "x"}));
    let partial = raw(&mut ap, "dl-c-ghost", DELIVERED, json!({"message_ids": ["c", "ghost"]}));
    let bad = raw(&mut ap, "dl-bad", DELIVERED, json!({"message_ids": "c"}));

    let d = delivery::state(&on_disk(&path));
    assert_eq!(d.state_of("a"), Some(State::Withdrawn), "a raw delivered naming a withdrawn message is ignored");
    assert_eq!(history(&d, "a"), vec![Act::Withdrawn]);
    assert_eq!(d.state_of("b"), Some(State::Delivered), "a valid raw delivered is honoured");
    assert_eq!(history(&d, "b"), vec![Act::Delivered]);
    assert_eq!(d.get("b").unwrap().text, "B", "a duplicate protoledger/message does not replace the first");
    assert_eq!(d.get("b").unwrap().meta, json!({"example.app": {"queue": 1}}).as_object().unwrap().clone());
    assert_eq!(d.state_of("c"), Some(State::Pending), "nothing valid named c after its message");
    assert_eq!(d.messages.len(), 3);
    let ignored: Vec<(&str, &DeliveryError)> = d.ignored.iter().map(|(id, why)| (id.as_str(), why)).collect();
    assert_eq!(
        ignored,
        vec![
            (early.as_str(), &DeliveryError::Unknown(DELIVERED, "c".into())),
            (dup.as_str(), &DeliveryError::Duplicate("b".into())),
            (after_wd.as_str(), &DeliveryError::Final(DELIVERED, "a".into(), State::Withdrawn)),
            (cancel_wd.as_str(), &DeliveryError::Final(CANCELLED, "a".into(), State::Withdrawn)),
            (twice.as_str(), &DeliveryError::Final(DELIVERED, "b".into(), State::Delivered)),
            (cancel_dl.as_str(), &DeliveryError::Final(CANCELLED, "b".into(), State::Delivered)),
            (empty.as_str(), &DeliveryError::Empty(DELIVERED)),
            (rep.as_str(), &DeliveryError::Repeated(CANCELLED, "c".into())),
            (partial.as_str(), &DeliveryError::Unknown(DELIVERED, "ghost".into())),
            (bad.as_str(), &DeliveryError::Malformed(DELIVERED, "message_ids is not a list of strings")),
        ]
    );
}

/// §11: a delivery fact takes effect wherever it stands, on an abandoned
/// branch too.
#[test]
fn a_delivery_fact_on_an_abandoned_branch_takes_effect() {
    let dir = tempfile::tempdir().unwrap();
    let (path, mut ap) = new_ledger(dir.path());
    ap.append(Event::new("t1", Kind::Text).with("role", "user").with("text", "one")).unwrap();
    queue(&mut ap, "m");
    delivery::delivered(&mut ap, &ids(&["m"]), None, None).unwrap();
    ap.append(Event::new("t2", Kind::Text).with("role", "user").with("text", "rewound").after(Some("t1"))).unwrap();
    let l = on_disk(&path);
    let on: Vec<String> = l.active_path().iter().map(|e| e.id.clone()).collect();
    assert_eq!(on, ids(&["t1", "t2"]), "the facts are on an abandoned branch");
    assert_eq!(delivery::state(&l).state_of("m"), Some(State::Delivered));
}

/// Redaction (§11.1) changes no delivery state and no history: not of a
/// protoledger/message, a delivery fact, or the text event that carried it.
#[test]
fn redaction_leaves_delivery_state_and_history_alone() {
    let dir = tempfile::tempdir().unwrap();
    let (path, mut ap) = new_ledger(dir.path());
    queue(&mut ap, "m1");
    queue(&mut ap, "m2");
    ap.append(Event::new("prompt", Kind::Text).with("role", "user").with("text", "[#m1] text of m1")).unwrap();
    delivery::delivered(&mut ap, &ids(&["m1"]), Some("turn"), Some("prompt")).unwrap();
    delivery::cancelled(&mut ap, &ids(&["m2"]), "interrupt").unwrap();
    let before = delivery::state(&on_disk(&path));
    assert_eq!(states(&before), vec![("m1".into(), State::Delivered), ("m2".into(), State::Cancelled)]);
    let facts: Vec<String> = on_disk(&path).events.iter().filter(|e| fact_type(e).is_some() || e.id == "prompt").map(|e| e.id.clone()).collect();
    assert_eq!(facts.len(), 5);
    ap.redaction(false, &facts, "scrub").unwrap();
    let l = on_disk(&path);
    for f in &facts {
        assert!(l.redacted().contains(f), "{f} is not redacted: the redaction did not take");
    }
    assert!(!l.projected_view().iter().any(|e| e.id == "prompt"), "the redacted prompt is still in the view");
    assert_eq!(delivery::state(&l), before);
}

// ---- the property: no message is ever lost, and each is sent at most once ----

struct Rng(u64);
impl Rng {
    fn next(&mut self) -> u64 {
        self.0 ^= self.0 << 13;
        self.0 ^= self.0 >> 7;
        self.0 ^= self.0 << 17;
        self.0
    }
    fn below(&mut self, n: usize) -> usize {
        (self.next() % n as u64) as usize
    }
}

/// An oracle written from SPEC §11.3's table, independent of the crate.
#[derive(Default)]
struct Model {
    queued: Vec<String>,
    state: HashMap<String, State>,
    history: HashMap<String, Vec<Act>>,
}

impl Model {
    fn message(&mut self, id: &str) -> Result<(), &'static str> {
        if self.state.contains_key(id) {
            return Err("duplicate");
        }
        self.queued.push(id.to_string());
        self.state.insert(id.to_string(), State::Pending);
        self.history.insert(id.to_string(), vec![]);
        Ok(())
    }
    fn act(&mut self, act: Act, list: &[String]) -> Result<(), &'static str> {
        if list.is_empty() {
            return Err("empty");
        }
        for (i, id) in list.iter().enumerate() {
            if list[..i].contains(id) {
                return Err("repeated");
            }
            match self.state.get(id) {
                None => return Err("unknown"),
                Some(State::Delivered | State::Withdrawn) => return Err("final"),
                _ => {}
            }
        }
        for id in list {
            let to = match act {
                Act::Delivered => State::Delivered,
                Act::Cancelled => State::Cancelled,
                Act::Withdrawn => State::Withdrawn,
            };
            self.state.insert(id.clone(), to);
            self.history.get_mut(id).unwrap().push(act);
        }
        Ok(())
    }
}

fn tag(e: &DeliveryError) -> &'static str {
    match e {
        DeliveryError::Duplicate(_) => "duplicate",
        DeliveryError::Empty(_) => "empty",
        DeliveryError::Repeated(..) => "repeated",
        DeliveryError::Unknown(..) => "unknown",
        DeliveryError::Final(..) => "final",
        DeliveryError::Malformed(..) => "malformed",
    }
}

/// Every message the model queued is in the derived state exactly once, in
/// the state the model says, with the model's history; it is sent at most
/// once and is sent exactly when it is delivered; should-send is queued −
/// delivered − withdrawn (so a cancelled message is owed).
fn check_against(m: &Model, d: &Delivery, at: &str) {
    let got: Vec<&str> = d.messages.iter().map(|x| x.message_id.as_str()).collect();
    let want: Vec<&str> = m.queued.iter().map(String::as_str).collect();
    assert_eq!(got, want, "{at}: a message was lost, duplicated or reordered");
    for x in &d.messages {
        assert_eq!(x.state, m.state[&x.message_id], "{at}: {} is in the wrong state", x.message_id);
        let acts: Vec<Act> = x.history.iter().map(|s| s.act).collect();
        assert_eq!(acts, m.history[&x.message_id], "{at}: {}'s history", x.message_id);
        let sends = acts.iter().filter(|a| **a == Act::Delivered).count();
        assert!(sends <= 1, "{at}: {} sent {sends} times", x.message_id);
        assert_eq!(sends == 1, x.state == State::Delivered, "{at}: {} sent iff delivered", x.message_id);
    }
    let owed: Vec<&str> = m.queued.iter().filter(|id| !matches!(m.state[*id], State::Delivered | State::Withdrawn)).map(String::as_str).collect();
    let should: Vec<&str> = d.should_send().iter().map(|x| x.message_id.as_str()).collect();
    assert_eq!(should, owed, "{at}: should send is not queued − delivered − withdrawn");
    assert!(d.ignored.is_empty(), "{at}: the writer wrote a fact a reader ignores: {:?}", d.ignored);
}

#[test]
fn property_no_message_is_lost_and_each_is_sent_at_most_once_over_random_message_deliver_cancel_withdraw() {
    let pool = ["m0", "m1", "m2", "m3", "m4", "m5", "ghost"];
    let mut accepted: HashMap<&str, usize> = HashMap::new();
    let mut refusals: HashMap<&'static str, usize> = HashMap::new();
    for seed in 1..=120u64 {
        let dir = tempfile::tempdir().unwrap();
        let (path, mut ap) = new_ledger(dir.path());
        let mut rng = Rng(seed.wrapping_mul(0x9E37_79B9_7F4A_7C15) | 1);
        let mut model = Model::default();
        for op in 0..40 {
            let before = std::fs::read(&path).unwrap();
            let kind = rng.below(10);
            let (name, ours, theirs) = if kind < 3 {
                let id = pool[rng.below(pool.len() - 1)];
                ("message", delivery::message(&mut ap, id, "t", &Obj::new()), model.message(id))
            } else {
                let n = rng.below(4);
                let list: Vec<String> = (0..n).map(|_| pool[rng.below(pool.len())].to_string()).collect();
                let act = [Act::Delivered, Act::Cancelled, Act::Cancelled, Act::Withdrawn][rng.below(4)];
                let r = match act {
                    Act::Delivered => delivery::delivered(&mut ap, &list, None, None),
                    Act::Cancelled => delivery::cancelled(&mut ap, &list, "interrupt"),
                    Act::Withdrawn => delivery::withdrawn(&mut ap, &list, "retracted"),
                };
                (act.fact(), r, model.act(act, &list))
            };
            let at = format!("seed {seed} op {op} ({name})");
            let after = std::fs::read(&path).unwrap();
            assert!(after.starts_with(&before), "{at}: an earlier byte of the ledger changed");
            match (ours, theirs) {
                (Ok(_), Ok(())) => {
                    assert!(after.len() > before.len(), "{at}: accepted and appended nothing");
                    *accepted.entry(name).or_default() += 1;
                }
                (Err(e), Err(why)) => {
                    let e = e.downcast::<DeliveryError>().expect("a DeliveryError");
                    assert_eq!(tag(&e), why, "{at}: refused for the wrong reason: {e}");
                    assert_eq!(after.len(), before.len(), "{at}: a refusal appended");
                    assert_eq!(after, before, "{at}: a refusal changed the file");
                    *refusals.entry(why).or_default() += 1;
                }
                (ours, theirs) => panic!("{at}: crate {:?}, SPEC table {theirs:?}", ours.map(|_| ()).map_err(|e| e.to_string())),
            }
            check_against(&model, &delivery::state(&on_disk(&path)), &at);
        }
        // Delivering everything owed leaves nothing owed and every message
        // that was not withdrawn sent exactly once.
        let owed: Vec<String> = delivery::state(ap.ledger()).should_send().iter().map(|m| m.message_id.clone()).collect();
        if !owed.is_empty() {
            delivery::delivered(&mut ap, &owed, None, None).unwrap();
            model.act(Act::Delivered, &owed).unwrap();
        }
        let d = delivery::state(&on_disk(&path));
        check_against(&model, &d, &format!("seed {seed} end"));
        assert!(d.should_send().is_empty());
        assert_eq!(d.sent().len() + d.messages.iter().filter(|m| m.state == State::Withdrawn).count(), d.messages.len());
    }
    // The generator reached every accepted act and every refusal.
    for k in ["message", DELIVERED, CANCELLED, WITHDRAWN] {
        assert!(accepted.get(k).copied().unwrap_or(0) > 0, "no accepted {k}: {accepted:?}");
    }
    for k in ["duplicate", "empty", "repeated", "unknown", "final"] {
        assert!(refusals.get(k).copied().unwrap_or(0) > 0, "no {k} refusal: {refusals:?}");
    }
    eprintln!("delivery property: accepted {accepted:?}, refused {refusals:?}");
}

#[cfg(feature = "store")]
mod store {
    use super::*;
    use protoledger::store::Store;

    /// The store's helpers append through the agent's one writer, are
    /// indexed like any event, refuse as the Appender does (appending
    /// nothing, indexing nothing), and derive the same state as the file.
    #[test]
    fn store_helpers_append_index_refuse_and_derive_the_file_s_state() {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path().join("store");
        let mut s = Store::open(&root).unwrap();
        s.create_agent(agent("a")).unwrap();
        s.message("a", "m1", "hello", &Obj::new()).unwrap();
        s.message("a", "m2", "again", &Obj::new()).unwrap();
        s.cancelled("a", &ids(&["m1", "m2"]), "interrupt").unwrap();
        s.delivered("a", &ids(&["m1"]), Some("turn"), None).unwrap();
        s.withdrawn("a", &ids(&["m2"]), "retracted").unwrap();
        let path = s.ledger_path("a").unwrap();
        let rows = s.events("a", 0, 100).unwrap().len();
        let bytes = std::fs::read(&path).unwrap();
        for r in [
            s.message("a", "m1", "dup", &Obj::new()),
            s.delivered("a", &ids(&["m1"]), None, None),
            s.cancelled("a", &[], "x"),
            s.withdrawn("a", &ids(&["m2"]), "x"),
            s.delivered("a", &ids(&["nope"]), None, None),
        ] {
            r.expect_err("refused").downcast::<DeliveryError>().expect("a DeliveryError");
        }
        assert_eq!(std::fs::read(&path).unwrap().len(), bytes.len(), "a refusal through the store appended");
        assert_eq!(std::fs::read(&path).unwrap(), bytes, "a refusal through the store changed the file");
        assert_eq!(s.events("a", 0, 100).unwrap().len(), rows, "a refusal was indexed");
        assert_eq!(rows, 5, "every accepted fact is an indexed event");
        let d = s.delivery("a").unwrap();
        assert_eq!(states(&d), vec![("m1".into(), State::Delivered), ("m2".into(), State::Withdrawn)]);
        assert_eq!(history(&d, "m1"), vec![Act::Cancelled, Act::Delivered]);
        assert_eq!(delivery::state(&on_disk(&path)), d);
        drop(s);
        let mut s = Store::open(&root).unwrap();
        s.reindex().unwrap();
        assert_eq!(s.delivery("a").unwrap(), d, "the state is derived from the ledger alone");
    }
}
