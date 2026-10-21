//! GitHub Copilot CLI (`copilot`, npm `@github/copilot`) sessions.
//!
//! - **`session-dir`** (>= 0.0.378): `~/.copilot/session-state/<session id>/`
//!   with `events.jsonl` (one `{type, data, id, timestamp, parentId}` event per
//!   line, subagent events marked by an envelope `agentId`) and `workspace.yaml`.
//!   What `copilot --resume <id>` loads.
//! - **`flat-jsonl`** (0.0.342 - 0.0.377): the same events in
//!   `~/.copilot/session-state/<session id>.jsonl`.
//! - **`legacy-json`** (<= 0.0.341): `~/.copilot/history-session-state/
//!   session_<id>_<ms>.json`, `{sessionId, startTime, chatMessages, timeline}`.
//! - **`interchange`** (>= 1.0.85): the JSONL `copilot sessions import` reads.
//!
//! See `docs/formats/copilot_cli.md`.

use crate::formats::{Detection, ExportOptions, Exported, Format, ImportOptions, Imported, Variant};
use crate::ledger::{Ledger, LedgerSet};
use crate::model::{Event, Kind, Native};
use anyhow::{Result, bail};
use serde_json::Value;
use std::path::Path;

mod events;
mod interchange;
mod legacy;

pub const NAME: &str = "copilot-cli";
pub(crate) const V_DIR: &str = "session-dir";
pub(crate) const V_FLAT: &str = "flat-jsonl";
pub(crate) const V_LEGACY: &str = "legacy-json";
pub(crate) const V_INTERCHANGE: &str = "interchange";
/// The producer version written into a foreign export's `session.start`.
pub(crate) const DEFAULT_VERSION: &str = "1.0.98";

pub struct CopilotCli;

static VARIANTS: [Variant; 4] = [
    Variant {
        name: V_DIR,
        versions: ">= 0.0.378",
        import: true,
        export: true,
        notes: "session-state/<id>/events.jsonl + workspace.yaml; loaded by `copilot --resume <id>`",
    },
    Variant {
        name: V_FLAT,
        versions: ">= 0.0.342, < 0.0.378 (refused by >= 1.0.44)",
        import: true,
        export: true,
        notes: "session-state/<id>.jsonl, the same events without a session directory",
    },
    Variant {
        name: V_LEGACY,
        versions: "<= 0.0.341 (refused by >= 1.0.44)",
        import: true,
        export: true,
        notes: "history-session-state/session_<id>_<ms>.json: OpenAI chatMessages + UI timeline",
    },
    Variant {
        name: V_INTERCHANGE,
        versions: "read by `copilot sessions import` >= 1.0.85",
        import: true,
        export: true,
        notes: "semantic JSONL interchange v1 (session header, message, tool-call, tool-result)",
    },
];

impl Format for CopilotCli {
    fn name(&self) -> &'static str {
        NAME
    }
    fn describe(&self) -> &'static str {
        "GitHub Copilot CLI sessions (session-state events.jsonl, flat JSONL, legacy JSON, import interchange)"
    }
    fn variants(&self) -> &'static [Variant] {
        &VARIANTS
    }
    fn detect(&self, input: &Path) -> Option<Detection> {
        detect(input)
    }
    fn import(&self, input: &Path, opts: &ImportOptions) -> Result<Imported> {
        // SPEC §3.1.6: user or assistant text holding the peer-message wrapper is that peer message.
        let mut im = import_records(input, opts)?;
        crate::formats::pi_session::unwrap_peers(&mut im.set, self.name());
        Ok(im)
    }
    fn export(&self, set: &LedgerSet, opts: &ExportOptions) -> Result<Exported> {
        let variant = opts.variant.clone().unwrap_or_else(|| {
            let own = set.iter().find_map(|l| l.agent().source.as_ref().filter(|s| s.format == NAME).and_then(|s| s.variant.clone()));
            match (own, opts.target_version.as_deref()) {
                (Some(v), _) => v,
                (None, Some(t)) if version_lt(t, "0.0.342") => V_LEGACY.into(),
                (None, Some(t)) if version_lt(t, "0.0.378") => V_FLAT.into(),
                _ => V_DIR.into(),
            }
        });
        match variant.as_str() {
            V_DIR | V_FLAT => events::export(set, opts, &variant),
            V_LEGACY => legacy::export(set, opts),
            V_INTERCHANGE => interchange::export(set, opts),
            v => bail!("copilot-cli: unknown variant {v}"),
        }
    }
}

