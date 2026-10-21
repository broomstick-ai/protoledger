//! opencode identifiers (`packages/schema/src/identifier.ts`).
//!
//! An id is a prefix (`ses_`, `msg_`, `prt_`) and 26 characters: 12 lower-case
//! hex digits encoding the low 48 bits of `timestamp_ms * 0x1000 + counter`
//! (bitwise-negated for the descending session ids, so newer sessions sort
//! first), then 14 characters from `[0-9A-Za-z]`. opencode fills the tail with
//! random bytes; protoledger derives it from a hash of the event or agent id, so
//! re-exports mint the same ids (SPEC §6).

use sha2::{Digest, Sha256};

const CHARS: &[u8] = b"0123456789ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz";

/// The 12-hex time component for `value` (= ms * 0x1000 + counter). Only the
/// low 48 bits are written, as opencode writes them, so ids order by time only
/// within a window of 2^36 ms (about 2.2 years); a session crossing a multiple
/// of that would see its ids wrap. That is opencode's own encoding, kept.
fn time_hex(value: u64, descending: bool) -> String {
    let v = if descending { !value } else { value };
    format!("{:012x}", v & 0xffff_ffff_ffff)
}

/// 14 base62 characters derived from `seed`.
fn tail(seed: &str) -> String {
    let h = Sha256::digest(seed.as_bytes());
    h.iter().take(14).map(|b| CHARS[(*b as usize) % 62] as char).collect()
}

/// `<prefix>_<12 hex><14 base62>` for an explicit time value.
pub fn make(prefix: &str, value: u64, descending: bool, seed: &str) -> String {
    format!("{prefix}_{}{}", time_hex(value, descending), tail(&format!("protoledger:{prefix}:{seed}")))
}

/// `ms * 0x1000 + counter`, saturating: a time no clock reaches (a damaged
/// source's) mints an id at the top of the range instead of overflowing.
fn time_value(ms: i64, counter: u64) -> u64 {
    checked_time_value(ms, counter).unwrap_or(u64::MAX)
}

/// `ms * 0x1000 + counter`, or `None` for a time no `u64` holds.
fn checked_time_value(ms: i64, counter: u64) -> Option<u64> {
    (ms.max(0) as u64).checked_mul(0x1000)?.checked_add(counter)
}

/// A session id for a session created at `ms`.
pub fn session(ms: i64, seed: &str) -> String {
    make("ses", time_value(ms, 1), true, seed)
}

/// Is `id` shaped like an id opencode mints (prefix, 12 hex, 14 base62)?
pub fn is_canonical(id: &str, prefix: &str) -> bool {
    let Some(rest) = id.strip_prefix(prefix).and_then(|r| r.strip_prefix('_')) else { return false };
    rest.len() == 26 && rest[..12].bytes().all(|b| b.is_ascii_digit() || (b'a'..=b'f').contains(&b)) && rest[12..].bytes().all(|b| b.is_ascii_alphanumeric())
}

/// The creation time (ms, modulo 2^36) encoded in an ascending id, when it is canonical.
#[cfg(test)]
pub fn ascending_ms(id: &str, prefix: &str) -> Option<i64> {
    if !is_canonical(id, prefix) {
        return None;
    }
    let rest = &id[prefix.len() + 1..];
    let v = u64::from_str_radix(&rest[..12], 16).ok()?;
    Some((v >> 12) as i64)
}

/// Mints strictly increasing ascending ids, so that opencode's id order (V1
/// sorts parts and messages by id) matches the ledger's order.
#[derive(Debug, Clone, Default)]
pub struct Ascending {
    last: u64,
    /// Times no `ms * 0x1000 + counter` holds: observed and ignored, or
    /// given to an id that then follows the one before it.
    pub damaged: u64,
}

impl Ascending {
    pub fn new() -> Self {
        Ascending { last: 0, damaged: 0 }
    }
    /// Make every later id sort after anything created at or before `ms`.
    /// A time no `u64` holds says nothing to sort after: it is ignored, and
    /// counted in [`Ascending::damaged`].
    pub fn observe_ms(&mut self, ms: i64) {
        match checked_time_value(ms, 0xfff) {
            Some(v) => self.last = self.last.max(v),
            None => self.damaged += 1,
        }
    }
    /// The next id, after every id minted so far. An event whose time no
    /// `u64` holds (a damaged source) gets the id right after the previous
    /// one, and is counted in [`Ascending::damaged`]: its time would sort it,
    /// and every healthy event after it, to the top of the range.
    pub fn next(&mut self, prefix: &str, ms: i64, seed: &str) -> String {
        let after = self.last.saturating_add(1);
        let v = match checked_time_value(ms, 1) {
            Some(want) => want.max(after),
            None => {
                self.damaged += 1;
                after
            }
        };
        self.last = v;
        make(prefix, v, false, seed)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn shapes_match_opencode() {
        // Real ids from an opencode capture: the session id is the bit-negation of
        // a time value close to the first message's.
        assert!(is_canonical("ses_f78f36bd6ffeTXo4UWv9dIETyI", "ses"));
        assert!(is_canonical("msg_0870c947c001u289U4KpLcgWyg", "msg"));
        let s = session(1788972143734, "x");
        assert!(is_canonical(&s, "ses"));
        assert!(s.starts_with("ses_f78f36b"), "{s}");
        let mut a = Ascending::new();
        // msg_0870c947c001… decodes to 1788972143740 ms, counter 1.
        let m1 = a.next("msg", 1788972143740, "a");
        let m2 = a.next("msg", 1788972143740, "b");
        let m0 = a.next("msg", 1, "c");
        assert!(m1 < m2 && m2 < m0, "{m1} {m2} {m0}");
        assert!(m1.starts_with("msg_0870c947c001"), "{m1}");
        assert_eq!(ascending_ms(&m1, "msg"), Some(1788972143740 & 0xf_ffff_ffff));
    }

    /// A damaged time (one no clock reaches) mints no id from it,
    /// and does not poison the order: a healthy event after it still sorts
    /// after its predecessor, and after the damaged one.
    #[test]
    fn a_damaged_time_does_not_poison_the_order_of_later_ids() {
        let mut a = Ascending::new();
        let before = a.next("msg", 1788972143740, "a");
        let damaged = a.next("msg", i64::MAX, "b");
        let later: Vec<String> = (0..4).map(|i| a.next("msg", 1788972143741 + i, &format!("c{i}"))).collect();
        let mut all = vec![before.clone(), damaged.clone()];
        all.extend(later.iter().cloned());
        assert!(all.windows(2).all(|w| w[0] < w[1]), "not strictly increasing: {all:?}");
        assert_eq!(a.damaged, 1);
        // Opposed: healthy times only, nothing counted, and still increasing.
        let mut b = Ascending::new();
        let ok: Vec<String> = (0..4).map(|i| b.next("msg", 1788972143741 + i, &format!("c{i}"))).collect();
        assert!(ok.windows(2).all(|w| w[0] < w[1]), "{ok:?}");
        assert_eq!(b.damaged, 0);
    }

    #[test]
    fn deterministic() {
        assert_eq!(session(5, "agent"), session(5, "agent"));
        assert_ne!(session(5, "agent"), session(5, "other"));
    }
}
