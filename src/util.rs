//! Small helpers shared by adapters.

use serde_json::{Map, Value};
use uuid::Uuid;

/// Map edits. A serde_json map is sorted by key (no `preserve_order`),
/// so position means nothing and these are plain `remove` / `insert`.
pub trait OrderedMap {
    fn remove_ordered(&mut self, key: &str) -> Option<Value>;
    fn insert_at(&mut self, index: usize, key: String, value: Value) -> Option<Value>;
}

impl OrderedMap for Map<String, Value> {
    fn remove_ordered(&mut self, key: &str) -> Option<Value> {
        self.remove(key)
    }
    fn insert_at(&mut self, _index: usize, key: String, value: Value) -> Option<Value> {
        self.insert(key, value)
    }
}

/// The protoledger UUID namespace (SPEC §6).
pub const NAMESPACE: Uuid = Uuid::from_u128(0x6f1f0d3c_7b0e_5a3e_9a43_70726f746f6c);

/// A deterministic UUID for `name`, so re-exports mint the same native ids.
pub fn uuid_for(name: &str) -> String {
    Uuid::new_v5(&NAMESPACE, name.as_bytes()).to_string()
}

/// A short deterministic hex id for `name` (for formats with non-UUID ids).
pub fn short_id(name: &str, len: usize) -> String {
    use sha2::{Digest, Sha256};
    let h = hex::encode(Sha256::digest(name.as_bytes()));
    h[..len.min(h.len())].to_string()
}

/// Normalise an RFC 3339 timestamp to millisecond precision in UTC (`Z`), so
/// sources with different precision compare equal. Unparseable input is
/// returned unchanged.
pub fn norm_ts(ts: &str) -> String {
    match parse_rfc3339_ms(ts) {
        Some(ms) => ms_to_rfc3339(ms),
        None => ts.to_string(),
    }
}

/// Milliseconds since the Unix epoch for an RFC 3339 timestamp.
pub fn parse_rfc3339_ms(ts: &str) -> Option<i64> {
    let b = ts.as_bytes();
    if b.len() < 19 || b[4] != b'-' || b[7] != b'-' || !(b[10] == b'T' || b[10] == b' ') {
        return None;
    }
    // `get`, not indexing: a damaged time may put a multi-byte character
    // across any of these offsets, and that is `None`, not a panic.
    let num = |a: usize, b: usize| ts.get(a..b).and_then(|s| s.parse::<i64>().ok());
    let (y, mo, d) = (num(0, 4)?, num(5, 7)?, num(8, 10)?);
    let (h, mi, s) = (num(11, 13)?, num(14, 16)?, num(17, 19)?);
    let mut rest = ts.get(19..)?;
    let mut ms = 0i64;
    if let Some(frac) = rest.strip_prefix('.') {
        let digits: String = frac.chars().take_while(|c| c.is_ascii_digit()).collect();
        let mut f = digits.clone();
        f.truncate(3);
        while f.len() < 3 {
            f.push('0');
        }
        ms = f.parse().ok()?;
        rest = &frac[digits.len()..];
    }
    let offset_min = match rest {
        "" | "Z" | "z" => 0,
        o if (o.starts_with('+') || o.starts_with('-')) && o.len() >= 6 => {
            let sign = if o.starts_with('-') { -1 } else { 1 };
            let part = |a: usize, b: usize| o.get(a..b).and_then(|s| s.parse::<i64>().ok());
            sign * (part(1, 3)? * 60 + part(4, 6)?)
        }
        _ => return None,
    };
    let days = days_from_civil(y, mo, d);
    Some(((days * 86400 + h * 3600 + mi * 60 + s) - offset_min * 60) * 1000 + ms)
}

/// RFC 3339 with milliseconds and `Z` for an epoch-milliseconds value.
pub fn ms_to_rfc3339(ms: i64) -> String {
    let secs = ms.div_euclid(1000);
    let milli = ms.rem_euclid(1000);
    let days = secs.div_euclid(86400);
    let sod = secs.rem_euclid(86400);
    let (y, m, d) = civil_from_days(days);
    format!("{y:04}-{m:02}-{d:02}T{:02}:{:02}:{:02}.{milli:03}Z", sod / 3600, (sod % 3600) / 60, sod % 60)
}

// Howard Hinnant's civil-date algorithms.
fn days_from_civil(y: i64, m: i64, d: i64) -> i64 {
    let y = if m <= 2 { y - 1 } else { y };
    let era = y.div_euclid(400);
    let yoe = y - era * 400;
    let mp = (m + 9) % 12;
    let doy = (153 * mp + 2) / 5 + d - 1;
    let doe = yoe * 365 + yoe / 4 - yoe / 100 + doy;
    era * 146097 + doe - 719468
}