/// What [`Format::import`] reads before peer messages are unwrapped.
fn import_records(input: &Path, opts: &ImportOptions) -> Result<Imported> {
    let variant = match opts.variant.as_deref() {
        Some(v) => v.to_string(),
        None => match detect(input) {
            Some(d) => d.variant.to_string(),
            None => bail!("{}: not a Copilot CLI session", input.display()),
        },
    };
    match variant.as_str() {
        V_DIR | V_FLAT => events::import(input, opts),
        V_LEGACY => legacy::import(input, opts),
        V_INTERCHANGE => interchange::import(input, opts),
        v => bail!("copilot-cli: unknown variant {v}"),
    }
}

pub(crate) fn version_lt(a: &str, b: &str) -> bool {
    let p = |s: &str| s.trim_start_matches('v').split(['.', '-']).take(3).map(|x| x.parse::<u64>().unwrap_or(0)).collect::<Vec<_>>();
    p(a) < p(b)
}

fn detect(input: &Path) -> Option<Detection> {
    if input.is_dir() {
        let mut best: Option<Detection> = None;
        for e in walkdir::WalkDir::new(input).max_depth(3).sort_by_file_name().into_iter().flatten() {
            if !e.file_type().is_file() {
                continue;
            }
            if let Some(d) = detect_file(e.path())
                && best.as_ref().is_none_or(|b| d.confidence > b.confidence)
            {
                best = Some(d);
            }
        }
        return best;
    }
    detect_file(input)
}

fn detect_file(p: &Path) -> Option<Detection> {
    let name = p.file_name()?.to_string_lossy().to_string();
    if !(name.ends_with(".jsonl") || name.ends_with(".json")) {
        return None;
    }
    let text = std::fs::read_to_string(p).ok()?;
    if name.ends_with(".json") {
        let v: Value = serde_json::from_str(&text).ok()?;
        if v.get("chatMessages").is_some() || v.get("timeline").is_some() {
            return Some(Detection { variant: V_LEGACY, confidence: 85, version: Some("<= 0.0.341".into()) });
        }
        return None;
    }
    for line in text.lines().filter(|l| l.trim_start().starts_with('{')).take(5) {
        let Ok(v) = serde_json::from_str::<Value>(line) else { continue };
        let ty = v.get("type").and_then(Value::as_str).unwrap_or("");
        if ty == "session" && v.get("externalId").is_some() {
            return Some(Detection { variant: V_INTERCHANGE, confidence: 85, version: None });
        }
        let copilotish = v.get("data").is_some_and(Value::is_object)
            && (ty.starts_with("session.")
                || ty.starts_with("user.")
                || ty.starts_with("assistant.")
                || ty.starts_with("tool.")
                || ty.starts_with("hook.")
                || ty.starts_with("subagent.")
                || ty.starts_with("system."));
        if copilotish {
            let version = v.get("data").and_then(|d| d.get("copilotVersion")).and_then(Value::as_str).map(String::from);
            let variant = if name == "events.jsonl" { V_DIR } else { V_FLAT };
            return Some(Detection { variant, confidence: 85, version });
        }
    }
    None
}

// ---- shared helpers ----

pub(crate) fn native(variant: &str, id: &str, version: Option<&str>) -> Native {
    Native::new(NAME, Some(id.to_string())).variant(Some(variant)).version(version)
}

pub(crate) fn push_record(l: &mut Ledger, nid: &str, n: Native, mut evs: Vec<Event>, ts: Option<String>) {
    if evs.is_empty() {
        evs.push(Event::new("", Kind::Native).ts(ts));
    }
    for (i, mut e) in evs.into_iter().enumerate() {
        e.id = if i == 0 { nid.to_string() } else { format!("{nid}#{i}") };
        e.native = Some(if i == 0 { n.clone() } else { n.part_of(i as u32) });
        l.append_unique(e);
    }
}

pub(crate) fn own<'a>(e: &'a Event, variant: &str) -> Option<&'a Native> {
    e.native.as_ref().filter(|n| n.format == NAME && n.variant.as_deref() == Some(variant))
}

/// Tool output as text; `true` if media was dropped.
pub(crate) fn output_text(v: Option<&Value>) -> (String, bool) {
    crate::formats::cursor::output_text(v)
}

/// A UUID for an exported record, stable across re-exports.
pub(crate) fn uuid(seed: &str) -> String {
    crate::util::uuid_for(seed)
}

pub(crate) fn session_id(l: &Ledger) -> String {
    let id = l.id();
    if uuid::Uuid::parse_str(id).is_ok() {
        return id.to_string();
    }
    l.agent()
        .source
        .as_ref()
        .filter(|s| s.format == NAME)
        .and_then(|s| s.native_id.clone())
        .filter(|n| uuid::Uuid::parse_str(n).is_ok())
        .unwrap_or_else(|| crate::util::uuid_for(id))
}

#[cfg(test)]
mod tests;
