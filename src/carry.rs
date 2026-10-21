//! Carriers (SPEC §6): protoledger events stowed inside a target format's
//! native record, so a round trip through a format that cannot express them
//! still comes back exact.
//!
//! A carrier is `{"v":"0.1","h":"<hash of the record without the carrier>","events":[...]}`
//! under the key `protoledger`. On re-import the record is re-hashed without
//! its carrier: if the hash still matches, nothing in the harness edited the
//! record, and the carried events are restored exactly. If it does not match,
//! the record is imported fresh.
//!
//! A carrier lives inside a parsed record (`Value`), and a `Value` does not
//! keep a native record's exact text (protoledger builds serde_json without
//! `preserve_order` and `arbitrary_precision`). So a
//! carried event whose native record would not re-serialise to its own text
//! also has that text under `"records": {"<index in events>": "<text>"}`,
//! and [`take`] restores it from there: carried events come back exactly as a
//! ledger line would hold them.

use crate::model::{Agent, Event, SPEC_VERSION};
use crate::util::OrderedMap;
use serde_json::{Value, json};
use sha2::{Digest, Sha256};

pub const KEY: &str = "protoledger";

/// SHA-256 over the canonical compact JSON of `record` minus any carrier, as
/// it will be re-read: serialised and parsed until a text repeats, then keys
/// sorted. A parser may re-spell a number (without serde_json's
/// `float_roundtrip`, `0.011300000000000001` parses as `0.0113`, and
/// `1.898781802818836e-8` and `1.8987818028188362e-8` turn into each other
/// forever), so the record hashed at attach is the one an importer will see at
/// take.
///
/// The hash is therefore a function of the record AS PARSED, not of the text a
/// harness wrote: `{"x":0.011300000000000001}` and
/// `{"x":0.0113}` hash the same, so an edit below the parser's precision is
/// not detected and the carrier is restored over it. That is intended: the
/// importer cannot tell the two apart either.
pub fn record_hash(record: &Value) -> String {
    let mut r = record.clone();
    strip(&mut r);
    let bytes = serde_json::to_vec(&canonical(&as_reread(r))).expect("json");
    hex::encode(&Sha256::digest(&bytes)[..12])
}

/// One re-read: a text as an importer writes it back after parsing it.
fn reread_text(t: &str) -> String {
    let back: Value = serde_json::from_str(t).expect("serialised JSON parses");
    serde_json::to_string(&back).expect("json")
}

/// How many re-reads [`as_reread`] and [`on_reread_path`] allow before they
/// fail loudly. The proof below says the walk ends; this bound is a tripwire
/// for a mistake in it (or a serde_json whose parser behaves differently),
/// not a part of the algorithm. The longest tail plus cycle measured over
/// 1,000,000 random f64 is 100 passes; `reread_cap_is_far_above_any_measured_tail`
/// holds the cap to at least 100 times what it measures.
const REREAD_CAP: usize = 1 << 16;

/// Panic for a record whose re-read walk outran `cap` passes.
fn reread_cap_exceeded(cap: usize, t: &str) -> ! {
    let shown: String = t.chars().take(300).collect();
    panic!("protoledger: a record's text still changes after {cap} serialise/parse passes; refusing to guess its fixed point (record starts: {shown})")
}

/// `v` once serialising and parsing it no longer produces a new text. Its
/// re-read sequence ends in a cycle `C` (proof below); this returns the value
/// parsed from the lexicographically smallest text `m` of `C`. That value
/// serialises to `f(m)`, which is itself a member of `C` (for a 2-cycle, the
/// other member, not `m`): what matters is that the result is a function of
/// the set `C` alone. The walk ends by the proof; [`REREAD_CAP`] passes is a
/// tripwire that panics, naming the record, if it ever does not.
///
/// Why this is a true fixed point, so attach (which hashes the record it holds)
/// and take (which hashes that record after any number of write/read hops)
/// always hash the same text:
/// 1. Let `f(t) = to_string(from_str(t))` and `S0 = to_string(v)`,
///    `S(k+1) = f(Sk)`. Every `Sk` for `k >= 1` serialises a parsed `Value`
///    with the shape, keys, strings and integers of `S0`, and in each float
///    slot one of the 2^64 f64 values written in its one shortest spelling
///    (under `arbitrary_precision` a number keeps its text, so `f` fixes it).
///    So the `Sk` lie in a finite set, `f` is a function on it, and the
///    sequence must revisit a text: it ends in exactly one cycle `C`, and the
///    cycle search below (Brent's) terminates. A number may drift for many
///    passes before entering `C`: `-8.926968263417197e+161` takes about 90,
///    which is why a pass cap is wrong.
/// 2. A number's spelling does not depend on what surrounds it, so a record an
///    importer parses back from an attached text serialises to `f(S0)` once
///    the carrier is removed, and after `k` hops to `Sk`. The sequence from
///    `Sk` is the sequence from `S0` minus its first `k` texts: it ends in the
///    same `C`.
/// 3. `C` as a set does not depend on where it is entered, and its smallest
///    text `m` is a function of that set alone. So every `Sk` gives the same
///    `m`, and the same value parsed from it.
///
/// For any record whose sequence repeats within 64 passes this is the same
/// value the earlier 64-pass version returned, so carriers already written
/// keep their hash.
fn as_reread(v: Value) -> Value {
    as_reread_within(v, REREAD_CAP)
}