fn civil_from_days(z: i64) -> (i64, i64, i64) {
    let z = z + 719468;
    let era = z.div_euclid(146097);
    let doe = z - era * 146097;
    let yoe = (doe - doe / 1460 + doe / 36524 - doe / 146096) / 365;
    let y = yoe + era * 400;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let d = doy - (153 * mp + 2) / 5 + 1;
    let m = if mp < 10 { mp + 3 } else { mp - 9 };
    (if m <= 2 { y + 1 } else { y }, m, d)
}

/// `value[path...]` as a str.
pub fn get_str<'a>(v: &'a Value, path: &[&str]) -> Option<&'a str> {
    get(v, path)?.as_str()
}

pub fn get<'a>(v: &'a Value, path: &[&str]) -> Option<&'a Value> {
    let mut cur = v;
    for k in path {
        cur = cur.get(*k)?;
    }
    Some(cur)
}

/// Split JSONL text into (byte offset, line) pairs, skipping blank lines.
pub fn jsonl_lines(text: &str) -> impl Iterator<Item = (usize, &str)> {
    let mut off = 0usize;
    text.split_inclusive('\n').filter_map(move |l| {
        let start = off;
        off += l.len();
        let t = l.trim_end_matches(['\n', '\r']);
        if t.trim().is_empty() { None } else { Some((start, t)) }
    })
}

// JSON text re-laid without being re-spelled.
//
// A native record is held as its source text (SPEC §4). Where a format writes
// records inside a larger pretty-printed document (`opencode export`, the
// opencode 1.x storage files, a Codex `.json` session, ATIF and Slack
// documents), the document is rebuilt from those texts: whitespace between
// tokens is removed ([`minify`]) or laid out again ([`pretty`]), and every
// token (string, number, literal) is copied byte for byte, so key order,
// number spelling and escapes come back as they were, whichever serde_json
// features are on.

/// `text` with every whitespace byte between tokens removed. `text` must be JSON.
pub(crate) fn minify(text: &str) -> String {
    let b = text.as_bytes();
    let mut out = Vec::with_capacity(b.len());
    let mut i = 0;
    while i < b.len() {
        match b[i] {
            b'"' => {
                let end = string_end(b, i);
                out.extend_from_slice(&b[i..end]);
                i = end;
                continue;
            }
            b' ' | b'\t' | b'\n' | b'\r' => {}
            c => out.push(c),
        }
        i += 1;
    }
    String::from_utf8(out).expect("removing ASCII whitespace keeps UTF-8")
}

/// Compact JSON text laid out as `JSON.stringify(x, null, 2)` lays it out (and
/// serde_json's pretty printer: two spaces, `": "`, `[]` and `{}` for empty
/// containers). Tokens are copied unchanged.
pub(crate) fn pretty(compact: &str) -> String {
    let b = compact.as_bytes();
    let mut out = Vec::with_capacity(b.len() * 2);
    let mut depth = 0usize;
    let newline = |out: &mut Vec<u8>, depth: usize| {
        out.push(b'\n');
        out.resize(out.len() + 2 * depth, b' ');
    };
    let mut i = 0;
    while i < b.len() {
        match b[i] {
            b'"' => {
                let end = string_end(b, i);
                out.extend_from_slice(&b[i..end]);
                i = end;
                continue;
            }
            c @ (b'{' | b'[') => {
                let close = if c == b'{' { b'}' } else { b']' };
                if b.get(i + 1) == Some(&close) {
                    out.extend_from_slice(&[c, close]);
                    i += 2;
                    continue;
                }
                out.push(c);
                depth += 1;
                newline(&mut out, depth);
            }
            c @ (b'}' | b']') => {
                depth = depth.saturating_sub(1);
                newline(&mut out, depth);
                out.push(c);
            }
            b',' => {
                out.push(b',');
                newline(&mut out, depth);
            }
            b':' => out.extend_from_slice(b": "),
            c => out.push(c),
        }
        i += 1;
    }
    String::from_utf8(out).expect("adding ASCII whitespace keeps UTF-8")
}

/// Whether a parsed `Value` re-serialises compact `text` to itself in this
/// build. A record for which it does not also needs its text kept as `raw`: a
/// carrier holds events as JSON values, so only `raw` survives a trip through
/// another harness with the source spelling intact.
pub(crate) fn value_round_trips(compact: &str) -> bool {
    serde_json::from_str::<serde_json::Value>(compact).ok().and_then(|v| serde_json::to_string(&v).ok()).as_deref() == Some(compact)
}

