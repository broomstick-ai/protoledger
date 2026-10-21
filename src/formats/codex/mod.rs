//! OpenAI Codex CLI rollouts (`$CODEX_HOME/sessions/YYYY/MM/DD/rollout-<ts>-<thread>.jsonl`).
//!
//! Four native shapes are supported (see `docs/formats/codex.md`):
//!
//! - `ts-json`: the TypeScript CLI's single pretty-printed JSON document
//!   `{session, items}` (April-May 2025).
//! - `rollout-v0`: the early Rust CLI's JSONL, a bare session-meta first line
//!   followed by bare Responses API items and `{"record_type":"state"}` lines
//!   (rust-v0.0.2504xx .. 0.32.0).
//! - `rollout-legacy`: the `{timestamp, type, payload}` envelope (0.33.0 ..
//!   today, `history_mode` legacy or absent). The UI transcript lives in
//!   `event_msg` records that duplicate the `response_item`s.
//! - `rollout-paginated`: the same envelope with a per-line `ordinal` and
//!   `history_mode: "paginated"` (0.143.0 ..), whose UI records are
//!   `event_msg/item_completed`.
//!
//! Any of them may be stored zstd-compressed as `.jsonl.zst`.

mod export;
mod import;
#[cfg(test)]
mod tests;

use crate::formats::{Detection, ExportOptions, Exported, Follow, Format, ImportOptions, Imported, Variant};
use crate::ledger::LedgerSet;
use crate::model::{Agent, Event, SPEC_VERSION};
use anyhow::{Context, Result, bail};
use serde_json::{Value, json};
use std::path::{Path, PathBuf};

pub struct Codex;

pub(crate) const FORMAT: &str = "codex";
pub(crate) const V_TS: &str = "ts-json";
pub(crate) const V_V0: &str = "rollout-v0";
pub(crate) const V_LEGACY: &str = "rollout-legacy";
pub(crate) const V_PAGINATED: &str = "rollout-paginated";

/// The producer version written into exported `session_meta.cli_version`
/// unless `--target-version` says otherwise: the Codex release whose loader
/// the exporter was validated against (openai/codex 0e67502, 2026-10-16).
pub(crate) const DEFAULT_CLI_VERSION: &str = "0.163.0";
/// What the exporter writes for a time it does not know (Codex needs one on
/// every line); read back as unknown.
pub(crate) const UNKNOWN_TS: &str = "1970-01-01T00:00:00.000Z";
/// What the exporter writes for a working directory it does not know
/// (`session_meta.cwd` is required); read back as unknown.
pub(crate) const UNKNOWN_CWD: &str = "/";
/// Thread names, at the root of `CODEX_HOME`: one `{id, thread_name,
/// updated_at}` line per rename, the newest winning
/// (`rollout/src/session_index.rs:20-50`).
pub(crate) const SESSION_INDEX: &str = "session_index.jsonl";
/// `session_meta.originator` written on export (the classic Rust CLI's value).
pub(crate) const ORIGINATOR: &str = "codex_cli_rs";

static VARIANTS: [Variant; 4] = [
    Variant { name: V_TS, versions: "TS CLI 0.1.2504xx-0.1.2505xx", import: true, export: true, notes: "rollout-YYYY-MM-DD-<id>.json, {session, items}" },
    Variant {
        name: V_V0,
        versions: "rust 0.0.2504xx - 0.32.x",
        import: true,
        export: true,
        notes: "bare meta line + bare items; current Codex cannot load it",
    },
    Variant { name: V_LEGACY, versions: ">= 0.33.0", import: true, export: true, notes: "default export; event_msg UI records duplicate response_items" },
    Variant {
        name: V_PAGINATED,
        versions: ">= 0.143.0",
        import: true,
        export: true,
        notes: "per-line ordinal, item_completed UI records; --variant paginated",
    },
];

impl Format for Codex {
    fn name(&self) -> &'static str {
        FORMAT
    }
    fn describe(&self) -> &'static str {
        "OpenAI Codex CLI rollouts"
    }
    fn variants(&self) -> &'static [Variant] {
        &VARIANTS
    }
    fn weight(&self) -> u8 {
        3
    }

    fn detect(&self, input: &Path) -> Option<Detection> {
        let files = rollout_files(input).ok()?;
        let (rel, path) = files.first()?;
        let text = read_text(path).ok()?.text;
        let (variant, version) = sniff(rel, &text)?;
        let mut confidence = match variant {
            V_LEGACY | V_PAGINATED => 95,
            V_TS => 80,
            _ => 60,
        };
        if input.is_dir() && !rel.contains("rollout-") {
            confidence -= 20;
        }
        Some(Detection { variant, confidence, version })
    }

    fn import(&self, input: &Path, opts: &ImportOptions) -> Result<Imported> {
        import::import(input, opts)
    }

    fn follow(&self, input: &Path, opts: &ImportOptions) -> Option<Result<(Imported, Box<dyn Follow>)>> {
        import::follow(input, opts)
    }

    fn export(&self, set: &LedgerSet, opts: &ExportOptions) -> Result<Exported> {
        export::export(set, opts)
    }
}