/// [`as_reread`] with its tripwire at `cap` passes (tests lower it).
fn as_reread_within(v: Value, cap: usize) -> Value {
    let f = reread_text;
    // Brent's cycle search: `tortoise` ends on the cycle and `len` is its length.
    let (mut power, mut len, mut passes) = (1usize, 1usize, 1usize);
    let mut tortoise = serde_json::to_string(&v).expect("json");
    let mut hare = f(&tortoise);
    while tortoise != hare {
        passes += 1;
        if passes > cap {
            reread_cap_exceeded(cap, &serde_json::to_string(&v).expect("json"));
        }
        if power == len {
            tortoise = hare.clone();
            power *= 2;
            len = 0;
        }
        hare = f(&hare);
        len += 1;
    }
    let mut pick = tortoise.clone();
    let mut t = tortoise;
    for _ in 1..len {
        t = f(&t);
        if t < pick {
            pick = t.clone();
        }
    }
    serde_json::from_str(&pick).expect("serialised JSON parses")
}

/// Sort object keys recursively, so key order never affects the hash.
fn canonical(v: &Value) -> Value {
    match v {
        Value::Object(m) => {
            let mut keys: Vec<_> = m.keys().cloned().collect();
            keys.sort();
            let mut out = serde_json::Map::new();
            for k in keys {
                out.insert(k.clone(), canonical(&m[&k]));
            }
            Value::Object(out)
        }
        Value::Array(a) => Value::Array(a.iter().map(canonical).collect()),
        other => other.clone(),
    }
}

/// Attach a carrier for `events` to an object record (under `KEY`, or under a
/// nested object path such as `["metadata"]` when the format only tolerates
/// extra fields there).
pub fn attach(record: &mut Value, at: &[&str], events: &[Event]) {
    attach_with(record, at, events, None);
}

/// Like [`attach`], also carrying the agent header. Exporters attach the
/// header once per agent (on its first record) when the target format cannot
/// express it: links, metadata, the original agent id.
pub fn attach_with(record: &mut Value, at: &[&str], events: &[Event], agent: Option<&Agent>) {
    if events.is_empty() && agent.is_none() {
        return;
    }
    let h = record_hash(record);
    let mut carrier = json!({"v": SPEC_VERSION, "h": h, "events": events});
    let records = exact_records(events, &carrier["events"]);
    if !records.is_empty() {
        carrier["records"] = Value::Object(records);
    }
    if let Some(a) = agent {
        carrier["agent"] = serde_json::to_value(a).expect("agent serialises");
    }
    let mut cur = record;
    for k in at {
        if !cur.get(*k).is_some_and(Value::is_object) {
            cur[*k] = json!({});
        }
        cur = cur.get_mut(*k).unwrap();
    }
    if let Value::Object(m) = cur {
        m.insert(KEY.to_string(), carrier);
    }
}

/// Is `v` shaped like a carrier [`attach`] writes (`h` a string, `events` an
/// array)? A member named `protoledger` that is not is the record's own data
/// (the rule `util::raw_matches` applies too).
pub fn is_carrier(v: &Value) -> bool {
    v.get("h").is_some_and(Value::is_string) && v.get("events").is_some_and(Value::is_array)
}

/// Remove every carrier from a record, wherever it sits, and say whether
/// there was one. A `protoledger` member that is not shaped like a carrier is
/// left where it is.
pub fn strip(record: &mut Value) -> bool {
    let mut found = false;
    if let Value::Object(m) = record {
        if m.get(KEY).is_some_and(is_carrier) {
            m.remove_ordered(KEY);
            found = true;
        }
        for v in m.values_mut() {
            if v.is_object() {
                found |= strip(v);
            }
        }
    }
    found
}

/// Take the carrier out of `record` (at the same nesting as `attach`). Returns
/// the carried events if the record was not edited since they were attached.
pub fn take(record: &mut Value, at: &[&str]) -> Option<Vec<Event>> {
    take_with(record, at).map(|c| c.events)
}

/// What a carrier held.
#[derive(Debug, Clone)]
pub struct Carried {
    pub events: Vec<Event>,
    pub agent: Option<Agent>,
}

/// Like [`take`], also returning a carried agent header.
pub fn take_with(record: &mut Value, at: &[&str]) -> Option<Carried> {
    let mut cur: &mut Value = record;
    for k in at {
        cur = cur.get_mut(*k)?;
    }
    let carrier = cur.as_object_mut()?.remove_ordered(KEY)?;
    // Hash with the carrier gone; an empty container created only to hold the
    // carrier must also go, so the hash matches the record as it was attached.
    if !at.is_empty() {
        prune_empty(record, at);
    }
    let h = carrier.get("h")?.as_str()?.to_string();
    if record_hash(record) != h {
        return None;
    }
    // Through text, not from_value: from_value re-spells some arbitrary-precision
    // numbers (0.000005 → 5e-6), which would break byte-exact re-export.
    let mut events: Vec<Event> = carrier.get("events").map_or(Some(vec![]), |e| serde_json::from_str(&e.to_string()).ok())?;
    if let Some(Value::Object(records)) = carrier.get("records") {
        restore_records(&mut events, records);
    }
    let agent = carrier.get("agent").and_then(|a| serde_json::from_str(&a.to_string()).ok());
    Some(Carried { events, agent })
}

