//! Python `json.dumps` output, for formats written by Python producers.
//!
//! Defaults are `separators=(", ", ": ")` and `ensure_ascii=True`; with an
//! indent, items end in `","` and each level is indented by `indent` spaces
//! (Python's own rules). Numbers are written as `serde_json` holds them, so a
//! value parsed from Python output re-serialises to the same digits.

use serde_json::Value;

#[derive(Clone, Copy)]
pub struct Opts {
    pub indent: Option<usize>,
    pub ensure_ascii: bool,
}

impl Opts {
    /// `json.dumps(v)`.
    pub const DEFAULT: Opts = Opts { indent: None, ensure_ascii: true };
}

pub fn dumps(v: &Value, o: Opts) -> String {
    let mut s = String::new();
    write(v, o, 0, &mut s);
    s
}

fn write(v: &Value, o: Opts, level: usize, out: &mut String) {
    match v {
        Value::Null => out.push_str("null"),
        Value::Bool(b) => out.push_str(if *b { "true" } else { "false" }),
        Value::Number(n) => out.push_str(&n.to_string()),
        Value::String(s) => string(s, o.ensure_ascii, out),
        Value::Array(a) => {
            if a.is_empty() {
                out.push_str("[]");
                return;
            }
            out.push('[');
            for (i, x) in a.iter().enumerate() {
                sep(i, o, level + 1, out);
                write(x, o, level + 1, out);
            }
            close(o, level, out);
            out.push(']');
        }
        Value::Object(m) => {
            if m.is_empty() {
                out.push_str("{}");
                return;
            }
            out.push('{');
            for (i, (k, x)) in m.iter().enumerate() {
                sep(i, o, level + 1, out);
                string(k, o.ensure_ascii, out);
                out.push_str(": ");
                write(x, o, level + 1, out);
            }
            close(o, level, out);
            out.push('}');
        }
    }
}

fn sep(i: usize, o: Opts, level: usize, out: &mut String) {
    match o.indent {
        Some(n) => {
            if i > 0 {
                out.push(',');
            }
            out.push('\n');
            out.push_str(&" ".repeat(n * level));
        }
        None => {
            if i > 0 {
                out.push_str(", ");
            }
        }
    }
}

fn close(o: Opts, level: usize, out: &mut String) {
    if let Some(n) = o.indent {
        out.push('\n');
        out.push_str(&" ".repeat(n * level));
    }
}

/// Python's string escaping: `\"`, `\\`, `\n`, `\r`, `\t`, `\b`, `\f`, other
/// controls as `\u00XX`; with `ensure_ascii`, everything above 0x7f as
/// lowercase `\uXXXX` (surrogate pairs beyond the BMP).
fn string(s: &str, ensure_ascii: bool, out: &mut String) {
    out.push('"');
    for c in s.chars() {
        match c {
            '"' => out.push_str("\\\""),
            '\\' => out.push_str("\\\\"),
            '\n' => out.push_str("\\n"),
            '\r' => out.push_str("\\r"),
            '\t' => out.push_str("\\t"),
            '\u{8}' => out.push_str("\\b"),
            '\u{c}' => out.push_str("\\f"),
            c if (c as u32) < 0x20 => out.push_str(&format!("\\u{:04x}", c as u32)),
            c if ensure_ascii && (c as u32) > 0x7e => {
                let mut buf = [0u16; 2];
                for u in c.encode_utf16(&mut buf) {
                    out.push_str(&format!("\\u{:04x}", u));
                }
            }
            c => out.push(c),
        }
    }
    out.push('"');
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn matches_python() {
        // python3 -c 'import json; print(json.dumps({"a":[1,"é"],"b":{}}))'
        assert_eq!(dumps(&json!({"a":[1,"é"],"b":{}}), Opts::DEFAULT), "{\"a\": [1, \"\\u00e9\"], \"b\": {}}");
        // json.dumps({"a":[1,{"b":None}],"c":[]}, indent=2, ensure_ascii=False)
        assert_eq!(
            dumps(&json!({"a":[1,{"b":null}],"c":[]}), Opts { indent: Some(2), ensure_ascii: false }),
            "{\n  \"a\": [\n    1,\n    {\n      \"b\": null\n    }\n  ],\n  \"c\": []\n}"
        );
        assert_eq!(dumps(&json!("\u{1F600}\u{7f}"), Opts::DEFAULT), "\"\\ud83d\\ude00\\u007f\"");
    }
}
