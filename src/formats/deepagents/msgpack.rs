//! A small MessagePack codec for LangGraph's `JsonPlusSerializer` blobs.
//!
//! Decoded values are JSON: extension types become
//! `{"__ext__": code, "data": <decoded payload>}` (or `"bin"` when the
//! payload is not itself MessagePack), binary becomes `{"__bin__": base64}`,
//! and a map with a non-string key becomes `{"__map__": [[k, v], ...]}`.
//! Encoding reverses that, choosing the smallest representation for each
//! value (as ormsgpack does), and always writing floats as float64.

use anyhow::{Result, bail};
use base64::Engine;
use serde_json::{Map, Value, json};

const B64: base64::engine::GeneralPurpose = base64::engine::general_purpose::STANDARD;

pub fn decode(bytes: &[u8]) -> Result<Value> {
    let mut r = Reader { b: bytes, i: 0 };
    let v = r.value()?;
    if r.i != bytes.len() {
        bail!("{} trailing bytes after msgpack value", bytes.len() - r.i);
    }
    Ok(v)
}

struct Reader<'a> {
    b: &'a [u8],
    i: usize,
}

impl Reader<'_> {
    fn take(&mut self, n: usize) -> Result<&[u8]> {
        if self.i + n > self.b.len() {
            bail!("msgpack truncated");
        }
        let s = &self.b[self.i..self.i + n];
        self.i += n;
        Ok(s)
    }
    fn u8(&mut self) -> Result<u8> {
        Ok(self.take(1)?[0])
    }
    fn be(&mut self, n: usize) -> Result<u64> {
        Ok(self.take(n)?.iter().fold(0u64, |a, b| (a << 8) | *b as u64))
    }
    fn str(&mut self, n: usize) -> Result<Value> {
        let s = self.take(n)?;
        Ok(Value::String(String::from_utf8_lossy(s).into_owned()))
    }
    fn arr(&mut self, n: usize) -> Result<Value> {
        let mut a = Vec::with_capacity(n.min(4096));
        for _ in 0..n {
            a.push(self.value()?);
        }
        Ok(Value::Array(a))
    }
    fn map(&mut self, n: usize) -> Result<Value> {
        let mut pairs = Vec::with_capacity(n.min(4096));
        for _ in 0..n {
            let k = self.value()?;
            let v = self.value()?;
            pairs.push((k, v));
        }
        if pairs.iter().all(|(k, _)| k.is_string()) {
            let mut m = Map::new();
            for (k, v) in pairs {
                m.insert(k.as_str().unwrap().to_string(), v);
            }
            Ok(Value::Object(m))
        } else {
            Ok(json!({"__map__": pairs.into_iter().map(|(k, v)| json!([k, v])).collect::<Vec<_>>()}))
        }
    }
    fn ext(&mut self, n: usize) -> Result<Value> {
        let code = self.u8()? as i8;
        let data = self.take(n)?.to_vec();
        Ok(match decode(&data) {
            Ok(v) => json!({"__ext__": code, "data": v}),
            Err(_) => json!({"__ext__": code, "bin": B64.encode(&data)}),
        })
    }
    fn value(&mut self) -> Result<Value> {
        let t = self.u8()?;
        Ok(match t {
            0x00..=0x7f => json!(t),
            0x80..=0x8f => return self.map((t & 0x0f) as usize),
            0x90..=0x9f => return self.arr((t & 0x0f) as usize),
            0xa0..=0xbf => return self.str((t & 0x1f) as usize),
            0xc0 => Value::Null,
            0xc2 => Value::Bool(false),
            0xc3 => Value::Bool(true),
            0xc4..=0xc6 => {
                let n = self.be(1 << (t - 0xc4))? as usize;
                json!({"__bin__": B64.encode(self.take(n)?)})
            }
            0xc7..=0xc9 => {
                let n = self.be(1 << (t - 0xc7))? as usize;
                return self.ext(n);
            }
            0xca => {
                let f = f32::from_bits(self.be(4)? as u32) as f64;
                serde_json::Number::from_f64(f).map(Value::Number).unwrap_or(Value::Null)
            }
            0xcb => {
                let f = f64::from_bits(self.be(8)?);
                serde_json::Number::from_f64(f).map(Value::Number).unwrap_or(Value::Null)
            }
            0xcc..=0xcf => json!(self.be(1 << (t - 0xcc))?),
            0xd0 => json!(self.be(1)? as u8 as i8),
            0xd1 => json!(self.be(2)? as u16 as i16),
            0xd2 => json!(self.be(4)? as u32 as i32),
            0xd3 => json!(self.be(8)? as i64),
            0xd4..=0xd8 => return self.ext(1 << (t - 0xd4)),
            0xd9..=0xdb => {
                let n = self.be(1 << (t - 0xd9))? as usize;
                return self.str(n);
            }
            0xdc | 0xdd => {
                let n = self.be(2 << (t - 0xdc))? as usize;
                return self.arr(n);
            }
            0xde | 0xdf => {
                let n = self.be(2 << (t - 0xde))? as usize;
                return self.map(n);
            }
            0xe0..=0xff => json!(t as i8),
            0xc1 => bail!("msgpack: reserved byte 0xc1"),
        })
    }
}

pub fn encode(v: &Value) -> Vec<u8> {
    let mut out = Vec::new();
    write(v, &mut out);
    out
}