/// The exact text of each carried native record that the carrier's `Value`
/// (`carried`, the serialised `events`) would not re-serialise to, keyed by
/// the event's index.
fn exact_records(events: &[Event], carried: &Value) -> serde_json::Map<String, Value> {
    let mut out = serde_json::Map::new();
    for (i, ev) in events.iter().enumerate() {
        let Some(text) = ev.native.as_ref().and_then(|n| n.record_text()) else { continue };
        let held = carried.get(i).and_then(|e| e.get("native")).and_then(|n| n.get("record"));
        if held.map(|r| serde_json::to_string(r).expect("json")).as_deref() != Some(text) {
            out.insert(i.to_string(), Value::String(text.to_string()));
        }
    }
    out
}

/// Put back the exact text of carried native records ([`exact_records`]); a
/// text is used only where the carried copy of the record is still what that
/// text becomes when nobody edits it, and is ignored otherwise.
///
/// The carried copy has been parsed more times than the text (once per
/// write/read hop), and a parse may re-spell a number (no `arbitrary_precision`),
/// so an unedited copy is not always the text's own parse. It is always on the
/// text's FORWARD re-read path, though: `parse(text)`, then that serialised
/// and parsed, and so on, until the texts cycle ([`on_reread_path`]). That
/// path, plus [`same_json`](crate::util::same_json) (within 2 ulps, key
/// order and whitespace aside), is all that is accepted. Comparing the two at
/// their common fixed point instead would accept every value whose drift ends
/// in the same cycle, an edit of 3 ulps or more included.
fn restore_records(events: &mut [Event], records: &serde_json::Map<String, Value>) {
    for (i, text) in records {
        let (Ok(i), Some(text)) = (i.parse::<usize>(), text.as_str()) else { continue };
        let Some(n) = events.get_mut(i).and_then(|e| e.native.as_mut()) else { continue };
        let (Some(carried_text), Some(carried)) = (n.record_text(), n.record_json()) else { continue };
        if !crate::util::same_json(carried_text, text) && !on_reread_path(text, &carried, REREAD_CAP) {
            continue;
        }
        if let Ok(exact) = n.clone().record_raw(text) {
            *n = exact;
        }
    }
}

/// Whether `target` is one of the values `text` passes through when it is
/// parsed, serialised and parsed again, until its texts repeat (so the whole
/// tail and the whole cycle are walked, and nothing beyond them can be
/// reached). False for a text that is not JSON. Panics past `cap` passes, as
/// [`as_reread_within`] does.
fn on_reread_path(text: &str, target: &Value, cap: usize) -> bool {
    let mut seen = std::collections::HashSet::new();
    let mut t = text.to_string();
    for _ in 0..cap {
        let Ok(v) = serde_json::from_str::<Value>(&t) else { return false };
        if &v == target {
            return true;
        }
        let next = serde_json::to_string(&v).expect("json");
        if !seen.insert(next.clone()) {
            return false;
        }
        t = next;
    }
    reread_cap_exceeded(cap, text)
}