/// A variant name from the command line, accepting the short forms.
pub(crate) fn canonical_variant(v: &str) -> Option<&'static str> {
    match v {
        "ts-json" | "ts" | "json" => Some(V_TS),
        "rollout-v0" | "v0" => Some(V_V0),
        "rollout-legacy" | "legacy" => Some(V_LEGACY),
        "rollout-paginated" | "paginated" => Some(V_PAGINATED),
        _ => None,
    }
}

// ---------------------------------------------------------------------------
// Files

/// The rollout files of an input, as (path relative to the input root, path).
/// A single file is keyed by its file name, matching `roundtrip::read_tree`.
pub(crate) fn rollout_files(input: &Path) -> Result<Vec<(String, PathBuf)>> {
    if input.is_file() {
        let name = input.file_name().context("input has no file name")?.to_string_lossy().to_string();
        return Ok(vec![(name, input.to_path_buf())]);
    }
    if !input.is_dir() {
        bail!("{}: no such file or directory", input.display());
    }
    // Codex's own names first; a directory without any (a hand-made set) falls
    // back to every JSONL/JSON file in it.
    for strict in [true, false] {
        let mut out = Vec::new();
        for e in walkdir::WalkDir::new(input).sort_by_file_name() {
            let e = e?;
            if !e.file_type().is_file() {
                continue;
            }
            let name = e.file_name().to_string_lossy();
            let ext_ok = name.ends_with(".jsonl") || name.ends_with(".jsonl.zst") || name.ends_with(".json");
            if !ext_ok || (strict && !name.starts_with("rollout-")) || name == SESSION_INDEX {
                continue;
            }
            let rel = e.path().strip_prefix(input)?.to_string_lossy().replace('\\', "/");
            if rel.split('/').any(|c| c.starts_with('.')) {
                continue;
            }
            out.push((rel, e.path().to_path_buf()));
        }
        if !out.is_empty() {
            return Ok(out);
        }
    }
    Ok(Vec::new())
}

pub(crate) struct FileText {
    pub text: String,
    /// `source.extra` facts needed to rebuild the stored bytes (compression).
    pub extra: serde_json::Map<String, Value>,
}

/// Read a rollout file, decompressing `.zst`. When the compressed bytes are not
/// what Codex's own encoder (level 3, pledged size) would produce from the
/// text, the original frame is kept so the self trip stays exact.
pub(crate) fn read_text(path: &Path) -> Result<FileText> {
    let bytes = std::fs::read(path).with_context(|| format!("reading {}", path.display()))?;
    let mut extra = serde_json::Map::new();
    let plain = if path.to_string_lossy().ends_with(".zst") {
        let plain = zstd_decode(&bytes).with_context(|| format!("decompressing {}", path.display()))?;
        extra.insert("compression".into(), json!("zstd"));
        if zstd_encode(&plain).ok().as_deref() != Some(&bytes[..]) {
            use base64::Engine;
            extra.insert("zstd_frame".into(), json!(base64::engine::general_purpose::STANDARD.encode(&bytes)));
        }
        plain
    } else {
        bytes
    };
    let text = String::from_utf8(plain).with_context(|| format!("{} is not UTF-8", path.display()))?;
    Ok(FileText { text, extra })
}

#[cfg(feature = "zstd")]
pub(crate) fn zstd_decode(bytes: &[u8]) -> Result<Vec<u8>> {
    Ok(zstd::stream::decode_all(bytes)?)
}

/// Codex's encoder: `rollout/src/compression.rs:1011-1018` (level 3, pledged
/// source size).
#[cfg(feature = "zstd")]
pub(crate) fn zstd_encode(plain: &[u8]) -> Result<Vec<u8>> {
    use std::io::Write;
    let mut enc = zstd::stream::write::Encoder::new(Vec::new(), 3)?;
    enc.set_pledged_src_size(Some(plain.len() as u64))?;
    enc.write_all(plain)?;
    Ok(enc.finish()?)
}

#[cfg(not(feature = "zstd"))]
pub(crate) fn zstd_decode(_: &[u8]) -> Result<Vec<u8>> {
    bail!("this build has no zstd support (enable the `zstd` feature)")
}

#[cfg(not(feature = "zstd"))]
pub(crate) fn zstd_encode(_: &[u8]) -> Result<Vec<u8>> {
    bail!("this build has no zstd support (enable the `zstd` feature)")
}