/// The index just past the string token that starts at `b[start]` (a `"`).
pub(crate) fn string_end(b: &[u8], start: usize) -> usize {
    let mut i = start + 1;
    while i < b.len() {
        match b[i] {
            b'\\' => i += 2,
            b'"' => return i + 1,
            _ => i += 1,
        }
    }
    b.len()
}

// A native record's `raw` (SPEC §4).

/// The `raw` text of native record `n` if an exporter may emit it: only while
/// it still matches `n`'s record ([`same_json`]), or when `n` has no record.
/// A writer that edits `record` and keeps a stale `raw` (SPEC §4: an editor
/// need not remove it) gets its edit exported, not the stale text.
///
/// A record a stale carrier was taken out of (an importer found a carrier
/// whose record had been edited) is held as what is left, with the line it
/// came from as `raw`: that `raw` matches the record once its carriers are
/// stripped as [`crate::carry::strip`] strips them, so the line is written
/// back as it was read.
pub(crate) fn current_raw(n: &crate::model::Native) -> Option<&str> {
    let raw = n.raw.as_deref()?;
    match n.record_text() {
        None => Some(raw),
        Some(text) => raw_matches(raw, text).then_some(raw),
    }
}

/// Whether `raw` still stands for the JSON text `text`: [`same_json`], or the
/// same once `raw`'s carriers are stripped ([`current_raw`]; only members
/// shaped as carriers, never a genuine member that shares the name). For a format
/// whose `raw` is one member of its record (an opencode database row keeps
/// its `data` column's text), `text` is that member's.
pub(crate) fn raw_matches(raw: &str, text: &str) -> bool {
    if same_json(raw, text) {
        return true;
    }
    let (Some(mut r), Some(t)) = (Json::parse(raw), Json::parse(text)) else { return false };
    r.strip_carriers() && r.same(&t)
}

/// The text an exporter emits for native record `n`, as held (SPEC §4): its
/// [`current_raw`], else its record's own text.
pub(crate) fn native_text(n: &crate::model::Native) -> Option<String> {
    current_raw(n).or_else(|| n.record_text()).map(str::to_string)
}

/// Whether JSON texts `a` and `b` say the same thing: the same structure,
/// whitespace aside, with object keys in any order,
/// strings equal once their escapes are decoded, integers equal as integers,
/// and other numbers within [`NUMBER_ULPS`] units in the last place of each
/// other as `f64`s. A text that is not JSON matches only itself.
///
/// It does not use serde_json's own parse, whose answer would depend on
/// serde_json's features, which a dependent's build can turn on even though
/// protoledger does not (with `arbitrary_precision`, `1e3` and `1000.0` differ; without
/// `float_roundtrip`, `0.011300000000000001` parses as `0.0113`): every token
/// is read here, the same way in every build, and numbers are compared as
/// [`str::parse`] (correctly rounded) reads them. The tolerance is the one
/// residual. A `record` held as a parsed `Value` (a record with a line break
/// in its text, or one a carrier brought back re-spelled) is spelled by the
/// build's parser, which without `float_roundtrip` may land a unit or two in
/// the last place away from the source's number; `raw` still stands for that
/// record. So an edit that moves a non-integer number by no more than
/// [`NUMBER_ULPS`] units in the last place is not seen, in either build.
pub(crate) fn same_json(a: &str, b: &str) -> bool {
    if a == b {
        return true;
    }
    match (Json::parse(a), Json::parse(b)) {
        (Some(x), Some(y)) => x.same(&y),
        _ => false,
    }
}

/// How far apart, in units in the last place, two non-integer numbers may be
/// and still be the same number to [`same_json`].
pub(crate) const NUMBER_ULPS: u64 = 2;