fn prune_empty(record: &mut Value, at: &[&str]) {
    if at.is_empty() {
        return;
    }
    let (last, init) = at.split_last().unwrap();
    let mut cur: &mut Value = record;
    for k in init {
        match cur.get_mut(*k) {
            Some(v) => cur = v,
            None => return,
        }
    }
    if cur.get(*last).is_some_and(|v| v.as_object().is_some_and(|m| m.is_empty()))
        && let Value::Object(m) = cur
    {
        m.remove_ordered(last);
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::model::{Kind, Native};

    #[test]
    fn carrier_restores_when_unedited_and_not_when_edited() {
        let ev = Event::new("e1", Kind::Text).with("role", "user").with("text", "hi");
        let original = json!({"type":"user","message":{"content":"hi"}});
        let mut rec = original.clone();
        attach(&mut rec, &[], std::slice::from_ref(&ev));
        let mut a = rec.clone();
        assert_eq!(take(&mut a, &[]).unwrap()[0].id, "e1");
        assert_eq!(a, original);
        let mut b = rec.clone();
        b["message"]["content"] = json!("edited");
        assert!(take(&mut b, &[]).is_none());
    }

    /// The record as an importer sees it: written to text and parsed back.
    fn reread(rec: &Value) -> Value {
        serde_json::from_str(&serde_json::to_string(rec).unwrap()).unwrap()
    }

    #[test]
    fn carried_numbers_keep_their_spelling() {
        // A number in a native record is held as text and keeps its spelling
        // through a carrier; a number in an event body is a `Value` (model.rs)
        // and is spelled by the ledger the same way with or without a carrier.
        let src = r#"{"cost_usd":0.000005}"#;
        let ev = Event::new("e1", Kind::Usage)
            .with("cost_usd", serde_json::from_str::<Value>("0.000005").unwrap())
            .native(Native::new("x", None).record_raw(src).unwrap());
        let mut rec = json!({"a": 1});
        attach(&mut rec, &[], std::slice::from_ref(&ev));
        let mut rec = reread(&rec);
        let back = take(&mut rec, &[]).unwrap();
        assert_eq!(back[0].native.as_ref().unwrap().record_text(), Some(src));
        assert_eq!(serde_json::to_string(&back[0]).unwrap(), serde_json::to_string(&ev).unwrap(), "the carried event is not the ledger's spelling of it");
    }

    #[test]
    fn carrier_survives_a_parser_that_respells_a_number() {
        let ev = Event::new("e1", Kind::Usage).with("cost_usd", 1);
        let rec = json!({"type": "step", "cost_usd": 0.011300000000000001_f64});
        let mut attached = rec.clone();
        attach(&mut attached, &[], std::slice::from_ref(&ev));
        let read = reread(&attached);
        // Sentinel: without arbitrary_precision the parser really re-spells the
        // number, so the record taken is not the record attached.
        {
            let mut r = read.clone();
            strip(&mut r);
            assert_ne!(r, rec, "the parser did not re-spell 0.011300000000000001");
            assert_eq!(serde_json::to_string(&r["cost_usd"]).unwrap(), "0.0113");
        }
        let mut a = read.clone();
        assert_eq!(take(&mut a, &[]).expect("carried events restored")[0].id, "e1");
        // Opposed: a record whose content changed between attach and take is not restored.
        let mut b = read.clone();
        b["cost_usd"] = json!(0.0114);
        assert!(take(&mut b, &[]).is_none(), "an edited record restored its carrier");
    }

    #[test]
    fn carrier_survives_a_number_the_parser_respells_back_and_forth() {
        let src = r#"{"type":"step","cost":1.898781802818836e-8}"#;
        let rec: Value = serde_json::from_str(src).unwrap();
        // Sentinel: without arbitrary_precision this number never settles: its
        // text alternates between two spellings, so an even pass count leaves
        // attach and take on opposite spellings.
        {
            let pass = |t: &str| serde_json::to_string(&serde_json::from_str::<Value>(t).unwrap()).unwrap();
            let t0 = serde_json::to_string(&rec).unwrap();
            let t1 = pass(&t0);
            assert_ne!(t0, t1, "the parser did not re-spell {src}");
            assert_eq!(pass(&t1), t0, "the re-spelling is not a 2-cycle");
        }
        let ev = Event::new("e1", Kind::Usage).with("cost_usd", 1);
        let mut attached = rec.clone();
        attach(&mut attached, &[], std::slice::from_ref(&ev));
        let mut a = reread(&attached);
        assert_eq!(take(&mut a, &[]).expect("carried events restored")[0].id, "e1");
        // Opposed: an edited record is still not restored.
        let mut b = reread(&attached);
        b["cost"] = json!(2.5e-8);
        assert!(take(&mut b, &[]).is_none(), "an edited record restored its carrier");
    }

    /// A deterministic xorshift64* stream, so the census is the same every run.
    fn xorshift(state: &mut u64) -> u64 {
        *state ^= *state >> 12;
        *state ^= *state << 25;
        *state ^= *state >> 27;
        state.wrapping_mul(0x2545_f491_4f6c_dd1d)
    }

    #[test]
    fn record_hash_census_attach_equals_take_over_20000_f64() {
        let mut seed: u64 = 0x5eed_0001_2026_1017;
        let (mut respelled, mut cycling, mut collided, mut mismatched) = (0usize, 0usize, 0usize, Vec::new());
        let pass = |t: &str| serde_json::to_string(&serde_json::from_str::<Value>(t).unwrap()).unwrap();
        for i in 0..20_000 {
            // Random finite f64 over the whole bit range, plus every 2nd one
            // scaled into decimal exponents [-30, 30].
            let mut x = f64::from_bits(xorshift(&mut seed));
            while !x.is_finite() {
                x = f64::from_bits(xorshift(&mut seed));
            }
            if i % 2 == 0 {
                let m = (xorshift(&mut seed) >> 11) as f64 / (1u64 << 53) as f64 + 1.0;
                let e = (xorshift(&mut seed) % 61) as i32 - 30;
                x = m * 10f64.powi(e);
            }
            let rec = json!({"type": "step", "n": i, "x": x});
            let t0 = serde_json::to_string(&rec).unwrap();
            let t1 = pass(&t0);
            if t1 != t0 {
                respelled += 1;
                if pass(&t1) == t0 {
                    cycling += 1;
                }
            }
            // attach hashes `rec`; take hashes what an importer parsed from its text.
            let read: Value = serde_json::from_str(&t0).unwrap();
            if record_hash(&rec) != record_hash(&read) {
                mismatched.push(t0);
            }
            // Opposed: the same record with different content hashes differently.
            if record_hash(&rec) == record_hash(&json!({"type": "step", "n": i + 1, "x": x})) {
                collided += 1;
            }
        }
        eprintln!("census: 20000 records, respelled={respelled} cycling={cycling} mismatched={} collided={collided}", mismatched.len());
        assert_eq!(collided, 0, "records with different content hashed the same");
        // Sentinel: without arbitrary_precision the census reaches both shapes
        // of re-spelling, so it could fail.
        assert!(respelled > 0 && cycling > 0, "census reached no re-spelling: respelled={respelled} cycling={cycling}");
        let _ = (respelled, cycling);
        assert!(
            mismatched.is_empty(),
            "{} of 20000 records hash differently at attach and take, e.g. {:?}",
            mismatched.len(),
            &mismatched[..mismatched.len().min(3)]
        );
    }

    #[test]
    fn carried_native_record_comes_back_byte_identical() {
        let src = r#"{"b":1,"a":1.0,"d":1e3,"g":"\u00e9", "f":[1 ,2]}"#;
        let ev = Event::new("e1", Kind::Native).native(Native::new("x", Some("1".into())).record_raw(src).unwrap());
        let mut rec = json!({"type": "x"});
        attach(&mut rec, &[], std::slice::from_ref(&ev));
        let mut rec = reread(&rec);
        // Sentinel: the carrier's parsed copy of the record is not the source text.
        let parsed_copy = serde_json::to_string(&rec[KEY]["events"][0]["native"]["record"]).unwrap();
        assert_ne!(parsed_copy, src);
        let mut tampered = rec.clone();
        let back = take(&mut rec, &[]).unwrap();
        assert_eq!(back[0].native.as_ref().unwrap().record_text(), Some(src));
        let line = serde_json::to_string(&back[0]).unwrap();
        assert!(line.contains(src), "ledger line does not hold the record verbatim: {line}");
        // Opposed: an exact text that is not the same JSON as the carried record is not used.
        tampered[KEY]["records"]["0"] = json!(r#"{"b":2}"#);
        let back = take(&mut tampered, &[]).unwrap();
        assert_eq!(back[0].native.as_ref().unwrap().record_text(), Some(parsed_copy.as_str()));
    }

    #[test]
    fn nested_carrier_prunes_its_container() {
        let ev = Event::new("e1", Kind::Text);
        let original = json!({"id":"m1"});
        let mut rec = original.clone();
        attach(&mut rec, &["metadata"], std::slice::from_ref(&ev));
        assert!(rec["metadata"][KEY].is_object());
        let mut a = rec.clone();
        assert!(take(&mut a, &["metadata"]).is_some());
        assert_eq!(a, original);
    }

    /// Serialise → parse passes until `rec`'s text first repeats one it had
    /// before (its re-read sequence's tail plus one cycle).
    fn passes_to_repeat(rec: &Value) -> usize {
        let mut seen = vec![serde_json::to_string(rec).unwrap()];
        loop {
            let next = serde_json::to_string(&serde_json::from_str::<Value>(seen.last().unwrap()).unwrap()).unwrap();
            if seen.contains(&next) {
                return seen.len();
            }
            assert!(seen.len() < 100_000, "no repeat in 100000 passes");
            seen.push(next);
        }
    }

    #[test]
    fn carrier_survives_a_number_that_drifts_past_any_pass_cap() {
        // -8.926968263417197e+161 drifts for about 90 passes before its text
        // repeats; 0.5 settles in one. Both must restore
        // byte-exact, after one hop and after several.
        for (x, drifts) in [(-8.926968263417197e+161_f64, true), (0.5_f64, false)] {
            let rec = json!({"type": "step", "x": x});
            // Sentinel: without arbitrary_precision the hard value really needs
            // more passes than the old cap of 64, and the easy one needs one.
            {
                let p = passes_to_repeat(&rec);
                if drifts {
                    assert!(p > 64, "{x:e} repeats after {p} passes, within the old cap");
                } else {
                    assert_eq!(p, 1, "{x:e} does not settle in one pass");
                }
            }
            let _ = drifts;
            let text = serde_json::to_string(&rec).unwrap();
            let ev = Event::new("e1", Kind::Native).native(Native::new("x", None).record_raw(&text).unwrap());
            let mut attached = rec.clone();
            attach(&mut attached, &[], std::slice::from_ref(&ev));
            let mut hop = reread(&attached);
            for k in 1..=3 {
                let mut r = hop.clone();
                let back = take(&mut r, &[]).unwrap_or_else(|| panic!("{x:e}: carried events not restored after {k} hop(s)"));
                assert_eq!(back[0].native.as_ref().unwrap().record_text(), Some(text.as_str()), "{x:e}: native record text after {k} hop(s)");
                assert_eq!(
                    serde_json::to_string(&back).unwrap(),
                    serde_json::to_string(std::slice::from_ref(&ev)).unwrap(),
                    "{x:e}: carried events not byte-exact after {k} hop(s)"
                );
                // Opposed: an edited record is not restored.
                let mut edited = hop.clone();
                edited["type"] = json!("edited");
                assert!(take(&mut edited, &[]).is_none(), "{x:e}: an edited record restored its carrier");
                hop = reread(&hop);
            }
        }
    }

    /// 1,000,000 finite f64 bit patterns, each in a record whose carrier holds
    /// that record's own native text, written, re-read and taken after one and
    /// after two hops: a mismatch is a carrier not restored or a native text not
    /// byte-exact. Slow in a debug build:
    /// run with `--ignored`.
    #[test]
    #[ignore = "1,000,000-value sweep; run with --ignored"]
    fn attach_take_sweep_over_1_000_000_random_f64_bit_patterns() {
        const SEED: u64 = 0x5eed_0004_b202_6101;
        const N: usize = 1_000_000;
        let mut seed = SEED;
        let (mut past_cap, mut worst, mut restored_edited, mut mismatched) = (0usize, 0usize, 0usize, Vec::new());
        for _ in 0..N {
            let mut x = f64::from_bits(xorshift(&mut seed));
            while !x.is_finite() {
                x = f64::from_bits(xorshift(&mut seed));
            }
            let rec = json!({"type": "step", "x": x});
            let p = passes_to_repeat(&rec);
            worst = worst.max(p);
            if p > 64 {
                past_cap += 1;
            }
            let text = serde_json::to_string(&rec).unwrap();
            let ev = Event::new("e1", Kind::Native).native(Native::new("x", None).record_raw(&text).unwrap());
            let mut attached = rec.clone();
            attach(&mut attached, &[], std::slice::from_ref(&ev));
            let mut hop = reread(&attached);
            for k in 1..=2 {
                let mut r = hop.clone();
                match take(&mut r, &[]) {
                    None => mismatched.push(format!("{x:e} (hop {k}: not restored)")),
                    Some(back) if back[0].native.as_ref().unwrap().record_text() != Some(text.as_str()) => {
                        mismatched.push(format!("{x:e} (hop {k}: native text not exact)"))
                    }
                    Some(_) => {}
                }
                hop = reread(&hop);
            }
            // Opposed: the same record edited is not restored.
            let mut edited = reread(&attached);
            edited["type"] = json!("edited");
            if take(&mut edited, &[]).is_some() {
                restored_edited += 1;
            }
        }
        let not_restored = mismatched.iter().filter(|m| m.contains("not restored")).count();
        eprintln!(
            "sweep: seed={SEED:#x} values={N} past_cap_64={past_cap} worst_passes={worst} mismatched={} (not_restored={not_restored} text_not_exact={}) restored_edited={restored_edited}",
            mismatched.len(),
            mismatched.len() - not_restored
        );
        assert_eq!(restored_edited, 0, "edited records restored their carriers");
        // Sentinel: without arbitrary_precision the sweep reaches values past the
        // old 64-pass cap, so it could fail.
        assert!(past_cap > 0, "sweep reached no value past 64 passes");
        assert!(mismatched.is_empty(), "{} attach/take mismatches, e.g. {:?}", mismatched.len(), &mismatched[..mismatched.len().min(5)]);
    }

    /// The values a record passes through as it is re-read (`rec` itself first),
    /// until its text repeats: the whole tail and the whole cycle. Computed
    /// here, independently of `on_reread_path`.
    fn drift_path(rec: &Value) -> Vec<Value> {
        let mut path = vec![rec.clone()];
        let mut texts = vec![serde_json::to_string(rec).unwrap()];
        loop {
            let next = reread(path.last().unwrap());
            let t = serde_json::to_string(&next).unwrap();
            if texts.contains(&t) {
                return path;
            }
            assert!(texts.len() < 100_000, "no repeat in 100000 passes");
            texts.push(t);
            path.push(next);
        }
    }

    /// `rec` with its carrier's first native record replaced by `record`.
    fn with_carried_record(rec: &Value, record: Value) -> Value {
        let mut r = rec.clone();
        r[KEY]["events"][0]["native"]["record"] = record;
        r
    }

    /// Units in the last place between two finite f64 of the same sign.
    fn ulps(a: f64, b: f64) -> u64 {
        if a.is_sign_negative() != b.is_sign_negative() {
            return u64::MAX;
        }
        a.abs().to_bits().abs_diff(b.abs().to_bits())
    }

    #[test]
    fn an_edited_carried_record_off_its_drift_path_keeps_its_own_text() {
        // SPECIMEN_X's carrier holds its exact text under `records`. Its carried
        // copy edited by SPECIMEN_ULPS ulps reads back as a value that is not on
        // the text's re-read path and is more than `same_json`'s 2 ulps from it, yet its
        // own drift ends in the same cycle: a comparison at the common fixed
        // point restored the stale text over it.
        const SPECIMEN_X: f64 = 8.074952350948886e+161_f64;
        const SPECIMEN_ULPS: u64 = 10;
        let rec = json!({"type": "step", "x": SPECIMEN_X});
        let text = serde_json::to_string(&rec).unwrap();
        let ev = Event::new("e1", Kind::Native).native(Native::new("x", None).record_raw(&text).unwrap());
        let mut attached = rec.clone();
        attach(&mut attached, &[], std::slice::from_ref(&ev));
        let base = reread(&attached);
        assert!(base[KEY].get("records").is_some(), "the carrier holds no exact text, so restore_records is not reached");
        let xe = f64::from_bits(SPECIMEN_X.to_bits() + SPECIMEN_ULPS);
        let edited = reread(&with_carried_record(&base, json!({"type": "step", "x": xe})));
        // What take compares: the carried copy as the carrier's text gives it back.
        let seen = reread(&edited[KEY]["events"][0]["native"]["record"]);
        let path = drift_path(&serde_json::from_str(&text).unwrap());
        // Sentinel: the edit as read is off the path and an edit beyond `same_json`'s 2 ulps, and
        // (without arbitrary_precision) shares the text's fixed point, so the
        // old comparison could not tell it from drift.
        assert!(!path.contains(&seen), "the edit reads back onto the text's own drift path");
        let carried_text = serde_json::to_string(&edited[KEY]["events"][0]["native"]["record"]).unwrap();
        assert!(
            !crate::util::same_json(&carried_text, &text) && !crate::util::same_json(&serde_json::to_string(&seen).unwrap(), &text),
            "the edit is within same_json's 2 ulps"
        );
        assert!(path.iter().all(|p| ulps(p["x"].as_f64().unwrap(), seen["x"].as_f64().unwrap()) >= 3), "the edit is within 2 ulps of a point on the path");
        assert_eq!(as_reread(seen.clone()), as_reread(serde_json::from_str(&text).unwrap()), "the edit does not share the text's fixed point");
        let mut r = edited.clone();
        let back = take(&mut r, &[]).expect("the outer record is unedited, so the carrier is taken");
        let got = back[0].native.as_ref().unwrap().record_text().unwrap();
        assert_ne!(got, text, "the stale exact text was restored over an edited carried record");
        assert_eq!(got, carried_text, "the edited carried record did not come back as carried");
        // Opposed: an unedited carried copy at every point of the text's drift
        // path, parsed k times (k up to its whole tail and cycle, about 90 for
        // the long drifter), restores the exact text.
        for x in [SPECIMEN_X, -8.926968263417197e+161_f64] {
            let rec = json!({"type": "step", "x": x});
            let text = serde_json::to_string(&rec).unwrap();
            let ev = Event::new("e1", Kind::Native).native(Native::new("x", None).record_raw(&text).unwrap());
            let mut attached = rec.clone();
            attach(&mut attached, &[], std::slice::from_ref(&ev));
            let path = drift_path(&serde_json::from_str(&text).unwrap());
            assert!(path.len() > 1, "{x:e} does not drift");
            for (k, copy) in path.iter().enumerate() {
                let mut r = with_carried_record(&attached, copy.clone());
                let back = take(&mut r, &[]).unwrap_or_else(|| panic!("{x:e}: not restored with a copy parsed {k} times"));
                assert_eq!(back[0].native.as_ref().unwrap().record_text(), Some(text.as_str()), "{x:e}: exact text not restored from a copy parsed {k} times");
            }
        }
    }

    /// The census: 3,000 random full-range f64 records
    /// carrying their own exact text, each carried copy edited by 3, 5, 10 and
    /// 40 ulps. The exact text may come back only where what take compares is
    /// on the text's drift path or `same_json` to it; and it must come back for
    /// every unedited copy one and two hops along.
    #[test]
    fn exact_text_comes_back_only_on_the_drift_path_census_3000_f64() {
        let mut seed: u64 = 0x1234_5678_9abc_def1;
        let (mut tried, mut with_records, mut restored_on_path, mut drifted_onto_path) = (0usize, 0usize, 0usize, 0usize);
        let (mut stale, mut unedited_lost) = (Vec::new(), Vec::new());
        for _ in 0..3000 {
            let mut x = f64::from_bits(xorshift(&mut seed));
            while !x.is_finite() || x == 0.0 {
                x = f64::from_bits(xorshift(&mut seed));
            }
            let rec = json!({"type": "step", "x": x});
            let text = serde_json::to_string(&rec).unwrap();
            let ev = Event::new("e1", Kind::Native).native(Native::new("x", None).record_raw(&text).unwrap());
            let mut attached = rec.clone();
            attach(&mut attached, &[], std::slice::from_ref(&ev));
            let base = reread(&attached);
            if base[KEY].get("records").is_some() {
                with_records += 1;
            }
            let path = drift_path(&serde_json::from_str(&text).unwrap());
            let mut hop = base.clone();
            for k in 1..=2 {
                let mut r = hop.clone();
                if take(&mut r, &[]).and_then(|b| b[0].native.as_ref().unwrap().record_text().map(str::to_string)).as_deref() != Some(text.as_str()) {
                    unedited_lost.push(format!("{x:e} hop {k}"));
                }
                hop = reread(&hop);
            }
            for d in [3u64, 5, 10, 40] {
                let xe = f64::from_bits(x.to_bits() + d);
                if !xe.is_finite() || path.iter().any(|p| p["x"].as_f64() == Some(xe)) {
                    continue;
                }
                tried += 1;
                let edited = reread(&with_carried_record(&base, json!({"type": "step", "x": xe})));
                // What take sees: the carried text, and the value it parses to.
                let carried_text = serde_json::to_string(&edited[KEY]["events"][0]["native"]["record"]).unwrap();
                let seen: Value = serde_json::from_str(&carried_text).unwrap();
                let reachable = path.contains(&seen) || crate::util::same_json(&carried_text, &text);
                let mut r = edited.clone();
                let back = take(&mut r, &[]).expect("the outer record is unedited");
                if back[0].native.as_ref().unwrap().record_text() == Some(text.as_str()) {
                    if !reachable {
                        stale.push(format!("{x:e} +{d} ulps"));
                    } else {
                        restored_on_path += 1;
                        if path.contains(&seen) {
                            drifted_onto_path += 1;
                        }
                    }
                }
            }
        }
        eprintln!(
            "restore census: values=3000 with_records={with_records} edits_tried={tried} stale_restored={} restored_on_path_or_same_json={restored_on_path} (edit read back onto the path: {drifted_onto_path}) unedited_lost={}",
            stale.len(),
            unedited_lost.len()
        );
        // Sentinel: without arbitrary_precision exact texts are carried, so the
        // restore path is exercised and the census could fail.
        assert!(with_records > 500 && tried > 1000, "census reached too few carried texts: with_records={with_records} tried={tried}");
        assert!(
            unedited_lost.is_empty(),
            "{} unedited copies lost their exact text, e.g. {:?}",
            unedited_lost.len(),
            &unedited_lost[..unedited_lost.len().min(5)]
        );
        assert!(stale.is_empty(), "{} edited carried records got the stale exact text back, e.g. {:?}", stale.len(), &stale[..stale.len().min(5)]);
    }

    /// `restore_records`' `same_json`
    /// acceptance clause, guarded on its own. Every case here is OFF the
    /// text's drift path, so the drift-path clause cannot restore it and only
    /// `same_json` (within 2 ulps once both are read by `str::parse`)
    /// decides. Within 2 ulps the exact text comes back; at 3 or more it does
    /// not. The ulp distance is measured independently of `same_json`.
    #[test]
    fn same_json_clause_restores_within_2_ulps_and_not_at_3_census_3000_f64() {
        let num = |t: &str| -> f64 {
            let at = t.find("\"x\":").expect("an x member") + 4;
            t[at..t.len() - 1].parse().expect("a number")
        };
        let mut seed: u64 = 0x0d19_2026_1019_0002;
        let (mut within, mut beyond) = (0usize, 0usize);
        let mut wrong = Vec::new();
        for _ in 0..3000 {
            let mut x = f64::from_bits(xorshift(&mut seed));
            while !x.is_finite() || x == 0.0 {
                x = f64::from_bits(xorshift(&mut seed));
            }
            let rec = json!({"type": "step", "x": x});
            let text = serde_json::to_string(&rec).unwrap();
            let ev = Event::new("e1", Kind::Native).native(Native::new("x", None).record_raw(&text).unwrap());
            let mut attached = rec.clone();
            attach(&mut attached, &[], std::slice::from_ref(&ev));
            let base = reread(&attached);
            if base[KEY].get("records").is_none() {
                continue;
            }
            let path = drift_path(&serde_json::from_str(&text).unwrap());
            for d in [1u64, 2, 3, 4, 6] {
                let xe = f64::from_bits(x.to_bits() + d);
                if !xe.is_finite() {
                    continue;
                }
                let edited = reread(&with_carried_record(&base, json!({"type": "step", "x": xe})));
                let carried_text = serde_json::to_string(&edited[KEY]["events"][0]["native"]["record"]).unwrap();
                let seen: Value = serde_json::from_str(&carried_text).unwrap();
                if path.contains(&seen) {
                    continue; // the drift-path clause could restore it
                }
                let apart = ulps(num(&carried_text), num(&text));
                let mut r = edited.clone();
                let back = take(&mut r, &[]).expect("the outer record is unedited");
                let restored = back[0].native.as_ref().unwrap().record_text() == Some(text.as_str());
                if apart <= 2 {
                    within += 1;
                    if !restored {
                        wrong.push(format!("{x:e} +{d}: {apart} ulps apart, not restored"));
                    }
                } else {
                    beyond += 1;
                    if restored {
                        wrong.push(format!("{x:e} +{d}: {apart} ulps apart, restored"));
                    }
                }
            }
        }
        eprintln!("same_json clause census: off-path within 2 ulps={within} at 3+ ulps={beyond} wrong={}", wrong.len());
        // Sentinel: both classes are populated, so either half could fail.
        assert!(within > 100 && beyond > 100, "census too thin: within={within} beyond={beyond}");
        assert!(wrong.is_empty(), "{} wrong restores, e.g. {:?}", wrong.len(), &wrong[..wrong.len().min(5)]);
    }

    #[test]
    fn reread_cap_is_far_above_any_measured_tail() {
        // The longest re-read walk over the long drifter and 20,000 random f64.
        let mut seed: u64 = 0x5eed_0005_2026_1018;
        let mut worst = passes_to_repeat(&json!({"type": "step", "x": -8.926968263417197e+161_f64}));
        for _ in 0..20_000 {
            let mut x = f64::from_bits(xorshift(&mut seed));
            while !x.is_finite() {
                x = f64::from_bits(xorshift(&mut seed));
            }
            worst = worst.max(passes_to_repeat(&json!({"type": "step", "x": x})));
        }
        eprintln!("reread cap: REREAD_CAP={REREAD_CAP} worst_measured={worst}");
        assert!(REREAD_CAP >= 100 * worst, "REREAD_CAP {REREAD_CAP} is not 100 times the worst measured walk ({worst})");
        // The tripwire fires, loudly and naming the record, when the walk
        // outruns the cap; and stays quiet when it does not.
        {
            let rec = json!({"type": "step", "x": -8.926968263417197e+161_f64});
            let text = serde_json::to_string(&rec).unwrap();
            let off_path = json!({"type": "step", "x": 0.5});
            let fired = std::panic::catch_unwind(|| as_reread_within(rec.clone(), 64));
            let walked = std::panic::catch_unwind(|| on_reread_path(&text, &off_path, 64));
            for (what, r) in [("as_reread_within", fired.err()), ("on_reread_path", walked.err())] {
                let msg = r.unwrap_or_else(|| panic!("{what}: no panic past a 64-pass cap"));
                let msg = msg.downcast_ref::<String>().cloned().unwrap_or_default();
                assert!(
                    msg.contains("after 64 serialise/parse passes") && msg.contains("-8.92696826341"),
                    "{what}: the panic does not name the cap and the record: {msg}"
                );
            }
            assert_eq!(as_reread_within(rec.clone(), 1000), as_reread(rec.clone()));
            assert!(!on_reread_path(&text, &off_path, 1000));
        }
    }
}