/// Which variant a file is, and the producer version where it says.
pub(crate) fn sniff(rel: &str, text: &str) -> Option<(&'static str, Option<String>)> {
    let trimmed = text.trim_start();
    if rel.ends_with(".json") || (trimmed.starts_with('{') && trimmed.contains("\n  \"session\"")) {
        let v: Value = serde_json::from_str(text).ok()?;
        if v.get("session").is_some_and(Value::is_object) && v.get("items").is_some_and(Value::is_array) {
            return Some((V_TS, None));
        }
        if rel.ends_with(".json") {
            return None;
        }
    }
    let mut lines = text.lines().filter(|l| !l.trim().is_empty());
    let first: Value = loop {
        let l = lines.next()?;
        if let Ok(v) = serde_json::from_str::<Value>(l)
            && v.is_object()
        {
            break v;
        }
    };
    if first.get("payload").is_some() && first.get("type").and_then(Value::as_str).is_some() {
        let version = (first["type"] == "session_meta").then(|| first["payload"].get("cli_version").and_then(Value::as_str).map(str::to_string)).flatten();
        let paginated = first["payload"].get("history_mode").and_then(Value::as_str) == Some("paginated")
            || (first.get("ordinal").is_some() && first["type"] == "session_meta");
        return Some((if paginated { V_PAGINATED } else { V_LEGACY }, version));
    }
    let is_meta = first.get("type").is_none() && first.get("id").is_some() && first.get("timestamp").is_some();
    let is_item = first.get("type").and_then(Value::as_str).is_some_and(is_response_item_type) || first.get("record_type").is_some();
    if is_meta || is_item {
        return Some((V_V0, None));
    }
    None
}

pub(crate) fn is_response_item_type(t: &str) -> bool {
    matches!(
        t,
        "message"
            | "reasoning"
            | "function_call"
            | "function_call_output"
            | "custom_tool_call"
            | "custom_tool_call_output"
            | "local_shell_call"
            | "web_search_call"
            | "tool_search_call"
            | "tool_search_output"
            | "image_generation_call"
            | "compaction"
            | "compaction_summary"
            | "context_compaction"
            | "agent_message"
            | "additional_tools"
            | "configuration_update"
            | "ghost_snapshot"
    )
}

// ---------------------------------------------------------------------------
// Carriers (SPEC §6)
//
// Codex decodes a rollout line by removing `timestamp`/`ordinal` and
// deserialising the rest as the internally tagged `RolloutItemWire`
// (`rollout/src/lib.rs:52-75`, `history/src/rollout_payload.rs:33-72`), which
// has no `deny_unknown_fields`, so an extra top-level key is ignored on load
// and dropped when Codex itself rewrites history. The carrier therefore sits at
// the top level of the line (of the bare item for `rollout-v0`, of the item
// object for `ts-json`). It is the shape of `carry::attach` plus an optional
// `agent` (the ledger header) on the first rendered line, and an empty `events`
// list marks a line that only accompanies another (an `event_msg` UI copy).

pub(crate) fn attach_carrier(record: &mut Value, events: &[Event], agent: Option<&Agent>) {
    if !events.is_empty() || agent.is_some() {
        crate::carry::attach_with(record, &[], events, agent);
        return;
    }
    // An empty carrier: the line stands for no event of its own.
    let h = crate::carry::record_hash(record);
    if let Value::Object(m) = record {
        m.insert(crate::carry::KEY.to_string(), json!({"v": SPEC_VERSION, "h": h, "events": []}));
    }
}

pub(crate) use crate::carry::Carried;

/// Remove the carrier from `record`. `Some` only when the record is unedited.
pub(crate) fn take_carrier(record: &mut Value) -> Option<Carried> {
    crate::carry::take_with(record, &[])
}

/// SPEC §3.1.6: an incoming peer message as the user text the model is shown.
pub(crate) fn peer_wrap(peer: Option<&str>, network: Option<&str>, text: &str) -> String {
    let mut open = String::from("<peer-message direction=\"in\"");
    if let Some(p) = peer {
        open.push_str(&format!(" peer=\"{p}\""));
    }
    if let Some(n) = network {
        open.push_str(&format!(" network=\"{n}\""));
    }
    format!("{open}>\n{text}\n</peer-message>")
}

/// The inverse of [`peer_wrap`]: `(peer, network, text)` when `t` is exactly
/// one wrapped incoming peer message.
pub(crate) fn peer_unwrap(t: &str) -> Option<(Option<String>, Option<String>, String)> {
    let rest = t.strip_prefix("<peer-message direction=\"in\"")?;
    let (attrs, body) = rest.split_once(">\n")?;
    let text = body.strip_suffix("\n</peer-message>")?;
    let (mut peer, mut network) = (None, None);
    let mut a = attrs;
    while !a.is_empty() {
        let (k, v) = a.strip_prefix(' ')?.split_once("=\"")?;
        let (v, after) = v.split_once('"')?;
        match k {
            "peer" if peer.is_none() && network.is_none() => peer = Some(v.to_string()),
            "network" if network.is_none() => network = Some(v.to_string()),
            _ => return None,
        }
        a = after;
    }
    (peer_wrap(peer.as_deref(), network.as_deref(), text) == t).then(|| (peer, network, text.to_string()))
}

/// Codex's timestamp shape: `[year]-[month]-[day]T[hour]:[minute]:[second].[ms]Z`
/// (`rollout/src/recorder.rs:2093-2095`).
pub(crate) fn codex_ts(ts: &str) -> Option<String> {
    crate::util::parse_rfc3339_ms(ts).map(crate::util::ms_to_rfc3339)
}

/// Is `s` a UUID Codex's `ThreadId::from_string` accepts (`protocol/src/thread_id.rs:41`)?
pub(crate) fn is_uuid(s: &str) -> bool {
    uuid::Uuid::parse_str(s).is_ok()
}