/// A JSON value as its text says it, read the same way in every build.
#[derive(Debug)]
enum Json<'a> {
    Null,
    Bool(bool),
    Num(&'a str),
    Str(String),
    Arr(Vec<Json<'a>>),
    Obj(std::collections::BTreeMap<String, Json<'a>>),
}

impl<'a> Json<'a> {
    fn parse(text: &'a str) -> Option<Json<'a>> {
        let mut i = 0;
        let v = Self::value(text, &mut i, 0)?;
        Self::ws(text.as_bytes(), &mut i);
        (i == text.len()).then_some(v)
    }

    fn ws(b: &[u8], i: &mut usize) {
        while *i < b.len() && matches!(b[*i], b' ' | b'\t' | b'\n' | b'\r') {
            *i += 1;
        }
    }

    fn value(t: &'a str, i: &mut usize, depth: usize) -> Option<Json<'a>> {
        let b = t.as_bytes();
        if depth > 512 {
            return None;
        }
        Self::ws(b, i);
        match *b.get(*i)? {
            b'"' => {
                let end = string_end(b, *i);
                let s: String = serde_json::from_str(t.get(*i..end)?).ok()?;
                *i = end;
                Some(Json::Str(s))
            }
            b'{' => {
                *i += 1;
                let mut m = std::collections::BTreeMap::new();
                Self::ws(b, i);
                if b.get(*i) == Some(&b'}') {
                    *i += 1;
                    return Some(Json::Obj(m));
                }
                loop {
                    Self::ws(b, i);
                    let Json::Str(k) = Self::value(t, i, depth + 1).filter(|k| matches!(k, Json::Str(_)))? else { return None };
                    Self::ws(b, i);
                    (b.get(*i) == Some(&b':')).then_some(())?;
                    *i += 1;
                    // A repeated key: the last one counts, as serde_json reads it.
                    m.insert(k, Self::value(t, i, depth + 1)?);
                    Self::ws(b, i);
                    match b.get(*i)? {
                        b',' => *i += 1,
                        b'}' => {
                            *i += 1;
                            return Some(Json::Obj(m));
                        }
                        _ => return None,
                    }
                }
            }
            b'[' => {
                *i += 1;
                let mut a = Vec::new();
                Self::ws(b, i);
                if b.get(*i) == Some(&b']') {
                    *i += 1;
                    return Some(Json::Arr(a));
                }
                loop {
                    a.push(Self::value(t, i, depth + 1)?);
                    Self::ws(b, i);
                    match b.get(*i)? {
                        b',' => *i += 1,
                        b']' => {
                            *i += 1;
                            return Some(Json::Arr(a));
                        }
                        _ => return None,
                    }
                }
            }
            _ => {
                for (word, v) in [("null", Json::Null), ("true", Json::Bool(true)), ("false", Json::Bool(false))] {
                    if t[*i..].starts_with(word) {
                        *i += word.len();
                        return Some(v);
                    }
                }
                let start = *i;
                while *i < b.len() && matches!(b[*i], b'-' | b'+' | b'.' | b'e' | b'E' | b'0'..=b'9') {
                    *i += 1;
                }
                let tok = &t[start..*i];
                // JSON's number grammar, which `str::parse::<f64>` is looser than.
                serde_json::from_str::<serde::de::IgnoredAny>(tok).ok()?;
                Some(Json::Num(tok))
            }
        }
    }

    /// Remove every carrier (from this object and every object member,
    /// recursively); whether there was one. A carrier is a `protoledger`
    /// member shaped as [`crate::carry::attach`] writes one: an object with a
    /// string `h` and an array `events`. A genuine member that merely has the
    /// name stays, so an edit that removes it is seen.
    fn strip_carriers(&mut self) -> bool {
        let Json::Obj(m) = self else { return false };
        let mut found = m.get(crate::carry::KEY).is_some_and(Json::is_carrier) && m.remove(crate::carry::KEY).is_some();
        for v in m.values_mut() {
            found |= v.strip_carriers();
        }
        found
    }

    fn is_carrier(&self) -> bool {
        let Json::Obj(m) = self else { return false };
        matches!(m.get("h"), Some(Json::Str(_))) && matches!(m.get("events"), Some(Json::Arr(_)))
    }

    fn same(&self, o: &Json) -> bool {
        match (self, o) {
            (Json::Null, Json::Null) => true,
            (Json::Bool(x), Json::Bool(y)) => x == y,
            (Json::Str(x), Json::Str(y)) => x == y,
            (Json::Num(x), Json::Num(y)) => same_number(x, y),
            (Json::Arr(x), Json::Arr(y)) => x.len() == y.len() && x.iter().zip(y).all(|(x, y)| x.same(y)),
            (Json::Obj(x), Json::Obj(y)) => x.len() == y.len() && x.iter().zip(y).all(|((kx, vx), (ky, vy))| kx == ky && vx.same(vy)),
            _ => false,
        }
    }
}

/// Two JSON number tokens as [`same_json`] compares them. When either is
/// spelled as an integer they are compared exactly, as decimals (`1e3` is
/// `1000`), so an integer edited and re-spelled as a float is an edit
/// (`9007199254740993` is not `9007199254740992.0`, `1` is not
/// `1.0000000000000002`). The one exception is an integer
/// no `i64`/`u64` holds, which serde_json reads as an `f64` and re-spells as
/// one: against a non-integer spelling it is compared as `f64`s, as two
/// non-integers are.
fn same_number(x: &str, y: &str) -> bool {
    if x == y {
        return true;
    }
    let int = |t: &str| !t.contains(['.', 'e', 'E']);
    if int(x) || int(y) {
        if exact_decimal(x).is_some_and(|d| Some(d) == exact_decimal(y)) {
            return true;
        }
        let wide = |t: &str| int(t) && t.parse::<i64>().is_err() && t.parse::<u64>().is_err();
        if (int(x) && int(y)) || !(wide(x) || wide(y)) {
            return false;
        }
    }
    match (x.parse::<f64>(), y.parse::<f64>()) {
        (Ok(a), Ok(b)) if a.is_finite() && b.is_finite() => a == b || ulps_apart(a, b) <= NUMBER_ULPS,
        _ => false,
    }
}

/// A JSON number token as the exact decimal it spells: sign, significant
/// digits (no leading or trailing zeros) and the power of ten they are
/// scaled by. Every zero is `(false, "", 0)`. `None` for an exponent past `i64`.
fn exact_decimal(t: &str) -> Option<(bool, String, i64)> {
    let (neg, t) = match t.strip_prefix('-') {
        Some(r) => (true, r),
        None => (false, t),
    };
    let (mant, exp) = match t.find(['e', 'E']) {
        Some(i) => (&t[..i], t[i + 1..].parse::<i64>().ok()?),
        None => (t, 0),
    };
    let (int, frac) = mant.split_once('.').unwrap_or((mant, ""));
    let digits = format!("{int}{frac}");
    let mut exp = exp.checked_sub(i64::try_from(frac.len()).ok()?)?;
    let digits = digits.trim_start_matches('0');
    if digits.is_empty() {
        return Some((false, String::new(), 0));
    }
    let kept = digits.trim_end_matches('0');
    exp = exp.checked_add(i64::try_from(digits.len() - kept.len()).ok()?)?;
    Some((neg, kept.to_string(), exp))
}

/// How many representable `f64`s lie between `a` and `b` (finite, same sign).
fn ulps_apart(a: f64, b: f64) -> u64 {
    if a.is_sign_negative() != b.is_sign_negative() {
        return u64::MAX;
    }
    a.abs().to_bits().abs_diff(b.abs().to_bits())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn ts_round_trip() {
        assert_eq!(norm_ts("2026-03-30T05:38:34.432Z"), "2026-03-30T05:38:34.432Z");
        assert_eq!(norm_ts("2026-03-30T05:38:34.432123+00:00"), "2026-03-30T05:38:34.432Z");
        assert_eq!(norm_ts("2026-03-30T07:38:34+02:00"), "2026-03-30T05:38:34.000Z");
        assert_eq!(ms_to_rfc3339(1767225600000), "2026-01-01T00:00:00.000Z");
        assert_eq!(parse_rfc3339_ms("2026-01-01T00:00:00Z"), Some(1767225600000));
        // A multi-byte character across a field's offset is not a time
        // (it panicked); opposed by the same shape with ASCII.
        assert_eq!(parse_rfc3339_ms("2026-01-01T0\u{e9}:00:00Z"), None);
        assert_eq!(parse_rfc3339_ms("2026-01-01T00:00:00+0\u{e9}:00"), None);
        assert_eq!(parse_rfc3339_ms("2026-01-01T00:00:00\u{e9}\u{e9}"), None);
        assert_eq!(parse_rfc3339_ms("2026-01-01T00:00:00+01:00"), Some(1767225600000 - 3_600_000));
    }

    #[test]
    fn pretty_matches_serde_pretty_and_keeps_tokens() {
        let src = "{\"b\":[1,2.50,{\"x\":\"a\\u00e9\\\"b, c: {d}\"}],\"a\":{},\"c\":[],\"d\":1e3,\"e\":null}";
        let want = "{\n  \"b\": [\n    1,\n    2.50,\n    {\n      \"x\": \"a\\u00e9\\\"b, c: {d}\"\n    }\n  ],\n  \"a\": {},\n  \"c\": [],\n  \"d\": 1e3,\n  \"e\": null\n}";
        assert_eq!(pretty(src), want);
        assert_eq!(minify(want), src);
        // On text serde would spell the same way, the layout is serde's own.
        let plain = r#"{"a":[1,{"b":"c"}],"d":{},"e":[]}"#;
        let v: serde_json::Value = serde_json::from_str(plain).unwrap();
        assert_eq!(pretty(plain), serde_json::to_string_pretty(&v).unwrap());
        assert_eq!(pretty("[]"), "[]");
        assert_eq!(pretty("\"x\""), "\"x\"");
    }

    #[test]
    fn value_round_trips_only_for_serde_spelling() {
        assert!(value_round_trips(r#"{"a":"x","b":[1,2]}"#));
        assert!(!value_round_trips(r#"{"a":"\u00e9"}"#));
        assert!(!value_round_trips(r#"{"a":1e3}"#));
        assert!(!value_round_trips(r#"{"a": 1}"#));
        assert!(value_round_trips(r#"{"a":1.5,"b":null}"#));
    }
}

/// `raw` is emitted only while it matches `record` (SPEC §4), in the
/// helper and in one exporter of each family: line formats, whole-file JSON,
/// and database stores.
#[cfg(test)]
mod raw_tests {
    use super::*;
    use crate::formats::{ExportOptions, ImportOptions, by_name};
    use crate::ledger::LedgerSet;
    use crate::model::Native;
    #[cfg(feature = "sqlite")] // used only by the SQLite tests
    use crate::roundtrip::materialise;
    use serde_json::json;

    /// Spellings a parsed `Value` loses in every build: inner whitespace, an
    /// escaped `é`, and `1e3` (serde writes `é` and no spaces, whatever
    /// serde_json features a build turns on).
    const PROBE: &str = r#""pl_n": 1e3, "pl_s": "é",  "#;

    /// `text` (a JSON object) with the probe members first.
    fn odd(text: &str) -> String {
        assert!(text.starts_with('{') && text.len() > 2, "{text}");
        format!("{{{PROBE}{}", &text[1..])
    }

    /// `odd` as a `Value` would hold it, with `pl_s` set to `s`.
    fn parsed(odd: &str, s: &str) -> Value {
        let mut v: Value = serde_json::from_str(odd).unwrap();
        v["pl_s"] = json!(s);
        v
    }

    #[test]
    fn current_raw_only_while_raw_matches_the_record() {
        let odd = odd(r#"{"a":[1,2],"b":{"c":null}}"#);
        let mut n = Native::new("x", None).record(parsed(&odd, "é"));
        n.raw = Some(odd.clone());
        // Sentinel: the record is re-spelled, so only `raw` gives the source text.
        assert_ne!(n.record_text(), Some(odd.as_str()));
        assert_eq!(native_text(&n).as_deref(), Some(odd.as_str()), "an unchanged record's raw is its text");
        // Opposed: an edited record keeps its stale raw and is exported with its edit.
        n.set_record(Some(parsed(&odd, "edited")));
        assert_eq!(current_raw(&n), None);
        assert!(native_text(&n).unwrap().contains(r#""pl_s":"edited""#));
        // A record with no text but `raw` is its raw.
        let bare = Native::new("x", None).raw("not json");
        assert_eq!(native_text(&bare).as_deref(), Some("not json"));
        // A line a stale carrier was taken out of is written back as read,
        // until the record left over is edited.
        let line = r#"{"a":1,"protoledger":{"h":"stale","events":[]}}"#;
        let mut stale = Native::new("x", None).record(json!({"a": 1}));
        stale.raw = Some(line.into());
        assert_eq!(current_raw(&stale), Some(line));
        stale.set_record(Some(json!({"a": 2})));
        assert_eq!(current_raw(&stale), None);
    }

    /// An edit that only removes a genuine member named
    /// `protoledger` (not shaped as a carrier), at any depth, is an edit: the
    /// stale raw is not emitted over it. Opposed: a carrier-shaped member, at
    /// the top or nested, is still stripped and the line written back as read.
    #[test]
    fn removing_a_genuine_protoledger_member_is_an_edit() {
        let held = |line: &str, record: Value| {
            let mut n = Native::new("x", None).record_raw(line).unwrap();
            n.raw = Some(line.to_string());
            assert_eq!(current_raw(&n), Some(line), "sentinel: an unedited record's raw is emitted");
            n.set_record(Some(record));
            current_raw(&n).map(str::to_string)
        };
        for (line, edited) in [
            (r#"{"a":1,"protoledger":"genuine"}"#, json!({"a": 1})),
            (r#"{"a":1,"protoledger":{"note":"mine"}}"#, json!({"a": 1})),
            (r#"{"a":{"b":1,"protoledger":7}}"#, json!({"a": {"b": 1}})),
            (r#"{"a":[{"protoledger":{"h":1,"events":[]}}]}"#, json!({"a": [{}]})),
        ] {
            assert_eq!(held(line, edited), None, "{line}: removing the genuine member is hidden");
        }
        for (line, rest) in [
            (r#"{"a":1,"protoledger":{"v":"0.1","h":"x","events":[]}}"#, json!({"a": 1})),
            (r#"{"a":{"b":1,"protoledger":{"h":"x","events":[{"id":"e"}]}}}"#, json!({"a": {"b": 1}})),
        ] {
            assert_eq!(held(line, rest).as_deref(), Some(line), "{line}: a carrier is no longer stripped");
        }
    }

    /// The predicate is the same in every build: it
    /// reads every token itself and compares numbers as `str::parse` reads
    /// them. Its one residual, pinned here and run under both feature sets:
    /// non-integer numbers within [`NUMBER_ULPS`] units in the last place are
    /// the same number, so an edit that small is not seen.
    #[test]
    fn same_json_is_build_independent() {
        // Whitespace, key order and escapes never decide it.
        assert!(same_json(r#"{"a": 1, "b": "é"}"#, r#"{"b":"é","a":1}"#));
        // `1e3` is 1000 (their `Value`s would differ only under serde_json's
        // `arbitrary_precision`, which protoledger does not turn on).
        assert!(same_json("[1e3]", "[1000]") && same_json("[1e3]", "[1000.0]"));
        // A number serde's default parser (no `float_roundtrip`) re-spells is
        // the same number as its re-spelling: the record a `Value` holds
        // still matches the source's raw.
        assert!(same_json("[0.011300000000000001]", "[0.0113]"));
        assert!(same_json("[1.0000000000000002]", "[1.0]"), "1 ulp: within the residual");
        // Opposed: anything larger is an edit.
        assert!(!same_json("[1.0000000000000007]", "[1.0]"), "3 ulps is an edit");
        assert!(!same_json("[0.0114]", "[0.0113]"));
        assert!(!same_json(r#"{"a":1}"#, r#"{"a":1,"b":2}"#));
        assert!(!same_json(r#"{"a":"x"}"#, r#"{"a":"y"}"#));
        assert!(!same_json("[1,2]", "[2,1]"));
        // Integers compare exactly, beyond f64's 2^53.
        assert!(!same_json("[12345678901234567890]", "[12345678901234567891]"));
        assert!(same_json("[-0]", "[0]"));
        // An integer edited and re-spelled as a float is
        // an edit, in both orders; an integer token is compared exactly.
        for (int, float) in [("9007199254740993", "9007199254740992.0"), ("1", "1.0000000000000002")] {
            assert!(!same_json(&format!("[{int}]"), &format!("[{float}]")), "{int} vs {float}");
            assert!(!same_json(&format!("[{float}]"), &format!("[{int}]")), "{float} vs {int}");
        }
        // Opposed: an integer and an exact float spelling of the same value are
        // the same number; so is an integer no u64 holds against the f64
        // spelling serde_json gives it.
        assert!(same_json("[9007199254740992]", "[9007199254740992.0]") && same_json("[1]", "[1.0]") && same_json("[100]", "[1e2]"));
        let wide = "123456789012345678901234567890";
        let respelled = serde_json::to_string(&serde_json::from_str::<Value>(wide).unwrap()).unwrap();
        assert!(respelled.contains(['e', '.']), "sentinel: serde_json re-spells {wide} as {respelled}");
        assert!(same_json(&format!("[{wide}]"), &format!("[{respelled}]")));
        // Text that is not JSON matches only itself.
        assert!(same_json("not json", "not json") && !same_json("not json", "\"not json\""));
        assert!(!same_json("[1 2]", "[12]"));
    }

    fn fixture(case: &str) -> std::path::PathBuf {
        let native = std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("fixtures").join(case).join("native");
        let entries: Vec<_> =
            std::fs::read_dir(&native).unwrap().filter_map(|e| e.ok()).filter(|e| !e.file_name().to_string_lossy().starts_with('.')).collect();
        if entries.len() == 1 && entries[0].path().is_file() { entries[0].path() } else { native }
    }

    fn import(format: &str, path: &std::path::Path) -> LedgerSet {
        by_name(format).unwrap().import(path, &ImportOptions::standard()).unwrap().set
    }

    fn export(format: &str, set: &LedgerSet) -> crate::formats::Output {
        by_name(format).unwrap().export(set, &ExportOptions::standard()).unwrap().output
    }

    /// The first part-0 native record of `format` in `set` that `pick`
    /// accepts, changed by `f`; its native id.
    fn change(set: &mut LedgerSet, format: &str, pick: impl Fn(&Native) -> bool, f: impl FnOnce(&mut Native)) -> String {
        for l in set.ledgers.values_mut() {
            if let Some(n) = l.events.iter_mut().filter_map(|e| e.native.as_mut()).find(|n| n.format == format && n.part == 0 && pick(n)) {
                f(n);
                return n.id.clone().unwrap();
            }
        }
        panic!("no {format} record to change");
    }

    fn lines(out: &crate::formats::Output) -> Vec<String> {
        out.files.values().flat_map(|b| String::from_utf8_lossy(b).lines().map(str::to_string).collect::<Vec<_>>()).collect()
    }

    /// A line format (Claude Code): an unchanged record whose `raw` spells it
    /// as no `Value` does is written as that `raw`; an edited one with its edit.
    #[test]
    fn line_exporter_emits_raw_only_while_it_matches() {
        let src = import("claude-code", &fixture("claude-code/plain-chat"));
        let is_line = |n: &Native| n.file.is_none() && n.record_text().is_some_and(|t| t.starts_with("{\"") && t.contains("\"type\""));
        let mut set = src.clone();
        let mut odd_text = String::new();
        change(&mut set, "claude-code", is_line, |n| {
            odd_text = odd(n.record_text().unwrap());
            n.set_record(Some(parsed(&odd_text, "é")));
            n.raw = Some(odd_text.clone());
        });
        assert!(lines(&export("claude-code", &set)).contains(&odd_text), "the unchanged record is not written as its raw");
        let mut set = src.clone();
        change(&mut set, "claude-code", is_line, |n| {
            let o = odd(n.record_text().unwrap());
            n.set_record(Some(parsed(&o, "edited")));
            n.raw = Some(o);
        });
        let out = lines(&export("claude-code", &set));
        assert!(out.iter().any(|l| l.contains(r#""pl_s":"edited""#)), "the edit was lost to a stale raw");
        assert!(!out.iter().any(|l| l.contains("pl_s\": \"\\u00e9")), "the stale raw was written");
    }

    /// Whole-file JSON (OpenHands `events.json` that no layout reproduces, kept
    /// whole as `raw`): written back whole while unchanged; an edited record
    /// is written anew.
    #[test]
    fn whole_file_exporter_emits_raw_only_while_it_matches() {
        let path = fixture("openhands/api-cleanup");
        let source = std::fs::read(&path).unwrap();
        let src = import("openhands", &path);
        // Sentinel: this file is kept whole (no layout reproduces it).
        let extra = &src.iter().next().unwrap().agent().source.as_ref().unwrap().extra;
        assert!(extra.get("raw").is_some() && extra.get("layout").is_none(), "api-cleanup is not a whole-file raw");
        let out = export("openhands", &src);
        assert_eq!(out.files.values().next().unwrap(), &source, "an unchanged file is not written back byte for byte");
        let mut set = src.clone();
        change(
            &mut set,
            "openhands",
            |n| n.record_json().is_some_and(|r| r.is_object()),
            |n| {
                let mut r = n.record_json().unwrap();
                r["pl_s"] = json!("edited");
                n.set_record(Some(r));
            },
        );
        let out = export("openhands", &set);
        assert!(String::from_utf8_lossy(out.files.values().next().unwrap()).contains("\"pl_s\": \"edited\""), "the edit was lost to the file's raw");
    }

    /// A database store (opencode 2.x `opencode.db`, whose rows keep their
    /// `data` column's text as `raw`).
    #[cfg(feature = "sqlite")] // reads or writes a SQLite store
    #[test]
    fn store_exporter_emits_raw_only_while_it_matches() {
        let src = import("opencode", &fixture("opencode/v2-sqlite-2.0.31-generated"));
        let is_msg = |n: &Native| n.record_json().is_some_and(|r| r.get("data").is_some_and(Value::is_object) && r.get("session_id").is_some());
        let data_text = |n: &Native| crate::formats::atif::jsonfmt::member(n.record_text().unwrap(), "data").unwrap().to_string();
        let trip = |set: &LedgerSet, id: &str| {
            let t = tempfile::tempdir().unwrap();
            let path = materialise(&export("opencode", set), t.path()).unwrap();
            let back = import("opencode", &path);
            back.iter()
                .flat_map(|l| l.events.iter())
                .filter_map(|e| e.native.clone())
                .find(|n| n.part == 0 && n.id.as_deref() == Some(id))
                .expect("the record came back")
        };
        let mut set = src.clone();
        let mut odd_data = String::new();
        let id = change(&mut set, "opencode", is_msg, |n| {
            odd_data = odd(&data_text(n));
            let mut r = n.record_json().unwrap();
            r["data"] = parsed(&odd_data, "é");
            n.set_record(Some(r));
            n.raw = Some(odd_data.clone());
        });
        assert_eq!(trip(&set, &id).raw.as_deref(), Some(odd_data.as_str()), "the unchanged row's data is not its raw");
        let mut set = src.clone();
        let id = change(&mut set, "opencode", is_msg, |n| {
            let o = odd(&data_text(n));
            let mut r = n.record_json().unwrap();
            r["data"] = parsed(&o, "edited");
            n.set_record(Some(r));
            n.raw = Some(o);
        });
        let back = trip(&set, &id);
        assert_eq!(back.record_json().unwrap()["data"]["pl_s"], json!("edited"), "the edit was lost to a stale raw");
    }
}