fn len_header(out: &mut Vec<u8>, n: usize, fix: Option<(u8, usize)>, w8: Option<u8>, w16: u8, w32: u8) {
    match (fix, w8) {
        (Some((base, max)), _) if n <= max => out.push(base | n as u8),
        (_, Some(c)) if n <= 0xff => {
            out.push(c);
            out.push(n as u8);
        }
        _ if n <= 0xffff => {
            out.push(w16);
            out.extend_from_slice(&(n as u16).to_be_bytes());
        }
        _ => {
            out.push(w32);
            out.extend_from_slice(&(n as u32).to_be_bytes());
        }
    }
}

fn write(v: &Value, out: &mut Vec<u8>) {
    match v {
        Value::Null => out.push(0xc0),
        Value::Bool(b) => out.push(if *b { 0xc3 } else { 0xc2 }),
        Value::Number(n) => {
            let s = n.to_string();
            if let (false, Some(u)) = (s.contains(['.', 'e', 'E']), n.as_u64()) {
                match u {
                    0..=0x7f => out.push(u as u8),
                    0x80..=0xff => out.extend_from_slice(&[0xcc, u as u8]),
                    0x100..=0xffff => {
                        out.push(0xcd);
                        out.extend_from_slice(&(u as u16).to_be_bytes())
                    }
                    0x1_0000..=0xffff_ffff => {
                        out.push(0xce);
                        out.extend_from_slice(&(u as u32).to_be_bytes())
                    }
                    _ => {
                        out.push(0xcf);
                        out.extend_from_slice(&u.to_be_bytes())
                    }
                }
            } else if let (false, Some(i)) = (s.contains(['.', 'e', 'E']), n.as_i64()) {
                match i {
                    -32..=-1 => out.push(i as i8 as u8),
                    -128..=-33 => out.extend_from_slice(&[0xd0, i as i8 as u8]),
                    -32768..=-129 => {
                        out.push(0xd1);
                        out.extend_from_slice(&(i as i16).to_be_bytes())
                    }
                    -2147483648..=-32769 => {
                        out.push(0xd2);
                        out.extend_from_slice(&(i as i32).to_be_bytes())
                    }
                    _ => {
                        out.push(0xd3);
                        out.extend_from_slice(&i.to_be_bytes())
                    }
                }
            } else {
                out.push(0xcb);
                out.extend_from_slice(&n.as_f64().unwrap_or(0.0).to_bits().to_be_bytes());
            }
        }
        Value::String(s) => {
            len_header(out, s.len(), Some((0xa0, 31)), Some(0xd9), 0xda, 0xdb);
            out.extend_from_slice(s.as_bytes());
        }
        Value::Array(a) => {
            len_header(out, a.len(), Some((0x90, 15)), None, 0xdc, 0xdd);
            for x in a {
                write(x, out);
            }
        }
        Value::Object(m) => {
            if let Some(code) = m.get("__ext__").and_then(Value::as_i64) {
                let payload = match (m.get("data"), m.get("bin").and_then(Value::as_str)) {
                    (Some(d), _) => encode(d),
                    (None, Some(b)) => B64.decode(b).unwrap_or_default(),
                    _ => vec![],
                };
                let n = payload.len();
                match n {
                    1 => out.push(0xd4),
                    2 => out.push(0xd5),
                    4 => out.push(0xd6),
                    8 => out.push(0xd7),
                    16 => out.push(0xd8),
                    _ => len_header(out, n, None, Some(0xc7), 0xc8, 0xc9),
                }
                out.push(code as i8 as u8);
                out.extend_from_slice(&payload);
                return;
            }
            if let (1, Some(b)) = (m.len(), m.get("__bin__").and_then(Value::as_str)) {
                let bytes = B64.decode(b).unwrap_or_default();
                len_header(out, bytes.len(), None, Some(0xc4), 0xc5, 0xc6);
                out.extend_from_slice(&bytes);
                return;
            }
            if let (1, Some(pairs)) = (m.len(), m.get("__map__").and_then(Value::as_array)) {
                len_header(out, pairs.len(), Some((0x80, 15)), None, 0xde, 0xdf);
                for p in pairs {
                    write(p.get(0).unwrap_or(&Value::Null), out);
                    write(p.get(1).unwrap_or(&Value::Null), out);
                }
                return;
            }
            len_header(out, m.len(), Some((0x80, 15)), None, 0xde, 0xdf);
            for (k, x) in m {
                write(&Value::String(k.clone()), out);
                write(x, out);
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn round_trips_langgraph_shapes() {
        // A real write value from deepagents (fixarray(1) → ext8 type 5 → [mod, cls, {...}, method]).
        let v = json!([{"__ext__": 5, "data": ["langchain_core.messages.human", "HumanMessage",
            {"content": "Say hello.", "additional_kwargs": {}, "response_metadata": {}, "type": "human", "name": null, "id": "x"},
            "model_validate_json"]}]);
        let b = encode(&v);
        assert_eq!(&b[..4], &[0x91, 0xc7, (b.len() - 4) as u8, 0x05]);
        assert_eq!(decode(&b).unwrap(), v);
        for x in [
            json!(-1),
            json!(-200),
            json!(70000),
            json!(1.5),
            json!("é"),
            json!({"__bin__": "AAE="}),
            json!({"__map__": [[1, "a"]]}),
            json!({"__ext__": 7, "data": [1, 2]}),
        ] {
            assert_eq!(decode(&encode(&x)).unwrap(), x, "{x}");
        }
    }
}
